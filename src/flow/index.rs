//! Проектный индекс потока данных: фикспойнт по `p:` / `c:` / `prop:` и поиск цепочек.
//!
//! Индекс строится по локальным фактам `super::model::FlowFile`: возвраты методов, аргументы вызовов,
//! присваивания и чтения. Истоки раскрываются до неподвижной точки, `traces` ищет чтения по имени
//! ключа/колонки/property и восстанавливает цепочку атомов до чтения.
//!
//! Результат вызова с call-site (`c:<method>@<line>`) при сборке квалифицируется в
//! `c:<method>@<file>#<line>`; такой атом раскрывает `p:<method>#<i>` аргументами именно своего вызова.
//! Голый `c:<method>` без `@` остаётся и раскрывается глобально (обратная совместимость).
//!
//! Создание объекта (`new:<class>@<line>`) квалифицируется так же — `new:<class>@<file>#<line>`;
//! `$this->p` (`prop:this::<p>`) получает класс контейнера метода (`prop:this:<class>::<p>`).
//! Получатели вызовов (`MethodFlow.receivers`) хранятся по `(цель, сайт)` и привязывают чтение
//! свойства через геттер к месту создания объекта, а не ко всем объектам класса.
//!
//! Контекст получателя пробрасывается по цепочке вызовов: `this` раскрывается получателями
//! конкретного call-site (`direct_ctx`), а вложенный вызов `$this->m()` наследует получателей
//! вызывающего. Записи `$this->p` из сеттеров привязываются к месту создания через аргументы
//! своего вызова (`setter_writes`).

use std::collections::hash_map::Entry;
use std::collections::{BTreeSet, HashMap, VecDeque};

use super::model::{FlowFile, Trace};

/// Предел числа атомов в одном раскрытом множестве.
const ATOM_LIMIT: usize = 32;
/// Предел числа итераций фикспойнта.
const ITERATION_LIMIT: usize = 32;
/// Предел глубины цепочки в рёбрах раскрытия.
const CHAIN_DEPTH: usize = 12;
/// Предел числа цепочек на один терм.
const TRACE_LIMIT: usize = 500;
/// Предел глубины подстановки аргументов конструктора при привязке к месту создания.
const ALLOCATION_DEPTH: usize = 6;
/// Предел глубины раскрытия получателя через параметры.
const RECEIVER_DEPTH: usize = 4;
/// Предел числа атомов в контексте получателя.
const CONTEXT_ATOM_LIMIT: usize = 8;
/// Предел глубины раскрытия с контекстом получателя.
const CONTEXT_DEPTH: usize = 12;
/// Предел числа записей свойства, учитываемых для одного места создания.
const SETTER_SITE_LIMIT: usize = 64;
/// Предел глубины `must`-раскрытия атома.
const MUST_DEPTH: usize = 6;

/// Чтение с контекстом файла и метода.
#[derive(Debug, Clone)]
struct ReadFact {
    file: String,
    method: String,
    line: u32,
    expr: String,
    origins: Vec<String>,
    /// `must`-истоки чтения (см. `Read::must`).
    must: Vec<String>,
}

/// Индекс по всем фактам проекта.
#[derive(Debug, Default)]
pub struct Index {
    /// Истоки возвратов по id метода.
    returns: HashMap<String, BTreeSet<String>>,
    /// Истоки аргументов по (цель вызова, индекс аргумента).
    args: HashMap<(String, usize), BTreeSet<String>>,
    /// Истоки аргументов по (цель вызова, сайт `file#line`, индекс аргумента).
    site_args: HashMap<(String, String, usize), BTreeSet<String>>,
    /// `must`-истоки возвратов по id метода (пересечение всех `return`).
    returns_must: HashMap<String, BTreeSet<String>>,
    /// `must`-истоки аргументов по (цель вызова, индекс аргумента).
    args_must: HashMap<(String, usize), BTreeSet<String>>,
    /// `must`-истоки аргументов по (цель вызова, сайт `file#line`, индекс аргумента).
    site_args_must: HashMap<(String, String, usize), BTreeSet<String>>,
    /// Истоки присваиваний по `lhs` (для property — `prop:<class>::<p>`).
    assigns: HashMap<String, BTreeSet<String>>,
    /// Записи `$this->p` из конструкторов: property → истоки (для привязки к месту создания).
    ctor_writes: HashMap<String, BTreeSet<String>>,
    /// Записи `$this->p` вне конструкторов: id метода → (property, истоки).
    this_writes: HashMap<String, Vec<(String, Vec<String>)>>,
    /// Записи свойства из сеттеров, привязанные к месту создания: (property, class, site) → истоки.
    setter_writes: HashMap<(String, String, String), BTreeSet<String>>,
    /// Истоки получателей вызовов по (цель вызова, сайт `file#line`).
    receivers: HashMap<(String, String), BTreeSet<String>>,
    /// Все чтения проекта.
    reads: Vec<ReadFact>,
    /// Прямые родители: класс → полные имена родителей.
    parents: HashMap<String, Vec<String>>,
    /// Обратная карта: класс → прямые потомки (реализации интерфейсов/трейтов).
    children: HashMap<String, Vec<String>>,
    /// `must`-истоки property по `lhs`; повторные записи сужают пересечением.
    prop_must: HashMap<String, BTreeSet<String>>,
    /// Сопоставление `(класс, property) → (таблица, колонка)`.
    mapping: HashMap<(String, String), (String, String)>,
    /// Транзитивное раскрытие каждого встреченного атома.
    expanded: HashMap<String, BTreeSet<String>>,
    /// Кэш раскрытия с контекстом получателя: (атом, контекст, глубина) → множество.
    ctx_expanded: std::cell::RefCell<HashMap<(String, String, usize), BTreeSet<String>>>,
}

impl Index {
    /// Построить индекс и посчитать фикспойнт (без наследования).
    pub fn build(files: &[FlowFile]) -> Self {
        Self::build_with_parents(files, &HashMap::new())
    }

    /// Построить индекс с учётом наследования: `parents` — класс → прямые родители (полные имена).
    /// Пустая карта — без наследования.
    pub fn build_with_parents(files: &[FlowFile], parents: &HashMap<String, Vec<String>>) -> Self {
        let mut index = Self {
            parents: parents.clone(),
            ..Self::default()
        };
        for (class, direct) in parents {
            for parent in direct {
                index
                    .children
                    .entry(parent.clone())
                    .or_default()
                    .push(class.clone());
            }
        }
        for children in index.children.values_mut() {
            children.sort();
            children.dedup();
        }
        for file in files {
            for method in &file.methods {
                let container = method.container.as_deref();
                // Возврат метода гарантирует атом, только если его гарантирует каждый `return`.
                let mut method_must: Option<BTreeSet<String>> = None;
                for ret in &method.returns {
                    let set: BTreeSet<String> = ret
                        .must
                        .iter()
                        .map(|origin| qualify(origin, &file.file, container))
                        .collect();
                    method_must = Some(match method_must {
                        Some(previous) => must_intersect(&previous, &set),
                        None => set,
                    });
                }
                if let Some(set) = method_must {
                    must_merge(&mut index.returns_must, method.id.clone(), set);
                }
                let returns = index.returns.entry(method.id.clone()).or_default();
                for ret in &method.returns {
                    returns.extend(
                        ret.origins
                            .iter()
                            .map(|origin| qualify(origin, &file.file, container)),
                    );
                }
                for assign in &method.assigns {
                    let lhs = qualify(&assign.lhs, &file.file, container);
                    if lhs.starts_with("prop:") {
                        let origins: Vec<String> = assign
                            .origins
                            .iter()
                            .map(|origin| qualify(origin, &file.file, container))
                            .collect();
                        // `must` property: повторная запись сужает гарантию; пустая — снимает.
                        let must: BTreeSet<String> = assign
                            .must
                            .iter()
                            .map(|origin| qualify(origin, &file.file, container))
                            .collect();
                        must_merge(&mut index.prop_must, lhs.clone(), must);
                        if lhs.starts_with("prop:this:") {
                            if is_constructor(&method.id) {
                                index
                                    .ctor_writes
                                    .entry(lhs.clone())
                                    .or_default()
                                    .extend(origins.iter().cloned());
                            } else {
                                index
                                    .this_writes
                                    .entry(method.id.clone())
                                    .or_default()
                                    .push((lhs.clone(), origins.clone()));
                            }
                        }
                        index.assigns.entry(lhs).or_default().extend(origins);
                    }
                }
                for arg in &method.args {
                    let site = format!("{}#{}", file.file, arg.line);
                    let origins: Vec<String> = arg
                        .origins
                        .iter()
                        .map(|origin| qualify(origin, &file.file, container))
                        .collect();
                    // `must`: атом гарантирован, только если он гарантирован на всех сайтах.
                    let must: BTreeSet<String> = arg
                        .must
                        .iter()
                        .map(|origin| qualify(origin, &file.file, container))
                        .collect();
                    index
                        .args
                        .entry((arg.target.clone(), arg.index))
                        .or_default()
                        .extend(origins.iter().cloned());
                    index
                        .site_args
                        .entry((arg.target.clone(), site.clone(), arg.index))
                        .or_default()
                        .extend(origins);
                    must_merge(
                        &mut index.args_must,
                        (arg.target.clone(), arg.index),
                        must.clone(),
                    );
                    must_merge(
                        &mut index.site_args_must,
                        (arg.target.clone(), site, arg.index),
                        must,
                    );
                }
                for receiver in &method.receivers {
                    let site = format!("{}#{}", file.file, receiver.line);
                    let origins: Vec<String> = receiver
                        .origins
                        .iter()
                        .map(|origin| qualify(origin, &file.file, container))
                        .collect();
                    index
                        .receivers
                        .entry((receiver.target.clone(), site))
                        .or_default()
                        .extend(origins);
                }
                for read in &method.reads {
                    index.reads.push(ReadFact {
                        file: file.file.clone(),
                        method: method.id.clone(),
                        line: read.line,
                        expr: read.expr.clone(),
                        origins: read
                            .origins
                            .iter()
                            .map(|origin| qualify(origin, &file.file, container))
                            .collect(),
                        must: read
                            .must
                            .iter()
                            .map(|origin| qualify(origin, &file.file, container))
                            .collect(),
                    });
                }
            }
        }
        index.build_setter_writes();
        index.fixpoint();
        index
    }

