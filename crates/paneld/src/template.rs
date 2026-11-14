//! Text templates (DESIGN.md §5.4): literal text with `{key}` and
//! `{key:spec}` placeholders filled from the store, e.g.
//! `"{ha:sensor.outdoor_temperature:.1}°  {aircraft:nearest.callsign}"`.
//!
//! ```text
//! template    = { literal | "{{" | "}}" | placeholder }
//! placeholder = "{" source ":" name [ ":" spec ] "}"
//! source      = (letter | "_") { letter | digit | "_" | "-" }
//! name        = (letter | digit | "_") { letter | digit | "_" | "." | "-" | "@" }
//! spec        = the format spec of `format.rs`: [<|>][W][.N]
//! ```
//!
//! # Splitting key and spec
//!
//! `:` separates the source from the name *and* the key from the spec, so
//! the rule is positional: the **first** `:` ends the source prefix, the name
//! runs up to the **second** `:` (if any), and everything after the second
//! `:` is the spec. Names therefore never contain `:`. `{ha:sensor.x}` has no
//! spec, `{ha:sensor.x:.1}` has the spec `.1`, `{ha:sensor.x:}` has an empty
//! spec (same as none). A key needs a source prefix; `{n}` is an error.
//! Whitespace inside braces is an error. Literal braces are `{{` and `}}`.
//!
//! # Rendering
//!
//! - `Number` with a finite value: formatted with the spec (default `{}`).
//! - `Text`: inserted as is. The spec's width pads it (`>` right, the
//!   default, or `<` left) and its precision truncates it to that many
//!   characters.
//! - `Bool`: `on` or `off`, then like text.
//! - Missing key, a non-finite number, or a `Series`, `Image` or `Json`
//!   value: `--` (padded to the width like text).
//!
//! # Cost
//!
//! A template is parsed once. [`Template::fingerprint`] hashes just what the
//! output depends on, so a widget can skip [`Template::render_into`] on
//! frames where nothing it shows has changed.

use std::collections::hash_map::DefaultHasher;
use std::fmt;
use std::hash::{Hash, Hasher};

use crate::format::Spec;
use crate::store::{StoreSnapshot, Value};

/// What a placeholder renders as when it has nothing to show.
pub const MISSING: &str = "--";

/// A syntax error with the 1-based column (in characters) where it was found.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TemplateError {
    pub column: usize,
    pub message: String,
}

impl fmt::Display for TemplateError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "column {}: {}", self.column, self.message)
    }
}

impl std::error::Error for TemplateError {}

#[derive(Debug, Clone, PartialEq)]
enum Part {
    Literal(String),
    Value { key: String, spec: Spec },
}

/// A parsed template.
#[derive(Debug, Clone, PartialEq)]
pub struct Template {
    parts: Vec<Part>,
}

fn err<T>(column: usize, message: impl Into<String>) -> Result<T, TemplateError> {
    Err(TemplateError {
        column,
        message: message.into(),
    })
}

fn is_source_start(c: char) -> bool {
    c.is_ascii_alphabetic() || c == '_'
}

fn is_source_char(c: char) -> bool {
    c.is_ascii_alphanumeric() || matches!(c, '_' | '-')
}

fn is_name_start(c: char) -> bool {
    c.is_ascii_alphanumeric() || c == '_'
}

fn is_name_char(c: char) -> bool {
    c.is_ascii_alphanumeric() || matches!(c, '_' | '.' | '-' | '@')
}

