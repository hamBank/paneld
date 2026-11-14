//! MQTT subscribe source (DESIGN.md §6 item 2): topics to store keys, with
//! optional JSON path extraction.
//!
//! Plain TCP only (see DESIGN.md §6 for why TLS is a follow-up). One
//! connection per source; `rumqttc`'s event loop reconnects by itself, so this
//! source only paces the retries and re-subscribes after every reconnect.
//!
//! - **Subscriptions:** QoS 0. Every value is "the latest reading"; a broker
//!   that replays a QoS 1 backlog after a reconnect would only show stale
//!   values for a moment, and retained messages (the real "last value") are
//!   delivered on subscribe at any QoS. The session is clean, so the broker
//!   forgets subscriptions on disconnect and rumqttc does *not* resend them:
//!   they are sent again on every ConnAck.
//! - **Payloads:** see [`decode`].
//! - **Wildcards** (`+`, `#`) are rejected in config: a wildcard topic would
//!   need a key per matching topic (templating), and one key per entry keeps
//!   the key list known at config load, like every other source.
//! - **Health:** `ctx.ok()` on ConnAck and on every publish, `ctx.error()` on
//!   connection errors. A payload that cannot be decoded only increments the
//!   error counter (the connection is fine, so `up` stays true).
//! - **Secrets:** the password comes from the env var named by
//!   `password_env`, is held in a [`Password`] whose `Debug` is redacted, and
//!   is never logged. Never `Debug`-print rumqttc's `MqttOptions`: it prints
//!   the credentials.
//!
//! [`ClientSettings`] is the connection half, kept apart from the topic
//! handling so M4's publishing client (DESIGN.md §7.2) can reuse it.

use std::collections::{BTreeMap, HashSet};
use std::fmt;
use std::sync::atomic::Ordering;
use std::time::Duration;

use rumqttc::{AsyncClient, Event, MqttOptions, Packet, QoS, SubscribeFilter, SubscribeReasonCode};
use serde_json::Value as Json;

use super::{Source, SourceCtx, SourceFuture};
use crate::config::MqttSourceConfig;
use crate::extract::{to_value, JsonPath};
use crate::store::{Change, Value};

/// Largest packet accepted (rumqttc's default is 10 KB, too small for some
/// JSON status messages).
const MAX_PACKET: usize = 256 * 1024;
/// Wait between reconnect attempts: 1 s doubling to 30 s, reset on ConnAck.
const RETRY_MIN: Duration = Duration::from_secs(1);
const RETRY_MAX: Duration = Duration::from_secs(30);

// ---------------------------------------------------------------------------
// Config validation

fn env_name_ok(n: &str) -> bool {
    !n.is_empty()
        && n.len() <= 64
        && !n.starts_with(|ch: char| ch.is_ascii_digit())
        && n.chars().all(|ch| ch.is_ascii_alphanumeric() || ch == '_')
}

pub fn validate(c: &MqttSourceConfig, at: &str, errs: &mut Vec<String>) {
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
                "{at}: password_env must be the NAME of an environment variable (letters, digits and '_', e.g. \"MQTT_PASSWORD\"), not the password itself"
            ));
        }
        if c.username.is_none() {
            errs.push(format!(
                "{at}: password_env needs a username (MQTT only sends a password with a username)"
            ));
        }
    }
    if let Some(id) = &c.client_id {
        if id.is_empty() || id.len() > 256 || id.contains('\0') {
            errs.push(format!("{at}: client_id must be 1 to 256 characters"));
        }
    }
    if !(1..=65_535).contains(&c.keep_alive_s) {
        errs.push(format!(
            "{at}: keep_alive_s must be 1 to 65535 (got {})",
            c.keep_alive_s
        ));
    }
    if c.topics.is_empty() {
        errs.push(format!(
            "{at}: topics is empty; add at least one, e.g. topics = {{ \"home:solar\" = \"home/power/solar\" }}"
        ));
    }
    for (key, spec) in &c.topics {
        if !crate::condition::is_valid_key(key) {
            errs.push(format!(
                "{at}.topics: {key:?} is not a valid key; keys look like `source:name`"
            ));
        }
        let topic = spec.topic();
        if topic.is_empty() || topic.contains('\0') || topic.len() > 65_535 {
            errs.push(format!(
                "{at}.topics.{key}: topic must be 1 to 65535 characters"
            ));
        } else if topic.contains(['+', '#']) {
            errs.push(format!(
                "{at}.topics.{key}: topic {topic:?} has a wildcard (+ or #); give each key one exact topic"
            ));
        }
        if let Some(p) = spec.path() {
            if let Err(e) = JsonPath::parse(p) {
                errs.push(format!("{at}.topics.{key}: path {p:?}: {e}"));
            }
        }
    }
}

// ---------------------------------------------------------------------------
// Connection settings (reusable by M4)

/// A password that cannot be printed by accident.
#[derive(Clone)]
pub struct Password(String);

impl fmt::Debug for Password {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("Password(<redacted>)")
    }
}