    /// Добавить сопоставление `(класс, property) → (таблица, колонка)`: раскрытие property
    /// получает терминальный атом `sql:<table>.<column>`. Раскрытие пересчитывается.
    pub fn with_mapping(mut self, mapping: &crate::flow::Mapping) -> Self {
        self.mapping = mapping
            .columns
            .iter()
            .map(|((class, property), column)| {
                (
                    (class.clone(), property.clone()),
                    (column.table.clone(), column.column.clone()),
                )
            })
            .collect();
        self.expanded.clear();
        self.ctx_expanded.borrow_mut().clear();
        self.fixpoint();
        self
    }

    /// Цепочки, в которых встречается `term` (ключ/колонка/property).
    pub fn traces(&self, term: &str) -> Vec<Trace> {
        let term = normalize_term(term);
        if term.is_empty() {
            return Vec::new();
        }
        let key_atom = format!("key:{term}");
        let prop_suffix = format!("::{term}");
        let mut traces = Vec::new();
        for read in &self.reads {
            let set = self.read_origins(read);
            let Some(matched) = match_atom(&set, &term, &key_atom, &prop_suffix) else {
                continue;
            };
            // Колонка — сам совпавший атом; для совпадения по последнему сегменту — голый терм.
            let column = if matched.starts_with("key:")
                || matched.starts_with("prop:")
                || matched.starts_with("sql:")
            {
                matched.clone()
            } else {
                term.clone()
            };
            let must = self.read_must(read);
            traces.push(Trace {
                must: must_atom(&must, &matched),
                column,
                chain: self.chain(read, &matched),
                file: read.file.clone(),
                method: read.method.clone(),
                line: read.line,
                expr: read.expr.clone(),
            });
        }
        traces.sort_by(|left, right| {
            left.file
                .cmp(&right.file)
                .then_with(|| left.line.cmp(&right.line))
                .then_with(|| left.expr.cmp(&right.expr))
        });
        traces.dedup_by(|left, right| {
            left.file == right.file
                && left.line == right.line
                && left.expr == right.expr
                && left.column == right.column
        });
        traces.truncate(TRACE_LIMIT);
        traces
    }

    /// Непосредственные потомки атома без контекста получателя.
    fn direct(&self, atom: &str) -> BTreeSet<String> {
        self.direct_ctx(atom, &BTreeSet::new())
    }

    /// Непосредственные потомки атома; `this` в получателе вызова раскрывается контекстом `ctx`.
    /// Для call-site `c:`-атома свойство `prop:this:<class>::<p>` раскрывается по получателю вызова.
    fn direct_ctx(&self, atom: &str, ctx: &BTreeSet<String>) -> BTreeSet<String> {
        if let Some(rest) = atom.strip_prefix("c:")
            && let (target, Some(site)) = split_call(rest)
            && let Some(origins) = self.returns_for_call(target)
        {
            let mut set = BTreeSet::new();
            for origin in &origins {
                if is_this_property(origin) {
                    set.extend(self.receiver_property(target, site, origin, ctx));
                    continue;
                }
                if let Some((param_target, index)) = parse_param(origin) {
                    // Собственный параметр метода (в т.ч. унаследованного с тем же именем):
                    // аргументы именно этого вызова; их отсутствие — пустой набор, не глобальные.
                    if same_method(param_target, target) {
                        if let Some(args) = self.site_args_for(target, site, index) {
                            set.extend(args.iter().cloned());
                        }
                        continue;
                    }
                }
                // Прочие истоки (включая `p:<other>#<i>`) раскрываются обычным путём:
                // `p:` берёт глобальные аргументы.
                set.insert(origin.clone());
            }
            return set;
        }
        self.raw_direct(atom)
    }

    /// Транзитивное раскрытие атома для сопоставления чтений: вложенный вызов `$this->m()`
    /// наследует получателей вызывающего, поэтому контекст пробрасывается по цепочке.
    fn expand_closure(&self, atom: &str, ctx: &BTreeSet<String>, depth: usize) -> BTreeSet<String> {
        let ctx = cap_ctx(ctx);
        let key = (atom.to_string(), ctx_key(&ctx), depth);
        if let Some(cached) = self.ctx_expanded.borrow().get(&key) {
            return cached.clone();
        }
        // Заглушка от циклов: повторный вход в тот же ключ даёт пустое множество.
        self.ctx_expanded
            .borrow_mut()
            .insert(key.clone(), BTreeSet::new());
        let mut set = BTreeSet::new();
        set.insert(atom.to_string());
        if depth < CONTEXT_DEPTH {
            self.closure_step(atom, &ctx, depth + 1, &mut set);
        }
        self.ctx_expanded.borrow_mut().insert(key, set.clone());
        set
    }

    /// Один шаг раскрытия с контекстом получателя; контекст вложенного вызова — места создания
    /// получателя вызывающего метода.
    fn closure_step(
        &self,
        atom: &str,
        ctx: &BTreeSet<String>,
        depth: usize,
        set: &mut BTreeSet<String>,
    ) {
        let call = atom
            .strip_prefix("c:")
            .map(split_call)
            .and_then(|(target, site)| site.map(|site| (target, site)));
        let Some((target, site)) = call else {
            for child in self.raw_direct(atom) {
                set.extend(self.expand_closure(&child, ctx, depth));
            }
            return;
        };
        let Some(origins) = self.returns_for_call(target) else {
            return;
        };
        let allocations = self.site_allocations(target, site, ctx);
        let callee_ctx: BTreeSet<String> = allocations
            .iter()
            .map(|(class, allocation_site)| format!("new:{class}@{allocation_site}"))
            .collect();
        for origin in &origins {
            if is_this_property(origin) {
                if allocations.is_empty() {
                    set.extend(self.expand_closure(origin, ctx, depth));
                } else {
                    for (class, allocation_site) in &allocations {
                        let property = format!("{origin}@new:{class}@{allocation_site}");
                        set.extend(self.expand_closure(&property, ctx, depth));
                    }
                }
                continue;
            }
            if let Some((param_target, index)) = parse_param(origin)
                && same_method(param_target, target)
            {
                if let Some(args) = self.site_args_for(target, site, index) {
                    for arg in args {
                        set.extend(self.expand_closure(arg, ctx, depth));
                    }
                }
                continue;
            }
            // Вложенный вызов выполняется на том же объекте: контекст — места создания callee.
            let child_ctx = if origin.starts_with("c:") {
                &callee_ctx
            } else {
                ctx
            };
            set.extend(self.expand_closure(origin, child_ctx, depth));
        }
    }

    /// Раскрытие без привязки к получателю: `p:` — глобальные аргументы, `c:` — возвраты
    /// (call-site — аргументы своего вызова), `prop:` — записи свойства.
    fn raw_direct(&self, atom: &str) -> BTreeSet<String> {
        // `prop:...@new:<class>@<file>#<line>`: свойство, привязанное к месту создания
        if let Some((property, allocation)) = atom.split_once('@')
            && property.starts_with("prop:")
            && let Some((class, Some(site))) = parse_new(allocation)
        {
            let mut set = self.allocation_property(property, class, site);
            if let Some(sql) = self.mapping_sql(property) {
                set.insert(sql);
            }
            return set;
        }
        if atom.starts_with("prop:") {
            let mut set = self.assigns.get(atom).cloned().unwrap_or_default();
            if let Some(sql) = self.mapping_sql(atom) {
                set.insert(sql);
            }
            return set;
        }
        if let Some(rest) = atom.strip_prefix("c:") {
            let (target, site) = split_call(rest);
            let Some(origins) = self.returns_for_call(target) else {
                return BTreeSet::new();
            };
            let Some(site) = site else {
                // Голый `c:<method>`: текущее глобальное раскрытие.
                return origins;
            };
            let mut set = BTreeSet::new();
            for origin in &origins {
                if let Some((param_target, index)) = parse_param(origin)
                    && same_method(param_target, target)
                {
                    if let Some(args) = self.site_args_for(target, site, index) {
                        set.extend(args.iter().cloned());
                    }
                    continue;
                }
                set.insert(origin.clone());
            }
            return set;
        }
        if let Some(rest) = atom.strip_prefix("p:")
            && let Some((target, index)) = rest.rsplit_once('#')
            && let Ok(index) = index.parse::<usize>()
            && let Some(set) = self.args_for(target, index)
        {
            return set.clone();
        }
        BTreeSet::new()
    }

    /// Истоки свойства для получателя вызова: раскрытие получателя до мест создания объекта.
    /// `this` раскрывается контекстом `ctx`; если мест создания нет — глобальные записи свойства.
    fn receiver_property(
        &self,
        target: &str,
        site: &str,
        property: &str,
        ctx: &BTreeSet<String>,
    ) -> BTreeSet<String> {
        let mut set = BTreeSet::new();
        if let Some(receivers) = self.receivers.get(&(target.to_string(), site.to_string())) {
            for receiver in receivers {
                let allocations = self.receiver_allocations(receiver, ctx);
                if allocations.is_empty() {
                    // `p:` без аргументов, `this` без контекста, `unknown` — глобальные записи.
                    set.insert(property.to_owned());
                    continue;
                }
                // свойство с контекстом места создания: `prop:...@new:<class>@<file>#<line>`
                for (class, allocation_site) in allocations {
                    set.insert(format!("{property}@new:{class}@{allocation_site}"));
                }
            }
        }
        if set.is_empty() {
            set.insert(property.to_owned());
        }
        set
    }

