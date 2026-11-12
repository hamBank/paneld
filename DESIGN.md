# Panel Daemon — Design

A Rust service that collects data from many sources (Home Assistant, HTTP APIs, Prometheus, pushed messages, media files), composes it into scenes, and drives HUB75 LED panels through a Colorlight 5A-75B receiver card over raw Ethernet.

Working name: `paneld` (rename freely).

---

## 1. Goals and non-goals

### Goals
- Drive Colorlight 5A-75B card(s) directly from a Raspberry Pi (4 or 5) over a dedicated Ethernet port, no Colorlight sender card.
- Show more than Home Assistant data: generic data sources behind a plugin trait.
- Scenes made of zones, with rotation/playlists and priority notifications that interrupt and then restore.
- First-class Home Assistant integration via MQTT discovery (control + state) and the HA WebSocket API (data).
- Develop and preview without hardware (a preview sink that serves frames over HTTP).
- Observable: Prometheus `/metrics`.
- Config-file driven (TOML), hot-reloaded.

### Non-goals (for now)
- No web UI for editing layouts. Config file only.
- No sequencing/xLights compatibility.
- Video playback is a **later milestone** (one zone only, see §9). Design must not preclude it, but don't build it early.

### Current hardware
- 1× Colorlight 5A-75B, **2 HUB75 panels, each 172×86 at 1/43 scan, stacked vertically** → a 172×172 logical canvas. Everything must still come from config.
- Expect growth to more panels and possibly more than one card.

---

## 2. Architecture overview

```
 ┌──────────────── Sources (async tasks) ────────────────┐
 │ HA WebSocket │ MQTT │ HTTP poll widgets │ Prometheus  │
 │ HTTP push API │ media folder │ clock/system           │
 └───────────────┬───────────────────────────────────────┘
                 │ updates (typed values, keyed)
                 ▼
          ┌──────────────┐
          │  Data store  │  latest value per key, change notifications
          └──────┬───────┘
                 │ snapshot per frame
                 ▼
 ┌───────────── Scene engine (render thread) ─────────────┐
 │ scheduler: playlist rotation + notification queue      │
 │ layout: scene → zones → widgets                        │
 │ renderer: widgets draw into Canvas (RGB888)            │
 │ post: gamma, colour temp, global brightness            │
 └──────────────┬─────────────────────────────────────────┘
                │ Frame (Arc<Canvas>)
                ▼
 ┌──────────────── Sinks ────────────────┐
 │ ColorlightSink (raw Ethernet, Linux)  │
 │ PreviewSink (HTTP PNG / MJPEG)        │
 │ (later) DDP / E1.31                   │
 └───────────────────────────────────────┘

 Control plane: MQTT discovery entities, HTTP API, config hot-reload, /metrics
```

Key rule: **the render loop never blocks on I/O.** Sources write into the data store from async tasks; the render thread reads a snapshot each tick. A slow API makes a widget's data stale, it never stalls the frame.

---

## 3. Repository layout

Cargo workspace:

```
paneld/
├── Cargo.toml                # workspace
├── crates/
│   ├── colorlight-proto/     # pure protocol encode/decode, no I/O, no_std-friendly where easy
│   ├── colorlight-net/       # raw socket transport (Linux AF_PACKET), card detection
│   └── paneld/               # the daemon: sources, store, engine, widgets, sinks, control
├── config/
│   └── example.toml
├── assets/fonts/             # bitmap fonts (BDF/PCF or embedded-graphics fonts)
├── deploy/
│   ├── paneld.service        # systemd unit
│   └── README.md             # Pi setup notes
└── tests/fixtures/           # captured packets, golden images
```

Why split the protocol out: it's testable without hardware or root, and reusable. `colorlight.rs` (crates.io `colorlight` 0.1.0) is a reference, not a dependency. Vendor ideas, own the code.

---

## 4. Colorlight output

### 4.1 Protocol (verify before implementing)

The 5A-75B takes **raw Ethernet frames** (not IP) on its Gigabit port. Known shape of the protocol from public reverse-engineering:

