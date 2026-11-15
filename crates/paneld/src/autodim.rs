//! Quiet hours and auto-dim (DESIGN.md section 5.2).
//!
//! A tokio task works out an auto-dim factor (0-255, 255 = no dimming) and
//! sends it to the render loop as [`Command::SetDim`] when it changes. The
//! factor is then combined with the user brightness into the
//! [`crate::control::OutputLevel`] the sinks follow.
//!
//! # Modes and how they combine
//!
//! Up to three inputs, each optional:
//!
//! - **quiet hours** (`[quiet_hours]`): a fixed dim level between `start` and
//!   `end` local time (the window may wrap midnight), 255 outside it.
//! - **sun** (`[auto_dim.sun]`): 255 by day, `night` after dusk, with a linear
//!   ramp of `ramp_min` minutes after sunset and before sunrise.
//! - **lux** (`[auto_dim.lux]`): a Home Assistant (or any) lux sensor in the
//!   store, mapped through a piecewise-linear `curve`. When the reading is
//!   missing or older than `stale_after_s` the lux input is ignored, so the
//!   schedule inputs alone decide.
//!
//! The result is the **minimum** of the inputs that apply: the panel is as
//! dim as the dimmest input asks for.

use std::time::{Duration, SystemTime};

use chrono::{DateTime, FixedOffset, NaiveDate, NaiveTime, Timelike, Utc};
use serde::Deserialize;

use crate::control::{Command, Control};
use crate::store::{Store, StoreSnapshot, Value};

/// How often the task re-evaluates.
pub const TICK: Duration = Duration::from_secs(30);
/// ... and when a lux sensor is configured, since the store has no change
/// notification to wake on.
pub const LUX_TICK: Duration = Duration::from_secs(5);

// ---------------------------------------------------------------- config

/// `[quiet_hours]`
#[derive(Debug, Clone, Deserialize, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct QuietHoursConfig {
    /// Local time `HH:MM`.
    pub start: String,
    /// Local time `HH:MM`; earlier than `start` means the window wraps midnight.
    pub end: String,
    /// Dim level 0-255 during quiet hours.
    pub brightness: u8,
}

/// `[auto_dim]`
#[derive(Debug, Clone, Default, Deserialize, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct AutoDimConfig {
    pub sun: Option<SunConfig>,
    pub lux: Option<LuxConfig>,
}

/// `[auto_dim.sun]`
#[derive(Debug, Clone, Deserialize, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct SunConfig {
    pub location: Location,
    /// Dim level 0-255 at night.
    #[serde(default = "default_night")]
    pub night: u8,
    /// Minutes of ramp after sunset and before sunrise.
    #[serde(default = "default_ramp_min")]
    pub ramp_min: f64,
}

fn default_night() -> u8 {
    40
}
fn default_ramp_min() -> f64 {
    30.0
}

#[derive(Debug, Clone, Copy, Deserialize, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct Location {
    pub lat: f64,
    pub lon: f64,
}

/// `[auto_dim.lux]`
#[derive(Debug, Clone, Deserialize, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct LuxConfig {
    /// Store key of the lux reading, e.g. `ha:sensor.lux`.
    pub key: String,
    /// `[lux, dim]` points, lux strictly increasing; linear in between,
    /// flat beyond the ends.
    pub curve: Vec<[f64; 2]>,
    /// A reading older than this is ignored.
    #[serde(default = "default_stale_after_s")]
    pub stale_after_s: f64,
}

fn default_stale_after_s() -> f64 {
    600.0
}

