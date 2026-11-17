//! Проектный индекс потока данных: фикспойнт по `p:` / `c:` / `prop:` и поиск цепочек.
//!
//! Индекс строится по локальным фактам `super::model::FlowFile`: возвраты методов, аргументы вызовов,
//! присваивания и чтения. Истоки раскрываются до неподвижной точки, `traces` ищет чтения по имени
//! ключа/колонки/property и восстанавливает цепочку атомов до чтения.
//!
//! Результат вызова с call-site (`c:<method>@<line>`) при сборке квалифицируется в
//! `c:<method>@<file>#<line>`; такой атом раскрывает `p:<method>#<i>` аргументами именно своего вызова.
//! Голый `c:<method>` без `@` остаётся и раскрывается глобально (обратная совместимость).

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

/// Чтение с контекстом файла и метода.
#[derive(Debug, Clone)]
struct ReadFact {
    file: String,
    method: String,
    line: u32,
    expr: String,
    origins: Vec<String>,
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
    /// Истоки присваиваний по `lhs` (для property — `prop:<class>::<p>`).
    assigns: HashMap<String, BTreeSet<String>>,
    /// Все чтения проекта.
    reads: Vec<ReadFact>,
    /// Прямые родители: класс → полные имена родителей.
    parents: HashMap<String, Vec<String>>,
    /// Транзитивное раскрытие каждого встреченного атома.
    expanded: HashMap<String, BTreeSet<String>>,
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
        for file in files {
            for method in &file.methods {
                let returns = index.returns.entry(method.id.clone()).or_default();
                for ret in &method.returns {
                    returns.extend(ret.origins.iter().map(|origin| qualify(origin, &file.file)));
                }
                for assign in &method.assigns {
                    if assign.lhs.starts_with("prop:") {
                        index.assigns.entry(assign.lhs.clone()).or_default().extend(
                            assign
                                .origins
                                .iter()
                                .map(|origin| qualify(origin, &file.file)),
                        );
                    }
                }
                for arg in &method.args {
                    let site = format!("{}#{}", file.file, arg.line);
                    let origins: Vec<String> = arg
                        .origins
                        .iter()
                        .map(|origin| qualify(origin, &file.file))
                        .collect();
                    index
                        .args
                        .entry((arg.target.clone(), arg.index))
                        .or_default()
                        .extend(origins.iter().cloned());
                    index
                        .site_args
                        .entry((arg.target.clone(), site, arg.index))
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
                            .map(|origin| qualify(origin, &file.file))
                            .collect(),
                    });
                }
            }
        }
        index.fixpoint();
        index
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
            let column = if matched.starts_with("key:") || matched.starts_with("prop:") {
                matched.clone()
            } else {
                term.clone()
            };
            traces.push(Trace {
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

    /// Непосредственные потомки атома по правилам раскрытия. Терминальные атомы дают пустое множество.
    fn direct(&self, atom: &str) -> BTreeSet<String> {
        if atom.starts_with("prop:") {
            return self.assigns.get(atom).cloned().unwrap_or_default();
        }
        if let Some(rest) = atom.strip_prefix("c:") {
            let (target, site) = split_call(rest);
            let Some(origins) = self.returns_for_target(target) else {
                return BTreeSet::new();
            };
            let Some(site) = site else {
                // Голый `c:<method>`: текущее глобальное раскрытие.
                return origins.clone();
            };
            let mut set = BTreeSet::new();
            for origin in origins {
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
        if let Some(rest) = atom.strip_prefix("p:")
            && let Some((target, index)) = rest.rsplit_once('#')
            && let Ok(index) = index.parse::<usize>()
            && let Some(set) = self.args_for(target, index)
        {
            return set.clone();
        }
        BTreeSet::new()
    }

    /// Истоки возвратов по цели вызова: `Класс::метод` с наследованием, иначе точный id (функция).
    fn returns_for_target(&self, target: &str) -> Option<&BTreeSet<String>> {
        match target.rsplit_once("::") {
            Some((class, method)) => self.returns_for(class, method),
            None => self.returns.get(target),
        }
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

    /// Полное множество истоков чтения: сами истоки плюс их раскрытие.
    fn read_origins(&self, read: &ReadFact) -> BTreeSet<String> {
        let mut set = BTreeSet::new();
        for origin in &read.origins {
            set.insert(origin.clone());
            if let Some(inner) = self.expanded.get(origin) {
                set.extend(inner.iter().cloned());
            }
        }
        set
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

/// Квалификация call-site атома: `c:<method>@<line>` → `c:<method>@<file>#<line>`.
/// Атом без `@`, без номера строки или уже с `#` после `@` не меняется.
fn qualify(origin: &str, file: &str) -> String {
    let Some(rest) = origin.strip_prefix("c:") else {
        return origin.to_string();
    };
    let Some((method, line)) = rest.split_once('@') else {
        return origin.to_string();
    };
    if method.is_empty()
        || line.is_empty()
        || line.contains('#')
        || !line.chars().all(|ch| ch.is_ascii_digit())
    {
        return origin.to_string();
    }
    format!("c:{method}@{file}#{line}")
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

/// Атом для выдачи: call-site суффикс скрывается, цепочка остаётся читаемой.
fn display_atom(atom: &str) -> String {
    call_base(atom).to_string()
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
    let mut prop = None;
    let mut bare = None;
    for atom in set {
        if atom == "lit" || atom == "unknown" {
            continue;
        }
        if atom == key_atom {
            return Some(atom.clone());
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
    let atom = call_base(atom);
    let after_scope = atom.rsplit("::").next().unwrap_or(atom);
    after_scope.rsplit('.').next().unwrap_or(after_scope)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::flow::model::{ArgEdge, Assign, MethodFlow, Read};

    fn read(line: u32, expr: &str, origins: &[&str]) -> Read {
        Read {
            line,
            expr: expr.into(),
            origins: origins.iter().map(|origin| (*origin).to_string()).collect(),
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
        }
    }

    fn arg(line: u32, target: &str, index: usize, origins: &[&str]) -> ArgEdge {
        ArgEdge {
            line,
            target: target.into(),
            index,
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
}
