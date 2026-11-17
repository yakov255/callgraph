//! Локальные факты потока данных: обход AST PHP и истоки переменных/свойств/ключей.
//!
//! Контракт — `super::model`. Для каждого метода/функции собираются присваивания, чтения,
//! возвраты и аргументы вызовов. Анализ нечувствителен к потоку управления: наборы истоков
//! объединяются (union), дедуплицируются и ограничиваются 16 атомами.

use std::collections::{HashMap, HashSet};

use mago_span::HasSpan;
use mago_syntax::cst::*;

use super::model::{ArgEdge, Assign, FlowFile, MethodFlow, MethodTypes, Read, ReceiverEdge};
use crate::lang::php::analysis::NameResolver;
use crate::lang::php::analysis::docblock::doc_comment_before;
use crate::lang::php::parser::{MagoAdapter, ParsedFile, PhpParser};
use crate::lang::php::printer::lossy;

/// Максимум атомов в одном наборе истоков.
const MAX_ATOMS: usize = 16;
/// Максимум вложенных ветвлений, которые анализ различает по путям.
const MAX_DEPTH: u32 = 4;
/// Максимум одновременных живых путей.
const MAX_PATHS: usize = 32;
/// Идентификатор кода верхнего уровня.
const TOP: &str = "__top__";

/// Тип возврата методов файла: `Class::method` / `function` → класс (для array — тип элемента).
pub fn file_return_types(path: &str, source: &[u8]) -> Vec<(String, String)> {
    MagoAdapter.parse(path, source, |parsed| {
        let mut collector = ReturnCollector {
            parsed,
            resolver: NameResolver::new(None),
            types: Vec::new(),
        };
        collector.statements(parsed.program.statements.as_slice());
        collector.types
    })
}

/// Разобрать файл и вернуть факты по методам (без таблицы типов возврата).
pub fn file_facts(path: &str, source: &[u8]) -> Vec<FlowFile> {
    file_facts_with_types(path, source, &MethodTypes::new())
}

/// Разобрать файл и вернуть факты по методам с таблицей типов возврата проекта.
pub fn file_facts_with_types(path: &str, source: &[u8], types: &MethodTypes) -> Vec<FlowFile> {
    MagoAdapter.parse(path, source, |parsed| {
        let magic = magic_classes(parsed);
        let mut collector = Collector {
            parsed,
            resolver: NameResolver::new(None),
            methods: Vec::new(),
            top: None,
            method_types: types,
            magic,
        };
        collector.statements(parsed.program.statements.as_slice());
        if let Some(top) = collector.top {
            collector.methods.push(top.flow);
        }
        vec![FlowFile {
            file: path.to_string(),
            methods: collector.methods,
        }]
    })
}

// ---------------------------------------------------------------------------
// Обход объявлений: пространства имён, импорты, классы, функции.
// ---------------------------------------------------------------------------

struct Collector<'p, 'a, 't> {
    parsed: &'p ParsedFile<'a>,
    resolver: NameResolver,
    methods: Vec<MethodFlow>,
    /// Накопитель фактов кода верхнего уровня (`__top__`).
    top: Option<Analyzer<'p, 'a, 't>>,
    /// Таблица типов возврата проекта.
    method_types: &'t MethodTypes,
    /// Классы файла с магическими аксессорами (`__get`/`__set`).
    magic: HashSet<String>,
}

impl<'a, 't> Collector<'_, 'a, 't> {
    fn statements(&mut self, statements: &[Statement<'a>]) {
        for statement in statements {
            self.statement(statement);
        }
    }

    fn statement(&mut self, statement: &Statement<'a>) {
        match statement {
            Statement::Namespace(node) => {
                let ns = node
                    .name
                    .map(|name| lossy(name.value()).trim_start_matches('\\').to_string());
                self.resolver = NameResolver::new(ns);
                self.statements(node.statements().as_slice());
            }
            Statement::Use(node) => self.imports(&node.items),
            Statement::Class(node) => {
                self.class_like(node.name.value, node.extends.as_ref(), &node.members)
            }
            Statement::Interface(node) => {
                self.class_like(node.name.value, node.extends.as_ref(), &node.members)
            }
            Statement::Trait(node) => self.class_like(node.name.value, None, &node.members),
            Statement::Enum(node) => self.class_like(node.name.value, None, &node.members),
            Statement::Function(node) => {
                let id = lossy(node.name.value);
                let mut analyzer = Analyzer::new(
                    self.parsed,
                    self.resolver.clone(),
                    None,
                    id,
                    HashMap::new(),
                    self.method_types,
                    ClassFacts {
                        magic: self.magic.clone(),
                        props: Vec::new(),
                    },
                );
                analyzer.seed_params(&node.parameter_list);
                analyzer.statements(node.body.statements.as_slice());
                self.methods.push(analyzer.flow);
            }
            Statement::OpeningTag(_)
            | Statement::ClosingTag(_)
            | Statement::Inline(_)
            | Statement::Noop(_)
            | Statement::Declare(_) => {}
            _ => self.top_statement(statement),
        }
    }

    /// Факты кода верхнего уровня: один накопитель на файл.
    fn top_statement(&mut self, statement: &Statement<'a>) {
        let mut analyzer = self.top.take().unwrap_or_else(|| {
            Analyzer::new(
                self.parsed,
                self.resolver.clone(),
                None,
                TOP.to_string(),
                HashMap::new(),
                self.method_types,
                ClassFacts {
                    magic: self.magic.clone(),
                    props: Vec::new(),
                },
            )
        });
        analyzer.resolver = self.resolver.clone();
        analyzer.statement(statement);
        self.top = Some(analyzer);
    }

    /// Класс/интерфейс/трейт/enum: типы свойств и факты по каждому методу.
    fn class_like(
        &mut self,
        name: &[u8],
        extends: Option<&Extends<'a>>,
        members: &Sequence<'a, ClassLikeMember<'a>>,
    ) {
        let short = lossy(name);
        let parents: Vec<String> = extends
            .map(|node| {
                node.types
                    .iter()
                    .map(|ty| self.resolver.resolve_class(&self.parsed.text(ty.span())))
                    .collect()
            })
            .unwrap_or_default();
        self.resolver.enter_container(&short, parents);
        let full = self.resolver.resolve_class("self");
        let types = self.prop_types(members);
        let properties = self.property_names(members);
        for member in members.iter() {
            let ClassLikeMember::Method(method) = member else {
                continue;
            };
            let MethodBody::Concrete(block) = &method.body else {
                continue;
            };
            let id = format!("{full}::{}", lossy(method.name.value));
            let mut analyzer = Analyzer::new(
                self.parsed,
                self.resolver.clone(),
                Some(full.clone()),
                id,
                types.clone(),
                self.method_types,
                ClassFacts {
                    magic: self.magic.clone(),
                    props: properties.clone(),
                },
            );
            analyzer.seed_params(&method.parameter_list);
            analyzer.statements(block.statements.as_slice());
            self.methods.push(analyzer.flow);
        }
        self.resolver.leave_container();
    }

    /// Типизированные свойства класса и продвинутые параметры конструктора.
    fn prop_types(&self, members: &Sequence<'a, ClassLikeMember<'a>>) -> HashMap<String, String> {
        let mut types = HashMap::new();
        for member in members.iter() {
            match member {
                ClassLikeMember::Property(Property::Plain(property)) => {
                    let Some(hint) = &property.hint else {
                        continue;
                    };
                    let Some(ty) = hint_type(self.parsed, &self.resolver, hint) else {
                        continue;
                    };
                    for item in property.items.iter() {
                        let variable = match item {
                            PropertyItem::Abstract(item) => &item.variable,
                            PropertyItem::Concrete(item) => &item.variable,
                        };
                        let name = lossy(variable.name);
                        types.insert(
                            format!("this->{}", name.trim_start_matches('$')),
                            ty.clone(),
                        );
                    }
                }
                ClassLikeMember::Method(method)
                    if lossy(method.name.value).eq_ignore_ascii_case("__construct") =>
                {
                    for parameter in method.parameter_list.parameters.iter() {
                        if !parameter.is_promoted_property() {
                            continue;
                        }
                        let Some(hint) = &parameter.hint else {
                            continue;
                        };
                        let Some(ty) = hint_type(self.parsed, &self.resolver, hint) else {
                            continue;
                        };
                        let name = lossy(parameter.variable.name);
                        types.insert(format!("this->{}", name.trim_start_matches('$')), ty);
                    }
                }
                _ => {}
            }
        }
        types
    }

    /// Имена объявленных свойств класса (без `$`), включая продвинутые параметры.
    fn property_names(&self, members: &Sequence<'a, ClassLikeMember<'a>>) -> Vec<String> {
        let mut names = Vec::new();
        for member in members.iter() {
            match member {
                ClassLikeMember::Property(Property::Plain(property)) => {
                    for item in property.items.iter() {
                        let variable = match item {
                            PropertyItem::Abstract(item) => &item.variable,
                            PropertyItem::Concrete(item) => &item.variable,
                        };
                        names.push(lossy(variable.name).trim_start_matches('$').to_string());
                    }
                }
                ClassLikeMember::Method(method)
                    if lossy(method.name.value).eq_ignore_ascii_case("__construct") =>
                {
                    for parameter in method.parameter_list.parameters.iter() {
                        if parameter.is_promoted_property() {
                            names.push(
                                lossy(parameter.variable.name)
                                    .trim_start_matches('$')
                                    .to_string(),
                            );
                        }
                    }
                }
                _ => {}
            }
        }
        names
    }

    /// Импорты `use`: классы (функции и константы пропускаются).
    fn imports(&mut self, items: &UseItems<'a>) {
        apply_imports(self.parsed, &mut self.resolver, items);
    }
}

// ---------------------------------------------------------------------------
// Сбор типов возврата (для межфайлового вывода типов).
// ---------------------------------------------------------------------------

struct ReturnCollector<'p, 'a> {
    parsed: &'p ParsedFile<'a>,
    resolver: NameResolver,
    types: Vec<(String, String)>,
}

impl<'a> ReturnCollector<'_, 'a> {
    fn statements(&mut self, statements: &[Statement<'a>]) {
        for statement in statements {
            self.statement(statement);
        }
    }

    fn statement(&mut self, statement: &Statement<'a>) {
        match statement {
            Statement::Namespace(node) => {
                let ns = node
                    .name
                    .map(|name| lossy(name.value()).trim_start_matches('\\').to_string());
                self.resolver = NameResolver::new(ns);
                self.statements(node.statements().as_slice());
            }
            Statement::Use(node) => apply_imports(self.parsed, &mut self.resolver, &node.items),
            Statement::Class(node) => {
                self.class_like(node.name.value, node.extends.as_ref(), &node.members)
            }
            Statement::Interface(node) => {
                self.class_like(node.name.value, node.extends.as_ref(), &node.members)
            }
            Statement::Trait(node) => self.class_like(node.name.value, None, &node.members),
            Statement::Enum(node) => self.class_like(node.name.value, None, &node.members),
            Statement::Function(node) => {
                let start = node.span().start.offset;
                if let Some(ty) = return_type(
                    self.parsed,
                    &self.resolver,
                    node.return_type_hint.as_ref(),
                    start,
                ) {
                    self.types.push((lossy(node.name.value), ty));
                }
            }
            _ => {}
        }
    }

    fn class_like(
        &mut self,
        name: &[u8],
        extends: Option<&Extends<'a>>,
        members: &Sequence<'a, ClassLikeMember<'a>>,
    ) {
        let short = lossy(name);
        let parents: Vec<String> = extends
            .map(|node| {
                node.types
                    .iter()
                    .map(|ty| self.resolver.resolve_class(&self.parsed.text(ty.span())))
                    .collect()
            })
            .unwrap_or_default();
        self.resolver.enter_container(&short, parents);
        let full = self.resolver.resolve_class("self");
        for member in members.iter() {
            let ClassLikeMember::Method(method) = member else {
                continue;
            };
            let start = method.span().start.offset;
            if let Some(ty) = return_type(
                self.parsed,
                &self.resolver,
                method.return_type_hint.as_ref(),
                start,
            ) {
                let id = format!("{full}::{}", lossy(method.name.value));
                self.types.push((id, ty));
            }
        }
        self.resolver.leave_container();
    }
}

// ---------------------------------------------------------------------------
// Магические аксессоры: предварительный проход по объявлениям файла.
// ---------------------------------------------------------------------------

/// Полные имена классов файла, объявляющих `__get`/`__set`.
fn magic_classes(parsed: &ParsedFile<'_>) -> HashSet<String> {
    struct MagicCollector<'p, 'a> {
        parsed: &'p ParsedFile<'a>,
        resolver: NameResolver,
        magic: HashSet<String>,
    }

    impl<'a> MagicCollector<'_, 'a> {
        fn statements(&mut self, statements: &[Statement<'a>]) {
            for statement in statements {
                self.statement(statement);
            }
        }

        fn statement(&mut self, statement: &Statement<'a>) {
            match statement {
                Statement::Namespace(node) => {
                    let ns = node
                        .name
                        .map(|name| lossy(name.value()).trim_start_matches('\\').to_string());
                    self.resolver = NameResolver::new(ns);
                    self.statements(node.statements().as_slice());
                }
                Statement::Use(node) => apply_imports(self.parsed, &mut self.resolver, &node.items),
                Statement::Class(node) => {
                    self.class_like(node.name.value, node.extends.as_ref(), &node.members)
                }
                Statement::Interface(node) => {
                    self.class_like(node.name.value, node.extends.as_ref(), &node.members)
                }
                Statement::Trait(node) => self.class_like(node.name.value, None, &node.members),
                Statement::Enum(node) => self.class_like(node.name.value, None, &node.members),
                _ => {}
            }
        }

        fn class_like(
            &mut self,
            name: &[u8],
            extends: Option<&Extends<'a>>,
            members: &Sequence<'a, ClassLikeMember<'a>>,
        ) {
            let short = lossy(name);
            let parents: Vec<String> = extends
                .map(|node| {
                    node.types
                        .iter()
                        .map(|ty| self.resolver.resolve_class(&self.parsed.text(ty.span())))
                        .collect()
                })
                .unwrap_or_default();
            self.resolver.enter_container(&short, parents);
            let magic = members.iter().any(|member| match member {
                ClassLikeMember::Method(method) => {
                    let name = lossy(method.name.value);
                    name.eq_ignore_ascii_case("__get") || name.eq_ignore_ascii_case("__set")
                }
                _ => false,
            });
            if magic {
                let full = self.resolver.resolve_class("self");
                self.magic.insert(full);
            }
            self.resolver.leave_container();
        }
    }

    let mut collector = MagicCollector {
        parsed,
        resolver: NameResolver::new(None),
        magic: HashSet::new(),
    };
    collector.statements(parsed.program.statements.as_slice());
    collector.magic
}

// ---------------------------------------------------------------------------
// Анализ тела метода.
// ---------------------------------------------------------------------------

/// Состояние анализа: достижимые значения и факты о текущем пути.
#[derive(Debug, Clone, Default)]
struct FlowState {
    /// Истоки переменных (may): `$x` -> атомы.
    vars: HashMap<String, Vec<String>>,
    /// Гарантированные истоки переменных (must): `$x` -> атомы, подмножество `vars`.
    must: HashMap<String, Vec<String>>,
    /// Минимальные типы: `$x` / `this->p` -> полное имя класса.
    types: HashMap<String, String>,
    /// Известные булевы значения: `$flag` -> `true`/`false`.
    bools: HashMap<String, bool>,
    /// Переменные, гарантированно равные `null`.
    nulls: HashSet<String>,
    /// Известные ключи массива по базовой переменной: `$row` -> {`name_en`}.
    offsets: HashMap<String, HashSet<String>>,
    /// Переменные-ключи `foreach ($row as $k => $v)`.
    keys: HashSet<String>,
    /// После `extract($arr)` неизвестная переменная читается как `key:<name>`.
    extracted: bool,
    /// Путь завершён (`return`): до конца блока записи не идут.
    terminated: bool,
}

/// Вид кадра переходов: цикл или `switch`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum JumpKind {
    Loop,
    Switch,
}

/// Кадр `break`/`continue`: состояния выходов текущей конструкции.
#[derive(Debug, Clone)]
struct JumpFrame {
    kind: JumpKind,
    /// Состояния на `break` (и `continue` внутри `switch`).
    breaks: Vec<FlowState>,
    /// Состояния на `continue` цикла: вливаются в обратное ребро.
    continues: Vec<FlowState>,
}

impl JumpFrame {
    fn new(kind: JumpKind) -> Self {
        Self {
            kind,
            breaks: Vec::new(),
            continues: Vec::new(),
        }
    }
}

/// Итог одной итерации цикла: обратное ребро и путь выхода по условию.
struct Iteration {
    back: Option<FlowState>,
    exit: Option<FlowState>,
}

/// Границы уже записанных фактов: повторный проход тела цикла не должен
/// дублировать чтения и рёбра.
struct FlowMark {
    assigns: usize,
    reads: usize,
    returns: usize,
    args: usize,
    receivers: usize,
}

impl FlowMark {
    fn new(flow: &MethodFlow) -> Self {
        Self {
            assigns: flow.assigns.len(),
            reads: flow.reads.len(),
            returns: flow.returns.len(),
            args: flow.args.len(),
            receivers: flow.receivers.len(),
        }
    }

    /// Оставить из дописанного только то, чего не было: повтор тела цикла
    /// видит те же узлы, поэтому точные дубликаты отбрасываются.
    fn dedupe(&self, flow: &mut MethodFlow) {
        dedupe_tail(&mut flow.assigns, self.assigns);
        dedupe_tail(&mut flow.reads, self.reads);
        dedupe_tail(&mut flow.returns, self.returns);
        dedupe_tail(&mut flow.args, self.args);
        dedupe_tail(&mut flow.receivers, self.receivers);
    }
}