/// Parse the text between `{` and `}`; `column` is the column of its first
/// character.
fn parse_placeholder(inner: &str, column: usize) -> Result<Part, TemplateError> {
    if inner.is_empty() {
        return err(
            column - 1,
            "empty placeholder; write {source:name} or {source:name:spec} (use '{{' for a literal brace)",
        );
    }
    let col_of = |byte: usize| column + inner[..byte].chars().count();
    let mut colons = inner.match_indices(':').map(|(i, _)| i);
    let first = colons.next();
    let second = colons.next();
    let Some(first) = first else {
        return err(
            column,
            format!("key {inner:?} needs a source prefix, like {{ha:{inner}}}"),
        );
    };
    let source = &inner[..first];
    let name_end = second.unwrap_or(inner.len());
    let name = &inner[first + 1..name_end];

    match source.chars().next() {
        None => {
            return err(
                column,
                "missing source before ':' (e.g. ha, prom, aircraft)",
            )
        }
        Some(c) if !is_source_start(c) => {
            return err(
                column,
                format!("source must start with a letter or '_', found {c:?}"),
            )
        }
        _ => {}
    }
    if let Some((i, c)) = source.char_indices().find(|(_, c)| !is_source_char(*c)) {
        return err(
            col_of(i),
            format!("unexpected {c:?} in source name (letters, digits, '_' and '-' only)"),
        );
    }
    match name.chars().next() {
        None => {
            return err(
                col_of(first + 1),
                format!("missing name after \"{source}:\""),
            )
        }
        Some(c) if !is_name_start(c) => {
            return err(
                col_of(first + 1),
                format!("name must start with a letter, digit or '_', found {c:?}"),
            )
        }
        _ => {}
    }
    if let Some((i, c)) = name.char_indices().find(|(_, c)| !is_name_char(*c)) {
        return err(
            col_of(first + 1 + i),
            format!("unexpected {c:?} in key name (letters, digits, '_', '.', '-' and '@' only)"),
        );
    }
    let spec = match second {
        None => Spec::default(),
        Some(s) => Spec::parse(&inner[s + 1..]).map_err(|message| TemplateError {
            column: col_of(s + 1),
            message,
        })?,
    };
    Ok(Part::Value {
        key: inner[..name_end].to_owned(),
        spec,
    })
}

impl Template {
    pub fn parse(src: &str) -> Result<Template, TemplateError> {
        let mut parts = Vec::new();
        let mut lit = String::new();
        let mut chars = src.chars().enumerate().peekable();
        while let Some((i, c)) = chars.next() {
            let col = i + 1;
            match c {
                '{' if chars.peek().is_some_and(|(_, n)| *n == '{') => {
                    chars.next();
                    lit.push('{');
                }
                '}' if chars.peek().is_some_and(|(_, n)| *n == '}') => {
                    chars.next();
                    lit.push('}');
                }
                '}' => return err(col, "unmatched '}' (use '}}' for a literal brace)"),
                '{' => {
                    let mut inner = String::new();
                    loop {
                        match chars.next() {
                            Some((_, '}')) => break,
                            Some((_, '{')) | None => {
                                return err(col, "unclosed '{' (use '{{' for a literal brace)")
                            }
                            Some((_, c)) => inner.push(c),
                        }
                    }
                    if let Some((off, c)) = inner.char_indices().find(|(_, c)| c.is_whitespace()) {
                        return err(
                            col + 1 + inner[..off].chars().count(),
                            format!("unexpected whitespace {c:?} inside a placeholder"),
                        );
                    }
                    if !lit.is_empty() {
                        parts.push(Part::Literal(std::mem::take(&mut lit)));
                    }
                    parts.push(parse_placeholder(&inner, col + 1)?);
                }
                c => lit.push(c),
            }
        }
        if !lit.is_empty() {
            parts.push(Part::Literal(lit));
        }
        Ok(Template { parts })
    }

    /// The store keys this template reads, in order of appearance.
    pub fn keys(&self) -> impl Iterator<Item = &str> {
        self.parts.iter().filter_map(|p| match p {
            Part::Value { key, .. } => Some(key.as_str()),
            Part::Literal(_) => None,
        })
    }

    /// True if the output never changes (no placeholders).
    pub fn is_static(&self) -> bool {
        self.keys().next().is_none()
    }

    /// Hash of everything the rendered output depends on: what each
    /// placeholder would show. Equal fingerprints mean equal output (up to
    /// 64-bit hash collisions); missing and unsupported values hash alike
    /// because they render alike. No allocation.
    pub fn fingerprint(&self, store: &StoreSnapshot) -> u64 {
        let mut h = DefaultHasher::new();
        for p in &self.parts {
            if let Part::Value { key, .. } = p {
                match store.get(key).map(|e| &e.value) {
                    Some(Value::Number { value, .. }) if value.is_finite() => {
                        1u8.hash(&mut h);
                        value.to_bits().hash(&mut h);
                    }
                    Some(Value::Text(t)) => {
                        2u8.hash(&mut h);
                        t.hash(&mut h);
                    }
                    Some(Value::Bool(b)) => {
                        3u8.hash(&mut h);
                        b.hash(&mut h);
                    }
                    _ => 0u8.hash(&mut h),
                }
            }
        }
        h.finish()
    }

