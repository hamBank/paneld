//! `type = "system"`: uptime, CPU temperature, load and memory of the machine
//! paneld runs on (DESIGN.md §6 item 5).
//!
//! ```toml
//! [sources.sys]
//! type = "system"
//! interval_s = 10
//! ```
//!
//! # Keys
//! Prefixed with the source's name (`[sources.sys]` writes `sys:*`):
//!
//! | key | value | from |
//! |---|---|---|
//! | `<name>:uptime_s` | Number, unit `s` | `/proc/uptime`, first field |
//! | `<name>:uptime` | Text, `3d 4h`, `5h 12m` or `42m` | the same |
//! | `<name>:cpu_temp_c` | Number, unit `°C`, 1 decimal | a thermal zone's `temp` (millidegrees, per the kernel's sysfs-class-thermal ABI) |
//! | `<name>:load_1m` | Number | `/proc/loadavg`, first field |
//! | `<name>:mem_used_pct` | Number, unit `%`, 1 decimal | `/proc/meminfo`: `100 × (1 − MemAvailable / MemTotal)` |
//!
//! File formats: `proc(5)` (`/proc/uptime`, `/proc/loadavg`, `/proc/meminfo`)
//! and the kernel's `Documentation/ABI/testing/sysfs-class-thermal`
//! (`thermal_zone*/type`, `thermal_zone*/temp` in millidegrees Celsius).
//!
//! A reading that cannot be made (no such file, as on macOS, or a format we
//! do not understand) removes its key, so widgets show their placeholder.
//! If no reading at all works, the source counts an error and logs a warning
//! once; it keeps trying, since files can appear later (a thermal driver
//! loading).
//!
//! The files are tiny and in memory, but reading them is still file I/O, so
//! each round runs on tokio's blocking pool.

use std::path::{Path, PathBuf};
use std::time::Duration;

use super::{Source, SourceCtx, SourceFuture};
use crate::config::SystemSourceConfig;
use crate::store::{Change, Value};

/// Where the readings come from. Real paths in production; tests point it
/// at a temporary directory.
#[derive(Debug, Clone)]
pub struct Paths {
    /// `/proc`.
    pub proc: PathBuf,
    /// `/sys/class/thermal`, searched when `thermal` is not given.
    pub thermal_root: PathBuf,
    /// The configured `thermal_zone` file, if any.
    pub thermal: Option<PathBuf>,
}

impl Paths {
    pub fn system(thermal: Option<PathBuf>) -> Paths {
        Paths {
            proc: PathBuf::from("/proc"),
            thermal_root: PathBuf::from("/sys/class/thermal"),
            thermal,
        }
    }
}

/// One round of readings; `None` where a reading failed.
#[derive(Debug, Clone, Copy, Default, PartialEq)]
pub struct Reading {
    pub uptime_s: Option<f64>,
    pub cpu_temp_c: Option<f64>,
    pub load_1m: Option<f64>,
    pub mem_used_pct: Option<f64>,
}

impl Reading {
    fn is_empty(&self) -> bool {
        *self == Reading::default()
    }
}

pub struct SystemSource {
    name: String,
    interval: Duration,
    paths: Paths,
}

impl SystemSource {
    pub fn new(name: &str, cfg: &SystemSourceConfig) -> Self {
        SystemSource::with_paths(name, cfg, Paths::system(cfg.thermal_zone.clone()))
    }

    pub fn with_paths(name: &str, cfg: &SystemSourceConfig, paths: Paths) -> Self {
        SystemSource {
            name: name.to_owned(),
            interval: Duration::from_secs_f64(cfg.interval_s.clamp(1.0, 3600.0)),
            paths,
        }
    }
}

impl Source for SystemSource {
    fn run(self: Box<Self>, mut ctx: SourceCtx) -> SourceFuture {
        Box::pin(async move {
            let SystemSource {
                name,
                interval,
                mut paths,
            } = *self;
            // Pick the thermal zone once per run; the configured one wins.
            if paths.thermal.is_none() {
                let root = paths.thermal_root.clone();
                paths.thermal = tokio::task::spawn_blocking(move || find_cpu_zone(&root))
                    .await
                    .ok()
                    .flatten();
                match &paths.thermal {
                    Some(p) => {
                        tracing::debug!(source = %name, zone = %p.display(), "system: CPU temperature")
                    }
                    None => tracing::debug!(source = %name, "system: no thermal zone found"),
                }
            }
            let mut warned = false;
            loop {
                let p = paths.clone();
                let reading = tokio::task::spawn_blocking(move || read(&p))
                    .await
                    .unwrap_or_default();
                ctx.store.apply(changes(&name, &reading));
                if reading.is_empty() {
                    if !warned {
                        tracing::warn!(
                            source = %name,
                            "system: no reading worked (needs Linux /proc and /sys)"
                        );
                        warned = true;
                    }
                    ctx.error();
                } else {
                    ctx.ok();
                }
                if !ctx.shutdown.sleep(interval).await {
                    return Ok(());
                }
            }
        })
    }
}

