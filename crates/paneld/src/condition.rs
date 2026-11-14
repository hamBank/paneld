//! Scene conditions (DESIGN.md §5.4): a tiny boolean expression language
//! evaluated against a [`StoreSnapshot`].
//!
//! ```text
//! expr    = or
//! or      = and { "||" and }
//! and     = unary { "&&" unary }
//! unary   = "!" unary | primary
//! primary = "(" expr ")" | key [ op literal ]
//! op      = ">" | ">=" | "<" | "<=" | "==" | "!="
//! literal = number | "true" | "false" | "'text'" | "\"text\""
//! key     = source ":" name        e.g. ha:sensor.outside_temp, prom:house_power
//! ```
//!
//! Keys are a source prefix, a colon, then letters, digits, `_`, `-` and `.`
//! (the first character must be a letter or `_`). Text literals take `\\`,
//! `\"` and `\'` escapes.
//!
//! # Evaluation semantics
//!
//! Evaluation never panics and never fails:
//! - A bare `key` is *truthy* when the value is a Bool that is true, a Number
//!   that is non-zero (NaN is not), or a non-empty Text. A missing key, or a
//!   value of any other kind, is falsy.
//! - A comparison against a **missing key** is `false`, for every operator
//!   including `!=`.
//! - A comparison whose operand types differ (Text vs number, Bool vs
//!   number, ...) is `false`, again for every operator including `!=`.
//! - Numbers compare as `f64`; any comparison involving NaN is `false`.
//!   Text compares byte-wise (all six operators). Bools support only `==`
//!   and `!=`; the ordering operators are `false`.
//! - `!` negates the result as usual, so `!(ha:x > 5)` is *true* when `ha:x`
//!   is missing. Write `ha:x && !(ha:x > 5)` if that is not wanted.

use std::fmt;

use crate::store::{StoreSnapshot, Value};

/// Maximum parenthesis / `!` nesting, so a hostile config can't overflow the stack.
const MAX_DEPTH: usize = 64;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Op {
    Gt,
    Ge,
    Lt,
    Le,
    Eq,
    Ne,
}

#[derive(Debug, Clone, PartialEq)]
enum Literal {
    Number(f64),
    Bool(bool),
    Text(String),
}

#[derive(Debug, Clone, PartialEq)]
enum Expr {
    Truthy(String),
    Compare(String, Op, Literal),
    Not(Box<Expr>),
    And(Box<Expr>, Box<Expr>),
    Or(Box<Expr>, Box<Expr>),
}

/// A parsed condition. Build with [`Condition::parse`], run with
/// [`Condition::eval`].
#[derive(Debug, Clone, PartialEq)]
pub struct Condition(Expr);

/// A syntax error, with the 1-based column (in characters) where it was found.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ParseError {
    pub column: usize,
    pub message: String,
}

impl fmt::Display for ParseError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{} (at column {})", self.message, self.column)
    }
}

impl std::error::Error for ParseError {}

#[derive(Debug, Clone, PartialEq)]
enum Tok {
    LParen,
    RParen,
    Not,
    And,
    Or,
    Op(Op),
    Number(f64),
    Text(String),
    /// A bare word: a key, or `true` / `false`.
    Word(String),
}

impl Tok {
    fn describe(&self) -> String {
        match self {
            Tok::LParen => "`(`".into(),
            Tok::RParen => "`)`".into(),
            Tok::Not => "`!`".into(),
            Tok::And => "`&&`".into(),
            Tok::Or => "`||`".into(),
            Tok::Op(o) => format!("`{}`", op_str(*o)),
            Tok::Number(n) => format!("number {n}"),
            Tok::Text(s) => format!("string {s:?}"),
            Tok::Word(w) => format!("`{w}`"),
        }
    }
}

fn op_str(o: Op) -> &'static str {
    match o {
        Op::Gt => ">",
        Op::Ge => ">=",
        Op::Lt => "<",
        Op::Le => "<=",
        Op::Eq => "==",
        Op::Ne => "!=",
    }
}