fn dedupe_tail<T: PartialEq>(items: &mut Vec<T>, keep: usize) {
    let tail: Vec<T> = items.split_off(keep);
    for item in tail {
        if !items.contains(&item) {
            items.push(item);
        }
    }
}

/// Факты файла и класса, нужные анализатору: магические аксессоры и свойства.
#[derive(Debug, Clone, Default)]
struct ClassFacts {
    /// Классы файла, объявляющие `__get`/`__set`.
    magic: HashSet<String>,
    /// Объявленные свойства текущего класса (без `$`).
    props: Vec<String>,
}

struct Analyzer<'p, 'a, 't> {
    parsed: &'p ParsedFile<'a>,
    resolver: NameResolver,
    /// Полное имя текущего класса (`None` для функций и верхнего уровня).
    class: Option<String>,
    /// `Class::method`, имя функции или `__top__`.
    method: String,
    /// Текущее состояние пути.
    state: FlowState,
    /// Глубина вложенных ветвлений (кап `MAX_DEPTH`).
    depth: u32,
    /// Число живых путей (кап `MAX_PATHS`).
    paths: usize,
    /// Типы возврата методов проекта (`Class::method` -> класс).
    method_types: &'t MethodTypes,
    /// Классы файла с магическими аксессорами (`__get`/`__set`).
    magic: HashSet<String>,
    /// Объявленные свойства текущего класса (без `$`).
    props: Vec<String>,
    /// Переменные, объявленные через `global` в этом методе.
    globals: HashSet<String>,
    /// Стек конструкций с `break`/`continue`.
    jumps: Vec<JumpFrame>,
    flow: MethodFlow,
}

impl<'p, 'a, 't> Analyzer<'p, 'a, 't> {
    fn new(
        parsed: &'p ParsedFile<'a>,
        resolver: NameResolver,
        class: Option<String>,
        method: String,
        types: HashMap<String, String>,
        method_types: &'t MethodTypes,
        class_facts: ClassFacts,
    ) -> Self {
        Self {
            parsed,
            resolver,
            flow: MethodFlow {
                id: method.clone(),
                container: class.clone(),
                ..MethodFlow::default()
            },
            class,
            method,
            state: FlowState {
                types,
                ..FlowState::default()
            },
            depth: 0,
            paths: 0,
            method_types,
            magic: class_facts.magic,
            props: class_facts.props,
            globals: HashSet::new(),
            jumps: Vec::new(),
        }
    }

    fn statements(&mut self, statements: &[Statement<'a>]) {
        for statement in statements {
            if self.state.terminated {
                break;
            }
            self.statement(statement);
        }
    }

    fn statement(&mut self, statement: &Statement<'a>) {
        match statement {
            Statement::Block(block) => self.statements(block.statements.as_slice()),
            Statement::If(node) => self.if_statement(node),
            Statement::While(node) => self.while_statement(node),
            Statement::DoWhile(node) => self.do_while_statement(node),
            Statement::For(node) => self.for_statement(node),
            Statement::Foreach(node) => self.foreach_statement(node),
            Statement::Switch(node) => self.switch_statement(node),
            Statement::Break(node) => self.break_statement(node.level),
            Statement::Continue(node) => self.continue_statement(node.level),
            Statement::Global(node) => {
                for variable in node.variables.iter() {
                    let Variable::Direct(variable) = variable else {
                        continue;
                    };
                    let name = lossy(variable.name);
                    let atom = format!("prop:global::{}", name.trim_start_matches('$'));
                    self.set_var(&name, &[atom], &[]);
                    self.globals.insert(name);
                }
            }
            Statement::Try(node) => self.try_statement(node),
            Statement::Return(node) => {
                if let Some(value) = node.value {
                    let origins = self.origins(value);
                    let must = self.origins_must(value);
                    if significant(&origins) {
                        let origins = norm(origins);
                        let must = must_within(must, &origins);
                        self.flow.returns.push(Read {
                            line: self.line_of(value),
                            expr: self.one_line(value),
                            origins,
                            must,
                        });
                    }
                }
                self.state.terminated = true;
            }
            Statement::Expression(node) => self.expression_statement(node.expression),
            Statement::Echo(node) => {
                for value in node.values.iter() {
                    self.expression_statement(value);
                }
            }
            _ => {}
        }
    }

    // -- ветвления ----------------------------------------------------------

    /// `if` / `elseif` / `else`: путь на ветку, слияние may/must на выходе.
    fn if_statement(&mut self, node: &If<'a>) {
        let mut branches: Vec<(&Expression<'a>, &[Statement<'a>])> =
            vec![(node.condition, node.body.statements())];
        for (condition, body) in node.body.else_if_clauses() {
            branches.push((condition, body));
        }
        self.chain(&branches, node.body.else_statements());
    }

    /// Цепочка условий: первое выполнимое, иначе следующее, иначе `else`.
    /// Условие уточняет состояние: `then` — истинные факты, `else` — ложные.
    fn chain(
        &mut self,
        branches: &[(&Expression<'a>, &[Statement<'a>])],
        otherwise: Option<&[Statement<'a>]>,
    ) {
        let Some((&(condition, body), rest)) = branches.split_first() else {
            if let Some(otherwise) = otherwise {
                self.statements(otherwise);
            }
            return;
        };
        self.condition(condition);
        let truthy = self.narrow(condition, true);
        let falsey = self.narrow(condition, false);
        match self.truth_value(condition) {
            // Статически известно значение: невыполнимые ветки не идут вовсе.
            Some(true) => {
                self.state = truthy;
                self.branch(body);
            }
            Some(false) => {
                self.state = falsey;
                self.chain(rest, otherwise);
            }
            None if !self.can_fork() => {
                let before = self.state.clone();
                self.state = truthy;
                self.branch(body);
                self.state = falsey;
                self.chain(rest, otherwise);
                self.conservative_after(&before);
            }
            None => {
                self.state = truthy;
                self.branch(body);
                let then_state = self.state.clone();
                self.state = falsey;
                self.chain(rest, otherwise);
                let else_state = self.state.clone();
                self.state = merge_states(then_state, else_state);
            }
        }
    }

