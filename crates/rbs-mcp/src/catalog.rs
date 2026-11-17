//! What a generated tool says about its method: a description and an input schema,
//! taken from the control API spec compiled into this program (mcp-server spec,
//! "Generated tools"). The text comes only from the build, never from RBS state.
//!
//! Until the control API has a generated JSON Schema (ADR-0044 follow-up), the
//! parameters are those the spec writes after the method, `method {a, b?}`: each one a
//! property of any type, required unless it ends in `?`. A method the spec writes
//! without braces gets an open object, so the API, not this program, checks its
//! parameters.

use serde_json::{Map, Value, json};

/// The control API spec, as built.
const SPEC: &str = include_str!("../../../docs/specs/control-api.md");

/// Longest description, in characters.
const MAX_DESCRIPTION: usize = 900;

/// What the spec says about one method.
#[derive(Debug, Clone, PartialEq)]
pub struct Entry {
    /// What it does, from the spec.
    pub description: String,
    /// JSON Schema of its parameters.
    pub input_schema: Value,
}

/// The entry of `method`.
pub fn entry(method: &str) -> Entry {
    let description = describe(method);
    let input_schema = match signature(method) {
        Some(params) => schema(&params),
        None => json!({ "type": "object" }),
    };
    Entry {
        description,
        input_schema,
    }
}

/// The parameters of the first `` `method {..}` `` in the spec, or `None`.
fn signature(method: &str) -> Option<Vec<(String, bool)>> {
    let needle = format!("`{method} {{");
    let start = SPEC.find(&needle)? + needle.len();
    let rest = &SPEC[start..];
    let end = rest.find("}`")?;
    let inner = &rest[..end];
    if inner.contains('`') {
        return None;
    }
    let mut params = Vec::new();
    for part in inner.split(',') {
        let part = part.trim();
        if part.is_empty() {
            continue;
        }
        let (name, optional) = match part.strip_suffix('?') {
            Some(n) => (n, true),
            None => (part, false),
        };
        // `{item, crop, keep_scale?}`: plain names only; anything else (a nested
        // shape) leaves the object open.
        if !name
            .bytes()
            .all(|b| b.is_ascii_lowercase() || b == b'_' || b.is_ascii_digit())
        {
            return None;
        }
        params.push((name.to_owned(), optional));
    }
    Some(params)
}

fn schema(params: &[(String, bool)]) -> Value {
    let mut properties = Map::new();
    let mut required = Vec::new();
    for (name, optional) in params {
        properties.insert(name.clone(), json!({}));
        if !optional {
            required.push(Value::String(name.clone()));
        }
    }
    let mut s = Map::new();
    s.insert("type".into(), "object".into());
    s.insert("properties".into(), Value::Object(properties));
    if !required.is_empty() {
        s.insert("required".into(), Value::Array(required));
    }
    Value::Object(s)
}

/// The spec's text on `method`: the purpose cell of its table row, or else the
/// sentence of its first mention.
fn describe(method: &str) -> String {
    let plain = format!("`{method}`");
    let braced = format!("`{method} {{");
    let mut sentence = None;
    for line in SPEC.lines() {
        if !line.contains(&plain) && !line.contains(&braced) {
            continue;
        }
        if line.starts_with("| ") {
            let cells: Vec<&str> = line.split(" | ").collect();
            if let Some(purpose) = cells.get(2) {
                return clean(purpose.trim_end_matches(" |").trim_end_matches('|'));
            }
        }
        if sentence.is_none() {
            let at = line
                .find(&braced)
                .or_else(|| line.find(&plain))
                .unwrap_or(0);
            let from = line[..at].rfind(". ").map_or(0, |i| i + 2);
            let to = line[at..].find(". ").map_or(line.len(), |i| at + i + 1);
            sentence = Some(clean(&line[from..to]));
        }
    }
    sentence.unwrap_or_else(|| format!("The control API method {method}."))
}

/// Without Markdown emphasis and links, cut at [`MAX_DESCRIPTION`] characters.
fn clean(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    let mut chars = text.trim().trim_start_matches("- ").chars().peekable();
    while let Some(c) = chars.next() {
        match c {
            '*' => {}
            // `[label](target)` keeps the label.
            ']' if chars.peek() == Some(&'(') => {
                for c in chars.by_ref() {
                    if c == ')' {
                        break;
                    }
                }
            }
            '[' => {}
            c => out.push(c),
        }
    }
    if out.chars().count() > MAX_DESCRIPTION {
        out = out.chars().take(MAX_DESCRIPTION - 1).collect();
        out.push('…');
    }
    out
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;

    #[test]
    fn a_braced_signature_gives_properties_and_required_names() {
        let e = entry("item.set_crop");
        assert_eq!(
            e.input_schema,
            json!({
                "type": "object",
                "properties": {"item": {}, "crop": {}, "keep_scale": {}},
                "required": ["item", "crop"]
            })
        );
        assert!(
            e.description.contains("cut from each edge"),
            "{}",
            e.description
        );
    }

    #[test]
    fn a_method_without_braces_gets_an_open_object() {
        let e = entry("studio.get");
        assert_eq!(e.input_schema, json!({"type": "object"}));
        assert!(e.description.contains("Canvas size"), "{}", e.description);
    }

    #[test]
    fn every_built_in_method_has_a_description() {
        for m in rbs_api::request::METHODS {
            let e = entry(m.name);
            assert!(!e.description.is_empty(), "{}", m.name);
            assert!(e.description.chars().count() <= MAX_DESCRIPTION);
            assert_eq!(e.input_schema["type"], "object", "{}", m.name);
        }
    }
}
