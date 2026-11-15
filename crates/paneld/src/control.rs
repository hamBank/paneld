//! The control plane's shared core (DESIGN.md §7, milestone M4).
//!
//! Everything that changes what the panel does at runtime goes through one
//! [`Control`] handle: the HTTP API (§7.1), the MQTT / Home Assistant
//! entities (§7.2) and the auto-dim schedule (§5.2). Commands travel over a
//! std channel to the render loop, which applies them at the top of the
//! next frame without blocking. What the panel is doing comes back as a
//! [`Status`] on a `watch` channel, and the panel's output level (on/off and
//! brightness) as an [`OutputLevel`] on another, which the sinks read.
//!
//! ```text
//!  HTTP API ─┐                         ┌─> watch<Arc<Status>>   ─> /api/state, MQTT state topics
//!  MQTT     ─┼─ Control ─ mpsc ─> render loop
//!  auto-dim ─┘                         └─> watch<OutputLevel>   ─> sinks (brightness, blanking)
//! ```

use std::sync::mpsc::{self, Receiver, Sender};
use std::sync::Arc;
use std::time::Duration;

use serde::{Deserialize, Serialize};
use tokio::sync::watch;

use crate::color::Color;

/// Highest accepted notification priority (and lowest is `-MAX_PRIORITY`).
pub const MAX_PRIORITY: i32 = 100;
/// Longest a single notification may show, per repeat.
pub const MAX_NOTIFICATION_S: f64 = 3600.0;
pub const MAX_REPEAT: u32 = 100;
pub const MAX_TEXT_LEN: usize = 500;

/// A notification as received from HTTP (`POST /api/notify`) or MQTT
/// (`<prefix>/notify`): exactly one of `text` (inline content) or `scene`.
///
/// ```json
/// { "text": "Washing done", "icon": "bolt", "color": "#00ff80",
///   "duration_s": 10, "priority": 5, "repeat": 1 }
/// ```
#[derive(Debug, Clone, PartialEq, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct NotifyRequest {
    /// Inline content: text shown full-screen (it scrolls if too wide).
    #[serde(default)]
    pub text: Option<String>,
    /// Built-in icon shown with the text (`icons.rs`).
    #[serde(default)]
    pub icon: Option<String>,
    /// Text colour. Default white.
    #[serde(default)]
    pub color: Option<Color>,
    /// Or: show this configured scene instead of inline content.
    #[serde(default)]
    pub scene: Option<String>,
    /// Seconds on screen per repeat. Default 10.
    #[serde(default = "default_duration_s")]
    pub duration_s: f64,
    /// Higher interrupts lower. Default 0. Range ±[`MAX_PRIORITY`].
    #[serde(default)]
    pub priority: i32,
    /// How many times to show it. Default 1.
    #[serde(default = "default_repeat")]
    pub repeat: u32,
}

fn default_duration_s() -> f64 {
    10.0
}
fn default_repeat() -> u32 {
    1
}

/// What a notification shows.
#[derive(Debug, Clone, PartialEq)]
pub enum NotificationContent {
    Inline {
        text: String,
        icon: Option<String>,
        color: Color,
    },
    /// A configured scene, by name.
    Scene(String),
}

/// A validated notification, ready for the queue.
#[derive(Debug, Clone, PartialEq)]
pub struct Notification {
    pub content: NotificationContent,
    pub duration: Duration,
    pub priority: i32,
    pub repeat: u32,
}

impl NotifyRequest {
    /// Check the request and turn it into a [`Notification`]. `scenes` is
    /// the list of configured scene names (from [`Status::scenes`]). The
    /// error is a message fit for an HTTP 400 body.
    pub fn validate(self, scenes: &[String]) -> Result<Notification, String> {
        let content = match (self.text, self.scene) {
            (Some(_), Some(_)) => return Err("set either `text` or `scene`, not both".into()),
            (None, None) => return Err("one of `text` or `scene` is required".into()),
            (Some(text), None) => {
                if text.trim().is_empty() {
                    return Err("`text` must not be empty".into());
                }
                if text.chars().count() > MAX_TEXT_LEN {
                    return Err(format!("`text` is longer than {MAX_TEXT_LEN} characters"));
                }
                if let Some(icon) = &self.icon {
                    if crate::icons::lookup(icon).is_none() {
                        return Err(format!(
                            "unknown icon {icon:?} (known: {})",
                            crate::icons::known_names()
                        ));
                    }
                }
                NotificationContent::Inline {
                    text,
                    icon: self.icon,
                    color: self.color.unwrap_or(Color::WHITE),
                }
            }
            (None, Some(scene)) => {
                if self.icon.is_some() || self.color.is_some() {
                    return Err("`icon` and `color` only apply to `text` notifications".into());
                }
                if !scenes.contains(&scene) {
                    return Err(format!("no scene named {scene:?}"));
                }
                NotificationContent::Scene(scene)
            }
        };
        if !(self.duration_s.is_finite()
            && self.duration_s > 0.0
            && self.duration_s <= MAX_NOTIFICATION_S)
        {
            return Err(format!(
                "`duration_s` must be > 0 and at most {MAX_NOTIFICATION_S}"
            ));
        }
        if self.priority.abs() > MAX_PRIORITY {
            return Err(format!("`priority` must be within ±{MAX_PRIORITY}"));
        }
        if !(1..=MAX_REPEAT).contains(&self.repeat) {
            return Err(format!("`repeat` must be 1 to {MAX_REPEAT}"));
        }
        Ok(Notification {
            content,
            duration: Duration::from_secs_f64(self.duration_s),
            priority: self.priority,
            repeat: self.repeat,
        })
    }
}

