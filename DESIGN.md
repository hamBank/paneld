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
├── assets/fonts/             # shipped BDF fonts + licences
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

- Pixel data goes out **per row**: each packet carries a row index, a column offset, a pixel count and 3 bytes per pixel, at most 497 pixels per packet, so long rows are split. **The wire byte order is not fixed by the protocol:** the card forwards the 3 bytes as they come and the order that gives correct colours depends on the panel (hkubota's panel needed BGR, FPP defaults to RGB and has a per-panel colour order setting). The sink therefore needs a per-output colour order option and must not assume BGR. See `PROTOCOL.md`, "Pixel byte order".
- After all rows, a **display/latch frame** is sent. It also carries global brightness (and colour temperature/channel scaling).
- There is a **detection/discovery** exchange to find the card and read its info.
- The first byte of the EtherType field is the packet type and the second is the first payload byte, so packets show up as distinct EtherTypes. For example, the display frame is `0x0107`. PanelPlayer's README says `0x0101`, but its code, like every other reference, sends `0x0107`. See `crates/colorlight-proto/PROTOCOL.md`.

**Do not trust the byte layouts from memory or from this document.** Before writing `colorlight-proto`, read and reconcile:
1. hkubota, "Winter project: Colorlight 5A-75B protocol": https://hkubota.wordpress.com/2022/01/31/winter-project-colorlight-5a-75b-protocol/
2. https://github.com/haraldkubota/colorlight
3. https://github.com/asamonik/colorlight.rs
4. PanelPlayer `colorlight.c`: https://github.com/ivancmz/PanelPlayer (fork of the original PanelPlayer)
5. FPP ColorLight output, the only reference here that handles **V13.x firmware**: `src/channeloutput/ColorLight-5a-75.{cpp,h}` in https://github.com/FalconChristmas/fpp (fix in commit `d49ad41`, discussion in https://github.com/FalconChristmas/fpp/issues/1849). It also ships a Wireshark dissector, `docs/ColorLight_Wireshark.lua`, useful for checking our own packets. FPP is GPL-2.0: take protocol facts from it, never copy code.

Record the confirmed layouts in `crates/colorlight-proto/PROTOCOL.md` with links, and add unit tests that encode known-good packets (from captures or reference implementations) byte-for-byte.

### 4.2 Card configuration prerequisites
- Panel size, scan type, chaining order etc. are configured **on the card** with Colorlight's LEDVision (Windows) once. The daemon only sends pixels.
- Firmware: V13.x broke raw senders that worked on 11.04 and older. FPP fixed it (reference 5) as follows. **The packet layouts are confirmed in `crates/colorlight-proto/PROTOCOL.md` (with sources). The V13 behaviour itself (the doubling and the version cut-off) rests on FPP alone; no other reference knows about V13:**
  - Discover receivers at startup with a `0x07` packet. Each card answers with `0x08`, which carries the firmware major/minor version and the card's configured width/height. The request carries a receiver index (0, 1, ...); ask for 0, then 1, and so on until nobody answers. colorlight.rs's "ack" is just the request for index 1.
  - Per frame: brightness (`0x0A`), then the `0x55` row packets, then a short gap (FPP default 500 µs), then the `0x01` display/sync packet.
  - On firmware **≥ 13**, send `0x01` (and `0x0A`) **twice**, back to back. Older firmware flickers if it gets two, so the count must follow the detected version.
  - `colorlight-proto::needs_double_sync(major)` is the rule (major >= 13).
- Mixing old and new firmware on one chain is not supported, by FPP or by us.
- What we do: `probe` prints each card's firmware version and configured size. `ColorlightSink` discovers cards on startup and picks the sync count from the highest firmware major found. An optional `firmware_major` override on the output wins over discovery (and is used when nothing answers; with neither, the sink warns and assumes firmware < 13). It warns if cards on one interface disagree, or if a card's reported size differs from the canvas. That last check is a warning only, because the reported size need not equal the canvas (two panels on two ports) and FPP calls the field unreliable.
- Per-frame order as implemented (`colorlight_sink::FrameSender`): `0x0A` (x2 on >= 13), all `0x55` rows with card offset 0, `sync_gap_us` pause, `0x01` (x2 on >= 13). Both brightness frames carry `card_brightness()`. If the rows fail to send, the `0x01` is skipped so the card keeps its last complete frame. Colour order is applied after the LUT, in place, via the output's `color_order`.
- Errors: a failed frame is counted and logged at most every 5 s; the sink keeps running and re-opens the socket with backoff (1 s up to 10 s) after a fatal error or three failures in a row. The rest of the daemon is unaffected.
- Deploy docs still recommend a known-good firmware (11.04 for this build). Supporting 13.x is still worth doing, because some panel types reportedly refresh better on 13.x (see the FPP issue).

### 4.3 Transport (`colorlight-net`)
- Linux `AF_PACKET` raw socket bound to a configured interface (e.g. `eth1`). **Implemented with `libc` directly** (`socket`, `bind`, `sendmmsg`, `poll`, `recv`), not `socket2`/`pnet_datalink`: `libc` is already in the tree through tokio and the calls we need are five. The socket is bound to EtherType `0x0805` so that it only receives discovery replies (the card answers broadcast); sending is not affected by that filter.
- The `Link` trait (`send(&[&[u8]])`, `recv(buf, timeout)`, `pause`) is what the sink and `discover()` are written against; `colorlight_net::mock::MockLink` replays captured frames in tests, so no test needs root or a NIC.
- Requires `CAP_NET_RAW`. Run via systemd `AmbientCapabilities=CAP_NET_RAW`. No full root.
- Batch sends per frame. Measure: a full frame for 2× 172×86 (172×172, ~89 KB of pixel data) is small, but 4+ panels at 60 fps should still be well inside Gigabit. Keep a per-frame send-time metric.
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
- Gamma LUT (configurable, default 2.2)
- Colour temperature / RGB scaling, configured as a white point colour
- Global brightness (`brightness_mode = "hardware"` puts it in the card's display frame; `"software"` folds it into the LUT)
- All of the above are combined into one 256-entry LUT per channel (`postprocess.rs`; order: gamma, white point, brightness, one rounding)
- Auto-dim: schedule-based (sunset/sunrise, quiet hours) or driven by a HA lux sensor entity

### 5.3 Data store
- Map of `Key → Value` where `Value` is an enum: `Number(f64, unit)`, `Text`, `Bool`, `Image(Arc<Bitmap>)`, `Series(Arc<[(SystemTime, f64)]>)` (immutable, so a per-frame clone is one refcount bump; sources append with `StoreWriter::push_point`), `Json`.
- `Bitmap` (`media.rs`) is our own small straight-alpha RGBA8 type instead of the `image` crate's `RgbaImage`: the `image` crate pulls in a large codec tree, and we only need PNG (the `png` crate we already use) and GIF (the `gif` crate). Widgets detect a new image by `Arc::ptr_eq`.
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
- `image` widget (M2): `path` (file, relative to the config file) or `key` (a `Value::Image` in the store), `fit` (`contain` default, `cover`, `stretch`, `none`), `align`, `smooth`, `loop`. Scaling is nearest-neighbour unless `smooth` (box filter on axes that shrink), results are cached per (image, zone size), and alpha is blended over the zone background. GIF frames are composited in full (disposal methods and transparency honoured) and picked from `FrameCtx::scene_elapsed`; delays under 20 ms count as 100 ms, like browsers. File decoding happens at config load time (`Config::load_media`), never on the render thread. A future video widget fits the same shape: decode off-thread into frames the widget selects by `scene_elapsed`. `Widget` gained an optional `prepare(zone_w, zone_h)` hook so scaling can be done before the first frame; the widget also copes if it is never called.
- Initial widgets: `clock`, `text` (static/scrolling, templated from store keys), `value` (number + unit + icon + threshold colours), `sparkline/graph` (from `Series`), `image` (static or GIF), `progress bar`, `icon`.
- Templating (M3, as built, `template.rs`): hand-written, no engine and no new dependency. A `text` widget's `text` is literal text plus `{source:name}` / `{source:name:spec}`; `{{` and `}}` are literal braces. `spec` is the existing `format::Spec` (`.N`, `>W`, `<W`), reused as is. The key/spec split is positional because `:` is both: first `:` ends the source prefix, the name (`[A-Za-z0-9_][A-Za-z0-9_.@-]*`) runs to the second `:`, the rest is the spec. Values: finite `Number` through the spec; `Text` as is (width pads, precision truncates); `Bool` as `on`/`off`; anything else (missing, non-finite, `Series`, `Image`, `Json`) as `--`, padded to the width. Parse errors carry a column and become config errors naming scene and zone; `Config::referenced_keys()` includes the template keys. Per frame: the template is parsed at widget build time, `Template::fingerprint` hashes only what the output depends on (what each placeholder would show), and the widget re-renders into its reused `String` (and re-measures the width) only when the hash changes, so unchanged frames do no formatting and no allocation. Comparing `updated_at` or `Arc` pointers was rejected: any store write changes the snapshot, and values can be rewritten unchanged. Deviation from the old behaviour: `text` that contains a lone `{` or `}` used to be shown literally and is now a template (use `{{`/`}}`). `clock` has no templates. Not done: per-placeholder staleness.
- Fonts (M2, as built): the `embedded-graphics` ISO-8859-1 mono fonts plus BDF fonts loaded at runtime, behind one `fonts::Font` (a `FontFace` enum of `Mono(&MonoFont)` / `Bdf(Arc<BdfFont>)` plus an integer scale). Decisions:
  - *Parser:* hand-written (`bdf.rs`, ~250 lines with tests), not `bdf-parser` or `eg-bdf`. The subset we need is a few keywords; our own parser reports `line N` errors that we can put in config errors, handles `DWIDTH` proportional advances directly, and adds no dependency (and no MSRV or `--locked` surface). `bdf-parser` is at 0.1.0 on crates.io. `eg-bdf` does not come up in `cargo search` (checked 2026-11-12); as far as we know it targets compile-time conversion to `static` data, which doesn't fit loading from a path named in config. Revisit `bdf-parser` if BDF support needs to grow beyond this subset.
  - *Abstraction:* our own `Font` with `text_width`, `line_height` and `draw`, rather than `TextRenderer`/`CharacterStyle` impls. The widgets only need "measure this string, draw it at x/centre-y with an alignment", and an integer scale is easiest as a `DrawTarget` adapter (`ScaleTarget`) that turns each font pixel into an N×N block; built-in fonts are drawn through the stock `MonoTextStyle` onto that adapter, so scale 1 is pixel-identical to the old rendering. BDF glyph pixels are emitted straight into it. No per-glyph rasterised cache: glyph bitmaps are already the cache.
  - *Config:* `[[font]] name = "...", path = "..."` (explicit table) rather than `[fonts] dir`. Names are decoupled from file names, nothing is loaded that is not asked for, and there is no directory-order dependence. Paths resolve against `Config::base_dir` like image paths. `Config::load_fonts` (called from `parse_in`, so `Config::load` stays a pure path-to-Config step) parses the files and resolves each widget's font into `face`; a widget never touches the filesystem.
  - *Scale:* `font_scale = N` (1–8) on `clock`/`text`/`value`, not a `"6x10@3"` name suffix. It composes with any font name, needs no string parsing, and is validated like any other number.
  - *Missing glyphs:* drawn as the font's `?` if it has one, else a blank of the font's `FONTBOUNDINGBOX` width.
  - Layout: the line box (`FONT_ASCENT + FONT_DESCENT` for BDF) times scale is centred on the zone's mid-line; width is the sum of `DWIDTH` advances.
  - Shipped fonts live in `assets/fonts/` (Spleen 12x24, 16x32, 32x64, BSD-2-Clause) with licences and sources in `assets/fonts/README.md`.

### 5.5 Scheduler
- **Playlist**: ordered scenes with durations. Conditional scenes are skipped when their condition is false.
- **Notifications**: priority queue. Each notification has priority, duration, optional repeat count, and either a scene ref or inline content (text/icon/colour). Higher priority interrupts the current one, then the playlist resumes **where it left off**.
- As built in M2 (`scheduler.rs`, `condition.rs`): the scheduler is pure state driven by `tick(now: Instant, &StoreSnapshot)`, so tests inject time. It makes at most one switch per tick (a stalled render loop doesn't fast-forward the playlist), leaves a scene at once if its condition turns false, and returns `None` (black frame, logged once per transition) when nothing is eligible. Conditions are `key op literal`, bare `key` (truthy), `!`, `&&`, `||` and parentheses; a missing key or type mismatch makes a comparison false. `[playlist] default_duration_s` (default 10) is an addition to the §8 schema. `paneld run --scene X` plays a playlist of one.
- Manual override: pin a scene until released (via MQTT select / HTTP).
- Transitions: none / cut first; slide/fade later.

---

## 6. Sources

`Source` trait, each running as its own tokio task, writing into the store:

```rust
// As built (crates/paneld/src/sources/mod.rs). No async-trait or tokio-util:
// a boxed future and our own `Shutdown` (a tokio watch channel) do the job.
pub trait Source: Send {
    fn run(self: Box<Self>, ctx: SourceCtx) -> SourceFuture; // Pin<Box<dyn Future<Output = anyhow::Result<()>> + Send>>
}
pub struct SourceCtx { pub name: Arc<str>, pub store: StoreWriter, pub shutdown: Shutdown, pub health: Arc<SourceHealth> }
```

- **Config:** each `[sources.<name>]` has a required `type`.
- **Supervisor:** rebuilds a failed source from its config and restarts it, with backoff of 1 s doubling to 60 s. The backoff resets after a run of 60 s.
- **Health:** each source has `up`, `errors`, `restarts` and `last_ok`, for `/metrics` in M5. Sources report a recoverable error with `ctx.error()`, a success with `ctx.ok()`, and return `Err` only to be restarted.
- **Writes:** `StoreWriter::apply` sets and removes several keys under one lock, so the render loop never sees half an update.
- **Subscriptions:** `Config::referenced_keys()` lists the keys the scenes read, so a source can subscribe only to those.
- **One HTTP/TLS stack:** `reqwest` and `tokio-tungstenite` (HA WebSocket), both on rustls with the `ring` provider (`crate::tls::init`), never `aws-lc-rs`. `ring` only needs a C cross-compiler for the Pi (CI installs `gcc-aarch64-linux-gnu`), not CMake. All HTTP goes through `sources::http_client()`, whose User-Agent is a generic `paneld/<version>`.
- **Hot reload:** a change to `[sources]` currently needs a restart; it logs a warning. Restarting only the changed sources on reload is a follow-up.
- **`type = "static"`:** fixed values written once, for demos and laying out scenes.

Initial sources:
1. **Home Assistant WebSocket**: authenticate with a long-lived token (env var, never in config), `subscribe_entities` or `subscribe_events(state_changed)` filtered to the entity IDs the config references. Keys: `ha:<entity_id>` plus `ha:<entity_id>.<attribute>`. Details for M3:
   - **Values:** a numeric state becomes a `Number`, with its unit taken from the `unit_of_measurement` attribute. `unavailable`/`unknown` remove the key, so widgets show `--` and conditions are false. Anything else becomes `Text`.
   - **Weather entities:** the state is the condition (`sunny`, `partlycloudy`, `rainy`, …), and the attributes hold the current `temperature`, `humidity`, `wind_speed` and so on. As far as we know, current HA no longer puts the forecast in the attributes; it comes from the `weather.get_forecasts` action, called over the WebSocket with `return_response`. **Confirm this against the HA developer docs before implementing.** When the config asks for forecasts, the source calls it on an interval (default 30 min) and writes `ha:<entity_id>.forecast.<n>.<field>` for the next few periods, with `n = 0` the soonest.
   - **Series (for sparklines):** for entities listed in `series = [...]`, the source keeps a rolling `Series` at `ha:<entity_id>@series`. It is sampled on every state change and capped by `points`. It can be backfilled at startup from HA's recorder history over the WebSocket; **confirm the command name and payload against the HA docs before implementing.**
   - **Weather icons (decided, as built):** a `condition_icons` table on the `value` widget (plus `show_text`), not a separate `weather` widget. The `value` widget already has the icon + text layout, alignment, font, colour and staleness; a `weather` widget would duplicate all of it for one extra lookup. When the value is `Text` and `condition_icons` is set, the icon comes from the table, then from a built-in map (`icons::DEFAULT_CONDITION_ICONS`) of the 15 conditions in the HA developer docs (https://contributors.home-assistant.io/core/entities/weather/, section Conditions; the page was `developers.home-assistant.io/docs/core/entity/weather/` earlier), then falls back to `icon`. Three 12x12 icons were added for it: `snow`, `moon` (`clear-night`) and `fog`. There is no partly-cloudy, hail or storm icon, so `partlycloudy` uses `cloud`, `hail` uses `snow`, `lightning-rainy` uses `bolt` and `exceptional` uses `thermometer`.
2. **MQTT subscribe**: arbitrary topics → keys, with optional JSON path extraction.
3. **HTTP poll**: URL + interval + JSON path(s) → keys. Covers weather and generic APIs.
4. **Prometheus query**: instant queries (and range queries for sparklines) against a Prometheus URL → `Number`/`Series`.
5. **Clock/system**: time, date, uptime, local CPU temp.
6. **Media folder**: images/GIFs loaded and scaled to zone size on demand (cached). *(M2 only has what the `image` widget needs: `path` files decoded at config load. The folder source, async loading and `media:` keys arrive in M3.)*
7. **HTTP push** (via control API, §7): external scripts write keys or raise notifications.
8. **Aircraft (ADS-B)**: aircraft currently near a configured point, from adsb.fi, with routes from adsbdb. Built so a local receiver can replace adsb.fi later. See §6.1.

### 6.1 Aircraft source (M3)

Chosen after comparing the free and paid APIs (2026-11-13): adsb.fi is free, needs no key, allows 1 request/s and already returns registration, type, distance and bearing. adsb.lol allows 500 requests/day without a key, which is too few for "what's overhead now". OpenSky has no aircraft type, and paid APIs are overkill.

**Provider: adsb.fi open data.** `GET https://opendata.adsb.fi/api/v2/lat/{lat}/lon/{lon}/dist/{nm}`, radius up to 250 NM. Terms (from its README, checked 2026-11-13):
- personal, non-commercial use only;
- public endpoints are limited to 1 request/s, and 400/401/403/404/429 responses count towards the limit;
- you must cite adsb.fi with a link to its home page. We do that in the README and in the preview page footer whenever the source is configured.

The response is ADS-B Exchange v2 compatible: `{"aircraft": [...], "now", "resultCount", "ptime"}`. Per aircraft we use:
- `hex`, `flight` (space-padded), `r` (registration), `t` (ICAO type), `desc`;
- `alt_baro` (feet, or the string `"ground"`), `gs` (knots), `track`, `baro_rate`, `squawk`, `emergency`;
- `lat`, `lon`, `dst` (NM from the query point), `dir` (bearing in degrees), `seen_pos`.

adsb.lol returns the same objects under `ac` instead of `aircraft`, so the parser accepts both.

**Config:**
```toml
[sources.aircraft]
type = "aircraft"
# A URL template; {lat} {lon} {radius_nm} are filled in. Default: adsb.fi.
url = "https://opendata.adsb.fi/api/v2/lat/{lat}/lon/{lon}/dist/{radius_nm}"
location = { lat = -33.94, lon = 151.18 }   # the point to measure from
location_decimals = 2      # round before sending (2 = about 1 km); see Privacy
radius_nm = 10
max_altitude_ft = 15000    # optional; ignore aircraft above this
include_ground = false     # ignore aircraft reporting alt_baro = "ground"
interval_s = 15            # >= 1; adsb.fi allows 1/s, so leave headroom
routes = true              # look up routes on adsbdb (below)
list_len = 5               # how many aircraft go into aircraft:list
```

**Store keys.** Aircraft are filtered (radius, altitude, ground) and then sorted by distance.
- `aircraft:count` (`Number`).
- `aircraft:nearest.*` describes the nearest aircraft:
  - text: `callsign` (trimmed), `registration`, `type`, `desc`, `bearing_compass` (`N`, `NE`, …), `route` (`SYD→AKL`), `origin` and `destination` (IATA, else ICAO);
  - numbers: `alt_ft`, `speed_kt`, `dist_nm`, `bearing`.
- `aircraft:list` (`Json`): the nearest `list_len` aircraft with the same fields, for a list widget later. This adds the `Json` store variant that §5.3 already plans.
- When nothing is in range, `count` is 0 and the `nearest.*` keys are **removed**, so `value` widgets show `--` and conditions such as `aircraft:nearest.alt_ft < 5000` are false. This needs a `StoreWriter::remove`.
- When a fetch fails, the last values stay, and their `updated_at` ages so `stale_after_s` can grey them out.

**Rate limits and errors.**
- Never poll faster than `interval_s`.
- A 429 waits for `Retry-After` (or 60 s).
- Other errors back off exponentially, up to 5 minutes. They are counted in `source_errors_total{source}` and reported by the M3 supervisor.
- The User-Agent is a generic `paneld/<version>`, with nothing that identifies the user.

**Routes: adsbdb.** `GET https://api.adsbdb.com/v0/callsign/{callsign}` returns airline, origin and destination. The rules:
- Look up only the aircraft in `aircraft:list`, and only from the source task, never the render thread.
- Cache results in memory only: up to 512 callsigns for 12 h, and misses for 1 h. adsbdb's route data may not be copied into other databases, so nothing is persisted.
- `routes = false` turns lookups off.

**As built (differences and details, 2026-11-14).**
- *Key prefix:* keys use the source's **name**, so `[sources.aircraft]` writes `aircraft:*` and `[sources.planes]` writes `planes:*`. All keys of one poll, starting with `RemovePrefix("<name>:nearest.")`, go in one `StoreWriter::apply` batch.
- *Intervals:* the wait is measured from the end of a poll, so polls are never closer than `interval_s`. A 429 waits `max(Retry-After, interval_s)` (default 60 s, at most 1 h). Other errors wait `interval_s * 2^n` for the n-th consecutive failure, capped at 5 min. A failed poll leaves the store untouched and calls `ctx.error()`.
- *Routes:* lookups never block the poll. A poll publishes positions with whatever routes are cached, then one background task looks up the missing callsigns one at a time. Each finished lookup republishes the last good list from the cache (no request to the provider), unless the latest poll failed, so stale values are never refreshed by it. Lookup errors (network, 5xx, 429) are cached for 60 s, so a flaky adsbdb is not hammered; 404 and unknown callsigns are cached for the 1 h miss time. Callsigns are used only if they are 2 to 8 ASCII letters or digits.
- *Parsing:* aircraft without `lat`/`lon` are skipped. `alt_geom` is used if `alt_baro` is missing. `emergency = "none"` counts as no emergency. Aircraft with unknown altitude pass `max_altitude_ft`. Distance and bearing are always computed locally from the *exact* configured location; the provider's `dst`/`dir` are ignored, since they are relative to the rounded query point. Only the rounded point is sent, and the request radius is widened by the most rounding can move it (`query_radius_nm`: +0.42 NM at 2 decimals), so nothing in range is lost.
- *Tests:* intervals, the backoff cap and the adsbdb base URL are injected (`Tuning`), not config fields, so no config surface was added for tests.

**Privacy.** A radius query has to send a location. We send it rounded to `location_decimals` (default 2, about 1 km), and the exact home coordinates are never logged at info level or above. The committed `config/example.toml` uses a public landmark, never a real home. Taking the location from HA's `zone.home` (`location = "ha:zone.home"`) is a possible follow-up once the HA source exists.

**Handles for a local receiver.** The source depends on one contract, so a local receiver can replace adsb.fi with only a `url` change:
- **Endpoint shape:** an HTTP GET to the templated `url` returns a JSON object with an `aircraft` (or `ac`) array of ADS-B Exchange v2 aircraft objects.
- **What it can leave out:** `dst`/`dir` (we compute them with haversine from `location`), `r`/`t`/`desc` (those keys are left unset), and the radius filter (we filter again ourselves anyway).

Two local setups that need no paneld change:
1. **readsb/tar1090 directly:** `url = "http://receiver.local/tar1090/data/aircraft.json"`. It has the same per-aircraft fields. `r`/`t` only appear if readsb runs with its aircraft database (`--db-file`).
2. **A small separate daemon (later, a separate process):** wraps readsb plus an aircraft database and serves adsb.fi's path, `/v2/lat/{lat}/lon/{lon}/dist/{nm}`, with identical JSON. paneld just points `url` at it. It can also add things adsb.fi can't, such as per-receiver signal strength.

A local source can poll every 1 s, since there is no external limit. Internally everything parses into one `Aircraft` struct; providers differ only by URL and small parser differences (`aircraft` vs `ac`). Tests use recorded fixtures (adsb.fi, adsb.lol and a readsb `aircraft.json`, captured at a public landmark) and a mock HTTP server for the 429 and backoff paths, with no network.

**Display.** Once M3 templating exists, an example "overhead" scene with `condition = "aircraft:count > 0"` can show `{aircraft:nearest.callsign} {aircraft:nearest.route}`, the type and altitude in `value` widgets, and the distance and compass bearing.

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
Watch the config file. On change: parse + validate fully, build the new scenes and playlist (including image decoding and `Widget::prepare`) off the render thread, then hand the prepared set to the render loop, which swaps it in atomically with a non-blocking check at the top of a frame. Sources whose config changed get restarted, others keep running (source restarts arrive with M3; M2 reloads scenes and playlist only). On error: log, keep the old config, count `config_reload_errors_total` (`ReloadStats::reload_errors`; exported as `paneld_config_reload_errors_total` in M5).

As built (`reload.rs`):
- **Polling instead of the `notify` crate.** Once a second the file is read and its content hashed. This works the same for in-place writes, rename-and-replace, network filesystems and macOS, is independent of mtime granularity, and adds no dependency. The cost is up to ~2 s latency. SIGHUP (unix) reloads immediately.
- **Debounce:** a change is parsed once the same new content has been seen on two consecutive polls. Identical content (a touch) never reloads. Content that failed is not retried until it changes or SIGHUP arrives.
- **Restart-only:** `canvas.fps`, `[http]` and all `[[output]]` settings (including Colorlight gamma, brightness, colour order; applying them live would need a settings channel into each sink, deferred). A difference logs a warning naming it, and scene and playlist changes still apply. A changed canvas width/height is different: the whole reload is rejected, because scenes laid out for the new size would not fit the running canvas.
- `run --scene X` keeps showing X; if X is gone the reload is rejected.
- Image files are not watched (limitation); edit the config or send SIGHUP.

---

## 8. Configuration (TOML) example

```toml
[canvas]
width = 172          # two 172x86 panels stacked
height = 172
fps = 30

[[output]]
type = "colorlight"
interface = "eth1"           # required; at most one output per interface
gamma = 2.2                  # 1.0..=4.0
brightness = 180             # 0..=255
white_point = "#ffd8b0"      # colour temperature as the colour of full white (default #ffffff)
brightness_mode = "hardware" # "hardware": in the card's display frame; "software": in the colour LUT
# firmware_major = 11        # optional override if card discovery fails (§4.2)
color_order = "rgb"          # wire byte order: rgb, rbg, grb, gbr, brg, bgr (PROTOCOL.md "Pixel byte order")
sync_gap_us = 500            # 0..=20000: gap between the last row packet and the display frame
# M1: one output maps the whole canvas to card origin (0,0). Regions come in M6:
#   [[output.region]]        # one card, two panels stacked
#   canvas = { x = 0, y = 0, w = 172, h = 172 }
#   card_offset = { x = 0, y = 0 }
#   rotate = 0

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
type = "home_assistant"
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

[sources.aircraft]           # §6.1; adsb.fi by default, a local receiver later by changing `url`
type = "aircraft"
location = { lat = -33.94, lon = 151.18 }   # example: Sydney airport, not a home
radius_nm = 10
interval_s = 15

[playlist]
default_duration_s = 10   # used by scenes without duration_s; > 0, at most 86400

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
- `paneld probe --interface eth1 [--timeout-ms 500]`: detect card(s), print index, model, firmware, reported size and whether double sync is needed; exits non-zero if none answer. Needs CAP_NET_RAW.
- `paneld test-pattern --width W --height H [--out png] [--interface eth1]`: colour bars / gradient / panel IDs / grid / solid / walk, for bring-up. With `--interface` it sends continuously at `--fps` (default 30) until Ctrl-C, using the same sink as `run`, with `--brightness` (default 64), `--gamma` (default 1.0), `--color-order`, `--firmware-major` and `--sync-gap-us`.
- `paneld render --config path --scene home --out frame.png`: offline render of one scene (uses golden-test path)

---

## 14. Milestones
CI (GitHub Actions: fmt, clippy, tests, an aarch64 check for the Pi, a macOS check for the non-Linux stub, and MSRV) was brought forward from M5 Ops to the middle of M1, so every later phase lands on a green build.

1. **M0 Skeleton**: workspace, config parsing, canvas, render loop, preview sink, `clock` + `text` widgets, `paneld render`. Runs on a laptop.
2. **M1 Hardware**: `colorlight-proto` (with PROTOCOL.md + tests), `colorlight-net`, `probe`, `test-pattern`, ColorlightSink, gamma/brightness. Two panels lit on the Pi.
3. **M2 Scenes**: zones, playlist rotation, conditions, `value`/`image`/`sparkline` widgets, hot reload.
4. **M3 Data**: HA WebSocket, MQTT subscribe, HTTP poll, Prometheus sources, the aircraft source (adsb.fi + adsbdb routes, §6.1), templating.
5. **M4 Control**: HTTP API, MQTT discovery entities, notifications queue, quiet hours/auto-dim.
6. **M5 Ops**: metrics, systemd unit, deploy docs, cross-compile.
7. **M6 Scale**: multi-region/multi-card output map, rotation/flip.
8. **M7 (optional)** Video zone.

Each milestone ends with: tests green, `cargo clippy -- -D warnings` clean, README updated.

---

## 15. Open questions
- ~~Exact panel resolution and scan rate~~ **Answered:** 172×86, 1/43 scan (86 rows / 2 = 43 scan lines, the usual two-halves HUB75 layout). Needed for the LEDVision settings in `deploy/README.md`.
- **Still open for the user (needs the hardware, see `deploy/README.md`):** the card's firmware version (plan: flash V11.x, 11.04 if available; the sink detects the version either way, §4.2); which HUB75 port each panel is on and what LEDVision needs for the right order; the panels' colour order.
- Card firmware version currently installed. Plan: flash V11.x (11.04 if available). Either way the sink detects the version (§4.2).
- ~~Side-by-side or stacked~~ **Answered:** stacked vertically, canvas 172×172.
- Pixel byte order of the actual panels (RGB or BGR). The protocol doesn't say; confirm with the M1 `test-pattern`, then set the output's colour order.
- Which HUB75 port(s) each panel is on and how the card maps them (sets `card_offset` in the M1 output map). Confirm in LEDVision.
- ~~Which HA entities and external APIs to show first~~ **Answered (2026-11-13):**
  - **External APIs:** aircraft near me first, via adsb.fi with adsbdb routes, plus hooks for a local receiver later (§6.1).
  - **HA, first three entities:**
    - an outdoor temperature sensor (an Ecowitt-style weather station);
    - the solar inverter's AC power in W (Fronius), shown as a value and a sparkline (`series`);
    - a `weather.*` forecast entity: condition, current temperature and forecast.
  - **Location:** provided.
  - **Kept out of the repo:** the real entity IDs and the coordinates live only in the user's own config (`config/local*.toml` is git-ignored). Examples, tests and fixtures use generic IDs and a public landmark.