/// Config problems, for [`crate::config::Config::validate`].
pub fn validate(q: Option<&QuietHoursConfig>, a: Option<&AutoDimConfig>) -> Vec<String> {
    let mut errs = Vec::new();
    if let Some(q) = q {
        let (s, e) = (parse_hhmm(&q.start), parse_hhmm(&q.end));
        for (name, r, v) in [("start", &s, &q.start), ("end", &e, &q.end)] {
            if r.is_none() {
                errs.push(format!(
                    "quiet_hours.{name}: expected HH:MM (24 h), got {v:?}"
                ));
            }
        }
        if s.is_some() && s == e {
            errs.push("quiet_hours: start and end must differ".into());
        }
    }
    if let Some(sun) = a.and_then(|a| a.sun.as_ref()) {
        let l = sun.location;
        if !(l.lat.is_finite() && (-90.0..=90.0).contains(&l.lat)) {
            errs.push(format!(
                "auto_dim.sun.location.lat: -90..=90 (got {})",
                l.lat
            ));
        }
        if !(l.lon.is_finite() && (-180.0..=180.0).contains(&l.lon)) {
            errs.push(format!(
                "auto_dim.sun.location.lon: -180..=180 (got {})",
                l.lon
            ));
        }
        if !(sun.ramp_min.is_finite() && (0.0..=180.0).contains(&sun.ramp_min)) {
            errs.push(format!(
                "auto_dim.sun.ramp_min: 0..=180 (got {})",
                sun.ramp_min
            ));
        }
    }
    if let Some(lux) = a.and_then(|a| a.lux.as_ref()) {
        if lux.key.trim().is_empty() {
            errs.push("auto_dim.lux.key must not be empty".into());
        }
        if lux.curve.is_empty() {
            errs.push("auto_dim.lux.curve needs at least one [lux, dim] point".into());
        }
        for (i, [x, y]) in lux.curve.iter().enumerate() {
            if !x.is_finite() || !(0.0..=255.0).contains(y) {
                errs.push(format!(
                    "auto_dim.lux.curve[{i}]: lux must be finite and dim 0..=255"
                ));
            }
        }
        if lux.curve.windows(2).any(|w| w[0][0] >= w[1][0]) {
            errs.push("auto_dim.lux.curve: lux values must be strictly increasing".into());
        }
        if !(lux.stale_after_s.is_finite() && lux.stale_after_s > 0.0) {
            errs.push("auto_dim.lux.stale_after_s must be > 0".into());
        }
    }
    errs
}

fn parse_hhmm(s: &str) -> Option<NaiveTime> {
    NaiveTime::parse_from_str(s.trim(), "%H:%M").ok()
}

// ------------------------------------------------------------ pure logic

/// Whether `t` is inside `[start, end)`, wrapping midnight when `end <= start`.
pub fn in_window(t: NaiveTime, start: NaiveTime, end: NaiveTime) -> bool {
    if start < end {
        start <= t && t < end
    } else {
        t >= start || t < end
    }
}

fn lerp(a: f64, b: f64, f: f64) -> f64 {
    a + (b - a) * f.clamp(0.0, 1.0)
}

/// Lux to dim through the curve (points sorted by lux).
pub fn lux_curve(curve: &[[f64; 2]], lux: f64) -> u8 {
    let (Some(first), Some(last)) = (curve.first(), curve.last()) else {
        return 255;
    };
    let v = if lux <= first[0] {
        first[1]
    } else if lux >= last[0] {
        last[1]
    } else {
        let w = curve
            .windows(2)
            .find(|w| lux <= w[1][0])
            .expect("lux is inside the curve");
        lerp(w[0][1], w[1][1], (lux - w[0][0]) / (w[1][0] - w[0][0]))
    };
    v.round().clamp(0.0, 255.0) as u8
}

/// Sunrise and sunset on one calendar day at one place.
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum SunDay {
    Normal {
        rise: DateTime<Utc>,
        set: DateTime<Utc>,
    },
    /// The sun stays up all day.
    PolarDay,
    /// The sun does not rise.
    PolarNight,
}

