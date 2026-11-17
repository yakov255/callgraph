//! Локальные факты потока данных: обход AST PHP и истоки переменных/свойств/ключей.
//!
//! Контракт — `super::model`. Для каждого метода/функции собираются присваивания, чтения,
//! возвраты и аргументы вызовов. Анализ нечувствителен к потоку управления: наборы истоков
//! объединяются (union), дедуплицируются и ограничиваются 16 атомами.

use std::collections::{HashMap, HashSet};

use mago_span::HasSpan;
use mago_syntax::cst::*;

use super::model::{ArgEdge, Assign, FlowFile, MethodFlow, MethodTypes, Read};
use crate::lang::php::analysis::NameResolver;
use crate::lang::php::analysis::docblock::doc_comment_before;
use crate::lang::php::parser::{MagoAdapter, ParsedFile, PhpParser};
use crate::lang::php::printer::lossy;

/// Максимум атомов в одном наборе истоков.
const MAX_ATOMS: usize = 16;
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
        let mut collector = Collector {
            parsed,
            resolver: NameResolver::new(None),
            methods: Vec::new(),
            top: None,
            method_types: types,
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
// Анализ тела метода.
// ---------------------------------------------------------------------------

struct Analyzer<'p, 'a, 't> {
    parsed: &'p ParsedFile<'a>,
    resolver: NameResolver,
    /// Полное имя текущего класса (`None` для функций и верхнего уровня).
    class: Option<String>,
    /// `Class::method`, имя функции или `__top__`.
    method: String,
    /// Истоки переменных: `$x` -> атомы.
    vars: HashMap<String, Vec<String>>,
    /// Минимальные типы: `$x` / `this->p` -> полное имя класса.
    types: HashMap<String, String>,
    /// Типы возврата методов проекта (`Class::method` -> класс).
    method_types: &'t MethodTypes,
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
            vars: HashMap::new(),
            types,
            method_types,
        }
    }

    fn statements(&mut self, statements: &[Statement<'a>]) {
        for statement in statements {
            self.statement(statement);
        }
    }

    fn statement(&mut self, statement: &Statement<'a>) {
        match statement {
            Statement::Block(block) => self.statements(block.statements.as_slice()),
            Statement::If(node) => {
                self.condition(node.condition);
                self.statements(node.body.statements());
                for (condition, body) in node.body.else_if_clauses() {
                    self.condition(condition);
                    self.statements(body);
                }
                if let Some(body) = node.body.else_statements() {
                    self.statements(body);
                }
            }
            Statement::While(node) => {
                self.condition(node.condition);
                self.statements(node.body.statements());
            }
            Statement::DoWhile(node) => {
                self.statement(node.statement);
                self.condition(node.condition);
            }
            Statement::For(node) => {
                for expression in node.initializations.iter() {
                    self.expression_statement(expression);
                }
                for expression in node.conditions.iter() {
                    self.condition(expression);
                }
                for expression in node.increments.iter() {
                    self.expression_statement(expression);
                }
                self.statements(node.body.statements());
            }
            Statement::Foreach(node) => self.foreach(node),
            Statement::Switch(node) => {
                self.condition(node.expression);
                for case in node.body.cases() {
                    if let Some(expression) = case.expression() {
                        self.expression_statement(expression);
                    }
                    self.statements(case.statements());
                }
            }
            Statement::Return(node) => {
                let Some(value) = node.value else {
                    return;
                };
                let origins = self.origins(value);
                if significant(&origins) {
                    self.flow.returns.push(Read {
                        line: self.line_of(value),
                        expr: self.one_line(value),
                        origins: norm(origins),
                    });
                }
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
        let origins = self.origins(expression);
        let expr = self.one_line(expression);
        if significant(&origins) {
            self.flow.reads.push(Read {
                line: self.line_of(expression),
                expr,
                origins: norm(origins),
            });
        }
    }

    fn foreach(&mut self, node: &Foreach<'a>) {
        let iterable = self.origins(node.expression);
        let element = self.type_of(node.expression);
        if let Some(name) = direct_var_name(node.target.value()) {
            self.set_var(&name, &iterable);
            if let Some(element) = element {
                self.types.insert(name, element);
            }
        }
        if let Some(key) = node.target.key()
            && let Some(name) = direct_var_name(key)
        {
            self.set_var(&name, &["unknown".to_string()]);
        }
        self.statements(node.body.statements());
    }

    // -- присваивания -------------------------------------------------------

    /// `=`-присваивание: `Assign`, обновление состояния и типов.
    fn assignment(&mut self, node: &Assignment<'a>) -> Vec<String> {
        if !node.operator.is_assign() {
            return Vec::new();
        }
        let origins = self.origins(node.rhs);
        let line = self.line_of(node);
        let ty = self.value_type(node.rhs);
        match node.lhs.unparenthesized() {
            Expression::Variable(Variable::Direct(variable)) => {
                let name = lossy(variable.name);
                self.set_var(&name, &origins);
                if let Some(ty) = ty {
                    self.types.insert(name.clone(), ty);
                }
                self.flow.assigns.push(Assign {
                    line,
                    lhs: name,
                    origins: origins.clone(),
                });
            }
            Expression::Access(Access::Property(access)) => {
                let ClassLikeMemberSelector::Identifier(property) = &access.property else {
                    return origins;
                };
                let name = lossy(property.value);
                if is_this(access.object) {
                    if let Some(class) = self.class.clone() {
                        self.set_var(&format!("this->{name}"), &origins);
                        if let Some(ty) = ty {
                            self.types.insert(format!("this->{name}"), ty);
                        }
                        self.flow.assigns.push(Assign {
                            line,
                            lhs: format!("prop:{class}::{name}"),
                            origins: origins.clone(),
                        });
                    }
                } else if let Some(class) = self.type_of(access.object) {
                    self.flow.assigns.push(Assign {
                        line,
                        lhs: format!("prop:{class}::{name}"),
                        origins: origins.clone(),
                    });
                }
            }
            Expression::ArrayAccess(access) => {
                let Some(base) = direct_var_name(access.array) else {
                    return origins;
                };
                self.set_var(&base, &origins);
                if let Some((_, text)) = self.literal_key(access.index) {
                    self.flow.assigns.push(Assign {
                        line,
                        lhs: format!("{base}[{text}]"),
                        origins: origins.clone(),
                    });
                }
            }
            _ => {}
        }
        origins
    }

    // -- выражения ----------------------------------------------------------

    /// Истоки значения выражения; попутно пишет чтения и рёбра аргументов.
    fn origins(&mut self, expression: &Expression<'a>) -> Vec<String> {
        match expression.unparenthesized() {
            Expression::Variable(Variable::Direct(variable)) => {
                let name = lossy(variable.name);
                self.vars
                    .get(&name)
                    .cloned()
                    .unwrap_or_else(|| vec!["unknown".into()])
            }
            Expression::Variable(_) => vec!["unknown".into()],
            Expression::Literal(_) | Expression::Identifier(_) => vec!["lit".into()],
            Expression::Access(Access::Property(access)) => {
                self.origins(access.object);
                let origins = self
                    .property_atom(access.object, &access.property)
                    .map(|atom| vec![atom])
                    .unwrap_or_else(|| vec!["unknown".into()]);
                self.record_read(expression, origins.clone());
                origins
            }
            Expression::Access(Access::NullSafeProperty(access)) => {
                self.origins(access.object);
                let origins = self
                    .property_atom(access.object, &access.property)
                    .map(|atom| vec![atom])
                    .unwrap_or_else(|| vec!["unknown".into()]);
                self.record_read(expression, origins.clone());
                origins
            }
            Expression::Access(Access::StaticProperty(_)) => vec!["unknown".into()],
            Expression::Access(_) => vec!["lit".into()],
            Expression::ArrayAccess(access) => {
                let mut origins = self.origins(access.array);
                let key = self.literal_key(access.index);
                match &key {
                    Some((name, _)) => origins.push(format!("key:{name}")),
                    None => origins.extend(self.origins(access.index)),
                }
                let origins = norm(origins);
                if key.is_some() {
                    self.record_read(expression, origins.clone());
                }
                origins
            }
            Expression::ArrayAppend(access) => self.origins(access.array),
            Expression::Call(call) => self.call(call, true),
            Expression::Instantiation(instantiation) => self.instantiation(instantiation),
            Expression::Assignment(assignment) => self.assignment(assignment),
            Expression::Binary(binary) => {
                let mut origins = self.origins(binary.lhs);
                origins.extend(self.origins(binary.rhs));
                norm(origins)
            }
            Expression::UnaryPrefix(unary) => self.origins(unary.operand),
            Expression::UnaryPostfix(unary) => self.origins(unary.operand),
            Expression::Parenthesized(inner) => self.origins(inner.expression),
            Expression::Conditional(conditional) => {
                let condition = self.origins(conditional.condition);
                self.record_read(conditional.condition, condition.clone());
                let mut origins = condition;
                if let Some(then) = conditional.then {
                    origins.extend(self.origins(then));
                }
                origins.extend(self.origins(conditional.r#else));
                norm(origins)
            }
            Expression::Match(node) => {
                self.condition(node.expression);
                vec!["unknown".into()]
            }
            Expression::Construct(_) => vec!["unknown".into()],
            Expression::CompositeString(_) => vec!["lit".into()],
            Expression::Array(_) | Expression::LegacyArray(_) | Expression::List(_) => {
                vec!["lit".into()]
            }
            Expression::Closure(closure) => {
                let before = self.vars.clone();
                self.closure_params(&closure.parameter_list);
                self.statements(closure.body.statements.as_slice());
                self.merge_closure(before);
                vec!["unknown".into()]
            }
            Expression::ArrowFunction(function) => {
                let before = self.vars.clone();
                self.closure_params(&function.parameter_list);
                self.origins(function.expression);
                self.merge_closure(before);
                vec!["unknown".into()]
            }
            _ => vec!["unknown".into()],
        }
    }

    // -- вызовы -------------------------------------------------------------

    /// Вызов: рёбра аргументов, исток `c:<target>@<line>` и (для значения) чтение.
    fn call(&mut self, call: &Call<'a>, used: bool) -> Vec<String> {
        match call {
            Call::Method(node) => {
                self.origins(node.object);
            }
            Call::NullSafeMethod(node) => {
                self.origins(node.object);
            }
            Call::StaticMethod(node) => {
                self.origins(node.class);
            }
            Call::Function(_) => {}
        }
        let target = self.call_target(call);
        let line = self.line_of(call);
        self.arguments(call.get_argument_list(), target.as_deref(), line);
        match target {
            Some(target) => {
                let origins = vec![format!("c:{target}@{line}")];
                if used {
                    self.record_read(call, origins.clone());
                }
                origins
            }
            None => vec!["unknown".into()],
        }
    }

    fn instantiation(&mut self, node: &Instantiation<'a>) -> Vec<String> {
        let class = self.class_name(node.class);
        if let Some(arguments) = &node.argument_list {
            let target = class.as_ref().map(|class| format!("{class}::__construct"));
            self.arguments(arguments, target.as_deref(), self.line_of(node));
        }
        match class {
            Some(class) => vec![format!("new:{class}")],
            None => vec!["unknown".into()],
        }
    }

    fn arguments(&mut self, list: &ArgumentList<'a>, target: Option<&str>, line: u32) {
        let Some(target) = target else {
            return;
        };
        for (index, argument) in list.arguments.iter().enumerate() {
            let Argument::Positional(argument) = argument else {
                continue;
            };
            let origins = self.origins(argument.value);
            self.flow.args.push(ArgEdge {
                line,
                target: target.to_string(),
                index,
                origins: norm(origins),
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
                self.types.get(&lossy(variable.name)).cloned()
            }
            Expression::Access(Access::Property(access)) if is_this(access.object) => {
                let ClassLikeMemberSelector::Identifier(property) = &access.property else {
                    return None;
                };
                self.types
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

    fn property_atom(
        &self,
        object: &Expression<'a>,
        property: &ClassLikeMemberSelector<'a>,
    ) -> Option<String> {
        let ClassLikeMemberSelector::Identifier(property) = property else {
            return None;
        };
        let name = lossy(property.value);
        if is_this(object) {
            return self
                .class
                .as_ref()
                .map(|class| format!("prop:{class}::{name}"));
        }
        self.type_of(object)
            .map(|class| format!("prop:{class}::{name}"))
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
        for (index, parameter) in list.parameters.iter().enumerate() {
            let name = lossy(parameter.variable.name);
            let atom = format!("p:{}#{index}", self.method);
            self.vars.insert(name.clone(), vec![atom.clone()]);
            let ty = parameter
                .hint
                .as_ref()
                .and_then(|hint| hint_type(self.parsed, &self.resolver, hint));
            if let Some(ty) = &ty {
                self.types.insert(name.clone(), ty.clone());
            }
            if parameter.is_promoted_property()
                && let Some(class) = self.class.clone()
            {
                let property = name.trim_start_matches('$');
                self.flow.assigns.push(Assign {
                    line: self.line_of(parameter),
                    lhs: format!("prop:{class}::{property}"),
                    origins: vec![atom],
                });
                if let Some(ty) = ty {
                    self.types.insert(format!("this->{property}"), ty);
                }
            }
        }
    }

    /// Параметры замыкания: отдельных `p:`-атомов у них нет.
    fn closure_params(&mut self, list: &FunctionLikeParameterList<'a>) {
        for parameter in list.parameters.iter() {
            self.vars
                .insert(lossy(parameter.variable.name), vec!["unknown".into()]);
        }
    }

    /// Слияние состояния после замыкания: внешние переменные объединяются.
    fn merge_closure(&mut self, before: HashMap<String, Vec<String>>) {
        for (name, origins) in before {
            let merged = match self.vars.get(&name) {
                Some(current) => union(current, &origins),
                None => origins,
            };
            self.vars.insert(name, merged);
        }
    }

    fn set_var(&mut self, name: &str, origins: &[String]) {
        let merged = match self.vars.get(name) {
            Some(current) => union(current, origins),
            None => norm(origins.to_vec()),
        };
        self.vars.insert(name.to_string(), merged);
    }

    fn record_read(&mut self, node: &impl HasSpan, origins: Vec<String>) {
        if !significant(&origins) {
            return;
        }
        self.flow.reads.push(Read {
            line: self.parsed.lines_of(node).0,
            expr: self.one_line(node),
            origins: norm(origins),
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
            .find(|assign| assign.lhs == "prop:C::p")
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
                .any(|ret| ret.origins.iter().any(|atom| atom == "prop:C::p")),
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
}