    /// Записи свойства без привязки к объекту.
    fn property_writes(&self, property: &str) -> BTreeSet<String> {
        self.assigns.get(property).cloned().unwrap_or_default()
    }

    /// Места создания получателей вызова `(цель, сайт)`; `this` раскрывается контекстом `ctx`.
    fn site_allocations(
        &self,
        target: &str,
        site: &str,
        ctx: &BTreeSet<String>,
    ) -> Vec<(String, String)> {
        let mut allocations = Vec::new();
        if let Some(receivers) = self.receivers.get(&(target.to_string(), site.to_string())) {
            for receiver in receivers {
                allocations.extend(self.receiver_allocations(receiver, ctx));
            }
        }
        allocations.sort();
        allocations.dedup();
        allocations.truncate(CONTEXT_ATOM_LIMIT);
        allocations
    }

    /// Места создания объекта в истоках получателя: раскрытие до неподвижной точки
    /// (`p:` → глобальные аргументы, `c:` → возвраты, `prop:` → записи) и отбор `new:`-атомов.
    fn receiver_allocations(
        &self,
        receiver: &str,
        ctx: &BTreeSet<String>,
    ) -> Vec<(String, String)> {
        let mut atoms = BTreeSet::new();
        let mut seen = BTreeSet::new();
        self.expand_receiver(receiver, ctx, 0, &mut seen, &mut atoms);
        let mut allocations = Vec::new();
        for atom in atoms {
            if let Some((class, Some(site))) = parse_new(&atom) {
                allocations.push((class.to_string(), site.to_string()));
            }
        }
        allocations
    }

    /// Раскрытие истока получателя до неподвижной точки с пределом глубины.
    /// `this` вне контекста вызова остаётся неизвестным получателем.
    fn expand_receiver(
        &self,
        origin: &str,
        ctx: &BTreeSet<String>,
        depth: usize,
        seen: &mut BTreeSet<String>,
        atoms: &mut BTreeSet<String>,
    ) {
        if origin == "this" {
            if ctx.is_empty() || !seen.insert(origin.to_string()) {
                return;
            }
            for receiver in ctx {
                self.expand_receiver(receiver, ctx, depth + 1, seen, atoms);
            }
            return;
        }
        if !seen.insert(origin.to_string()) {
            return;
        }
        atoms.insert(origin.to_string());
        if depth >= RECEIVER_DEPTH {
            return;
        }
        for child in self.raw_direct(origin) {
            self.expand_receiver(&child, ctx, depth + 1, seen, atoms);
        }
    }

    /// Записи свойства, раскрытые аргументами конструктора этого места создания, плюс записи
    /// сеттеров, чей вызов получателем указывает на это место создания.
    fn allocation_property(&self, property: &str, class: &str, site: &str) -> BTreeSet<String> {
        let property = self.receiver_property_atom(property, class);
        let mut set = BTreeSet::new();
        // Конструкторы привязаны к месту создания; прочие `$this->p` — через `setter_writes`.
        let writes = if property.starts_with("prop:this:") {
            self.ctor_writes.get(&property).cloned().unwrap_or_default()
        } else {
            self.property_writes(&property)
        };
        for write in writes {
            set.extend(self.expand_allocation(&write, class, site, 0));
        }
        if let Some(extra) =
            self.setter_writes
                .get(&(property.clone(), class.to_string(), site.to_string()))
        {
            set.extend(extra.iter().cloned());
        }
        set
    }

    /// Trait-метод читает `$this->p` как `prop:this:<Trait>::p`, а запись живёт в классе,
    /// который использует trait. Если у получателя есть одноимённое свойство — берём его атом.
    fn receiver_property_atom(&self, property: &str, class: &str) -> String {
        let Some((prefix, name)) = property.rsplit_once("::") else {
            return property.to_owned();
        };
        if !prefix.starts_with("prop:this:") {
            return property.to_owned();
        }
        let candidate = format!("prop:this:{class}::{name}");
        let has = self
            .ctor_writes
            .get(&candidate)
            .is_some_and(|writes| !writes.is_empty())
            || self
                .assigns
                .get(&candidate)
                .is_some_and(|writes| !writes.is_empty())
            || self
                .prop_must
                .get(&candidate)
                .is_some_and(|writes| !writes.is_empty());
        if has { candidate } else { property.to_owned() }
    }

    /// Записи `$this->p` из методов-сеттеров, привязанные к месту создания: вызов сеттера
    /// раскрывается до мест создания, совпавшее место получает записи с аргументами своего вызова.
    fn build_setter_writes(&mut self) {
        let mut setter_writes: HashMap<(String, String, String), BTreeSet<String>> = HashMap::new();
        for (target, site) in self.receivers.keys() {
            let Some(writes) = self.this_writes.get(target) else {
                continue;
            };
            for (class, allocation_site) in self.site_allocations(target, site, &BTreeSet::new()) {
                for (property, origins) in writes.iter().take(SETTER_SITE_LIMIT) {
                    let entry = setter_writes
                        .entry((property.clone(), class.clone(), allocation_site.clone()))
                        .or_default();
                    for origin in origins {
                        if let Some((param_target, index)) = parse_param(origin)
                            && same_method(param_target, target)
                        {
                            if let Some(args) = self.site_args_for(target, site, index) {
                                for arg in args {
                                    entry.extend(self.expand_allocation(
                                        arg,
                                        &class,
                                        &allocation_site,
                                        0,
                                    ));
                                }
                            }
                            continue;
                        }
                        entry.extend(self.expand_allocation(origin, &class, &allocation_site, 0));
                    }
                }
            }
        }
        self.setter_writes = setter_writes;
    }

    /// Подстановка аргументов конструктора места создания в истоки записи свойства.
    /// `p:<C0>::__construct#<k>` для класса создания заменяется аргументами этого вызова;
    /// прочие `p:` сначала раскрываются глобальными аргументами (форвардинг `parent::__construct`).
    fn expand_allocation(
        &self,
        origin: &str,
        class: &str,
        site: &str,
        depth: usize,
    ) -> BTreeSet<String> {
        let mut set = BTreeSet::new();
        if depth >= ALLOCATION_DEPTH {
            set.insert(origin.to_string());
            return set;
        }
        if let Some((target, index)) = parse_param(origin) {
            let own_constructor = target
                .rsplit_once("::")
                .is_some_and(|(owner, method)| owner == class && method == "__construct");
            if own_constructor {
                if let Some(args) = self.site_args_for(target, site, index) {
                    for arg in args {
                        set.extend(self.expand_allocation(arg, class, site, depth + 1));
                    }
                }
                return set;
            }
            if let Some(args) = self.args_for(target, index) {
                for arg in args {
                    set.extend(self.expand_allocation(arg, class, site, depth + 1));
                }
                return set;
            }
            set.insert(origin.to_string());
            return set;
        }
        set.insert(origin.to_string());
        set
    }

    /// Истоки возвратов по цели вызова: `Класс::метод` с наследованием, иначе точный id (функция).
    fn returns_for_target(&self, target: &str) -> Option<&BTreeSet<String>> {
        match target.rsplit_once("::") {
            Some((class, method)) => self.returns_for(class, method),
            None => self.returns.get(target),
        }
    }

    /// Истоки возвратов с диспетчеризацией: точный id и родители первыми (точнее),
    /// затем потомки интерфейса/трейта — объединение возвратов реализаций.
    fn returns_for_call(&self, target: &str) -> Option<BTreeSet<String>> {
        if let Some(set) = self.returns_for_target(target) {
            return Some(set.clone());
        }
        let (class, method) = target.rsplit_once("::")?;
        let mut set = BTreeSet::new();
        for id in self.descendant_method_ids(class, method) {
            if let Some(origins) = self.returns.get(&id) {
                set.extend(origins.iter().cloned());
            }
        }
        (!set.is_empty()).then_some(set)
    }

    /// `must`-возвраты с диспетчеризацией: пересечение реализаций потомков.
    fn returns_must_for_call(&self, target: &str) -> Option<BTreeSet<String>> {
        if let Some(set) = self.returns_must_for_target(target) {
            return Some(set.clone());
        }
        let (class, method) = target.rsplit_once("::")?;
        self.descendant_returns_must(class, method)
    }

    /// Транзитивные потомки `class`, определяющие `<Child>::<method>` (точный id).
    fn descendant_method_ids(&self, class: &str, method: &str) -> Vec<String> {
        let mut ids = Vec::new();
        let mut queue = VecDeque::new();
        let mut visited = BTreeSet::new();
        visited.insert(class.to_string());
        queue.push_back(class.to_string());
        while let Some(current) = queue.pop_front() {
            let Some(children) = self.children.get(&current) else {
                continue;
            };
            for child in children {
                if !visited.insert(child.clone()) {
                    continue;
                }
                let id = format!("{child}::{method}");
                if self.returns.contains_key(&id) {
                    ids.push(id);
                }
                queue.push_back(child.clone());
            }
        }
        ids
    }

    /// `must`-гарантия реализаций: атом остаётся, только если он есть у каждой.
    fn descendant_returns_must(&self, class: &str, method: &str) -> Option<BTreeSet<String>> {
        let ids = self.descendant_method_ids(class, method);
        if ids.is_empty() {
            return None;
        }
        let mut result: Option<BTreeSet<String>> = None;
        for id in ids {
            let set = self.returns_must.get(&id).cloned().unwrap_or_default();
            result = Some(match result {
                Some(previous) => must_intersect(&previous, &set),
                None => set,
            });
        }
        result
    }