/// Read the password from the environment variable `var`. The error names
/// the variable, never a value.
pub fn read_password(var: &str) -> Result<Password, String> {
    match std::env::var(var) {
        Ok(v) if !v.is_empty() => Ok(Password(v)),
        Ok(_) => Err(format!(
            "environment variable {var} is empty; it must hold the MQTT password"
        )),
        Err(_) => Err(format!(
            "environment variable {var} is not set; it must hold the MQTT password"
        )),
    }
}

/// Everything needed to connect to a broker. `Debug` is safe to log.
#[derive(Debug, Clone)]
pub struct ClientSettings {
    pub host: String,
    pub port: u16,
    pub username: Option<String>,
    pub password: Option<Password>,
    pub client_id: String,
    pub keep_alive: Duration,
}

impl ClientSettings {
    /// Settings for source `name`; reads the password from its env var.
    pub fn from_source_config(name: &str, c: &MqttSourceConfig) -> Result<Self, String> {
        let password = c.password_env.as_deref().map(read_password).transpose()?;
        Ok(ClientSettings {
            host: c.host.clone(),
            port: c.port,
            username: c.username.clone(),
            password,
            client_id: c
                .client_id
                .clone()
                .unwrap_or_else(|| format!("paneld-{name}")),
            keep_alive: Duration::from_secs(c.keep_alive_s),
        })
    }

    /// rumqttc options: plain TCP, clean session. Never `Debug`-print the
    /// result (it includes the credentials).
    pub fn options(&self) -> MqttOptions {
        let mut o = MqttOptions::new(self.client_id.clone(), self.host.clone(), self.port);
        o.set_keep_alive(self.keep_alive.max(Duration::from_secs(1)));
        o.set_clean_session(true);
        o.set_max_packet_size(MAX_PACKET, MAX_PACKET);
        if let Some(user) = &self.username {
            o.set_credentials(user.clone(), self.password.as_ref().map_or("", |p| &p.0));
        }
        o
    }
}

// ---------------------------------------------------------------------------
// Payload decoding (pure, so it is unit-testable)

/// One store key fed from a topic.
#[derive(Debug, Clone)]
struct Route {
    key: String,
    path: Option<JsonPath>,
    unit: Option<String>,
}

/// What a payload means for the keys of its topic.
///
/// - Empty (or whitespace-only) payload: every key of the topic is removed
///   (that is how a retained message is deleted).
/// - With `path`: the payload must be JSON; the value at the path is stored.
///   `null` removes the key. A path that finds nothing, or a payload that is
///   not JSON, is a problem and leaves the key unchanged.
/// - Without `path`: JSON is tried first (`21.5` is a Number, `true` a Bool,
///   an object or array a Json value); anything else is the UTF-8 text,
///   trimmed, so `on` becomes Text. A string that looks like a number becomes
///   a Number either way (see `to_value`).
///
/// Returns the changes to apply in one batch and a description of each
/// problem.
fn decode(routes: &[Route], payload: &[u8]) -> (Vec<Change>, Vec<String>) {
    let mut changes = Vec::new();
    let mut problems = Vec::new();
    let text = payload.trim_ascii();
    if text.is_empty() {
        changes.extend(routes.iter().map(|r| Change::Remove(r.key.clone())));
        return (changes, problems);
    }
    let doc: Option<Json> = serde_json::from_slice(text).ok();
    for r in routes {
        let unit = r.unit.as_deref();
        let found: Result<Option<Value>, String> = match (&r.path, &doc) {
            (Some(p), Some(doc)) => match p.find(doc) {
                Some(v) => Ok(to_value(v, unit)),
                None => Err(format!("{}: path not found in payload", r.key)),
            },
            (Some(_), None) => Err(format!("{}: payload is not valid JSON", r.key)),
            (None, Some(doc)) => Ok(to_value(doc, unit)),
            (None, None) => match std::str::from_utf8(text) {
                Ok(s) => Ok(to_value(&Json::String(s.to_owned()), unit)),
                Err(_) => Err(format!("{}: payload is not valid UTF-8 text", r.key)),
            },
        };
        match found {
            Ok(Some(v)) => changes.push(Change::Set(r.key.clone(), v)),
            Ok(None) => changes.push(Change::Remove(r.key.clone())),
            Err(e) => problems.push(e),
        }
    }
    (changes, problems)
}

// ---------------------------------------------------------------------------
// The source

/// `type = "mqtt"`.
#[derive(Debug)]
pub struct MqttSource {
    settings: ClientSettings,
    /// Exact topic to the keys it feeds (several keys can share a topic, each
    /// with its own path).
    topics: BTreeMap<String, Vec<Route>>,
    retry_min: Duration,
}