fn err<T>(column: usize, message: impl Into<String>) -> Result<T, ParseError> {
    Err(ParseError {
        column,
        message: message.into(),
    })
}

fn is_word_start(c: char) -> bool {
    c.is_ascii_alphabetic() || c == '_'
}

fn is_word_char(c: char) -> bool {
    c.is_ascii_alphanumeric() || matches!(c, '_' | '-' | '.' | ':')
}

/// Tokens with the 1-based column each starts at.
fn tokenize(src: &str) -> Result<Vec<(usize, Tok)>, ParseError> {
    let chars: Vec<char> = src.chars().collect();
    let mut toks = Vec::new();
    let mut i = 0;
    while i < chars.len() {
        let c = chars[i];
        let col = i + 1;
        let next = chars.get(i + 1).copied();
        match c {
            _ if c.is_whitespace() => i += 1,
            '(' => {
                toks.push((col, Tok::LParen));
                i += 1;
            }
            ')' => {
                toks.push((col, Tok::RParen));
                i += 1;
            }
            '&' => {
                if next != Some('&') {
                    return err(col, "expected `&&`, found a single `&`");
                }
                toks.push((col, Tok::And));
                i += 2;
            }
            '|' => {
                if next != Some('|') {
                    return err(col, "expected `||`, found a single `|`");
                }
                toks.push((col, Tok::Or));
                i += 2;
            }
            '!' if next == Some('=') => {
                toks.push((col, Tok::Op(Op::Ne)));
                i += 2;
            }
            '!' => {
                toks.push((col, Tok::Not));
                i += 1;
            }
            '=' => {
                if next != Some('=') {
                    return err(col, "expected `==`, found a single `=`");
                }
                toks.push((col, Tok::Op(Op::Eq)));
                i += 2;
            }
            '>' | '<' => {
                let (op, len) = match (c, next) {
                    ('>', Some('=')) => (Op::Ge, 2),
                    ('>', _) => (Op::Gt, 1),
                    ('<', Some('=')) => (Op::Le, 2),
                    _ => (Op::Lt, 1),
                };
                toks.push((col, Tok::Op(op)));
                i += len;
            }
            '"' | '\'' => {
                let quote = c;
                let mut s = String::new();
                i += 1;
                loop {
                    match chars.get(i) {
                        None => return err(col, "unterminated string literal"),
                        Some(&q) if q == quote => {
                            i += 1;
                            break;
                        }
                        Some('\\') => match chars.get(i + 1) {
                            Some(&e @ ('\\' | '"' | '\'')) => {
                                s.push(e);
                                i += 2;
                            }
                            _ => {
                                return err(
                                    i + 1,
                                    "invalid escape in string (use \\\\, \\\" or \\')",
                                )
                            }
                        },
                        Some(&ch) => {
                            s.push(ch);
                            i += 1;
                        }
                    }
                }
                toks.push((col, Tok::Text(s)));
            }
            _ if c.is_ascii_digit()
                || c == '.'
                || (matches!(c, '-' | '+')
                    && next.is_some_and(|n| n.is_ascii_digit() || n == '.')) =>
            {
                let start = i;
                i += 1;
                while i < chars.len() {
                    let d = chars[i];
                    let exp_sign = matches!(d, '+' | '-') && matches!(chars[i - 1], 'e' | 'E');
                    if d.is_ascii_digit() || d == '.' || d == 'e' || d == 'E' || exp_sign {
                        i += 1;
                    } else {
                        break;
                    }
                }
                let text: String = chars[start..i].iter().collect();
                match text.parse::<f64>() {
                    Ok(n) if n.is_finite() => toks.push((col, Tok::Number(n))),
                    _ => return err(col, format!("invalid number `{text}`")),
                }
            }
            _ if is_word_start(c) => {
                let start = i;
                while i < chars.len() && is_word_char(chars[i]) {
                    i += 1;
                }
                toks.push((col, Tok::Word(chars[start..i].iter().collect())));
            }
            _ => return err(col, format!("unexpected character {c:?}")),
        }
    }
    Ok(toks)
}

