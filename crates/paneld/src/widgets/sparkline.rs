use std::sync::Arc;
use std::time::SystemTime;

use embedded_graphics::pixelcolor::Rgb888;
use embedded_graphics::prelude::*;
use embedded_graphics::primitives::Line;

use super::{FrameCtx, Widget};
use crate::canvas::ZoneTarget;
use crate::config::{Fill, SparklineConfig};
use crate::store::{StoreSnapshot, Value};

/// Share of the line colour used for `fill = true`, out of 256.
const AUTO_FILL_LEVEL: u16 = 64;
/// Auto-scale headroom above and below the data, as a fraction of its range.
const AUTO_MARGIN: f64 = 0.1;

/// A line graph of a `Series`, the newest point at the right edge.
///
/// Missing key, a non-series value or an empty series: draws nothing.
pub struct Sparkline {
    key: String,
    color: Rgb888,
    fill: Option<Rgb888>,
    min: Option<f64>,
    max: Option<f64>,
    points: Option<usize>,
    series: Option<Arc<[(SystemTime, f64)]>>,
}

impl Sparkline {
    pub fn new(cfg: &SparklineConfig) -> Self {
        let color = cfg.color.0;
        let fill = match cfg.fill {
            None | Some(Fill::Enabled(false)) => None,
            Some(Fill::Enabled(true)) => Some(scale(color, AUTO_FILL_LEVEL)),
            Some(Fill::Color(c)) => Some(c.0),
        };
        Sparkline {
            key: cfg.key.clone(),
            color,
            fill,
            min: cfg.min,
            max: cfg.max,
            points: cfg.points.map(|p| p as usize),
            series: None,
        }
    }
}

fn scale(c: Rgb888, level: u16) -> Rgb888 {
    let s = |v: u8| (u16::from(v) * level / 256) as u8;
    Rgb888::new(s(c.r()), s(c.g()), s(c.b()))
}

/// The value range to plot `values` on: fixed ends where configured, else
/// the data's range with a margin. A flat range is widened so the line sits
/// in the middle rather than dividing by zero.
fn scale_range(values: &[f64], min: Option<f64>, max: Option<f64>) -> (f64, f64) {
    let (dmin, dmax) = values
        .iter()
        .fold((f64::INFINITY, f64::NEG_INFINITY), |(lo, hi), &v| {
            (lo.min(v), hi.max(v))
        });
    let margin = (dmax - dmin) * AUTO_MARGIN;
    let lo = min.unwrap_or(dmin - margin);
    let hi = max.unwrap_or(dmax + margin);
    if hi - lo > f64::EPSILON * lo.abs().max(hi.abs()).max(1.0) {
        (lo, hi)
    } else {
        // Flat (or inverted by a one-sided fixed bound): centre on the data.
        let mid = if min.is_some() && max.is_none() {
            lo + 0.5
        } else if max.is_some() && min.is_none() {
            hi - 0.5
        } else {
            (lo + hi) / 2.0
        };
        (mid - 1.0, mid + 1.0)
    }
}

/// Pixel positions for the newest `n` of `values` across a `w × h` zone.
/// With fewer values than `n` the graph grows in from the right edge.
fn layout(values: &[f64], n: usize, w: i32, h: i32, range: (f64, f64)) -> Vec<Point> {
    let shown = &values[values.len().saturating_sub(n)..];
    let step = f64::from(w - 1) / (n - 1) as f64;
    let (lo, hi) = range;
    shown
        .iter()
        .enumerate()
        .map(|(i, &v)| {
            let x = f64::from(w - 1) - (shown.len() - 1 - i) as f64 * step;
            let frac = ((v - lo) / (hi - lo)).clamp(0.0, 1.0);
            let y = f64::from(h - 1) * (1.0 - frac);
            Point::new(x.round() as i32, y.round() as i32)
        })
        .collect()
}

impl Widget for Sparkline {
    fn update(&mut self, _ctx: &FrameCtx, store: &StoreSnapshot) {
        // One Arc clone; the series itself is never copied.
        self.series = match store.get(&self.key).map(|e| &e.value) {
            Some(Value::Series(s)) => Some(s.clone()),
            _ => None,
        };
    }

