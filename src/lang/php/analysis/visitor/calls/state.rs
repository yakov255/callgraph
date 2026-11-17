use super::super::context::AnonFrames;
use crate::lang::php::{
    analysis::{NameResolver, type_tracker::TypeMap},
    categorize::{CallInfo, CategoryChain},
    framework::{FrameworkStrategy, GenericStrategy},
};
use crate::model::Call;
use std::{collections::HashMap, collections::HashSet, rc::Rc};

/// Статически известная цель ссылки на callable (`'strlen'`, `'Foo::bar'`, `[$this,'m']`, `Foo::m(...)`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) struct CallableTarget {
    /// Вид при вызове: `func_call` / `method_call` / `static_call`.
    pub(super) kind: &'static str,
    pub(super) target: String,
    pub(super) resolved: Option<String>,
}

pub(super) struct State {
    pub(super) container: Option<String>,
    pub(super) caller: Option<String>,
    pub(super) closure_depth: usize,
    pub(super) anon: AnonFrames,
    pub(super) resolver: NameResolver,
    pub(super) tracker: TypeMap,
    pub(super) imports: HashSet<String>,
    pub(super) framework: Rc<dyn FrameworkStrategy>,
    pub(super) chain: CategoryChain,
    pub(super) calls: Vec<Call>,
    /// Вызываемые переменные функции (`$f`) → цель ссылки. `None`: известный callable без цели
    /// (closure-литерал, параметр `Closure` / `callable`) — `$var()` тогда не динамический, но ребра нет.
    pub(super) callables: HashMap<String, Option<CallableTarget>>,
    /// Callable-property уровня класса (`$this->cb = …`) → цель ссылки; переживает переход между методами.
    pub(super) callable_props: HashMap<String, Option<CallableTarget>>,
}

impl Default for State {
    fn default() -> Self {
        Self {
            container: None,
            caller: None,
            closure_depth: 0,
            anon: AnonFrames::default(),
            resolver: NameResolver::default(),
            tracker: TypeMap::default(),
            imports: HashSet::new(),
            framework: Rc::new(GenericStrategy),
            chain: CategoryChain::with_defaults(),
            calls: Vec::new(),
            callables: HashMap::new(),
            callable_props: HashMap::new(),
        }
    }
}

pub(super) fn categorize(call: &mut Call, state: &State) {
    let short = call
        .target
        .split("::")
        .next()
        .filter(|x| x.contains('\\') || x.chars().next().is_some_and(char::is_uppercase));
    let info = CallInfo {
        kind: &call.kind,
        target: &call.target,
        on: call.on.as_deref(),
        resolved: call.resolved.as_deref(),
        class_short_name: short,
        has_use_import: short.is_some_and(|x| state.imports.contains(x)),
        is_this: call.on.as_deref() == Some("this"),
        framework: state.framework.as_ref(),
    };
    call.category = state.chain.categorize(&info).into();
}