impl MqttSource {
    pub fn new(name: &str, c: &MqttSourceConfig) -> anyhow::Result<Self> {
        let settings = ClientSettings::from_source_config(name, c)
            .map_err(|e| anyhow::anyhow!("[sources.{name}]: {e}"))?;
        let mut topics: BTreeMap<String, Vec<Route>> = BTreeMap::new();
        for (key, spec) in &c.topics {
            let path = spec
                .path()
                .map(JsonPath::parse)
                .transpose()
                .map_err(|e| anyhow::anyhow!("[sources.{name}.topics.{key}]: {e}"))?;
            topics
                .entry(spec.topic().to_owned())
                .or_default()
                .push(Route {
                    key: key.clone(),
                    path,
                    unit: spec.unit().map(str::to_owned),
                });
        }
        Ok(MqttSource {
            settings,
            topics,
            retry_min: RETRY_MIN,
        })
    }
}

impl Source for MqttSource {
    fn run(self: Box<Self>, mut ctx: SourceCtx) -> SourceFuture {
        Box::pin(async move {
            let (client, mut events) =
                AsyncClient::new(self.settings.options(), self.topics.len() + 16);
            let mut delay = self.retry_min;
            // Topics already reported as undecodable, so a chatty device
            // logs one warning, not one per message.
            let mut warned: HashSet<String> = HashSet::new();
            loop {
                let event = tokio::select! {
                    e = events.poll() => e,
                    () = ctx.shutdown.cancelled() => {
                        let _ = client.try_disconnect();
                        return Ok(());
                    }
                };
                match event {
                    Ok(Event::Incoming(Packet::ConnAck(_))) => {
                        delay = self.retry_min;
                        ctx.ok();
                        tracing::info!(source = %ctx.name, host = %self.settings.host, "mqtt connected");
                        // Clean session: the broker forgot our subscriptions.
                        // One SUBSCRIBE packet for all topics.
                        client
                            .try_subscribe_many(
                                self.topics
                                    .keys()
                                    .map(|t| SubscribeFilter::new(t.clone(), QoS::AtMostOnce)),
                            )
                            .map_err(|e| anyhow::anyhow!("subscribe: {e}"))?;
                    }
                    Ok(Event::Incoming(Packet::SubAck(ack))) => {
                        if ack
                            .return_codes
                            .iter()
                            .any(|c| matches!(c, SubscribeReasonCode::Failure))
                        {
                            ctx.health.errors.fetch_add(1, Ordering::Relaxed);
                            tracing::warn!(source = %ctx.name, "broker refused a subscription (check topic permissions)");
                        }
                    }
                    Ok(Event::Incoming(Packet::Publish(p))) => {
                        ctx.ok();
                        let Some(routes) = self.topics.get(&p.topic) else {
                            continue;
                        };
                        let (changes, problems) = decode(routes, &p.payload);
                        ctx.store.apply(changes);
                        if problems.is_empty() {
                            warned.remove(&p.topic);
                            continue;
                        }
                        ctx.health
                            .errors
                            .fetch_add(problems.len() as u64, Ordering::Relaxed);
                        let detail = problems.join("; ");
                        if warned.insert(p.topic.clone()) {
                            tracing::warn!(source = %ctx.name, topic = %p.topic, "mqtt payload ignored: {detail}");
                        } else {
                            tracing::debug!(source = %ctx.name, topic = %p.topic, "mqtt payload ignored: {detail}");
                        }
                    }
                    Ok(_) => {}
                    Err(e) => {
                        // rumqttc reconnects on the next poll, immediately
                        // and in a tight loop if the broker is down, so wait.
                        ctx.error();
                        tracing::warn!(source = %ctx.name, error = %e, retry_in = ?delay, "mqtt connection lost");
                        if !ctx.shutdown.sleep(delay).await {
                            return Ok(());
                        }
                        delay = (delay * 2).min(RETRY_MAX);
                    }
                }
            }
        })
    }
}

// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::{Config, SourceConfig};
    use crate::sources::{Shutdown, SourceHealth};
    use crate::store::Store;
    use std::sync::Arc;
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    use tokio::net::tcp::OwnedReadHalf;
    use tokio::net::TcpListener;
    use tokio::sync::{mpsc, watch};
    use tokio::task::JoinHandle;

    // ---- A minimal fake MQTT 3.1.1 broker -------------------------------
    //
    // Spec: MQTT Version 3.1.1, OASIS Standard, 29 October 2014,
    // https://docs.oasis-open.org/mqtt/mqtt/v3.1.1/os/mqtt-v3.1.1-os.html
    // Covered: CONNECT (read) and CONNACK, SUBSCRIBE (read) and SUBACK,
    // PUBLISH at QoS 0 (written), PINGREQ/PINGRESP, DISCONNECT, and a hard
    // connection drop. Nothing else (no QoS 1/2, no UNSUBSCRIBE, no wills).

    /// §2.2.3 Remaining Length: 7 bits per byte, least significant group
    /// first, bit 7 set when another byte follows.
    fn varint(mut n: usize) -> Vec<u8> {
        let mut out = Vec::new();
        loop {
            let mut b = (n % 128) as u8;
            n /= 128;
            if n > 0 {
                b |= 0x80;
            }
            out.push(b);
            if n == 0 {
                return out;
            }
        }
    }

    /// §2.2 fixed header: one byte (type in the high nibble, flags in the
    /// low nibble), then the Remaining Length, then the body.
    fn packet(first: u8, body: &[u8]) -> Vec<u8> {
        let mut p = vec![first];
        p.extend(varint(body.len()));
        p.extend_from_slice(body);
        p
    }

    /// §1.5.3 UTF-8 string: two-byte big-endian length, then the bytes.
    fn mqtt_str(s: &str) -> Vec<u8> {
        let mut v = (s.len() as u16).to_be_bytes().to_vec();
        v.extend_from_slice(s.as_bytes());
        v
    }

    /// §3.3 PUBLISH, type 3: flags are DUP (bit 3), QoS (bits 2-1) and
    /// RETAIN (bit 0). Variable header: topic name (§3.3.2), then a packet
    /// identifier only when QoS > 0 (§3.3.2.2), so none here. Then the payload.
    fn publish_packet(topic: &str, payload: &[u8], retain: bool) -> Vec<u8> {
        let mut body = mqtt_str(topic);
        body.extend_from_slice(payload);
        packet(0x30 | u8::from(retain), &body)
    }

    /// §3.2 CONNACK, type 2, remaining length 2: acknowledge flags (bit 0 =
    /// session present, 0 here) then the return code (0 = accepted).
    const CONNACK: [u8; 4] = [0x20, 0x02, 0x00, 0x00];

    /// §3.9 SUBACK, type 9: the SUBSCRIBE's packet identifier, then one
    /// return code per topic filter (0x00 = success at maximum QoS 0).
    fn suback(id: [u8; 2], filters: usize) -> Vec<u8> {
        let mut body = id.to_vec();
        body.extend(std::iter::repeat_n(0x00, filters));
        packet(0x90, &body)
    }

    /// §3.13 PINGRESP, type 13, no body.
    const PINGRESP: [u8; 2] = [0xD0, 0x00];

    /// Read one packet: fixed header byte, Remaining Length (§2.2.3), body.
    async fn read_packet(r: &mut OwnedReadHalf) -> std::io::Result<(u8, Vec<u8>)> {
        let first = r.read_u8().await?;
        let (mut len, mut mult) = (0usize, 1usize);
        loop {
            let b = r.read_u8().await?;
            len += usize::from(b & 0x7f) * mult;
            if b & 0x80 == 0 {
                break;
            }
            mult *= 128;
        }
        let mut body = vec![0; len];
        r.read_exact(&mut body).await?;
        Ok((first, body))
    }

    fn take_str(b: &[u8]) -> (String, &[u8]) {
        let n = usize::from(u16::from_be_bytes([b[0], b[1]]));
        (
            String::from_utf8(b[2..2 + n].to_vec()).unwrap(),
            &b[2 + n..],
        )
    }

    /// What the broker saw from the client.
    #[derive(Debug)]
    enum Seen {
        Connect {
            client_id: String,
            username: Option<String>,
            password: Option<String>,
        },
        Subscribe(Vec<String>),
    }

    enum Cmd {
        Publish {
            topic: String,
            payload: Vec<u8>,
            retain: bool,
        },
        Drop,
    }

    struct FakeBroker {
        port: u16,
        cmds: mpsc::UnboundedSender<Cmd>,
        seen: mpsc::UnboundedReceiver<Seen>,
    }

    impl FakeBroker {
        async fn start() -> FakeBroker {
            let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
            let port = listener.local_addr().unwrap().port();
            let (cmds, cmd_rx) = mpsc::unbounded_channel();
            let (seen_tx, seen) = mpsc::unbounded_channel();
            tokio::spawn(serve(listener, cmd_rx, seen_tx));
            FakeBroker { port, cmds, seen }
        }

        /// Send a QoS 0 PUBLISH to the connected client.
        fn publish(&self, topic: &str, payload: &str, retain: bool) {
            self.cmds
                .send(Cmd::Publish {
                    topic: topic.into(),
                    payload: payload.as_bytes().to_vec(),
                    retain,
                })
                .unwrap();
        }

        fn drop_connection(&self) {
            self.cmds.send(Cmd::Drop).unwrap();
        }

        async fn next_seen(&mut self) -> Seen {
            tokio::time::timeout(Duration::from_secs(5), self.seen.recv())
                .await
                .expect("broker saw nothing in 5 s")
                .expect("broker stopped")
        }

        /// Skip to the next SUBSCRIBE and return its topic filters.
        async fn next_subscribe(&mut self) -> Vec<String> {
            loop {
                if let Seen::Subscribe(t) = self.next_seen().await {
                    return t;
                }
            }
        }
    }

    /// Accept one connection at a time; serve it until it drops.
    async fn serve(
        listener: TcpListener,
        mut cmds: mpsc::UnboundedReceiver<Cmd>,
        seen: mpsc::UnboundedSender<Seen>,
    ) {
        loop {
            let Ok((stream, _)) = listener.accept().await else {
                return;
            };
            let (rd, mut wr) = stream.into_split();
            // Read on a separate task: reading is not cancel-safe inside select.
            let (pk_tx, mut pk_rx) = mpsc::unbounded_channel();
            let reader = tokio::spawn(async move {
                let mut rd = rd;
                while let Ok(p) = read_packet(&mut rd).await {
                    if pk_tx.send(p).is_err() {
                        break;
                    }
                }
            });
            loop {
                tokio::select! {
                    pkt = pk_rx.recv() => {
                        let Some((first, body)) = pkt else { break };
                        match first >> 4 {
                            // §3.1 CONNECT. Variable header: protocol name
                            // (string), level (1 byte, 4 = 3.1.1), connect
                            // flags (§3.1.2.3: bit 7 user name, bit 6
                            // password, bit 2 will), keep alive (2 bytes).
                            // Payload (§3.1.3): client id, [will topic, will
                            // message], [user name], [password].
                            1 => {
                                let (proto, rest) = take_str(&body);
                                assert_eq!(proto, "MQTT");
                                assert_eq!(rest[0], 4, "protocol level 4 = MQTT 3.1.1");
                                let flags = rest[1];
                                assert_eq!(flags & 0x04, 0, "no will expected");
                                let (client_id, rest) = take_str(&rest[4..]);
                                let (username, rest) = if flags & 0x80 != 0 {
                                    let (u, r) = take_str(rest);
                                    (Some(u), r)
                                } else {
                                    (None, rest)
                                };
                                let password = (flags & 0x40 != 0).then(|| take_str(rest).0);
                                let _ = seen.send(Seen::Connect { client_id, username, password });
                                let _ = wr.write_all(&CONNACK).await;
                            }
                            // §3.8 SUBSCRIBE: fixed header flags must be
                            // 0b0010; variable header is the packet
                            // identifier; payload is pairs of topic filter
                            // (string) and requested QoS (1 byte).
                            8 => {
                                assert_eq!(first & 0x0f, 0x02);
                                let id = [body[0], body[1]];
                                let mut rest = &body[2..];
                                let mut filters = Vec::new();
                                while !rest.is_empty() {
                                    let (f, r) = take_str(rest);
                                    assert_eq!(r[0], 0, "QoS 0 requested");
                                    filters.push(f);
                                    rest = &r[1..];
                                }
                                let n = filters.len();
                                let _ = seen.send(Seen::Subscribe(filters));
                                let _ = wr.write_all(&suback(id, n)).await;
                            }
                            // §3.12 PINGREQ -> §3.13 PINGRESP.
                            12 => {
                                let _ = wr.write_all(&PINGRESP).await;
                            }
                            // §3.14 DISCONNECT.
                            14 => break,
                            _ => {}
                        }
                    }
                    cmd = cmds.recv() => match cmd {
                        Some(Cmd::Publish { topic, payload, retain }) => {
                            let _ = wr.write_all(&publish_packet(&topic, &payload, retain)).await;
                        }
                        Some(Cmd::Drop) | None => break,
                    },
                }
            }
            reader.abort();
            // `wr` drops here, closing the connection.
        }
    }

    // ---- Test harness ---------------------------------------------------

    fn source_config(port: u16, extra: &str) -> MqttSourceConfig {
        toml::from_str(&format!("host = \"127.0.0.1\"\nport = {port}\n{extra}")).unwrap()
    }

    struct Running {
        store: Store,
        health: Arc<SourceHealth>,
        stop: watch::Sender<bool>,
        task: JoinHandle<anyhow::Result<()>>,
    }

    fn run(src: MqttSource) -> Running {
        let store = Store::new();
        let health = Arc::new(SourceHealth::default());
        let (stop, rx) = watch::channel(false);
        let ctx = SourceCtx {
            name: "m".into(),
            store: store.writer("m"),
            shutdown: Shutdown(rx),
            health: health.clone(),
        };
        let task = tokio::spawn(Box::new(src).run(ctx));
        Running {
            store,
            health,
            stop,
            task,
        }
    }

    fn fast(mut src: MqttSource) -> MqttSource {
        src.retry_min = Duration::from_millis(10);
        src
    }

    /// Start a source with `extra` TOML (the `[topics]` table etc.) against
    /// a fresh broker, and wait until it has subscribed.
    async fn started(extra: &str) -> (FakeBroker, Running, Vec<String>) {
        let mut broker = FakeBroker::start().await;
        let cfg = source_config(broker.port, extra);
        let r = run(fast(MqttSource::new("m", &cfg).unwrap()));
        let topics = broker.next_subscribe().await;
        (broker, r, topics)
    }

    impl Running {
        async fn wait_for(&self, what: &str, cond: impl Fn(&crate::store::StoreSnapshot) -> bool) {
            let deadline = std::time::Instant::now() + Duration::from_secs(5);
            while !cond(&self.store.snapshot()) {
                assert!(std::time::Instant::now() < deadline, "timed out: {what}");
                tokio::time::sleep(Duration::from_millis(5)).await;
            }
        }

        fn value(&self, key: &str) -> Option<Value> {
            self.store.snapshot().get(key).map(|e| e.value.clone())
        }

        async fn shut_down(self) {
            let _ = self.stop.send(true);
            tokio::time::timeout(Duration::from_secs(5), self.task)
                .await
                .expect("source stops promptly")
                .unwrap()
                .unwrap();
        }
    }

    fn num(value: f64, unit: Option<&str>) -> Value {
        Value::Number {
            value,
            unit: unit.map(str::to_owned),
        }
    }

    const TOPICS: &str = r#"