struct Parser {
    toks: Vec<(usize, Tok)>,
    pos: usize,
    /// Column just past the end of the input, for "unexpected end" errors.
    end_col: usize,
}

impl Parser {
    fn peek(&self) -> Option<&Tok> {
        self.toks.get(self.pos).map(|(_, t)| t)
    }

    fn col(&self) -> usize {
        self.toks.get(self.pos).map_or(self.end_col, |(c, _)| *c)
    }

    fn or(&mut self, depth: usize) -> Result<Expr, ParseError> {
        let mut lhs = self.and(depth)?;
        while self.peek() == Some(&Tok::Or) {
            self.pos += 1;
            let rhs = self.and(depth)?;
            lhs = Expr::Or(Box::new(lhs), Box::new(rhs));
        }
        Ok(lhs)
    }

    fn and(&mut self, depth: usize) -> Result<Expr, ParseError> {
        let mut lhs = self.unary(depth)?;
        while self.peek() == Some(&Tok::And) {
            self.pos += 1;
            let rhs = self.unary(depth)?;
            lhs = Expr::And(Box::new(lhs), Box::new(rhs));
        }
        Ok(lhs)
    }

    fn unary(&mut self, depth: usize) -> Result<Expr, ParseError> {
        if depth > MAX_DEPTH {
            return err(
                self.col(),
                format!("nested more than {MAX_DEPTH} levels deep"),
            );
        }
        if self.peek() == Some(&Tok::Not) {
            self.pos += 1;
            return Ok(Expr::Not(Box::new(self.unary(depth + 1)?)));
        }
        self.primary(depth)
    }

    fn primary(&mut self, depth: usize) -> Result<Expr, ParseError> {
        let col = self.col();
        let Some(tok) = self.toks.get(self.pos).map(|(_, t)| t.clone()) else {
            return err(col, "unexpected end of expression, expected a key or `(`");
        };
        match tok {
            Tok::LParen => {
                self.pos += 1;
                let inner = self.or(depth + 1)?;
                if self.peek() == Some(&Tok::RParen) {
                    self.pos += 1;
                    Ok(inner)
                } else {
                    err(self.col(), "expected `)` to close the `(` opened earlier")
                }
            }
            Tok::Word(w) => {
                if w == "true" || w == "false" {
                    return err(
                        col,
                        format!("`{w}` is a literal, not a key; keys look like `source:name`"),
                    );
                }
                if !is_valid_key(&w) {
                    return err(
                        col,
                        format!(
                            "`{w}` is not a valid key; keys look like `source:name` (e.g. `ha:sensor.temp`)"
                        ),
                    );
                }
                self.pos += 1;
                let Some(Tok::Op(op)) = self.peek().cloned() else {
                    return Ok(Expr::Truthy(w));
                };
                self.pos += 1;
                let lit = self.literal()?;
                Ok(Expr::Compare(w, op, lit))
            }
            other => err(
                col,
                format!("expected a key or `(`, found {}", other.describe()),
            ),
        }
    }

    fn literal(&mut self) -> Result<Literal, ParseError> {
        let col = self.col();
        let lit = match self.peek() {
            Some(Tok::Number(n)) => Literal::Number(*n),
            Some(Tok::Text(s)) => Literal::Text(s.clone()),
            Some(Tok::Word(w)) if w == "true" => Literal::Bool(true),
            Some(Tok::Word(w)) if w == "false" => Literal::Bool(false),
            Some(t) => {
                return err(
                    col,
                    format!(
                        "expected a number, `true`, `false` or a quoted string after the operator, found {}",
                        t.describe()
                    ),
                )
            }
            None => {
                return err(
                    col,
                    "unexpected end of expression, expected a literal after the operator",
                )
            }
        };
        self.pos += 1;
        Ok(lit)
    }
}

