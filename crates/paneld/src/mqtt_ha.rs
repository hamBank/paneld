//! MQTT with Home Assistant discovery (DESIGN.md §7.2).
//!
//! One tokio task publishes retained discovery configs so HA creates a single
//! `paneld` device, mirrors [`Status`] to retained state topics and turns
//! commands from HA into [`Command`]s on the shared [`Control`] handle. What
//! HA's discovery expects was checked against its docs, see
//! `mqtt_ha/HA_DISCOVERY.md`.
//!
//! Topics, with `B` = `base_topic` (default `paneld/<node_id>`), `P` =
//! `discovery_prefix` and `N` = `node_id`:
//!
//! | entity | config (retained) | state (retained) | command |
//! |---|---|---|---|
//! | light | `P/light/N/light/config` | `B/light/state` | `B/light/set` |
//! | select scene | `P/select/N/scene/config` | `B/scene/state` | `B/scene/set` |
//! | text message | `P/text/N/message/config` | none | `B/message/set` |
//! | binary_sensor card | `P/binary_sensor/N/card/config` | `B/card/state` | none |
//! | sensor fps | `P/sensor/N/fps/config` | `B/fps/state` | none |
//!
//! Also: `B/availability` (`online`/`offline`, retained, with `offline` as
//! the last will), `B/notify` (raw [`NotifyRequest`] JSON) and `P/status`
//! (HA's birth message).
//!
//! - **`base_topic` default `paneld/<node_id>`:** a private namespace per
//!   node, so two panels on one broker never collide and the topics do not
//!   look like part of HA's own tree.
//! - **Light brightness is the user brightness** ([`Status::brightness`]),
//!   not the effective level after auto-dim. HA's slider then shows what it
//!   set, and a command echoes back unchanged; reporting the dimmed level
//!   would make the slider jump around every time the schedule changes.
//! - **Republish:** everything (availability, configs, states) is sent on
//!   every ConnAck (clean session, so the broker may know nothing about us)
//!   and when HA's birth message says `online`. The select config is also
//!   sent again whenever [`Status::scenes`] changes (config reload).
//! - **QoS 0** throughout (HA's default for commands and states); the
//!   periodic republish on reconnect covers losses. The last will is QoS 1.
//! - **Secrets:** the password is a [`Password`] read from the env var named
//!   by `password_env`; `Debug` is redacted and it is never logged.
//! - **Restart-only:** `[mqtt]` is read at startup, like `[http]`.

use std::sync::atomic::Ordering;
use std::sync::Arc;
use std::time::{Duration, Instant};

use rumqttc::{
    AsyncClient, Event, LastWill, MqttOptions, Packet, QoS, SubscribeFilter, SubscribeReasonCode,
};
use serde_json::{json, Value as Json};
use tokio::sync::watch;

use crate::colorlight_sink::SinkStats;
use crate::config::{Config, MqttConfig};
use crate::control::{Command, Control, NotifyRequest, Status};
use crate::sources::mqtt::{env_name_ok, read_password, ClientSettings};

const RETRY_MIN: Duration = Duration::from_secs(1);
const RETRY_MAX: Duration = Duration::from_secs(30);
/// Request queue between us and rumqttc's event loop. One full republish is
/// about 12 packets; we poll in the same task, so never block on it.
const QUEUE: usize = 64;
/// The "auto" option of the scene select: release the pin.
pub const AUTO: &str = "auto";
const ONLINE: &str = "online";
const OFFLINE: &str = "offline";

// ---------------------------------------------------------------------------
// Config

fn name_ok(s: &str) -> bool {
    !s.is_empty()
        && s.len() <= 64
        && s.chars()
            .all(|c| c.is_ascii_alphanumeric() || c == '_' || c == '-')
}

fn topic_prefix_ok(s: &str) -> bool {
    !s.is_empty()
        && s.len() <= 200
        && !s.starts_with('/')
        && !s.ends_with('/')
        && !s.contains(['+', '#', '\0'])
        && !s.contains("//")
        && !s.starts_with('$')
}

pub fn validate(c: &MqttConfig, at: &str, errs: &mut Vec<String>) {
    if c.host.is_empty() || c.host.contains(|ch: char| ch.is_whitespace() || ch == '/') {
        errs.push(format!(
            "{at}: host must be a host name or IP address like \"homeassistant.local\" (no mqtt:// and no path)"
        ));
    }
    if c.port == 0 {
        errs.push(format!("{at}: port must be 1 to 65535"));
    }
    if c.username.as_deref().is_some_and(str::is_empty) {
        errs.push(format!(
            "{at}: username must not be empty (omit it instead)"
        ));
    }
    if let Some(var) = &c.password_env {
        if !env_name_ok(var) {
            errs.push(format!(
                "{at}: password_env must be the NAME of an environment variable (letters, digits and '_', e.g. \"PANELD_MQTT_PASSWORD\"), not the password itself"
            ));
        }
        if c.username.is_none() {
            errs.push(format!(
                "{at}: password_env needs a username (MQTT only sends a password with a username)"
            ));
        }
    }
    if !topic_prefix_ok(&c.discovery_prefix) {
        errs.push(format!(
            "{at}: discovery_prefix must be a topic like \"homeassistant\" (no wildcards, no leading or trailing '/')"
        ));
    }
    if !name_ok(&c.node_id) {
        errs.push(format!(
            "{at}: node_id may only use letters, digits, '_' and '-' (1 to 64 characters), as Home Assistant requires for discovery"
        ));
    }
    if let Some(b) = &c.base_topic {
        if !topic_prefix_ok(b) {
            errs.push(format!(
                "{at}: base_topic must be a topic like \"paneld/paneld\" (no wildcards, no leading or trailing '/')"
            ));
        }
    }
}