    /// Терминальный `sql:<table>.<column>` для property, если сопоставление знает пару.
    fn mapping_sql(&self, property: &str) -> Option<String> {
        let rest = property.strip_prefix("prop:")?;
        let rest = rest.strip_prefix("this:").unwrap_or(rest);
        let (class, name) = rest.split_once("::")?;
        let (table, column) = self
            .mapping
            .get(&(class.to_string(), name.to_string()))
            .or_else(|| {
                let bare = name.strip_prefix('$')?;
                self.mapping.get(&(class.to_string(), bare.to_string()))
            })?;
        Some(format!("sql:{table}.{column}"))
    }

    /// `must` property: для места создания — записи конструктора с подстановкой аргументов;
    /// иначе — `prop_must`. Плюс терминальный `sql:`-атом по сопоставлению.
    fn must_property(&self, atom: &str, depth: usize) -> BTreeSet<String> {
        if let Some((property, allocation)) = atom.split_once('@')
            && property.starts_with("prop:")
            && let Some((class, Some(site))) = parse_new(allocation)
        {
            let property = self.receiver_property_atom(property, class);
            let mut set = BTreeSet::new();
            for origin in self.prop_must.get(&property).cloned().unwrap_or_default() {
                set.extend(self.expand_allocation(&origin, class, site, depth + 1));
            }
            if let Some(sql) = self.mapping_sql(&property) {
                set.insert(sql);
            }
            return set;
        }
        let mut set = self.prop_must.get(atom).cloned().unwrap_or_default();
        if let Some(sql) = self.mapping_sql(atom) {
            set.insert(sql);
        }
        set
    }

    /// Аргументы сайта `(цель, сайт, индекс)`; отсутствие — вызов без аргументов на этом сайте.
    fn site_args_for(&self, target: &str, site: &str, index: usize) -> Option<&BTreeSet<String>> {
        self.site_args
            .get(&(target.to_string(), site.to_string(), index))
    }

    /// Истоки возвратов метода: точный id, иначе первый родитель с точным `Родитель::метод`.
    fn returns_for(&self, class: &str, method: &str) -> Option<&BTreeSet<String>> {
        let id = self.ancestor_method(class, method, |id| self.returns.contains_key(id))?;
        self.returns.get(&id)
    }

    /// Истоки аргументов: точная пара (цель, индекс), иначе первый родитель с точной парой.
    fn args_for(&self, target: &str, index: usize) -> Option<&BTreeSet<String>> {
        if let Some(set) = self.args.get(&(target.to_string(), index)) {
            return Some(set);
        }
        let (class, method) = target.rsplit_once("::")?;
        let id = self.ancestor_method(class, method, |id| {
            self.args.contains_key(&(id.to_string(), index))
        })?;
        self.args.get(&(id, index))
    }

    /// Поиск в ширину по `parents` от класса: первый предок с `Предок::метод`, где `exists` истинно.
    fn ancestor_method<F>(&self, class: &str, method: &str, exists: F) -> Option<String>
    where
        F: Fn(&str) -> bool,
    {
        let exact = format!("{class}::{method}");
        if exists(&exact) {
            return Some(exact);
        }
        let mut queue = VecDeque::new();
        let mut visited = BTreeSet::new();
        visited.insert(class.to_string());
        queue.push_back(class.to_string());
        while let Some(current) = queue.pop_front() {
            let Some(parents) = self.parents.get(&current) else {
                continue;
            };
            for parent in parents {
                if !visited.insert(parent.clone()) {
                    continue;
                }
                let candidate = format!("{parent}::{method}");
                if exists(&candidate) {
                    return Some(candidate);
                }
                queue.push_back(parent.clone());
            }
        }
        None
    }

    /// Все атомы, встреченные в фактах: ключи раскрытия и все истоки.
    fn all_atoms(&self) -> BTreeSet<String> {
        let mut atoms = BTreeSet::new();
        for (method, origins) in &self.returns {
            atoms.insert(format!("c:{method}"));
            atoms.extend(origins.iter().cloned());
        }
        for ((target, index), origins) in &self.args {
            atoms.insert(format!("p:{target}#{index}"));
            atoms.extend(origins.iter().cloned());
        }
        for (lhs, origins) in &self.assigns {
            atoms.insert(lhs.clone());
            atoms.extend(origins.iter().cloned());
        }
        for ((target, _site), origins) in &self.receivers {
            atoms.insert(format!("c:{target}"));
            atoms.extend(origins.iter().cloned());
        }
        for read in &self.reads {
            atoms.extend(read.origins.iter().cloned());
        }
        atoms
    }

    /// Фикспойнт: множество атома = потомки ∪ раскрытие потомков, с пределом на размер.
    fn fixpoint(&mut self) {
        let mut atoms = self.all_atoms();
        // Замыкание по рёбрам раскрытия: найденный потомок тоже должен быть раскрыт.
        let mut queue: Vec<String> = atoms.iter().cloned().collect();
        while let Some(atom) = queue.pop() {
            for child in self.direct(&atom) {
                if atoms.insert(child.clone()) {
                    queue.push(child);
                }
            }
        }
        for atom in &atoms {
            self.expanded.entry(atom.clone()).or_default();
        }
        for _ in 0..ITERATION_LIMIT {
            let mut changed = false;
            for atom in &atoms {
                let mut set = self.direct(atom);
                let children: Vec<String> = set.iter().cloned().collect();
                for child in children {
                    if let Some(inner) = self.expanded.get(&child) {
                        set.extend(inner.iter().cloned());
                    }
                }
                while set.len() > ATOM_LIMIT {
                    let Some(last) = set.iter().next_back().cloned() else {
                        break;
                    };
                    set.remove(&last);
                }
                if self.expanded.get(atom) != Some(&set) {
                    self.expanded.insert(atom.clone(), set);
                    changed = true;
                }
            }
            if !changed {
                break;
            }
        }
    }

    /// Полное множество истоков чтения: сами истоки плюс раскрытие с контекстом получателя.
    fn read_origins(&self, read: &ReadFact) -> BTreeSet<String> {
        let mut set = BTreeSet::new();
        for origin in &read.origins {
            set.insert(origin.clone());
            set.extend(self.expand_closure(origin, &BTreeSet::new(), 0));
        }
        set
    }

    /// `must`-раскрытие атома: только гарантированные истоки. Любые отсутствующие данные
    /// (property, `this`, неизвестный вызов) дают пустое множество.
    fn must_expand(&self, atom: &str, depth: usize) -> BTreeSet<String> {
        let mut set = BTreeSet::new();
        if depth >= MUST_DEPTH {
            return set;
        }
        // `this` не несёт `must`-данных; property — через `prop_must`.
        if atom == "this" {
            return set;
        }
        if atom.starts_with("prop:") {
            return self.must_property(atom, depth);
        }
        // Терминальные атомы: исток сам по себе.
        if atom.starts_with("key:")
            || atom.starts_with("new:")
            || atom == "lit"
            || atom == "unknown"
        {
            set.insert(atom.to_string());
            return set;
        }
        if let Some((target, index)) = parse_param(atom) {
            if let Some(args) = self.args_must_for(target, index) {
                for arg in args {
                    set.extend(self.must_expand(arg, depth + 1));
                }
            }
            return set;
        }
        let Some(rest) = atom.strip_prefix("c:") else {
            return set;
        };
        let (target, site) = split_call(rest);
        let Some(origins) = self.returns_must_for_call(target) else {
            return set;
        };
        for origin in &origins {
            if let Some((param_target, index)) = parse_param(origin) {
                // Параметр своего вызова — аргументы именно этого сайта, иначе глобальные.
                let args = site
                    .filter(|_| same_method(param_target, target))
                    .and_then(|site| self.site_args_must_for(target, site, index))
                    .or_else(|| self.args_must_for(param_target, index));
                if let Some(args) = args {
                    for arg in args {
                        set.extend(self.must_expand(arg, depth + 1));
                    }
                }
                continue;
            }
            set.extend(self.must_expand(origin, depth + 1));
        }
        set
    }

    /// `must`-замыкание чтения: раскрытие `read.must`; пусто — гарантии нет.
    fn read_must(&self, read: &ReadFact) -> BTreeSet<String> {
        let mut set = BTreeSet::new();
        for origin in &read.must {
            set.extend(self.must_expand(origin, 0));
        }
        set
    }

    /// `must`-истоки возвратов по цели вызова: как `returns_for_target`, но по `returns_must`.
    fn returns_must_for_target(&self, target: &str) -> Option<&BTreeSet<String>> {
        match target.rsplit_once("::") {
            Some((class, method)) => self.returns_must_for(class, method),
            None => self.returns_must.get(target),
        }
    }

    /// `must`-истоки возвратов метода: точный id, иначе первый родитель с точным `Родитель::метод`.
    fn returns_must_for(&self, class: &str, method: &str) -> Option<&BTreeSet<String>> {
        let id = self.ancestor_method(class, method, |id| self.returns_must.contains_key(id))?;
        self.returns_must.get(&id)
    }

    /// `must`-истоки аргументов: точная пара (цель, индекс), иначе первый родитель с точной парой.
    fn args_must_for(&self, target: &str, index: usize) -> Option<&BTreeSet<String>> {
        if let Some(set) = self.args_must.get(&(target.to_string(), index)) {
            return Some(set);
        }
        let (class, method) = target.rsplit_once("::")?;
        let id = self.ancestor_method(class, method, |id| {
            self.args_must.contains_key(&(id.to_string(), index))
        })?;
        self.args_must.get(&(id, index))
    }

    /// `must`-аргументы сайта `(цель, сайт, индекс)`; отсутствие — нет гарантии.
    fn site_args_must_for(
        &self,
        target: &str,
        site: &str,
        index: usize,
    ) -> Option<&BTreeSet<String>> {
        self.site_args_must
            .get(&(target.to_string(), site.to_string(), index))
    }

