//! Ссылки на callable: `'strlen'`, `'Foo::bar'`, `[$this, 'm']`, `[Foo::class, 'm']`,
//! `Closure::fromCallable(...)`, `foo(...)`, `$this->m(...)`, `Foo::m(...)`.
//!
//! Статически известная ссылка — ребро `callable_ref` (категория `callable_use`) от вызывающего метода к цели:
//! `--grep` / `--depth` видят, где метод передают как callable. Ссылку, положенную в переменную или property,
//! помнят `State.callables` / `State.callable_props`: последующий `$f()` / `($this->cb)()` разрешается в реальный
//! вызов, а не в `dynamic_call`. Closure-литерал и параметр `Closure` / `callable` по-прежнему без ребра (нет
//! статической цели), но и не динамические.

use super::{
    helpers::Collector,
    state::{CallableTarget, State},
};
use crate::lang::php::analysis::type_tracker::TypeTracker;
use crate::lang::php::printer::lossy;
use mago_span::HasSpan;
use mago_syntax::cst::Call as AstCall;
use mago_syntax::cst::*;

/// Встроенные функции, у которых обычный строковый литерал-аргумент — callable (`array_map('trim', …)`).
/// В остальных местах строка остаётся обычным значением: `assertSame('x', $y)` — не ссылка на callable.
const HIGHER_ORDER: [&str; 19] = [
    "array_filter",
    "array_map",
    "array_reduce",
    "array_walk",
    "array_walk_recursive",
    "call_user_func",
    "call_user_func_array",
    "forward_static_call",
    "forward_static_call_array",
    "iterator_apply",
    "preg_replace_callback",
    "preg_replace_callback_array",
    "register_shutdown_function",
    "set_error_handler",
    "set_exception_handler",
    "spl_autoload_register",
    "uasort",
    "uksort",
    "usort",
];

/// Имя, похожее на идентификатор (`strlen`, `App\helpers\run`, `bar`).
fn is_identifier(value: &str) -> bool {
    !value.is_empty()
        && value
            .chars()
            .all(|c| c.is_alphanumeric() || c == '_' || c == '\\')
        && !value.starts_with('\\')
        && !value.ends_with('\\')
}

/// Выражение элемента массива (части `[$this, 'm']`).
fn element_expr<'e>(element: &'e ArrayElement<'e>) -> Option<&'e Expression<'e>> {
    match element {
        ArrayElement::Value(v) => Some(v.value),
        ArrayElement::KeyValue(v) => Some(v.value),
        _ => None,
    }
}

impl Collector<'_, '_> {
    /// Статически известная цель callable для выражения; closure / arrow function / неизвестная форма: `None`.
    pub(super) fn callable_target(&self, s: &State, e: &Expression<'_>) -> Option<CallableTarget> {
        match super::strip(e) {
            Expression::Literal(Literal::String(lit)) => {
                let value = String::from_utf8_lossy(lit.value?).into_owned();
                self.string_callable(s, &value)
            }
            Expression::Array(a) => {
                let elements: Vec<&ArrayElement<'_>> = a.elements.iter().collect();
                self.array_callable(s, &elements)
            }
            Expression::LegacyArray(a) => {
                let elements: Vec<&ArrayElement<'_>> = a.elements.iter().collect();
                self.array_callable(s, &elements)
            }
            Expression::Call(AstCall::StaticMethod(c)) => self.closure_from_callable(s, c),
            Expression::PartialApplication(p) => self.partial_callable(s, p),
            _ => None,
        }
    }

    /// `'strlen'` / `'Foo::bar'`.
    fn string_callable(&self, s: &State, value: &str) -> Option<CallableTarget> {
        let value = value.trim();
        if let Some((class, method)) = value.rsplit_once("::") {
            let method = method.trim();
            if class.trim().is_empty() || !is_identifier(method) {
                return None;
            }
            return Some(CallableTarget {
                kind: "static_call",
                target: method.to_owned(),
                resolved: Some(format!(
                    "{}::{method}",
                    s.resolver.resolve_class(class.trim())
                )),
            });
        }
        is_identifier(value).then(|| CallableTarget {
            kind: "func_call",
            target: value.to_owned(),
            resolved: Some(value.to_owned()),
        })
    }