[topics]
"t:solar" = "home/power/solar"
"t:temp" = { topic = "home/temp", unit = "C" }
"t:state" = "home/state"
"t:flag" = "home/flag"
"t:w" = { topic = "home/inverter", path = "$.power.w", unit = "W" }
"t:mode" = { topic = "home/inverter", path = "$.mode" }
"t:str" = { topic = "home/strnum", path = "$.v" }
"t:obj" = "home/obj"
"#;

    // ---- Source behaviour -----------------------------------------------

    #[tokio::test]
    async fn plain_json_path_unit_and_numeric_string_payloads() {
        let (broker, r, topics) = started(TOPICS).await;
        // One subscription per distinct topic, even when two keys share one.
        assert_eq!(topics.len(), 7, "{topics:?}");
        assert_eq!(topics.iter().filter(|t| *t == "home/inverter").count(), 1);

        broker.publish("home/power/solar", "21.5", true);
        broker.publish("home/temp", "19", false);
        broker.publish("home/state", "on", false);
        broker.publish("home/flag", "true", false);
        broker.publish(
            "home/inverter",
            r#"{"power": {"w": 1234}, "mode": "eco"}"#,
            false,
        );
        broker.publish("home/strnum", r#"{"v": "42"}"#, false);
        broker.publish("home/obj", r#"{"a": [1, 2]}"#, false);
        r.wait_for("all keys", |s| s.len() == 8).await;

        assert_eq!(r.value("t:solar"), Some(num(21.5, None)));
        assert_eq!(r.value("t:temp"), Some(num(19.0, Some("C"))));
        assert_eq!(r.value("t:state"), Some(Value::Text("on".into())));
        assert_eq!(r.value("t:flag"), Some(Value::Bool(true)));
        assert_eq!(r.value("t:w"), Some(num(1234.0, Some("W"))));
        assert_eq!(r.value("t:mode"), Some(Value::Text("eco".into())));
        assert_eq!(r.value("t:str"), Some(num(42.0, None)));
        assert!(matches!(r.value("t:obj"), Some(Value::Json(_))));
        let snap = r.store.snapshot();
        assert_eq!(&*snap["t:solar"].source, "m");
        assert!(r.health.up.load(Ordering::Relaxed));
        assert_eq!(r.health.errors.load(Ordering::Relaxed), 0);
        r.shut_down().await;
    }

    #[tokio::test]
    async fn empty_payload_removes_keys_and_retained_values_count() {
        let (broker, r, _) = started(TOPICS).await;
        broker.publish("home/inverter", r#"{"power": {"w": 5}, "mode": "x"}"#, true);
        broker.publish("home/state", "on", true);
        r.wait_for("values", |s| {
            s.contains_key("t:w") && s.contains_key("t:state")
        })
        .await;
        // A deleted retained message arrives as an empty payload.
        broker.publish("home/inverter", "", true);
        r.wait_for("removed", |s| {
            !s.contains_key("t:w") && !s.contains_key("t:mode")
        })
        .await;
        assert_eq!(r.value("t:state"), Some(Value::Text("on".into())));
        r.shut_down().await;
    }

    #[tokio::test]
    async fn bad_json_with_a_path_is_counted_and_leaves_the_key_alone() {
        let (broker, r, _) = started(TOPICS).await;
        broker.publish("home/strnum", r#"{"v": "1"}"#, false);
        r.wait_for("first", |s| s.contains_key("t:str")).await;
        broker.publish("home/strnum", "{not json", false); // bad JSON
        broker.publish("home/strnum", r#"{"other": 1}"#, false); // path missing
        broker.publish("home/state", "sentinel", false);
        r.wait_for("sentinel", |s| s.contains_key("t:state")).await;
        // Publishes are handled in order, so both bad ones are done.
        assert_eq!(r.value("t:str"), Some(num(1.0, None)));
        assert_eq!(r.health.errors.load(Ordering::Relaxed), 2);
        assert!(
            r.health.up.load(Ordering::Relaxed),
            "the connection is fine"
        );
        r.shut_down().await;
    }

    #[tokio::test]
    async fn reconnects_and_resubscribes() {
        let (mut broker, r, _) = started(TOPICS).await;
        broker.publish("home/state", "before", false);
        r.wait_for("before", |s| s.contains_key("t:state")).await;

        broker.drop_connection();
        // The source reconnects (a new CONNECT) and subscribes again.
        let topics = broker.next_subscribe().await;
        assert_eq!(topics.len(), 7);
        assert!(r.health.errors.load(Ordering::Relaxed) >= 1);
        // Last values stay while down; new ones arrive after.
        assert_eq!(r.value("t:state"), Some(Value::Text("before".into())));
        broker.publish("home/state", "after", false);
        r.wait_for("after", |s| {
            s["t:state"].value == Value::Text("after".into())
        })
        .await;
        assert!(r.health.up.load(Ordering::Relaxed));
        r.shut_down().await;
    }

    #[tokio::test]
    async fn keeps_retrying_while_the_broker_is_down() {
        // Nothing listens on this port: bind, note the port, drop.
        let port = {
            let l = TcpListener::bind("127.0.0.1:0").await.unwrap();
            l.local_addr().unwrap().port()
        };
        let cfg = source_config(port, "[topics]\n\"t:a\" = \"x/y\"\n");
        let r = run(fast(MqttSource::new("m", &cfg).unwrap()));
        tokio::time::sleep(Duration::from_millis(300)).await;
        let errors = r.health.errors.load(Ordering::Relaxed);
        // Paced (10 ms doubling), not a tight loop of thousands.
        assert!((2..=40).contains(&errors), "errors = {errors}");
        assert!(!r.health.up.load(Ordering::Relaxed));
        r.shut_down().await;
    }

    #[tokio::test]
    async fn sends_credentials_and_client_id_but_never_shows_the_password() {
        const PW: &str = "hunter2-not-a-real-password";
        std::env::set_var("PANELD_TEST_MQTT_PW", PW);
        let mut broker = FakeBroker::start().await;
        let cfg = source_config(
            broker.port,
            "username = \"panel\"\npassword_env = \"PANELD_TEST_MQTT_PW\"\n[topics]\n\"t:a\" = \"x/y\"\n",
        );
        let src = MqttSource::new("m", &cfg).unwrap();
        let shown = format!("{src:?} | {cfg:?} | {:?}", src.settings);
        assert!(!shown.contains(PW), "{shown}");
        assert!(shown.contains("<redacted>"));
        let r = run(fast(src));
        match broker.next_seen().await {
            Seen::Connect {
                client_id,
                username,
                password,
            } => {
                assert_eq!(client_id, "paneld-m");
                assert_eq!(username.as_deref(), Some("panel"));
                assert_eq!(password.as_deref(), Some(PW));
            }
            other => panic!("{other:?}"),
        }
        r.shut_down().await;
    }

    #[test]
    fn a_missing_password_variable_is_named_in_the_error_and_check_environment() {
        let cfg = source_config(
            1883,
            "username = \"u\"\npassword_env = \"PANELD_TEST_MQTT_UNSET\"\n[topics]\n\"t:a\" = \"x/y\"\n",
        );
        let e = MqttSource::new("m", &cfg).unwrap_err().to_string();
        assert!(
            e.contains("PANELD_TEST_MQTT_UNSET") && e.contains("not set"),
            "{e}"
        );

        let full = parse(
            "host = \"h\"\nusername = \"u\"\npassword_env = \"PANELD_TEST_MQTT_UNSET\"\n[sources.m.topics]\n\"t:a\" = \"x/y\"\n",
        )
        .unwrap();
        let e = crate::sources::check_environment(&full)
            .unwrap_err()
            .to_string();
        assert!(
            e.contains("[sources.m]") && e.contains("PANELD_TEST_MQTT_UNSET"),
            "{e}"
        );
    }

    #[tokio::test]
    async fn shutdown_stops_the_source() {
        let (_broker, r, _) = started(TOPICS).await;
        r.shut_down().await;
    }

    // ---- Decoding -------------------------------------------------------

    fn route(key: &str, path: Option<&str>, unit: Option<&str>) -> Route {
        Route {
            key: key.into(),
            path: path.map(|p| JsonPath::parse(p).unwrap()),
            unit: unit.map(str::to_owned),
        }
    }

    #[test]
    fn decode_rules() {
        let plain = [route("k:a", None, Some("W"))];
        let set = |v: Value| vec![Change::Set("k:a".into(), v)];
        let d = |r: &[Route], p: &str| decode(r, p.as_bytes());

        assert_eq!(d(&plain, "21.5"), (set(num(21.5, Some("W"))), vec![]));
        assert_eq!(d(&plain, " 7\n").0, set(num(7.0, Some("W"))));
        assert_eq!(d(&plain, "007").0, set(num(7.0, Some("W")))); // not JSON, still a number
        assert_eq!(d(&plain, "on\n").0, set(Value::Text("on".into())));
        assert_eq!(d(&plain, "\"hi\"").0, set(Value::Text("hi".into())));
        assert_eq!(d(&plain, "true").0, set(Value::Bool(true)));
        assert_eq!(d(&plain, "NaN").0, set(Value::Text("NaN".into())));
        assert!(matches!(
            d(&plain, "[1,2]").0[..],
            [Change::Set(_, Value::Json(_))]
        ));
        // Empty, whitespace-only and `null` payloads remove the key.
        let rm = vec![Change::Remove("k:a".into())];
        assert_eq!(d(&plain, "").0, rm);
        assert_eq!(d(&plain, "  \n").0, rm);
        assert_eq!(d(&plain, "null").0, rm);
        // Not UTF-8 text.
        let (c, p) = decode(&plain, &[0xff, 0xfe]);
        assert!(c.is_empty() && p[0].contains("UTF-8"), "{p:?}");

        let pathed = [route("k:a", Some("$.a.b"), None)];
        assert_eq!(d(&pathed, r#"{"a":{"b":"3"}}"#).0, set(num(3.0, None)));
        assert_eq!(d(&pathed, r#"{"a":{"b":null}}"#).0, rm);
        let (c, p) = d(&pathed, r#"{"a":{}}"#);
        assert!(c.is_empty() && p[0].contains("path not found"), "{p:?}");
        let (c, p) = d(&pathed, "12");
        assert!(c.is_empty() && p[0].contains("path not found"), "{p:?}");
        let (c, p) = d(&pathed, "oops");
        assert!(c.is_empty() && p[0].contains("not valid JSON"), "{p:?}");
        // The empty payload removes even keys that have a path.
        assert_eq!(d(&pathed, "").0, rm);
    }

    // ---- Config ---------------------------------------------------------

    /// Parse a whole config with `[sources.m]` of type mqtt; `body` is the
    /// rest of that table (and its sub-tables).
    fn parse(body: &str) -> anyhow::Result<Config> {
        Config::parse(&format!(
            "[canvas]\nwidth = 8\nheight = 8\n[[scene]]\nname = \"a\"\n[sources.m]\ntype = \"mqtt\"\n{body}"
        ))
    }

    #[test]
    fn config_defaults_and_both_topic_forms() {
        let cfg = parse(
            "host = \"broker.local\"\n[sources.m.topics]\n\"t:a\" = \"home/power/solar\"\n\"t:b\" = { topic = \"home/x\", path = \"$.v\", unit = \"W\" }\n",
        )
        .unwrap();
        let SourceConfig::Mqtt(c) = &cfg.sources["m"] else {
            panic!("not mqtt")
        };
        assert_eq!((c.port, c.keep_alive_s), (1883, 30));
        assert!(c.client_id.is_none() && c.username.is_none() && c.password_env.is_none());
        assert_eq!(c.topics["t:a"].topic(), "home/power/solar");
        assert_eq!(c.topics["t:a"].path(), None);
        assert_eq!(c.topics["t:b"].path(), Some("$.v"));
        assert_eq!(c.topics["t:b"].unit(), Some("W"));
        let src = MqttSource::new("m", c).unwrap();
        assert_eq!(src.settings.client_id, "paneld-m");
        assert_eq!(src.settings.keep_alive, Duration::from_secs(30));
    }

    #[test]
    fn config_validation_errors() {
        let t = "[sources.m.topics]\n\"t:a\" = \"x/y\"\n";
        let cases: Vec<(String, &str)> =
            vec![
            (
                "host = \"h\"\n[sources.m.topics]\n\"t:a\" = \"home/+/x\"\n".into(),
                "wildcard",
            ),
            (
                "host = \"h\"\n[sources.m.topics]\n\"t:a\" = \"home/#\"\n".into(),
                "wildcard",
            ),
            (
                "host = \"h\"\n[sources.m.topics]\nnocolon = \"x/y\"\n".into(),
                "not a valid key",
            ),
            (
                "host = \"h\"\n[sources.m.topics]\n\"t:a\" = \"\"\n".into(),
                "topic must be",
            ),
            (
                "host = \"h\"\n[sources.m.topics]\n\"t:a\" = { topic = \"x\", path = \"power\" }\n"
                    .into(),
                "must start with `$`",
            ),
            (
                "host = \"h\"\n[sources.m.topics]\n\"t:a\" = { topic = \"x\", path = \"$.a[\" }\n"
                    .into(),
                "column 4",
            ),
            ("host = \"h\"\n[sources.m.topics]\n".into(), "topics is empty"),
            (format!("host = \"h\"\nport = 0\n{t}"), "port must be 1 to 65535"),
            (format!("host = \"h\"\nkeep_alive_s = 0\n{t}"), "keep_alive_s"),
            (format!("host = \"mqtt://h\"\n{t}"), "host must be"),
            (
                format!("host = \"h\"\nusername = \"u\"\npassword_env = \"hunter2!\"\n{t}"),
                "NAME of an environment variable",
            ),
            (
                format!("host = \"h\"\npassword_env = \"MQTT_PASSWORD\"\n{t}"),
                "needs a username",
            ),
            (format!("host = \"h\"\nclient_id = \"\"\n{t}"), "client_id"),
        ];
        for (body, want) in cases {
            let e = parse(&body).unwrap_err().to_string();
            assert!(e.contains(want), "want {want:?} in {e}\n(config: {body})");
        }
        // A password given inline is an unknown field, not a way to leak it.
        assert!(parse(&format!("host = \"h\"\npassword = \"x\"\n{t}")).is_err());
    }
}