    /// Цепочка от совпавшего атома до истока чтения, который его содержит.
    fn chain(&self, read: &ReadFact, matched: &str) -> Vec<String> {
        let mut origins: Vec<&String> = read.origins.iter().collect();
        origins.sort();
        origins.dedup();
        for origin in origins {
            if origin == matched {
                return vec![display_atom(matched)];
            }
            let contains = self
                .expanded
                .get(origin)
                .is_some_and(|set| set.contains(matched));
            if !contains {
                continue;
            }
            if let Some(path) = self.path(origin, matched) {
                return path.iter().map(|atom| display_atom(atom)).collect();
            }
        }
        vec![display_atom(matched)]
    }

    /// Путь по рёбрам раскрытия от `from` к `to`; возвращается в порядке «источник → чтение».
    fn path(&self, from: &str, to: &str) -> Option<Vec<String>> {
        if from == to {
            return Some(vec![to.to_string()]);
        }
        let mut parent: HashMap<String, String> = HashMap::new();
        let mut depth: HashMap<String, usize> = HashMap::new();
        let mut queue = VecDeque::new();
        depth.insert(from.to_string(), 0);
        queue.push_back(from.to_string());
        while let Some(node) = queue.pop_front() {
            let node_depth = depth.get(&node).copied().unwrap_or(0);
            if node_depth >= CHAIN_DEPTH {
                continue;
            }
            for child in self.direct(&node) {
                if child == to {
                    // Цепочка: совпавший атом, затем шаги раскрытия до истока чтения.
                    let mut chain = vec![to.to_string(), node.clone()];
                    let mut current = node.clone();
                    while let Some(previous) = parent.get(&current) {
                        chain.push(previous.clone());
                        current = previous.clone();
                    }
                    return Some(chain);
                }
                if child == "lit" || child == "unknown" {
                    continue;
                }
                if depth.contains_key(&child) {
                    continue;
                }
                depth.insert(child.clone(), node_depth + 1);
                parent.insert(child.clone(), node.clone());
                queue.push_back(child);
            }
        }
        None
    }
}

/// Конструктор ли метод (его записи `$this->p` привязаны к аргументам места создания).
fn is_constructor(id: &str) -> bool {
    id.rsplit_once("::")
        .is_some_and(|(_, method)| method == "__construct")
}

/// Контекст получателя с пределом числа атомов.
fn cap_ctx(ctx: &BTreeSet<String>) -> BTreeSet<String> {
    if ctx.len() <= CONTEXT_ATOM_LIMIT {
        return ctx.clone();
    }
    ctx.iter().take(CONTEXT_ATOM_LIMIT).cloned().collect()
}

/// Ключ контекста получателя для кэша раскрытия.
fn ctx_key(ctx: &BTreeSet<String>) -> String {
    ctx.iter().cloned().collect::<Vec<String>>().join("\u{1}")
}

/// Квалификация атома: call-site `c:`/`new:` → `@<file>#<line>`, `prop:this::<p>` → `prop:this:<class>::<p>`.
/// Атом без `@`, без номера строки или уже с `#` после `@` не меняется.
fn qualify(origin: &str, file: &str, container: Option<&str>) -> String {
    if let Some(rest) = origin.strip_prefix("c:") {
        return match rest.split_once('@') {
            Some((method, line)) if valid_call_site(method, line) => {
                format!("c:{method}@{file}#{line}")
            }
            _ => origin.to_string(),
        };
    }
    if let Some(rest) = origin.strip_prefix("new:") {
        return match rest.split_once('@') {
            Some((class, line)) if valid_call_site(class, line) => {
                format!("new:{class}@{file}#{line}")
            }
            _ => origin.to_string(),
        };
    }
    if let Some(rest) = origin.strip_prefix("prop:this::") {
        return match container {
            Some(class) if !rest.is_empty() => format!("prop:this:{class}::{rest}"),
            _ => origin.to_string(),
        };
    }
    origin.to_string()
}

/// Голова и номер строки call-site атома пригодны для квалификации.
fn valid_call_site(head: &str, line: &str) -> bool {
    !head.is_empty()
        && !line.is_empty()
        && !line.contains('#')
        && line.chars().all(|ch| ch.is_ascii_digit())
}

/// Разбор тела `c:`-атома на цель и сайт `file#line` (только для квалифицированного атома).
fn split_call(rest: &str) -> (&str, Option<&str>) {
    match rest.split_once('@') {
        Some((method, site)) if site.contains('#') && !method.is_empty() => (method, Some(site)),
        _ => (rest, None),
    }
}

/// `c:`-атом без call-site суффикса `@<file>#<line>`.
fn call_base(atom: &str) -> &str {
    let Some(rest) = atom.strip_prefix("c:") else {
        return atom;
    };
    match rest.split_once('@') {
        Some((method, site)) if site.contains('#') && !method.is_empty() => {
            &atom[..2 + method.len()]
        }
        _ => atom,
    }
}

/// Атом без call-site суффикса: `c:<method>` и `new:<class>` (суффикс скрывается).
fn atom_base(atom: &str) -> &str {
    if (atom.starts_with("new:") || atom.starts_with("prop:"))
        && let Some(at) = atom.find('@')
        && atom[at + 1..].contains('#')
    {
        return &atom[..at];
    }
    call_base(atom)
}

/// Атом для выдачи: call-site суффикс скрывается, цепочка остаётся читаемой.
fn display_atom(atom: &str) -> String {
    atom_base(atom).to_string()
}

/// Пересечение двух `must`-множеств.
fn must_intersect(left: &BTreeSet<String>, right: &BTreeSet<String>) -> BTreeSet<String> {
    left.intersection(right).cloned().collect()
}

/// Записать `must`-множество под ключ: повторный сайт сужает его пересечением.
fn must_merge<K: std::hash::Hash + Eq>(
    map: &mut HashMap<K, BTreeSet<String>>,
    key: K,
    set: BTreeSet<String>,
) {
    match map.entry(key) {
        Entry::Vacant(slot) => {
            slot.insert(set);
        }
        Entry::Occupied(mut slot) => {
            let kept = must_intersect(slot.get(), &set);
            slot.insert(kept);
        }
    }
}

/// Совпавший атом гарантирован на каждом пути: его база есть в `must`-замыкании.
fn must_atom(must: &BTreeSet<String>, matched: &str) -> bool {
    let base = atom_base(matched);
    must.iter().any(|atom| atom_base(atom) == base)
}

/// Свойство текущего объекта: `prop:this::<p>` или квалифицированный `prop:this:<class>::<p>`.
fn is_this_property(atom: &str) -> bool {
    atom.starts_with("prop:this:")
}

/// Разбор `new:<class>@<file>#<line>`; у неквалифицированного сайта — `None`.
fn parse_new(atom: &str) -> Option<(&str, Option<&str>)> {
    let rest = atom.strip_prefix("new:")?;
    if rest.is_empty() {
        return None;
    }
    match rest.split_once('@') {
        Some((class, site)) if !class.is_empty() && site.contains('#') => Some((class, Some(site))),
        _ => Some((rest, None)),
    }
}

/// Разбор `p:<method>#<i>`.
/// Совпадает ли имя метода у двух id (для унаследованного вызова: `Child::m` → `Parent::m`).
fn same_method(left: &str, right: &str) -> bool {
    let method = |id: &str| id.rsplit_once("::").map(|(_, m)| m.to_owned());
    match (method(left), method(right)) {
        (Some(a), Some(b)) => a == b,
        (None, None) => left == right,
        _ => false,
    }
}

fn parse_param(atom: &str) -> Option<(&str, usize)> {
    let rest = atom.strip_prefix("p:")?;
    let (target, index) = rest.rsplit_once('#')?;
    Some((target, index.parse().ok()?))
}

/// Нормализация терма: обрезка пробелов, снятие ведущих `$` и `@`.
fn normalize_term(term: &str) -> String {
    let mut term = term.trim();
    if let Some(rest) = term.strip_prefix('$') {
        term = rest;
    }
    if let Some(rest) = term.strip_prefix('@') {
        term = rest;
    }
    term.to_string()
}

/// Совпавший атом: сначала `key:<term>`, затем `prop:...::<term>`, затем совпадение по последнему сегменту.
fn match_atom(
    set: &BTreeSet<String>,
    term: &str,
    key_atom: &str,
    prop_suffix: &str,
) -> Option<String> {
    let qualified = term.matches('.').count() == 1;
    let mut prop = None;
    let mut bare = None;
    for atom in set {
        if atom == "lit" || atom == "unknown" {
            continue;
        }
        if atom == key_atom {
            return Some(atom.clone());
        }
        if let Some(value) = atom.strip_prefix("sql:") {
            // Квалифицированный терм `<table>.<column>` — точное совпадение без учёта регистра;
            // иначе — совпадение по имени колонки (последний сегмент).
            if qualified {
                if value.eq_ignore_ascii_case(term) {
                    return Some(atom.clone());
                }
            } else if last_segment(atom) == term {
                bare.get_or_insert_with(|| atom.clone());
            }
            continue;
        }
        if atom.starts_with("prop:") && atom.ends_with(prop_suffix) {
            prop.get_or_insert_with(|| atom.clone());
        } else if last_segment(atom) == term {
            bare.get_or_insert_with(|| atom.clone());
        }
    }
    prop.or(bare)
}