/// Make every reading. Blocking file I/O.
pub fn read(paths: &Paths) -> Reading {
    let text = |p: PathBuf| std::fs::read_to_string(p).ok();
    Reading {
        uptime_s: text(paths.proc.join("uptime")).and_then(|s| parse_uptime(&s)),
        cpu_temp_c: paths
            .thermal
            .as_ref()
            .and_then(|p| text(p.clone()))
            .and_then(|s| parse_millidegrees(&s)),
        load_1m: text(paths.proc.join("loadavg")).and_then(|s| parse_loadavg(&s)),
        mem_used_pct: text(paths.proc.join("meminfo")).and_then(|s| parse_meminfo(&s)),
    }
}

/// The store changes for one reading: set what was read, remove the rest.
pub fn changes(name: &str, r: &Reading) -> Vec<Change> {
    let key = |f: &str| format!("{name}:{f}");
    let num = |f: &str, v: Option<f64>, unit: Option<&str>| match v {
        Some(value) => Change::Set(
            key(f),
            Value::Number {
                value,
                unit: unit.map(str::to_owned),
            },
        ),
        None => Change::Remove(key(f)),
    };
    vec![
        num("uptime_s", r.uptime_s, Some("s")),
        match r.uptime_s {
            Some(s) => Change::Set(key("uptime"), Value::Text(format_uptime(s))),
            None => Change::Remove(key("uptime")),
        },
        num("cpu_temp_c", r.cpu_temp_c, Some("°C")),
        num("load_1m", r.load_1m, None),
        num("mem_used_pct", r.mem_used_pct, Some("%")),
    ]
}

/// The CPU's thermal zone under `root` (`/sys/class/thermal`): the first
/// `thermal_zone<N>` (by N) whose `type` names the CPU or SoC, else zone 0.
pub fn find_cpu_zone(root: &Path) -> Option<PathBuf> {
    let mut zones: Vec<(u32, PathBuf)> = std::fs::read_dir(root)
        .ok()?
        .filter_map(|e| {
            let e = e.ok()?;
            let n = e
                .file_name()
                .to_str()?
                .strip_prefix("thermal_zone")?
                .parse()
                .ok()?;
            Some((n, e.path()))
        })
        .collect();
    zones.sort();
    let is_cpu = |dir: &Path| {
        std::fs::read_to_string(dir.join("type")).is_ok_and(|t| {
            let t = t.trim().to_ascii_lowercase();
            t.contains("cpu") || t.contains("x86_pkg_temp") || t.contains("soc")
        })
    };
    zones
        .iter()
        .find(|(_, d)| is_cpu(d))
        .or_else(|| zones.first())
        .map(|(_, d)| d.join("temp"))
}

/// `/proc/uptime`: "seconds-up idle-seconds".
pub fn parse_uptime(s: &str) -> Option<f64> {
    finite(s.split_whitespace().next()?.parse().ok()?)
}

/// `/proc/loadavg`: "1m 5m 15m running/total last-pid".
pub fn parse_loadavg(s: &str) -> Option<f64> {
    finite(s.split_whitespace().next()?.parse().ok()?)
}

/// A thermal zone's `temp`: an integer in millidegrees Celsius.
pub fn parse_millidegrees(s: &str) -> Option<f64> {
    let m: i64 = s.trim().parse().ok()?;
    Some(round1(m as f64 / 1000.0))
}

/// `/proc/meminfo`: percent of `MemTotal` not `MemAvailable` (kernel 3.14+).
pub fn parse_meminfo(s: &str) -> Option<f64> {
    let field = |name: &str| -> Option<f64> {
        s.lines()
            .find_map(|l| l.strip_prefix(name)?.strip_prefix(':'))?
            .split_whitespace()
            .next()?
            .parse()
            .ok()
    };
    let (total, avail) = (field("MemTotal")?, field("MemAvailable")?);
    (total > 0.0 && avail <= total).then(|| round1(100.0 * (1.0 - avail / total)))
}

/// Uptime as two units: `3d 4h`, `5h 12m`, `42m` (`0m` under a minute).
pub fn format_uptime(secs: f64) -> String {
    let m = (secs.max(0.0) / 60.0) as u64;
    let (d, h, m) = (m / 1440, m / 60 % 24, m % 60);
    if d > 0 {
        format!("{d}d {h}h")
    } else if h > 0 {
        format!("{h}h {m}m")
    } else {
        format!("{m}m")
    }
}

