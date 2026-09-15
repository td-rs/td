//! Port of `td/utils/JsonBuilder.h` + `td/utils/JsonBuilder.cpp`.
//!
//! Byte-exact JSON: numbers keep their source text, strings escape control
//! characters and all non-ASCII as `\uXXXX` (C++ `JsonString`), compact mode
//! uses `:` separators and pretty mode ` : ` with three-space indentation,
//! and the decoder reproduces tdlib's error messages and `\u` surrogate
//! handling.

use crate::status::{Result, Status};
use std::fmt::Write as _;

// ---------------------------------------------------------------------------
// Value model
// ---------------------------------------------------------------------------

/// `td::JsonValueType`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum JsonValueType {
    Null,
    Number,
    Boolean,
    String,
    Array,
    Object,
}

impl JsonValueType {
    /// `td::JsonValue::get_type_name`.
    pub fn name(self) -> &'static str {
        match self {
            Self::Null => "Null",
            Self::Number => "Number",
            Self::Boolean => "Boolean",
            Self::String => "String",
            Self::Array => "Array",
            Self::Object => "Object",
        }
    }
}

impl std::fmt::Display for JsonValueType {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.name())
    }
}

/// `td::JsonObject`: an ordered list of named fields.
#[derive(Debug, Default)]
pub struct JsonObject {
    field_values: Vec<(String, JsonValue)>,
}

impl JsonObject {
    pub fn new() -> Self {
        Self::default()
    }

    /// `JsonObject(vector<pair<Slice, JsonValue>> &&)`.
    pub fn from_fields(field_values: Vec<(String, JsonValue)>) -> Self {
        Self { field_values }
    }

    pub fn field_count(&self) -> usize {
        self.field_values.len()
    }

    /// `JsonObject::get_field`.
    pub fn get_field(&self, name: &str) -> Option<&JsonValue> {
        self.field_values
            .iter()
            .find(|(field, _)| field == name)
            .map(|(_, value)| value)
    }

    /// `JsonObject::extract_field`: moves the field out, `Null` when absent
    /// (a successful extraction leaves `Null` behind, like the C++ move).
    pub fn extract_field(&mut self, name: &str) -> JsonValue {
        match self
            .field_values
            .iter_mut()
            .find(|(field, _)| field == name)
        {
            Some((_, value)) => std::mem::replace(value, JsonValue::Null),
            None => JsonValue::Null,
        }
    }

    /// `JsonObject::extract_optional_field`.
    pub fn extract_optional_field(&mut self, name: &str, ty: JsonValueType) -> Result<JsonValue> {
        if let Some((_, value)) = self
            .field_values
            .iter_mut()
            .find(|(field, _)| field == name)
        {
            if ty != JsonValueType::Null && value.ty() != ty {
                return Result::Err(Status::error(
                    400,
                    &format!("Field \"{name}\" must be of type {}", ty.name()),
                ));
            }
            return Result::Ok(std::mem::replace(value, JsonValue::Null));
        }
        Result::Ok(JsonValue::Null)
    }

    /// `JsonObject::extract_required_field`.
    pub fn extract_required_field(&mut self, name: &str, ty: JsonValueType) -> Result<JsonValue> {
        if let Some((_, value)) = self
            .field_values
            .iter_mut()
            .find(|(field, _)| field == name)
        {
            if ty != JsonValueType::Null && value.ty() != ty {
                return Result::Err(Status::error(
                    400,
                    &format!("Field \"{name}\" must be of type {}", ty.name()),
                ));
            }
            return Result::Ok(std::mem::replace(value, JsonValue::Null));
        }
        Result::Err(Status::error(400, &format!("Can't find field \"{name}\"")))
    }

    /// `JsonObject::has_field`.
    pub fn has_field(&self, name: &str) -> bool {
        self.get_field(name).is_some()
    }

    /// `JsonObject::get_optional_bool_field`.
    pub fn get_optional_bool_field(&self, name: &str, default_value: bool) -> Result<bool> {
        match self.get_field(name) {
            Some(JsonValue::Boolean(value)) => Result::Ok(*value),
            Some(_) => Result::Err(Status::error(
                400,
                &format!("Field \"{name}\" must be of type Boolean"),
            )),
            None => Result::Ok(default_value),
        }
    }

    /// `JsonObject::get_required_bool_field`.
    pub fn get_required_bool_field(&self, name: &str) -> Result<bool> {
        match self.get_field(name) {
            Some(JsonValue::Boolean(value)) => Result::Ok(*value),
            Some(_) => Result::Err(Status::error(
                400,
                &format!("Field \"{name}\" must be of type Boolean"),
            )),
            None => Result::Err(Status::error(400, &format!("Can't find field \"{name}\""))),
        }
    }

    fn get_integer_field(name: &str, value: &str, ty: fn(&str) -> Option<i64>) -> Result<i64> {
        match ty(value) {
            Some(integer) => Result::Ok(integer),
            None => Result::Err(Status::error(
                400,
                &format!("Field \"{name}\" must be a valid Number"),
            )),
        }
    }

    fn integer_field(&self, name: &str, ty: fn(&str) -> Option<i64>) -> Option<Result<i64>> {
        match self.get_field(name) {
            Some(JsonValue::String(value)) | Some(JsonValue::Number(value)) => {
                Some(Self::get_integer_field(name, value, ty))
            }
            Some(_) => Some(Result::Err(Status::error(
                400,
                &format!("Field \"{name}\" must be a Number"),
            ))),
            None => None,
        }
    }

    /// `JsonObject::get_optional_int_field`.
    pub fn get_optional_int_field(&self, name: &str, default_value: i32) -> Result<i32> {
        match self.integer_field(name, parse_int64) {
            Some(result) => match result {
                Result::Ok(value) => match i32::try_from(value) {
                    Ok(value) => Result::Ok(value),
                    Err(_) => Result::Err(Status::error(
                        400,
                        &format!("Field \"{name}\" must be a valid Number"),
                    )),
                },
                Result::Err(status) => Result::Err(status),
            },
            None => Result::Ok(default_value),
        }
    }

    /// `JsonObject::get_required_int_field`.
    pub fn get_required_int_field(&self, name: &str) -> Result<i32> {
        match self.integer_field(name, parse_int64) {
            Some(result) => match result {
                Result::Ok(value) => match i32::try_from(value) {
                    Ok(value) => Result::Ok(value),
                    Err(_) => Result::Err(Status::error(
                        400,
                        &format!("Field \"{name}\" must be a valid Number"),
                    )),
                },
                Result::Err(status) => Result::Err(status),
            },
            None => Result::Err(Status::error(400, &format!("Can't find field \"{name}\""))),
        }
    }

    /// `JsonObject::get_optional_long_field`.
    pub fn get_optional_long_field(&self, name: &str, default_value: i64) -> Result<i64> {
        match self.integer_field(name, parse_int64) {
            Some(result) => result,
            None => Result::Ok(default_value),
        }
    }

    /// `JsonObject::get_required_long_field`.
    pub fn get_required_long_field(&self, name: &str) -> Result<i64> {
        match self.integer_field(name, parse_int64) {
            Some(result) => result,
            None => Result::Err(Status::error(400, &format!("Can't find field \"{name}\""))),
        }
    }