/// Sunrise and sunset for the local calendar day `date` at (`lat`, `lon`)
/// (degrees, north and east positive), using NOAA's general solar position
/// equations (<https://gml.noaa.gov/grad/solcalc/solareqns.PDF>, based on
/// Meeus, Astronomical Algorithms): fractional year, equation of time,
/// declination, then the hour angle for zenith 90.833 degrees,
/// `sunrise = 720 - 4 (lon + ha) - eqtime` minutes UTC. `date` must be the
/// date at the place, since the result may fall on the previous or next UTC
/// day. The fractional year is evaluated at the event time itself (one
/// refinement pass). Accurate to about a minute away from the poles.
pub fn sun_day(date: NaiveDate, lat: f64, lon: f64) -> SunDay {
    use chrono::Datelike;
    let days_in_year = if date.leap_year() { 366.0 } else { 365.0 };
    let day0 = f64::from(date.ordinal0());
    // (eqtime minutes, declination radians) at `hour` UTC.
    let at = |hour: f64| {
        let g = 2.0 * std::f64::consts::PI / days_in_year * (day0 + (hour - 12.0) / 24.0);
        let eqtime = 229.18
            * (0.000075 + 0.001868 * g.cos()
                - 0.032077 * g.sin()
                - 0.014615 * (2.0 * g).cos()
                - 0.040849 * (2.0 * g).sin());
        let decl = 0.006918 - 0.399912 * g.cos() + 0.070257 * g.sin() - 0.006758 * (2.0 * g).cos()
            + 0.000907 * (2.0 * g).sin()
            - 0.002697 * (3.0 * g).cos()
            + 0.00148 * (3.0 * g).sin();
        (eqtime, decl)
    };
    let lat_r = lat.to_radians();
    // Hour angle in degrees, or the polar case.
    let ha = |decl: f64| -> Result<f64, SunDay> {
        let c =
            90.833_f64.to_radians().cos() / (lat_r.cos() * decl.cos()) - lat_r.tan() * decl.tan();
        if c > 1.0 {
            Err(SunDay::PolarNight)
        } else if c < -1.0 {
            Err(SunDay::PolarDay)
        } else {
            Ok(c.acos().to_degrees())
        }
    };
    // Minutes after 00:00 UTC of `date`'s UTC day; `sign` +1 sunrise, -1 sunset.
    let event = |sign: f64| -> Result<f64, SunDay> {
        let mut t = 720.0 - 4.0 * lon;
        for _ in 0..2 {
            let (eq, decl) = at(t / 60.0);
            t = 720.0 - 4.0 * (lon + sign * ha(decl)?) - eq;
        }
        Ok(t)
    };
    match (event(1.0), event(-1.0)) {
        (Ok(r), Ok(s)) => {
            let midnight = date.and_hms_opt(0, 0, 0).expect("midnight").and_utc();
            let at_min = |m: f64| midnight + chrono::Duration::seconds((m * 60.0).round() as i64);
            SunDay::Normal {
                rise: at_min(r),
                set: at_min(s),
            }
        }
        (Err(e), _) | (_, Err(e)) => e,
    }
}

/// Dim level from the sun: 255 by day, `night` after dusk, linear ramps of
/// `ramp_min` after sunset and before sunrise.
pub fn sun_dim(cfg: &SunConfig, now: DateTime<FixedOffset>) -> u8 {
    let day = sun_day(now.date_naive(), cfg.location.lat, cfg.location.lon);
    match day {
        SunDay::PolarDay => 255,
        SunDay::PolarNight => cfg.night,
        SunDay::Normal { rise, set } => {
            let t = now.with_timezone(&Utc);
            let ramp = chrono::Duration::seconds((cfg.ramp_min * 60.0) as i64);
            let frac = |from: DateTime<Utc>, to: DateTime<Utc>| {
                let span = (to - from).num_milliseconds() as f64;
                if span <= 0.0 {
                    1.0
                } else {
                    (t - from).num_milliseconds() as f64 / span
                }
            };
            let night = f64::from(cfg.night);
            let v = if t < rise - ramp {
                night
            } else if t < rise {
                lerp(night, 255.0, frac(rise - ramp, rise))
            } else if t < set {
                255.0
            } else if t < set + ramp {
                lerp(255.0, night, frac(set, set + ramp))
            } else {
                night
            };
            v.round().clamp(0.0, 255.0) as u8
        }
    }
}

/// Everything the dim level depends on.
#[derive(Debug, Clone, Default)]
pub struct AutoDim {
    pub quiet: Option<QuietHoursConfig>,
    pub sun: Option<SunConfig>,
    pub lux: Option<LuxConfig>,
}

impl AutoDim {
    pub fn new(quiet: Option<QuietHoursConfig>, auto: Option<AutoDimConfig>) -> Self {
        let auto = auto.unwrap_or_default();
        AutoDim {
            quiet,
            sun: auto.sun,
            lux: auto.lux,
        }
    }

