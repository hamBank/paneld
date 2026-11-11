//! Named bitmap fonts usable from config.
//!
//! These are the `embedded-graphics` ISO-8859-1 mono fonts (so `°` works).
//! BDF loading for nicer pixel fonts comes later (DESIGN.md §5.4).

use embedded_graphics::mono_font::{iso_8859_1 as f, MonoFont};

/// All fonts by config name, e.g. `font = "6x10"`.
pub const FONTS: &[(&str, &MonoFont<'static>)] = &[
    ("4x6", &f::FONT_4X6),
    ("5x7", &f::FONT_5X7),
    ("5x8", &f::FONT_5X8),
    ("6x9", &f::FONT_6X9),
    ("6x10", &f::FONT_6X10),
    ("6x12", &f::FONT_6X12),
    ("6x13", &f::FONT_6X13),
    ("6x13_bold", &f::FONT_6X13_BOLD),
    ("7x13", &f::FONT_7X13),
    ("7x13_bold", &f::FONT_7X13_BOLD),
    ("7x14", &f::FONT_7X14),
    ("7x14_bold", &f::FONT_7X14_BOLD),
    ("8x13", &f::FONT_8X13),
    ("8x13_bold", &f::FONT_8X13_BOLD),
    ("9x15", &f::FONT_9X15),
    ("9x15_bold", &f::FONT_9X15_BOLD),
    ("9x18", &f::FONT_9X18),
    ("9x18_bold", &f::FONT_9X18_BOLD),
    ("10x20", &f::FONT_10X20),
];

pub const DEFAULT_FONT: &str = "6x10";

/// Look up a font by its config name.
pub fn lookup(name: &str) -> Option<&'static MonoFont<'static>> {
    FONTS.iter().find(|(n, _)| *n == name).map(|(_, f)| *f)
}

/// Comma-separated list of known font names, for error messages.
pub fn known_names() -> String {
    FONTS.iter().map(|(n, _)| *n).collect::<Vec<_>>().join(", ")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn default_font_exists() {
        assert!(lookup(DEFAULT_FONT).is_some());
        assert!(lookup("nope").is_none());
    }
}