    fn double_field(&self, name: &str) -> Option<Result<f64>> {
        match self.get_field(name) {
            Some(JsonValue::Number(value)) => Some(Result::Ok(to_double(value))),
            Some(_) => Some(Result::Err(Status::error(
                400,
                &format!("Field \"{name}\" must be of type Number"),
            ))),
            None => None,
        }
    }

    /// `JsonObject::get_optional_double_field`.
    pub fn get_optional_double_field(&self, name: &str, default_value: f64) -> Result<f64> {
        match self.double_field(name) {
            Some(result) => result,
            None => Result::Ok(default_value),
        }
    }

    /// `JsonObject::get_required_double_field`.
    pub fn get_required_double_field(&self, name: &str) -> Result<f64> {
        match self.double_field(name) {
            Some(result) => result,
            None => Result::Err(Status::error(400, &format!("Can't find field \"{name}\""))),
        }
    }

    fn string_field(&self, name: &str) -> Option<Result<String>> {
        match self.get_field(name) {
            Some(JsonValue::String(value)) => Some(Result::Ok(value.clone())),
            Some(JsonValue::Number(value)) => Some(Result::Ok(value.clone())),
            Some(_) => Some(Result::Err(Status::error(
                400,
                &format!("Field \"{name}\" must be of type String"),
            ))),
            None => None,
        }
    }

    /// `JsonObject::get_optional_string_field`.
    pub fn get_optional_string_field(&self, name: &str, default_value: &str) -> Result<String> {
        match self.string_field(name) {
            Some(result) => result,
            None => Result::Ok(default_value.to_string()),
        }
    }

    /// `JsonObject::get_required_string_field`.
    pub fn get_required_string_field(&self, name: &str) -> Result<String> {
        match self.string_field(name) {
            Some(result) => result,
            None => Result::Err(Status::error(400, &format!("Can't find field \"{name}\""))),
        }
    }

    /// `JsonObject::foreach`.
    pub fn foreach(&self, mut callback: impl FnMut(&str, &JsonValue)) {
        for (name, value) in &self.field_values {
            callback(name, value);
        }
    }
}

/// `td::JsonValue`: one JSON value. Numbers keep their source text (the C++
/// class stores the parsed slice and re-emits it verbatim).
#[derive(Debug)]
pub enum JsonValue {
    Null,
    Number(String),
    Boolean(bool),
    String(String),
    Array(Vec<JsonValue>),
    Object(JsonObject),
}

impl JsonValue {
    pub fn ty(&self) -> JsonValueType {
        match self {
            Self::Null => JsonValueType::Null,
            Self::Number(_) => JsonValueType::Number,
            Self::Boolean(_) => JsonValueType::Boolean,
            Self::String(_) => JsonValueType::String,
            Self::Array(_) => JsonValueType::Array,
            Self::Object(_) => JsonValueType::Object,
        }
    }

    pub fn get_number(&self) -> &str {
        match self {
            Self::Number(number) => number,
            _ => panic!("get_number on a non-Number JsonValue"),
        }
    }

    pub fn get_boolean(&self) -> bool {
        match self {
            Self::Boolean(value) => *value,
            _ => panic!("get_boolean on a non-Boolean JsonValue"),
        }
    }

    pub fn get_string(&self) -> &str {
        match self {
            Self::String(value) => value,
            _ => panic!("get_string on a non-String JsonValue"),
        }
    }

    pub fn get_array(&self) -> &[JsonValue] {
        match self {
            Self::Array(value) => value,
            _ => panic!("get_array on a non-Array JsonValue"),
        }
    }

    pub fn get_object(&self) -> &JsonObject {
        match self {
            Self::Object(value) => value,
            _ => panic!("get_object on a non-Object JsonValue"),
        }
    }

    pub fn get_object_mut(&mut self) -> &mut JsonObject {
        match self {
            Self::Object(value) => value,
            _ => panic!("get_object_mut on a non-Object JsonValue"),
        }
    }
}

// ---------------------------------------------------------------------------
// Serializer
// ---------------------------------------------------------------------------

/// `td::Jsonable` — anything storable into a JSON builder.
pub trait Jsonable {
    fn store(&self, jb: &mut JsonBuilder);
}

/// `td::JsonTrue`.
pub struct JsonTrue;
/// `td::JsonFalse`.
pub struct JsonFalse;
/// `td::JsonNull`.
pub struct JsonNull;
/// `td::JsonBool`.
pub struct JsonBool(pub bool);
/// `td::JsonInt`.
pub struct JsonInt(pub i32);
/// `td::JsonLong`.
pub struct JsonLong(pub i64);
/// `td::JsonFloat` — fixed-point with 6 decimals, like C++ `FixedDouble(x, 6)`.
pub struct JsonFloat(pub f64);
/// `td::JsonString` — full escaping: control characters and all non-ASCII
/// become `\uXXXX` (surrogate pairs above U+FFFF).
pub struct JsonString<'a>(pub &'a str);
/// `td::JsonRawString` — only quotes, backslashes and control characters are
/// escaped; non-ASCII bytes pass through.
pub struct JsonRawString<'a>(pub &'a str);
/// `td::JsonRaw` — the value is appended verbatim.
pub struct JsonRaw<'a>(pub &'a str);

/// Writes one `\uXXXX` escape (lowercase hex, like `JsonOneChar`).
fn write_one_char(out: &mut String, c: u32) {
    let _ = write!(out, "\\u{c:04x}");
}

/// `td::JsonChar`: `\uXXXX` or a surrogate pair for `c > 0xFFFF`.
fn write_json_char(out: &mut String, c: u32) {
    if c < 0x10000 {
        debug_assert!(!(0xD7FF < c && c < 0xE000), "surrogate in JSON output");
        write_one_char(out, c);
    } else {
        debug_assert!(c <= 0x10FFFF);
        write_one_char(out, 0xD7C0 + (c >> 10));
        write_one_char(out, 0xDC00 + (c & 0x3FF));
    }
}

/// Appends the C++ `JsonString` escaping of `s` (with quotes).
fn append_json_string(out: &mut String, s: &str) {
    out.push('"');
    for c in s.chars() {
        match c {
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            '\u{8}' => out.push_str("\\b"),
            '\u{c}' => out.push_str("\\f"),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            '\t' => out.push_str("\\t"),
            c if (c as u32) <= 31 => write_one_char(out, c as u32),
            c if (c as u32) < 0x80 => out.push(c),
            c => write_json_char(out, c as u32),
        }
    }
    out.push('"');
}