/// Fail early (before the daemon starts) when `password_env` is unset.
pub fn check_environment(cfg: &Config) -> anyhow::Result<()> {
    if let Some(var) = cfg.mqtt.as_ref().and_then(|m| m.password_env.as_deref()) {
        read_password(var).map_err(|e| anyhow::anyhow!("[mqtt]: {e}"))?;
    }
    Ok(())
}

// ---------------------------------------------------------------------------
// Topics and payloads (pure)

#[derive(Debug, Clone)]
struct Topics {
    prefix: String,
    node: String,
    base: String,
}

impl Topics {
    fn availability(&self) -> String {
        format!("{}/availability", self.base)
    }
    fn light_state(&self) -> String {
        format!("{}/light/state", self.base)
    }
    fn light_set(&self) -> String {
        format!("{}/light/set", self.base)
    }
    fn scene_state(&self) -> String {
        format!("{}/scene/state", self.base)
    }
    fn scene_set(&self) -> String {
        format!("{}/scene/set", self.base)
    }
    fn message_set(&self) -> String {
        format!("{}/message/set", self.base)
    }
    fn card_state(&self) -> String {
        format!("{}/card/state", self.base)
    }
    fn fps_state(&self) -> String {
        format!("{}/fps/state", self.base)
    }
    fn notify(&self) -> String {
        format!("{}/notify", self.base)
    }
    /// HA's birth (and last will) topic.
    fn birth(&self) -> String {
        format!("{}/status", self.prefix)
    }
    /// `<prefix>/<component>/<node_id>/<object_id>/config`.
    fn config(&self, component: &str, object: &str) -> String {
        format!("{}/{component}/{}/{object}/config", self.prefix, self.node)
    }
}

/// Keys every entity shares: identity, device, origin and availability.
fn common(t: &Topics, object: &str, name: Json) -> serde_json::Map<String, Json> {
    let version = env!("CARGO_PKG_VERSION");
    let mut m = serde_json::Map::new();
    m.insert("name".into(), name);
    m.insert("unique_id".into(), json!(format!("{}_{object}", t.node)));
    m.insert(
        "device".into(),
        json!({
            "identifiers": [t.node],
            "name": "paneld",
            "manufacturer": "paneld",
            "model": "LED panel controller",
            "sw_version": version,
        }),
    );
    m.insert(
        "origin".into(),
        json!({ "name": "paneld", "sw_version": version }),
    );
    m.insert("availability_topic".into(), json!(t.availability()));
    m.insert("payload_available".into(), json!(ONLINE));
    m.insert("payload_not_available".into(), json!(OFFLINE));
    m
}

fn with(mut m: serde_json::Map<String, Json>, extra: Json) -> Json {
    if let Json::Object(e) = extra {
        m.extend(e);
    }
    Json::Object(m)
}

fn light_config(t: &Topics) -> Json {
    with(
        // name null: the entity is the device itself (light.paneld).
        common(t, "light", Json::Null),
        json!({
            "schema": "json",
            "command_topic": t.light_set(),
            "state_topic": t.light_state(),
            "brightness": true,
            "brightness_scale": 255,
            "supported_color_modes": ["brightness"],
        }),
    )
}

fn select_config(t: &Topics, scenes: &[String]) -> Json {
    let mut options = vec![AUTO.to_owned()];
    options.extend(scenes.iter().filter(|s| *s != AUTO).cloned());
    with(
        common(t, "scene", json!("Scene")),
        json!({
            "command_topic": t.scene_set(),
            "state_topic": t.scene_state(),
            "options": options,
        }),
    )
}

fn text_config(t: &Topics) -> Json {
    with(
        common(t, "message", json!("Message")),
        json!({ "command_topic": t.message_set(), "min": 1, "max": 255, "mode": "text" }),
    )
}

fn card_config(t: &Topics) -> Json {
    with(
        common(t, "card", json!("Card")),
        json!({
            "state_topic": t.card_state(),
            "device_class": "connectivity",
            "entity_category": "diagnostic",
        }),
    )
}

fn fps_config(t: &Topics) -> Json {
    with(
        common(t, "fps", json!("FPS")),
        json!({
            "state_topic": t.fps_state(),
            "unit_of_measurement": "fps",
            "state_class": "measurement",
            "suggested_display_precision": 0,
            "entity_category": "diagnostic",
        }),
    )
}

/// The light's state payload: user brightness (see the module docs).
fn light_state(s: &Status) -> String {
    json!({
        "state": if s.on { "ON" } else { "OFF" },
        "brightness": s.brightness,
        "color_mode": "brightness",
    })
    .to_string()
}

fn scene_state(s: &Status) -> String {
    s.pinned.clone().unwrap_or_else(|| AUTO.to_owned())
}

/// Commands for a payload on `B/light/set` (JSON schema): `state` and
/// `brightness`; everything else (`transition`, ...) is ignored. Brightness
/// first, so turning on at a level lights up at that level.
fn parse_light_command(payload: &[u8]) -> Result<Vec<Command>, String> {
    let v: Json = serde_json::from_slice(payload).map_err(|e| format!("not JSON: {e}"))?;
    let obj = v.as_object().ok_or("not a JSON object")?;
    let mut out = Vec::new();
    if let Some(b) = obj.get("brightness") {
        let n = b
            .as_f64()
            .filter(|n| (0.0..=255.0).contains(n))
            .ok_or("brightness must be a number 0 to 255")?;
        out.push(Command::SetBrightness(n.round() as u8));
    }
    if let Some(s) = obj.get("state") {
        match s.as_str().map(str::to_ascii_uppercase).as_deref() {
            Some("ON") => out.push(Command::SetPower(true)),
            Some("OFF") => out.push(Command::SetPower(false)),
            _ => return Err("state must be \"ON\" or \"OFF\"".into()),
        }
    }
    if out.is_empty() {
        return Err("neither state nor brightness".into());
    }
    Ok(out)
}