/// A command for the render loop.
#[derive(Debug, Clone, PartialEq)]
pub enum Command {
    /// Queue a notification.
    Notify(Notification),
    /// Drop every queued and showing notification.
    ClearNotifications,
    /// Pin a scene by name (`Some`) or release the pin (`None`, "auto").
    Pin(Option<String>),
    /// Panel on or off (off = blank). Separate from brightness, so turning
    /// on restores the previous brightness.
    SetPower(bool),
    /// User brightness, 0–255. Scales each output's configured brightness
    /// (which acts as the calibrated maximum); auto-dim scales it further.
    SetBrightness(u8),
    /// Auto-dim factor from quiet hours / sun / lux, 0–255 (255 = no dim).
    /// Sent by the auto-dim task, not by users.
    SetDim(u8),
}

/// Sends [`Command`]s to the render loop and reads its [`Status`]. Cheap to
/// clone; give one to every control surface.
#[derive(Clone)]
pub struct Control {
    tx: Sender<Command>,
    status: watch::Receiver<Arc<Status>>,
}

impl Control {
    /// Queue a command; applied at the start of the next frame. Errors only
    /// if the render loop has stopped.
    pub fn send(&self, cmd: Command) -> Result<(), String> {
        self.tx
            .send(cmd)
            .map_err(|_| "the render loop is not running".to_owned())
    }

    /// The latest status.
    pub fn status(&self) -> Arc<Status> {
        self.status.borrow().clone()
    }

    /// A receiver to await status changes on (MQTT state publishing).
    pub fn subscribe(&self) -> watch::Receiver<Arc<Status>> {
        self.status.clone()
    }
}

/// The render loop's side of the control plane.
pub struct ControlLink {
    pub commands: Receiver<Command>,
    pub status: watch::Sender<Arc<Status>>,
    pub output: watch::Sender<OutputLevel>,
}

/// What the sinks should do with frames. The sinks subscribe with
/// [`OutputLevel`]'s `watch::Receiver`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
pub struct OutputLevel {
    /// `false`: show black (and hardware brightness 0 where supported).
    pub on: bool,
    /// Effective level 0–255: user brightness × dim / 255. Each sink scales
    /// its configured `brightness` by `level / 255`.
    pub level: u8,
}

impl OutputLevel {
    pub const FULL: OutputLevel = OutputLevel {
        on: true,
        level: 255,
    };

    /// Combine user brightness and the auto-dim factor (both 0–255).
    pub fn from_parts(on: bool, user: u8, dim: u8) -> OutputLevel {
        OutputLevel {
            on,
            level: ((u16::from(user) * u16::from(dim) + 127) / 255) as u8,
        }
    }

    /// Scale an output's configured brightness by this level.
    pub fn scale(self, configured: u8) -> u8 {
        if !self.on {
            return 0;
        }
        ((u16::from(configured) * u16::from(self.level) + 127) / 255) as u8
    }
}

/// A notification in the queue, as reported in [`Status`].
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct QueuedNotification {
    /// The text, or `scene:<name>` for scene notifications.
    pub summary: String,
    pub priority: i32,
    /// Repeats left, including the current one.
    pub repeats_left: u32,
    /// Whether it is on screen now.
    pub showing: bool,
}

/// What the panel is doing, published by the render loop whenever it
/// changes. Serialized as-is by `GET /api/state`.
#[derive(Debug, Clone, PartialEq, Serialize, Default)]
pub struct Status {
    /// Every configured scene, in playlist order.
    pub scenes: Vec<String>,
    /// Scene on screen (a notification's scene while one shows), `None` when
    /// nothing is eligible or an inline notification shows.
    pub scene: Option<String>,
    /// Pinned scene, `None` for "auto" (the playlist).
    pub pinned: Option<String>,
    pub notifications: Vec<QueuedNotification>,
    pub on: bool,
    /// User brightness 0–255.
    pub brightness: u8,
    /// Auto-dim factor 0–255 (255 = none).
    pub dim: u8,
    /// Frames rendered in the last second.
    pub fps: f32,
}