/// Appends the C++ `JsonRawString` escaping of `s` (with quotes): bytes at
/// and above 0x80 pass through raw, so the escaping works on the UTF-8 byte
/// stream (the input `&str` is valid UTF-8 and only ASCII escapes are
/// inserted, keeping the result valid).
fn append_json_raw_string(out: &mut String, s: &str) {
    let mut buf = Vec::with_capacity(s.len() + 2);
    buf.push(b'"');
    for &byte in s.as_bytes() {
        match byte {
            b'"' => buf.extend_from_slice(b"\\\""),
            b'\\' => buf.extend_from_slice(b"\\\\"),
            b'\x08' => buf.extend_from_slice(b"\\b"),
            b'\x0c' => buf.extend_from_slice(b"\\f"),
            b'\n' => buf.extend_from_slice(b"\\n"),
            b'\r' => buf.extend_from_slice(b"\\r"),
            b'\t' => buf.extend_from_slice(b"\\t"),
            b if b <= 31 => {
                let mut escape = String::new();
                write_one_char(&mut escape, u32::from(b));
                buf.extend_from_slice(escape.as_bytes());
            }
            b => buf.push(b),
        }
    }
    buf.push(b'"');
    out.push_str(&String::from_utf8(buf).expect("valid UTF-8 in, valid UTF-8 out"));
}

/// `td::JsonBuilder` — scope-based writer with compact (`offset < 0`) and
/// pretty (three-space indentation) modes. The C++ scope objects become
/// closure scopes: [`JsonBuilder::value`], [`JsonBuilder::array`] and
/// [`JsonBuilder::object`].
pub struct JsonBuilder {
    buf: String,
    offset: Option<usize>,
}

impl JsonBuilder {
    pub fn new(pretty: bool) -> Self {
        Self {
            buf: String::new(),
            offset: if pretty { Some(0) } else { None },
        }
    }

    pub fn is_pretty(&self) -> bool {
        self.offset.is_some()
    }

    /// `td::JsonBuilder::string_builder()`.
    pub fn as_str(&self) -> &str {
        &self.buf
    }

    pub fn offset(&self) -> Option<usize> {
        self.offset
    }

    fn print_offset(&mut self) {
        if let Some(offset) = self.offset {
            self.buf.push('\n');
            for _ in 0..offset {
                self.buf.push_str("   ");
            }
        }
    }

    fn inc_offset(&mut self) {
        if let Some(offset) = &mut self.offset {
            *offset += 1;
        }
    }

    fn dec_offset(&mut self) {
        if let Some(offset) = &mut self.offset {
            debug_assert!(*offset > 0);
            *offset -= 1;
        }
    }

    /// `td::JsonBuilder::enter_value() << value`.
    pub fn value<T: Jsonable + ?Sized>(&mut self, value: &T) {
        value.store(self);
    }

    /// `td::JsonBuilder::enter_array()` with the elements written in `f`.
    pub fn array(&mut self, f: impl FnOnce(&mut JsonArrayScope<'_>)) {
        self.inc_offset();
        self.buf.push('[');
        f(&mut JsonArrayScope {
            jb: self,
            is_first: false,
        });
        self.dec_offset();
        self.print_offset();
        self.buf.push(']');
    }

    /// `td::JsonBuilder::enter_object()` with the fields written in `f`.
    pub fn object(&mut self, f: impl FnOnce(&mut JsonObjectScope<'_>)) {
        self.inc_offset();
        self.buf.push('{');
        f(&mut JsonObjectScope {
            jb: self,
            is_first: false,
        });
        self.dec_offset();
        self.print_offset();
        self.buf.push('}');
    }
}

/// `td::JsonArrayScope`.
pub struct JsonArrayScope<'a> {
    jb: &'a mut JsonBuilder,
    is_first: bool,
}

impl JsonArrayScope<'_> {
    /// `td::JsonArrayScope::enter_value() << x` — a comma is emitted before
    /// every element except the first.
    pub fn value<T: Jsonable + ?Sized>(&mut self, value: &T) {
        if self.is_first {
            self.jb.buf.push(',');
        } else {
            self.is_first = true;
        }
        self.jb.print_offset();
        self.jb.value(value);
    }
}

/// `td::JsonObjectScope`.
pub struct JsonObjectScope<'a> {
    jb: &'a mut JsonBuilder,
    is_first: bool,
}

impl JsonObjectScope<'_> {
    /// `td::JsonObjectScope::operator()(field, value)` — the key is escaped
    /// like a `JsonString`; the separator is ` : ` in pretty mode.
    pub fn field<T: Jsonable + ?Sized>(&mut self, name: &str, value: &T) {
        if self.is_first {
            self.jb.buf.push(',');
        } else {
            self.is_first = true;
        }
        self.jb.print_offset();
        append_json_string(&mut self.jb.buf, name);
        if self.jb.is_pretty() {
            self.jb.buf.push_str(" : ");
        } else {
            self.jb.buf.push(':');
        }
        self.jb.value(value);
    }

    /// `td::JsonObjectScope::operator<<(JsonRaw)` — appends raw content
    /// without a comma; the next `field` call resumes comma separation.
    pub fn raw(&mut self, raw: &str) {
        self.is_first = true;
        self.jb.buf.push_str(raw);
    }
}

impl Jsonable for JsonTrue {
    fn store(&self, jb: &mut JsonBuilder) {
        jb.buf.push_str("true");
    }
}

impl Jsonable for JsonFalse {
    fn store(&self, jb: &mut JsonBuilder) {
        jb.buf.push_str("false");
    }
}

impl Jsonable for JsonNull {
    fn store(&self, jb: &mut JsonBuilder) {
        jb.buf.push_str("null");
    }
}

impl Jsonable for JsonBool {
    fn store(&self, jb: &mut JsonBuilder) {
        jb.buf.push_str(if self.0 { "true" } else { "false" });
    }
}

impl Jsonable for JsonInt {
    fn store(&self, jb: &mut JsonBuilder) {
        let _ = write!(jb.buf, "{}", self.0);
    }
}

impl Jsonable for JsonLong {
    fn store(&self, jb: &mut JsonBuilder) {
        let _ = write!(jb.buf, "{}", self.0);
    }
}

impl Jsonable for JsonFloat {
    fn store(&self, jb: &mut JsonBuilder) {
        // C++ `operator<<(double)` = `FixedDouble(x, 6)`: fixed, 6 decimals.
        let _ = write!(jb.buf, "{:.6}", self.0);
    }
}

impl Jsonable for JsonString<'_> {
    fn store(&self, jb: &mut JsonBuilder) {
        append_json_string(&mut jb.buf, self.0);
    }
}

impl Jsonable for JsonRawString<'_> {
    fn store(&self, jb: &mut JsonBuilder) {
        append_json_raw_string(&mut jb.buf, self.0);
    }
}

impl Jsonable for JsonRaw<'_> {
    fn store(&self, jb: &mut JsonBuilder) {
        jb.buf.push_str(self.0);
    }
}

/// `JsonValueScope::operator<<(Slice)` — a string value.
impl Jsonable for str {
    fn store(&self, jb: &mut JsonBuilder) {
        append_json_string(&mut jb.buf, self);
    }
}

/// `JsonValueScope::operator<<(int32)`.
impl Jsonable for i32 {
    fn store(&self, jb: &mut JsonBuilder) {
        let _ = write!(jb.buf, "{self}");
    }
}

/// `JsonValueScope::operator<<(int64)`.
impl Jsonable for i64 {
    fn store(&self, jb: &mut JsonBuilder) {
        let _ = write!(jb.buf, "{self}");
    }
}

/// `JsonValueScope::operator<<(double)`.
impl Jsonable for f64 {
    fn store(&self, jb: &mut JsonBuilder) {
        let _ = write!(jb.buf, "{self:.6}");
    }
}