fn parse_scene_command(payload: &[u8], scenes: &[String]) -> Result<Command, String> {
    let name = std::str::from_utf8(payload)
        .map_err(|_| "not UTF-8")?
        .trim();
    if name == AUTO {
        return Ok(Command::Pin(None));
    }
    if scenes.iter().any(|s| s == name) {
        Ok(Command::Pin(Some(name.to_owned())))
    } else {
        Err(format!("no scene named {name:?}"))
    }
}

fn notify_from_json(payload: &[u8], scenes: &[String]) -> Result<Command, String> {
    let req: NotifyRequest =
        serde_json::from_slice(payload).map_err(|e| format!("invalid JSON: {e}"))?;
    req.validate(scenes).map(Command::Notify)
}

fn notify_from_text(payload: &[u8], scenes: &[String]) -> Result<Command, String> {
    let text = std::str::from_utf8(payload).map_err(|_| "not UTF-8")?;
    // Through serde so the defaults (10 s, priority 0, repeat 1) are the
    // same ones HTTP uses.
    let req: NotifyRequest =
        serde_json::from_value(json!({ "text": text })).map_err(|e| e.to_string())?;
    req.validate(scenes).map(Command::Notify)
}

/// Whether the fps sensor should be published: more than 1 fps off the last
/// published value, or any change once `every` has passed.
fn fps_due(last: Option<(f32, Instant)>, now: Instant, fps: f32, every: Duration) -> bool {
    match last {
        None => true,
        Some((v, at)) => {
            (fps - v).abs() > 1.0
                || (format!("{fps:.1}") != format!("{v:.1}") && now.duration_since(at) >= every)
        }
    }
}

// ---------------------------------------------------------------------------
// Card

/// Derives the `card` binary_sensor from the Colorlight sinks' counters:
/// ON when, since the last sample, every sink sent frames and none had a
/// send error.
#[derive(Debug)]
pub struct CardMonitor {
    sinks: Vec<(Arc<SinkStats>, u64, u64)>,
}

impl CardMonitor {
    /// `None` when there is no Colorlight output (no entity then).
    pub fn new(stats: Vec<Arc<SinkStats>>) -> Option<CardMonitor> {
        if stats.is_empty() {
            return None;
        }
        Some(CardMonitor {
            sinks: stats.into_iter().map(|s| (s, 0, 0)).collect(),
        })
    }

    /// Compare the counters with the previous sample.
    pub fn sample(&mut self) -> bool {
        let mut ok = true;
        for (s, frames, errors) in &mut self.sinks {
            let (f, e) = (
                s.frames_sent.load(Ordering::Relaxed),
                s.send_errors.load(Ordering::Relaxed),
            );
            ok &= f > *frames && e == *errors;
            (*frames, *errors) = (f, e);
        }
        ok
    }
}

// ---------------------------------------------------------------------------
// The task

/// Cadences, overridable by tests.
#[derive(Debug, Clone, Copy)]
struct Timing {
    /// How often the loop wakes to sample the card and the fps.
    tick: Duration,
    card_every: Duration,
    fps_every: Duration,
    retry_min: Duration,
}

impl Default for Timing {
    fn default() -> Self {
        Timing {
            tick: Duration::from_secs(1),
            card_every: Duration::from_secs(5),
            fps_every: Duration::from_secs(10),
            retry_min: RETRY_MIN,
        }
    }
}

/// The Home Assistant MQTT client. `Debug` is safe to log.
#[derive(Debug)]
pub struct HaMqtt {
    settings: ClientSettings,
    topics: Topics,
    timing: Timing,
}

impl HaMqtt {
    pub fn new(c: &MqttConfig) -> anyhow::Result<Self> {
        let password = c
            .password_env
            .as_deref()
            .map(read_password)
            .transpose()
            .map_err(|e| anyhow::anyhow!("[mqtt]: {e}"))?;
        let settings = ClientSettings {
            host: c.host.clone(),
            port: c.port,
            username: c.username.clone(),
            password,
            client_id: format!("paneld-ha-{}", c.node_id),
            keep_alive: Duration::from_secs(30),
        };
        let base = c
            .base_topic
            .clone()
            .unwrap_or_else(|| format!("paneld/{}", c.node_id));
        Ok(HaMqtt {
            settings,
            topics: Topics {
                prefix: c.discovery_prefix.clone(),
                node: c.node_id.clone(),
                base,
            },
            timing: Timing::default(),
        })
    }

    fn options(&self) -> MqttOptions {
        let mut o = self.settings.options();
        // §3.1.2.5 to §3.1.2.7: broker publishes this if we vanish.
        o.set_last_will(LastWill::new(
            self.topics.availability(),
            OFFLINE,
            QoS::AtLeastOnce,
            true,
        ));
        o
    }