    /// Whether there is anything to do.
    pub fn is_active(&self) -> bool {
        self.quiet.is_some() || self.sun.is_some() || self.lux.is_some()
    }

    /// The dim level now: the minimum of the inputs that apply (255 if none).
    pub fn dim(
        &self,
        now: DateTime<FixedOffset>,
        sys_now: SystemTime,
        store: &StoreSnapshot,
    ) -> u8 {
        let mut dim = 255u8;
        if let Some(q) = &self.quiet {
            if let (Some(s), Some(e)) = (parse_hhmm(&q.start), parse_hhmm(&q.end)) {
                // Compare at minute resolution.
                let t = now.time().with_second(0).and_then(|t| t.with_nanosecond(0));
                if t.is_some_and(|t| in_window(t, s, e)) {
                    dim = dim.min(q.brightness);
                }
            }
        }
        if let Some(sun) = &self.sun {
            dim = dim.min(sun_dim(sun, now));
        }
        if let Some(lux) = &self.lux {
            if let Some(v) = fresh_lux(lux, store, sys_now) {
                dim = dim.min(lux_curve(&lux.curve, v));
            }
        }
        dim
    }
}

fn fresh_lux(cfg: &LuxConfig, store: &StoreSnapshot, now: SystemTime) -> Option<f64> {
    let e = store.get(&cfg.key)?;
    let Value::Number { value, .. } = &e.value else {
        return None;
    };
    let age = now.duration_since(e.updated_at).unwrap_or_default();
    (value.is_finite() && age.as_secs_f64() <= cfg.stale_after_s).then_some(*value)
}

// ------------------------------------------------------------------ task

/// Remembers the last level sent, so `SetDim` goes out only on change.
#[derive(Debug, Default)]
pub struct Dimmer {
    last: Option<u8>,
}

impl Dimmer {
    /// Evaluate once and send `SetDim` if the level changed. Returns the
    /// level if it was sent.
    pub fn tick(
        &mut self,
        ad: &AutoDim,
        now: DateTime<FixedOffset>,
        sys_now: SystemTime,
        store: &StoreSnapshot,
        control: &Control,
    ) -> Option<u8> {
        let dim = ad.dim(now, sys_now, store);
        if self.last == Some(dim) {
            return None;
        }
        // A stopped render loop means shutdown; try again next tick.
        control.send(Command::SetDim(dim)).ok()?;
        self.last = Some(dim);
        Some(dim)
    }
}

