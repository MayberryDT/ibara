//! Contract 4: the types every tool speaks, and the only source of the tool
//! list, the JSON Schemas and `help:<tool>`. See docs/agent-tools.md.
//!
//! - `tools`: input types for the eleven tools and the action, expectation and
//!   check vocabularies.
//! - `envelope`: the response envelope and the result types.
//! - `schema`: tool definitions and help generated from the input types.
//! - `render`: the compact text rendering of an envelope.
//!
//! Inputs are parsed with [`parse_input`], which names the offending field path
//! in every error. Tagged unions (`kind`, `op`) are parsed by hand so that paths
//! inside them survive; serde's own internally tagged enums lose them.

mod envelope;
mod render;
mod schema;
mod tools;

pub use envelope::*;
pub use render::render_text;
pub use schema::{ToolDef, help, tool_definitions};
pub use tools::*;

use crate::error::{IbaraError, Result, invalid};
use schemars::{JsonSchema, Schema, SchemaGenerator, json_schema};
use serde::de::{self, DeserializeOwned, Deserializer, Error as _};
use serde::{Deserialize, Serialize};
use serde_json::{Map, Value};
use std::borrow::Cow;
use std::fmt;

pub const CONTRACT_VERSION: u32 = 4;

/// The pattern every reference and request id matches.
pub const REF_PATTERN: &str = "^[A-Za-z0-9_.:-]{1,128}$";

/// A reference or id: `task_…`, `op_…`, `e12`, `c3`, a request id, `help:<tool>`.
#[derive(Debug, Clone, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize)]
#[serde(transparent)]
pub struct Ref(String);

impl Ref {
    /// `None` unless `s` matches [`REF_PATTERN`].
    pub fn new(s: impl Into<String>) -> Option<Ref> {
        let s = s.into();
        is_ref(&s).then_some(Ref(s))
    }
    pub fn as_str(&self) -> &str {
        &self.0
    }
    pub fn into_string(self) -> String {
        self.0
    }
}

fn is_ref(s: &str) -> bool {
    (1..=128).contains(&s.len())
        && s.bytes().all(|b| b.is_ascii_alphanumeric() || matches!(b, b'_' | b'.' | b':' | b'-'))
}

impl fmt::Display for Ref {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

impl AsRef<str> for Ref {
    fn as_ref(&self) -> &str {
        &self.0
    }
}

impl<'de> Deserialize<'de> for Ref {
    fn deserialize<D: Deserializer<'de>>(d: D) -> std::result::Result<Self, D::Error> {
        let s = String::deserialize(d)?;
        if is_ref(&s) {
            Ok(Ref(s))
        } else {
            Err(D::Error::custom(format!(
                "'{}' is not a reference; expected 1-128 of A-Z a-z 0-9 _ . : -",
                s.chars().take(40).collect::<String>()
            )))
        }
    }
}

impl JsonSchema for Ref {
    fn schema_name() -> Cow<'static, str> {
        "Ref".into()
    }
    fn inline_schema() -> bool {
        true
    }
    fn json_schema(_: &mut SchemaGenerator) -> Schema {
        json_schema!({ "type": "string" })
    }
}

/// A cross-field rule broken by otherwise well-formed input.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FieldError {
    pub path: String,
    pub message: String,
}

impl FieldError {
    pub fn new(path: impl Into<String>, message: impl Into<String>) -> Self {
        FieldError { path: path.into(), message: message.into() }
    }
}

/// Rules serde cannot express (exactly one of two fields, duplicate ids).
pub trait Validate {
    fn validate(&self) -> std::result::Result<(), FieldError> {
        Ok(())
    }
}

/// Parse a tool's arguments. Errors are `INVALID_ARGUMENT` with a message that
/// starts with the offending field path, e.g.
/// `steps[2].expect.kind: unknown variant 'windw', expected one of window, …`.
pub fn parse_input<T: DeserializeOwned + Validate>(tool: &str, args: &Value) -> Result<T> {
    let value: T = serde_path_to_error::deserialize(args).map_err(|e| {
        let path = e.path().to_string();
        let (path, message) = locate(&path, &e.into_inner().to_string());
        field_error(tool, &path, &message)
    })?;
    value.validate().map_err(|e| field_error(tool, &e.path, &e.message))?;
    Ok(value)
}

fn field_error(tool: &str, path: &str, message: &str) -> IbaraError {
    let text = if path.is_empty() { message.to_string() } else { format!("{path}: {message}") };
    let target = if path.is_empty() { "the arguments".to_string() } else { format!("`{path}`") };
    invalid(text).with("field", path).with(
        "next",
        format!("Fix {target} and call {tool} again. computer_status({{ref: \"help:{tool}\"}}) shows the full shape and an example."),
    )
}

// ---- path-preserving parsing of tagged unions ----

/// Marks an error message that carries a path relative to where it was raised.
const MARK: char = '\u{1}';

fn marked<E: de::Error>(path: &str, message: &str) -> E {
    E::custom(format!("{MARK}{path}{MARK}{message}"))
}

/// Combine a serde_path_to_error path (`.` for the root) with a message that may
/// carry a marked inner path, and tidy serde's wording.
fn locate(outer: &str, message: &str) -> (String, String) {
    let outer = if outer == "." { "" } else { outer };
    let (path, message) = match message.strip_prefix(MARK).and_then(|m| m.split_once(MARK)) {
        Some((inner, msg)) => (join(outer, inner), msg),
        None => (outer.to_string(), message),
    };
    tidy(path, message)
}