- Pixel data goes out **per row**: each packet carries a row index, a column offset, a pixel count and **BGR** pixel bytes. Long rows may need splitting across packets.
- After all rows, a **display/latch frame** is sent. It also carries global brightness (and colour temperature/channel scaling).
- There is a **detection/discovery** exchange to find the card and read its info.
- Distinct EtherTypes identify the packet types (PanelPlayer documents EtherType `0x0101` among others).

**Do not trust the byte layouts from memory or from this document.** Before writing `colorlight-proto`, read and reconcile:
1. hkubota, "Winter project: Colorlight 5A-75B protocol": https://hkubota.wordpress.com/2022/01/31/winter-project-colorlight-5a-75b-protocol/
2. https://github.com/haraldkubota/colorlight
3. https://github.com/asamonik/colorlight.rs
4. PanelPlayer `colorlight.c`: https://github.com/ivancmz/PanelPlayer (fork of the original PanelPlayer)
5. FPP ColorLight output, the only reference here that handles **V13.x firmware**: `src/channeloutput/ColorLight-5a-75.{cpp,h}` in https://github.com/FalconChristmas/fpp (fix in commit `d49ad41`, discussion in https://github.com/FalconChristmas/fpp/issues/1849). It also ships a Wireshark dissector, `docs/ColorLight_Wireshark.lua`, useful for checking our own packets. FPP is GPL-2.0: take protocol facts from it, never copy code.

Record the confirmed layouts in `crates/colorlight-proto/PROTOCOL.md` with links, and add unit tests that encode known-good packets (from captures or reference implementations) byte-for-byte.

### 4.2 Card configuration prerequisites
- Panel size, scan type, chaining order etc. are configured **on the card** with Colorlight's LEDVision (Windows) once. The daemon only sends pixels.
- Firmware: V13.x broke raw senders that worked on 11.04 and older. FPP fixed it (reference 5) as follows. **To be confirmed against the source and captures before M1 code is written:**
  - Discover receivers at startup with a `0x07` packet. Each card answers with `0x08`, which carries the firmware major/minor version and the card's configured width/height.
  - Per frame: brightness (`0x0A`), then the `0x55` row packets, then a short gap (FPP default 500 µs), then the `0x01` display/sync packet.
  - On firmware **≥ 13**, send `0x01` (and `0x0A`) **twice**, back to back. Older firmware flickers if it gets two, so the count must follow the detected version.
  - Mixing old and new firmware on one chain is not supported, by FPP or by us.
- What we do: `probe` prints each card's firmware version and configured size. `ColorlightSink` discovers cards on startup and picks the sync count from the result. An optional `firmware_major` override on the output covers cards that don't answer discovery. Log a warning if cards on one interface disagree, or if the card's reported size doesn't match the configured regions.
- Deploy docs still recommend a known-good firmware (11.04 for this build). Supporting 13.x is still worth doing, because some panel types reportedly refresh better on 13.x (see the FPP issue).

### 4.3 Transport (`colorlight-net`)
- Linux `AF_PACKET` raw socket bound to a configured interface (e.g. `eth1`). Use `socket2` or `pnet_datalink`; prefer minimal deps.
- Requires `CAP_NET_RAW`. Run via systemd `AmbientCapabilities=CAP_NET_RAW`. No full root.
- Batch sends per frame. Measure: a full frame for 2× 172×86 (172×172, ~89 KB of BGR) is small, but 4+ panels at 60 fps should still be well inside Gigabit. Keep a per-frame send-time metric.
- Gate behind `cfg(target_os = "linux")`. On macOS/Windows the crate compiles to a stub that errors at runtime, so devs can still build the daemon and use the preview sink.

