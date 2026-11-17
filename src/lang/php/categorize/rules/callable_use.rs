//! Статически известная ссылка на callable (`'strlen'`, `[$this, 'm']`, `Foo::m(...)`): не вызов, а ребро
//! к указанной цели — `--grep` / `--depth` видят, где метод передают как callable.
use crate::lang::php::categorize::{CallInfo, CategoryRule};

/// Категория ребра типа `callable_ref` — `callable_use` (в начале цепочки: другие правила её не перекрывают).
pub struct CallableUseRule;

impl CategoryRule for CallableUseRule {
    fn categorize(&self, call: &CallInfo<'_>) -> Option<&'static str> {
        (call.kind == "callable_ref").then_some("callable_use")
    }
}