/// `source:name`, both parts non-empty.
pub fn is_valid_key(w: &str) -> bool {
    match w.split_once(':') {
        Some((source, name)) => !source.is_empty() && !name.is_empty(),
        None => false,
    }
}

impl Condition {
    pub fn parse(src: &str) -> Result<Condition, ParseError> {
        let toks = tokenize(src)?;
        if toks.is_empty() {
            return err(1, "condition is empty");
        }
        let mut p = Parser {
            toks,
            pos: 0,
            end_col: src.chars().count() + 1,
        };
        let expr = p.or(0)?;
        if let Some((col, tok)) = p.toks.get(p.pos) {
            let msg = match tok {
                Tok::RParen => "unmatched `)`".to_string(),
                t => format!(
                    "unexpected {}, expected `&&`, `||` or the end",
                    t.describe()
                ),
            };
            return err(*col, msg);
        }
        Ok(Condition(expr))
    }

    /// Evaluate against a snapshot. See the module docs for the handling of
    /// missing keys and type mismatches (both make a comparison `false`).
    /// Every store key the condition reads (sources use this to know what
    /// to subscribe to).
    pub fn keys(&self) -> Vec<&str> {
        fn walk<'a>(e: &'a Expr, out: &mut Vec<&'a str>) {
            match e {
                Expr::Truthy(k) | Expr::Compare(k, _, _) => out.push(k),
                Expr::Not(a) => walk(a, out),
                Expr::And(a, b) | Expr::Or(a, b) => {
                    walk(a, out);
                    walk(b, out);
                }
            }
        }
        let mut out = Vec::new();
        walk(&self.0, &mut out);
        out
    }

    pub fn eval(&self, store: &StoreSnapshot) -> bool {
        eval(&self.0, store)
    }
}

fn eval(e: &Expr, store: &StoreSnapshot) -> bool {
    match e {
        Expr::Truthy(k) => store.get(k).is_some_and(|en| match &en.value {
            Value::Bool(b) => *b,
            Value::Number { value, .. } => *value != 0.0 && !value.is_nan(),
            Value::Text(s) => !s.is_empty(),
            #[allow(unreachable_patterns)]
            _ => false,
        }),
        Expr::Compare(k, op, lit) => store.get(k).is_some_and(|en| compare(&en.value, *op, lit)),
        Expr::Not(inner) => !eval(inner, store),
        Expr::And(a, b) => eval(a, store) && eval(b, store),
        Expr::Or(a, b) => eval(a, store) || eval(b, store),
    }
}