/// Последний сегмент атома после `::` или `.` (call-site суффикс не учитывается).
fn last_segment(atom: &str) -> &str {
    let atom = atom_base(atom);
    let after_scope = atom.rsplit("::").next().unwrap_or(atom);
    after_scope.rsplit('.').next().unwrap_or(after_scope)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::flow::model::{ArgEdge, Assign, MethodFlow, Read, ReceiverEdge};

    fn read(line: u32, expr: &str, origins: &[&str]) -> Read {
        Read {
            line,
            expr: expr.into(),
            origins: origins.iter().map(|origin| (*origin).to_string()).collect(),
            must: Vec::new(),
        }
    }

    fn read_must(line: u32, expr: &str, origins: &[&str], must: &[&str]) -> Read {
        Read {
            line,
            expr: expr.into(),
            origins: origins.iter().map(|origin| (*origin).to_string()).collect(),
            must: must.iter().map(|origin| (*origin).to_string()).collect(),
        }
    }

    fn method(id: &str) -> MethodFlow {
        MethodFlow {
            id: id.into(),
            ..Default::default()
        }
    }

    fn file(file: &str, methods: Vec<MethodFlow>) -> FlowFile {
        FlowFile {
            file: file.into(),
            methods,
        }
    }

    fn assign(line: u32, lhs: &str, origins: &[&str]) -> Assign {
        Assign {
            line,
            lhs: lhs.into(),
            origins: origins.iter().map(|origin| (*origin).to_string()).collect(),
            must: Vec::new(),
        }
    }

    fn assign_must(line: u32, lhs: &str, origins: &[&str], must: &[&str]) -> Assign {
        Assign {
            line,
            lhs: lhs.into(),
            origins: origins.iter().map(|origin| (*origin).to_string()).collect(),
            must: must.iter().map(|origin| (*origin).to_string()).collect(),
        }
    }

    fn arg(line: u32, target: &str, index: usize, origins: &[&str]) -> ArgEdge {
        ArgEdge {
            line,
            target: target.into(),
            index,
            origins: origins.iter().map(|origin| (*origin).to_string()).collect(),
            must: Vec::new(),
        }
    }

    fn arg_must(line: u32, target: &str, index: usize, origins: &[&str], must: &[&str]) -> ArgEdge {
        ArgEdge {
            line,
            target: target.into(),
            index,
            origins: origins.iter().map(|origin| (*origin).to_string()).collect(),
            must: must.iter().map(|origin| (*origin).to_string()).collect(),
        }
    }

    fn receiver(line: u32, target: &str, origins: &[&str]) -> ReceiverEdge {
        ReceiverEdge {
            line,
            target: target.into(),
            origins: origins.iter().map(|origin| (*origin).to_string()).collect(),
        }
    }

    fn has(trace: &Trace, atom: &str) -> bool {
        trace.chain.iter().any(|found| found == atom)
    }

    fn first(trace: &Trace) -> Option<&str> {
        trace.chain.first().map(String::as_str)
    }

    /// `c:<method>` раскрывается до `returns`, чтение находит ключ через цепочку вызова.
    #[test]
    fn call_return_chain() {
        let mut repo = method("A::repo");
        repo.returns
            .push(read(3, "$this->service_type", &["key:service_type"]));
        let mut user = method("A::use");
        user.assigns.push(assign(5, "$x", &["c:A::repo"]));
        user.reads.push(read(6, "$x", &["c:A::repo"]));
        let index = Index::build(&[file("a.php", vec![repo, user])]);

        let traces = index.traces("service_type");
        assert_eq!(traces.len(), 1);
        assert_eq!((traces[0].method.as_str(), traces[0].line), ("A::use", 6));
        assert!(has(&traces[0], "c:A::repo") && has(&traces[0], "key:service_type"));
        assert_eq!(first(&traces[0]), Some("key:service_type"));
        // Терм нормализуется: ведущие `$` и `@` снимаются.
        assert_eq!(index.traces("$service_type").len(), 1);
        assert_eq!(index.traces("@service_type").len(), 1);
    }

    /// Вызов без аргументов на своём сайте не наследует аргументы других вызовов того же метода.
    #[test]
    fn zero_arg_call_does_not_inherit_other_sites() {
        let mut helper = method("H::isEn");
        helper.params = 2;
        helper.returns.push(read(2, "$v", &["p:H::isEn#0"]));
        let mut user = method("A::use");
        user.args.push(arg(10, "H::isEn", 0, &["key:name_en"]));
        user.reads.push(read(11, "$a", &["c:H::isEn@10"]));
        user.reads.push(read(12, "$b", &["c:H::isEn@12"]));
        let index = Index::build(&[file("a.php", vec![helper, user])]);

        let traces = index.traces("name_en");
        assert_eq!(traces.len(), 1, "{traces:#?}");
        assert_eq!(traces[0].line, 11);
    }

    /// `p:<method>#<i>` раскрывается до истоков аргументов вызова.
    #[test]
    fn param_to_args_chain() {
        let mut make = method("A::make");
        make.returns.push(read(2, "$id", &["p:A::make#0"]));
        let mut user = method("A::use");
        user.args.push(arg(8, "A::make", 0, &["key:id"]));
        user.reads.push(read(9, "$row['id']", &["c:A::make"]));
        let index = Index::build(&[file("a.php", vec![make, user])]);

        let traces = index.traces("id");
        assert_eq!(traces.len(), 1);
        assert_eq!(traces[0].line, 9);
        assert!(has(&traces[0], "p:A::make#0") && has(&traces[0], "key:id"));
        assert!(has(&traces[0], "c:A::make"));
    }

    /// `prop:<class>::<p>` раскрывается до истоков присваивания property.
    #[test]
    fn property_chain() {
        let mut init = method("A::init");
        init.assigns.push(assign(4, "prop:A::$p", &["key:name_en"]));
        let mut get = method("A::get");
        get.returns.push(read(7, "$this->p", &["prop:A::$p"]));
        let mut run = method("A::run");
        run.reads.push(read(11, "$this->get()", &["c:A::get"]));
        let index = Index::build(&[file("a.php", vec![init, get, run])]);

        let traces = index.traces("name_en");
        assert_eq!(traces.len(), 1);
        assert_eq!(traces[0].column, "key:name_en");
        assert_eq!(first(&traces[0]), Some("key:name_en"));
        assert!(has(&traces[0], "prop:A::$p") && has(&traces[0], "c:A::get"));
        assert_eq!(traces[0].chain.last().map(String::as_str), Some("c:A::get"));
    }

    /// Наследование: `c:<класс>::<метод>` без точного id резолвится к методу родителя.
    #[test]
    fn inherited_method_chain() {
        let mut parent = method("QueueNoId::getServiceType");
        parent.returns.push(read(
            3,
            "$this->serviceType",
            &["prop:QueueNoId::serviceType"],
        ));
        let mut user = method("Queue::run");
        user.reads.push(read(
            9,
            "$queue->getServiceType()",
            &["c:Queue::getServiceType"],
        ));
        let mut parents = HashMap::new();
        parents.insert("Queue".to_string(), vec!["QueueNoId".to_string()]);
        let index = Index::build_with_parents(&[file("a.php", vec![parent, user])], &parents);

        let traces = index.traces("serviceType");
        assert_eq!(traces.len(), 1);
        assert_eq!(traces[0].method, "Queue::run");
        assert_eq!(first(&traces[0]), Some("prop:QueueNoId::serviceType"));
        assert!(has(&traces[0], "c:Queue::getServiceType"));
        assert_eq!(
            traces[0].chain.last().map(String::as_str),
            Some("c:Queue::getServiceType")
        );
    }

    /// Неизвестный терм не даёт цепочек.
    #[test]
    fn no_match_is_empty() {
        let mut repo = method("A::repo");
        repo.returns
            .push(read(3, "$this->service_type", &["key:service_type"]));
        let index = Index::build(&[file("a.php", vec![repo])]);
        assert!(index.traces("name_en").is_empty());
        assert!(index.traces("   ").is_empty());
    }

    /// Два вызова `H::isEn` с разными аргументами: ключ приходит только из своего call-site.
    fn call_site_index() -> Index {
        let mut is_en = method("H::isEn");
        is_en.returns.push(read(2, "$value", &["p:H::isEn#0"]));
        let mut user = method("A::use");
        user.args.push(arg(3, "H::isEn", 0, &["lit"]));
        user.args.push(arg(5, "H::isEn", 0, &["key:name_en"]));
        user.assigns.push(assign(3, "$a", &["c:H::isEn@3"]));
        user.assigns.push(assign(5, "$b", &["c:H::isEn@5"]));
        user.reads.push(read(4, "$a", &["c:H::isEn@3"]));
        user.reads.push(read(6, "$b", &["c:H::isEn@5"]));
        Index::build(&[file("/abs/file.php", vec![is_en, user])])
    }

    /// `c:<method>@<line>` раскрывает `p:<method>#<i>` аргументами именно своего вызова.
    #[test]
    fn call_site_precision() {
        let index = call_site_index();
        let traces = index.traces("name_en");
        assert_eq!(traces.len(), 1);
        assert_eq!(traces[0].line, 6);
        assert_eq!(traces[0].expr, "$b");
        assert!(has(&traces[0], "c:H::isEn") && has(&traces[0], "key:name_en"));
    }

    /// Цепочка скрывает call-site суффикс `@<file>#<line>`.
    #[test]
    fn chain_hides_call_site_suffix() {
        let index = call_site_index();
        let traces = index.traces("name_en");
        assert_eq!(traces.len(), 1);
        assert!(
            traces[0].chain.iter().all(|atom| !atom.contains('@')),
            "chain: {:?}",
            traces[0].chain
        );
        assert_eq!(
            traces[0].chain.last().map(String::as_str),
            Some("c:H::isEn")
        );
    }

    /// Два создания одного класса с разными аргументами: геттер читает свойство своего объекта.
    #[test]
    fn allocation_site_binds_the_property() {
        let mut ctor = method("Item::__construct");
        ctor.container = Some("Item".into());
        ctor.params = 1;
        ctor.assigns
            .push(assign(3, "prop:this::name", &["p:Item::__construct#0"]));
        let mut getter = method("Item::getName");
        getter.container = Some("Item".into());
        getter
            .returns
            .push(read(7, "$this->name", &["prop:this::name"]));
        let mut run = method("A::run");
        run.args
            .push(arg(20, "Item::__construct", 0, &["key:name_en"]));
        run.args.push(arg(21, "Item::__construct", 0, &["lit"]));
        run.receivers
            .push(receiver(22, "Item::getName", &["new:Item@20"]));
        run.receivers
            .push(receiver(23, "Item::getName", &["new:Item@21"]));
        run.reads
            .push(read(22, "$a->getName()", &["c:Item::getName@22"]));
        run.reads
            .push(read(23, "$b->getName()", &["c:Item::getName@23"]));
        let index = Index::build(&[file("a.php", vec![ctor, getter, run])]);

        let traces = index.traces("name_en");
        assert_eq!(traces.len(), 1, "{traces:#?}");
        assert_eq!(traces[0].line, 22);
    }

    /// Форвардинг `parent::__construct` сохраняет место создания: свойство родителя видно.
    #[test]
    fn parent_constructor_forwarding_keeps_the_allocation() {
        let mut parent_ctor = method("Parent::__construct");
        parent_ctor.container = Some("Parent".into());
        parent_ctor.params = 1;
        parent_ctor
            .assigns
            .push(assign(3, "prop:this::name", &["p:Parent::__construct#0"]));
        let mut parent_get = method("Parent::getName");
        parent_get.container = Some("Parent".into());
        parent_get
            .returns
            .push(read(6, "$this->name", &["prop:this::name"]));
        let mut child_ctor = method("Child::__construct");
        child_ctor.container = Some("Child".into());
        child_ctor.params = 1;
        child_ctor.args.push(arg(
            10,
            "Parent::__construct",
            0,
            &["p:Child::__construct#0"],
        ));
        let mut run = method("A::run");
        run.args
            .push(arg(20, "Child::__construct", 0, &["key:name_en"]));
        run.args.push(arg(21, "Child::__construct", 0, &["lit"]));
        run.receivers
            .push(receiver(22, "Child::getName", &["new:Child@20"]));
        run.receivers
            .push(receiver(23, "Child::getName", &["new:Child@21"]));
        run.reads
            .push(read(22, "$a->getName()", &["c:Child::getName@22"]));
        run.reads
            .push(read(23, "$b->getName()", &["c:Child::getName@23"]));
        let mut parents = HashMap::new();
        parents.insert("Child".to_string(), vec!["Parent".to_string()]);
        let index = Index::build_with_parents(
            &[file(
                "a.php",
                vec![parent_ctor, parent_get, child_ctor, run],
            )],
            &parents,
        );

        let traces = index.traces("name_en");
        assert_eq!(traces.len(), 1, "{traces:#?}");
        assert_eq!(traces[0].line, 22);
    }

    /// Получатель через параметр: аргумент вызывающего — место создания объекта.
    #[test]
    fn receiver_through_a_parameter() {
        let mut ctor = method("Item::__construct");
        ctor.container = Some("Item".into());
        ctor.params = 1;
        ctor.assigns
            .push(assign(3, "prop:this::name", &["p:Item::__construct#0"]));
        let mut getter = method("Item::getName");
        getter.container = Some("Item".into());
        getter
            .returns
            .push(read(7, "$this->name", &["prop:this::name"]));
        let mut use_method = method("A::use");
        use_method.params = 1;
        use_method
            .receivers
            .push(receiver(8, "Item::getName", &["p:A::use#0"]));
        use_method
            .reads
            .push(read(8, "$item->getName()", &["c:Item::getName@8"]));
        let mut caller = method("A::caller");
        caller.params = 1;
        caller.args.push(arg(20, "A::use", 0, &["new:Item@20"]));
        caller
            .args
            .push(arg(20, "Item::__construct", 0, &["key:name_en"]));
        let index = Index::build(&[file("a.php", vec![ctor, getter, use_method, caller])]);

        let traces = index.traces("name_en");
        assert_eq!(traces.len(), 1, "{traces:#?}");
        assert_eq!(traces[0].method, "A::use");
        assert_eq!(traces[0].line, 8);
    }

    /// Неизвестный получатель: глобальные записи свойства, без паники.
    #[test]
    fn unknown_receiver_falls_back_to_global() {
        let mut init = method("Item::init");
        init.container = Some("Item".into());
        init.assigns
            .push(assign(4, "prop:this::name", &["key:name_en"]));
        let mut getter = method("Item::getName");
        getter.container = Some("Item".into());
        getter
            .returns
            .push(read(7, "$this->name", &["prop:this::name"]));
        let mut run = method("A::run");
        run.receivers
            .push(receiver(9, "Item::getName", &["unknown"]));
        run.reads
            .push(read(9, "$item->getName()", &["c:Item::getName@9"]));
        let index = Index::build(&[file("a.php", vec![init, getter, run])]);

        let traces = index.traces("name_en");
        assert_eq!(traces.len(), 1, "{traces:#?}");
        assert_eq!(traces[0].line, 9);
    }

    /// Получатель раскрывается через цепочку вызовов до места создания объекта.
    #[test]
    fn receiver_expands_through_calls() {
        let mut ctor = method("Item::__construct");
        ctor.container = Some("Item".into());
        ctor.params = 1;
        ctor.assigns
            .push(assign(3, "prop:this::name", &["p:Item::__construct#0"]));
        let mut make = method("A::make");
        make.args
            .push(arg(3, "Item::__construct", 0, &["key:name_en"]));
        make.returns.push(read(3, "new Item('x')", &["new:Item@3"]));
        let mut repo = method("A::repo");
        repo.returns
            .push(read(2, "$this->make()", &["c:A::make@2"]));
        let mut get = method("A::get");
        get.returns
            .push(read(4, "$this->name", &["prop:this:Item::name"]));
        let mut user = method("A::use");
        user.receivers.push(receiver(6, "A::get", &["c:A::repo@5"]));
        user.reads.push(read(6, "$x->get()", &["c:A::get@6"]));
        let index = Index::build(&[file("a.php", vec![ctor, make, repo, get, user])]);

        let traces = index.traces("name_en");
        assert_eq!(traces.len(), 1, "{traces:#?}");
        assert_eq!(traces[0].line, 6);
        assert!(has(&traces[0], "c:A::get") && has(&traces[0], "key:name_en"));
    }

    /// Сеттер привязан к своему месту создания: аргумент чужого вызова не подмешивается.
    #[test]
    fn setter_binds_to_the_allocation() {
        let mut ctor = method("Item::__construct");
        ctor.container = Some("Item".into());
        ctor.params = 1;
        ctor.assigns
            .push(assign(3, "prop:this::id", &["p:Item::__construct#0"]));
        let mut setter = method("Item::setName");
        setter.container = Some("Item".into());
        setter.params = 1;
        setter
            .assigns
            .push(assign(6, "prop:this::name", &["p:Item::setName#0"]));
        let mut getter = method("Item::getName");
        getter.container = Some("Item".into());
        getter
            .returns
            .push(read(9, "$this->name", &["prop:this::name"]));
        let mut run = method("A::run");
        run.args.push(arg(20, "Item::__construct", 0, &["lit"]));
        run.args
            .push(arg(21, "Item::__construct", 0, &["key:name_en"]));
        run.args.push(arg(22, "Item::setName", 0, &["lit"]));
        run.args.push(arg(23, "Item::setName", 0, &["key:name_en"]));
        run.receivers
            .push(receiver(22, "Item::setName", &["new:Item@20"]));
        run.receivers
            .push(receiver(23, "Item::setName", &["new:Item@21"]));
        run.receivers
            .push(receiver(24, "Item::getName", &["new:Item@20"]));
        run.receivers
            .push(receiver(25, "Item::getName", &["new:Item@21"]));
        run.reads
            .push(read(24, "$a->getName()", &["c:Item::getName@24"]));
        run.reads
            .push(read(25, "$b->getName()", &["c:Item::getName@25"]));
        let index = Index::build(&[file("a.php", vec![ctor, setter, getter, run])]);

        let traces = index.traces("name_en");
        assert_eq!(traces.len(), 1, "{traces:#?}");
        assert_eq!(traces[0].line, 25);
    }

    /// `$this->load()` наследует получателя своего call-site, а не всех вызовов метода.
    #[test]
    fn this_chain_uses_the_enclosing_receiver() {
        let mut ctor = method("A::__construct");
        ctor.container = Some("A".into());
        ctor.params = 1;
        ctor.assigns
            .push(assign(3, "prop:this::name", &["p:A::__construct#0"]));
        let mut load = method("A::load");
        load.container = Some("A".into());
        load.returns
            .push(read(7, "$this->name", &["prop:this::name"]));
        let mut run = method("A::run");
        run.container = Some("A".into());
        run.receivers.push(receiver(5, "A::load", &["this"]));
        run.returns.push(read(5, "$this->load()", &["c:A::load@5"]));
        let mut main = method("Main::go");
        main.args
            .push(arg(20, "A::__construct", 0, &["key:name_en"]));
        main.args.push(arg(21, "A::__construct", 0, &["lit"]));
        main.receivers.push(receiver(22, "A::run", &["new:A@20"]));
        main.receivers.push(receiver(23, "A::run", &["new:A@21"]));
        main.reads.push(read(22, "$a->run()", &["c:A::run@22"]));
        main.reads.push(read(23, "$b->run()", &["c:A::run@23"]));
        let index = Index::build(&[file("a.php", vec![ctor, load, run, main])]);

        let traces = index.traces("name_en");
        assert_eq!(traces.len(), 1, "{traces:#?}");
        assert_eq!(traces[0].method, "Main::go");
        assert_eq!(traces[0].line, 22);
    }

    /// Сеттер через параметр: запись привязывается к месту создания аргумента вызывающего.
    #[test]
    fn setter_through_a_parameter() {
        let mut setter = method("Item::setName");
        setter.container = Some("Item".into());
        setter.params = 1;
        setter
            .assigns
            .push(assign(4, "prop:this::name", &["p:Item::setName#0"]));
        let mut getter = method("Item::getName");
        getter.container = Some("Item".into());
        getter
            .returns
            .push(read(7, "$this->name", &["prop:this::name"]));
        let mut apply = method("A::apply");
        apply.params = 2;
        apply
            .receivers
            .push(receiver(8, "Item::setName", &["p:A::apply#0"]));
        apply
            .args
            .push(arg(8, "Item::setName", 0, &["p:A::apply#1"]));
        apply
            .receivers
            .push(receiver(9, "Item::getName", &["p:A::apply#0"]));
        apply
            .reads
            .push(read(9, "$item->getName()", &["c:Item::getName@9"]));
        let mut caller = method("A::caller");
        caller.params = 2;
        caller.args.push(arg(20, "A::apply", 0, &["new:Item@20"]));
        caller.args.push(arg(20, "A::apply", 1, &["key:name_en"]));
        let index = Index::build(&[file("a.php", vec![setter, getter, apply, caller])]);

        let traces = index.traces("name_en");
        assert_eq!(traces.len(), 1, "{traces:#?}");
        assert_eq!(traces[0].method, "A::apply");
        assert_eq!(traces[0].line, 9);
    }

    /// `this` без контекста вызова остаётся неизвестным: глобальные записи, без паники.
    #[test]
    fn unknown_this_falls_back() {
        let mut init = method("Item::init");
        init.container = Some("Item".into());
        init.assigns
            .push(assign(4, "prop:this::name", &["key:name_en"]));
        let mut getter = method("Item::getName");
        getter.container = Some("Item".into());
        getter
            .returns
            .push(read(7, "$this->name", &["prop:this::name"]));
        let mut run = method("A::run");
        run.receivers.push(receiver(9, "Item::getName", &["this"]));
        run.reads
            .push(read(9, "$this->getName()", &["c:Item::getName@9"]));
        let index = Index::build(&[file("a.php", vec![init, getter, run])]);

        let traces = index.traces("name_en");
        assert_eq!(traces.len(), 1, "{traces:#?}");
        assert_eq!(traces[0].line, 9);
    }

    /// Чтение с `must`-истоком: колонка доходит на каждом пути.
    #[test]
    fn must_is_true_when_the_read_is_must() {
        let mut run = method("A::run");
        run.reads.push(read_must(
            4,
            "$row['name_en']",
            &["key:name_en"],
            &["key:name_en"],
        ));
        let index = Index::build(&[file("a.php", vec![run])]);

        let traces = index.traces("name_en");
        assert_eq!(traces.len(), 1, "{traces:#?}");
        assert!(traces[0].must);
    }

    /// `must` чтения пуст: совпадение только may.
    #[test]
    fn must_is_false_for_a_may_only_read() {
        let mut run = method("A::run");
        run.reads.push(read(4, "$row['name_en']", &["key:name_en"]));
        let index = Index::build(&[file("a.php", vec![run])]);

        let traces = index.traces("name_en");
        assert_eq!(traces.len(), 1, "{traces:#?}");
        assert!(!traces[0].must);
    }

    /// Возврат метода гарантирован: `must` идёт по цепочке вызова.
    #[test]
    fn must_follows_a_must_return() {
        let mut get = method("A::get");
        get.returns.push(read_must(
            3,
            "$row['name_en']",
            &["key:name_en"],
            &["key:name_en"],
        ));
        let mut run = method("A::run");
        run.reads
            .push(read_must(9, "$x", &["c:A::get"], &["c:A::get"]));
        let index = Index::build(&[file("a.php", vec![get, run])]);

        let traces = index.traces("name_en");
        assert_eq!(traces.len(), 1, "{traces:#?}");
        assert!(traces[0].must);
    }

    /// Возврат метода только may: `must` не наследуется.
    #[test]
    fn must_is_false_when_the_callee_return_is_may_only() {
        let mut get = method("A::get");
        get.returns
            .push(read(3, "$row['name_en']", &["key:name_en"]));
        let mut run = method("A::run");
        run.reads
            .push(read_must(9, "$x", &["c:A::get"], &["c:A::get"]));
        let index = Index::build(&[file("a.php", vec![get, run])]);

        let traces = index.traces("name_en");
        assert_eq!(traces.len(), 1, "{traces:#?}");
        assert!(!traces[0].must);
    }

    /// `must` проходит через гарантированный аргумент своего call-site.
    #[test]
    fn must_through_a_must_argument() {
        let mut id = method("A::id");
        id.params = 1;
        id.returns
            .push(read_must(2, "$v", &["p:A::id#0"], &["p:A::id#0"]));
        let mut run = method("A::run");
        run.args
            .push(arg_must(10, "A::id", 0, &["key:id"], &["key:id"]));
        run.reads
            .push(read_must(10, "$x", &["c:A::id@10"], &["c:A::id@10"]));
        let index = Index::build(&[file("a.php", vec![id, run])]);

        let traces = index.traces("id");
        assert_eq!(traces.len(), 1, "{traces:#?}");
        assert!(traces[0].must);
    }

    /// Интерфейс: возвраты реализаций объединяются (may), гарантии нет (`must` = false).
    #[test]
    fn interface_dispatch_unions_implementations() {
        let mut first = method("AImpl::get");
        first
            .returns
            .push(read(2, "$row['name_en']", &["key:name_en"]));
        let mut second = method("BImpl::get");
        second.returns.push(read(2, "$row['id']", &["key:id"]));
        let mut run = method("Svc::run");
        run.reads.push(read(9, "$i->get()", &["c:IFace::get"]));
        let mut parents = HashMap::new();
        parents.insert("AImpl".to_string(), vec!["IFace".to_string()]);
        parents.insert("BImpl".to_string(), vec!["IFace".to_string()]);
        let index = Index::build_with_parents(&[file("a.php", vec![first, second, run])], &parents);

        let traces = index.traces("name_en");
        assert_eq!(traces.len(), 1, "{traces:#?}");
        assert!(!traces[0].must);
        assert!(has(&traces[0], "c:IFace::get") && has(&traces[0], "key:name_en"));
    }

    /// Трейт: метод резолвится в реализацию потомка.
    #[test]
    fn trait_method_resolves_through_descendants() {
        let mut label = method("Item::label");
        label
            .returns
            .push(read(3, "$this->label", &["key:name_en"]));
        let mut run = method("Svc::run");
        run.reads
            .push(read(9, "$x->label()", &["c:HasName::label"]));
        let mut parents = HashMap::new();
        parents.insert("Item".to_string(), vec!["HasName".to_string()]);
        let index = Index::build_with_parents(&[file("a.php", vec![label, run])], &parents);

        let traces = index.traces("name_en");
        assert_eq!(traces.len(), 1, "{traces:#?}");
        assert!(has(&traces[0], "c:HasName::label"));
    }

    /// Property всегда пишется с колонкой — чтение через геттер становится `must`.
    #[test]
    fn property_must_through_a_getter() {
        let mut ctor = method("Item::__construct");
        ctor.container = Some("Item".into());
        ctor.assigns.push(assign_must(
            3,
            "prop:this::name",
            &["key:name_en"],
            &["key:name_en"],
        ));
        let mut getter = method("Item::getName");
        getter.container = Some("Item".into());
        getter.returns.push(read_must(
            7,
            "$this->name",
            &["prop:this::name"],
            &["prop:this::name"],
        ));
        let mut run = method("A::run");
        run.reads.push(read_must(
            11,
            "$item->getName()",
            &["c:Item::getName"],
            &["c:Item::getName"],
        ));
        let index = Index::build(&[file("a.php", vec![ctor, getter, run])]);

        let traces = index.traces("name_en");
        assert_eq!(traces.len(), 1, "{traces:#?}");
        assert!(traces[0].must);
    }

    fn mapping_index() -> Index {
        let mut init = method("Item::init");
        init.container = Some("Item".into());
        init.assigns
            .push(assign(4, "prop:this::name", &["unknown"]));
        let mut getter = method("Item::getName");
        getter.container = Some("Item".into());
        getter
            .returns
            .push(read(7, "$this->name", &["prop:this::name"]));
        let mut run = method("A::run");
        run.reads
            .push(read(11, "$item->getName()", &["c:Item::getName"]));
        let mut mapping = crate::flow::Mapping::default();
        mapping.insert("Item", "name", "items", "name_en", "test");
        Index::build(&[file("a.php", vec![init, getter, run])]).with_mapping(&mapping)
    }

    /// Сопоставление добавляет терминальный `sql:<table>.<column>` к раскрытию property.
    #[test]
    fn mapping_adds_the_sql_atom() {
        let index = mapping_index();
        let traces = index.traces("items.name_en");
        assert_eq!(traces.len(), 1, "{traces:#?}");
        assert_eq!(traces[0].column, "sql:items.name_en");
        assert!(has(&traces[0], "sql:items.name_en"));
    }

    /// Квалифицированный терм `<table>.<column>` сравнивается без учёта регистра;
    /// без таблицы — совпадение по имени колонки.
    #[test]
    fn table_qualified_term_matches_sql_atom() {
        let index = mapping_index();
        assert_eq!(index.traces("ITEMS.NAME_EN").len(), 1);
        assert!(index.traces("other.name_en").is_empty());
        let traces = index.traces("name_en");
        assert_eq!(traces.len(), 1, "{traces:#?}");
        assert_eq!(traces[0].column, "sql:items.name_en");
    }
}