    /// `[$this, 'm']`, `[Foo::class, 'm']`, `[$obj, 'm']`, `['Foo', 'm']`.
    fn array_callable(&self, s: &State, elements: &[&ArrayElement<'_>]) -> Option<CallableTarget> {
        // PHP callable-массив: ровно два элемента — получатель и имя метода
        if elements.len() != 2 {
            return None;
        }
        let first = element_expr(elements.first()?).map(super::strip)?;
        let method = element_expr(elements.last()?).map(super::strip)?;
        let Expression::Literal(Literal::String(lit)) = method else {
            return None;
        };
        let method = String::from_utf8_lossy(lit.value?).into_owned();
        if !is_identifier(&method) {
            return None;
        }
        match first {
            Expression::Variable(Variable::Direct(v)) if v.name == b"$this" => {
                let class = s
                    .container
                    .as_ref()
                    .map(|_| s.resolver.resolve_class("self"));
                Some(CallableTarget {
                    kind: "method_call",
                    target: method.clone(),
                    resolved: class.map(|c| format!("{c}::{method}")),
                })
            }
            Expression::Variable(Variable::Direct(v)) => {
                let ty = s.tracker.type_of(&lossy(v.name))?;
                Some(CallableTarget {
                    kind: "method_call",
                    target: method.clone(),
                    resolved: Some(format!("{ty}::{method}")),
                })
            }
            Expression::Access(Access::Property(p)) => {
                let ty = self.property_type(s, p)?;
                Some(CallableTarget {
                    kind: "method_call",
                    target: method.clone(),
                    resolved: Some(format!("{ty}::{method}")),
                })
            }
            Expression::Access(Access::ClassConstant(c)) => {
                let class = self.class(c.class)?;
                Some(CallableTarget {
                    kind: "static_call",
                    target: method.clone(),
                    resolved: Some(format!("{}::{method}", s.resolver.resolve_class(&class))),
                })
            }
            // `['Foo', 'm']` намеренно нет: обычный массив из двух строк выглядит так же
            _ => None,
        }
    }

    /// `Closure::fromCallable([$this, 'm'])`.
    fn closure_from_callable(&self, s: &State, c: &StaticMethodCall<'_>) -> Option<CallableTarget> {
        let class = self.class(c.class)?;
        let ClassLikeMemberSelector::Identifier(m) = &c.method else {
            return None;
        };
        if !class.eq_ignore_ascii_case("Closure") || !m.value.eq_ignore_ascii_case(b"fromCallable")
        {
            return None;
        }
        let first = c.argument_list.arguments.iter().next()?;
        let expr = match first {
            Argument::Positional(a) => a.value,
            Argument::Named(a) => a.value,
        };
        self.callable_target(s, expr)
    }

    /// Callable первого класса: `foo(...)`, `$this->m(...)`, `Foo::m(...)`.
    pub(super) fn partial_callable(
        &self,
        s: &State,
        p: &PartialApplication<'_>,
    ) -> Option<CallableTarget> {
        match p {
            PartialApplication::Function(f) => {
                let Expression::Identifier(i) = super::strip(f.function) else {
                    return None;
                };
                let name = lossy(i.value()).trim_start_matches('\\').to_owned();
                Some(CallableTarget {
                    kind: "func_call",
                    target: name.clone(),
                    resolved: Some(name),
                })
            }
            PartialApplication::Method(m) => {
                let ClassLikeMemberSelector::Identifier(i) = &m.method else {
                    return None;
                };
                let method = lossy(i.value);
                let class = self.receiver_class(s, m.object)?;
                Some(CallableTarget {
                    kind: "method_call",
                    target: method.clone(),
                    resolved: Some(format!("{class}::{method}")),
                })
            }
            PartialApplication::StaticMethod(m) => {
                let ClassLikeMemberSelector::Identifier(i) = &m.method else {
                    return None;
                };
                let class = self.class(m.class)?;
                let method = lossy(i.value);
                Some(CallableTarget {
                    kind: "static_call",
                    target: method.clone(),
                    resolved: Some(format!("{}::{method}", s.resolver.resolve_class(&class))),
                })
            }
        }
    }

    /// Класс получателя вызова (`$this` → текущий класс, переменная / property → отслеживаемый тип).
    fn receiver_class(&self, s: &State, e: &Expression<'_>) -> Option<String> {
        if matches!(super::strip(e), Expression::Variable(Variable::Direct(v)) if v.name == b"$this")
            && s.container.is_some()
        {
            return Some(s.resolver.resolve_class("self"));
        }
        self.set_receiver_type(s, e)
    }

    /// Тип `$this->prop` из type tracker.
    fn property_type(&self, s: &State, p: &PropertyAccess<'_>) -> Option<String> {
        let (Expression::Variable(Variable::Direct(v)), ClassLikeMemberSelector::Identifier(i)) =
            (super::strip(p.object), &p.property)
        else {
            return None;
        };
        if v.name != b"$this" {
            return None;
        }
        let name = lossy(i.value);
        s.tracker
            .type_of(&format!("this->{name}"))
            .or_else(|| s.tracker.type_of(name.trim_start_matches('$')))
    }

    /// Добавить ссылку как ребро (от вызывающего метода к цели).
    pub(super) fn push_callable_ref(&self, s: &mut State, n: &impl HasSpan, t: &CallableTarget) {
        let x = self.push(s, n, "callable_ref", t.target.clone());
        s.calls[x].resolved = t.resolved.clone();
    }

    /// Добавить ссылки на callable из аргументов вызова. Строка считается callable только у известной
    /// higher-order функции (`string_ok`); массив / `Closure::fromCallable` / callable первого класса — всегда.
    pub(super) fn callable_args(&self, s: &mut State, list: &ArgumentList<'_>, string_ok: bool) {
        for a in list.arguments.iter() {
            let expr = match a {
                Argument::Positional(x) => x.value,
                Argument::Named(x) => x.value,
            };
            // callable первого класса walker уже записывает как `method_call` — не дублируем
            if matches!(super::strip(expr), Expression::PartialApplication(_)) {
                continue;
            }
            if !string_ok && matches!(super::strip(expr), Expression::Literal(Literal::String(_))) {
                continue;
            }
            if let Some(t) = self.callable_target(s, expr) {
                self.push_callable_ref(s, a, &t);
            }
        }
    }

    /// `array_map(...)` / `usort(...)`: встроенная higher-order функция?
    pub(super) fn is_higher_order(name: &str) -> bool {
        let name = name.trim_start_matches('\\');
        HIGHER_ORDER.iter().any(|f| f.eq_ignore_ascii_case(name))
    }

    /// `$f = …` / `$this->cb = …`: запомнить ссылку на callable (или удалить, если значение не callable).
    pub(super) fn register_callable_assignment(
        &self,
        s: &mut State,
        lhs: &Expression<'_>,
        rhs: &Expression<'_>,
    ) {
        let key = match super::strip(lhs) {
            Expression::Variable(Variable::Direct(v)) => lossy(v.name),
            Expression::Access(Access::Property(p)) => {
                match (super::strip(p.object), &p.property) {
                    (
                        Expression::Variable(Variable::Direct(v)),
                        ClassLikeMemberSelector::Identifier(i),
                    ) if v.name == b"$this" => format!("this->{}", lossy(i.value)),
                    _ => return,
                }
            }
            _ => return,
        };
        let anonymous = matches!(
            super::strip(rhs),
            Expression::Closure(_) | Expression::ArrowFunction(_)
        );
        // `$status = 'active'`: обычная строка не callable — только известная нативная функция, имя с
        // пространством имён или форма `Класс::метод`. (В позиции аргумента higher-order функции callable — любое имя.)
        if let Expression::Literal(Literal::String(lit)) = super::strip(rhs) {
            let value = String::from_utf8_lossy(lit.value.unwrap_or_default()).into_owned();
            let value = value.trim();
            if !value.contains("::")
                && !value.contains('\\')
                && !crate::lang::php::categorize::data::is_native_php(value)
            {
                let map = if key.starts_with("this->") {
                    &mut s.callable_props
                } else {
                    &mut s.callables
                };
                map.remove(&key);
                return;
            }
        }
        // переменная → переменная / property: ранее известная цель callable наследуется
        let inherited = match super::strip(rhs) {
            Expression::Variable(Variable::Direct(v)) => s.callables.get(&lossy(v.name)).cloned(),
            _ => None,
        };
        let target = match &inherited {
            Some(entry) => entry.clone(),
            None => self.callable_target(s, rhs),
        };
        if !anonymous && target.is_none() && inherited.is_none() {
            // значение не callable: удаляем прежнюю (устаревшую) запись
            let map = if key.starts_with("this->") {
                &mut s.callable_props
            } else {
                &mut s.callables
            };
            map.remove(&key);
            return;
        }
        if let Some(target) = &target {
            self.push_callable_ref(s, rhs, target);
        }
        let map = if key.starts_with("this->") {
            &mut s.callable_props
        } else {
            &mut s.callables
        };
        map.insert(key, target);
    }
}
