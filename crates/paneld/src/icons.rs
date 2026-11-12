//! Built-in 12x12 one-bit icons for the `value` widget.
//!
//! Each icon is 12 rows of 12 characters: `#` is a lit pixel, `.` is off.
//! The art is const data, so there are no assets to load or embed.

use embedded_graphics::pixelcolor::Rgb888;
use embedded_graphics::prelude::*;

pub const ICON_SIZE: u32 = 12;

pub struct Icon {
    pub name: &'static str,
    rows: [&'static str; ICON_SIZE as usize],
}

impl Icon {
    /// Whether the pixel at `(x, y)` is lit. Outside the icon: false.
    pub fn lit(&self, x: u32, y: u32) -> bool {
        self.rows
            .get(y as usize)
            .and_then(|r| r.as_bytes().get(x as usize))
            .is_some_and(|b| *b == b'#')
    }

    /// Draw in one colour with its top-left corner at `origin`.
    pub fn draw<D: DrawTarget<Color = Rgb888>>(
        &self,
        target: &mut D,
        origin: Point,
        color: Rgb888,
    ) {
        let lit = (0..ICON_SIZE)
            .flat_map(|y| (0..ICON_SIZE).map(move |x| (x, y)))
            .filter(|&(x, y)| self.lit(x, y))
            .map(|(x, y)| Pixel(origin + Point::new(x as i32, y as i32), color));
        let _ = target.draw_iter(lit);
    }
}

pub const ICONS: &[Icon] = &[
    Icon {
        name: "thermometer",
        rows: [
            "....####....",
            "....#..#....",
            "....#..#....",
            "....#..#....",
            "....#..#....",
            "....####....",
            "....####....",
            "...######...",
            "..########..",
            "..########..",
            "...######...",
            "....####....",
        ],
    },
    Icon {
        name: "droplet",
        rows: [
            ".....##.....",
            ".....##.....",
            "....####....",
            "....####....",
            "...######...",
            "..########..",
            "..########..",
            ".##########.",
            ".##########.",
            "..########..",
            "...######...",
            "....####....",
        ],
    },
    Icon {
        name: "bolt",
        rows: [
            ".......###..",
            "......###...",
            ".....###....",
            "....###.....",
            "...#######..",
            "..#######...",
            ".....###....",
            "....###.....",
            "...###......",
            "...##.......",
            "..##........",
            "..#.........",
        ],
    },
    Icon {
        name: "sun",
        rows: [
            ".....##.....",
            "..#..##..#..",
            "...#....#...",
            "....####....",
            ".#.######.#.",
            "##.######.##",
            "##.######.##",
            ".#.######.#.",
            "....####....",
            "...#....#...",
            "..#..##..#..",
            ".....##.....",
        ],
    },
    Icon {
        name: "cloud",
        rows: [
            "............",
            "............",
            "....###.....",
            "...#####.##.",
            ".##########.",
            "############",
            "############",
            "############",
            ".##########.",
            "............",
            "............",
            "............",
        ],
    },
    Icon {
        name: "rain",
        rows: [
            "....###.....",
            "...#####.##.",
            ".##########.",
            "############",
            "############",
            ".##########.",
            "............",
            "..#...#...#.",
            "..#...#...#.",
            ".#...#...#..",
            ".#...#...#..",
            "............",
        ],
    },
    Icon {
        name: "home",
        rows: [
            ".....##.....",
            "....####....",
            "...######...",
            "..########..",
            ".##########.",
            "############",
            "..########..",
            "..###..###..",
            "..###..###..",
            "..###..###..",
            "..###..###..",
            "............",
        ],
    },
    Icon {
        name: "wind",
        rows: [
            "............",
            ".....###....",
            "....#...#...",
            ".........#..",
            ".#########..",
            "............",
            "###########.",
            "............",
            ".#########..",
            ".........#..",
            "....#...#...",
            ".....###....",
        ],
    },
];

/// Look up an icon by its config name.
pub fn lookup(name: &str) -> Option<&'static Icon> {
    ICONS.iter().find(|i| i.name == name)
}

/// Comma-separated list of known icon names, for error messages.
pub fn known_names() -> String {
    ICONS.iter().map(|i| i.name).collect::<Vec<_>>().join(", ")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn icons_are_well_formed() {
        for icon in ICONS {
            for row in icon.rows {
                assert_eq!(row.len(), ICON_SIZE as usize, "{}: {row:?}", icon.name);
                assert!(row.bytes().all(|b| b == b'#' || b == b'.'), "{}", icon.name);
            }
            assert!(
                (0..ICON_SIZE).any(|y| (0..ICON_SIZE).any(|x| icon.lit(x, y))),
                "{} is blank",
                icon.name
            );
        }
    }

    #[test]
    fn names_are_unique_and_found() {
        for icon in ICONS {
            assert_eq!(ICONS.iter().filter(|i| i.name == icon.name).count(), 1);
            assert!(lookup(icon.name).is_some());
        }
        assert!(lookup("nope").is_none());
        assert!(known_names().starts_with("thermometer, droplet"));
    }
}
