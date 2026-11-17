//! Литеральные значения из значений property по умолчанию (`$tableName = 'x'`, `$fieldsMap = ['a' => 'b']`).

use mago_syntax::cst::*;

use crate::lang::php::parser::ParsedFile;

/// Статически известный литерал (плагины строят из него имя таблицы / колонки).
#[derive(Debug, Clone, PartialEq, serde::Serialize)]
#[serde(untagged)]
pub enum Value {
    Str(String),
    Int(String),
    Bool(bool),
    Null,
    Array(Vec<(Option<Value>, Value)>),
}

impl Value {
    pub fn as_str(&self) -> Option<&str> {
        match self {
            Value::Str(s) => Some(s),
            _ => None,
        }
    }

    pub fn as_array(&self) -> Option<&[(Option<Value>, Value)]> {
        match self {
            Value::Array(items) => Some(items),
            _ => None,
        }
    }
}

/// Литеральное значение выражения; не литерал (переменная, вызов, константа): `None`.
pub fn value_of(parsed: &ParsedFile<'_>, expression: &Expression<'_>) -> Option<Value> {
    match expression.unparenthesized() {
        Expression::Literal(Literal::String(s)) => Some(Value::Str(
            String::from_utf8_lossy(s.value.unwrap_or(s.raw)).into_owned(),
        )),
        Expression::Literal(Literal::Integer(i)) => {
            Some(Value::Int(String::from_utf8_lossy(i.raw).into_owned()))
        }
        Expression::Literal(Literal::True(_)) => Some(Value::Bool(true)),
        Expression::Literal(Literal::False(_)) => Some(Value::Bool(false)),
        Expression::Literal(Literal::Null(_)) => Some(Value::Null),
        Expression::Array(a) => array_of(parsed, &a.elements),
        Expression::LegacyArray(a) => array_of(parsed, &a.elements),
        // `'a' . 'b'` — только из литеральных частей
        Expression::Binary(b) if b.operator.is_concatenation() => {
            let left = value_of(parsed, b.lhs)?;
            let right = value_of(parsed, b.rhs)?;
            Some(Value::Str(format!("{}{}", left.as_str()?, right.as_str()?)))
        }
        _ => None,
    }
}

fn array_of(
    parsed: &ParsedFile<'_>,
    elements: &TokenSeparatedSequence<'_, ArrayElement<'_>>,
) -> Option<Value> {
    let mut out = Vec::new();
    for element in elements.iter() {
        match element {
            ArrayElement::Value(v) => out.push((None, value_of(parsed, v.value)?)),
            ArrayElement::KeyValue(kv) => {
                out.push((Some(value_of(parsed, kv.key)?), value_of(parsed, kv.value)?))
            }
            // `...$spread`, `[,]` — не статично
            _ => return None,
        }
    }
    Some(Value::Array(out))
}