impl Jsonable for JsonValue {
    fn store(&self, jb: &mut JsonBuilder) {
        match self {
            Self::Null => jb.buf.push_str("null"),
            Self::Boolean(true) => jb.buf.push_str("true"),
            Self::Boolean(false) => jb.buf.push_str("false"),
            Self::Number(number) => jb.buf.push_str(number),
            Self::String(string) => append_json_string(&mut jb.buf, string),
            Self::Array(values) => jb.array(|array| {
                for value in values {
                    array.value(value);
                }
            }),
            Self::Object(object) => jb.object(|scope| {
                object.foreach(|name, value| {
                    scope.field(name, value);
                });
            }),
        }
    }
}

/// `td::json_object(f)` — builds an object from a closure.
pub fn json_object_fn(f: impl FnOnce(&mut JsonObjectScope<'_>)) -> impl Jsonable {
    struct JsonObjectFn<F>(std::cell::UnsafeCell<F>);
    impl<F: FnOnce(&mut JsonObjectScope<'_>)> Jsonable for JsonObjectFn<F> {
        fn store(&self, jb: &mut JsonBuilder) {
            // SAFETY: the adapter is single-use (like C++ `JsonObjectImpl`);
            // `store` runs exactly once before the wrapper is dropped, so
            // moving the closure out of the `UnsafeCell` cannot be observed
            // by any other access.
            let f = unsafe { self.0.get().read() };
            jb.object(f);
        }
    }
    JsonObjectFn(std::cell::UnsafeCell::new(f))
}

/// `td::json_array(f)` — builds an array from a closure.
pub fn json_array_fn(f: impl FnOnce(&mut JsonArrayScope<'_>)) -> impl Jsonable {
    struct JsonArrayFn<F>(std::cell::UnsafeCell<F>);
    impl<F: FnOnce(&mut JsonArrayScope<'_>)> Jsonable for JsonArrayFn<F> {
        fn store(&self, jb: &mut JsonBuilder) {
            // SAFETY: same single-use reasoning as `json_object_fn`.
            let f = unsafe { self.0.get().read() };
            jb.array(f);
        }
    }
    JsonArrayFn(std::cell::UnsafeCell::new(f))
}

/// `td::json_array(a, f)` — maps an iterator into an array.
pub fn json_array_items<I, F, V>(iter: I, f: F) -> impl Jsonable
where
    I: IntoIterator,
    F: Fn(&I::Item) -> V,
    V: Jsonable,
{
    json_array_fn(move |array| {
        for item in iter {
            array.value(&f(&item));
        }
    })
}

/// `td::json_encode(val, pretty)`.
pub fn json_encode<T: Jsonable + ?Sized>(val: &T, pretty: bool) -> String {
    let mut jb = JsonBuilder::new(pretty);
    jb.value(val);
    if pretty {
        jb.buf.push('\n');
    }
    jb.buf
}

// ---------------------------------------------------------------------------
// Decoder
// ---------------------------------------------------------------------------

/// `td::Parser` subset used by the JSON decoder.
pub struct JsonParser<'a> {
    data: &'a [u8],
    pos: usize,
}

impl<'a> JsonParser<'a> {
    /// `td::Parser(Slice)`.
    pub fn new(data: &'a [u8]) -> Self {
        Self { data, pos: 0 }
    }

    pub fn empty(&self) -> bool {
        self.pos >= self.data.len()
    }

    /// `td::Parser::ptr()` as a byte offset into the input — lets callers
    /// slice out the raw text a skip-based walk just consumed.
    pub fn position(&self) -> usize {
        self.pos
    }

    /// `td::Parser::peek_char` — 0 at the end.
    pub fn peek_char(&self) -> u8 {
        self.data.get(self.pos).copied().unwrap_or(0)
    }

    pub fn next_byte(&mut self) -> Option<u8> {
        let byte = self.data.get(self.pos).copied()?;
        self.pos += 1;
        Some(byte)
    }

    /// `td::Parser::try_skip(Slice)`.
    pub fn try_skip(&mut self, s: &[u8]) -> bool {
        if self.data[self.pos..].starts_with(s) {
            self.pos += s.len();
            true
        } else {
            false
        }
    }

    /// `td::Parser::skip(char)`.
    pub fn skip(&mut self, c: u8) {
        assert_eq!(self.peek_char(), c, "skip expected {c}");
        self.pos += 1;
    }

    pub fn skip_whitespaces(&mut self) {
        while matches!(self.peek_char(), b' ' | b'\t' | b'\n' | b'\r') {
            self.pos += 1;
        }
    }

    /// `td::Parser::read_while(pred)`.
    pub fn read_while(&mut self, pred: impl Fn(u8) -> bool) -> &'a [u8] {
        let start = self.pos;
        while self.pos < self.data.len() && pred(self.data[self.pos]) {
            self.pos += 1;
        }
        &self.data[start..self.pos]
    }

    pub fn advance(&mut self, n: usize) {
        self.pos += n;
    }
}

fn hex_to_int(byte: u8) -> u32 {
    match byte {
        b'0'..=b'9' => u32::from(byte - b'0'),
        b'a'..=b'f' => u32::from(byte - b'a' + 10),
        b'A'..=b'F' => u32::from(byte - b'A' + 10),
        _ => 16,
    }
}

/// Encodes `code` as UTF-8 bytes; lone surrogates (which the C++ decoder
/// writes out as raw 3-byte sequences) become U+FFFD, the documented
/// divergence for invalid-UTF-8 results.
fn push_utf8_character(bytes: &mut Vec<u8>, code: u32) {
    match char::from_u32(code) {
        Some(c) => {
            let mut buf = [0u8; 4];
            bytes.extend_from_slice(c.encode_utf8(&mut buf).as_bytes());
        }
        None => bytes.extend_from_slice("\u{FFFD}".as_bytes()),
    }
}

/// `td::json_string_decode` — decodes a quoted string, processing escapes.
pub fn json_string_decode(parser: &mut JsonParser<'_>) -> Result<String> {
    if !parser.try_skip(b"\"") {
        return Result::Err(Status::error_message("Opening '\"' expected"));
    }
    let mut result = Vec::new();
    loop {
        match parser.next_byte() {
            None => return Result::Err(Status::error_message("Closing '\"' not found")),
            Some(b'"') => break,
            Some(b'\\') => match parser.next_byte() {
                None => return Result::Err(Status::error_message("Closing '\"' not found")),
                Some(b'b') => result.push(0x08),
                Some(b'f') => result.push(0x0C),
                Some(b'n') => result.push(b'\n'),
                Some(b'r') => result.push(b'\r'),
                Some(b't') => result.push(b'\t'),
                Some(b'u') => {
                    let mut num = 0u32;
                    for _ in 0..4 {
                        let Some(byte) = parser.next_byte() else {
                            return Result::Err(Status::error_message(
                                "\\u has less than 4 symbols",
                            ));
                        };
                        let digit = hex_to_int(byte);
                        if digit == 16 {
                            return Result::Err(Status::error_message(
                                "Invalid \\u -- not hex digit",
                            ));
                        }
                        num = num * 16 + digit;
                    }
                    if 0xD7FF < num && num < 0xE000 {
                        // Look for a second surrogate of a valid pair.
                        if parser.data[parser.pos..].starts_with(b"\\u") {
                            let save = parser.pos;
                            parser.pos += 2;
                            let mut new_num = 0u32;
                            for _ in 0..4 {
                                let Some(byte) = parser.next_byte() else {
                                    parser.pos = save;
                                    break;
                                };
                                let digit = hex_to_int(byte);
                                if digit == 16 {
                                    parser.pos = save;
                                    break;
                                }
                                new_num = new_num * 16 + digit;
                            }
                            if parser.pos != save {
                                if 0xD7FF < new_num && new_num < 0xE000 {
                                    num = (((num & 0x3FF) << 10) | (new_num & 0x3FF)) + 0x10000;
                                } else {
                                    parser.pos = save;
                                }
                            }
                        }
                    }
                    push_utf8_character(&mut result, num);
                }
                Some(c) => result.push(c),
            },
            Some(c) => result.push(c),
        }
    }
    Result::Ok(String::from_utf8_lossy(&result).into_owned())
}

