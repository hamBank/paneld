//! Output settings that follow config reloads (DESIGN.md §7.3).
//!
//! The running sinks and the preview are matched to the reloaded config:
//! - a Colorlight output is matched **by interface**; when its table differs
//!   from what that sink last got, the sink receives new [`SinkSettings`]
//!   (gamma, brightness, white point, brightness mode, colour order, sync
//!   gap, firmware override, regions and card size) and uses them from the
//!   next frame;
//! - the preview gets its new `scale` and `grid`.
//!
//! Adding or removing an output, or changing an output's interface, still
//! needs a restart: the set of sinks (their threads, `/metrics` series and
//! the Home Assistant card sensor) is fixed at start-up. Those changes are
//! reported by [`crate::reload::RestartOnly`] as before, and the settings of
//! the outputs that do match are still applied.

use std::collections::HashMap;
use std::sync::Mutex;

use crate::colorlight_sink::{SinkSettings, SinkUpdater};
use crate::config::{Config, OutputConfig};
use crate::preview::PreviewControl;

/// Handles to the running outputs, for the reload task.
pub struct OutputReload {
    sinks: HashMap<String, SinkUpdater>,
    preview: Option<PreviewControl>,
    /// `Debug` of each sink's output table as last applied, by interface.
    applied: Mutex<HashMap<String, String>>,
}

impl OutputReload {
    /// `sinks` are the running sinks by interface; `cfg` is the config they
    /// were started with.
    pub fn new(
        cfg: &Config,
        sinks: impl IntoIterator<Item = (String, SinkUpdater)>,
        preview: Option<PreviewControl>,
    ) -> Self {
        OutputReload {
            sinks: sinks.into_iter().collect(),
            preview,
            applied: Mutex::new(colorlight_tables(cfg)),
        }
    }

    /// Push the reloaded output settings to the running outputs. Returns a
    /// line per output that changed (for the log), or the errors of outputs
    /// whose new settings could not be built (those keep their old ones).
    pub fn apply(&self, cfg: &Config) -> (Vec<String>, Vec<String>) {
        let (mut changed, mut errors) = (Vec::new(), Vec::new());
        let mut applied = self.applied.lock().unwrap_or_else(|e| e.into_inner());
        for (iface, table) in colorlight_tables(cfg) {
            let Some(sink) = self.sinks.get(&iface) else {
                continue; // a new output: restart-only, warned about elsewhere
            };
            if applied.get(&iface) == Some(&table) {
                continue;
            }
            let Some(cl) = cfg.outputs.iter().find_map(|o| match o {
                OutputConfig::Colorlight(c) if c.interface == iface => Some(c),
                _ => None,
            }) else {
                continue;
            };
            match SinkSettings::from_output(cl, cfg.canvas.width, cfg.canvas.height) {
                Ok(s) => {
                    sink.update(s);
                    applied.insert(iface.clone(), table);
                    changed.push(format!("colorlight output {iface}"));
                }
                Err(e) => errors.push(format!("{e:#}")),
            }
        }
        if let (Some(control), Some(p)) = (&self.preview, cfg.preview()) {
            if control.update(p) {
                changed.push("preview".into());
            }
        }
        (changed, errors)
    }
}

/// Each Colorlight output's table as `Debug` text, by interface.
fn colorlight_tables(cfg: &Config) -> HashMap<String, String> {
    cfg.outputs
        .iter()
        .filter_map(|o| match o {
            OutputConfig::Colorlight(c) => Some((c.interface.clone(), format!("{c:?}"))),
            _ => None,
        })
        .collect()
}

/// The output topology: what must match for a reload to apply live (kind
/// and interface of each output, in order). Used by
/// [`crate::reload::RestartOnly`].
pub fn topology(cfg: &Config) -> String {
    let parts: Vec<String> = cfg
        .outputs
        .iter()
        .map(|o| match o {
            OutputConfig::Preview(_) => "preview".to_owned(),
            OutputConfig::Colorlight(c) => format!("colorlight:{}", c.interface),
        })
        .collect();
    parts.join(",")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn topology_ignores_settings_but_not_interfaces() {
        let a = Config::parse(&cfg("eth1", 2.2, 4)).unwrap();
        let b = Config::parse(&cfg("eth1", 1.8, 8)).unwrap();
        let c = Config::parse(&cfg("eth2", 2.2, 4)).unwrap();
        assert_eq!(topology(&a), "preview,colorlight:eth1");
        assert_eq!(topology(&a), topology(&b));
        assert_ne!(topology(&a), topology(&c));
    }

    #[test]
    fn apply_updates_only_changed_matching_outputs() {
        let start = Config::parse(&cfg("eth1", 2.2, 4)).unwrap();
        let cl = |c: &Config| match &c.outputs[1] {
            OutputConfig::Colorlight(cl) => cl.clone(),
            _ => unreachable!(),
        };
        let first = SinkSettings::from_output(&cl(&start), 8, 4).unwrap();
        let (updater, mut rx) = SinkUpdater::detached(first);
        rx.borrow_and_update();
        let (prev_routes, preview) = crate::preview::router(
            crate::engine::frame_channel(8, 4).1,
            start.preview().unwrap(),
            30,
            &[],
        );
        drop(prev_routes);
        let live = OutputReload::new(&start, [("eth1".to_owned(), updater)], Some(preview));

        // Same config: nothing to do.
        assert_eq!(live.apply(&start), (vec![], vec![]));
        assert!(!rx.has_changed().unwrap());

        // New gamma and preview scale: both applied.
        let next = Config::parse(&cfg("eth1", 1.8, 8)).unwrap();
        let (changed, errors) = live.apply(&next);
        assert_eq!(changed, ["colorlight output eth1", "preview"]);
        assert!(errors.is_empty());
        assert_eq!(rx.borrow_and_update().plan.gamma, 1.8);

        // Applying the same again changes nothing.
        assert_eq!(live.apply(&next), (vec![], vec![]));

        // Another interface is not a running sink: ignored here (restart-only).
        let other = Config::parse(&cfg("eth2", 3.0, 8)).unwrap();
        assert_eq!(live.apply(&other), (vec![], vec![]));
        assert!(!rx.has_changed().unwrap());
    }

    pub(crate) fn cfg(iface: &str, gamma: f32, scale: u32) -> String {
        format!(
            r#"
[canvas]
width = 8
height = 4
[[output]]
type = "preview"
scale = {scale}
[[output]]
type = "colorlight"
interface = "{iface}"
gamma = {gamma}
[[scene]]
name = "a"
  [[scene.zone]]
  rect = {{ x = 0, y = 0, w = 8, h = 4 }}
  widget = {{ type = "clock" }}
"#
        )
    }
}