fn finite(v: f64) -> Option<f64> {
    v.is_finite().then_some(v)
}

fn round1(v: f64) -> f64 {
    (v * 10.0).round() / 10.0
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_proc_files() {
        assert_eq!(parse_uptime("12345.67 98765.43\n"), Some(12345.67));
        assert_eq!(parse_uptime(""), None);
        assert_eq!(parse_loadavg("0.52 0.58 0.59 1/389 12345\n"), Some(0.52));
        assert_eq!(parse_loadavg("x 0.58"), None);
        let meminfo = "MemTotal:        3884356 kB\nMemFree:          123456 kB\nMemAvailable:    2913267 kB\n";
        assert_eq!(parse_meminfo(meminfo), Some(25.0));
        assert_eq!(parse_meminfo("MemTotal: 100 kB\n"), None, "no MemAvailable");
        assert_eq!(parse_meminfo("MemTotal: 0 kB\nMemAvailable: 0 kB\n"), None);
    }

    #[test]
    fn parses_millidegrees() {
        assert_eq!(parse_millidegrees("48312\n"), Some(48.3));
        assert_eq!(parse_millidegrees("-5250"), Some(-5.3));
        assert_eq!(parse_millidegrees("hot"), None);
    }

    #[test]
    fn formats_uptime_with_two_units() {
        assert_eq!(format_uptime(30.0), "0m");
        assert_eq!(format_uptime(42.0 * 60.0), "42m");
        assert_eq!(format_uptime(5.0 * 3600.0 + 12.0 * 60.0 + 59.0), "5h 12m");
        assert_eq!(
            format_uptime(3.0 * 86400.0 + 4.0 * 3600.0 + 5.0 * 60.0),
            "3d 4h"
        );
    }

    #[test]
    fn changes_set_what_was_read_and_remove_the_rest() {
        let r = Reading {
            uptime_s: Some(3600.0),
            cpu_temp_c: None,
            load_1m: Some(0.5),
            mem_used_pct: None,
        };
        let c = changes("sys", &r);
        assert!(c.contains(&Change::Set(
            "sys:uptime_s".into(),
            Value::Number {
                value: 3600.0,
                unit: Some("s".into())
            }
        )));
        assert!(c.contains(&Change::Set(
            "sys:uptime".into(),
            Value::Text("1h 0m".into())
        )));
        assert!(c.contains(&Change::Remove("sys:cpu_temp_c".into())));
        assert!(c.contains(&Change::Remove("sys:mem_used_pct".into())));
    }

    fn tmp(tag: &str) -> PathBuf {
        let p = std::env::temp_dir().join(format!("paneld-system-{tag}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&p);
        std::fs::create_dir_all(&p).unwrap();
        p
    }

    #[test]
    fn finds_the_cpu_zone_by_type_else_zone_0() {
        let root = tmp("zones");
        for (n, t) in [(0, "acpitz"), (1, "x86_pkg_temp"), (2, "cpu-thermal")] {
            let d = root.join(format!("thermal_zone{n}"));
            std::fs::create_dir_all(&d).unwrap();
            std::fs::write(d.join("type"), format!("{t}\n")).unwrap();
        }
        std::fs::create_dir_all(root.join("cooling_device0")).unwrap();
        assert_eq!(find_cpu_zone(&root), Some(root.join("thermal_zone1/temp")));

        std::fs::write(root.join("thermal_zone1/type"), "iwlwifi\n").unwrap();
        std::fs::write(root.join("thermal_zone2/type"), "wifi\n").unwrap();
        assert_eq!(find_cpu_zone(&root), Some(root.join("thermal_zone0/temp")));

        assert_eq!(find_cpu_zone(&root.join("missing")), None);
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn reads_a_fake_proc_and_sys() {
        let root = tmp("read");
        let proc = root.join("proc");
        std::fs::create_dir_all(&proc).unwrap();
        std::fs::write(proc.join("uptime"), "90061.5 1.0\n").unwrap();
        std::fs::write(proc.join("loadavg"), "1.25 1.0 0.5 2/100 42\n").unwrap();
        std::fs::write(root.join("temp"), "51234\n").unwrap();
        let paths = Paths {
            proc,
            thermal_root: root.join("none"),
            thermal: Some(root.join("temp")),
        };
        assert_eq!(
            read(&paths),
            Reading {
                uptime_s: Some(90061.5),
                cpu_temp_c: Some(51.2),
                load_1m: Some(1.25),
                mem_used_pct: None,
            }
        );
        let nothing = Paths {
            proc: root.join("nope"),
            thermal_root: root.join("nope"),
            thermal: None,
        };
        assert!(read(&nothing).is_empty());
        let _ = std::fs::remove_dir_all(&root);
    }
}