/// `td::json_string_skip` — skips a quoted string without building it.
pub fn json_string_skip(parser: &mut JsonParser<'_>) -> Status {
    if !parser.try_skip(b"\"") {
        return Status::error_message("Opening '\"' expected");
    }
    loop {
        match parser.next_byte() {
            None => return Status::error_message("Closing '\"' not found"),
            Some(b'"') => return Status::ok(),
            Some(b'\\') => match parser.next_byte() {
                None => return Status::error_message("Closing '\"' not found"),
                Some(b'u') => {
                    for _ in 0..4 {
                        let Some(byte) = parser.next_byte() else {
                            return Status::error_message("\\u has less than 4 symbols");
                        };
                        if hex_to_int(byte) == 16 {
                            return Status::error_message("Invalid \\u -- not hex digit");
                        }
                    }
                    let num_start = parser.pos - 4;
                    let num = hex4(&parser.data[num_start..parser.pos]);
                    if 0xD7FF < num && num < 0xE000 && parser.data[parser.pos..].starts_with(b"\\u")
                    {
                        parser.pos += 2;
                        for _ in 0..4 {
                            let Some(byte) = parser.next_byte() else {
                                return Status::error_message("\\u has less than 4 symbols");
                            };
                            if hex_to_int(byte) == 16 {
                                return Status::error_message("Invalid \\u -- not hex digit");
                            }
                        }
                        let second_start = parser.pos - 4;
                        let new_num = hex4(&parser.data[second_start..parser.pos]);
                        if !(0xD7FF < new_num && new_num < 0xE000) {
                            // C++ `cur_src -= 6`: leave the second \uXXXX
                            // unconsumed.
                            parser.pos = num_start + 4;
                        }
                    }
                }
                Some(_) => {}
            },
            Some(_) => {}
        }
    }
}

fn hex4(bytes: &[u8]) -> u32 {
    let mut num = 0u32;
    for &byte in bytes {
        num = num * 16 + hex_to_int(byte);
    }
    num
}

/// `td::do_json_decode` with the default max depth applied by
/// [`json_decode`].
pub fn do_json_decode(parser: &mut JsonParser<'_>, max_depth: i32) -> Result<JsonValue> {
    if max_depth < 0 {
        return Result::Err(Status::error_message("Too big object depth"));
    }

    parser.skip_whitespaces();
    match parser.peek_char() {
        b'f' => {
            if parser.try_skip(b"false") {
                return Result::Ok(JsonValue::Boolean(false));
            }
            Result::Err(Status::error_message(
                "Token starts with 'f' -- false expected",
            ))
        }
        b't' => {
            if parser.try_skip(b"true") {
                return Result::Ok(JsonValue::Boolean(true));
            }
            Result::Err(Status::error_message(
                "Token starts with 't' -- true expected",
            ))
        }
        b'n' => {
            if parser.try_skip(b"null") {
                return Result::Ok(JsonValue::Null);
            }
            Result::Err(Status::error_message(
                "Token starts with 'n' -- null expected",
            ))
        }
        b'"' => {
            crate::try_result!(string, json_string_decode(parser));
            Result::Ok(JsonValue::String(string))
        }
        b'[' => {
            parser.skip(b'[');
            parser.skip_whitespaces();
            let mut result = Vec::new();
            if parser.try_skip(b"]") {
                return Result::Ok(JsonValue::Array(result));
            }
            loop {
                if parser.empty() {
                    return Result::Err(Status::error_message("Unexpected string end"));
                }
                crate::try_result!(value, do_json_decode(parser, max_depth - 1));
                result.push(value);

                parser.skip_whitespaces();
                if parser.try_skip(b"]") {
                    break;
                }
                if parser.try_skip(b",") {
                    parser.skip_whitespaces();
                    continue;
                }
                if parser.empty() {
                    return Result::Err(Status::error_message("Unexpected string end"));
                }
                return Result::Err(Status::error_message(
                    "Unexpected symbol while parsing JSON Array",
                ));
            }
            Result::Ok(JsonValue::Array(result))
        }
        b'{' => {
            parser.skip(b'{');
            parser.skip_whitespaces();
            if parser.try_skip(b"}") {
                return Result::Ok(JsonValue::Object(JsonObject::new()));
            }
            let mut field_values = Vec::new();
            loop {
                if parser.empty() {
                    return Result::Err(Status::error_message("Unexpected string end"));
                }
                crate::try_result!(field, json_string_decode(parser));
                parser.skip_whitespaces();
                if !parser.try_skip(b":") {
                    return Result::Err(Status::error_message("':' expected"));
                }
                crate::try_result!(value, do_json_decode(parser, max_depth - 1));
                field_values.push((field, value));

                parser.skip_whitespaces();
                if parser.try_skip(b"}") {
                    break;
                }
                if parser.try_skip(b",") {
                    parser.skip_whitespaces();
                    continue;
                }
                if parser.empty() {
                    return Result::Err(Status::error_message("Unexpected string end"));
                }
                return Result::Err(Status::error_message(
                    "Unexpected symbol while parsing JSON Object",
                ));
            }
            Result::Ok(JsonValue::Object(JsonObject::from_fields(field_values)))
        }
        b'-' | b'+' | b'.' | b'0'..=b'9' => {
            let number = parser
                .read_while(|c| {
                    c == b'-'
                        || c.is_ascii_digit()
                        || c == b'e'
                        || c == b'E'
                        || c == b'+'
                        || c == b'.'
                })
                .to_vec();
            Result::Ok(JsonValue::Number(String::from_utf8(number).unwrap()))
        }
        0 => Result::Err(Status::error_message("Unexpected string end")),
        next => {
            if (0 < next) && (next < 127) {
                Result::Err(Status::error_message(&format!(
                    "Unexpected symbol '{}'",
                    next as char
                )))
            } else {
                Result::Err(Status::error_message("Unexpected symbol"))
            }
        }
    }
}

