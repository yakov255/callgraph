//! Разбор типов из сигнатуры: именованные типы и их позиции в строке.
//!
//! Нам нужны только имена классов: `?Foo`, `Foo|Bar`, `list<Foo>`, `array{a: Foo}`,
//! `callable(Foo): Bar`, `Foo[]`. Псевдо-типы PHPStan (`list`, `non-empty-array`,
//! `class-string`, `positive-int`) и скаляры пропускаются: они не ведут в класс.

/// Имена, которые не являются классами (или не ведут в исходник).
const BUILTINS: &[&str] = &[
    "array",
    "bool",
    "boolean",
    "callable",
    "closed-resource",
    "double",
    "false",
    "float",
    "int",
    "integer",
    "iterable",
    "list",
    "mixed",
    "never",
    "null",
    "numeric",
    "numeric-string",
    "object",
    "parent",
    "resource",
    "self",
    "static",
    "string",
    "true",
    "void",
    "__string",
];

/// Именованный тип: байтовые границы внутри строки типа и само имя (с ведущим `\`).
pub(crate) fn named(text: &str) -> Vec<(usize, usize, String)> {
    let bytes = text.as_bytes();
    let mut out = Vec::new();
    let mut i = 0;
    while i < bytes.len() {
        if !is_ident_start(bytes[i]) {
            i += 1;
            continue;
        }
        let start = i;
        while i < bytes.len() && is_ident(bytes[i]) {
            i += 1;
        }
        let name = &text[start..i];
        let bare = name.trim_start_matches('\\');
        let lower = bare.to_ascii_lowercase();
        if bare.is_empty()
            || BUILTINS.contains(&lower.as_str())
            || lower.starts_with("non-empty-")
            || lower.starts_with("class-string")
            || lower.starts_with("positive-")
            || lower.starts_with("negative-")
            || lower.starts_with("key-of")
            || lower.starts_with("value-of")
            || lower.starts_with("int-mask")
        {
            continue;
        }
        // Псевдо-типы и функции пишутся с маленькой буквы; классы — с заглавной.
        if bare.starts_with(|c: char| c.is_ascii_lowercase()) {
            continue;
        }
        // `shapeKey:` — ключ массива, а не тип; `Foo::class` — выражение, не тип.
        if next_non_space(bytes, i) == Some(b':') || next_non_space(bytes, i) == Some(b'\\') {
            continue;
        }
        out.push((start, i, name.to_owned()));
    }
    out
}

fn is_ident_start(byte: u8) -> bool {
    byte.is_ascii_alphabetic() || byte == b'_' || byte == b'\\'
}

fn is_ident(byte: u8) -> bool {
    byte.is_ascii_alphanumeric() || byte == b'_' || byte == b'\\'
}

fn next_non_space(bytes: &[u8], from: usize) -> Option<u8> {
    bytes[from..]
        .iter()
        .copied()
        .find(|byte| !byte.is_ascii_whitespace())
}

#[cfg(test)]
mod tests {
    use super::named;

    fn names(text: &str) -> Vec<String> {
        named(text)
            .into_iter()
            .map(|(_, _, name)| name.trim_start_matches('\\').to_owned())
            .collect()
    }

    #[test]
    fn nullable_and_union() {
        assert_eq!(names("?ErrorLogEntity"), ["ErrorLogEntity"]);
        assert_eq!(names("Foo|Bar|null"), ["Foo", "Bar"]);
    }

    #[test]
    fn generics_and_shapes() {
        assert_eq!(names("list<Order>"), ["Order"]);
        assert_eq!(
            names("array{hotel: Hotel, rows: list<Row>}"),
            ["Hotel", "Row"]
        );
        assert_eq!(names("non-empty-list<Passenger>"), ["Passenger"]);
    }

    #[test]
    fn callable_signature() {
        assert_eq!(names("callable(Foo): Bar"), ["Foo", "Bar"]);
        assert_eq!(
            names("Closure(Order): Receipt"),
            ["Closure", "Order", "Receipt"]
        );
    }

    #[test]
    fn builtins_and_variables_are_skipped() {
        assert!(names("int|string|mixed").is_empty());
        assert!(names("$foo") == Vec::<String>::new());
        assert_eq!(names("Foo[]"), ["Foo"]);
    }
}