fn join(a: &str, b: &str) -> String {
    match (a.is_empty(), b.is_empty()) {
        (true, _) => b.to_string(),
        (_, true) => a.to_string(),
        _ if b.starts_with('[') => format!("{a}{b}"),
        _ => format!("{a}.{b}"),
    }
}

/// The name between the first pair of backticks after `prefix`.
fn quoted_after<'a>(message: &'a str, prefix: &str) -> Option<(&'a str, &'a str)> {
    let rest = message.strip_prefix(prefix)?.strip_prefix('`')?;
    let (name, tail) = rest.split_once('`')?;
    Some((name, tail.trim_start_matches([',', ' '])))
}

fn tidy(path: String, message: &str) -> (String, String) {
    if let Some((field, _)) = quoted_after(message, "missing field ") {
        return (join(&path, field), "required field is missing".into());
    }
    if let Some((field, tail)) = quoted_after(message, "unknown field ") {
        let path = if path.rsplit('.').next() == Some(field) { path } else { join(&path, field) };
        return (path, format!("unknown field, {}", tail.replace('`', "")));
    }
    if let Some((variant, tail)) = quoted_after(message, "unknown variant ") {
        return (path, format!("unknown variant '{variant}', {}", tail.replace('`', "")));
    }
    (path, message.replace('`', "'"))
}

/// Deserialize `value` (found at relative `prefix`) keeping the path of any error.
pub(crate) fn from_value_at<T: DeserializeOwned, E: de::Error>(prefix: &str, value: Value) -> std::result::Result<T, E> {
    serde_path_to_error::deserialize(value).map_err(|e| {
        let inner = e.path().to_string();
        let (path, message) = locate(&inner, &e.into_inner().to_string());
        marked(&join(prefix, &path), &message)
    })
}

/// Remove a required field from a map, parsing it with its path preserved.
pub(crate) fn take_field<T: DeserializeOwned, E: de::Error>(map: &mut Map<String, Value>, name: &str) -> std::result::Result<T, E> {
    match map.shift_remove(name) {
        Some(v) => from_value_at(name, v),
        None => Err(marked(name, "required field is missing")),
    }
}

/// Split a tagged object into its tag value and the remaining fields.
pub(crate) fn split_tag<E: de::Error>(mut map: Map<String, Value>, tag: &str, kinds: &[&str]) -> std::result::Result<(String, Value), E> {
    match map.shift_remove(tag) {
        Some(Value::String(kind)) if kinds.contains(&kind.as_str()) => Ok((kind, Value::Object(map))),
        Some(Value::String(kind)) => Err(marked(
            tag,
            &format!("unknown variant '{kind}', expected one of {}", kinds.join(", ")),
        )),
        Some(_) => Err(marked(tag, &format!("expected a string, one of {}", kinds.join(", ")))),
        None => Err(marked(tag, &format!("required field is missing; one of {}", kinds.join(", ")))),
    }
}

/// Deserialize a `Vec` of at most `N` items.
pub(crate) fn at_most<'de, D, T, const N: usize>(d: D) -> std::result::Result<Vec<T>, D::Error>
where
    D: Deserializer<'de>,
    T: Deserialize<'de>,
{
    let items = Vec::<T>::deserialize(d)?;
    if items.len() > N {
        return Err(D::Error::custom(format!("at most {N} items allowed, got {}", items.len())));
    }
    Ok(items)
}

/// A union discriminated by a string field (`kind` or `op`). Each variant holds
/// a struct with the variant's fields. Generates the enum, `Serialize`, a
/// path-preserving `Deserialize` and a flat JSON Schema (one object whose tag
/// is an `enum`, with a description listing each kind's fields).
macro_rules! tagged_union {
    (
        $(#[$meta:meta])*
        pub enum $name:ident tag $tag:literal {
            $( $(#[$vmeta:meta])* $lit:literal => $variant:ident($ty:ty) ),+ $(,)?
        }
    ) => {
        $(#[$meta])*
        #[derive(Debug, Clone, PartialEq, serde::Serialize)]
        #[serde(tag = $tag)]
        pub enum $name {
            $( $(#[$vmeta])* #[serde(rename = $lit)] $variant($ty) ),+
        }

        impl $name {
            pub const KINDS: &'static [&'static str] = &[$($lit),+];
            pub fn kind(&self) -> &'static str {
                match self { $( Self::$variant(_) => $lit ),+ }
            }
        }

        impl<'de> serde::Deserialize<'de> for $name {
            fn deserialize<D: serde::Deserializer<'de>>(d: D) -> std::result::Result<Self, D::Error> {
                let map = serde_json::Map::<String, serde_json::Value>::deserialize(d)?;
                let (kind, rest) = $crate::contract::split_tag::<D::Error>(map, $tag, Self::KINDS)?;
                match kind.as_str() {
                    $( $lit => $crate::contract::from_value_at("", rest).map(Self::$variant), )+
                    _ => unreachable!("split_tag accepts only listed kinds"),
                }
            }
        }

        impl schemars::JsonSchema for $name {
            fn schema_name() -> std::borrow::Cow<'static, str> {
                stringify!($name).into()
            }
            fn inline_schema() -> bool {
                true
            }
            fn json_schema(g: &mut schemars::SchemaGenerator) -> schemars::Schema {
                $crate::contract::schema::flat_union($tag, &[$( ($lit, <$ty as schemars::JsonSchema>::json_schema(g)) ),+])
            }
        }
    };
}
pub(crate) use tagged_union;

#[cfg(test)]
mod tests;