/// Run until the task is dropped. Does nothing if no input is configured.
pub async fn run(ad: AutoDim, store: Store, control: Control) {
    if !ad.is_active() {
        return;
    }
    let period = if ad.lux.is_some() { LUX_TICK } else { TICK };
    let mut every = tokio::time::interval(period);
    every.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
    let mut dimmer = Dimmer::default();
    loop {
        every.tick().await;
        let now = chrono::Local::now().fixed_offset();
        if let Some(d) = dimmer.tick(&ad, now, SystemTime::now(), &store.snapshot(), &control) {
            tracing::info!(dim = d, "auto-dim");
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::control::channel;
    use crate::store::Entry;
    use std::collections::HashMap;
    use std::sync::Arc;

    fn t(h: u32, m: u32) -> NaiveTime {
        NaiveTime::from_hms_opt(h, m, 0).unwrap()
    }

    fn at(offset_h: i32, y: i32, mo: u32, d: u32, h: u32, mi: u32) -> DateTime<FixedOffset> {
        use chrono::TimeZone;
        FixedOffset::east_opt(offset_h * 3600)
            .unwrap()
            .with_ymd_and_hms(y, mo, d, h, mi, 0)
            .unwrap()
    }

    fn quiet() -> QuietHoursConfig {
        QuietHoursConfig {
            start: "22:00".into(),
            end: "06:30".into(),
            brightness: 20,
        }
    }

    #[test]
    fn windows_wrap_midnight() {
        let (s, e) = (t(22, 0), t(6, 30));
        assert!(in_window(t(22, 0), s, e));
        assert!(in_window(t(23, 59), s, e));
        assert!(in_window(t(0, 0), s, e));
        assert!(in_window(t(6, 29), s, e));
        assert!(!in_window(t(6, 30), s, e));
        assert!(!in_window(t(12, 0), s, e));
        assert!(!in_window(t(21, 59), s, e));
        // Same-day window.
        assert!(in_window(t(13, 0), t(12, 0), t(14, 0)));
        assert!(!in_window(t(14, 0), t(12, 0), t(14, 0)));
        assert!(!in_window(t(11, 59), t(12, 0), t(14, 0)));
    }

    #[test]
    fn quiet_hours_set_the_dim_level() {
        let ad = AutoDim::new(Some(quiet()), None);
        let store = Arc::new(HashMap::new());
        let sys = SystemTime::now();
        assert_eq!(ad.dim(at(10, 2026, 1, 5, 23, 0), sys, &store), 20);
        assert_eq!(ad.dim(at(10, 2026, 1, 5, 3, 0), sys, &store), 20);
        assert_eq!(ad.dim(at(10, 2026, 1, 5, 12, 0), sys, &store), 255);
    }

    #[test]
    fn lux_curve_interpolates_and_clamps() {
        let c = [[0.0, 20.0], [50.0, 120.0], [400.0, 255.0]];
        assert_eq!(lux_curve(&c, -5.0), 20);
        assert_eq!(lux_curve(&c, 0.0), 20);
        assert_eq!(lux_curve(&c, 25.0), 70);
        assert_eq!(lux_curve(&c, 50.0), 120);
        assert_eq!(lux_curve(&c, 225.0), 188); // 120 + 135 * 175/350 = 187.5
        assert_eq!(lux_curve(&c, 400.0), 255);
        assert_eq!(lux_curve(&c, 5000.0), 255);
        assert_eq!(lux_curve(&[[10.0, 99.0]], 3.0), 99);
    }

    fn lux_entry(v: f64, age: Duration, now: SystemTime) -> StoreSnapshot {
        let mut m = HashMap::new();
        m.insert(
            "ha:sensor.lux".to_string(),
            Entry {
                value: Value::Number {
                    value: v,
                    unit: None,
                },
                updated_at: now - age,
                source: "ha".into(),
            },
        );
        Arc::new(m)
    }

    fn lux_cfg() -> LuxConfig {
        LuxConfig {
            key: "ha:sensor.lux".into(),
            curve: vec![[0.0, 20.0], [50.0, 120.0], [400.0, 255.0]],
            stale_after_s: 300.0,
        }
    }

    #[test]
    fn lux_goes_stale_and_falls_back_to_the_schedule() {
        let ad = AutoDim::new(
            Some(quiet()),
            Some(AutoDimConfig {
                sun: None,
                lux: Some(lux_cfg()),
            }),
        );
        let sys = SystemTime::now();
        let day = at(10, 2026, 1, 5, 12, 0);
        let night = at(10, 2026, 1, 5, 23, 0);
        let fresh = lux_entry(50.0, Duration::from_secs(10), sys);
        let stale = lux_entry(50.0, Duration::from_secs(301), sys);
        let none = Arc::new(HashMap::new());
        assert_eq!(ad.dim(day, sys, &fresh), 120);
        assert_eq!(ad.dim(day, sys, &stale), 255, "no schedule dim by day");
        assert_eq!(ad.dim(night, sys, &stale), 20, "quiet hours still apply");
        assert_eq!(ad.dim(day, sys, &none), 255);
        // Minimum rule: bright room, but quiet hours win; dark room beats day.
        let bright = lux_entry(1000.0, Duration::ZERO, sys);
        assert_eq!(ad.dim(night, sys, &bright), 20);
        let dark = lux_entry(0.0, Duration::ZERO, sys);
        assert_eq!(ad.dim(day, sys, &dark), 20);
        assert_eq!(ad.dim(night, sys, &fresh), 20, "min(20, 120)");
    }

    // Sunrise/sunset reference values: US Naval Observatory, Astronomical
    // Applications Dept., "Complete Sun and Moon Data for One Day" API
    // (https://aa.usno.navy.mil/api/rstt/oneday?date=..&coords=lat,lon&tz=..),
    // fetched for these dates; minute resolution, times local to the stated
    // UTC offset. Landmarks: Royal Observatory Greenwich (51.4769, -0.0005),
    // Sydney Observatory (-33.8590, 151.2050), Longyearbyen (78.22, 15.65).
    fn local_hm(d: DateTime<Utc>, off_h: i32) -> (u32, u32) {
        let l = d.with_timezone(&FixedOffset::east_opt(off_h * 3600).unwrap());
        (l.hour(), l.minute())
    }

    fn check(
        date: (i32, u32, u32),
        lat: f64,
        lon: f64,
        off: i32,
        rise: (u32, u32),
        set: (u32, u32),
    ) {
        let d = NaiveDate::from_ymd_opt(date.0, date.1, date.2).unwrap();
        let SunDay::Normal { rise: r, set: s } = sun_day(d, lat, lon) else {
            panic!("expected sunrise and sunset on {d}");
        };
        let diff = |a: (u32, u32), b: (u32, u32)| {
            (i64::from(a.0 * 60 + a.1) - i64::from(b.0 * 60 + b.1)).abs()
        };
        let (gr, gs) = (local_hm(r, off), local_hm(s, off));
        assert!(diff(gr, rise) <= 2, "{d} rise {gr:?} vs {rise:?}");
        assert!(diff(gs, set) <= 2, "{d} set {gs:?} vs {set:?}");
    }

    #[test]
    fn sun_times_match_the_usno_for_landmarks() {
        let (glat, glon) = (51.4769, -0.0005);
        check((2026, 3, 20), glat, glon, 0, (6, 3), (18, 13));
        check((2026, 6, 21), glat, glon, 1, (4, 43), (21, 21));
        check((2026, 12, 21), glat, glon, 0, (8, 3), (15, 53));
        let (slat, slon) = (-33.8590, 151.2050);
        check((2026, 1, 15), slat, slon, 11, (6, 0), (20, 9));
        check((2026, 6, 21), slat, slon, 10, (7, 0), (16, 54));
        check((2026, 9, 22), slat, slon, 10, (5, 45), (17, 51));
    }

    #[test]
    fn polar_day_and_night() {
        // USNO: Longyearbyen is continuously above the horizon on 2026-06-21
        // and continuously below it on 2026-12-21.
        let day = NaiveDate::from_ymd_opt(2026, 6, 21).unwrap();
        let night = NaiveDate::from_ymd_opt(2026, 12, 21).unwrap();
        assert_eq!(sun_day(day, 78.22, 15.65), SunDay::PolarDay);
        assert_eq!(sun_day(night, 78.22, 15.65), SunDay::PolarNight);
        // Southern hemisphere mirrors it.
        assert_eq!(sun_day(day, -78.22, 15.65), SunDay::PolarNight);
        let sun = SunConfig {
            location: Location {
                lat: 78.22,
                lon: 15.65,
            },
            night: 40,
            ramp_min: 30.0,
        };
        assert_eq!(sun_dim(&sun, at(2, 2026, 6, 21, 3, 0)), 255);
        assert_eq!(sun_dim(&sun, at(1, 2026, 12, 21, 12, 0)), 40);
    }

    #[test]
    fn sun_ramp_follows_sunset_and_sunrise() {
        // Greenwich, 2026-03-20 (UTC): sunrise ~06:03, sunset ~18:13.
        let sun = SunConfig {
            location: Location {
                lat: 51.4769,
                lon: -0.0005,
            },
            night: 40,
            ramp_min: 30.0,
        };
        let d = NaiveDate::from_ymd_opt(2026, 3, 20).unwrap();
        let SunDay::Normal { rise, set } = sun_day(d, sun.location.lat, sun.location.lon) else {
            panic!()
        };
        let z = |dt: DateTime<Utc>| dt.fixed_offset();
        let m = chrono::Duration::minutes;
        assert_eq!(sun_dim(&sun, z(rise - m(31))), 40);
        assert_eq!(sun_dim(&sun, z(rise - m(15))), 148); // halfway 40..255
        assert_eq!(sun_dim(&sun, z(rise + m(1))), 255);
        assert_eq!(sun_dim(&sun, z(set - m(1))), 255);
        assert_eq!(sun_dim(&sun, z(set + m(15))), 148);
        assert_eq!(sun_dim(&sun, z(set + m(31))), 40);
        assert_eq!(sun_dim(&sun, at(0, 2026, 3, 20, 0, 5)), 40);
        // Ramp 0 is a step.
        let step = SunConfig {
            ramp_min: 0.0,
            ..sun
        };
        assert_eq!(sun_dim(&step, z(set + m(1))), 40);
    }

    #[test]
    fn set_dim_is_sent_only_on_change() {
        let (control, link, _levels) = channel(vec![]);
        let ad = AutoDim::new(Some(quiet()), None);
        let store = Arc::new(HashMap::new());
        let sys = SystemTime::now();
        let mut d = Dimmer::default();
        let drain = || link.commands.try_iter().collect::<Vec<_>>();

        // First tick always sends.
        d.tick(&ad, at(0, 2026, 1, 5, 12, 0), sys, &store, &control);
        assert_eq!(drain(), vec![Command::SetDim(255)]);
        // Same level at other times of the day: nothing.
        d.tick(&ad, at(0, 2026, 1, 5, 12, 30), sys, &store, &control);
        d.tick(&ad, at(0, 2026, 1, 5, 21, 59), sys, &store, &control);
        assert!(drain().is_empty());
        // Quiet hours begin.
        d.tick(&ad, at(0, 2026, 1, 5, 22, 0), sys, &store, &control);
        d.tick(&ad, at(0, 2026, 1, 5, 23, 0), sys, &store, &control);
        assert_eq!(drain(), vec![Command::SetDim(20)]);
        // Across midnight, still quiet; then back to full.
        d.tick(&ad, at(0, 2026, 1, 6, 0, 30), sys, &store, &control);
        assert!(drain().is_empty());
        d.tick(&ad, at(0, 2026, 1, 6, 6, 30), sys, &store, &control);
        assert_eq!(drain(), vec![Command::SetDim(255)]);
    }

    #[test]
    fn validation_reports_bad_config() {
        let bad = QuietHoursConfig {
            start: "25:00".into(),
            end: "x".into(),
            brightness: 1,
        };
        assert_eq!(validate(Some(&bad), None).len(), 2);
        let same = QuietHoursConfig {
            start: "22:00".into(),
            end: "22:00".into(),
            brightness: 1,
        };
        assert_eq!(validate(Some(&same), None).len(), 1);
        assert!(validate(Some(&quiet()), None).is_empty());
        let mut lux = lux_cfg();
        lux.curve = vec![[5.0, 1.0], [5.0, 2.0]];
        let a = AutoDimConfig {
            sun: None,
            lux: Some(lux),
        };
        assert_eq!(validate(None, Some(&a)).len(), 1);
    }

    #[test]
    fn config_parses_and_validates() {
        let base = "[canvas]\nwidth = 4\nheight = 2\n[[scene]]\nname = \"s\"\n";
        let ok = format!(
            "{base}[quiet_hours]\nstart = \"22:00\"\nend = \"06:30\"\nbrightness = 20\n\
             [auto_dim.sun]\nlocation = {{ lat = 51.4769, lon = -0.0005 }}\nnight = 40\n\
             [auto_dim.lux]\nkey = \"ha:sensor.lux\"\ncurve = [[0, 20], [50, 120], [400, 255]]\n"
        );
        let cfg = crate::config::Config::parse(&ok).unwrap();
        assert_eq!(cfg.quiet_hours.unwrap().brightness, 20);
        let a = cfg.auto_dim.unwrap();
        assert_eq!(a.sun.unwrap().ramp_min, 30.0);
        assert_eq!(a.lux.unwrap().stale_after_s, 600.0);
        let bad =
            format!("{base}[quiet_hours]\nstart = \"7pm\"\nend = \"06:30\"\nbrightness = 20\n");
        let e = crate::config::Config::parse(&bad).unwrap_err().to_string();
        assert!(e.contains("quiet_hours.start"), "{e}");
    }
}
