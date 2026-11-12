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
    /// Sorted ascending by `above`.
    thresholds: Vec<(f64, Rgb888)>,
    stale_after: Option<Duration>,
    // Computed in `update`.
    text: String,
    text_color: Rgb888,
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
            thresholds,
            stale_after: cfg
                .stale_after_s
                .map(|s| Duration::try_from_secs_f64(s).unwrap_or(Duration::MAX)),
            text: PLACEHOLDER.into(),
            text_color: PLACEHOLDER_COLOR,
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
        let shown = store.get(&self.key).and_then(|e| match &e.value {
            Value::Number { value, .. } if value.is_finite() => {
                let stale = self
                    .stale_after
                    .is_some_and(|after| is_stale(ctx.now, e.updated_at, after));
                (!stale).then_some(*value)
            }
            _ => None,
        });
        match shown {
            Some(v) => {
                self.text = format!("{}{}", self.format.format(v), self.unit);
                self.text_color = self.color_for(v);
            }
            None => {
                self.text = PLACEHOLDER.into();
                self.text_color = PLACEHOLDER_COLOR;
            }
        }
    }

    fn draw(&self, target: &mut ZoneTarget<'_>) {
        let (w, h) = (target.width() as i32, target.height() as i32);
        let text_w = self.font.text_width(&self.text) as i32;
        let icon_w = self.icon.map_or(0, |_| ICON_SIZE as i32 + ICON_GAP);
        let group_w = icon_w + text_w;
        // Too wide to fit: keep the left edge, let the zone clip the rest.
        let x0 = match self.align {
            Align::Left => 0,
            Align::Center => (w - group_w) / 2,
            Align::Right => w - group_w,
        }
        .max(0);

        if let Some(icon) = self.icon {
            let origin = Point::new(x0, (h - ICON_SIZE as i32) / 2);
            icon.draw(target, origin, self.text_color);
        }
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