    /// Run until `shutdown` turns true. `card` is `None` without a
    /// Colorlight output.
    pub async fn run(
        self,
        control: Control,
        card: Option<CardMonitor>,
        mut shutdown: watch::Receiver<bool>,
    ) {
        let (client, mut events) = AsyncClient::new(self.options(), QUEUE);
        let mut r = Runner {
            t: &self.topics,
            timing: self.timing,
            client: &client,
            control: &control,
            card,
            connected: false,
            sent: Sent::default(),
            card_now: None,
            last_card: None,
        };
        let mut status = control.subscribe();
        let mut status_open = true;
        let mut delay = self.timing.retry_min;
        let mut tick = tokio::time::interval(self.timing.tick);
        tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
        loop {
            tokio::select! {
                e = events.poll() => match e {
                    Ok(Event::Incoming(Packet::ConnAck(_))) => {
                        delay = self.timing.retry_min;
                        tracing::info!(host = %self.settings.host, "mqtt (home assistant) connected");
                        r.connected = true;
                        r.subscribe();
                        r.publish_all();
                    }
                    Ok(Event::Incoming(Packet::SubAck(a))) => {
                        if a.return_codes.iter().any(|c| matches!(c, SubscribeReasonCode::Failure)) {
                            tracing::warn!("broker refused a subscription (check topic permissions)");
                        }
                    }
                    Ok(Event::Incoming(Packet::Publish(p))) => r.incoming(&p.topic, &p.payload),
                    Ok(_) => {}
                    Err(e) => {
                        r.connected = false;
                        tracing::warn!(error = %e, retry_in = ?delay, "mqtt (home assistant) connection lost");
                        let stop = tokio::select! {
                            () = tokio::time::sleep(delay) => false,
                            () = stopped(&mut shutdown) => true,
                        };
                        if stop {
                            return;
                        }
                        delay = (delay * 2).min(RETRY_MAX);
                    }
                },
                c = status.changed(), if status_open => {
                    if c.is_err() {
                        status_open = false;
                    }
                    r.publish_states();
                }
                _ = tick.tick() => {
                    r.sample_card();
                    r.publish_states();
                }
                () = stopped(&mut shutdown) => break,
            }
        }
        // A clean DISCONNECT does not fire the will (§3.14.4), so say it.
        if r.connected {
            r.put(&r.t.availability(), OFFLINE, true);
            let _ = client.try_disconnect();
            // Poll so the queued packets are actually written.
            let _ = tokio::time::timeout(Duration::from_secs(1), async {
                while events.poll().await.is_ok() {}
            })
            .await;
        }
    }
}

/// What was last published, to send only changes.
#[derive(Default)]
struct Sent {
    light: Option<String>,
    scene: Option<String>,
    scenes: Option<Vec<String>>,
    fps: Option<(f32, Instant)>,
    card: Option<bool>,
}

struct Runner<'a> {
    t: &'a Topics,
    timing: Timing,
    client: &'a AsyncClient,
    control: &'a Control,
    card: Option<CardMonitor>,
    connected: bool,
    sent: Sent,
    card_now: Option<bool>,
    last_card: Option<Instant>,
}

impl Runner<'_> {
    fn put(&self, topic: &str, payload: impl Into<Vec<u8>>, retain: bool) {
        if let Err(e) = self
            .client
            .try_publish(topic, QoS::AtMostOnce, retain, payload)
        {
            tracing::warn!(topic, error = %e, "mqtt publish dropped");
        }
    }

    fn subscribe(&self) {
        let filters = [
            self.t.light_set(),
            self.t.scene_set(),
            self.t.message_set(),
            self.t.notify(),
            self.t.birth(),
        ]
        .into_iter()
        .map(|t| SubscribeFilter::new(t, QoS::AtMostOnce));
        if let Err(e) = self.client.try_subscribe_many(filters) {
            tracing::warn!(error = %e, "mqtt subscribe failed");
        }
    }

    /// Availability, every config and every state, from scratch.
    fn publish_all(&mut self) {
        self.sent = Sent {
            card: None,
            ..Sent::default()
        };
        self.put(&self.t.availability(), ONLINE, true);
        let t = self.t;
        self.put(
            &t.config("light", "light"),
            light_config(t).to_string(),
            true,
        );
        self.put(
            &t.config("text", "message"),
            text_config(t).to_string(),
            true,
        );
        self.put(&t.config("sensor", "fps"), fps_config(t).to_string(), true);
        // Select: sent with the scene list from publish_states.
        let card_topic = t.config("binary_sensor", "card");
        if self.card.is_some() {
            self.put(&card_topic, card_config(t).to_string(), true);
        } else {
            // No Colorlight output: remove an entity an earlier config left.
            self.put(&card_topic, "", true);
        }
        self.publish_states();
    }

    fn publish_states(&mut self) {
        if !self.connected {
            return;
        }
        let s = self.control.status();
        let t = self.t;
        if self.sent.scenes.as_ref() != Some(&s.scenes) {
            self.put(
                &t.config("select", "scene"),
                select_config(t, &s.scenes).to_string(),
                true,
            );
            self.sent.scenes = Some(s.scenes.clone());
        }
        let light = light_state(&s);
        if self.sent.light.as_ref() != Some(&light) {
            self.put(&t.light_state(), light.clone(), true);
            self.sent.light = Some(light);
        }
        let scene = scene_state(&s);
        if self.sent.scene.as_ref() != Some(&scene) {
            self.put(&t.scene_state(), scene.clone(), true);
            self.sent.scene = Some(scene);
        }
        if fps_due(self.sent.fps, Instant::now(), s.fps, self.timing.fps_every) {
            self.put(&t.fps_state(), format!("{:.1}", s.fps), true);
            self.sent.fps = Some((s.fps, Instant::now()));
        }
        if let Some(on) = self.card_now {
            if self.sent.card != Some(on) {
                self.put(&t.card_state(), if on { "ON" } else { "OFF" }, true);
                self.sent.card = Some(on);
            }
        }
    }

    /// Sample the sink counters every `card_every`.
    fn sample_card(&mut self) {
        let Some(card) = &mut self.card else { return };
        if self
            .last_card
            .is_none_or(|at| at.elapsed() >= self.timing.card_every)
        {
            self.last_card = Some(Instant::now());
            self.card_now = Some(card.sample());
        }
    }

    fn incoming(&mut self, topic: &str, payload: &[u8]) {
        let t = self.t;
        let scenes = self.control.status().scenes.clone();
        let result = if topic == t.birth() {
            // HA (re)started: it has forgotten our entities.
            if payload.trim_ascii() == ONLINE.as_bytes() {
                tracing::info!("home assistant is online, republishing discovery");
                self.publish_all();
            }
            return;
        } else if topic == t.light_set() {
            parse_light_command(payload).map_err(|e| ("light command", e))
        } else if topic == t.scene_set() {
            parse_scene_command(payload, &scenes)
                .map(|c| vec![c])
                .map_err(|e| ("scene command", e))
        } else if topic == t.message_set() {
            notify_from_text(payload, &scenes)
                .map(|c| vec![c])
                .map_err(|e| ("message", e))
        } else if topic == t.notify() {
            notify_from_json(payload, &scenes)
                .map(|c| vec![c])
                .map_err(|e| ("notify request", e))
        } else {
            return;
        };
        match result {
            Ok(cmds) => {
                for c in cmds {
                    if let Err(e) = self.control.send(c) {
                        tracing::warn!(error = %e, "mqtt command not delivered");
                    }
                }
            }
            Err((what, e)) => tracing::warn!(topic, "ignoring invalid {what}: {e}"),
        }
    }
}

