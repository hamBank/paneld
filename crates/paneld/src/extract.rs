//! Tiny JSON path extraction, shared by the HTTP poll and MQTT sources
//! (DESIGN.md §6 items 2 and 3). No JSONPath crate: we only need to walk to
//! one value.
//!
//! Syntax: `$` is the root, then any of `.name`, `["name"]` (for names with
//! dots or spaces) and `[index]` (non-negative array index). Examples:
//! `$.current.temp`, `$.list[0].main.temp`, `$["weird.key"]`. No wildcards,
//! filters or slices.
//!
//! [`to_value`] turns the found JSON into a store [`Value`]: numbers (and
//! numeric strings) become `Number`, booleans `Bool`, strings `Text`, arrays
//! and objects `Json`; `null` means "no value".

use std::sync::Arc;

use serde_json::Value as Json;

use crate::store::Value;

/// One step of a parsed path.
#[derive(Debug, Clone, PartialEq, Eq)]
enum Step {
    Key(String),
    Index(usize),
}

/// A parsed path. Build with [`JsonPath::parse`] at config load.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct JsonPath(Vec<Step>);

impl JsonPath {
    /// Parse `$...`. Errors name the 1-based column.
    pub fn parse(src: &str) -> Result<JsonPath, String> {
        let b = src.as_bytes();
        if b.first() != Some(&b'$') {
            return Err("a path must start with `$`".into());
        }
        let mut steps = Vec::new();
        let mut i = 1;
        while i < b.len() {
            match b[i] {
                b'.' => {
                    let start = i + 1;
                    let mut j = start;
                    while j < b.len() && b[j] != b'.' && b[j] != b'[' {
                        j += 1;
                    }
                    if j == start {
                        return Err(format!("column {}: expected a name after `.`", i + 1));
                    }
                    steps.push(Step::Key(src[start..j].to_owned()));
                    i = j;
                }
                b'[' => {
                    let close = src[i..]
                        .find(']')
                        .map(|k| i + k)
                        .ok_or_else(|| format!("column {}: `[` without `]`", i + 1))?;
                    let inner = &src[i + 1..close];
                    if let Some(q) = inner.strip_prefix('"').and_then(|r| r.strip_suffix('"')) {
                        steps.push(Step::Key(q.to_owned()));
                    } else if !inner.is_empty() && inner.bytes().all(|c| c.is_ascii_digit()) {
                        let n = inner
                            .parse()
                            .map_err(|_| format!("column {}: index too large", i + 2))?;
                        steps.push(Step::Index(n));
                    } else {
                        return Err(format!(
                            "column {}: expected an index like [0] or a quoted name like [\"a.b\"]",
                            i + 2
                        ));
                    }
                    i = close + 1;
                }
                _ => {
                    return Err(format!(
                        "column {}: expected `.` or `[` (wildcards and filters are not supported)",
                        i + 1
                    ))
                }
            }
        }
        Ok(JsonPath(steps))
    }

    /// The value at this path, if every step exists.
    pub fn find<'a>(&self, root: &'a Json) -> Option<&'a Json> {
        self.0.iter().try_fold(root, |v, step| match step {
            Step::Key(k) => v.get(k.as_str()),
            Step::Index(n) => v.get(*n),
        })
    }
}

/// Convert found JSON into a store value. `null` gives `None`. Strings that
/// parse as finite numbers become `Number` (many APIs and MQTT payloads send
/// numbers as text).
pub fn to_value(v: &Json, unit: Option<&str>) -> Option<Value> {
    let number = |value: f64| Value::Number {
        value,
        unit: unit.map(str::to_owned),
    };
    match v {
        Json::Null => None,
        Json::Bool(b) => Some(Value::Bool(*b)),
        Json::Number(n) => n.as_f64().filter(|f| f.is_finite()).map(number),
        Json::String(s) => Some(match s.trim().parse::<f64>() {
            Ok(f) if f.is_finite() => number(f),
            _ => Value::Text(s.clone()),
        }),
        Json::Array(_) | Json::Object(_) => Some(Value::Json(Arc::new(v.clone()))),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn walks_names_quoted_names_and_indexes() {
        let doc = json!({"current": {"temp": 21.5}, "list": [{"main": {"t": 1}}, {"main": {"t": 2}}], "a.b": "x"});
        let get = |p: &str| JsonPath::parse(p).unwrap().find(&doc).cloned();
        assert_eq!(get("$.current.temp"), Some(json!(21.5)));
        assert_eq!(get("$.list[1].main.t"), Some(json!(2)));
        assert_eq!(get(r#"$["a.b"]"#), Some(json!("x")));
        assert_eq!(get("$"), Some(doc.clone()));
        assert_eq!(get("$.missing.deep"), None);
        assert_eq!(get("$.list[9]"), None);
    }

    #[test]
    fn parse_errors_name_the_column() {
        for (p, want) in [
            ("current", "must start with `$`"),
            ("$.", "column 2: expected a name"),
            ("$.a[", "column 4: `[` without `]`"),
            ("$.a[x]", "column 5: expected an index"),
            ("$.a[*]", "column 5"),
            ("$..a", "column 2"),
            ("$a", "column 2: expected `.` or `[`"),
        ] {
            let e = JsonPath::parse(p).unwrap_err();
            assert!(e.contains(want), "{p}: {e}");
        }
    }

    #[test]
    fn converts_to_store_values() {
        let num = |v: f64, u: Option<&str>| Value::Number {
            value: v,
            unit: u.map(str::to_owned),
        };
        assert_eq!(to_value(&json!(3), Some("W")), Some(num(3.0, Some("W"))));
        assert_eq!(to_value(&json!(" 4.5 "), None), Some(num(4.5, None)));
        assert_eq!(to_value(&json!("on"), None), Some(Value::Text("on".into())));
        assert_eq!(
            to_value(&json!("NaN"), None),
            Some(Value::Text("NaN".into()))
        );
        assert_eq!(to_value(&json!(true), None), Some(Value::Bool(true)));
        assert_eq!(to_value(&json!(null), None), None);
        assert!(matches!(
            to_value(&json!([1, 2]), None),
            Some(Value::Json(_))
        ));
    }
}
