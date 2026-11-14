use std::collections::HashMap;
use std::time::{Duration, SystemTime};

use chrono::{DateTime, FixedOffset, Utc};
use embedded_graphics::pixelcolor::Rgb888;
use embedded_graphics::prelude::*;
use embedded_graphics::text::Alignment;

use super::{font, FrameCtx, Widget};
use crate::canvas::ZoneTarget;
use crate::config::{Align, ValueConfig};
use crate::fonts::Font;
use crate::format::NumberFormat;
use crate::icons::{self, Icon, ICON_SIZE};
use crate::store::{StoreSnapshot, Value};

/// What is shown when the value is missing, stale or not a number.
pub const PLACEHOLDER: &str = "--";
/// Colour of the placeholder (and of the icon beside it).
pub const PLACEHOLDER_COLOR: Rgb888 = Rgb888::new(0x50, 0x50, 0x50);
/// Pixels between the icon and the text.
const ICON_GAP: i32 = 3;

/// A number from the store: formatted, coloured by threshold, optionally
/// with an icon.
pub struct ValueWidget {
    key: String,
    format: NumberFormat,
    unit: String,
    font: Font,
    color: Rgb888,
    align: Align,
    icon: Option<&'static Icon>,
    /// User `condition_icons` (None = condition icons off).
    condition_icons: Option<HashMap<String, &'static Icon>>,
    show_text: bool,
    /// Sorted ascending by `above`.
    thresholds: Vec<(f64, Rgb888)>,
    stale_after: Option<Duration>,
    // Computed in `update`.
    text: String,
    text_color: Rgb888,
    /// The icon to draw this frame (the static `icon`, or a condition's).
    shown_icon: Option<&'static Icon>,
    /// False when a condition icon stands in for the text.
    draw_text: bool,
}

impl ValueWidget {
    pub fn new(cfg: &ValueConfig) -> Self {
        let mut thresholds: Vec<_> = cfg
            .thresholds
            .iter()
            .map(|t| (t.above, t.color.0))
            .collect();
        thresholds.sort_by(|a, b| a.0.total_cmp(&b.0));
        ValueWidget {
            key: cfg.key.clone(),
            // Validation rejects bad formats; fall back rather than panic anyway.
            format: NumberFormat::parse(&cfg.format)
                .or_else(|_| NumberFormat::parse("{}"))
                .expect("default format parses"),
            unit: cfg.unit.clone().unwrap_or_default(),
            font: font(&cfg.font, &cfg.face, cfg.font_scale),
            color: cfg.color.0,
            align: cfg.align,
            icon: cfg.icon.as_deref().and_then(icons::lookup),
            condition_icons: cfg.condition_icons.as_ref().map(|m| {
                m.iter()
                    .filter_map(|(c, i)| icons::lookup(i).map(|i| (c.clone(), i)))
                    .collect()
            }),
            show_text: cfg.show_text,
            thresholds,
            stale_after: cfg
                .stale_after_s
                .map(|s| Duration::try_from_secs_f64(s).unwrap_or(Duration::MAX)),
            text: PLACEHOLDER.into(),
            text_color: PLACEHOLDER_COLOR,
            shown_icon: cfg.icon.as_deref().and_then(icons::lookup),
            draw_text: true,
        }
    }

    /// Colour for `v`: the highest threshold it is strictly above, else the base colour.
    fn color_for(&self, v: f64) -> Rgb888 {
        self.thresholds
            .iter()
            .rev()
            .find(|(above, _)| v > *above)
            .map_or(self.color, |(_, c)| *c)
    }
}

/// True if a value stamped `updated_at` is older than `after` at `now`.
/// Timestamps in the future count as fresh.
fn is_stale(now: DateTime<FixedOffset>, updated_at: SystemTime, after: Duration) -> bool {
    let updated: DateTime<Utc> = updated_at.into();
    now.signed_duration_since(updated)
        .to_std()
        .is_ok_and(|age| age > after)
}

impl Widget for ValueWidget {
    fn update(&mut self, ctx: &FrameCtx, store: &StoreSnapshot) {
        self.shown_icon = self.icon;
        self.draw_text = true;
        let entry = store.get(&self.key).filter(|e| {
            !self
                .stale_after
                .is_some_and(|after| is_stale(ctx.now, e.updated_at, after))
        });
        match entry.map(|e| &e.value) {
            Some(Value::Number { value, .. }) if value.is_finite() => {
                let v = *value;
                self.text = format!("{}{}", self.format.format(v), self.unit);
                self.text_color = self.color_for(v);
            }
            Some(Value::Text(t)) if self.condition_icons.is_some() => {
                self.text.clear();
                self.text.push_str(t);
                self.text_color = self.color;
                let found = self
                    .condition_icons
                    .as_ref()
                    .and_then(|m| m.get(t.as_str()).copied())
                    .or_else(|| icons::default_condition_icon(t));
                if let Some(icon) = found {
                    self.shown_icon = Some(icon);
                    self.draw_text = self.show_text;
                }
                // No icon for this text: show the text even with
                // show_text = false, rather than a blank zone.
            }
            _ => {
                self.text.clear();
                self.text.push_str(PLACEHOLDER);
                self.text_color = PLACEHOLDER_COLOR;
            }
        }
    }

    fn draw(&self, target: &mut ZoneTarget<'_>) {
        let (w, h) = (target.width() as i32, target.height() as i32);
        let text_w = if self.draw_text {
            self.font.text_width(&self.text) as i32
        } else {
            0
        };
        let icon_w = self.shown_icon.map_or(0, |_| {
            ICON_SIZE as i32 + if self.draw_text { ICON_GAP } else { 0 }
        });
        let group_w = icon_w + text_w;
        // Too wide to fit: keep the left edge, let the zone clip the rest.
        let x0 = match self.align {
            Align::Left => 0,
            Align::Center => (w - group_w) / 2,
            Align::Right => w - group_w,
        }
        .max(0);

        if let Some(icon) = self.shown_icon {
            let origin = Point::new(x0, (h - ICON_SIZE as i32) / 2);
            icon.draw(target, origin, self.text_color);
        }
        if self.draw_text {
            let _ = self.font.draw(
                target,
                &self.text,
                x0 + icon_w,
                h / 2,
                Alignment::Left,
                self.text_color,
            );
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn now() -> DateTime<FixedOffset> {
        DateTime::parse_from_rfc3339("2026-01-02T13:37:42+01:00").unwrap()
    }

    #[test]
    fn staleness_is_strictly_older_than_the_limit() {
        let limit = Duration::from_secs(60);
        let at = |age: u64| SystemTime::from(now()) - Duration::from_secs(age);
        assert!(!is_stale(now(), at(0), limit));
        assert!(!is_stale(now(), at(60), limit));
        assert!(is_stale(now(), at(61), limit));
    }

    #[test]
    fn future_timestamps_are_fresh() {
        let future = SystemTime::from(now()) + Duration::from_secs(5);
        assert!(!is_stale(now(), future, Duration::from_secs(1)));
    }
}