    fn draw(&self, target: &mut ZoneTarget<'_>) {
        let (w, h) = (target.width() as i32, target.height() as i32);
        let Some(series) = &self.series else { return };
        // Non-finite samples are dropped rather than plotted.
        let values: Vec<f64> = series
            .iter()
            .map(|p| p.1)
            .filter(|v| v.is_finite())
            .collect();
        if values.is_empty() || w < 1 || h < 1 {
            return;
        }
        let n = self.points.unwrap_or(w as usize).max(2);
        let shown = &values[values.len().saturating_sub(n)..];
        let range = scale_range(shown, self.min, self.max);
        let pts = layout(&values, n, w, h, range);

        if let Some(fill) = self.fill {
            // Per column, the highest line pixel; fill everything below it.
            let mut top = vec![i32::MAX; w as usize];
            let mut note = |p: Point| {
                if let Some(t) = top.get_mut(p.x as usize).filter(|_| p.x >= 0) {
                    *t = (*t).min(p.y);
                }
            };
            if let [only] = pts.as_slice() {
                note(*only);
            }
            for pair in pts.windows(2) {
                Line::new(pair[0], pair[1]).points().for_each(&mut note);
            }
            let fill_px = top.iter().enumerate().flat_map(|(x, &t)| {
                (t.saturating_add(1)..h).map(move |y| Pixel(Point::new(x as i32, y), fill))
            });
            let _ = target.draw_iter(fill_px);
        }

        match pts.as_slice() {
            [only] => {
                let _ = target.draw_iter([Pixel(*only, self.color)]);
            }
            _ => {
                for pair in pts.windows(2) {
                    let line = Line::new(pair[0], pair[1])
                        .points()
                        .map(|p| Pixel(p, self.color));
                    let _ = target.draw_iter(line);
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn auto_scale_adds_margin() {
        let (lo, hi) = scale_range(&[10.0, 20.0], None, None);
        assert!((lo - 9.0).abs() < 1e-9 && (hi - 21.0).abs() < 1e-9);
    }

    #[test]
    fn flat_data_does_not_divide_by_zero() {
        let (lo, hi) = scale_range(&[5.0, 5.0, 5.0], None, None);
        assert!(hi > lo);
        assert!((lo + hi) / 2.0 - 5.0 < 1e-9);
        let (lo, hi) = scale_range(&[0.0], None, None);
        assert!(hi > lo);
        // One fixed bound that collides with flat data is still a valid range.
        let (lo, hi) = scale_range(&[5.0, 5.0], Some(5.0), None);
        assert!(hi > lo);
        let (lo, hi) = scale_range(&[5.0, 5.0], None, Some(5.0));
        assert!(hi > lo);
    }

    #[test]
    fn fixed_scale_wins_over_data() {
        assert_eq!(
            scale_range(&[1.0, 2.0], Some(0.0), Some(100.0)),
            (0.0, 100.0)
        );
        let (lo, hi) = scale_range(&[10.0, 20.0], Some(0.0), None);
        assert_eq!(lo, 0.0);
        assert!((hi - 21.0).abs() < 1e-9);
    }

    #[test]
    fn newest_point_is_at_the_right_edge() {
        let pts = layout(&[0.0, 1.0, 2.0, 3.0], 4, 10, 5, (0.0, 3.0));
        assert_eq!(pts.last().unwrap().x, 9);
        assert_eq!(pts[0], Point::new(0, 4));
        assert_eq!(pts[3], Point::new(9, 0));
        // More data than points: only the newest are used.
        let pts = layout(&[9.0, 9.0, 0.0, 3.0], 2, 10, 5, (0.0, 3.0));
        assert_eq!(pts, vec![Point::new(0, 4), Point::new(9, 0)]);
        // Fewer than asked for: grows in from the right.
        let pts = layout(&[0.0, 3.0], 4, 10, 5, (0.0, 3.0));
        assert_eq!(pts, vec![Point::new(6, 4), Point::new(9, 0)]);
    }
}