fn compare(v: &Value, op: Op, lit: &Literal) -> bool {
    use std::cmp::Ordering;
    let ord = |o: Option<Ordering>| -> bool {
        let Some(o) = o else { return false };
        match op {
            Op::Gt => o == Ordering::Greater,
            Op::Ge => o != Ordering::Less,
            Op::Lt => o == Ordering::Less,
            Op::Le => o != Ordering::Greater,
            Op::Eq => o == Ordering::Equal,
            Op::Ne => o != Ordering::Equal,
        }
    };
    match (v, lit) {
        (Value::Number { value, .. }, Literal::Number(n)) => ord(value.partial_cmp(n)),
        (Value::Text(s), Literal::Text(t)) => ord(Some(s.as_str().cmp(t.as_str()))),
        (Value::Bool(a), Literal::Bool(b)) => match op {
            Op::Eq => a == b,
            Op::Ne => a != b,
            _ => false,
        },
        #[allow(unreachable_patterns)]
        _ => false,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn keys_lists_every_key_read() {
        let c = Condition::parse("!(ha:a > 1) && (prom:b || weather:c == 'x')").unwrap();
        assert_eq!(c.keys(), vec!["ha:a", "prom:b", "weather:c"]);
    }
    use crate::store::Store;

    fn snap(entries: &[(&str, Value)]) -> StoreSnapshot {
        let store = Store::new();
        let w = store.writer("test");
        for (k, v) in entries {
            w.set(*k, v.clone());
        }
        store.snapshot()
    }

    fn num(v: f64) -> Value {
        Value::Number {
            value: v,
            unit: None,
        }
    }

    fn text(s: &str) -> Value {
        Value::Text(s.into())
    }

    fn ev(cond: &str, s: &StoreSnapshot) -> bool {
        Condition::parse(cond)
            .unwrap_or_else(|e| panic!("{cond:?}: {e}"))
            .eval(s)
    }

    fn perr(cond: &str) -> String {
        Condition::parse(cond).expect_err(cond).to_string()
    }

    #[test]
    fn comparison_operators() {
        let s = snap(&[("prom:p", num(5.0))]);
        for (c, want) in [
            ("prom:p > 4", true),
            ("prom:p > 5", false),
            ("prom:p >= 5", true),
            ("prom:p < 5", false),
            ("prom:p < 5.5", true),
            ("prom:p <= 5", true),
            ("prom:p == 5", true),
            ("prom:p == 5.0", true),
            ("prom:p != 5", false),
            ("prom:p != 6", true),
            ("prom:p > -1", true),
            ("prom:p < 1e1", true),
            ("prom:p>4", true),
        ] {
            assert_eq!(ev(c, &s), want, "{c}");
        }
    }

    #[test]
    fn realistic_keys_tokenize() {
        let s = snap(&[
            ("ha:sensor.outside_temp", num(3.0)),
            ("weather:temp", num(-2.0)),
            ("ha:binary_sensor.front-door.state", Value::Bool(true)),
            ("ha:light.x.brightness", num(1.0)),
        ]);
        assert!(ev("ha:sensor.outside_temp > 0", &s));
        assert!(ev("weather:temp < 0", &s));
        assert!(ev("weather:temp == -2", &s));
        assert!(ev("ha:binary_sensor.front-door.state", &s));
        assert!(ev("ha:light.x.brightness>=1&&weather:temp<0", &s));
    }

    #[test]
    fn bare_key_truthiness() {
        let s = snap(&[
            ("a:t", Value::Bool(true)),
            ("a:f", Value::Bool(false)),
            ("a:n1", num(0.1)),
            ("a:n0", num(0.0)),
            ("a:nan", num(f64::NAN)),
            ("a:s1", text("x")),
            ("a:s0", text("")),
        ]);
        for (c, want) in [
            ("a:t", true),
            ("a:f", false),
            ("a:n1", true),
            ("a:n0", false),
            ("a:nan", false),
            ("a:s1", true),
            ("a:s0", false),
            ("a:missing", false),
            ("!a:missing", true),
        ] {
            assert_eq!(ev(c, &s), want, "{c}");
        }
    }

    #[test]
    fn text_and_bool_literals() {
        let s = snap(&[("ha:mode", text("home")), ("ha:on", Value::Bool(true))]);
        assert!(ev(r#"ha:mode == "home""#, &s));
        assert!(ev("ha:mode == 'home'", &s));
        assert!(ev(r#"ha:mode != "away""#, &s));
        assert!(!ev(r#"ha:mode == "away""#, &s));
        assert!(ev(r#"ha:mode < "i""#, &s));
        assert!(ev("ha:on == true", &s));
        assert!(!ev("ha:on == false", &s));
        assert!(ev("ha:on != false", &s));
        assert!(!ev("ha:on > false", &s));
        let s2 = snap(&[("ha:q", text(r#"a"b\c"#))]);
        assert!(ev(r#"ha:q == "a\"b\\c""#, &s2));
    }

    #[test]
    fn missing_keys_and_mismatches_are_false_for_every_operator() {
        let s = snap(&[
            ("a:txt", text("12")),
            ("a:num", num(12.0)),
            ("a:b", Value::Bool(true)),
            ("a:nan", num(f64::NAN)),
        ]);
        for op in [">", ">=", "<", "<=", "==", "!="] {
            for template in [
                "a:missing {} 1",
                "a:missing {} 'x'",
                "a:missing {} true",
                "a:txt {} 12",   // text vs number
                "a:num {} '12'", // number vs text
                "a:num {} true", // number vs bool
                "a:b {} 1",      // bool vs number
                "a:b {} 'true'", // bool vs text
                "a:nan {} 1",    // NaN
            ] {
                let c = template.replace("{}", op);
                assert!(!ev(&c, &s), "{c} should be false");
            }
        }
        // Negation of a false comparison is true.
        assert!(ev("!(a:missing > 1)", &s));
    }

    #[test]
    fn logic_and_precedence() {
        let s = snap(&[("a:t", Value::Bool(true)), ("a:f", Value::Bool(false))]);
        for (c, want) in [
            ("a:t && a:f", false),
            ("a:t || a:f", true),
            ("a:f || a:t && a:f", false), // && binds tighter
            ("(a:f || a:t) && a:f", false),
            ("a:f || (a:t && a:t)", true),
            ("!a:f && a:t", true),
            ("!(a:t && a:f)", true),
            ("!!a:t", true),
            ("! ( a:f )", true),
            ("!a:t || !a:f", true),
        ] {
            assert_eq!(ev(c, &s), want, "{c}");
        }
    }

    #[test]
    fn parse_errors_have_messages_and_columns() {
        for (src, want) in [
            ("", "condition is empty (at column 1)"),
            ("   ", "condition is empty (at column 1)"),
            (
                "prom:x >",
                "unexpected end of expression, expected a literal after the operator (at column 9)",
            ),
            (
                "prom:x > > 1",
                "after the operator, found `>` (at column 10)",
            ),
            ("prom:x > prom:y", "found `prom:y` (at column 10)"),
            (
                "prom:x = 1",
                "expected `==`, found a single `=` (at column 8)",
            ),
            (
                "prom:x & prom:y",
                "expected `&&`, found a single `&` (at column 8)",
            ),
            (
                "prom:x | prom:y",
                "expected `||`, found a single `|` (at column 8)",
            ),
            (
                "(prom:x",
                "expected `)` to close the `(` opened earlier (at column 8)",
            ),
            ("prom:x)", "unmatched `)` (at column 7)"),
            ("()", "expected a key or `(`, found `)` (at column 2)"),
            (
                "temp > 1",
                "`temp` is not a valid key; keys look like `source:name`",
            ),
            ("a: > 1", "`a:` is not a valid key"),
            ("true", "`true` is a literal, not a key"),
            (
                "5 > prom:x",
                "expected a key or `(`, found number 5 (at column 1)",
            ),
            (
                "a:x b:y",
                "unexpected `b:y`, expected `&&`, `||` or the end (at column 5)",
            ),
            ("a:x == 'oops", "unterminated string literal (at column 8)"),
            (r#"a:x == "a\n""#, "invalid escape in string"),
            ("a:x > 1.2.3", "invalid number `1.2.3` (at column 7)"),
            ("a:x > 1 @", "unexpected character '@' (at column 9)"),
            (
                "a:x && ",
                "unexpected end of expression, expected a key or `(` (at column 8)",
            ),
        ] {
            let got = perr(src);
            assert!(got.contains(want), "{src:?}: got {got:?}, want {want:?}");
        }
    }

    #[test]
    fn deep_nesting_is_an_error_not_a_stack_overflow() {
        let deep = format!("{}a:x{}", "(".repeat(10_000), ")".repeat(10_000));
        assert!(perr(&deep).contains("nested more than"));
        let nots = format!("{}a:x", "!".repeat(10_000));
        assert!(perr(&nots).contains("nested more than"));
        let ok = format!("{}a:x{}", "(".repeat(30), ")".repeat(30));
        assert!(Condition::parse(&ok).is_ok());
    }

    #[test]
    fn empty_snapshot_never_panics() {
        let s = snap(&[]);
        assert!(!ev("a:x > 1 || a:y == 'z' || a:w", &s));
    }
}
