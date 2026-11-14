//! Minimal number formatting for strings like `"{:.1}°"` (DESIGN.md §5.4).
//!
//! A format string is literal text with exactly one placeholder:
//!
//! | placeholder            | meaning                                          |
//! |------------------------|--------------------------------------------------|
//! | `{}`                   | the number as Rust prints an `f64` (`21`, `21.5`) |
//! | `{:.N}`                | `N` decimals (`{:.1}` gives `21.5`)              |
//! | `{:>W}` `{:<W}` `{:W}` | padded to width `W` (right-aligned unless `<`)   |
//! | `{:>W.N}` `{:<W.N}`    | both                                             |
//!
//! `{{` and `}}` are literal braces. Anything else inside braces is rejected
//! with an error naming the supported syntax. [`Spec`] is public so that the
//! templating (`{ha:sensor.x:.1}`, see `template.rs`) reuses it for the part after the key.

use std::str::FromStr;

pub const MAX_WIDTH: usize = 32;
pub const MAX_PRECISION: usize = 12;

const SUPPORTED: &str = "supported: {}, {:.N}, {:>W}, {:<W}, {:>W.N}, {:<W.N}";

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Align {
    Left,
    Right,
}

/// The part of a placeholder after the `:`, e.g. `>6.1`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Spec {
    pub align: Align,
    pub width: usize,
    pub precision: Option<usize>,
}

impl Default for Spec {
    /// Equivalent to `{}`.
    fn default() -> Self {
        Spec {
            align: Align::Right,
            width: 0,
            precision: None,
        }
    }
}

impl Spec {
    /// Parse `[<|>][W][.N]`. An empty string is `{}`.
    pub fn parse(s: &str) -> Result<Spec, String> {
        let bad = |why: &str| format!("unsupported format spec {s:?}: {why} ({SUPPORTED})");
        let mut rest = s;
        let mut align = Align::Right;
        if let Some(r) = rest.strip_prefix('<') {
            align = Align::Left;
            rest = r;
        } else if let Some(r) = rest.strip_prefix('>') {
            rest = r;
        }
        let (width_s, prec_s) = match rest.split_once('.') {
            Some((w, p)) => (w, Some(p)),
            None => (rest, None),
        };
        let digits = |t: &str| !t.is_empty() && t.bytes().all(|b| b.is_ascii_digit());
        let width = if width_s.is_empty() {
            0
        } else if digits(width_s) {
            width_s
                .parse::<usize>()
                .map_err(|_| bad("width too large"))?
        } else {
            return Err(bad("expected a width"));
        };
        if width > MAX_WIDTH {
            return Err(bad(&format!("width must be at most {MAX_WIDTH}")));
        }
        let precision = match prec_s {
            None => None,
            Some(p) if digits(p) => {
                let n = p.parse::<usize>().map_err(|_| bad("precision too large"))?;
                if n > MAX_PRECISION {
                    return Err(bad(&format!("precision must be at most {MAX_PRECISION}")));
                }
                Some(n)
            }
            Some(_) => return Err(bad("expected a precision after '.'")),
        };
        Ok(Spec {
            align,
            width,
            precision,
        })
    }

    /// Format `v`. A negative value that rounds to zero prints without a sign
    /// (`-0.04` with `.1` is `0.0`, not `-0.0`).
    pub fn format(&self, v: f64) -> String {
        let mut s = String::new();
        self.write_number(v, &mut s);
        s
    }

    /// Append the formatted `v` to `out` (no allocation beyond `out` growing).
    pub fn write_number(&self, v: f64, out: &mut String) {
        use std::fmt::Write;
        let start = out.len();
        let _ = match self.precision {
            Some(p) => write!(out, "{v:.p$}"),
            None => write!(out, "{v}"),
        };
        let body = &out[start..];
        if body.starts_with('-') && body[1..].bytes().all(|b| b == b'0' || b == b'.') {
            out.remove(start);
        }
        self.pad(start, out);
    }

    /// Append `s` to `out`, truncated to the precision (as a character count)
    /// and padded to the width. For text and booleans in templates.
    pub fn write_text(&self, s: &str, out: &mut String) {
        let start = out.len();
        match self.precision {
            Some(p) => out.extend(s.chars().take(p)),
            None => out.push_str(s),
        }
        self.pad(start, out);
    }

    /// Pad what was written since `start` to the width.
    pub fn pad(&self, start: usize, out: &mut String) {
        let len = out[start..].chars().count();
        let fill = self.width.saturating_sub(len);
        match self.align {
            Align::Right => (0..fill).for_each(|_| out.insert(start, ' ')),
            Align::Left => out.extend(std::iter::repeat_n(' ', fill)),
        }
    }
}

/// A parsed format string: `prefix`, one number, `suffix`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct NumberFormat {
    prefix: String,
    spec: Spec,
    suffix: String,
}