    /// Одна ветка: учитывает кап вложенности и живых путей.
    fn branch(&mut self, statements: &[Statement<'a>]) {
        self.depth += 1;
        self.paths += 1;
        self.statements(statements);
        self.paths -= 1;
        self.depth -= 1;
    }

    fn can_fork(&self) -> bool {
        self.depth < MAX_DEPTH && self.paths < MAX_PATHS
    }

    // -- циклы --------------------------------------------------------------

    /// `while`: условие проверяется перед каждой итерацией.
    fn while_statement(&mut self, node: &While<'a>) {
        let before = self.state.clone();
        let zero = (!self.truth_value(node.condition).unwrap_or(false)).then(|| before.clone());
        self.loop_fixpoint(before.clone(), zero, true, false, |this| {
            this.condition(node.condition);
            let exit = (!this.state.terminated)
                .then(|| this.narrow_state(&this.state, node.condition, false));
            this.state = this.narrow_state(&this.state, node.condition, true);
            if this.state.terminated {
                return Iteration { back: None, exit };
            }
            this.statements(node.body.statements());
            let back = (!this.state.terminated).then(|| this.state.clone());
            Iteration { back, exit }
        });
        self.generalize_loop(&before, &written_vars(node.body.statements()));
    }

    /// `do-while`: тело исполняется хотя бы раз, поэтому путь «ноль итераций»
    /// отсутствует, а присваивания тела сохраняют `must`.
    fn do_while_statement(&mut self, node: &DoWhile<'a>) {
        let entry = self.state.clone();
        self.loop_fixpoint(entry, None, false, false, |this| {
            this.statement(node.statement);
            if this.state.terminated {
                return Iteration {
                    back: None,
                    exit: None,
                };
            }
            this.condition(node.condition);
            let exit = this.narrow_state(&this.state, node.condition, false);
            let back = this.narrow_state(&this.state, node.condition, true);
            Iteration {
                back: Some(back),
                exit: Some(exit),
            }
        });
    }

    /// `for`: инициализация один раз, условие и инкременты на каждой итерации.
    fn for_statement(&mut self, node: &For<'a>) {
        for expression in node.initializations.iter() {
            self.expression_statement(expression);
        }
        let before = self.state.clone();
        let always = node
            .conditions
            .iter()
            .all(|condition| self.truth_value(condition) == Some(true));
        let zero = (!node.conditions.is_empty() && !always).then(|| before.clone());
        self.loop_fixpoint(before.clone(), zero, true, false, |this| {
            let mut exit: Option<FlowState> = None;
            for expression in node.conditions.iter() {
                this.condition(expression);
                let falsey = this.narrow_state(&this.state, expression, false);
                exit = Some(join_state_value(exit, falsey));
                this.state = this.narrow_state(&this.state, expression, true);
            }
            if this.state.terminated {
                return Iteration { back: None, exit };
            }
            this.statements(node.body.statements());
            if !this.state.terminated {
                for expression in node.increments.iter() {
                    this.expression_statement(expression);
                }
            }
            let back = (!this.state.terminated).then(|| this.state.clone());
            Iteration { back, exit }
        });
        self.generalize_loop(&before, &written_vars(node.body.statements()));
    }

    /// `foreach`: истоки элемента — итерируемое, ключ — `keys`.
    fn foreach_statement(&mut self, node: &Foreach<'a>) {
        let before = self.state.clone();
        // Итерируемое вычисляется один раз: повторные проходы тела не должны
        // дублировать чтения и рёбра аргументов.
        let iterable = self.origins(node.expression);
        let iterable_must = self.origins_must(node.expression);
        let element = self.type_of(node.expression);
        self.loop_fixpoint(before.clone(), Some(before.clone()), true, true, |this| {
            if let Some(name) = direct_var_name(node.target.value()) {
                this.set_var(&name, &iterable, &iterable_must);
                this.state.offsets.remove(&name);
                this.state.bools.remove(&name);
                this.state.nulls.remove(&name);
                if let Some(element) = &element {
                    this.state.types.insert(name, element.clone());
                }
            }
            if let Some(key) = node.target.key()
                && let Some(name) = direct_var_name(key)
            {
                this.set_var(&name, &["unknown".to_string()], &[]);
                this.state.keys.insert(name);
            }
            this.statements(node.body.statements());
            let back = (!this.state.terminated).then(|| this.state.clone());
            Iteration { back, exit: None }
        });
        self.generalize_loop(&before, &written_vars(node.body.statements()));
    }

    /// Неподвижная точка цикла: 2–3 прохода тела от слитого состояния.
    /// `head` — состояние входа в итерацию, `zero` — путь «ноль итераций»,
    /// `merge_head` — сливать ли вход с обратным ребром (`false` для `do-while`),
    /// `head_exits` — выход из цикла равен голове после последней итерации
    /// (`foreach`: условие проверять нечем).
    fn loop_fixpoint<F>(
        &mut self,
        head: FlowState,
        zero: Option<FlowState>,
        merge_head: bool,
        head_exits: bool,
        mut iteration: F,
    ) where
        F: FnMut(&mut Self) -> Iteration,
    {
        self.jumps.push(JumpFrame::new(JumpKind::Loop));
        let mut head = head;
        let mut exits: Option<FlowState> = None;
        let mut exit_paths: Option<FlowState> = None;
        for pass in 0..3 {
            self.state = head.clone();
            let mark = FlowMark::new(&self.flow);
            let step = iteration(self);
            if pass > 0 {
                mark.dedupe(&mut self.flow);
            }
            let (breaks, continues) = self.drain_jumps();
            for state in breaks {
                join_state(&mut exits, state);
            }
            if let Some(state) = step.exit {
                join_state(&mut exit_paths, state);
            }
            let mut back = step.back;
            for state in continues {
                back = Some(match back {
                    Some(current) => merge_states(current, state),
                    None => state,
                });
            }
            let Some(back) = back else {
                break;
            };
            let next = if merge_head {
                merge_states(head.clone(), back)
            } else {
                back
            };
            let grew = origins_grew(&head, &next);
            head = next;
            if !grew {
                break;
            }
        }
        self.jumps.pop();
        let mut post = exits;
        if let Some(zero) = zero {
            join_state(&mut post, zero);
        }
        let has_exit = exit_paths.is_some();
        if let Some(exit) = exit_paths {
            join_state(&mut post, exit);
        }
        // Голова фиксированной точки — состояние после последней итерации.
        // Для `foreach` это единственный выход; для `while`/`for` он есть
        // только когда условие может стать ложным.
        if head_exits || has_exit {
            join_state(&mut post, head.clone());
        }
        match post {
            Some(state) => self.state = state,
            None => {
                self.state = head;
                self.state.terminated = true;
            }
        }
    }

    /// После цикла: у записанных в теле переменных `must` — только общий для
    /// входа и итераций; остальные сохраняют факты входа.
    fn generalize_loop(&mut self, entry: &FlowState, written: &HashSet<String>) {
        if self.state.terminated {
            return;
        }
        for name in written {
            let entry_must = entry.must.get(name).cloned().unwrap_or_default();
            let current = self.state.must.get(name).cloned().unwrap_or_default();
            let common = intersect(&entry_must, &current);
            if common.is_empty() {
                self.state.must.remove(name);
            } else {
                self.state.must.insert(name.clone(), common);
            }
        }
    }

    fn drain_jumps(&mut self) -> (Vec<FlowState>, Vec<FlowState>) {
        match self.jumps.last_mut() {
            Some(frame) => (
                std::mem::take(&mut frame.breaks),
                std::mem::take(&mut frame.continues),
            ),
            None => (Vec::new(), Vec::new()),
        }
    }

    fn jump_level(&self, level: Option<&Expression<'a>>) -> usize {
        let Some(level) = level else {
            return 1;
        };
        match level.unparenthesized() {
            Expression::Literal(Literal::Integer(integer)) => integer
                .value
                .map(|value| value.max(1) as usize)
                .unwrap_or(1),
            _ => 1,
        }
    }

    /// `break [n]`: состояние уходит в выход цели, путь завершается.
    fn break_statement(&mut self, level: Option<&Expression<'a>>) {
        let level = self.jump_level(level);
        let state = self.state.clone();
        match self.jumps.len().checked_sub(level) {
            Some(index) => {
                self.jumps[index].breaks.push(state);
            }
            None => {
                // Уровень за пределами вложенности: пишем в ближайший кадр,
                // путь всё равно завершается.
                if let Some(frame) = self.jumps.last_mut() {
                    frame.breaks.push(state);
                }
            }
        }
        self.state.terminated = true;
    }

    /// `continue [n]`: `continue 1` в `switch` равносилен `break` этого switch.
    fn continue_statement(&mut self, level: Option<&Expression<'a>>) {
        let level = self.jump_level(level);
        let mut remaining = level;
        let mut target = None;
        for index in (0..self.jumps.len()).rev() {
            remaining -= 1;
            if remaining == 0 {
                target = Some(index);
                break;
            }
        }
        let state = self.state.clone();
        match target {
            Some(index) => match self.jumps[index].kind {
                JumpKind::Loop => self.jumps[index].continues.push(state),
                JumpKind::Switch if level == 1 => self.jumps[index].breaks.push(state),
                JumpKind::Switch => {
                    // `continue 2` из switch внутри цикла: ищем цикл выше.
                    if let Some(loop_index) = (0..index)
                        .rev()
                        .find(|&i| self.jumps[i].kind == JumpKind::Loop)
                    {
                        self.jumps[loop_index].continues.push(state);
                    } else if let Some(frame) = self.jumps.last_mut() {
                        frame.breaks.push(state);
                    }
                }
            },
            None => {
                if let Some(frame) = self.jumps.last_mut() {
                    frame.breaks.push(state);
                }
            }
        }
        self.state.terminated = true;
    }

    // -- уточнение условий --------------------------------------------------

    /// Состояние после условия: копия с фактами `truthy`.
    fn narrow(&self, expression: &Expression<'a>, truthy: bool) -> FlowState {
        self.narrow_state(&self.state, expression, truthy)
    }

    fn narrow_state(
        &self,
        state: &FlowState,
        expression: &Expression<'a>,
        truthy: bool,
    ) -> FlowState {
        let mut state = state.clone();
        self.apply_narrow(&mut state, expression, truthy);
        state
    }

    fn apply_narrow(&self, state: &mut FlowState, expression: &Expression<'a>, truthy: bool) {
        match expression.unparenthesized() {
            Expression::Parenthesized(inner) => self.apply_narrow(state, inner.expression, truthy),
            Expression::UnaryPrefix(unary)
                if matches!(unary.operator, UnaryPrefixOperator::Not(_)) =>
            {
                self.apply_narrow(state, unary.operand, !truthy);
            }
            Expression::Variable(Variable::Direct(variable)) => {
                let name = lossy(variable.name);
                if state.bools.contains_key(&name) {
                    state.bools.insert(name, truthy);
                }
            }
            Expression::Binary(binary) => self.apply_binary_narrow(state, binary, truthy),
            Expression::Construct(Construct::Isset(node)) => {
                self.apply_isset_narrow(state, &node.values, truthy);
            }
            Expression::Construct(Construct::Empty(node)) => {
                if !truthy && let Some(name) = direct_var_name(node.value) {
                    state.nulls.remove(&name);
                }
            }
            Expression::Call(call) => self.apply_call_narrow(state, call, truthy),
            _ => {}
        }
    }

    fn apply_binary_narrow(&self, state: &mut FlowState, binary: &Binary<'a>, truthy: bool) {
        match binary.operator {
            BinaryOperator::And(_) | BinaryOperator::LowAnd(_) => {
                if truthy {
                    // Оба истинны: уточнения применяются последовательно.
                    self.apply_narrow(state, binary.lhs, true);
                    self.apply_narrow(state, binary.rhs, true);
                } else {
                    // Ложно левое или ложно правое при истинном левом.
                    let mut left = state.clone();
                    self.apply_narrow(&mut left, binary.lhs, false);
                    let mut right = state.clone();
                    self.apply_narrow(&mut right, binary.lhs, true);
                    self.apply_narrow(&mut right, binary.rhs, false);
                    *state = merge_states(left, right);
                }
            }
            BinaryOperator::Or(_) | BinaryOperator::LowOr(_) => {
                if truthy {
                    // Истинно левое или истинно правое при ложном левом.
                    let mut left = state.clone();
                    self.apply_narrow(&mut left, binary.lhs, true);
                    let mut right = state.clone();
                    self.apply_narrow(&mut right, binary.lhs, false);
                    self.apply_narrow(&mut right, binary.rhs, true);
                    *state = merge_states(left, right);
                } else {
                    self.apply_narrow(state, binary.lhs, false);
                    self.apply_narrow(state, binary.rhs, false);
                }
            }
            BinaryOperator::Identical(_) | BinaryOperator::NotIdentical(_) => {
                let identical = matches!(binary.operator, BinaryOperator::Identical(_));
                self.apply_equality_narrow(state, binary.lhs, binary.rhs, identical == truthy);
            }
            BinaryOperator::Instanceof(_) => {
                let Expression::Variable(Variable::Direct(variable)) = binary.lhs.unparenthesized()
                else {
                    return;
                };
                let name = lossy(variable.name);
                let class = self.class_name(binary.rhs);
                if truthy {
                    if let Some(class) = class {
                        state.types.insert(name, class);
                    }
                } else if let Some(class) = class
                    && state.types.get(&name) == Some(&class)
                {
                    state.types.remove(&name);
                }
            }
            _ => {}
        }
    }

    /// `$x === null` / `$x === true` / `$x === 'k'` и отрицания.
    fn apply_equality_narrow(
        &self,
        state: &mut FlowState,
        lhs: &Expression<'a>,
        rhs: &Expression<'a>,
        equal: bool,
    ) {
        if is_null_literal(rhs) || is_null_literal(lhs) {
            let name = if is_null_literal(rhs) {
                direct_var_name(lhs)
            } else {
                direct_var_name(rhs)
            };
            let Some(name) = name else {
                return;
            };
            if equal {
                state.nulls.insert(name);
            } else {
                state.nulls.remove(&name);
            }
            return;
        }
        let variable = direct_var_name(lhs).or_else(|| direct_var_name(rhs));
        let Some(name) = variable else {
            return;
        };
        let literal = if direct_var_name(lhs).is_some() {
            rhs
        } else {
            lhs
        };
        match literal.unparenthesized() {
            Expression::Literal(Literal::True(_)) if equal => {
                state.bools.insert(name, true);
            }
            Expression::Literal(Literal::False(_)) if equal => {
                state.bools.insert(name, false);
            }
            Expression::Literal(Literal::String(_)) | Expression::Literal(Literal::Integer(_))
                if equal =>
            {
                state.bools.remove(&name);
                state.nulls.remove(&name);
            }
            _ => {}
        }
    }

    fn apply_isset_narrow(
        &self,
        state: &mut FlowState,
        values: &TokenSeparatedSequence<'a, &'a Expression<'a>>,
        truthy: bool,
    ) {
        for value in values.iter() {
            if let Some(name) = direct_var_name(value) {
                if truthy {
                    state.nulls.remove(&name);
                }
                continue;
            }
            let Expression::ArrayAccess(access) = value.unparenthesized() else {
                continue;
            };
            let Some(base) = direct_var_name(access.array) else {
                continue;
            };
            let Some((key, _)) = self.literal_key(access.index) else {
                continue;
            };
            let offsets = state.offsets.entry(base).or_default();
            if truthy {
                offsets.insert(key);
            } else {
                offsets.remove(&key);
            }
        }
    }

    /// `is_null($x)`, `array_key_exists('k', $a)` и их отрицания.
    fn apply_call_narrow(&self, state: &mut FlowState, call: &Call<'a>, truthy: bool) {
        let Some(target) = self.call_target(call) else {
            return;
        };
        let short = target
            .rsplit('\\')
            .next()
            .unwrap_or(&target)
            .to_ascii_lowercase();
        let arguments = positional_values(call.get_argument_list());
        match short.as_str() {
            "is_null" => {
                let Some(value) = arguments.first() else {
                    return;
                };
                let Some(name) = direct_var_name(value) else {
                    return;
                };
                if truthy {
                    state.nulls.insert(name);
                } else {
                    state.nulls.remove(&name);
                }
            }
            "array_key_exists" => {
                let (Some(key), Some(array)) = (arguments.first(), arguments.get(1)) else {
                    return;
                };
                // Нелитеральный ключ ничего не уточняет.
                let Some((key, _)) = self.literal_key(key) else {
                    return;
                };
                let Some(base) = direct_var_name(array) else {
                    return;
                };
                let offsets = state.offsets.entry(base).or_default();
                if truthy {
                    offsets.insert(key);
                } else {
                    offsets.remove(&key);
                }
            }
            _ => {}
        }
    }

    /// `switch`: реальный fallthrough. Каждый `case` стартует с текущего
    /// fallthrough-состояния; `break`/`continue` завершают путь и отдают
    /// состояние в выход, а следующий `case` снова стартует с состояния до
    /// `switch`. Без `default` возможен путь «ничего не совпало».
    fn switch_statement(&mut self, node: &Switch<'a>) {
        self.condition(node.expression);
        let before = self.state.clone();
        let subject = direct_var_name(node.expression);
        let has_default = node.body.has_default_case();
        self.jumps.push(JumpFrame::new(JumpKind::Switch));
        let mut exits: Option<FlowState> = None;
        let mut fall = before.clone();
        let mut last_fell = false;
        for case in node.body.cases() {
            let mut entry = fall.clone();
            entry.terminated = false;
            let mut reachable = true;
            if let Some(expression) = case.expression() {
                self.expression_statement(expression);
                if let Some(subject) = &subject {
                    reachable = self.apply_literal(&mut entry, subject, expression);
                }
            }
            self.state = entry;
            self.statements(case.statements());
            let (breaks, _continues) = self.drain_jumps();
            let terminated = self.state.terminated;
            let end = (!terminated).then(|| self.state.clone());
            if reachable {
                for state in breaks {
                    join_state(&mut exits, state);
                }
                fall = match (terminated, end) {
                    (false, Some(end)) => end,
                    _ => before.clone(),
                };
            } else {
                fall = before.clone();
            }
            last_fell = reachable && !terminated;
        }
        self.jumps.pop();
        let mut post = exits;
        if last_fell {
            join_state(&mut post, fall);
        }
        if !has_default {
            join_state(&mut post, before.clone());
        }
        match post {
            Some(state) => self.state = state,
            None => {
                self.state = before;
                self.state.terminated = true;
            }
        }
    }

    /// Уточнить состояние при совпадении субъекта с литералом. `false` —
    /// ветка недостижима (противоречие известным фактам).
    fn apply_literal(&self, state: &mut FlowState, subject: &str, value: &Expression<'a>) -> bool {
        match value.unparenthesized() {
            Expression::Literal(Literal::True(_)) => {
                if state.bools.get(subject) == Some(&false) {
                    return false;
                }
                state.bools.insert(subject.to_string(), true);
            }
            Expression::Literal(Literal::False(_)) => {
                if state.bools.get(subject) == Some(&true) {
                    return false;
                }
                state.bools.insert(subject.to_string(), false);
            }
            Expression::Literal(Literal::Null(_)) => {
                state.nulls.insert(subject.to_string());
            }
            Expression::Literal(Literal::String(_)) | Expression::Literal(Literal::Integer(_)) => {
                if state.nulls.contains(subject) {
                    return false;
                }
                state.bools.remove(subject);
            }
            _ => {}
        }
        true
    }

    /// `try` / `catch` / `finally`: тело `try` с текущего состояния, каждое
    /// `catch` — с копии состояния до `try` (исключение возможно до первого
    /// оператора). Слияние: may — union, must — пересечение; `finally` — один
    /// раз на слитом состоянии.
    fn try_statement(&mut self, node: &Try<'a>) {
        let before = self.state.clone();
        self.statements(node.block.statements.as_slice());
        let mut merged = self.state.clone();
        for clause in node.catch_clauses.iter() {
            self.state = before.clone();
            if let Some(variable) = &clause.variable {
                let name = lossy(variable.name);
                self.set_var(&name, &["unknown".to_string()], &[]);
                self.state.offsets.remove(&name);
                if let Some(ty) = hint_type(self.parsed, &self.resolver, &clause.hint) {
                    self.state.types.insert(name, ty);
                }
            }
            self.statements(clause.block.statements.as_slice());
            let arm = self.state.clone();
            merged = merge_states(merged, arm);
        }
        self.state = merged;
        if let Some(finally) = &node.finally_clause {
            let terminated = self.state.terminated;
            self.state.terminated = false;
            self.statements(finally.block.statements.as_slice());
            self.state.terminated |= terminated;
        }
    }

    /// Статически известное значение условия, если его можно решить.
    fn truth_value(&self, expression: &Expression<'a>) -> Option<bool> {
        match expression.unparenthesized() {
            Expression::Literal(Literal::True(_)) => Some(true),
            Expression::Literal(Literal::False(_)) => Some(false),
            Expression::Variable(Variable::Direct(variable)) => {
                self.state.bools.get(&lossy(variable.name)).copied()
            }
            Expression::UnaryPrefix(unary)
                if matches!(unary.operator, UnaryPrefixOperator::Not(_)) =>
            {
                self.truth_value(unary.operand).map(|value| !value)
            }
            Expression::Parenthesized(inner) => self.truth_value(inner.expression),
            Expression::Binary(binary) => match binary.operator {
                BinaryOperator::Identical(_) => self.null_value(binary.lhs, binary.rhs),
                BinaryOperator::NotIdentical(_) => {
                    self.null_value(binary.lhs, binary.rhs).map(|value| !value)
                }
                _ => None,
            },
            _ => None,
        }
    }

    /// `$x === null`, где `$x` уже известен как `null` на текущем пути.
    fn null_value(&self, lhs: &Expression<'a>, rhs: &Expression<'a>) -> Option<bool> {
        let name = if is_null_literal(rhs) {
            direct_var_name(lhs)
        } else if is_null_literal(lhs) {
            direct_var_name(rhs)
        } else {
            None
        }?;
        self.state.nulls.contains(&name).then_some(true)
    }

    /// Переполнение капов: ветки уже пройдены последовательно, must сброшен
    /// для переменных, которые они присваивают.
    fn conservative_after(&mut self, before: &FlowState) {
        self.state
            .bools
            .retain(|name, value| before.bools.get(name) == Some(value));
        self.state.nulls.retain(|name| before.nulls.contains(name));
        self.state
            .types
            .retain(|name, ty| before.types.get(name) == Some(ty));
        self.state
            .offsets
            .retain(|base, keys| before.offsets.get(base).is_some_and(|known| known == keys));
        let changed: Vec<String> = self
            .state
            .vars
            .iter()
            .filter(|(name, origins)| before.vars.get(*name) != Some(*origins))
            .map(|(name, _)| name.clone())
            .collect();
        for name in changed {
            self.state.must.remove(&name);
        }
    }

    /// Выражение-инструкция: вызов тоже даёт чтение цели (нужно для трассировки).
    fn expression_statement(&mut self, expression: &Expression<'a>) {
        match expression.unparenthesized() {
            Expression::Call(call) => {
                self.call(call, true);
            }
            Expression::Assignment(assignment) => {
                self.assignment(assignment);
            }
            Expression::Instantiation(instantiation) => {
                self.instantiation(instantiation);
            }
            _ => {
                self.origins(expression);
            }
        }
    }

    fn condition(&mut self, expression: &Expression<'a>) {
        let must = self.origins_must(expression);
        let origins = self.origins(expression);
        let expr = self.one_line(expression);
        if significant(&origins) {
            let origins = norm(origins);
            let must = must_within(must, &origins);
            self.flow.reads.push(Read {
                line: self.line_of(expression),
                expr,
                origins,
                must,
            });
        }
    }

    // -- присваивания -------------------------------------------------------

    /// `=`-присваивание: `Assign`, обновление состояния и типов.
    fn assignment(&mut self, node: &Assignment<'a>) -> Vec<String> {
        if !node.operator.is_assign() {
            return Vec::new();
        }
        let origins = self.origins(node.rhs);
        let must = self.origins_must(node.rhs);
        let line = self.line_of(node);
        let ty = self.value_type(node.rhs);
        match node.lhs.unparenthesized() {
            Expression::Variable(Variable::Direct(variable)) => {
                let name = lossy(variable.name);
                self.set_var(&name, &origins, &must);
                self.facts(&name, node.rhs);
                if let Some(ty) = ty {
                    self.state.types.insert(name.clone(), ty);
                }
                self.flow.assigns.push(Assign {
                    line,
                    lhs: name.clone(),
                    origins: origins.clone(),
                    must: must_within(must.clone(), &origins),
                });
                // `global $x`: значение живёт и в глобальной области.
                if self.globals.contains(&name) {
                    self.flow.assigns.push(Assign {
                        line,
                        lhs: format!("prop:global::{}", name.trim_start_matches('$')),
                        origins: origins.clone(),
                        must: must_within(must, &origins),
                    });
                }
            }
            Expression::Access(Access::Property(access)) => {
                let ClassLikeMemberSelector::Identifier(property) = &access.property else {
                    return origins;
                };
                let name = lossy(property.value);
                if is_this(access.object) {
                    if self.class.is_some() {
                        self.set_var(&format!("this->{name}"), &origins, &must);
                        if let Some(ty) = ty {
                            self.state.types.insert(format!("this->{name}"), ty);
                        }
                        self.flow.assigns.push(Assign {
                            line,
                            lhs: format!("prop:this::{name}"),
                            origins: origins.clone(),
                            must: must_within(must, &origins),
                        });
                    }
                } else if let Some(class) = self.type_of(access.object) {
                    self.flow.assigns.push(Assign {
                        line,
                        lhs: format!("prop:{class}::{name}"),
                        origins: origins.clone(),
                        must: must_within(must, &origins),
                    });
                }
            }
            Expression::Access(Access::StaticProperty(access)) => {
                if let Some(atom) = self.static_property_atom(access) {
                    self.flow.assigns.push(Assign {
                        line,
                        lhs: atom,
                        origins: origins.clone(),
                        must: must_within(must, &origins),
                    });
                }
            }
            Expression::ArrayAccess(access) => {
                let Some(base) = direct_var_name(access.array) else {
                    return origins;
                };
                // Запись в элемент: значение всего массива не гарантировано.
                self.set_var(&base, &origins, &[]);
                if let Some((_, text)) = self.literal_key(access.index) {
                    self.flow.assigns.push(Assign {
                        line,
                        lhs: format!("{base}[{text}]"),
                        origins: origins.clone(),
                        must: Vec::new(),
                    });
                }
            }
            Expression::ArrayAppend(append) => {
                let Some(base) = direct_var_name(append.array) else {
                    return origins;
                };
                self.set_var(&base, &origins, &[]);
                self.flow.assigns.push(Assign {
                    line,
                    lhs: format!("{base}[]"),
                    origins: origins.clone(),
                    must: Vec::new(),
                });
            }
            Expression::List(list) => {
                self.destructure(&list.elements, &origins, line);
            }
            Expression::Array(array) => {
                self.destructure(&array.elements, &origins, line);
            }
            _ => {}
        }
        origins
    }

    /// Деструктуризация `list(…)` / `[…]`: каждой привязанной переменной —
    /// may правой части, must пуст (элемент не гарантирован). Ключи читаются
    /// как обычные выражения.
    fn destructure(
        &mut self,
        elements: &TokenSeparatedSequence<'a, ArrayElement<'a>>,
        origins: &[String],
        line: u32,
    ) {
        for element in elements.iter() {
            match element {
                ArrayElement::KeyValue(item) => {
                    self.origins(item.key);
                    self.destructure_value(item.value, origins, line);
                }
                ArrayElement::Value(item) => self.destructure_value(item.value, origins, line),
                ArrayElement::Variadic(item) => self.destructure_value(item.value, origins, line),
                ArrayElement::Missing(_) => {}
            }
        }
    }

    fn destructure_value(&mut self, value: &Expression<'a>, origins: &[String], line: u32) {
        match value.unparenthesized() {
            Expression::List(list) => self.destructure(&list.elements, origins, line),
            Expression::Array(array) => self.destructure(&array.elements, origins, line),
            _ => {
                if let Some(name) = direct_var_name(value) {
                    self.set_var(&name, origins, &[]);
                    self.flow.assigns.push(Assign {
                        line,
                        lhs: name,
                        origins: origins.to_vec(),
                        must: Vec::new(),
                    });
                }
            }
        }
    }

    // -- выражения ----------------------------------------------------------

    /// Истоки значения выражения (may); попутно пишет чтения и рёбра аргументов.
    /// Истоки элементов массива-литерала (объединение значений; ключи не истоки).
    fn array_origins(
        &mut self,
        elements: &TokenSeparatedSequence<'a, ArrayElement<'a>>,
    ) -> Vec<String> {
        let mut origins = Vec::new();
        for element in elements.iter() {
            match element {
                ArrayElement::Value(value) => origins.extend(self.origins(value.value)),
                ArrayElement::KeyValue(pair) => origins.extend(self.origins(pair.value)),
                _ => {}
            }
        }
        norm(origins)
    }

    /// `match`: каждое плечо стартует с состояния до `match`, уточнённого
    /// литеральными условиями. Результат — union значений; `must` — только
    /// при наличии `default`.
    fn match_origins(&mut self, node: &Match<'a>) -> Vec<String> {
        self.condition(node.expression);
        let before = self.state.clone();
        let subject = direct_var_name(node.expression);
        let mut origins = Vec::new();
        for arm in node.arms.iter() {
            let mut entry = before.clone();
            let mut reachable = true;
            if let MatchArm::Expression(arm) = arm {
                for condition in arm.conditions.iter() {
                    self.origins(condition);
                    if let Some(subject) = &subject {
                        reachable &= self.apply_literal(&mut entry, subject, condition);
                    }
                }
            }
            self.state = entry;
            let value = self.origins(arm.expression());
            self.state = before.clone();
            if reachable {
                origins.extend(value);
            }
        }
        self.state = before;
        if origins.is_empty() {
            vec!["unknown".into()]
        } else {
            norm(origins)
        }
    }

    /// `must` результата `match`: все плечи должны совпасть, иначе пусто.
    /// `default` обязателен: без него возможно «ни одно плечо».
    fn match_must(&self, node: &Match<'a>) -> Vec<String> {
        let has_default = node.arms.iter().any(|arm| arm.is_default());
        if !has_default {
            return Vec::new();
        }
        let mut arms: Vec<Vec<String>> = Vec::new();
        for arm in node.arms.iter() {
            arms.push(self.origins_must(arm.expression()));
        }
        let Some(first) = arms.first() else {
            return Vec::new();
        };
        if arms.iter().all(|must| must == first) {
            first.clone()
        } else {
            Vec::new()
        }
    }

    fn origins(&mut self, expression: &Expression<'a>) -> Vec<String> {
        match expression.unparenthesized() {
            Expression::Variable(Variable::Direct(variable)) => {
                let name = lossy(variable.name);
                let known = self.state.vars.get(&name).cloned();
                if let Some(origins) = &known
                    && significant(origins)
                {
                    return origins.clone();
                }
                // `extract($arr)` наполняет область видимости ключами массива.
                if self.state.extracted && name != "$this" {
                    let atom = format!("key:{}", name.trim_start_matches('$'));
                    self.record_read(expression, vec![atom.clone()], Vec::new());
                    return vec![atom];
                }
                known.unwrap_or_else(|| vec!["unknown".into()])
            }
            Expression::Variable(_) => vec!["unknown".into()],
            Expression::Literal(_) | Expression::Identifier(_) => vec!["lit".into()],
            Expression::Access(Access::Property(access)) => {
                self.origins(access.object);
                let (origins, must) = self.property_read(access.object, &access.property);
                self.record_read(expression, origins.clone(), must);
                origins
            }
            Expression::Access(Access::NullSafeProperty(access)) => {
                self.origins(access.object);
                let (origins, must) = self.property_read(access.object, &access.property);
                self.record_read(expression, origins.clone(), must);
                origins
            }
            Expression::Access(Access::StaticProperty(access)) => {
                let origins = self
                    .static_property_atom(access)
                    .map(|atom| vec![atom])
                    .unwrap_or_else(|| vec!["unknown".into()]);
                self.record_read(expression, origins.clone(), origins.clone());
                origins
            }
            Expression::Access(_) => vec!["lit".into()],
            Expression::ArrayAccess(access) => {
                let mut origins = self.origins(access.array);
                let key = self.literal_key(access.index);
                let mut must = self.origins_must(access.array);
                match &key {
                    Some((name, _)) => {
                        origins.push(format!("key:{name}"));
                        must.push(format!("key:{name}"));
                    }
                    None => {
                        origins.extend(self.origins(access.index));
                        must.extend(self.origins_must(access.index));
                    }
                }
                if key.is_some() {
                    self.record_read(expression, origins.clone(), must.clone());
                }
                norm(origins)
            }
            Expression::ArrayAppend(access) => self.origins(access.array),
            Expression::Call(call) => self.call(call, true),
            Expression::Instantiation(instantiation) => self.instantiation(instantiation),
            Expression::Assignment(assignment) => self.assignment(assignment),
            Expression::Yield(node) => self.yield_expression(node),
            Expression::Binary(binary) => {
                let mut origins = self.origins(binary.lhs);
                origins.extend(self.origins(binary.rhs));
                // `$k === 'name_en'`, где `$k` — ключ `foreach`: колонка-ключ.
                if matches!(
                    binary.operator,
                    BinaryOperator::Identical(_) | BinaryOperator::Equal(_)
                ) && let Some(key) = self.key_literal(binary.lhs, binary.rhs)
                {
                    self.record_read(expression, vec![format!("key:{key}")], Vec::new());
                }
                norm(origins)
            }
            Expression::UnaryPrefix(unary) => self.origins(unary.operand),
            Expression::UnaryPostfix(unary) => self.origins(unary.operand),
            Expression::Parenthesized(inner) => self.origins(inner.expression),
            Expression::Conditional(conditional) => {
                let condition_must = self.origins_must(conditional.condition);
                let condition = self.origins(conditional.condition);
                self.record_read(conditional.condition, condition.clone(), condition_must);
                let mut origins = condition;
                if let Some(then) = conditional.then {
                    origins.extend(self.origins(then));
                }
                origins.extend(self.origins(conditional.r#else));
                norm(origins)
            }
            Expression::Match(node) => self.match_origins(node),
            Expression::Construct(_) => vec!["unknown".into()],
            Expression::CompositeString(_) => vec!["lit".into()],
            Expression::Array(array) => self.array_origins(&array.elements),
            Expression::LegacyArray(array) => self.array_origins(&array.elements),
            Expression::List(_) => vec!["lit".into()],
            Expression::Closure(closure) => {
                let before = self.state.clone();
                self.closure_params(&closure.parameter_list);
                self.statements(closure.body.statements.as_slice());
                self.merge_closure(before);
                vec!["unknown".into()]
            }
            Expression::ArrowFunction(function) => {
                let before = self.state.clone();
                self.closure_params(&function.parameter_list);
                self.origins(function.expression);
                self.merge_closure(before);
                vec!["unknown".into()]
            }
            _ => vec!["unknown".into()],
        }
    }

    /// Истоки значения выражения (must); без записи чтений и рёбер.
    fn origins_must(&self, expression: &Expression<'a>) -> Vec<String> {
        match expression.unparenthesized() {
            Expression::Variable(Variable::Direct(variable)) => self
                .state
                .must
                .get(&lossy(variable.name))
                .cloned()
                .unwrap_or_default(),
            Expression::Variable(_) => Vec::new(),
            Expression::Literal(_) | Expression::Identifier(_) => vec!["lit".into()],
            Expression::Access(Access::Property(access)) => {
                self.property_read(access.object, &access.property).1
            }
            Expression::Access(Access::NullSafeProperty(access)) => {
                self.property_read(access.object, &access.property).1
            }
            Expression::Access(Access::StaticProperty(access)) => self
                .static_property_atom(access)
                .map(|atom| vec![atom])
                .unwrap_or_default(),
            Expression::Access(_) => vec!["lit".into()],
            Expression::ArrayAccess(access) => {
                let mut must = self.origins_must(access.array);
                match self.literal_key(access.index) {
                    Some((name, _)) => must.push(format!("key:{name}")),
                    None => must.extend(self.origins_must(access.index)),
                }
                norm(must)
            }
            Expression::ArrayAppend(access) => self.origins_must(access.array),
            Expression::Call(call) => self.call_must(call),
            Expression::Instantiation(node) => self
                .class_name(node.class)
                .map(|class| vec![format!("new:{class}@{}", self.line_of(node))])
                .unwrap_or_default(),
            Expression::Binary(binary) => {
                let left = self.origins_must(binary.lhs);
                let right = self.origins_must(binary.rhs);
                intersect(&left, &right)
            }
            Expression::UnaryPrefix(unary) => self.origins_must(unary.operand),
            Expression::UnaryPostfix(unary) => self.origins_must(unary.operand),
            Expression::Parenthesized(inner) => self.origins_must(inner.expression),
            Expression::Conditional(conditional) => {
                let then = match conditional.then {
                    Some(then) => self.origins_must(then),
                    None => self.origins_must(conditional.condition),
                };
                let otherwise = self.origins_must(conditional.r#else);
                intersect(&then, &otherwise)
            }
            Expression::Match(node) => self.match_must(node),
            _ => Vec::new(),
        }
    }

    // -- генераторы ---------------------------------------------------------

    /// `yield <expr>`, `yield <key> => <expr>`, `yield from <expr>`: значение
    /// попадает в генератор, поэтому пишется в `returns` (для `c:@site`).
    fn yield_expression(&mut self, node: &Yield<'a>) -> Vec<String> {
        match node {
            Yield::Value(value) => match value.value {
                Some(expression) => self.yielded(expression),
                None => vec!["unknown".into()],
            },
            Yield::Pair(pair) => {
                self.origins(pair.key);
                self.yielded(pair.value)
            }
            Yield::From(from) => self.yielded(from.iterator),
        }
    }

    fn yielded(&mut self, expression: &Expression<'a>) -> Vec<String> {
        let origins = self.origins(expression);
        let must = self.origins_must(expression);
        if significant(&origins) {
            let origins = norm(origins);
            let must = must_within(must, &origins);
            self.flow.returns.push(Read {
                line: self.line_of(expression),
                expr: self.one_line(expression),
                origins: origins.clone(),
                must,
            });
            origins
        } else {
            origins
        }
    }

    // -- вызовы -------------------------------------------------------------

    /// Вызов: рёбра аргументов, исток `c:<target>@<line>` и (для значения) чтение.
    fn call(&mut self, call: &Call<'a>, used: bool) -> Vec<String> {
        if let Some(origins) = self.array_column(call, used) {
            return origins;
        }
        if let Some(origins) = self.call_user_func(call, used) {
            return origins;
        }
        if let Some(origins) = self.builtin_call(call, used) {
            return origins;
        }
        let object = match call {
            Call::Method(node) => Some(node.object),
            Call::NullSafeMethod(node) => Some(node.object),
            Call::StaticMethod(node) => {
                self.origins(node.class);
                None
            }
            Call::Function(_) => None,
        };
        let target = self.call_target(call);
        let line = self.line_of(call);
        if let Some(object) = object
            && let Some(target) = &target
        {
            let origins = if is_this(object) {
                vec!["this".to_string()]
            } else {
                self.origins(object)
            };
            self.flow.receivers.push(ReceiverEdge {
                line,
                target: target.clone(),
                origins: norm(origins),
            });
        }
        self.arguments(call.get_argument_list(), target.as_deref(), line);
        match target {
            Some(target) => {
                let origins = vec![format!("c:{target}@{line}")];
                if used {
                    self.record_read(call, origins.clone(), origins.clone());
                }
                origins
            }
            None => vec!["unknown".into()],
        }
    }

    /// Встроенные функции с прозрачными истоками: `array_*`, `extract`,
    /// `compact`, `get_object_vars`, `array_map`.
    fn builtin_call(&mut self, call: &Call<'a>, used: bool) -> Option<Vec<String>> {
        let Call::Function(_) = call else {
            return None;
        };
        let name = self.call_target(call)?;
        let short = name
            .rsplit('\\')
            .next()
            .unwrap_or(&name)
            .to_ascii_lowercase();
        let line = self.line_of(call);
        let arguments = call.get_argument_list();
        let origins = match short.as_str() {
            // Массив на входе, массив на выходе: истоки проходят насквозь.
            "array_values" | "array_filter" | "array_merge" | "array_slice" | "array_reverse"
            | "array_unique" | "array_diff" | "array_intersect" | "array_splice" => {
                let mut origins = Vec::new();
                for argument in arguments.arguments.iter() {
                    origins.extend(self.origins(argument_value(argument)));
                }
                norm(origins)
            }
            // `extract($arr)`: дальше неизвестные переменные — ключи массива.
            "extract" => {
                let mut origins = Vec::new();
                for argument in arguments.arguments.iter() {
                    origins.extend(self.origins(argument_value(argument)));
                }
                self.state.extracted = true;
                norm(origins)
            }
            // `compact('a', 'b')`: значения названных переменных.
            "compact" => {
                let mut origins = Vec::new();
                for argument in arguments.arguments.iter() {
                    let value = argument_value(argument);
                    let Expression::Literal(Literal::String(string)) = value.unparenthesized()
                    else {
                        origins.extend(self.origins(value));
                        continue;
                    };
                    let name = string.value.map(lossy).unwrap_or_else(|| lossy(string.raw));
                    match self.state.vars.get(&format!("${name}")) {
                        Some(found) => origins.extend(found.clone()),
                        None => origins.push(format!("key:{name}")),
                    }
                }
                norm(origins)
            }
            // `get_object_vars($this)`: все объявленные свойства класса.
            "get_object_vars" => {
                let mut origins = Vec::new();
                for argument in arguments.arguments.iter() {
                    let value = argument_value(argument);
                    if is_this(value) {
                        for property in &self.props {
                            origins.push(format!("prop:this::{property}"));
                        }
                    } else {
                        origins.extend(self.origins(value));
                    }
                }
                norm(origins)
            }
            "array_map" => self.array_map(arguments, line),
            _ => return None,
        };
        let origins = norm(origins);
        if used {
            self.record_read(call, origins.clone(), Vec::new());
        }
        Some(origins)
    }

    /// `array_map(callback, ...arrays)`: истоки результата — по цели callback
    /// (`c:<target>@<line>`), телу замыкания/стрелочной функции или массивам.
    fn array_map(&mut self, arguments: &ArgumentList<'a>, line: u32) -> Vec<String> {
        let mut arrays = Vec::new();
        let mut result: Vec<String> = Vec::new();
        for (index, argument) in arguments.arguments.iter().enumerate() {
            let value = argument_value(argument);
            if index == 0 {
                match value.unparenthesized() {
                    Expression::Closure(closure) => {
                        result = self.closure_returns(closure);
                        continue;
                    }
                    Expression::ArrowFunction(function) => {
                        let before = self.state.clone();
                        self.closure_params(&function.parameter_list);
                        result = self.origins(function.expression);
                        self.merge_closure(before);
                        continue;
                    }
                    _ => {}
                }
                if let Some(target) = self.callable_target(value) {
                    result = vec![format!("c:{target}@{line}")];
                    continue;
                }
            }
            arrays.extend(self.origins(value));
        }
        if result.is_empty() {
            norm(arrays)
        } else {
            norm(result)
        }
    }

    /// Тело замыкания в отдельном анализаторе: union истоков его `returns`.
    fn closure_returns(&mut self, closure: &Closure<'a>) -> Vec<String> {
        let mut nested = Analyzer::new(
            self.parsed,
            self.resolver.clone(),
            self.class.clone(),
            self.method.clone(),
            self.state.types.clone(),
            self.method_types,
            ClassFacts {
                magic: self.magic.clone(),
                props: self.props.clone(),
            },
        );
        nested.state.vars = self.state.vars.clone();
        nested.state.must = self.state.must.clone();
        nested.state.keys = self.state.keys.clone();
        nested.state.extracted = self.state.extracted;
        nested.closure_params(&closure.parameter_list);
        nested.statements(closure.body.statements.as_slice());
        let mut origins = Vec::new();
        for ret in &nested.flow.returns {
            origins.extend(ret.origins.iter().cloned());
        }
        self.flow.reads.extend(nested.flow.reads);
        norm(origins)
    }

    /// `must`-исток вызова: для `array_column`/`call_user_func*` — тот же
    /// атом, что и в `origins`, иначе обычный `c:<target>@<line>`.
    fn call_must(&self, call: &Call<'a>) -> Vec<String> {
        if let Some(name) = self.array_column_name(call) {
            return vec![format!("key:{name}")];
        }
        if let Some(callee) = self.direct_callee(call) {
            return vec![format!("c:{callee}@{}", self.line_of(call))];
        }
        self.call_target(call)
            .map(|target| vec![format!("c:{target}@{}", self.line_of(call))])
            .unwrap_or_default()
    }

    /// `array_column($rows, 'name_en')`: результат — значения колонки,
    /// поэтому исток — `key:name_en`. Нелитеральная колонка — обычный вызов.
    fn array_column(&mut self, call: &Call<'a>, used: bool) -> Option<Vec<String>> {
        let name = self.array_column_name(call)?;
        let line = self.line_of(call);
        let target = self.call_target(call)?;
        self.arguments(call.get_argument_list(), Some(&target), line);
        let origins = vec![format!("key:{name}")];
        if used {
            self.record_read(call, origins.clone(), origins.clone());
        }
        Some(origins)
    }

    fn array_column_name(&self, call: &Call<'a>) -> Option<String> {
        let target = self.call_target(call)?;
        if !target.eq_ignore_ascii_case("array_column") {
            return None;
        }
        let argument = positional_values(call.get_argument_list())
            .into_iter()
            .nth(1)?;
        match argument.unparenthesized() {
            Expression::Literal(Literal::String(string)) => {
                Some(string.value.map(lossy).unwrap_or_else(|| lossy(string.raw)))
            }
            _ => None,
        }
    }

    /// `call_user_func` / `call_user_func_array` со статически известной
    /// вызываемой целью: настоящий `c:<target>@<line>` и рёбра остальных
    /// аргументов (индексы с 0, сама цель не считается аргументом).
    fn call_user_func(&mut self, call: &Call<'a>, used: bool) -> Option<Vec<String>> {
        let callee = self.direct_callee(call)?;
        let name = self.call_target(call)?;
        let short = name
            .rsplit('\\')
            .next()
            .unwrap_or(&name)
            .to_ascii_lowercase();
        let line = self.line_of(call);
        let arguments = positional_values(call.get_argument_list());
        if short == "call_user_func_array" {
            let array = *arguments.get(1)?;
            self.array_arguments(array, &callee, line)?;
        } else {
            for (index, argument) in arguments.iter().skip(1).enumerate() {
                let origins = self.origins(argument);
                let must = self.origins_must(argument);
                self.flow.args.push(ArgEdge {
                    line,
                    target: callee.clone(),
                    index,
                    origins: norm(origins),
                    must: norm(must),
                    name: None,
                    spread: false,
                });
            }
        }
        let origins = vec![format!("c:{callee}@{line}")];
        if used {
            self.record_read(call, origins.clone(), origins.clone());
        }
        Some(origins)
    }

    /// Цель `call_user_func*`, если она известна статически (иначе `None` —
    /// остаётся динамическое поведение).
    fn direct_callee(&self, call: &Call<'a>) -> Option<String> {
        let Call::Function(_) = call else {
            return None;
        };
        let name = self.call_target(call)?;
        let short = name.rsplit('\\').next().unwrap_or(&name);
        let arguments = positional_values(call.get_argument_list());
        let callable = *arguments.first()?;
        if short.eq_ignore_ascii_case("call_user_func") {
            return self.callable_target(callable);
        }
        if short.eq_ignore_ascii_case("call_user_func_array") {
            let array = arguments.get(1)?;
            if matches!(
                array.unparenthesized(),
                Expression::Array(_) | Expression::LegacyArray(_)
            ) {
                return self.callable_target(callable);
            }
        }
        None
    }

    /// Разобрать статически известную вызываемую цель: `[$this, 'm']`,
    /// `[Foo::class, 'm']`, `'Foo::bar'`, `'func'`, `$this->m(...)`.
    fn callable_target(&self, expression: &Expression<'a>) -> Option<String> {
        match expression.unparenthesized() {
            Expression::Literal(Literal::String(string)) => {
                let name = string.value.map(lossy).unwrap_or_else(|| lossy(string.raw));
                let name = name.trim().trim_start_matches('\\');
                match name.split_once("::") {
                    Some((class, method)) => {
                        let class = self.resolver.resolve_class(class.trim());
                        Some(format!("{class}::{method}"))
                    }
                    None if !name.is_empty() => Some(name.to_string()),
                    None => None,
                }
            }
            Expression::Array(array) => self.callable_pair(&array.elements),
            Expression::LegacyArray(array) => self.callable_pair(&array.elements),
            Expression::PartialApplication(PartialApplication::Function(node)) => {
                match node.function.unparenthesized() {
                    Expression::Identifier(identifier) => Some(
                        lossy(identifier.value())
                            .trim_start_matches('\\')
                            .to_string(),
                    ),
                    _ => None,
                }
            }
            Expression::PartialApplication(PartialApplication::Method(node)) => {
                let ClassLikeMemberSelector::Identifier(method) = &node.method else {
                    return None;
                };
                let class = if is_this(node.object) {
                    self.class.clone()?
                } else {
                    self.type_of(node.object)?
                };
                Some(format!("{class}::{}", lossy(method.value)))
            }
            Expression::PartialApplication(PartialApplication::StaticMethod(node)) => {
                let ClassLikeMemberSelector::Identifier(method) = &node.method else {
                    return None;
                };
                let class = self.class_name(node.class)?;
                Some(format!("{class}::{}", lossy(method.value)))
            }
            _ => None,
        }
    }

    /// Пара `[получатель, 'метод']` из литерала массива.
    fn callable_pair(
        &self,
        elements: &TokenSeparatedSequence<'a, ArrayElement<'a>>,
    ) -> Option<String> {
        let receiver = elements.first()?.get_value()?;
        let method = elements.get(1)?.get_value()?;
        let Expression::Literal(Literal::String(method)) = method.unparenthesized() else {
            return None;
        };
        let method = method.value.map(lossy).unwrap_or_else(|| lossy(method.raw));
        let class = match receiver.unparenthesized() {
            Expression::Literal(Literal::String(class)) => class
                .value
                .map(lossy)
                .unwrap_or_else(|| lossy(class.raw))
                .trim_start_matches('\\')
                .to_string(),
            Expression::Access(Access::ClassConstant(access)) => {
                let ClassLikeConstantSelector::Identifier(name) = &access.constant else {
                    return None;
                };
                if !lossy(name.value).eq_ignore_ascii_case("class") {
                    return None;
                }
                self.class_name(access.class)?
            }
            _ if is_this(receiver) => self.class.clone()?,
            _ => self.type_of(receiver)?,
        };
        Some(format!("{class}::{method}"))
    }

    /// Аргументы из литерала массива: индексы по порядку элементов.
    fn array_arguments(
        &mut self,
        expression: &Expression<'a>,
        target: &str,
        line: u32,
    ) -> Option<()> {
        let elements = match expression.unparenthesized() {
            Expression::Array(array) => &array.elements,
            Expression::LegacyArray(array) => &array.elements,
            _ => return None,
        };
        let mut index = 0;
        for element in elements.iter() {
            let value = match element {
                ArrayElement::KeyValue(item) => {
                    self.origins(item.key);
                    item.value
                }
                ArrayElement::Value(item) => item.value,
                ArrayElement::Variadic(item) => item.value,
                ArrayElement::Missing(_) => continue,
            };
            let origins = self.origins(value);
            let must = self.origins_must(value);
            self.flow.args.push(ArgEdge {
                line,
                target: target.to_string(),
                index,
                origins: norm(origins),
                must: norm(must),
                name: None,
                spread: false,
            });
            index += 1;
        }
        Some(())
    }

    fn instantiation(&mut self, node: &Instantiation<'a>) -> Vec<String> {
        let class = self.class_name(node.class);
        let line = self.line_of(node);
        if let Some(arguments) = &node.argument_list {
            let target = class.as_ref().map(|class| format!("{class}::__construct"));
            self.arguments(arguments, target.as_deref(), line);
        }
        match class {
            Some(class) => vec![format!("new:{class}@{line}")],
            None => vec!["unknown".into()],
        }
    }

    fn arguments(&mut self, list: &ArgumentList<'a>, target: Option<&str>, line: u32) {
        let Some(target) = target else {
            return;
        };
        for (index, argument) in list.arguments.iter().enumerate() {
            let (index, name, spread, value) = match argument {
                Argument::Positional(argument) => {
                    let spread = argument.ellipsis.is_some();
                    let index = if spread { usize::MAX } else { index };
                    (index, None, spread, argument.value)
                }
                Argument::Named(argument) => (
                    usize::MAX,
                    Some(lossy(argument.name.value)),
                    false,
                    argument.value,
                ),
            };
            let origins = self.origins(value);
            let must = self.origins_must(value);
            self.flow.args.push(ArgEdge {
                line,
                target: target.to_string(),
                index,
                origins: norm(origins),
                must: norm(must),
                name,
                spread,
            });
        }
    }

    fn call_target(&self, call: &Call<'a>) -> Option<String> {
        match call {
            Call::Function(node) => match node.function.unparenthesized() {
                Expression::Identifier(identifier) => Some(
                    lossy(identifier.value())
                        .trim_start_matches('\\')
                        .to_string(),
                ),
                _ => None,
            },
            Call::Method(node) => {
                let ClassLikeMemberSelector::Identifier(method) = &node.method else {
                    return None;
                };
                let class = if is_this(node.object) {
                    self.class.clone()?
                } else {
                    self.type_of(node.object)?
                };
                Some(format!("{class}::{}", lossy(method.value)))
            }
            Call::NullSafeMethod(node) => {
                let ClassLikeMemberSelector::Identifier(method) = &node.method else {
                    return None;
                };
                let class = if is_this(node.object) {
                    self.class.clone()?
                } else {
                    self.type_of(node.object)?
                };
                Some(format!("{class}::{}", lossy(method.value)))
            }
            Call::StaticMethod(node) => {
                let ClassLikeMemberSelector::Identifier(method) = &node.method else {
                    return None;
                };
                let class = self.class_name(node.class)?;
                Some(format!("{class}::{}", lossy(method.value)))
            }
        }
    }

    // -- типы и имена -------------------------------------------------------

    /// Полное имя класса из выражения-класса (`Foo`, `self`, `parent`, `\A\B`).
    fn class_name(&self, expression: &Expression<'a>) -> Option<String> {
        match expression.unparenthesized() {
            Expression::Identifier(identifier) => {
                Some(self.resolver.resolve_class(&lossy(identifier.value())))
            }
            Expression::Self_(_) | Expression::Static(_) => {
                Some(self.resolver.resolve_class("self"))
            }
            Expression::Parent(_) => Some(self.resolver.resolve_class("parent")),
            _ => None,
        }
    }

    /// Тип получателя: `$x`, `$this->p`, `new C`, вызов; только известные классы.
    fn type_of(&self, expression: &Expression<'a>) -> Option<String> {
        match expression.unparenthesized() {
            Expression::Variable(Variable::Direct(variable)) => {
                self.state.types.get(&lossy(variable.name)).cloned()
            }
            Expression::Access(Access::Property(access)) if is_this(access.object) => {
                let ClassLikeMemberSelector::Identifier(property) = &access.property else {
                    return None;
                };
                self.state
                    .types
                    .get(&format!("this->{}", lossy(property.value)))
                    .cloned()
            }
            Expression::Instantiation(instantiation) => self.class_name(instantiation.class),
            Expression::Call(call) => self.call_type(call),
            _ => None,
        }
    }

    /// Тип значения для `$x = …`.
    fn value_type(&self, expression: &Expression<'a>) -> Option<String> {
        self.type_of(expression)
    }

    /// Тип возврата вызова из таблицы проекта.
    fn call_type(&self, call: &Call<'a>) -> Option<String> {
        let target = self.call_target(call)?;
        self.method_types.get(&target).cloned()
    }

    /// Чтение свойства: `prop:<class>::<p>` для известного типа, `key:<p>` для
    /// неизвестного получателя; магический аксессор добавляет `key:<p>` к may.
    fn property_read(
        &self,
        object: &Expression<'a>,
        property: &ClassLikeMemberSelector<'a>,
    ) -> (Vec<String>, Vec<String>) {
        let ClassLikeMemberSelector::Identifier(property) = property else {
            return (vec!["unknown".into()], Vec::new());
        };
        let name = lossy(property.value);
        let (class, atom) = if is_this(object) {
            let class = self.class.clone();
            let atom = "prop:this".to_string();
            (class, atom)
        } else {
            match self.type_of(object) {
                Some(class) => (Some(class.clone()), format!("prop:{class}")),
                // Получатель неизвестен: читается колонка/ключ того же имени.
                None => return (vec![format!("key:{name}")], Vec::new()),
            }
        };
        let prop = format!("{atom}::{name}");
        let mut origins = vec![prop.clone()];
        if class
            .as_ref()
            .is_some_and(|class| self.magic.contains(class))
        {
            origins.push(format!("key:{name}"));
        }
        (origins, vec![prop])
    }

    /// `Foo::$x` / `self::$x` / `static::$x` / `parent::$x` → `prop:<class>::$x`.
    fn static_property_atom(&self, access: &StaticPropertyAccess<'a>) -> Option<String> {
        let Variable::Direct(variable) = &access.property else {
            return None;
        };
        let class = self.class_name(access.class)?;
        Some(format!("prop:{class}::{}", lossy(variable.name)))
    }

    /// Сравнение переменной-ключа со строковым литералом: `('' $k …)`.
    fn key_literal(&self, lhs: &Expression<'a>, rhs: &Expression<'a>) -> Option<String> {
        let (variable, literal) = match direct_var_name(lhs) {
            Some(name) => (name, rhs),
            None => (direct_var_name(rhs)?, lhs),
        };
        if !self.state.keys.contains(&variable) {
            return None;
        }
        let (name, _) = self.literal_key(literal)?;
        Some(name)
    }

    /// Литеральный ключ массива: (имя для `key:`, текст для `lhs`).
    fn literal_key(&self, expression: &Expression<'a>) -> Option<(String, String)> {
        match expression.unparenthesized() {
            Expression::Literal(Literal::String(string)) => {
                let name = string.value.map(lossy).unwrap_or_else(|| lossy(string.raw));
                Some((name, self.parsed.text(string.span()).trim().to_string()))
            }
            Expression::Literal(Literal::Integer(integer)) => {
                let name = integer
                    .value
                    .map(|value| value.to_string())
                    .unwrap_or_else(|| lossy(integer.raw));
                Some((name.clone(), name))
            }
            _ => None,
        }
    }

    // -- состояние и запись фактов -----------------------------------------

    fn seed_params(&mut self, list: &FunctionLikeParameterList<'a>) {
        self.flow.params = list.parameters.len();
        self.flow.param_names = list
            .parameters
            .iter()
            .map(|parameter| {
                lossy(parameter.variable.name)
                    .trim_start_matches('$')
                    .to_string()
            })
            .collect();
        for (index, parameter) in list.parameters.iter().enumerate() {
            let name = lossy(parameter.variable.name);
            let atom = format!("p:{}#{index}", self.method);
            self.state.vars.insert(name.clone(), vec![atom.clone()]);
            self.state.must.insert(name.clone(), vec![atom.clone()]);
            let ty = parameter
                .hint
                .as_ref()
                .and_then(|hint| hint_type(self.parsed, &self.resolver, hint));
            if let Some(ty) = &ty {
                self.state.types.insert(name.clone(), ty.clone());
            }
            if parameter.is_promoted_property() && self.class.is_some() {
                let property = name.trim_start_matches('$');
                self.flow.assigns.push(Assign {
                    line: self.line_of(parameter),
                    lhs: format!("prop:this::{property}"),
                    origins: vec![atom.clone()],
                    must: vec![atom],
                });
                if let Some(ty) = ty {
                    self.state.types.insert(format!("this->{property}"), ty);
                }
            }
        }
    }

    /// Параметры замыкания: отдельных `p:`-атомов у них нет.
    fn closure_params(&mut self, list: &FunctionLikeParameterList<'a>) {
        for parameter in list.parameters.iter() {
            let name = lossy(parameter.variable.name);
            self.state.vars.insert(name.clone(), vec!["unknown".into()]);
            self.state.must.remove(&name);
            self.state.bools.remove(&name);
            self.state.nulls.remove(&name);
            self.state.offsets.remove(&name);
        }
    }

    /// Слияние состояния после замыкания: замыкание может не выполниться,
    /// поэтому must — только то, что было до него.
    fn merge_closure(&mut self, before: FlowState) {
        for (name, origins) in before.vars.iter() {
            let merged = match self.state.vars.get(name) {
                Some(current) => union(current, origins),
                None => origins.clone(),
            };
            self.state.vars.insert(name.clone(), merged);
            match before.must.get(name) {
                Some(before_must) => {
                    let current = self.state.must.get(name).cloned().unwrap_or_default();
                    let common = intersect(&current, before_must);
                    if common.is_empty() {
                        self.state.must.remove(name);
                    } else {
                        self.state.must.insert(name.clone(), common);
                    }
                }
                None => {
                    self.state.must.remove(name);
                }
            }
        }
    }

    /// Установить значение переменной: may — union, must — сильное обновление.
    fn set_var(&mut self, name: &str, origins: &[String], must: &[String]) {
        let merged = match self.state.vars.get(name) {
            Some(current) => union(current, origins),
            None => norm(origins.to_vec()),
        };
        self.state.vars.insert(name.to_string(), merged);
        let must = norm(must.to_vec());
        if must.is_empty() {
            self.state.must.remove(name);
        } else {
            self.state.must.insert(name.to_string(), must);
        }
    }

    /// Булевы/`null`/ключевые факты после присваивания.
    fn facts(&mut self, name: &str, rhs: &Expression<'a>) {
        self.state.bools.remove(name);
        self.state.nulls.remove(name);
        self.state.offsets.remove(name);
        match rhs.unparenthesized() {
            Expression::Literal(Literal::True(_)) => {
                self.state.bools.insert(name.to_string(), true);
            }
            Expression::Literal(Literal::False(_)) => {
                self.state.bools.insert(name.to_string(), false);
            }
            Expression::Literal(Literal::Null(_)) => {
                self.state.nulls.insert(name.to_string());
            }
            _ => {}
        }
    }

    fn record_read(&mut self, node: &impl HasSpan, origins: Vec<String>, must: Vec<String>) {
        if !significant(&origins) {
            return;
        }
        let origins = norm(origins);
        let must = must_within(must, &origins);
        self.flow.reads.push(Read {
            line: self.parsed.lines_of(node).0,
            expr: self.one_line(node),
            origins,
            must,
        });
    }

    fn one_line(&self, node: &impl HasSpan) -> String {
        self.parsed
            .text(node.span())
            .split_whitespace()
            .collect::<Vec<_>>()
            .join(" ")
    }

    fn line_of(&self, node: &impl HasSpan) -> u32 {
        self.parsed.lines_of(node).0
    }
}

// ---------------------------------------------------------------------------
// Свободные помощники.
// ---------------------------------------------------------------------------

/// Тип из подсказки: только простые имена классов (скаляры и union/intersection — `None`).
fn hint_type(parsed: &ParsedFile<'_>, resolver: &NameResolver, hint: &Hint<'_>) -> Option<String> {
    let raw = parsed.text(hint.span());
    let raw = raw.trim().trim_start_matches('?');
    if raw.is_empty() || raw.contains(['|', '&']) {
        return None;
    }
    let lower = raw.to_ascii_lowercase();
    if matches!(
        lower.as_str(),
        "array"
            | "callable"
            | "bool"
            | "float"
            | "int"
            | "integer"
            | "iterable"
            | "mixed"
            | "never"
            | "null"
            | "object"
            | "string"
            | "true"
            | "false"
            | "void"
    ) {
        return None;
    }
    Some(resolver.resolve_class(raw))
}

/// `$this` (без учёта скобок).
fn is_this(expression: &Expression<'_>) -> bool {
    matches!(
        expression.unparenthesized(),
        Expression::Variable(Variable::Direct(variable)) if variable.name == b"$this"
    )
}

/// Имя прямой переменной (`$x`), иначе `None`.
fn direct_var_name(expression: &Expression<'_>) -> Option<String> {
    match expression.unparenthesized() {
        Expression::Variable(Variable::Direct(variable)) => Some(lossy(variable.name)),
        _ => None,
    }
}

/// Значение аргумента: позиционного или именованного.
fn argument_value<'a>(argument: &Argument<'a>) -> &'a Expression<'a> {
    match argument {
        Argument::Positional(argument) => argument.value,
        Argument::Named(argument) => argument.value,
    }
}

/// Позиционные значения аргументов вызова.
fn positional_values<'a>(list: &ArgumentList<'a>) -> Vec<&'a Expression<'a>> {
    list.arguments
        .iter()
        .filter_map(|argument| match argument {
            Argument::Positional(argument) => Some(argument.value),
            Argument::Named(_) => None,
        })
        .collect()
}

/// Есть ли в наборе атом, который стоит показывать в выдаче.
fn significant(origins: &[String]) -> bool {
    origins
        .iter()
        .any(|atom| atom != "lit" && atom != "unknown")
}

/// Дедупликация с сохранением порядка и ограничение `MAX_ATOMS`.
fn norm(mut origins: Vec<String>) -> Vec<String> {
    let mut seen = HashSet::new();
    origins.retain(|atom| seen.insert(atom.clone()));
    origins.truncate(MAX_ATOMS);
    origins
}

fn union(left: &[String], right: &[String]) -> Vec<String> {
    let mut origins = left.to_vec();
    origins.extend_from_slice(right);
    norm(origins)
}

/// Пересечение множеств с сохранением порядка левого и капом `MAX_ATOMS`.
fn intersect(left: &[String], right: &[String]) -> Vec<String> {
    let right: HashSet<&String> = right.iter().collect();
    let common: Vec<String> = left
        .iter()
        .filter(|atom| right.contains(atom))
        .cloned()
        .collect();
    norm(common)
}

/// Оставить в `must` только атомы из `origins` (инвариант `must ⊆ origins`).
fn must_within(must: Vec<String>, origins: &[String]) -> Vec<String> {
    let accepted: HashSet<&String> = origins.iter().collect();
    let kept: Vec<String> = must
        .into_iter()
        .filter(|atom| accepted.contains(atom))
        .collect();
    norm(kept)
}

/// Слияние двух достижимых состояний: may — union, must — intersection.
fn merge_states(left: FlowState, right: FlowState) -> FlowState {
    if left.terminated && right.terminated {
        return FlowState {
            terminated: true,
            ..left
        };
    }
    if left.terminated {
        return right;
    }
    if right.terminated {
        return left;
    }
    let mut vars = left.vars.clone();
    for (name, origins) in &right.vars {
        match vars.get_mut(name) {
            Some(current) => *current = union(current, origins),
            None => {
                vars.insert(name.clone(), origins.clone());
            }
        }
    }
    let mut must = HashMap::new();
    for (name, left_must) in &left.must {
        let Some(right_must) = right.must.get(name) else {
            continue;
        };
        let common = intersect(left_must, right_must);
        if !common.is_empty() {
            must.insert(name.clone(), common);
        }
    }
    let mut bools = HashMap::new();
    for (name, value) in &left.bools {
        if right.bools.get(name) == Some(value) {
            bools.insert(name.clone(), *value);
        }
    }
    let nulls = left.nulls.intersection(&right.nulls).cloned().collect();
    // Ключ гарантирован только там, где он известен на обоих путях.
    let mut offsets: HashMap<String, HashSet<String>> = HashMap::new();
    for (base, keys) in &left.offsets {
        if let Some(other) = right.offsets.get(base) {
            let common: HashSet<String> = keys.intersection(other).cloned().collect();
            if !common.is_empty() {
                offsets.insert(base.clone(), common);
            }
        }
    }
    let keys = left.keys.union(&right.keys).cloned().collect();
    let mut types = HashMap::new();
    for (name, ty) in &left.types {
        if right.types.get(name) == Some(ty) {
            types.insert(name.clone(), ty.clone());
        }
    }
    FlowState {
        vars,
        must,
        types,
        bools,
        nulls,
        offsets,
        keys,
        extracted: left.extracted || right.extracted,
        terminated: false,
    }
}

/// Литерал `null`.
fn is_null_literal(expression: &Expression<'_>) -> bool {
    matches!(
        expression.unparenthesized(),
        Expression::Literal(Literal::Null(_))
    )
}

/// Слить состояние в накопитель: `None` заменяется, иначе may union.
fn join_state(target: &mut Option<FlowState>, state: FlowState) {
    *target = Some(join_state_value(target.take(), state));
}

fn join_state_value(current: Option<FlowState>, state: FlowState) -> FlowState {
    match current {
        Some(current) => merge_states(current, state),
        None => state,
    }
}

/// Появились ли в `after` истоки, которых не было в `before` (критерий
/// остановки фиксированной точки цикла).
fn origins_grew(before: &FlowState, after: &FlowState) -> bool {
    after.vars.iter().any(|(name, atoms)| {
        let known = before.vars.get(name);
        atoms
            .iter()
            .any(|atom| known.is_none_or(|origins| !origins.contains(atom)))
    })
}

/// Переменные, которые тело может записать (синтаксически): присваивания,
/// `++`/`--`, привязки `foreach`, деструктуризация, переменные `catch`.
fn written_vars(statements: &[Statement<'_>]) -> HashSet<String> {
    let mut writes = HashSet::new();
    for statement in statements {
        collect_statement_writes(statement, &mut writes);
    }
    writes
}

fn collect_statement_writes(statement: &Statement<'_>, writes: &mut HashSet<String>) {
    match statement {
        Statement::Block(block) => {
            for statement in block.statements.as_slice() {
                collect_statement_writes(statement, writes);
            }
        }
        Statement::If(node) => {
            collect_expression_writes(node.condition, writes);
            for statement in node.body.statements() {
                collect_statement_writes(statement, writes);
            }
            for (condition, body) in node.body.else_if_clauses() {
                collect_expression_writes(condition, writes);
                for statement in body {
                    collect_statement_writes(statement, writes);
                }
            }
            if let Some(statements) = node.body.else_statements() {
                for statement in statements {
                    collect_statement_writes(statement, writes);
                }
            }
        }
        Statement::While(node) => {
            collect_expression_writes(node.condition, writes);
            for statement in node.body.statements() {
                collect_statement_writes(statement, writes);
            }
        }
        Statement::DoWhile(node) => {
            collect_statement_writes(node.statement, writes);
            collect_expression_writes(node.condition, writes);
        }
        Statement::For(node) => {
            for expression in node.initializations.iter() {
                collect_expression_writes(expression, writes);
            }
            for expression in node.conditions.iter() {
                collect_expression_writes(expression, writes);
            }
            for expression in node.increments.iter() {
                collect_expression_writes(expression, writes);
            }
            for statement in node.body.statements() {
                collect_statement_writes(statement, writes);
            }
        }
        Statement::Foreach(node) => {
            collect_expression_writes(node.expression, writes);
            collect_expression_writes(node.target.value(), writes);
            if let Some(key) = node.target.key() {
                collect_expression_writes(key, writes);
            }
            for statement in node.body.statements() {
                collect_statement_writes(statement, writes);
            }
        }
        Statement::Switch(node) => {
            collect_expression_writes(node.expression, writes);
            for case in node.body.cases() {
                if let Some(expression) = case.expression() {
                    collect_expression_writes(expression, writes);
                }
                for statement in case.statements() {
                    collect_statement_writes(statement, writes);
                }
            }
        }
        Statement::Try(node) => {
            for statement in node.block.statements.as_slice() {
                collect_statement_writes(statement, writes);
            }
            for clause in node.catch_clauses.iter() {
                if let Some(variable) = &clause.variable {
                    writes.insert(lossy(variable.name));
                }
                for statement in clause.block.statements.as_slice() {
                    collect_statement_writes(statement, writes);
                }
            }
            if let Some(finally) = &node.finally_clause {
                for statement in finally.block.statements.as_slice() {
                    collect_statement_writes(statement, writes);
                }
            }
        }
        Statement::Global(node) => {
            for variable in node.variables.iter() {
                if let Variable::Direct(variable) = variable {
                    writes.insert(lossy(variable.name));
                }
            }
        }
        Statement::Return(node) => {
            if let Some(value) = node.value {
                collect_expression_writes(value, writes);
            }
        }
        Statement::Echo(node) => {
            for value in node.values.iter() {
                collect_expression_writes(value, writes);
            }
        }
        Statement::Expression(node) => collect_expression_writes(node.expression, writes),
        _ => {}
    }
}

fn collect_expression_writes(expression: &Expression<'_>, writes: &mut HashSet<String>) {
    match expression.unparenthesized() {
        Expression::Assignment(node) => {
            collect_expression_writes(node.rhs, writes);
            collect_lhs_writes(node.lhs, writes);
        }
        Expression::UnaryPrefix(node)
            if matches!(
                node.operator,
                UnaryPrefixOperator::PreIncrement(_) | UnaryPrefixOperator::PreDecrement(_)
            ) =>
        {
            collect_lhs_writes(node.operand, writes);
        }
        Expression::UnaryPostfix(node)
            if matches!(
                node.operator,
                UnaryPostfixOperator::PostIncrement(_) | UnaryPostfixOperator::PostDecrement(_)
            ) =>
        {
            collect_lhs_writes(node.operand, writes);
        }
        Expression::Binary(node) => {
            collect_expression_writes(node.lhs, writes);
            collect_expression_writes(node.rhs, writes);
        }
        Expression::Conditional(node) => {
            collect_expression_writes(node.condition, writes);
            if let Some(then) = node.then {
                collect_expression_writes(then, writes);
            }
            collect_expression_writes(node.r#else, writes);
        }
        Expression::Call(call) => {
            match call {
                Call::Method(node) => collect_expression_writes(node.object, writes),
                Call::NullSafeMethod(node) => collect_expression_writes(node.object, writes),
                Call::StaticMethod(node) => collect_expression_writes(node.class, writes),
                Call::Function(_) => {}
            }
            for argument in call.get_argument_list().arguments.iter() {
                collect_expression_writes(argument_value(argument), writes);
            }
        }
        Expression::Instantiation(node) => {
            if let Some(arguments) = &node.argument_list {
                for argument in arguments.arguments.iter() {
                    collect_expression_writes(argument_value(argument), writes);
                }
            }
        }
        Expression::ArrayAccess(node) => {
            collect_expression_writes(node.array, writes);
            collect_expression_writes(node.index, writes);
        }
        Expression::ArrayAppend(node) => collect_expression_writes(node.array, writes),
        Expression::Array(node) => collect_element_writes(&node.elements, writes),
        Expression::LegacyArray(node) => collect_element_writes(&node.elements, writes),
        Expression::List(node) => collect_element_writes(&node.elements, writes),
        Expression::Access(Access::Property(node)) => {
            collect_expression_writes(node.object, writes)
        }
        Expression::Access(Access::NullSafeProperty(node)) => {
            collect_expression_writes(node.object, writes)
        }
        Expression::Match(node) => {
            collect_expression_writes(node.expression, writes);
            for arm in node.arms.iter() {
                if let MatchArm::Expression(arm) = arm {
                    for condition in arm.conditions.iter() {
                        collect_expression_writes(condition, writes);
                    }
                }
                collect_expression_writes(arm.expression(), writes);
            }
        }
        Expression::Yield(Yield::Value(node)) => {
            if let Some(value) = node.value {
                collect_expression_writes(value, writes);
            }
        }
        Expression::Yield(Yield::Pair(node)) => {
            collect_expression_writes(node.key, writes);
            collect_expression_writes(node.value, writes);
        }
        Expression::Yield(Yield::From(node)) => collect_expression_writes(node.iterator, writes),
        Expression::Parenthesized(node) => collect_expression_writes(node.expression, writes),
        _ => {}
    }
}

fn collect_element_writes(
    elements: &TokenSeparatedSequence<'_, ArrayElement<'_>>,
    writes: &mut HashSet<String>,
) {
    for element in elements.iter() {
        match element {
            ArrayElement::KeyValue(item) => {
                collect_expression_writes(item.key, writes);
                collect_expression_writes(item.value, writes);
            }
            ArrayElement::Value(item) => collect_expression_writes(item.value, writes),
            ArrayElement::Variadic(item) => collect_expression_writes(item.value, writes),
            ArrayElement::Missing(_) => {}
        }
    }
}

fn collect_lhs_writes(expression: &Expression<'_>, writes: &mut HashSet<String>) {
    match expression.unparenthesized() {
        Expression::Variable(Variable::Direct(variable)) => {
            writes.insert(lossy(variable.name));
        }
        Expression::ArrayAccess(node) => {
            if let Some(base) = direct_var_name(node.array) {
                writes.insert(base);
            }
            collect_lhs_writes(node.array, writes);
            collect_expression_writes(node.index, writes);
        }
        Expression::ArrayAppend(node) => {
            if let Some(base) = direct_var_name(node.array) {
                writes.insert(base);
            }
        }
        Expression::Access(Access::Property(node)) if is_this(node.object) => {
            if let ClassLikeMemberSelector::Identifier(property) = &node.property {
                writes.insert(format!("this->{}", lossy(property.value)));
            }
        }
        Expression::Array(node) => collect_element_writes(&node.elements, writes),
        Expression::LegacyArray(node) => collect_element_writes(&node.elements, writes),
        Expression::List(node) => collect_element_writes(&node.elements, writes),
        _ => {}
    }
}

/// Применить `use`-импорты файла к резолверу.
fn apply_imports(parsed: &ParsedFile<'_>, resolver: &mut NameResolver, items: &UseItems<'_>) {
    match items {
        UseItems::Sequence(sequence) => {
            for item in sequence.items.iter() {
                add_import(parsed, resolver, item, None);
            }
        }
        UseItems::TypedSequence(sequence) => {
            if !sequence.r#type.is_function() && !sequence.r#type.is_const() {
                for item in sequence.items.iter() {
                    add_import(parsed, resolver, item, None);
                }
            }
        }
        UseItems::TypedList(list) => {
            if !list.r#type.is_function() && !list.r#type.is_const() {
                let prefix = parsed.text(list.namespace.span());
                for item in list.items.iter() {
                    add_import(parsed, resolver, item, Some(&prefix));
                }
            }
        }
        UseItems::MixedList(list) => {
            let prefix = parsed.text(list.namespace.span());
            for maybe in list.items.iter() {
                if maybe
                    .r#type
                    .as_ref()
                    .is_none_or(|ty| !ty.is_function() && !ty.is_const())
                {
                    add_import(parsed, resolver, &maybe.item, Some(&prefix));
                }
            }
        }
    }
}

fn add_import(
    parsed: &ParsedFile<'_>,
    resolver: &mut NameResolver,
    item: &UseItem<'_>,
    prefix: Option<&str>,
) {
    let raw = parsed.text(item.name.span());
    let raw = raw.trim().trim_start_matches('\\');
    let full = match prefix {
        Some(prefix) => format!("{}\\{raw}", prefix.trim().trim_end_matches('\\')),
        None => raw.to_string(),
    };
    let full = full.trim_start_matches('\\').to_string();
    let alias = match &item.alias {
        Some(alias) => lossy(alias.identifier.value),
        None => full.rsplit('\\').next().unwrap_or(&full).to_string(),
    };
    resolver.add_import(&alias, &full);
}

/// Тип возврата метода: объявленный hint важнее, `array`/`iterable`/`mixed` уступают docblock.
fn return_type(
    parsed: &ParsedFile<'_>,
    resolver: &NameResolver,
    hint: Option<&FunctionLikeReturnTypeHint<'_>>,
    start: u32,
) -> Option<String> {
    let doc = doc_comment_before(parsed, start)
        .and_then(|raw| doc_return_type(&raw))
        .and_then(|text| resolve_type_text(resolver, &text));
    let Some(hint) = hint else {
        return doc;
    };
    let text = parsed.text(hint.hint.span());
    let bare = text.trim().trim_start_matches('?').to_ascii_lowercase();
    if matches!(bare.as_str(), "array" | "iterable" | "mixed") {
        return doc;
    }
    resolve_type_text(resolver, &text)
}

/// `@return` из docblock: первый класс-подобный тип.
fn doc_return_type(raw: &str) -> Option<String> {
    for line in raw.lines() {
        let line = line
            .trim()
            .trim_start_matches("/**")
            .trim_end_matches("*/")
            .trim()
            .trim_start_matches('*')
            .trim();
        let lower = line.to_ascii_lowercase();
        let Some(at) = lower.find("@return") else {
            continue;
        };
        let rest = line[at + "@return".len()..].trim();
        if !rest.is_empty() {
            return type_from_text(rest);
        }
    }
    None
}

/// Первый класс-подобный тип из `Foo[]`, `array<Foo>`, `?Foo`, `Foo|null`, `Foo|Bar`.
fn type_from_text(text: &str) -> Option<String> {
    let token = type_token(text);
    for part in token.split('|') {
        if let Some(name) = single_type(part) {
            return Some(name);
        }
    }
    None
}

/// Токен типа до пробела нулевого уровня (пары `{}`/`<>`/`()`/`[]` считаются одним целым).
fn type_token(text: &str) -> &str {
    let mut depth = 0i32;
    for (at, c) in text.char_indices() {
        match c {
            '{' | '<' | '(' | '[' => depth += 1,
            '}' | '>' | ')' | ']' => depth -= 1,
            c if c.is_whitespace() && depth <= 0 => return &text[..at],
            _ => {}
        }
    }
    text
}

fn single_type(text: &str) -> Option<String> {
    let text = text.trim().trim_start_matches('?').trim();
    if text.is_empty() {
        return None;
    }
    if let Some(at) = text.find('<') {
        let head = text[..at].trim();
        let inner = text[at + 1..].trim_end_matches('>');
        if matches!(
            head.to_ascii_lowercase().as_str(),
            "array" | "list" | "iterable" | "non-empty-array" | "non-empty-list"
        ) {
            return element_type(inner);
        }
        return single_type(head);
    }
    let text = text.trim_end_matches("[]").trim();
    if text.contains(['{', '(']) {
        return None;
    }
    let name = text.trim_start_matches('\\');
    if name.is_empty() || is_scalar_name(name) {
        return None;
    }
    Some(name.to_string())
}

fn element_type(inner: &str) -> Option<String> {
    let mut depth = 0i32;
    let mut start = 0;
    for (at, c) in inner.char_indices() {
        match c {
            '{' | '<' | '(' | '[' => depth += 1,
            '}' | '>' | ')' | ']' => depth -= 1,
            ',' if depth == 0 => start = at + 1,
            _ => {}
        }
    }
    type_from_text(inner[start..].trim())
}

fn resolve_type_text(resolver: &NameResolver, text: &str) -> Option<String> {
    type_from_text(text).map(|name| resolver.resolve_class(&name))
}

fn is_scalar_name(name: &str) -> bool {
    matches!(
        name.to_ascii_lowercase().as_str(),
        "string"
            | "int"
            | "integer"
            | "bool"
            | "boolean"
            | "float"
            | "double"
            | "real"
            | "void"
            | "mixed"
            | "object"
            | "array"
            | "callable"
            | "iterable"
            | "null"
            | "never"
            | "true"
            | "false"
            | "class-string"
            | "numeric"
            | "scalar"
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    fn facts(source: &str) -> Vec<FlowFile> {
        file_facts("t.php", source.as_bytes())
    }

    fn method<'a>(files: &'a [FlowFile], id: &str) -> &'a MethodFlow {
        files
            .iter()
            .flat_map(|file| file.methods.iter())
            .find(|method| method.id == id)
            .unwrap_or_else(|| panic!("нет метода {id}"))
    }

    #[test]
    fn repository_call_argument_carries_array_key() {
        let source = r#"<?php
namespace Api\Queue\Repository;

use Api\Queue\Connection\Connection;
use Api\Queue\Entity\QueueEntity;

class QueueRepository
{
    private Connection $connection;

    public function makeEntity($serviceType): QueueEntity
    {
        return new QueueEntity($serviceType);
    }

    public function find(int $id): QueueEntity
    {
        $row = $this->connection->fetchAllAssociative('SELECT * FROM queue', [$id]);
        return $this->makeEntity($row['service_type']);
    }
}
"#;
        let files = facts(source);
        let find = method(&files, "Api\\Queue\\Repository\\QueueRepository::find");
        let edge = find
            .args
            .iter()
            .find(|edge| {
                edge.target == "Api\\Queue\\Repository\\QueueRepository::makeEntity"
                    && edge.index == 0
            })
            .expect("нет аргумента makeEntity");
        assert!(
            edge.origins.iter().any(|atom| atom == "key:service_type"),
            "{:?}",
            edge.origins
        );
    }

    #[test]
    fn promoted_constructor_parameter_becomes_property_assignment() {
        let source = r#"<?php
class C
{
    public function __construct(private string $p)
    {
    }
}
"#;
        let files = facts(source);
        let ctor = method(&files, "C::__construct");
        let assign = ctor
            .assigns
            .iter()
            .find(|assign| assign.lhs == "prop:this::p")
            .expect("нет присваивания свойства");
        assert_eq!(assign.origins, vec!["p:C::__construct#0"]);
    }

    #[test]
    fn getter_return_carries_property_origin() {
        let source = r#"<?php
class C
{
    private $p;

    public function getP()
    {
        return $this->p;
    }
}
"#;
        let files = facts(source);
        let getter = method(&files, "C::getP");
        assert!(
            getter
                .returns
                .iter()
                .any(|ret| ret.origins.iter().any(|atom| atom == "prop:this::p")),
            "{:?}",
            getter.returns
        );
    }

    #[test]
    fn condition_records_read_at_its_line() {
        let source = r#"<?php
class C
{
    public function check(C $obj)
    {
        if ($obj->getX() === null) {
            return 1;
        }
    }
}
"#;
        let files = facts(source);
        let check = method(&files, "C::check");
        assert!(
            check.reads.iter().any(|read| read.line == 6),
            "{:?}",
            check.reads
        );
    }

    #[test]
    fn inferred_return_type_reaches_condition_read() {
        let source = r#"<?php
class Repo
{
    public function find(int $id): ?Item
    {
        return null;
    }
}

class Item
{
    private string $serviceType;

    public function getServiceType(): string
    {
        return $this->serviceType;
    }
}

class Service
{
    private Repo $repo;

    public function check(int $id): void
    {
        $item = $this->repo->find($id);
        if ($item->getServiceType() === 'x') {
        }
    }
}
"#;
        let types: MethodTypes = file_return_types("t.php", source.as_bytes())
            .into_iter()
            .collect();
        assert_eq!(types.get("Repo::find").map(String::as_str), Some("Item"));
        let files = file_facts_with_types("t.php", source.as_bytes(), &types);
        let check = method(&files, "Service::check");
        let read = check
            .reads
            .iter()
            .find(|read| read.line == 27)
            .expect("нет чтения в условии");
        assert!(
            read.origins
                .iter()
                .any(|atom| atom == "c:Item::getServiceType@27"),
            "{:?}",
            read.origins
        );
    }

    #[test]
    fn docblock_array_element_type_reaches_foreach_body() {
        let source = r#"<?php
class Item
{
    private string $serviceType;

    public function getServiceType(): string
    {
        return $this->serviceType;
    }
}

class Repo
{
    /**
     * @return Item[]
     */
    public function all(): array
    {
        return [];
    }
}

class Service
{
    private Repo $repo;

    public function run(): void
    {
        foreach ($this->repo->all() as $item) {
            $item->getServiceType();
        }
    }
}
"#;
        let types: MethodTypes = file_return_types("t.php", source.as_bytes())
            .into_iter()
            .collect();
        assert_eq!(types.get("Repo::all").map(String::as_str), Some("Item"));
        let files = file_facts_with_types("t.php", source.as_bytes(), &types);
        let run = method(&files, "Service::run");
        let read = run
            .reads
            .iter()
            .find(|read| read.line == 30)
            .expect("нет чтения в теле foreach");
        assert!(
            read.origins
                .iter()
                .any(|atom| atom == "c:Item::getServiceType@30"),
            "{:?}",
            read.origins
        );
    }

    #[test]
    fn repeated_call_atoms_differ_by_line() {
        let source = r#"<?php
class A
{
    public function make($v)
    {
        return $v;
    }

    public function run($x, $y): void
    {
        $a = $this->make($x);
        $b = $this->make($y);
    }
}
"#;
        let files = facts(source);
        let run = method(&files, "A::run");
        let atoms: Vec<&str> = run
            .assigns
            .iter()
            .flat_map(|assign| assign.origins.iter())
            .filter(|atom| atom.starts_with("c:A::make@"))
            .map(String::as_str)
            .collect();
        assert_eq!(atoms, vec!["c:A::make@11", "c:A::make@12"]);

        let mut edges: Vec<(u32, usize, Vec<&str>)> = run
            .args
            .iter()
            .filter(|edge| edge.target == "A::make")
            .map(|edge| {
                (
                    edge.line,
                    edge.index,
                    edge.origins.iter().map(String::as_str).collect(),
                )
            })
            .collect();
        edges.sort_by_key(|edge| edge.0);
        assert_eq!(
            edges,
            vec![(11, 0, vec!["p:A::run#0"]), (12, 0, vec!["p:A::run#1"]),]
        );
    }

    #[test]
    fn this_property_assignment_uses_receiver_relative_atom() {
        let source = r#"<?php
class C
{
    private $p;

    public function setP($x): void
    {
        $this->p = $x;
    }

    public function getP()
    {
        return $this->p;
    }
}
"#;
        let files = facts(source);
        let setter = method(&files, "C::setP");
        let assign = setter
            .assigns
            .iter()
            .find(|assign| assign.lhs == "prop:this::p")
            .expect("нет присваивания prop:this::p");
        assert_eq!(assign.origins, vec!["p:C::setP#0"]);

        let getter = method(&files, "C::getP");
        assert!(
            getter
                .returns
                .iter()
                .any(|ret| ret.origins.iter().any(|atom| atom == "prop:this::p")),
            "{:?}",
            getter.returns
        );
    }

    #[test]
    fn this_call_receiver_is_this() {
        let source = r#"<?php
class C
{
    public function m(): void
    {
    }

    public function run(): void
    {
        $this->m();
    }
}
"#;
        let files = facts(source);
        let run = method(&files, "C::run");
        let receiver = run
            .receivers
            .iter()
            .find(|edge| edge.target == "C::m")
            .expect("нет получателя вызова");
        assert_eq!(receiver.origins, vec!["this"]);
        assert_eq!(receiver.line, 10);
    }

    #[test]
    fn instantiated_receiver_carries_allocation_site() {
        let source = r#"<?php
class C
{
    public function m(): void
    {
    }
}

class User
{
    public function run(): void
    {
        $obj = new C();
        $obj->m();
    }
}
"#;
        let files = facts(source);
        let run = method(&files, "User::run");
        let receiver = run
            .receivers
            .iter()
            .find(|edge| edge.target == "C::m")
            .expect("нет получателя вызова");
        assert!(
            receiver
                .origins
                .iter()
                .any(|atom| atom.starts_with("new:C@")),
            "{:?}",
            receiver.origins
        );
    }

    #[test]
    fn array_append_propagates_origin_into_return() {
        let source = r#"<?php
class Entity
{
}

class Repo
{
    public function makeEntity($row): Entity
    {
        return new Entity();
    }

    public function all(array $rows): array
    {
        $result = [];
        foreach ($rows as $row) {
            $result[] = $this->makeEntity($row);
        }
        return $result;
    }
}
"#;
        let files = facts(source);
        let all = method(&files, "Repo::all");
        assert!(
            all.assigns.iter().any(|assign| assign.lhs == "$result[]"
                && assign
                    .origins
                    .iter()
                    .any(|atom| atom.starts_with("c:Repo::makeEntity@"))),
            "{:?}",
            all.assigns
        );
        assert!(
            all.returns.iter().any(|ret| ret
                .origins
                .iter()
                .any(|atom| atom.starts_with("c:Repo::makeEntity@"))),
            "{:?}",
            all.returns
        );
    }

    #[test]
    fn must_survives_a_side_branch() {
        let source = r#"<?php
class C
{
    public function run($row, $c): void
    {
        $x = $row['name_en'];
        if ($c) {
            $y = 1;
        }
        if ($x) {
        }
    }
}
"#;
        let files = facts(source);
        let run = method(&files, "C::run");
        let read = run
            .reads
            .iter()
            .find(|read| read.expr == "$x")
            .expect("нет чтения $x");
        assert!(
            read.must.iter().any(|atom| atom == "key:name_en"),
            "{:?}",
            read.must
        );
    }

    #[test]
    fn must_drops_when_a_branch_reassigns() {
        let source = r#"<?php
class C
{
    public function run($row, $c): void
    {
        $x = $row['name_en'];
        if ($c) {
            $x = 'lit';
        }
        if ($x) {
        }
    }
}
"#;
        let files = facts(source);
        let run = method(&files, "C::run");
        let read = run
            .reads
            .iter()
            .find(|read| read.expr == "$x")
            .expect("нет чтения $x");
        assert!(
            read.origins.iter().any(|atom| atom == "key:name_en"),
            "{:?}",
            read.origins
        );
        assert!(read.must.is_empty(), "{:?}", read.must);
    }

    #[test]
    fn prunes_a_contradictory_branch() {
        let source = r#"<?php
class C
{
    public function run($row): void
    {
        $flag = false;
        if ($flag) {
            $x = $row['name_en'];
        }
        if ($flag) {
            if ($x) {
            }
        }
    }
}
"#;
        let files = facts(source);
        let run = method(&files, "C::run");
        assert!(
            run.reads.iter().all(|read| read.expr != "$x"),
            "{:?}",
            run.reads
        );
    }

    #[test]
    fn early_return_terminates_the_path() {
        let source = r#"<?php
class C
{
    public function run($row, $c): void
    {
        if ($c) {
            return $row['name_en'];
        }
        $x = 'lit';
        $x = $row['id'];
        if ($x) {
        }
    }
}
"#;
        let files = facts(source);
        let run = method(&files, "C::run");
        let read = run
            .reads
            .iter()
            .find(|read| read.expr == "$x")
            .expect("нет чтения $x");
        assert!(
            !read.origins.iter().any(|atom| atom == "key:name_en"),
            "{:?}",
            read.origins
        );
        let assign = run
            .assigns
            .iter()
            .find(|assign| assign.lhs == "$x")
            .expect("нет присваивания $x");
        assert!(
            !assign.origins.iter().any(|atom| atom == "key:name_en"),
            "{:?}",
            assign.origins
        );
    }

    #[test]
    fn must_on_return() {
        let source = r#"<?php
class C
{
    public function name($row)
    {
        return $row['name_en'];
    }
}
"#;
        let files = facts(source);
        let name = method(&files, "C::name");
        let ret = name.returns.first().expect("нет возврата");
        assert!(
            ret.must.iter().any(|atom| atom == "key:name_en"),
            "{:?}",
            ret.must
        );
    }

    #[test]
    fn return_types_cover_declared_and_docblock_forms() {
        let source = r#"<?php
class Item
{
    public function getServiceType(): string
    {
        return $this->serviceType;
    }
}

class Repo
{
    public function find(int $id): ?Item
    {
        return null;
    }

    /** @return Item[] */
    public function all(): array
    {
        return [];
    }

    public function selfRef(): static
    {
        return $this;
    }
}

function makeItem(): Item
{
    return new Item();
}
"#;
        let types: MethodTypes = file_return_types("t.php", source.as_bytes())
            .into_iter()
            .collect();
        assert_eq!(types.get("Repo::find").map(String::as_str), Some("Item"));
        assert_eq!(types.get("Repo::all").map(String::as_str), Some("Item"));
        assert_eq!(types.get("Repo::selfRef").map(String::as_str), Some("Repo"));
        assert_eq!(types.get("makeItem").map(String::as_str), Some("Item"));
    }

    #[test]
    fn try_catch_and_finally_bodies_feed_reads() {
        let source = r#"<?php
class C
{
    public function run($row, $c): void
    {
        try {
            $x = 'lit';
        } catch (\RuntimeException $e) {
            $x = $row['name_en'];
        } finally {
            $z = $row['id'];
        }
        if ($x) {
        }
        if ($z) {
        }
    }
}
"#;
        let files = facts(source);
        let run = method(&files, "C::run");
        let x = run
            .reads
            .iter()
            .find(|read| read.expr == "$x")
            .expect("нет чтения $x");
        assert!(
            x.origins.iter().any(|atom| atom == "key:name_en"),
            "{:?}",
            x.origins
        );
        assert!(x.must.is_empty(), "{:?}", x.must);
        let z = run
            .reads
            .iter()
            .find(|read| read.expr == "$z")
            .expect("нет чтения $z");
        assert!(
            z.origins.iter().any(|atom| atom == "key:id"),
            "{:?}",
            z.origins
        );
    }

    #[test]
    fn destructuring_binds_variables_to_rhs_origins() {
        let source = r#"<?php
function run($row)
{
    $data = $row['name_en'];
    list($a, ['k' => [$b]]) = $data;
    [$c] = $data;
    if ($a) {
    }
}
"#;
        let files = facts(source);
        let run = method(&files, "run");
        for name in ["$a", "$b", "$c"] {
            let assign = run
                .assigns
                .iter()
                .find(|assign| assign.lhs == name)
                .unwrap_or_else(|| panic!("нет присваивания {name}"));
            assert!(
                assign.origins.iter().any(|atom| atom == "key:name_en"),
                "{name}: {:?}",
                assign.origins
            );
            assert!(assign.must.is_empty(), "{name}: {:?}", assign.must);
        }
        let read = run
            .reads
            .iter()
            .find(|read| read.expr == "$a")
            .expect("нет чтения $a");
        assert!(
            read.origins.iter().any(|atom| atom == "key:name_en"),
            "{:?}",
            read.origins
        );
    }

    #[test]
    fn yield_adds_generator_returns() {
        let source = r#"<?php
function each($rows, $more)
{
    foreach ($rows as $row) {
        yield $row['x'];
    }
    yield 'k' => $more['y'];
    yield from $more['z'];
}
"#;
        let files = facts(source);
        let each = method(&files, "each");
        for key in ["key:x", "key:y", "key:z"] {
            assert!(
                each.returns
                    .iter()
                    .any(|ret| ret.origins.iter().any(|atom| atom == key)),
                "{key}: {:?}",
                each.returns
            );
        }
    }

    #[test]
    fn array_column_literal_returns_key_origin() {
        let source = r#"<?php
function names($rows)
{
    $names = array_column($rows, 'name_en');
    if ($names) {
    }
}
"#;
        let files = facts(source);
        let names = method(&files, "names");
        let assign = names
            .assigns
            .iter()
            .find(|assign| assign.lhs == "$names")
            .expect("нет присваивания $names");
        assert_eq!(assign.origins, vec!["key:name_en".to_string()]);
        let read = names
            .reads
            .iter()
            .find(|read| read.expr == "$names")
            .expect("нет чтения $names");
        assert!(
            read.origins.iter().any(|atom| atom == "key:name_en"),
            "{:?}",
            read.origins
        );
        assert!(
            names.args.iter().any(|edge| edge.target == "array_column"
                && edge.index == 0
                && edge.origins.iter().any(|atom| atom == "p:names#0")),
            "{:?}",
            names.args
        );
        assert!(
            names
                .args
                .iter()
                .any(|edge| edge.target == "array_column" && edge.index == 1),
            "{:?}",
            names.args
        );
    }

    #[test]
    fn call_user_func_static_callable_emits_real_target() {
        let source = r#"<?php
class C
{
    public function m($v): void
    {
    }

    public function bar($v): void
    {
    }

    public function run($row): void
    {
        $x = call_user_func([$this, 'm'], $row['x']);
        $y = call_user_func_array([$this, 'm'], [$row['y']]);
        $z = call_user_func('C::bar', $row['z']);
        $w = call_user_func($this->m(...), $row['w']);
    }
}
"#;
        let files = facts(source);
        let run = method(&files, "C::run");
        let x = run
            .assigns
            .iter()
            .find(|assign| assign.lhs == "$x")
            .expect("нет присваивания $x");
        assert_eq!(x.origins, vec!["c:C::m@14".to_string()]);
        let edge = run
            .args
            .iter()
            .find(|edge| edge.target == "C::m" && edge.line == 14 && edge.index == 0)
            .expect("нет аргумента C::m");
        assert!(
            edge.origins.iter().any(|atom| atom == "key:x"),
            "{:?}",
            edge.origins
        );
        let y = run
            .assigns
            .iter()
            .find(|assign| assign.lhs == "$y")
            .expect("нет присваивания $y");
        assert_eq!(y.origins, vec!["c:C::m@15".to_string()]);
        let array_edge = run
            .args
            .iter()
            .find(|edge| edge.target == "C::m" && edge.line == 15 && edge.index == 0)
            .expect("нет аргумента C::m из call_user_func_array");
        assert!(
            array_edge.origins.iter().any(|atom| atom == "key:y"),
            "{:?}",
            array_edge.origins
        );
        let z = run
            .assigns
            .iter()
            .find(|assign| assign.lhs == "$z")
            .expect("нет присваивания $z");
        assert_eq!(z.origins, vec!["c:C::bar@16".to_string()]);
        let w = run
            .assigns
            .iter()
            .find(|assign| assign.lhs == "$w")
            .expect("нет присваивания $w");
        assert_eq!(w.origins, vec!["c:C::m@17".to_string()]);
        let partial_edge = run
            .args
            .iter()
            .find(|edge| edge.target == "C::m" && edge.line == 17 && edge.index == 0)
            .expect("нет аргумента C::m из first-class callable");
        assert!(
            partial_edge.origins.iter().any(|atom| atom == "key:w"),
            "{:?}",
            partial_edge.origins
        );
        assert!(
            run.args.iter().all(
                |edge| edge.target != "call_user_func" && edge.target != "call_user_func_array"
            ),
            "{:?}",
            run.args
        );
    }

    #[test]
    fn param_names_follow_params_order() {
        let source = r#"<?php
class C
{
    public function m($a, int $b): void
    {
    }
}

function f($x, $y): void
{
}
"#;
        let files = facts(source);
        assert_eq!(method(&files, "C::m").param_names, vec!["a", "b"]);
        assert_eq!(method(&files, "f").param_names, vec!["x", "y"]);
    }

    #[test]
    fn named_and_spread_arguments_get_name_and_flag() {
        let source = r#"<?php
class C
{
    public function m($a, $b): void
    {
    }

    public function run($x, $arr): void
    {
        $this->m(b: $x, a: $x);
        $this->m(...$arr);
    }
}
"#;
        let files = facts(source);
        let run = method(&files, "C::run");
        assert!(
            run.args.iter().any(|edge| edge.target == "C::m"
                && edge.name.as_deref() == Some("b")
                && edge.index == usize::MAX
                && !edge.spread
                && edge.origins.iter().any(|atom| atom == "p:C::run#0")),
            "{:?}",
            run.args
        );
        assert!(
            run.args.iter().any(|edge| edge.target == "C::m"
                && edge.spread
                && edge.index == usize::MAX
                && edge.origins.iter().any(|atom| atom == "p:C::run#1")),
            "{:?}",
            run.args
        );
    }

    #[test]
    fn unknown_receiver_property_read_becomes_key() {
        let source = r#"<?php
function f($row): void
{
    $x = $row->name_en;
    if ($x) {
    }
}
"#;
        let files = facts(source);
        let f = method(&files, "f");
        let assign = f
            .assigns
            .iter()
            .find(|assign| assign.lhs == "$x")
            .expect("нет присваивания $x");
        assert!(
            assign.origins.iter().any(|atom| atom == "key:name_en"),
            "{:?}",
            assign.origins
        );
        assert!(
            f.reads.iter().any(|read| read.expr == "$row->name_en"
                && read.origins.iter().any(|atom| atom == "key:name_en")),
            "{:?}",
            f.reads
        );
    }

    #[test]
    fn magic_accessor_adds_key_to_property_read() {
        let source = r#"<?php
class Magic
{
    public function __get($name)
    {
        return 1;
    }

    public function __set($name, $value): void
    {
    }
}

class C
{
    public function run(Magic $obj): void
    {
        $x = $obj->name_en;
    }
}
"#;
        let files = facts(source);
        let run = method(&files, "C::run");
        let assign = run
            .assigns
            .iter()
            .find(|assign| assign.lhs == "$x")
            .expect("нет присваивания $x");
        assert!(
            assign
                .origins
                .iter()
                .any(|atom| atom == "prop:Magic::name_en"),
            "{:?}",
            assign.origins
        );
        assert!(
            assign.origins.iter().any(|atom| atom == "key:name_en"),
            "{:?}",
            assign.origins
        );
    }

    #[test]
    fn extract_makes_unknown_variable_a_key() {
        let source = r#"<?php
function f($row): void
{
    extract($row);
    $x = $name_en;
    if ($x) {
    }
}
"#;
        let files = facts(source);
        let f = method(&files, "f");
        let assign = f
            .assigns
            .iter()
            .find(|assign| assign.lhs == "$x")
            .expect("нет присваивания $x");
        assert!(
            assign.origins.iter().any(|atom| atom == "key:name_en"),
            "{:?}",
            assign.origins
        );
        assert!(
            f.reads.iter().any(|read| read.expr == "$name_en"
                && read.origins.iter().any(|atom| atom == "key:name_en")),
            "{:?}",
            f.reads
        );
    }

    #[test]
    fn get_object_vars_and_compact_union_origins() {
        let source = r#"<?php
class C
{
    private $a;
    public $b;

    public function run(): void
    {
        $vars = get_object_vars($this);
    }
}

function f($a, $b): void
{
    $x = compact('a', 'b');
}
"#;
        let files = facts(source);
        let run = method(&files, "C::run");
        let vars = run
            .assigns
            .iter()
            .find(|assign| assign.lhs == "$vars")
            .expect("нет присваивания $vars");
        for atom in ["prop:this::a", "prop:this::b"] {
            assert!(
                vars.origins.iter().any(|origin| origin == atom),
                "{atom}: {:?}",
                vars.origins
            );
        }
        let f = method(&files, "f");
        let x = f
            .assigns
            .iter()
            .find(|assign| assign.lhs == "$x")
            .expect("нет присваивания $x");
        for atom in ["p:f#0", "p:f#1"] {
            assert!(
                x.origins.iter().any(|origin| origin == atom),
                "{atom}: {:?}",
                x.origins
            );
        }
    }

    #[test]
    fn match_value_unions_arm_origins() {
        let source = r#"<?php
function f($row, $kind)
{
    $x = match ($kind) {
        'a' => $row['col_a'],
        default => $row['col_b'],
    };
    if ($x) {
    }
}
"#;
        let files = facts(source);
        let f = method(&files, "f");
        let assign = f
            .assigns
            .iter()
            .find(|assign| assign.lhs == "$x")
            .expect("нет присваивания $x");
        for atom in ["key:col_a", "key:col_b"] {
            assert!(
                assign.origins.iter().any(|origin| origin == atom),
                "{atom}: {:?}",
                assign.origins
            );
        }
        let read = f
            .reads
            .iter()
            .find(|read| read.expr == "$x")
            .expect("нет чтения $x");
        assert!(
            read.origins.iter().any(|atom| atom == "key:col_a"),
            "{:?}",
            read.origins
        );
        assert!(read.must.is_empty(), "{:?}", read.must);
    }

    #[test]
    fn switch_cases_fall_through_sequentially() {
        let source = r#"<?php
function f($mode, $row)
{
    switch ($mode) {
        case 'a':
            $x = $row['from_a'];
        case 'b':
            if ($x) {
            }
            break;
    }
}
"#;
        let files = facts(source);
        let f = method(&files, "f");
        let read = f
            .reads
            .iter()
            .find(|read| read.expr == "$x")
            .expect("нет чтения $x во втором случае");
        assert!(
            read.origins.iter().any(|atom| atom == "key:from_a"),
            "{:?}",
            read.origins
        );
    }

    #[test]
    fn array_passthrough_functions_keep_origins() {
        let source = r#"<?php
function f($rows)
{
    $names = array_values(array_filter($rows));
    $merged = array_merge($rows, array_slice($rows, 0, 2));
}
"#;
        let files = facts(source);
        let f = method(&files, "f");
        for lhs in ["$names", "$merged"] {
            let assign = f
                .assigns
                .iter()
                .find(|assign| assign.lhs == lhs)
                .unwrap_or_else(|| panic!("нет присваивания {lhs}"));
            assert!(
                assign.origins.iter().any(|atom| atom == "p:f#0"),
                "{lhs}: {:?}",
                assign.origins
            );
        }
    }

    #[test]
    fn array_map_uses_callable_arrow_closure_and_arrays() {
        let source = r#"<?php
class C
{
    public function m($v)
    {
        return $v;
    }

    public function run($rows): void
    {
        $a = array_map([$this, 'm'], $rows);
        $b = array_map(fn($r) => $r['col_fn'], $rows);
        $c = array_map(function ($r) { return $r['col_closure']; }, $rows);
        $d = array_map(null, $rows);
    }
}
"#;
        let files = facts(source);
        let run = method(&files, "C::run");
        let origins = |lhs: &str| {
            run.assigns
                .iter()
                .find(|assign| assign.lhs == lhs)
                .unwrap_or_else(|| panic!("нет присваивания {lhs}"))
                .origins
                .clone()
        };
        assert!(
            origins("$a").iter().any(|atom| atom.starts_with("c:C::m@")),
            "{:?}",
            origins("$a")
        );
        assert!(
            origins("$b").iter().any(|atom| atom == "key:col_fn"),
            "{:?}",
            origins("$b")
        );
        assert!(
            origins("$c").iter().any(|atom| atom == "key:col_closure"),
            "{:?}",
            origins("$c")
        );
        assert!(
            origins("$d").iter().any(|atom| atom == "p:C::run#0"),
            "{:?}",
            origins("$d")
        );
    }

    #[test]
    fn static_property_read_and_write_use_class_atom() {
        let source = r#"<?php
class C
{
    private static $count;

    public function run($x): void
    {
        $v = self::$count;
        self::$count = $x;
    }
}
"#;
        let files = facts(source);
        let run = method(&files, "C::run");
        let read = run
            .assigns
            .iter()
            .find(|assign| assign.lhs == "$v")
            .expect("нет присваивания $v");
        assert!(
            read.origins.iter().any(|atom| atom == "prop:C::$count"),
            "{:?}",
            read.origins
        );
        let write = run
            .assigns
            .iter()
            .find(|assign| assign.lhs == "prop:C::$count")
            .expect("нет присваивания статического свойства");
        assert_eq!(write.origins, vec!["p:C::run#0".to_string()]);
    }

    #[test]
    fn global_variable_reads_and_writes_global_property() {
        let source = r#"<?php
class C
{
    public function set($value): void
    {
        global $counter;
        $counter = $value;
        $x = $counter;
    }
}
"#;
        let files = facts(source);
        let set = method(&files, "C::set");
        assert!(
            set.assigns
                .iter()
                .any(|assign| assign.lhs == "prop:global::counter"
                    && assign.origins.iter().any(|atom| atom == "p:C::set#0")),
            "{:?}",
            set.assigns
        );
        let x = set
            .assigns
            .iter()
            .find(|assign| assign.lhs == "$x")
            .expect("нет присваивания $x");
        assert!(
            x.origins.iter().any(|atom| atom == "prop:global::counter"),
            "{:?}",
            x.origins
        );
    }

    #[test]
    fn foreach_key_compared_with_literal_records_key_read() {
        let source = r#"<?php
function f($map): void
{
    foreach ($map as $key => $value) {
        if ($key === 'service_type') {
        }
    }
}
"#;
        let files = facts(source);
        let f = method(&files, "f");
        assert!(
            f.reads
                .iter()
                .any(|read| read.origins.iter().any(|atom| atom == "key:service_type")),
            "{:?}",
            f.reads
        );
    }

    #[test]
    fn reference_assignment_copies_origins() {
        let source = r#"<?php
function f($row): void
{
    $b = $row['name_en'];
    $a = &$b;
    if ($a) {
    }
}
"#;
        let files = facts(source);
        let f = method(&files, "f");
        let assign = f
            .assigns
            .iter()
            .find(|assign| assign.lhs == "$a")
            .expect("нет присваивания $a");
        assert!(
            assign.origins.iter().any(|atom| atom == "key:name_en"),
            "{:?}",
            assign.origins
        );
    }

    #[test]
    fn isset_narrows_the_nullable_variable() {
        let source = r#"<?php
function f($row): void
{
    $x = $row['name_en'] ?? null;
    if ($x !== null) {
        if ($x) {
        }
    }
    if (isset($row['name_en'])) {
        if ($row['name_en']) {
        }
    }
}
"#;
        let files = facts(source);
        let f = method(&files, "f");
        let read = f
            .reads
            .iter()
            .find(|read| read.expr == "$x")
            .expect("нет чтения $x");
        assert!(
            read.origins.iter().any(|atom| atom == "key:name_en"),
            "{:?}",
            read.origins
        );
        assert!(
            f.reads.iter().any(|read| read.expr == "$row['name_en']"
                && read.origins.iter().any(|atom| atom == "key:name_en")),
            "{:?}",
            f.reads
        );
    }

    #[test]
    fn array_key_exists_narrows_the_offset() {
        let source = r#"<?php
function f($row): void
{
    if (array_key_exists('name_en', $row)) {
        if ($row['name_en']) {
        }
    }
}
"#;
        let files = facts(source);
        let f = method(&files, "f");
        let read = f
            .reads
            .iter()
            .find(|read| read.expr == "$row['name_en']")
            .expect("нет чтения $row['name_en']");
        assert!(
            read.origins.iter().any(|atom| atom == "key:name_en"),
            "{:?}",
            read.origins
        );
    }

    #[test]
    fn loop_fixpoint_reaches_the_second_iteration() {
        let source = r#"<?php
function f($row, $c): void
{
    $x = 'a';
    while ($c) {
        $x = $row['name_en'];
        $c = false;
    }
    if ($x) {
    }
}
"#;
        let files = facts(source);
        let f = method(&files, "f");
        let read = f
            .reads
            .iter()
            .find(|read| read.expr == "$x")
            .expect("нет чтения $x");
        assert!(
            read.origins.iter().any(|atom| atom == "key:name_en"),
            "{:?}",
            read.origins
        );
    }

    #[test]
    fn switch_break_ends_fallthrough() {
        let source = r#"<?php
function f($mode, $row): void
{
    switch ($mode) {
        case 'a':
            $x = $row['from_a'];
            break;
        case 'b':
            if ($x) {
            }
            break;
    }
}
"#;
        let files = facts(source);
        let f = method(&files, "f");
        assert!(
            f.reads
                .iter()
                .all(|read| read.expr != "$x"
                    || !read.origins.iter().any(|atom| atom == "key:from_a")),
            "{:?}",
            f.reads
        );
    }

    #[test]
    fn match_arms_union_origins() {
        let source = r#"<?php
function f($row, $kind): void
{
    $x = match ($kind) {
        'a' => $row['name_en'],
        default => 'x',
    };
    if ($x) {
    }
}
"#;
        let files = facts(source);
        let f = method(&files, "f");
        let read = f
            .reads
            .iter()
            .find(|read| read.expr == "$x")
            .expect("нет чтения $x");
        assert!(
            read.origins.iter().any(|atom| atom == "key:name_en"),
            "{:?}",
            read.origins
        );
        assert!(read.must.is_empty(), "{:?}", read.must);
    }

    #[test]
    fn match_all_arms_must() {
        let source = r#"<?php
function f($row, $kind): void
{
    $x = match ($kind) {
        'a' => $row['name_en'],
        default => $row['name_en'],
    };
    if ($x) {
    }
}
"#;
        let files = facts(source);
        let f = method(&files, "f");
        let read = f
            .reads
            .iter()
            .find(|read| read.expr == "$x")
            .expect("нет чтения $x");
        assert!(
            read.must.iter().any(|atom| atom == "key:name_en"),
            "{:?}",
            read.must
        );
    }
}
