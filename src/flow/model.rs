//! Контракт потока данных: факты по файлу/методу и одна цепочка «источник → чтение».
//!
//! Атомы истоков — строки с префиксами: `p:<method>#<i>` (параметр), `c:<method>` (результат вызова),
//! `prop:<class>::<p>` (property), `key:<name>` (ключ массива / колонка), `new:<class>`, `lit`, `unknown`.
//! Формат фиксирован: его читают `local` (сбор), `index` (фикспойнт) и `trace` (вывод).
//!
//! Результат вызова несёт call-site: `c:<method>@<line>` (1-based строка вызова). Индекс квалифицирует его
//! в `c:<method>@<file>#<line>` и раскрывает `p:<method>#<i>` аргументами именно этого вызова; голый
//! `c:<method>` (без `@`) остаётся для обратной совместимости и раскрывается глобально.
//!
//! Создание объекта — `new:<class>@<line>` (индекс: `new:<class>@<file>#<line>`), чтобы свойство можно было
//! привязать к месту создания. `$this->p` — `prop:this::<p>` (индекс: `prop:this:<class>::<p>`); получатель
//! вызова записывается в `MethodFlow.receivers` и раскрывает такое свойство для конкретного объекта.

/// Типы возврата методов: `Class::method` → класс. Для `array` — тип элемента из `@return Foo[]`.
pub type MethodTypes = std::collections::HashMap<String, String>;

/// Факты одного файла.
#[derive(Debug, Clone, PartialEq, Default)]
pub struct FlowFile {
    pub file: String,
    pub methods: Vec<MethodFlow>,
}

/// Факты одного метода (или функции).
#[derive(Debug, Clone, PartialEq, Default)]
pub struct MethodFlow {
    /// `Class::method`, `function` или `__top__`.
    pub id: String,
    pub container: Option<String>,
    /// Число параметров (для `p:<id>#<i>`).
    pub params: usize,
    pub assigns: Vec<Assign>,
    pub reads: Vec<Read>,
    pub returns: Vec<Read>,
    pub args: Vec<ArgEdge>,
    pub receivers: Vec<ReceiverEdge>,
}

/// Присваивание: `lhs` получает истоки `origins` (ребро, в выдачу не идёт).
#[derive(Debug, Clone, PartialEq)]
pub struct Assign {
    pub line: u32,
    /// `$x`, `this->p`, `Class::p` или `$x['k']`.
    pub lhs: String,
    pub origins: Vec<String>,
    /// `must`-множество значения (см. `Read::must`).
    pub must: Vec<String>,
}

/// Чтение значения (условие, property, ключ массива) — узел выдачи.
#[derive(Debug, Clone, PartialEq)]
pub struct Read {
    pub line: u32,
    pub expr: String,
    /// `may`: истоки, которые достигают чтения хотя бы на одном пути.
    pub origins: Vec<String>,
    /// `must`: истоки, которые достигают чтения на каждом достижимом пути; `must ⊆ origins`.
    /// Пусто — анализ не даёт гарантии (консервативно).
    pub must: Vec<String>,
}

/// Получатель вызова метода: истоки объекта (для привязки `prop:this::` к месту создания).
#[derive(Debug, Clone, PartialEq)]
pub struct ReceiverEdge {
    pub line: u32,
    pub target: String,
    pub origins: Vec<String>,
}

/// Аргумент вызова: `index`-й аргумент вызова `target` получает истоки.
#[derive(Debug, Clone, PartialEq)]
pub struct ArgEdge {
    pub line: u32,
    pub target: String,
    pub index: usize,
    pub origins: Vec<String>,
    /// `must`-множество аргумента (см. `Read::must`).
    pub must: Vec<String>,
}

/// Одна цепочка от источника до чтения.
#[derive(Debug, Clone, PartialEq)]
pub struct Trace {
    /// Совпавший ключ/колонка (`service_type`).
    pub column: String,
    /// `true`: колонка доходит на каждом пути; `false`: только may.
    pub must: bool,
    /// Цепочка атомов от источника к чтению.
    pub chain: Vec<String>,
    pub file: String,
    pub method: String,
    pub line: u32,
    pub expr: String,
}