/// Make the control channels. `scenes` seeds [`Status::scenes`]; the render
/// loop keeps the status current from then on.
pub fn channel(scenes: Vec<String>) -> (Control, ControlLink, watch::Receiver<OutputLevel>) {
    let (tx, commands) = mpsc::channel();
    let initial = Status {
        scenes,
        on: true,
        brightness: 255,
        dim: 255,
        ..Default::default()
    };
    let (status_tx, status_rx) = watch::channel(Arc::new(initial));
    let (output_tx, output_rx) = watch::channel(OutputLevel::FULL);
    (
        Control {
            tx,
            status: status_rx,
        },
        ControlLink {
            commands,
            status: status_tx,
            output: output_tx,
        },
        output_rx,
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    fn req(json: &str) -> Result<Notification, String> {
        let r: NotifyRequest = serde_json::from_str(json).map_err(|e| e.to_string())?;
        r.validate(&["home".into(), "alert".into()])
    }

    #[test]
    fn inline_notification_defaults() {
        let n = req(r#"{"text": "Washing done"}"#).unwrap();
        assert_eq!(
            n,
            Notification {
                content: NotificationContent::Inline {
                    text: "Washing done".into(),
                    icon: None,
                    color: Color::WHITE
                },
                duration: Duration::from_secs(10),
                priority: 0,
                repeat: 1,
            }
        );
        let n = req(r##"{"text": "x", "icon": "bolt", "color": "#ff0000", "duration_s": 2.5, "priority": 5, "repeat": 3}"##).unwrap();
        assert_eq!(n.duration, Duration::from_millis(2500));
        assert_eq!((n.priority, n.repeat), (5, 3));
    }

    #[test]
    fn scene_notification() {
        let n = req(r#"{"scene": "alert", "priority": 9}"#).unwrap();
        assert_eq!(n.content, NotificationContent::Scene("alert".into()));
    }

    #[test]
    fn rejects_bad_requests() {
        for (json, want) in [
            (r#"{}"#, "one of `text` or `scene`"),
            (r#"{"text": "a", "scene": "home"}"#, "not both"),
            (r#"{"text": "  "}"#, "must not be empty"),
            (r#"{"text": "a", "icon": "umbrella"}"#, "unknown icon"),
            (r#"{"scene": "nope"}"#, "no scene named"),
            (
                r#"{"scene": "home", "icon": "sun"}"#,
                "only apply to `text`",
            ),
            (r#"{"text": "a", "duration_s": 0}"#, "`duration_s` must be"),
            (
                r#"{"text": "a", "duration_s": 99999}"#,
                "`duration_s` must be",
            ),
            (r#"{"text": "a", "priority": 1000}"#, "`priority` must be"),
            (r#"{"text": "a", "repeat": 0}"#, "`repeat` must be"),
            (r##"{"text": "a", "colour": "#fff"}"##, "unknown field"),
            (r#"{"text": "a", "color": "red"}"#, "invalid colour"),
        ] {
            let e = req(json).unwrap_err();
            assert!(e.contains(want), "{json}: {e}");
        }
        let long = format!(r#"{{"text": "{}"}}"#, "x".repeat(MAX_TEXT_LEN + 1));
        assert!(req(&long).unwrap_err().contains("longer than"));
    }

    #[test]
    fn output_level_combines_and_scales() {
        assert_eq!(OutputLevel::from_parts(true, 255, 255).level, 255);
        assert_eq!(OutputLevel::from_parts(true, 128, 255).level, 128);
        assert_eq!(OutputLevel::from_parts(true, 255, 128).level, 128);
        assert_eq!(OutputLevel::from_parts(true, 128, 128).level, 64);
        assert_eq!(OutputLevel::from_parts(true, 0, 255).level, 0);
        assert_eq!(OutputLevel::FULL.scale(180), 180);
        assert_eq!(OutputLevel::from_parts(true, 128, 255).scale(180), 90);
        assert_eq!(OutputLevel::from_parts(false, 255, 255).scale(180), 0);
    }

    #[test]
    fn control_sends_and_reads_status() {
        let (control, link, _out) = channel(vec!["a".into()]);
        control.send(Command::SetBrightness(10)).unwrap();
        assert_eq!(
            link.commands.try_recv().unwrap(),
            Command::SetBrightness(10)
        );
        assert_eq!(control.status().scenes, vec!["a".to_string()]);
        assert!(control.status().on);
        drop(link);
        assert!(control.send(Command::SetPower(false)).is_err());
    }
}