/// `td::do_json_skip`.
pub fn do_json_skip(parser: &mut JsonParser<'_>, max_depth: i32) -> Status {
    if max_depth < 0 {
        return Status::error_message("Too big object depth");
    }

    parser.skip_whitespaces();
    match parser.peek_char() {
        b'f' => {
            if parser.try_skip(b"false") {
                return Status::ok();
            }
            Status::error_message("Starts with 'f' -- false expected")
        }
        b't' => {
            if parser.try_skip(b"true") {
                return Status::ok();
            }
            Status::error_message("Starts with 't' -- true expected")
        }
        b'n' => {
            if parser.try_skip(b"null") {
                return Status::ok();
            }
            Status::error_message("Starts with 'n' -- null expected")
        }
        b'"' => json_string_skip(parser),
        b'[' => {
            parser.skip(b'[');
            parser.skip_whitespaces();
            if parser.try_skip(b"]") {
                return Status::ok();
            }
            loop {
                if parser.empty() {
                    return Status::error_message("Unexpected end");
                }
                let status = do_json_skip(parser, max_depth - 1);
                if status.is_error() {
                    return status;
                }

                parser.skip_whitespaces();
                if parser.try_skip(b"]") {
                    break;
                }
                if parser.try_skip(b",") {
                    parser.skip_whitespaces();
                    continue;
                }
                return Status::error_message("Unexpected symbol");
            }
            Status::ok()
        }
        b'{' => {
            parser.skip(b'{');
            parser.skip_whitespaces();
            if parser.try_skip(b"}") {
                return Status::ok();
            }
            loop {
                if parser.empty() {
                    return Status::error_message("Unexpected end");
                }
                let status = json_string_skip(parser);
                if status.is_error() {
                    return status;
                }
                parser.skip_whitespaces();
                if !parser.try_skip(b":") {
                    return Status::error_message("':' expected");
                }
                let status = do_json_skip(parser, max_depth - 1);
                if status.is_error() {
                    return status;
                }

                parser.skip_whitespaces();
                if parser.try_skip(b"}") {
                    break;
                }
                if parser.try_skip(b",") {
                    parser.skip_whitespaces();
                    continue;
                }
                return Status::error_message("Unexpected symbol");
            }
            Status::ok()
        }
        b'-' | b'+' | b'.' | b'0'..=b'9' => {
            parser.read_while(|c| {
                c == b'-' || c.is_ascii_digit() || c == b'e' || c == b'E' || c == b'+' || c == b'.'
            });
            Status::ok()
        }
        0 => Status::error_message("Unexpected end"),
        next => {
            if (0 < next) && (next < 127) {
                Status::error_message(&format!("Unexpected symbol '{}'", next as char))
            } else {
                Status::error_message("Unexpected symbol")
            }
        }
    }
}

/// `td::json_decode(json)`.
pub fn json_decode(json: &str) -> Result<JsonValue> {
    const DEFAULT_MAX_DEPTH: i32 = 100;
    let mut parser = JsonParser::new(json.as_bytes());
    let result = do_json_decode(&mut parser, DEFAULT_MAX_DEPTH);
    if result.is_ok() {
        parser.skip_whitespaces();
        if !parser.empty() {
            return Result::Err(Status::error_message("Expected string end"));
        }
    }
    result
}

// ---------------------------------------------------------------------------
// Number helpers (td::to_integer_safe / td::to_double semantics)
// ---------------------------------------------------------------------------

fn parse_int64(s: &str) -> Option<i64> {
    let bytes = s.as_bytes();
    if bytes.is_empty() {
        return None;
    }
    let (negative, digits) = match bytes[0] {
        b'-' => (true, &bytes[1..]),
        _ => (false, bytes),
    };
    if digits.is_empty() || !digits.iter().all(u8::is_ascii_digit) {
        return None;
    }
    let mut value: i64 = 0;
    for &digit in digits {
        value = value
            .checked_mul(10)?
            .checked_add(i64::from(digit - b'0'))?;
    }
    Some(if negative { -value } else { value })
}

