// %CopyrightBegin%
//
// SPDX-License-Identifier: Apache-2.0
//
// Copyright Ericsson AB 2025-2026. All Rights Reserved.
//
// Licensed under the Apache License, Version 2.0 (the "License");
// you may not use this file except in compliance with the License.
// You may obtain a copy of the License at
//
//     http://www.apache.org/licenses/LICENSE-2.0
//
// Unless required by applicable law or agreed to in writing, software
// distributed under the License is distributed on an "AS IS" BASIS,
// WITHOUT WARRANTIES OR CONDITIONS OF ANY KIND, either express or implied.
// See the License for the specific language governing permissions and
// limitations under the License.
//
// %CopyrightEnd%
//! A tiny, dependency-free JSON **writer** (there is no reader: the crate only ever
//! emits reports, and the curated `RESULTS.md` is written from those artifacts).
//!
//! The crate is standalone (see `Cargo.toml`), so `serde_json` is unavailable; this
//! module replaces it with the small subset the reports need. Output is pretty-printed
//! with two-space indentation so the committed artifacts stay reviewable in git diffs.
/// A JSON value. Object keys are `String` so dynamic keys (module names) are possible;
/// the constructor helpers below keep the call sites terse.
#[derive(Debug, Clone, PartialEq)]
pub enum Json {
    /// `null`
    Null,
    /// `true` / `false`
    Bool(bool),
    /// An integer (counts, exit codes — never floats, which the reports do not need).
    Int(i64),
    /// A string, escaped on render.
    Str(String),
    /// An array.
    Arr(Vec<Json>),
    /// An object; member order is preserved as written.
    Obj(Vec<(String, Json)>),
}
impl Json {
    /// Renders the value as pretty-printed JSON text (no trailing newline).
    pub fn render(&self) -> String {
        let mut out = String::new();
        self.write(&mut out, 0);
        out
    }
    fn write(&self, out: &mut String, indent: usize) {
        match self {
            Json::Null => out.push_str("null"),
            Json::Bool(b) => out.push_str(if *b { "true" } else { "false" }),
            Json::Int(i) => out.push_str(&i.to_string()),
            Json::Str(s) => write_str(out, s),
            Json::Arr(items) => {
                if items.is_empty() {
                    out.push_str("[]");
                    return;
                }
                out.push_str("[\n");
                for (idx, item) in items.iter().enumerate() {
                    push_indent(out, indent + 1);
                    item.write(out, indent + 1);
                    if idx + 1 < items.len() {
                        out.push(',');
                    }
                    out.push('\n');
                }
                push_indent(out, indent);
                out.push(']');
            }
            Json::Obj(pairs) => {
                if pairs.is_empty() {
                    out.push_str("{}");
                    return;
                }
                out.push_str("{\n");
                for (idx, (key, value)) in pairs.iter().enumerate() {
                    push_indent(out, indent + 1);
                    write_str(out, key);
                    out.push_str(": ");
                    value.write(out, indent + 1);
                    if idx + 1 < pairs.len() {
                        out.push(',');
                    }
                    out.push('\n');
                }
                push_indent(out, indent);
                out.push('}');
            }
        }
    }
}
fn push_indent(out: &mut String, indent: usize) {
    for _ in 0..indent {
        out.push_str("  ");
    }
}
/// Writes a JSON string literal, escaping the characters JSON requires.
fn write_str(out: &mut String, s: &str) {
    out.push('"');
    for ch in s.chars() {
        match ch {
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            '\t' => out.push_str("\\t"),
            c if (c as u32) < 0x20 => out.push_str(&format!("\\u{:04x}", c as u32)),
            c => out.push(c),
        }
    }
    out.push('"');
}
/// `Json::Str` from anything string-like.
pub fn s(value: impl Into<String>) -> Json {
    Json::Str(value.into())
}
/// `Json::Int` from any non-negative integer (every count in a report is one).
pub fn i(value: impl Into<u64>) -> Json {
    Json::Int(value.into() as i64)
}
/// `Json::Arr` from a vector of values.
pub fn arr(values: Vec<Json>) -> Json {
    Json::Arr(values)
}
/// `Json::Obj` from ordered key/value pairs.
pub fn obj(pairs: Vec<(&str, Json)>) -> Json {
    Json::Obj(pairs.into_iter().map(|(k, v)| (k.to_string(), v)).collect())
}
/// `Json::Null` for an absent optional value.
pub fn opt_str(value: Option<&str>) -> Json {
    match value {
        Some(v) => Json::Str(v.to_string()),
        None => Json::Null,
    }
}
/// `Json::Int` for an optional count.
pub fn opt_int(value: Option<i64>) -> Json {
    match value {
        Some(v) => Json::Int(v),
        None => Json::Null,
    }
}
#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn renders_nested_structure() {
        let value = obj(vec![
            ("schema", s("suites/1")),
            ("n", i(3u64)),
            ("ok", Json::Bool(true)),
            ("missing", Json::Null),
            ("rows", arr(vec![obj(vec![("id", s("a"))]), obj(vec![("id", s("b"))])])),
            ("empty", arr(vec![])),
        ]);
        let text = value.render();
        assert!(text.contains("\"schema\": \"suites/1\""), "{text}");
        assert!(text.contains("\"n\": 3"), "{text}");
        assert!(text.contains("\"ok\": true"), "{text}");
        assert!(text.contains("\"empty\": []"), "{text}");
        assert!(text.contains("\"id\": \"a\""), "{text}");
    }
    #[test]
    fn escapes_control_characters_and_quotes() {
        let text = s("a\"b\\c\nd\te").render();
        assert_eq!(text, "\"a\\\"b\\\\c\\nd\\te\"");
        assert_eq!(s("\u{1}").render(), "\"\\u0001\"");
    }
}
