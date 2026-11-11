//! `#rrggbb` colours as they appear in the config file.

use std::fmt;
use std::str::FromStr;

use embedded_graphics::pixelcolor::{Rgb888, RgbColor};
use serde::{Deserialize, Deserializer};

/// An RGB colour parsed from `#rrggbb` (the leading `#` is required).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Color(pub Rgb888);

impl Color {
    pub const WHITE: Color = Color(Rgb888::WHITE);
    pub const BLACK: Color = Color(Rgb888::BLACK);
}

impl FromStr for Color {
    type Err = String;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        let hex = s
            .strip_prefix('#')
            .ok_or_else(|| format!("invalid colour {s:?}: expected \"#rrggbb\""))?;
        if hex.len() != 6 || !hex.bytes().all(|b| b.is_ascii_hexdigit()) {
            return Err(format!("invalid colour {s:?}: expected \"#rrggbb\""));
        }
        let channel = |i: usize| u8::from_str_radix(&hex[i..i + 2], 16).expect("validated hex");
        Ok(Color(Rgb888::new(channel(0), channel(2), channel(4))))
    }
}

impl fmt::Display for Color {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "#{:02x}{:02x}{:02x}", self.0.r(), self.0.g(), self.0.b())
    }
}

impl<'de> Deserialize<'de> for Color {
    fn deserialize<D: Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
        let s = String::deserialize(d)?;
        s.parse().map_err(serde::de::Error::custom)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_hex() {
        assert_eq!("#ffaa00".parse(), Ok(Color(Rgb888::new(255, 170, 0))));
        assert_eq!("#FFAA00".parse(), Ok(Color(Rgb888::new(255, 170, 0))));
    }

    #[test]
    fn rejects_bad_input() {
        for bad in ["ffaa00", "#ffaa0", "#ffaa000", "#gg0000", "", "#"] {
            assert!(bad.parse::<Color>().is_err(), "{bad:?} should fail");
        }
    }

    #[test]
    fn round_trips_display() {
        assert_eq!("#0a0b0c".parse::<Color>().unwrap().to_string(), "#0a0b0c");
    }
}