/// `td::to_double`: `strtod` semantics with explicit `inf`/`infinity`/`nan`
/// recognition; unparseable input yields 0.0, exactly like the C++ version.
pub fn to_double(s: &str) -> f64 {
    let trimmed = s.trim();
    let (is_negative, rest) = match trimmed.as_bytes().first() {
        Some(b'-') => (true, &trimmed[1..]),
        Some(b'+') => (false, &trimmed[1..]),
        _ => (false, trimmed),
    };
    let token = rest
        .split(char::is_whitespace)
        .next()
        .unwrap_or("")
        .to_ascii_lowercase();
    if !token.is_empty() && matches!(token.as_str(), "inf" | "infinity" | "nan") {
        let value = if token.as_str() == "nan" {
            f64::NAN
        } else {
            f64::INFINITY
        };
        return if is_negative { -value } else { value };
    }
    trimmed.parse::<f64>().unwrap_or(0.0)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn decode_encode(str: &str, result: &str) {
        let value = match json_decode(str) {
            Result::Ok(value) => value,
            Result::Err(status) => panic!("decode of {str:?}: {status}"),
        };
        let new_str = json_encode(&value, false);
        assert_eq!(new_str, result, "roundtrip of {str:?}");
    }

    #[test]
    fn json_array_builder() {
        let mut jb = JsonBuilder::new(false);
        jb.array(|a| {
            a.value("Hello");
            a.value(&-123);
        });
        assert_eq!(jb.as_str(), "[\"Hello\",-123]");
        decode_encode(jb.as_str(), jb.as_str());
    }

    #[test]
    fn json_object_builder() {
        let mut jb = JsonBuilder::new(false);
        jb.object(|o| {
            o.field("key", "value");
            o.field("1", &2);
        });
        assert_eq!(jb.as_str(), "{\"key\":\"value\",\"1\":2}");
        decode_encode(jb.as_str(), jb.as_str());
    }

    #[test]
    fn json_nested_builder() {
        let mut jb = JsonBuilder::new(false);
        jb.array(|a| {
            a.value(&1);
            a.value(&json_array_fn(|inner| {
                inner.value(&2);
            }));
            a.value(&3);
        });
        assert_eq!(jb.as_str(), "[1,[2],3]");
        decode_encode(jb.as_str(), jb.as_str());
    }

    #[test]
    fn pretty_formatting() {
        let mut jb = JsonBuilder::new(true);
        jb.object(|o| {
            o.field("a", &1);
            o.field(
                "b",
                &json_array_fn(|a| {
                    a.value(&2);
                    a.value(&3);
                }),
            );
        });
        assert_eq!(
            jb.as_str(),
            "{\n   \"a\" : 1,\n   \"b\" : [\n      2,\n      3\n   ]\n}"
        );

        let value = json_decode("{\"a\":1}").unwrap();
        assert_eq!(json_encode(&value, true), "{\n   \"a\" : 1\n}\n");
        assert_eq!(json_encode(&value, false), "{\"a\":1}");
    }

    #[test]
    fn json_string_escapes_non_ascii_and_controls() {
        assert_eq!(json_encode("héllo", false), "\"h\\u00e9llo\"");
        assert_eq!(json_encode("\u{1F600}", false), "\"\\ud83d\\ude00\"");
        assert_eq!(
            json_encode("a\nb\tc\"d\\e", false),
            "\"a\\nb\\tc\\\"d\\\\e\""
        );
        assert_eq!(json_encode("\u{1}", false), "\"\\u0001\"");
        assert_eq!(json_encode("\u{1f}", false), "\"\\u001f\"");
        assert_eq!(json_encode(" ", false), "\" \"");

        // JsonRawString keeps high bytes raw but escapes quotes/controls.
        let mut jb = JsonBuilder::new(false);
        jb.value(&JsonRawString("é\""));
        assert_eq!(jb.as_str(), "\"é\\\"\"");
    }

    #[test]
    fn json_float_is_fixed_six_decimals() {
        let mut jb = JsonBuilder::new(false);
        jb.value(&JsonFloat(0.1));
        assert_eq!(jb.as_str(), "0.100000");
        let mut jb = JsonBuilder::new(false);
        jb.value(&1.5f64);
        assert_eq!(jb.as_str(), "1.500000");
    }

    #[test]
    fn kphp_roundtrips() {
        decode_encode("[]", "[]");
        decode_encode("[[]]", "[[]]");
        decode_encode("{}", "{}");
        decode_encode("\"\\n\"", "\"\\n\"");
        decode_encode(
            "\"some long string \\t \\r \\\\ \\n \\f \\\" \\u1234\"",
            "\"some long string \\t \\r \\\\ \\n \\f \\\" \\u1234\"",
        );
        decode_encode(
            "{\"keyboard\":[[\"\\u2022 abcdefg\"],[\"\\u2022 hijklmnop\"],[\"\\u2022 qrstuvwxyz\"]],\"one_time_keyboard\":true}",
            "{\"keyboard\":[[\"\\u2022 abcdefg\"],[\"\\u2022 hijklmnop\"],[\"\\u2022 qrstuvwxyz\"]],\"one_time_keyboard\":true}",
        );
        decode_encode(
            "  \n   {  \"keyboard\"  : \n  [[  \"\\u2022 abcdefg\"  ]  , \n [  \"\\u2022 hijklmnop\" \n ],[  \n \"\\u2022 qrstuvwxyz\"]], \n  \"one_time_keyboard\"\n:\ntrue\n}\n   \n",
            "{\"keyboard\":[[\"\\u2022 abcdefg\"],[\"\\u2022 hijklmnop\"],[\"\\u2022 qrstuvwxyz\"]],\"one_time_keyboard\":true}",
        );
    }

    #[test]
    fn json_object_get_field() {
        const ENCODED_OBJECT: &str = "{\"null\":null,\"bool\":true,\"int\":\"1\",\"int2\":2,\"long\":\"123456789012\",\"long2\":2123456789012,\"double\":12345678901.1,\"string\":\"string\",\"string2\":12345e+1,\"array\":[],\"object\":{}}";
        {
            let mut value = json_decode(ENCODED_OBJECT).unwrap();
            let object = value.get_object_mut();
            assert_eq!(json_encode(&object.extract_field("null"), false), "null");
            assert_eq!(json_encode(&object.extract_field("bool"), false), "true");
            assert_eq!(json_encode(&object.extract_field("bool"), false), "null");
            assert_eq!(json_encode(&object.extract_field("int"), false), "\"1\"");
            assert_eq!(json_encode(&object.extract_field("int2"), false), "2");
            assert_eq!(json_encode(&object.extract_field("int3"), false), "null");
            assert_eq!(
                json_encode(&object.extract_field("long"), false),
                "\"123456789012\""
            );
            assert_eq!(
                json_encode(&object.extract_field("long2"), false),
                "2123456789012"
            );
            assert_eq!(
                json_encode(&object.extract_field("double"), false),
                "12345678901.1"
            );
            assert_eq!(
                json_encode(&object.extract_field("string"), false),
                "\"string\""
            );
            assert_eq!(
                json_encode(&object.extract_field("string2"), false),
                "12345e+1"
            );
            assert_eq!(json_encode(&object.extract_field("array"), false), "[]");
            assert_eq!(json_encode(&object.extract_field("object"), false), "{}");
            assert_eq!(json_encode(&object.extract_field(""), false), "null");
        }

        {
            let mut value = json_decode(ENCODED_OBJECT).unwrap();
            let object = value.get_object_mut();
            assert!(
                object
                    .extract_optional_field("int", JsonValueType::Number)
                    .is_error()
            );
            assert!(
                object
                    .extract_optional_field("int2", JsonValueType::Number)
                    .is_ok()
            );
            assert!(
                object
                    .extract_optional_field("int2", JsonValueType::Number)
                    .is_error()
            );
            assert!(
                object
                    .extract_optional_field("int3", JsonValueType::Number)
                    .is_ok()
            );
            assert!(
                object
                    .extract_optional_field("int3", JsonValueType::Null)
                    .is_ok()
            );
            assert_eq!(
                object
                    .extract_optional_field("int", JsonValueType::String)
                    .ok()
                    .get_string(),
                "1"
            );
            assert_eq!(
                object
                    .extract_optional_field("int", JsonValueType::Null)
                    .ok()
                    .ty(),
                JsonValueType::Null
            );

            assert!(
                object
                    .extract_required_field("long", JsonValueType::Number)
                    .is_error()
            );
            assert!(
                object
                    .extract_required_field("long2", JsonValueType::Number)
                    .is_ok()
            );
            assert!(
                object
                    .extract_required_field("long3", JsonValueType::Number)
                    .is_error()
            );
            assert_eq!(
                object
                    .extract_required_field("long", JsonValueType::String)
                    .ok()
                    .get_string(),
                "123456789012"
            );
            assert_eq!(
                object
                    .extract_required_field("long", JsonValueType::Null)
                    .ok()
                    .ty(),
                JsonValueType::Null
            );
        }

        let value = json_decode(ENCODED_OBJECT).unwrap();
        let object = value.get_object();
        assert!(object.has_field("null"));
        assert!(object.has_field("object"));
        assert!(!object.has_field(""));
        assert!(!object.has_field("objec"));
        assert!(!object.has_field("object2"));

        assert!(object.get_optional_bool_field("int", false).is_error());
        assert_eq!(object.get_optional_bool_field("bool", false).ok(), &true);
        assert_eq!(object.get_required_bool_field("bool").ok(), &true);
        assert_eq!(object.get_optional_bool_field("bool3", false).ok(), &false);
        assert_eq!(object.get_optional_bool_field("bool4", true).ok(), &true);
        assert!(object.get_required_bool_field("bool5").is_error());

        assert!(object.get_optional_int_field("null", 0).is_error());
        assert_eq!(object.get_optional_int_field("int", 0).ok(), &1);
        assert_eq!(object.get_required_int_field("int").ok(), &1);
        assert_eq!(object.get_optional_int_field("int2", 0).ok(), &2);
        assert_eq!(object.get_required_int_field("int2").ok(), &2);
        assert_eq!(object.get_optional_int_field("int3", 0).ok(), &0);
        assert_eq!(object.get_optional_int_field("int4", 5).ok(), &5);
        assert!(object.get_required_int_field("int5").is_error());
        assert!(object.get_optional_int_field("long", 0).is_error());
        assert!(object.get_optional_int_field("long2", 0).is_error());

        assert!(object.get_optional_long_field("null", 0).is_error());
        assert_eq!(
            object.get_optional_long_field("long", 0).ok(),
            &123_456_789_012
        );
        assert_eq!(
            object.get_required_long_field("long").ok(),
            &123_456_789_012
        );
        assert_eq!(
            object.get_optional_long_field("long2", 0).ok(),
            &2_123_456_789_012
        );
        assert_eq!(
            object.get_required_long_field("long2").ok(),
            &2_123_456_789_012
        );
        assert_eq!(object.get_optional_long_field("long3", 0).ok(), &0);
        assert_eq!(object.get_optional_long_field("long4", 5).ok(), &5);
        assert!(object.get_required_long_field("long5").is_error());
        assert_eq!(object.get_optional_long_field("int", 0).ok(), &1);
        assert_eq!(object.get_optional_long_field("int2", 0).ok(), &2);

        let are_equal_double = |lhs: &f64, rhs: f64| rhs - 1e-3 < *lhs && *lhs < rhs + 1e-3;

        assert!(object.get_optional_double_field("null", 0.0).is_error());
        assert!(are_equal_double(
            object.get_optional_double_field("double", 0.0).ok(),
            12_345_678_901.1
        ));
        assert!(are_equal_double(
            object.get_required_double_field("double").ok(),
            12_345_678_901.1
        ));
        assert!(are_equal_double(
            object.get_optional_double_field("long2", 0.0).ok(),
            2_123_456_789_012.0
        ));
        assert!(are_equal_double(
            object.get_required_double_field("long2").ok(),
            2_123_456_789_012.0
        ));
        assert!(are_equal_double(
            object.get_optional_double_field("double3", 0.0).ok(),
            0.0
        ));
        assert!(are_equal_double(
            object.get_optional_double_field("double4", -5.23).ok(),
            -5.23
        ));
        assert!(object.get_required_double_field("double5").is_error());
        assert!(object.get_optional_double_field("int", 0.0).is_error());
        assert!(are_equal_double(
            object.get_optional_double_field("int2", 0.0).ok(),
            2.0
        ));

        assert!(object.get_optional_string_field("null", "").is_error());
        assert_eq!(
            object.get_optional_string_field("string", "").ok(),
            "string"
        );
        assert_eq!(object.get_required_string_field("string").ok(), "string");
        assert_eq!(
            object.get_optional_string_field("string2", "").ok(),
            "12345e+1"
        );
        assert_eq!(object.get_required_string_field("string2").ok(), "12345e+1");
        assert_eq!(object.get_optional_string_field("string3", "").ok(), "");
        assert_eq!(
            object.get_optional_string_field("string4", "abacaba").ok(),
            "abacaba"
        );
        assert!(object.get_required_string_field("string5").is_error());
        assert_eq!(object.get_optional_string_field("int", "").ok(), "1");
        assert_eq!(object.get_optional_string_field("int2", "").ok(), "2");
    }

    fn test_string_decode(str: &str, result: &str) {
        let mut skip_parser = JsonParser::new(str.as_bytes());
        let status = json_string_skip(&mut skip_parser);
        assert!(status.is_ok(), "skip of {str:?}: {status}");
        assert!(skip_parser.empty());

        let mut parser = JsonParser::new(str.as_bytes());
        let value = json_string_decode(&mut parser)
            .unwrap_or_else(|status| panic!("decode of {str:?}: {status}"));
        assert!(parser.empty());
        assert_eq!(value, result);
    }

    fn test_string_decode_error(str: &str) {
        let mut skip_parser = JsonParser::new(str.as_bytes());
        assert!(json_string_skip(&mut skip_parser).is_error());

        let mut parser = JsonParser::new(str.as_bytes());
        assert!(json_string_decode(&mut parser).is_error());
    }

    #[test]
    fn string_decode() {
        test_string_decode("\"\"", "");
        test_string_decode("\"abacaba\"", "abacaba");
        test_string_decode(
            "\"\\1\\a\\b\\c\\d\\e\\f\\g\\h\\i\\j\\k\\l\\m\\n\\o\\p\\q\\r\\s\\t\\u00201\\v\\w\\x\\y\\z\\U\\\"\\\\\\/\\+\\-\"",
            "1a\u{8}cde\u{c}ghijklm\nopq\rs\t 1vwxyzU\"\\/+-",
        );
        test_string_decode(
            "\"\\u0373\\ud7FB\\uD840\\uDC04\\uD840a\\uD840\\u0373\"",
            "\u{0373}\u{D7FB}\u{20004}\u{FFFD}a\u{FFFD}\u{0373}",
        );

        test_string_decode_error(" \"\"");
        test_string_decode_error("\"");
        test_string_decode_error("\"\\");
        test_string_decode_error("\"\\b'");
        test_string_decode_error("\"\\u\"");
        test_string_decode_error("\"\\u123\"");
        test_string_decode_error("\"\\u123g\"");
        test_string_decode_error("\"\\u123G\"");
        test_string_decode_error("\"\\u123 \"");
        test_string_decode_error("\"\\ug123\"");
        test_string_decode_error("\"\\uG123\"");
        test_string_decode_error("\"\\u 123\"");
        test_string_decode_error("\"\\uD800\\ug123\"");
        test_string_decode_error("\"\\uD800\\u123\"");
    }

    #[test]
    fn decode_error_messages_match_cpp() {
        let cases = [
            ("", "Unexpected string end"),
            ("[", "Unexpected string end"),
            ("[1 2]", "Unexpected symbol while parsing JSON Array"),
            ("{\"a\" 1}", "':' expected"),
            ("{", "Unexpected string end"),
            ("nul", "Token starts with 'n' -- null expected"),
            ("tru", "Token starts with 't' -- true expected"),
            ("fals", "Token starts with 'f' -- false expected"),
            ("1 x", "Expected string end"),
            ("[", "Unexpected string end"),
        ];
        for (input, expected) in cases {
            let status = json_decode(input).error().clone();
            assert_eq!(status.message(), expected, "decode of {input:?}");
        }
        let status = json_decode("[[").error().clone();
        assert_eq!(status.message(), "Unexpected string end");
    }

    #[test]
    fn to_double_semantics() {
        assert_eq!(to_double("1.5"), 1.5);
        assert_eq!(to_double("  -2.5  "), -2.5);
        assert_eq!(to_double("+3"), 3.0);
        assert_eq!(to_double("inf"), f64::INFINITY);
        assert_eq!(to_double("-infinity"), f64::NEG_INFINITY);
        assert!(to_double("nan").is_nan());
        assert_eq!(to_double("not a number"), 0.0);
        assert_eq!(to_double(""), 0.0);
    }

    #[test]
    fn raw_number_text_is_preserved() {
        for number in ["12345e+1", "0.30000000000000004", "-1.5E-3", ".5", "5."] {
            let json = format!("[{number}]");
            let value = json_decode(&json).unwrap();
            assert_eq!(json_encode(&value, false), json);
        }
    }

    #[test]
    fn object_raw_merges_and_comma_resumes() {
        let mut jb = JsonBuilder::new(false);
        jb.object(|o| {
            o.field("a", &1);
            o.raw(",\"b\":2");
            o.field("c", &3);
        });
        assert_eq!(jb.as_str(), "{\"a\":1,\"b\":2,\"c\":3}");
    }
}