impl NumberFormat {
    pub fn parse(s: &str) -> Result<NumberFormat, String> {
        let mut prefix = String::new();
        let mut suffix = String::new();
        let mut spec = None;
        let mut chars = s.chars().peekable();
        while let Some(c) = chars.next() {
            let out = if spec.is_none() {
                &mut prefix
            } else {
                &mut suffix
            };
            match c {
                '{' if chars.peek() == Some(&'{') => {
                    chars.next();
                    out.push('{');
                }
                '}' if chars.peek() == Some(&'}') => {
                    chars.next();
                    out.push('}');
                }
                '}' => {
                    return Err(format!(
                        "format {s:?}: unmatched '}}' (use '}}}}' for a literal)"
                    ))
                }
                '{' => {
                    let mut inner = String::new();
                    loop {
                        match chars.next() {
                            Some('}') => break,
                            Some(c) => inner.push(c),
                            None => {
                                return Err(format!(
                                    "format {s:?}: unclosed '{{' (use '{{{{' for a literal)"
                                ))
                            }
                        }
                    }
                    if spec.is_some() {
                        return Err(format!(
                            "format {s:?}: only one placeholder is allowed per value"
                        ));
                    }
                    let parsed = match inner.strip_prefix(':') {
                        None if inner.is_empty() => Spec::default(),
                        None => {
                            return Err(format!(
                            "format {s:?}: unsupported placeholder \"{{{inner}}}\" ({SUPPORTED})"
                        ))
                        }
                        Some(sp) => Spec::parse(sp).map_err(|e| format!("format {s:?}: {e}"))?,
                    };
                    spec = Some(parsed);
                }
                c => out.push(c),
            }
        }
        let spec = spec.ok_or_else(|| {
            format!("format {s:?}: needs a placeholder such as \"{{}}\" or \"{{:.1}}\"")
        })?;
        Ok(NumberFormat {
            prefix,
            spec,
            suffix,
        })
    }

    pub fn format(&self, v: f64) -> String {
        format!("{}{}{}", self.prefix, self.spec.format(v), self.suffix)
    }
}

impl FromStr for NumberFormat {
    type Err = String;

    fn from_str(s: &str) -> Result<Self, String> {
        NumberFormat::parse(s)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn fmt(f: &str, v: f64) -> String {
        NumberFormat::parse(f).unwrap().format(v)
    }

    #[test]
    fn plain_and_precision() {
        assert_eq!(fmt("{}", 21.5), "21.5");
        assert_eq!(fmt("{}", 21.0), "21");
        assert_eq!(fmt("{:.1}°", 21.54), "21.5°");
        assert_eq!(fmt("{:.0} W", 1234.4), "1234 W");
        assert_eq!(fmt("T={:.2}!", -3.15159), "T=-3.15!");
    }

    #[test]
    fn width_and_alignment() {
        assert_eq!(fmt("[{:>6.1}]", 3.15159), "[   3.2]");
        assert_eq!(fmt("[{:<6.1}]", 3.15159), "[3.2   ]");
        assert_eq!(fmt("[{:6}]", 42.0), "[    42]");
        assert_eq!(fmt("[{:<4}]", 7.0), "[7   ]");
        assert_eq!(fmt("[{:>2}]", 12345.0), "[12345]");
    }

    #[test]
    fn negative_zero_loses_its_sign() {
        assert_eq!(fmt("{:.1}", -0.04), "0.0");
        assert_eq!(fmt("{:.0}", -0.4), "0");
        assert_eq!(fmt("{:.1}", -0.06), "-0.1");
        assert_eq!(fmt("{}", -0.0), "0");
    }

    #[test]
    fn escaped_braces() {
        assert_eq!(fmt("{{{}}}", 1.0), "{1}");
        assert_eq!(fmt("{:.1}}}", 1.0), "1.0}");
    }

    #[test]
    fn rejects_bad_formats() {
        for (bad, want) in [
            ("", "needs a placeholder"),
            ("no placeholder", "needs a placeholder"),
            ("{} {}", "only one placeholder"),
            ("{", "unclosed"),
            ("}", "unmatched"),
            ("{x}", "unsupported placeholder"),
            ("{:x}", "unsupported format spec"),
            ("{:+.1}", "unsupported format spec"),
            ("{:.}", "expected a precision"),
            ("{:.1f}", "expected a precision"),
            ("{:^6}", "unsupported format spec"),
            ("{:>99}", "width must be at most 32"),
            ("{:.99}", "precision must be at most 12"),
            ("{:e}", "unsupported format spec"),
        ] {
            let e = NumberFormat::parse(bad).expect_err(bad);
            assert!(e.contains(want), "{bad:?}: {e}");
        }
    }

    #[test]
    fn spec_is_reusable_on_its_own() {
        assert_eq!(Spec::parse(".1").unwrap().format(2.25), "2.2");
        assert_eq!(Spec::parse("").unwrap(), Spec::default());
    }
}