/// Resolves when shutdown is requested (or the sender is gone).
async fn stopped(rx: &mut watch::Receiver<bool>) {
    let _ = rx.wait_for(|&s| s).await;
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::control::{channel, ControlLink, NotificationContent};
    use crate::test_broker::{FakeBroker, Seen, Will};
    use std::collections::HashMap;
    use tokio::task::JoinHandle;

    fn mqtt_cfg(port: u16, extra: &str) -> MqttConfig {
        toml::from_str(&format!("host = \"127.0.0.1\"\nport = {port}\n{extra}")).unwrap()
    }

    /// Everything the broker saw, in order, plus the latest payload per topic.
    #[derive(Default)]
    struct Log {
        all: Vec<(String, String, bool)>,
        last: HashMap<String, (String, bool)>,
        connects: Vec<Seen>,
        subs: Vec<Vec<String>>,
    }

    impl Log {
        fn count(&self, topic: &str) -> usize {
            self.all.iter().filter(|(t, _, _)| t == topic).count()
        }
        fn json(&self, topic: &str) -> Json {
            serde_json::from_str(&self.last[topic].0).unwrap()
        }
        fn text(&self, topic: &str) -> Option<&str> {
            self.last.get(topic).map(|(p, _)| p.as_str())
        }
    }

    struct Rig {
        broker: FakeBroker,
        log: Log,
        link: ControlLink,
        stop: watch::Sender<bool>,
        task: JoinHandle<()>,
    }

    impl Rig {
        async fn start(scenes: &[&str], extra: &str, card: Option<CardMonitor>) -> Rig {
            let broker = FakeBroker::start().await;
            let ha = HaMqtt::new(&mqtt_cfg(broker.port, extra)).unwrap();
            Rig::start_with(broker, ha, scenes, card)
        }

        fn start_with(
            broker: FakeBroker,
            mut ha: HaMqtt,
            scenes: &[&str],
            card: Option<CardMonitor>,
        ) -> Rig {
            ha.timing = Timing {
                tick: Duration::from_millis(20),
                card_every: Duration::from_millis(60),
                fps_every: Duration::from_secs(3600),
                retry_min: Duration::from_millis(10),
            };
            let (control, link, _out) = channel(scenes.iter().map(|s| s.to_string()).collect());
            let (stop, rx) = watch::channel(false);
            let task = tokio::spawn(ha.run(control, card, rx));
            Rig {
                broker,
                log: Log::default(),
                link,
                stop,
                task,
            }
        }

        fn record(&mut self, seen: Seen) {
            match seen {
                Seen::Publish {
                    topic,
                    payload,
                    retain,
                } => {
                    let p = String::from_utf8(payload).unwrap();
                    self.log.all.push((topic.clone(), p.clone(), retain));
                    self.log.last.insert(topic, (p, retain));
                }
                Seen::Subscribe(s) => self.log.subs.push(s),
                c @ Seen::Connect { .. } => self.log.connects.push(c),
            }
        }

        /// Read what the broker sees until `cond` holds.
        async fn until(&mut self, what: &str, cond: impl Fn(&Log) -> bool) {
            while !cond(&self.log) {
                let seen = tokio::time::timeout(Duration::from_secs(5), self.broker.next_seen())
                    .await
                    .unwrap_or_else(|_| panic!("timed out: {what}"));
                self.record(seen);
            }
        }

        /// Record whatever arrives within `d`.
        async fn settle(&mut self, d: Duration) {
            let end = tokio::time::Instant::now() + d;
            while let Ok(s) = tokio::time::timeout_at(end, self.broker.next_seen()).await {
                self.record(s);
            }
        }

        /// Wait for the initial burst (the select's state is the last one).
        async fn ready(&mut self) {
            self.until("initial publish", |l| {
                !l.subs.is_empty() && l.last.contains_key("paneld/paneld/scene/state")
            })
            .await;
        }

        async fn command(&mut self, topic: &str, payload: &str) -> Vec<Command> {
            self.broker.publish(topic, payload, false);
            let deadline = Instant::now() + Duration::from_millis(400);
            let mut got = Vec::new();
            while Instant::now() < deadline {
                while let Ok(c) = self.link.commands.try_recv() {
                    got.push(c);
                }
                if !got.is_empty() {
                    break;
                }
                tokio::time::sleep(Duration::from_millis(5)).await;
            }
            got
        }

        async fn shut_down(self) {
            let _ = self.stop.send(true);
            tokio::time::timeout(Duration::from_secs(5), self.task)
                .await
                .expect("stops promptly")
                .unwrap();
        }
    }

    fn status(f: impl FnOnce(&mut Status)) -> Arc<Status> {
        let mut s = Status {
            scenes: vec!["home".into(), "alert".into()],
            on: true,
            brightness: 255,
            dim: 255,
            ..Default::default()
        };
        f(&mut s);
        Arc::new(s)
    }

    const CONFIGS: [&str; 5] = [
        "homeassistant/light/paneld/light/config",
        "homeassistant/select/paneld/scene/config",
        "homeassistant/text/paneld/message/config",
        "homeassistant/binary_sensor/paneld/card/config",
        "homeassistant/sensor/paneld/fps/config",
    ];

    #[tokio::test]
    async fn publishes_retained_discovery_for_each_entity() {
        let stats = Arc::new(SinkStats::default());
        let mut r = Rig::start(&["home", "alert"], "", CardMonitor::new(vec![stats])).await;
        r.ready().await;
        r.until("all configs", |l| {
            CONFIGS.iter().all(|k| l.last.contains_key(*k))
        })
        .await;
        let l = &r.log;
        for (topic, (_, retain)) in &l.last {
            if topic.starts_with("homeassistant/") || topic.ends_with("availability") {
                assert!(retain, "{topic} is retained");
            }
        }
        assert_eq!(l.text("paneld/paneld/availability"), Some("online"));

        let light = l.json(CONFIGS[0]);
        assert_eq!(light["schema"], "json");
        assert_eq!(light["name"], Json::Null);
        assert_eq!(light["unique_id"], "paneld_light");
        assert_eq!(light["command_topic"], "paneld/paneld/light/set");
        assert_eq!(light["state_topic"], "paneld/paneld/light/state");
        assert_eq!(light["brightness"], true);
        assert_eq!(light["brightness_scale"], 255);
        assert_eq!(light["availability_topic"], "paneld/paneld/availability");
        assert_eq!(light["device"]["identifiers"], json!(["paneld"]));
        assert_eq!(light["device"]["name"], "paneld");
        assert_eq!(light["device"]["sw_version"], env!("CARGO_PKG_VERSION"));
        assert_eq!(light["origin"]["name"], "paneld");

        let sel = l.json(CONFIGS[1]);
        assert_eq!(sel["options"], json!(["auto", "home", "alert"]));
        assert_eq!(sel["command_topic"], "paneld/paneld/scene/set");
        assert_eq!(sel["state_topic"], "paneld/paneld/scene/state");

        let text = l.json(CONFIGS[2]);
        assert_eq!(text["command_topic"], "paneld/paneld/message/set");
        assert_eq!(text["max"], 255);

        let card = l.json(CONFIGS[3]);
        assert_eq!(card["state_topic"], "paneld/paneld/card/state");
        assert_eq!(card["device_class"], "connectivity");

        let fps = l.json(CONFIGS[4]);
        assert_eq!(fps["state_topic"], "paneld/paneld/fps/state");
        assert_eq!(fps["unit_of_measurement"], "fps");
        // Every entity shares the one device.
        for k in CONFIGS {
            assert_eq!(l.json(k)["device"], light["device"], "{k}");
        }

        // States, with user brightness.
        assert_eq!(
            l.json("paneld/paneld/light/state"),
            json!({"state": "ON", "brightness": 255, "color_mode": "brightness"})
        );
        assert_eq!(l.text("paneld/paneld/scene/state"), Some("auto"));
        r.shut_down().await;
    }

    #[tokio::test]
    async fn no_colorlight_output_removes_the_card_entity() {
        let mut r = Rig::start(&["home"], "", None).await;
        r.until("empty card config", |l| l.text(CONFIGS[3]) == Some(""))
            .await;
        assert!(r.log.last[CONFIGS[3]].1);
        r.shut_down().await;
    }

    #[tokio::test]
    async fn connect_carries_the_will_and_subscribes_to_command_topics() {
        let mut r = Rig::start(&["home"], "", None).await;
        r.ready().await;
        // MQTT 3.1.1 §3.1.2.5 to §3.1.2.7 (will flag, QoS, retain) and
        // §3.1.3 (will topic and message in the payload).
        match &r.log.connects[0] {
            Seen::Connect {
                client_id, will, ..
            } => {
                assert_eq!(client_id, "paneld-ha-paneld");
                assert_eq!(
                    will.as_ref().unwrap(),
                    &Will {
                        topic: "paneld/paneld/availability".into(),
                        payload: b"offline".to_vec(),
                        retain: true,
                        qos: 1,
                    }
                );
            }
            o => panic!("{o:?}"),
        }
        let mut subs = r.log.subs[0].clone();
        subs.sort();
        assert_eq!(
            subs,
            [
                "homeassistant/status",
                "paneld/paneld/light/set",
                "paneld/paneld/message/set",
                "paneld/paneld/notify",
                "paneld/paneld/scene/set",
            ]
        );
        r.shut_down().await;
    }

    #[tokio::test]
    async fn light_commands_map_to_power_and_brightness() {
        let mut r = Rig::start(&["home"], "", None).await;
        r.ready().await;
        let set = "paneld/paneld/light/set";
        let c = r.command(set, r#"{"state":"ON","brightness":128}"#).await;
        assert_eq!(c, [Command::SetBrightness(128), Command::SetPower(true)]);
        let c = r.command(set, r#"{"state":"OFF"}"#).await;
        assert_eq!(c, [Command::SetPower(false)]);
        let c = r.command(set, r#"{"brightness":10,"transition":2}"#).await;
        assert_eq!(c, [Command::SetBrightness(10)]);
        for bad in [
            "nope",
            r#"{"state":"MAYBE"}"#,
            r#"{"brightness":999}"#,
            "{}",
        ] {
            assert!(r.command(set, bad).await.is_empty(), "{bad}");
        }
        r.shut_down().await;
    }

    #[tokio::test]
    async fn scene_and_message_commands() {
        let mut r = Rig::start(&["home", "alert"], "", None).await;
        r.ready().await;
        let c = r.command("paneld/paneld/scene/set", "alert").await;
        assert_eq!(c, [Command::Pin(Some("alert".into()))]);
        let c = r.command("paneld/paneld/scene/set", "auto").await;
        assert_eq!(c, [Command::Pin(None)]);
        assert!(r
            .command("paneld/paneld/scene/set", "nope")
            .await
            .is_empty());

        let c = r.command("paneld/paneld/message/set", "Washing done").await;
        match &c[..] {
            [Command::Notify(n)] => {
                assert_eq!(
                    n.content,
                    NotificationContent::Inline {
                        text: "Washing done".into(),
                        icon: None,
                        color: crate::color::Color::WHITE
                    }
                );
                assert_eq!(
                    (n.duration, n.priority, n.repeat),
                    (Duration::from_secs(10), 0, 1)
                );
            }
            o => panic!("{o:?}"),
        }
        assert!(r
            .command("paneld/paneld/message/set", "  ")
            .await
            .is_empty());
        r.shut_down().await;
    }

    #[tokio::test]
    async fn notify_topic_takes_raw_json_and_ignores_invalid() {
        let mut r = Rig::start(&["home", "alert"], "", None).await;
        r.ready().await;
        let topic = "paneld/paneld/notify";
        let c = r
            .command(topic, r#"{"scene":"alert","priority":9,"duration_s":3}"#)
            .await;
        match &c[..] {
            [Command::Notify(n)] => {
                assert_eq!(n.content, NotificationContent::Scene("alert".into()));
                assert_eq!((n.priority, n.duration), (9, Duration::from_secs(3)));
            }
            o => panic!("{o:?}"),
        }
        for bad in [
            "garbage",
            r#"{}"#,
            r#"{"scene":"missing"}"#,
            r#"{"text":"a","repeat":0}"#,
            r#"{"text":"a","bogus":1}"#,
        ] {
            assert!(r.command(topic, bad).await.is_empty(), "{bad}");
        }
        r.shut_down().await;
    }

    #[tokio::test]
    async fn birth_message_and_reconnect_republish_everything() {
        let mut r = Rig::start(&["home"], "", None).await;
        r.ready().await;
        assert_eq!(r.log.count(CONFIGS[0]), 1);

        r.broker.publish("homeassistant/status", "offline", false);
        r.broker.publish("homeassistant/status", "online", false);
        r.until("republished", |l| l.count(CONFIGS[0]) == 2).await;
        r.until("republished state", |l| {
            l.count("paneld/paneld/light/state") == 2
        })
        .await;
        assert_eq!(r.log.count(CONFIGS[1]), 2);
        assert_eq!(r.log.count("paneld/paneld/availability"), 2);

        r.broker.drop_connection();
        r.until("republished after reconnect", |l| l.count(CONFIGS[0]) == 3)
            .await;
        assert_eq!(r.log.connects.len(), 2);
        r.shut_down().await;
    }

    #[tokio::test]
    async fn state_changes_and_a_new_scene_list_are_published() {
        let mut r = Rig::start(&["home"], "", None).await;
        r.ready().await;
        r.link.status.send_replace(status(|s| {
            s.scenes = vec!["home".into(), "extra".into()];
            s.pinned = Some("extra".into());
            s.on = false;
            s.brightness = 40;
        }));
        r.until("new state", |l| {
            l.text("paneld/paneld/scene/state") == Some("extra")
        })
        .await;
        r.until("new options", |l| {
            l.json(CONFIGS[1])["options"] == json!(["auto", "home", "extra"])
        })
        .await;
        r.until("light", |l| {
            l.json("paneld/paneld/light/state")["state"] == "OFF"
        })
        .await;
        assert_eq!(r.log.json("paneld/paneld/light/state")["brightness"], 40);
        // A status change that touches none of it publishes nothing new.
        let n = r.log.count(CONFIGS[1]);
        r.link.status.send_replace(status(|s| {
            s.scenes = vec!["home".into(), "extra".into()];
            s.pinned = Some("extra".into());
            s.on = false;
            s.brightness = 40;
            s.scene = Some("extra".into());
        }));
        r.settle(Duration::from_millis(200)).await;
        assert_eq!(r.log.count(CONFIGS[1]), n);
        r.shut_down().await;
    }

    #[tokio::test]
    async fn card_follows_the_sink_counters() {
        let stats = Arc::new(SinkStats::default());
        let mut r = Rig::start(&["home"], "", CardMonitor::new(vec![stats.clone()])).await;
        r.until("card off", |l| {
            l.text("paneld/paneld/card/state") == Some("OFF")
        })
        .await;
        // Frames advancing, no errors: on.
        let keep = stats.clone();
        let pump = tokio::spawn(async move {
            loop {
                keep.frames_sent.fetch_add(1, Ordering::Relaxed);
                tokio::time::sleep(Duration::from_millis(5)).await;
            }
        });
        r.until("card on", |l| {
            l.text("paneld/paneld/card/state") == Some("ON")
        })
        .await;
        // A send error in the window turns it off again.
        stats.send_errors.fetch_add(1, Ordering::Relaxed);
        r.until("card off again", |l| {
            l.text("paneld/paneld/card/state") == Some("OFF")
        })
        .await;
        pump.abort();
        r.shut_down().await;
    }

    #[tokio::test]
    async fn shutdown_publishes_offline() {
        let mut r = Rig::start(&["home"], "", None).await;
        r.ready().await;
        let _ = r.stop.send(true);
        r.until("offline", |l| {
            l.all
                .last()
                .is_some_and(|(t, p, _)| t.ends_with("availability") && p == "offline")
        })
        .await;
        tokio::time::timeout(Duration::from_secs(5), r.task)
            .await
            .unwrap()
            .unwrap();
    }

    #[test]
    fn card_monitor_needs_frames_and_no_errors() {
        assert!(CardMonitor::new(vec![]).is_none());
        let s = Arc::new(SinkStats::default());
        let mut m = CardMonitor::new(vec![s.clone()]).unwrap();
        assert!(!m.sample(), "no frames yet");
        s.frames_sent.fetch_add(5, Ordering::Relaxed);
        assert!(m.sample());
        assert!(!m.sample(), "no new frames");
        s.frames_sent.fetch_add(5, Ordering::Relaxed);
        s.send_errors.fetch_add(1, Ordering::Relaxed);
        assert!(!m.sample(), "an error in the window");
        s.frames_sent.fetch_add(5, Ordering::Relaxed);
        assert!(m.sample(), "recovered");
    }

    #[test]
    fn fps_is_rate_limited() {
        let now = Instant::now();
        let every = Duration::from_secs(10);
        assert!(fps_due(None, now, 30.0, every));
        let last = Some((30.0, now));
        assert!(!fps_due(last, now + Duration::from_secs(1), 29.5, every));
        assert!(fps_due(last, now + Duration::from_secs(1), 28.5, every));
        assert!(!fps_due(last, now + Duration::from_secs(5), 29.7, every));
        assert!(fps_due(last, now + Duration::from_secs(11), 29.7, every));
        assert!(!fps_due(last, now + Duration::from_secs(11), 30.0, every));
    }

    #[test]
    fn config_validation() {
        let ok = mqtt_cfg(1883, "");
        let mut e = Vec::new();
        validate(&ok, "[mqtt]", &mut e);
        assert!(e.is_empty(), "{e:?}");
        assert_eq!(ok.discovery_prefix, "homeassistant");
        assert_eq!(ok.node_id, "paneld");

        for (extra, want) in [
            ("node_id = \"my panel\"", "node_id"),
            ("node_id = \"a/b\"", "node_id"),
            ("discovery_prefix = \"home/\"", "discovery_prefix"),
            ("base_topic = \"a/#\"", "base_topic"),
            ("base_topic = \"/a\"", "base_topic"),
            ("password_env = \"X\"", "needs a username"),
            (
                "username = \"u\"\npassword_env = \"hunter2!\"",
                "NAME of an environment",
            ),
            ("username = \"\"", "username"),
        ] {
            let mut e = Vec::new();
            validate(&mqtt_cfg(1883, extra), "[mqtt]", &mut e);
            assert!(e.iter().any(|m| m.contains(want)), "{extra}: {e:?}");
        }
        let mut e = Vec::new();
        let mut bad = mqtt_cfg(1883, "");
        bad.port = 0;
        bad.host = "mqtt://x".into();
        validate(&bad, "[mqtt]", &mut e);
        assert_eq!(e.len(), 2, "{e:?}");

        // Unknown fields are rejected, the section is optional and checked
        // as part of the whole config.
        assert!(toml::from_str::<MqttConfig>("host = \"h\"\nbogus = 1").is_err());
        let canvas = "[canvas]\nwidth = 8\nheight = 8\nfps = 10\n[[scene]]\nname = \"a\"\n";
        assert!(Config::parse(canvas).unwrap().mqtt.is_none());
        let e = Config::parse(&format!(
            "{canvas}[mqtt]\nhost = \"h\"\nnode_id = \"bad id\"\n"
        ))
        .unwrap_err();
        assert!(format!("{e:#}").contains("[mqtt]: node_id"), "{e:#}");
    }

    #[test]
    fn base_topic_default_and_override() {
        let h = HaMqtt::new(&mqtt_cfg(1, "node_id = \"hall\"")).unwrap();
        assert_eq!(h.topics.base, "paneld/hall");
        let h = HaMqtt::new(&mqtt_cfg(1, "base_topic = \"x/y\"")).unwrap();
        assert_eq!(h.topics.availability(), "x/y/availability");
    }

    #[tokio::test]
    async fn password_is_sent_but_never_shown() {
        const PW: &str = "hunter2-not-a-real-ha-password";
        std::env::set_var("PANELD_TEST_HA_MQTT_PW", PW);
        let broker = FakeBroker::start().await;
        let cfg = mqtt_cfg(
            broker.port,
            "username = \"panel\"\npassword_env = \"PANELD_TEST_HA_MQTT_PW\"\n",
        );
        let ha = HaMqtt::new(&cfg).unwrap();
        let shown = format!("{ha:?} | {cfg:?} | {:?}", ha.settings);
        assert!(!shown.contains(PW), "{shown}");
        assert!(shown.contains("<redacted>"));
        let mut r = Rig::start_with(broker, ha, &[], None);
        r.ready().await;
        match &r.log.connects[0] {
            Seen::Connect {
                username, password, ..
            } => {
                assert_eq!(username.as_deref(), Some("panel"));
                assert_eq!(password.as_deref(), Some(PW));
            }
            o => panic!("{o:?}"),
        }
        r.shut_down().await;

        let cfg = Config::parse(
            "[canvas]\nwidth = 8\nheight = 8\nfps = 10\n[[scene]]\nname = \"a\"\n[mqtt]\nhost = \"h\"\nusername = \"u\"\npassword_env = \"PANELD_TEST_HA_MQTT_UNSET\"\n",
        )
        .unwrap();
        let e = check_environment(&cfg).unwrap_err().to_string();
        assert!(
            e.contains("PANELD_TEST_HA_MQTT_UNSET") && e.contains("not set"),
            "{e}"
        );
    }
}