    /// Append the rendered template to `out`.
    pub fn render_into(&self, store: &StoreSnapshot, out: &mut String) {
        for p in &self.parts {
            match p {
                Part::Literal(s) => out.push_str(s),
                Part::Value { key, spec } => match store.get(key).map(|e| &e.value) {
                    Some(Value::Number { value, .. }) if value.is_finite() => {
                        spec.write_number(*value, out)
                    }
                    Some(Value::Text(t)) => spec.write_text(t, out),
                    Some(Value::Bool(b)) => spec.write_text(if *b { "on" } else { "off" }, out),
                    _ => {
                        // Padded but never truncated by a precision.
                        let start = out.len();
                        out.push_str(MISSING);
                        spec.pad(start, out);
                    }
                },
            }
        }
    }

    /// Render to a new `String` (for tests and one-off use).
    pub fn render(&self, store: &StoreSnapshot) -> String {
        let mut s = String::new();
        self.render_into(store, &mut s);
        s
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::media::Bitmap;
    use crate::store::Entry;
    use std::collections::HashMap;
    use std::sync::Arc;
    use std::time::SystemTime;

    fn store(values: &[(&str, Value)]) -> StoreSnapshot {
        let m: HashMap<String, Entry> = values
            .iter()
            .map(|(k, v)| {
                (
                    (*k).to_owned(),
                    Entry {
                        value: v.clone(),
                        updated_at: SystemTime::UNIX_EPOCH,
                        source: "test".into(),
                    },
                )
            })
            .collect();
        Arc::new(m)
    }

    fn num(v: f64) -> Value {
        Value::Number {
            value: v,
            unit: Some("ignored".into()),
        }
    }

    fn render(t: &str, s: &StoreSnapshot) -> String {
        Template::parse(t).unwrap().render(s)
    }

    #[test]
    fn plain_text_is_one_literal() {
        let t = Template::parse("hello 21°").unwrap();
        assert!(t.is_static());
        assert_eq!(t.render(&store(&[])), "hello 21°");
        assert_eq!(Template::parse("").unwrap().render(&store(&[])), "");
    }

    #[test]
    fn numbers_use_the_spec() {
        let s = store(&[("ha:sensor.outdoor_temperature", num(21.54))]);
        assert_eq!(render("{ha:sensor.outdoor_temperature}", &s), "21.54");
        assert_eq!(render("{ha:sensor.outdoor_temperature:.1}°", &s), "21.5°");
        assert_eq!(
            render("[{ha:sensor.outdoor_temperature:>7.1}]", &s),
            "[   21.5]"
        );
        assert_eq!(
            render("[{ha:sensor.outdoor_temperature:<7.0}]", &s),
            "[22     ]"
        );
        assert_eq!(render("{ha:sensor.outdoor_temperature:}", &s), "21.54");
        let neg = store(&[("ha:x", num(-0.04))]);
        assert_eq!(render("{ha:x:.1}", &neg), "0.0");
    }

    #[test]
    fn text_and_bool_are_inserted() {
        let s = store(&[
            ("aircraft:nearest.callsign", Value::Text("DLH4YA".into())),
            ("ha:binary_sensor.door", Value::Bool(true)),
            ("ha:binary_sensor.window", Value::Bool(false)),
        ]);
        assert_eq!(render("{aircraft:nearest.callsign}!", &s), "DLH4YA!");
        assert_eq!(render("[{aircraft:nearest.callsign:>8}]", &s), "[  DLH4YA]");
        assert_eq!(render("[{aircraft:nearest.callsign:<8}]", &s), "[DLH4YA  ]");
        assert_eq!(render("[{aircraft:nearest.callsign:.3}]", &s), "[DLH]");
        assert_eq!(
            render("{ha:binary_sensor.door} {ha:binary_sensor.window:5}", &s),
            "on   off"
        );
    }

    #[test]
    fn missing_and_unsupported_values_are_dashes() {
        let img = Value::Image(Arc::new(Bitmap::transparent(1, 1)));
        let s = store(&[
            ("ha:inf", num(f64::INFINITY)),
            ("ha:nan", num(f64::NAN)),
            ("ha:series", Value::Series(Vec::new().into())),
            ("ha:image", img),
            (
                "ha:json",
                Value::Json(Arc::new(serde_json::json!({"a": 1}))),
            ),
        ]);
        for k in [
            "ha:none",
            "ha:inf",
            "ha:nan",
            "ha:series",
            "ha:image",
            "ha:json",
        ] {
            assert_eq!(render(&format!("<{{{k}:.1}}>"), &s), "<-->", "{k}");
        }
        assert_eq!(render("[{ha:none:>5}]", &s), "[   --]");
        assert_eq!(render("[{ha:none:<5}]", &s), "[--   ]");
    }

    #[test]
    fn braces_can_be_escaped() {
        let s = store(&[("ha:x", num(1.0))]);
        assert_eq!(render("{{{ha:x}}}", &s), "{1}");
        assert_eq!(render("a{{b}}c", &s), "a{b}c");
        assert_eq!(render("{{}}", &s), "{}");
    }

    #[test]
    fn keys_are_listed_in_order() {
        let t = Template::parse("{ha:a.b} x {prom:c_d:.1} {ha:a.b}").unwrap();
        assert_eq!(
            t.keys().collect::<Vec<_>>(),
            ["ha:a.b", "prom:c_d", "ha:a.b"]
        );
        assert!(!t.is_static());
    }

    #[test]
    fn key_characters_and_the_colon_split() {
        let s = store(&[
            ("ha:sensor.a-b_c@2", num(3.0)),
            ("my-src:9x", Value::Text("t".into())),
        ]);
        assert_eq!(render("{ha:sensor.a-b_c@2:.1}", &s), "3.0");
        assert_eq!(render("{my-src:9x}", &s), "t");
        // First ':' = source, second ':' starts the spec.
        let t = Template::parse("{ha:sensor.x:>6.1}").unwrap();
        assert_eq!(t.keys().collect::<Vec<_>>(), ["ha:sensor.x"]);
    }

    #[test]
    fn parse_errors_name_the_column() {
        for (src, col, want) in [
            ("abc {", 5, "unclosed '{'"),
            ("abc {ha:x", 5, "unclosed '{'"),
            ("abc }", 5, "unmatched '}'"),
            ("a {} b", 3, "empty placeholder"),
            ("a {x} b", 4, "needs a source prefix"),
            ("{:x}", 2, "missing source"),
            ("{1a:x}", 2, "source must start"),
            ("{ha.b:x}", 4, "unexpected '.' in source"),
            ("{ha:}", 5, "missing name"),
            ("{ha:.x}", 5, "name must start"),
            ("{ha:a/b}", 6, "unexpected '/' in key name"),
            ("ab {ha:x:q}", 10, "unsupported format spec"),
            ("{ha:x:.}", 7, "expected a precision"),
            ("{ha:x:>99}", 7, "width must be at most"),
            ("{ha: x}", 5, "whitespace"),
            ("{ha:x:.1:2}", 7, "unsupported format spec"),
            ("é {ha:é}", 7, "name must start"),
            ("{ha:x {ha:y}", 1, "unclosed '{'"),
        ] {
            let e = Template::parse(src).expect_err(src);
            assert_eq!(e.column, col, "{src:?}: {e}");
            assert!(e.message.contains(want), "{src:?}: {e}");
        }
        let e = Template::parse("{ha:x:q}").unwrap_err();
        assert!(e.to_string().starts_with("column 7: "), "{e}");
    }

    #[test]
    fn fingerprint_tracks_only_what_is_shown() {
        let t = Template::parse("{ha:a:.1} {ha:b} {ha:c}").unwrap();
        let base = [("ha:a", num(1.0)), ("ha:b", Value::Text("x".into()))];
        let f = t.fingerprint(&store(&base));
        // Unrelated keys do not matter.
        let mut other = base.to_vec();
        other.push(("ha:zzz", num(9.0)));
        assert_eq!(f, t.fingerprint(&store(&other)));
        // Missing and unsupported render alike.
        let mut series = base.to_vec();
        series.push(("ha:c", Value::Series(Vec::new().into())));
        assert_eq!(f, t.fingerprint(&store(&series)));
        // Changes do.
        let mut changed = base.to_vec();
        changed[0].1 = num(1.5);
        assert_ne!(f, t.fingerprint(&store(&changed)));
        changed[0].1 = Value::Text("1".into());
        assert_ne!(f, t.fingerprint(&store(&changed)));
        let mut b = base.to_vec();
        b[1].1 = Value::Bool(true);
        assert_ne!(f, t.fingerprint(&store(&b)));
    }
}