### 4.4 Multiple panels / cards
- The engine renders one **logical canvas** (e.g. 172×172 for the two stacked 172×86 panels).
- An **output map** in config maps canvas regions to card coordinates: `card`, `canvas_rect`, `card_offset`. With one card this is a single region.
- Multiple cards: either daisy-chained (single NIC, each card's region set in LEDVision) or one NIC per card. Support both by letting each `ColorlightSink` own an interface plus a list of regions. Confirm daisy-chain addressing against the protocol references before implementing.
- Also support rotation/flip per region (panels mounted upside down happen).

---

## 5. Core engine

### 5.1 Canvas and frame
- `Canvas`: `width × height` RGB888 buffer, implementing `embedded_graphics::DrawTarget<Color = Rgb888>`.
- Frame loop on a dedicated thread at a configured fps (default 30, max 60). Use a fixed-rate ticker; if a frame overruns, skip rather than queue up.
- Each tick: snapshot store → scheduler picks active scene/notification → widgets draw → post-processing → `Arc<Canvas>` handed to all sinks.
- Sinks run on their own threads/tasks with a bounded channel of capacity 1 (latest frame wins).

### 5.2 Post-processing
Applied per sink (so the preview can show "true" colours while the panel gets corrected output):
- Gamma LUT (configurable, default ~2.2)
- Colour temperature / RGB scaling
- Global brightness (prefer the card's hardware brightness in the latch frame where possible, software scaling as fallback)
- Auto-dim: schedule-based (sunset/sunrise, quiet hours) or driven by a HA lux sensor entity

### 5.3 Data store
- Map of `Key → Value` where `Value` is an enum: `Number(f64, unit)`, `Text`, `Bool`, `Image(Arc<RgbaImage>)`, `Series(Vec<(ts, f64)>)`, `Json`.
- Each entry records `updated_at` and source, so widgets can show staleness.
- Implementation: `ArcSwap<HashMap>` or `RwLock` plus a `tokio::sync::watch` for change notifications. Render reads must be cheap and lock-light.

### 5.4 Scenes, zones, widgets
- **Scene** = name + list of zones + optional duration + optional condition (e.g. only show when `ha:sensor.solar_power > 0`).
- **Zone** = rect on the canvas + one widget + optional background/border.
- **Widget** trait:
  ```rust
  trait Widget: Send {
      fn update(&mut self, ctx: &FrameCtx, store: &StoreSnapshot); // cheap state advance (scroll position, animation frame)
      fn draw(&self, target: &mut ZoneTarget<'_>);                 // clipped to zone
  }
  ```
- Initial widgets: `clock`, `text` (static/scrolling, templated from store keys), `value` (number + unit + icon + threshold colours), `sparkline/graph` (from `Series`), `image` (static or GIF), `progress bar`, `icon`.
- Templating: a small template syntax for text, e.g. `"{ha:sensor.outside_temp:.1}°"`. Keep it minimal and implement it yourself or with a tiny crate. Don't pull in a full template engine.
- Fonts: bitmap fonts via `embedded-graphics` mono fonts plus BDF loading (e.g. `bdf-parser` / `eg-bdf`) for nicer pixel fonts.

### 5.5 Scheduler
- **Playlist**: ordered scenes with durations. Conditional scenes are skipped when their condition is false.
- **Notifications**: priority queue. Each notification has priority, duration, optional repeat count, and either a scene ref or inline content (text/icon/colour). Higher priority interrupts the current one, then the playlist resumes **where it left off**.
- Manual override: pin a scene until released (via MQTT select / HTTP).
- Transitions: none / cut first; slide/fade later.

---

## 6. Sources

`Source` trait, each running as its own tokio task, writing into the store:

```rust
#[async_trait]
trait Source {
    fn name(&self) -> &str;
    async fn run(self: Box<Self>, store: StoreWriter, shutdown: CancellationToken) -> anyhow::Result<()>;
}
```

Supervisor restarts failed sources with backoff and reports health in metrics.

Initial sources:
1. **Home Assistant WebSocket**: authenticate with a long-lived token (env var, never in config), `subscribe_entities` or `subscribe_events(state_changed)` filtered to the entity IDs the config references. Keys: `ha:<entity_id>` plus `ha:<entity_id>.<attribute>`.
2. **MQTT subscribe**: arbitrary topics → keys, with optional JSON path extraction.
3. **HTTP poll**: URL + interval + JSON path(s) → keys. Covers weather and generic APIs.
4. **Prometheus query**: instant queries (and range queries for sparklines) against a Prometheus URL → `Number`/`Series`.
5. **Clock/system**: time, date, uptime, local CPU temp.
6. **Media folder**: images/GIFs loaded and scaled to zone size on demand (cached).
7. **HTTP push** (via control API, §7): external scripts write keys or raise notifications.

---

## 7. Control plane

### 7.1 HTTP (axum)
- `GET  /metrics`: Prometheus
- `GET  /preview.png`: current frame (scaled ×N, optional pixel-grid look)
- `GET  /preview`: tiny HTML page that refreshes / shows an MJPEG stream
- `POST /api/notify`: `{ "text": "...", "icon": "...", "color": "#ff0000", "duration_s": 10, "priority": 5 }`
- `POST /api/data/{key}`: push a value into the store
- `POST /api/scene/{name}`: pin scene; `DELETE /api/scene`: release
- `POST /api/brightness`: `{ "value": 0-255 }`
- `GET  /api/state`: current scene, notification queue, card status
- Bind address configurable; optional bearer token.

### 7.2 MQTT + Home Assistant discovery
Publish discovery configs so HA auto-creates one device with:
- `light.<name>`: on/off + brightness (maps to global brightness / blank)
- `select.<name>_scene`: current/pinned scene (options = configured scenes + "auto")
- `text.<name>_message`: setting it raises a notification
- `binary_sensor.<name>_card`: card detected/responding
- `sensor.<name>_fps`: actual fps

Also accept raw notification JSON on `<prefix>/notify` (same schema as HTTP). Use `rumqttc`. Availability topic + LWT.

### 7.3 Config hot reload
Watch the config file (`notify` crate). On change: parse + validate fully → swap scenes/widgets atomically. Sources whose config changed get restarted, others keep running. On validation error: log, keep the old config, expose `paneld_config_reload_errors_total`.

---

## 8. Configuration (TOML) example

```toml
[canvas]
width = 172          # two 172x86 panels stacked
height = 172
fps = 30

[[output]]
type = "colorlight"
interface = "eth1"
gamma = 2.2
brightness = 180
  [[output.region]]          # one card, two panels stacked
  canvas = { x = 0, y = 0, w = 172, h = 172 }
  card_offset = { x = 0, y = 0 }
  rotate = 0

[[output]]
type = "preview"
scale = 8

[http]
bind = "0.0.0.0:8080"

[mqtt]
host = "homeassistant.local"
port = 1883
username = "paneld"
password_env = "PANELD_MQTT_PASSWORD"
discovery_prefix = "homeassistant"
node_id = "paneld"

[sources.ha]
url = "ws://homeassistant.local:8123/api/websocket"
token_env = "PANELD_HA_TOKEN"

[sources.prom]
type = "prometheus"
url = "http://prometheus.local:9090"
queries = { "prom:house_power" = "sum(power_watts)" }
interval_s = 10

[sources.weather]
type = "http_poll"
url = "https://example/api/forecast"
interval_s = 600
extract = { "weather:temp" = "$.current.temp", "weather:icon" = "$.current.icon" }

[[scene]]
name = "home"
duration_s = 20
  [[scene.zone]]
  rect = { x = 0, y = 0, w = 172, h = 86 }      # top panel
  widget = { type = "clock", format = "%H:%M", font = "6x10" }
  [[scene.zone]]
  rect = { x = 0, y = 86, w = 172, h = 86 }     # bottom panel
  widget = { type = "value", key = "ha:sensor.outside_temperature", format = "{:.1}°", icon = "thermometer" }

[[scene]]
name = "power"
duration_s = 15
condition = "prom:house_power > 0"
  [[scene.zone]]
  rect = { x = 0, y = 0, w = 172, h = 172 }
  widget = { type = "sparkline", key = "prom:house_power_series", color = "#ffaa00" }

[quiet_hours]
start = "22:00"
end = "06:30"
brightness = 20
```

Secrets only via env vars (`*_env` keys). Never commit tokens.

---

## 9. Future: video zone (not now)
- A `video` widget that owns a zone and pulls frames from an `ffmpeg` subprocess (`-f rawvideo -pix_fmt rgb24 -s WxH`) into a ring buffer, decoded off the render thread.
- Render thread just blits the latest decoded frame, so the frame loop design is unchanged.
- Sources: file, RTSP (e.g. a camera via HA/go2rtc).
- Keep the `Widget` trait and canvas API compatible with this. That's the only requirement now.

---

## 10. Observability
Prometheus metrics (prefix `paneld_`):
- `frames_rendered_total`, `frames_skipped_total`, `render_duration_seconds` (histogram)
- `sink_send_duration_seconds{sink}`, `sink_errors_total{sink}`
- `card_detected{card}` (gauge 0/1)
- `source_up{source}`, `source_errors_total{source}`, `source_last_update_timestamp{source}`
- `store_keys`, `notifications_queued`
- `config_reload_errors_total`, `current_scene{scene}` (info-style gauge)

Logging: `tracing` with env-filter; JSON output option for journald.

---

## 11. Deployment (Pi)
- Raspberry Pi 4/5, Raspberry Pi OS Lite 64-bit. Onboard Ethernet → LAN; USB 3 Gigabit adapter → Colorlight card (or the reverse; config picks the interface). Wi-Fi for LAN is also fine.
- The card-facing interface gets **no IP config** (disable DHCP on it in NetworkManager/dhcpcd) so the OS doesn't spam it.
- Cross-compile with `cross` for `aarch64-unknown-linux-gnu`, or build on-device.
- systemd unit (`deploy/paneld.service`): `DynamicUser=yes` or a dedicated user, `AmbientCapabilities=CAP_NET_RAW`, `CapabilityBoundingSet=CAP_NET_RAW`, `EnvironmentFile=/etc/paneld/env`, `Restart=on-failure`.
- Card config (panel size/scan) done once in LEDVision from a Windows machine. Document the settings used in `deploy/README.md`.

---

## 12. Testing strategy
- `colorlight-proto`: byte-exact encode tests against fixtures from captures/reference implementations; property tests for row splitting and region mapping.
- Widgets/scenes: **golden image tests**. Render a scene with a fixed store snapshot + fixed clock into PNG, compare to `tests/fixtures/golden/*.png`. Provide `UPDATE_GOLDEN=1` to regenerate.
- Scheduler: deterministic tests with an injected clock (notification interrupt/resume, conditional skip, pinning).
- Config: parse/validate tests incl. error messages for bad configs.
- Sources: mock servers (`wiremock`) for HTTP/Prometheus; HA WebSocket against a small fake server.
- No test may require root, a NIC, or hardware. Hardware checks live in a `paneld probe` subcommand.

---

## 13. CLI
- `paneld run --config path`: main daemon
- `paneld check --config path`: validate config and exit
- `paneld probe --interface eth1`: detect card(s), print info, needs CAP_NET_RAW
- `paneld test-pattern --interface eth1 --width W --height H`: colour bars / gradient / per-panel ID numbers, for bring-up
- `paneld render --config path --scene home --out frame.png`: offline render of one scene (uses golden-test path)

---

## 14. Milestones
1. **M0 Skeleton**: workspace, config parsing, canvas, render loop, preview sink, `clock` + `text` widgets, `paneld render`. Runs on a laptop.
2. **M1 Hardware**: `colorlight-proto` (with PROTOCOL.md + tests), `colorlight-net`, `probe`, `test-pattern`, ColorlightSink, gamma/brightness. Two panels lit on the Pi.
3. **M2 Scenes**: zones, playlist rotation, conditions, `value`/`image`/`sparkline` widgets, hot reload.
4. **M3 Data**: HA WebSocket, MQTT subscribe, HTTP poll, Prometheus sources, templating.
5. **M4 Control**: HTTP API, MQTT discovery entities, notifications queue, quiet hours/auto-dim.
6. **M5 Ops**: metrics, systemd unit, deploy docs, cross-compile.
7. **M6 Scale**: multi-region/multi-card output map, rotation/flip.
8. **M7 (optional)** Video zone.

Each milestone ends with: tests green, `cargo clippy -- -D warnings` clean, README updated.

---

## 15. Open questions
- ~~Exact panel resolution and scan rate~~ **Answered:** 172×86, 1/43 scan (86 rows / 2 = 43 scan lines, the usual two-halves HUB75 layout). Needed for the LEDVision settings in `deploy/README.md`.
- Card firmware version currently installed. Plan: flash V11.x (11.04 if available). Either way the sink detects the version (§4.2).
- ~~Side-by-side or stacked~~ **Answered:** stacked vertically, canvas 172×172.
- Which HUB75 port(s) each panel is on and how the card maps them (sets `card_offset` in the M1 output map). Confirm in LEDVision.
- Which HA entities and external APIs to show first (drives M3 priorities).
