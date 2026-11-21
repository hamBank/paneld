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
│   ├── INSTALL-debian.md     # install guide
│   └── README.md             # Pi setup notes
├── docs/                     # user reference: config, sources, scenes, outputs, control
└── tests/fixtures/           # captured packets, golden images
```

`docs/` is the user reference and must match the code; this file holds the design and the decisions behind it. A change to config keys, keys a source writes, the API or the MQTT topics updates the matching `docs/` page in the same change.

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
- What we do: `probe` prints each card's firmware version and configured size as a table (§4.4 for chains). `ColorlightSink` discovers cards on startup and picks the sync count from the highest firmware major found. An optional `firmware_major` override on the output wins over discovery (and is used when nothing answers; with neither, the sink warns and assumes firmware < 13). It warns if cards on one interface disagree, or if a card's reported size differs from the canvas. That last check is a warning only, because the reported size need not equal the canvas (two panels on two ports) and FPP calls the field unreliable.
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
- Each `ColorlightSink` owns one interface and an **output map** (`output_map.rs`) from canvas regions to the card's coordinate space. Without `[[output.region]]` the map is one implicit region, the whole canvas at card origin, and the sink does no extra work.
- A region is `canvas = {x,y,w,h}`, `card_offset = {x,y}` (default 0,0), `rotate` (0/90/180/270, clockwise) and `flip` (none/horizontal/vertical). **Flip is applied first, then rotation**, then the result is placed at `card_offset`; 90/270 give an `h×w` footprint. A 90° rotation puts the region's top edge on the right of the footprint. The exact formulas are in the `output_map.rs` module docs and are covered by exhaustive tests and golden images.
- `card_size = {w,h}` on the output is the card's whole display space (default: the bounding box of the regions from the card origin). Validation: regions inside the canvas; no overlap in card space (overlap in canvas space is allowed, so one area can be mirrored); every footprint inside `card_size`; card space at most 4096 per side and 4 Mpx. Errors name the output and region index.
- **Per-frame cost:** at startup a table with one `u32` per card pixel (the canvas pixel it shows, or none) is built. Each frame is one gather pass into a reused card-space buffer, *then* gamma/white point and colour order. Mapping first is no dearer than mapping last (those are per-pixel LUTs either way), reuses `PostProcess::apply_into` unchanged, and makes uncovered pixels plain black (LUT[0] = 0). An identity map is skipped entirely.
- **Uncovered card pixels and rows are sent as black**, every frame: every row of the card space is sent, full width. The card shows whatever it last received, so omitting rows would leave power-on or stale content there; the cost is a few percent of the frame in the stacked-panel case (nothing when the regions tile the card).
- Discovery's size warning compares against the card space. The debug aid `paneld render --output <n>` writes the card-space framebuffer of a colorlight output as a PNG; `paneld test-pattern --config --output [--card-space]` draws patterns through the map or directly in card coordinates.
- Multiple cards: either daisy-chained (single NIC, each card's region set in LEDVision) or one NIC per card. Support both by letting each `ColorlightSink` own an interface plus a list of regions.
- **Daisy-chain addressing (checked against the references, details and citations in `crates/colorlight-proto/PROTOCOL.md`, "Multiple receivers"):**
  - Conclusion: one NIC port is **one coordinate space**. `0x55` row packets carry a row and a pixel offset but no receiver field, as do `0x01` and `0x0A`; the layout packet `0x02` (FPP's header comment, `ColorLight_Wireshark.lua`) describes receivers as rectangles inside one total display, and FPP's pixel path numbers rows and offsets from 0 for the whole frame. So `card_offset` and the row number in the output map are positions in the whole display of that port, and each card picks its region from the layout stored on it by LEDVision. This is **inferred**, not stated by any source and not checked on hardware.
  - A `0x01` display frame is sent once per frame (twice on >= 13) for the whole port, and brightness is global. Also inferred from the lack of a receiver field.
  - Discovery asks receiver numbers 0, 1, ... (`0x07`, offset 16) and each `0x08` reply says which receiver it is (offset 98) (FPP, gbestwick under the HK post). It stops at the first silent number. Receiver x/y offsets are **not** decodable: no source says where the reply carries them, and the width/height fields may report offset + size rather than the cabinet's size (FPP's caveat and example table).
  - paneld never sends the `0x02` layout packet: it reconfigures the card, which is LEDVision's job.
  - No source states a limit on row width or total pixels; rows are split into 497-pixel packets with pixel offsets (u16).
  - What we do: `probe` prints a table of receivers (index, firmware, reported size, offset `unknown`), the card space the port expects (the largest size any receiver reports) and warns about firmware >= 13 mixed with older. `colorlight_net::check_layout(card_space, receivers)` does the same checks for the sink at startup; the sink passes its card space (the output map's `card_size`).
  - Bring-up order for a chain is in `deploy/README.md` (section 6, "Several cards on one port").

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

**As built (M4, `autodim.rs`, `colorlight_sink.rs`):**
- **Sinks follow `OutputLevel` live.** Each colorlight sink holds a `watch::Receiver<OutputLevel>` and reads it once per frame (it also wakes on a level change, so a dim change shows without waiting for a new frame). `BrightnessPlan::apply` turns the level into the sink's settings: hardware mode puts `level.scale(brightness)` into both the `0x0A` and `0x01` frames and leaves the LUT at gamma and white point; software mode keeps the card at 255 and rebuilds the LUT with that brightness (only when the level changes, never per frame). Off means black pixels and card brightness 0 in both modes; frames keep being sent so the card never shows a stale image. The preview stays true colour: it shows content, not brightness.
- **Auto-dim task** (`autodim::run`) evaluates every 30 s (every 5 s when a lux sensor is configured; the store has no change notification, so polling the snapshot is the cheap option) and sends `Command::SetDim` only when the value changes. Local time is `chrono::Local`; the logic takes the time as an argument so tests inject it. Config changes need a restart.
- **Inputs** (`[quiet_hours]`, `[auto_dim.sun]`, `[auto_dim.lux]`, see section 8): quiet hours give a fixed dim level inside a window that may wrap midnight; sun gives 255 by day and `night` after dusk with a linear ramp of `ramp_min` after sunset and before sunrise (polar day: 255, polar night: `night`); lux maps a store value through a piecewise-linear curve and is ignored when missing or older than `stale_after_s`.
- **Combination:** the dim level is the minimum of the inputs that apply (255 if none), so a stale or missing lux sensor falls back to the schedule inputs.
- **Sun times** are NOAA's general solar position equations (fractional year, equation of time, declination, hour angle for zenith 90.833 degrees; <https://gml.noaa.gov/grad/solcalc/solareqns.PDF>, from Meeus, *Astronomical Algorithms*), evaluated at the event time with one refinement pass. Tests check Greenwich and Sydney Observatory against US Naval Observatory data to within 2 minutes.

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
- **Notifications, as built (M4, `scheduler.rs` + `notifications.rs`).** The queue is part of the scheduler and uses the same injected clock:
  - *Order:* highest priority first, FIFO among equals. A *strictly* higher priority interrupts the one showing; the interrupted one returns to the queue keeping its arrival order, its repeats and the **remaining time of the current repeat** (nothing is lost, and a scene notification's `scene_elapsed` continues).
  - *Repeat* means "shown N times" **back-to-back** (`A` x3, then a later equal-priority `B`: `A A A B`), not interleaved with the playlist.
  - *Playlist:* frozen while any notification shows; when the queue empties it resumes where it left off, same scene, remaining duration preserved. A **pinned scene is interrupted too** (notifications are alerts) and is shown again afterwards.
  - `ClearNotifications` empties the queue and ends the current one. The queue holds at most 50; on overflow the lowest priority (newest among equals) waiting notification is dropped with a warning, never the one showing.
  - A config reload keeps the queue; scene notifications whose scene disappeared are dropped.
  - *Rendering:* a scene notification renders that scene with `scene_elapsed` from 0. Inline content is a one-off `Scene` (`inline.rs`) of ordinary text widgets plus a scaled icon: icon and text centred on a `background` (default black) in the notification's colour, in the largest font that fits the canvas width and height. Too wide even for the smallest font: it scrolls beside a fixed icon, in the largest font no taller than a sixth of the canvas height (about 28 px on 172 px, so roughly 9 characters are visible), at about 3 characters per second (at least 40 px/s). `[notifications]` in config (all optional): `font` (built-in or `[[font]]`; unset = auto over built-in fonts of 13 px height or more), `font_scale` (1..=8; unset = auto-fit), `background`.
  - `Status.notifications` lists the showing one first, then the waiting ones in order; `Status.scene` is the notification's scene name, or `None` for inline content.
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
- **Hot reload (as built, `Supervisor::reconcile`):** after a reload is accepted, each source is compared by name with the running one. Unchanged: left running (health and state untouched). Changed: stopped (its own shutdown channel, aborted after 5 s) and started fresh with new health counters; its old values stay until the new instance reports success, then the keys it did not rewrite are removed. Removed: stopped and its keys cleared. Added: started. Logged as `source restarted: <name> (config changed)`, `source started: <name>`, `source stopped: <name> ...`.
  - **What "changed" means:** a source's *effective config*, `sources::effective_config(scfg, referenced_keys)`, compared as `Debug` text. That is its `[sources.<name>]` table plus whatever it derives from the rest of the config. Today only Home Assistant derives anything: the `ha:` keys the scenes read, reduced to entity IDs. So a scene that starts showing another `ha:` entity restarts HA, while an edit to scenes, other sources' keys, or another attribute or `@series` of an already-subscribed entity does not. `Debug` text was chosen over `PartialEq` so a new `SourceConfig` variant needs no extra code and no edits to `config.rs`; it is deterministic because config types hold no hash maps, and it holds no secrets (only env var names). A variant that derives something from the whole config adds it in `effective_config`; one that needs an env var adds it in `check_source_environment`.
  - **Keys:** keys are prefixed by convention and not tracked per source, but every `Entry` records its `source`, so `Store::clear_source(name)` removes exactly what that source wrote, in one write, after its task has stopped (so nothing is written after the clear). It is done for removed sources. For changed ones, `Store::clear_source_older_than(name, restart time)` runs once the new instance's first `ctx.ok()` arrives (or after 10 minutes), so a key the new config no longer produces does not linger, there is no flash of placeholders for keys it still writes, and state a source re-reads from the store (the HA `@series`) survives the restart.
  - **Missing env var:** before a changed or added source is touched, `check_source_environment` runs. If it fails (e.g. the HA `token_env` is unset), that source is skipped and the old one keeps running; the error is logged and counted in `reload_errors` (the scene changes of the same reload still apply and count in `reloads`). The next reload retries, because the running fingerprint is unchanged.
  - **Wiring:** the watch task owns access to the supervisor (`Arc<tokio::sync::Mutex<Supervisor>>`, shared only with shutdown). It reconciles after sending the scene set to the render loop; the render thread never waits on it.
  - **Limitation:** the preview page's data-provider credits (`attributions`) are computed at start-up, so adding an aircraft source on reload does not add its credit until a restart.
- **`type = "static"`:** fixed values written once, for demos and laying out scenes.

Initial sources:
1. **Home Assistant WebSocket**: authenticate with a long-lived token (env var, never in config), `subscribe_entities` or `subscribe_events(state_changed)` filtered to the entity IDs the config references. Keys: `ha:<entity_id>` plus `ha:<entity_id>.<attribute>`. Details for M3:
   - **Values:** a numeric state becomes a `Number`, with its unit taken from the `unit_of_measurement` attribute. `unavailable`/`unknown` remove the key, so widgets show `--` and conditions are false. Anything else becomes `Text`.
   - **Weather entities:** the state is the condition (`sunny`, `partlycloudy`, `rainy`, …), and the attributes hold the current `temperature`, `humidity`, `wind_speed` and so on. As far as we know, current HA no longer puts the forecast in the attributes; it comes from the `weather.get_forecasts` action, called over the WebSocket with `return_response`. Confirmed against the HA docs (see `crates/paneld/src/sources/HA_PROTOCOL.md`). When the config asks for forecasts, the source calls it on an interval (default 30 min) and writes `ha:<entity_id>.forecast.<n>.<field>` for the next few periods, with `n = 0` the soonest.
   - **Series (for sparklines):** for entities listed in `series = [...]`, the source keeps a rolling `Series` at `ha:<entity_id>@series`. It is sampled on every numeric state change and capped by `series_points` (config name; the DESIGN text said `points`). At startup (and after each reconnect) it is backfilled from the recorder with `history/history_during_period` over the last 24 h, downsampled to `series_points`. That command is not in the developer docs page; it was confirmed from HA core source (`history/websocket_api.py`), and a failure is logged and ignored.
   - **As built (`sources/ha.rs`):** config `url`, `token_env`, `entities`, `series`, `series_points` (288), `forecasts`, `forecast_type` (`daily`|`hourly`), `forecast_interval_s` (1800), `forecast_len` (5). Details and deviations:
     - **`subscribe_entities`, not `subscribe_events`:** the docs describe it as the compact form (`a`/`c`/`r`) and it takes an `entity_ids` filter. A `c` diff omits `s` when only attributes changed, so the source keeps each entity's last state and attributes.
     - **Which entities:** `Config::referenced_keys()` (the keys widgets, templates, conditions and `[auto_dim.lux]` read; `ha:` keys, with attribute and `@series` suffixes stripped; the entity is the first two dot-separated parts) plus `entities`, `series` and `forecasts`. The supervisor copies the referenced keys into `HaConfig::referenced` (a `#[serde(skip)]` field) when it starts sources, so `build()` keeps its signature and reload still compares only the TOML. An empty set means no subscription (HA reads an empty `entity_ids` as "everything"). Changing which entities scenes use takes a source restart.
     - **Keys:** attributes are `Number`/`Text`/`Bool`, or `Json` for arrays and objects (null is skipped). Removed attributes and removed entities have their keys removed in the same `apply` batch. `ha:<entity>.forecast` is `Json` of the first `forecast_len` periods; one `call_service` per forecast entity at start and then every interval.
     - **Series:** kept in the source and written whole, in the same batch as the state. After a reconnect it is seeded from the store (`StoreWriter::snapshot` was added for this), so it survives drops.
     - **Connection:** one connection per run; any failure returns `Err` and the supervisor backs off (1 s to 60 s). `ping` every 30 s; no `pong` within 10 s ends the run. `auth_invalid` is an error naming `token_env`, never the token. Last values stay in the store while down. `paneld run` checks that the `token_env` variable is set at startup.
   - **Weather icons (decided, as built):** a `condition_icons` table on the `value` widget (plus `show_text`), not a separate `weather` widget. The `value` widget already has the icon + text layout, alignment, font, colour and staleness; a `weather` widget would duplicate all of it for one extra lookup. When the value is `Text` and `condition_icons` is set, the icon comes from the table, then from a built-in map (`icons::DEFAULT_CONDITION_ICONS`) of the 15 conditions in the HA developer docs (https://contributors.home-assistant.io/core/entities/weather/, section Conditions; the page was `developers.home-assistant.io/docs/core/entity/weather/` earlier), then falls back to `icon`. Three 12x12 icons were added for it: `snow`, `moon` (`clear-night`) and `fog`. There is no partly-cloudy, hail or storm icon, so `partlycloudy` uses `cloud`, `hail` uses `snow`, `lightning-rainy` uses `bolt` and `exceptional` uses `thermometer`.
2. **MQTT subscribe**: arbitrary topics → keys, with optional JSON path extraction.
   - **As built (`sources/mqtt.rs`):** config `host`, `port` (1883), `username`, `password_env`, `client_id` (`paneld-<source name>`), `keep_alive_s` (30) and `topics`, a table of store key to topic, where a topic is a string or `{ topic, path, unit }`. Keys are used as written (like `static`), not prefixed with the source name. Decisions:
     - **No wildcards:** `+` and `#` are rejected in validation. A wildcard topic would need a key per matching topic (templating from the topic), which a config-time key list can't express. One key per entry; two keys can share a topic with different paths (one subscription).
     - **Payloads:** an empty or whitespace-only payload removes the topic's keys (a deleted retained message). With `path`: JSON required, the value at the path is stored, `null` removes, a missing path or non-JSON leaves the key unchanged. Without `path`: JSON first, else the trimmed UTF-8 text through `extract::to_value`, so `21.5` is a `Number` and `on` is `Text`. Each publish is one `apply`. Retained messages are ordinary values. Bad payloads are logged once per topic (then at debug), add to the error counter, and don't flip `up`, since the connection is fine.
     - **QoS 0, clean session:** values are "latest reading", retained messages arrive on subscribe at any QoS, and a QoS 1 backlog after a reconnect would only replay stale values. rumqttc does not resubscribe on a clean session, so the source sends one SUBSCRIBE (all topics) on every ConnAck.
     - **Reconnects:** rumqttc reconnects on the next `poll()` after an error, in a tight loop if the broker is down. The source calls `ctx.error()`, waits 1 s doubling to 30 s (reset on ConnAck) and polls again; `ctx.ok()` on ConnAck and on each publish. Last values stay in the store while down. Only a failed subscribe request (channel full, which can't happen with the sized channel) returns `Err`.
     - **Secrets:** the password lives in a `Password` type with a redacted `Debug`; rumqttc's `MqttOptions` `Debug` prints credentials, so it is never printed. `password_env` requires `username` (MQTT 3.1.1 §3.1.2.9) and is checked at `run` start.
     - **Reuse for M4:** `ClientSettings` (host, port, credentials, client id, keep-alive, `options()`) is separate from the topic handling, so §7.2's publishing client can build from the same settings.
     - **Plain TCP only (follow-up: TLS):** `rumqttc` is added with `default-features = false`. Its `use-rustls-no-provider` feature was tried: it keeps one `rustls` 0.23 and no `aws-lc-rs`, but adds a second `rustls-webpki` (0.102 beside 0.103), `rustls-native-certs` and `rustls-pemfile` (about 85 more lockfile lines than plain TCP), and needs our own `rustls::ClientConfig` (ring provider, CA file or webpki roots) and config fields (`tls`, `ca_file`) wired through. Home LAN brokers are usually plain, so that waits for a need. Until then don't point this source at a broker across the internet.
     - **Tests:** a fake MQTT 3.1.1 broker on `127.0.0.1:0` (CONNECT/CONNACK, SUBSCRIBE/SUBACK, QoS 0 PUBLISH, PINGREQ, DISCONNECT, hard drop), packet layouts cited from the OASIS spec in comments.
3. **HTTP poll** (`sources/http_poll.rs`): URL + interval + JSON path(s) → keys. Covers weather and generic APIs. As built:
   - **Config:** `url`, `interval_s` (1 to 86400, 60), `extract` (key → `"$.path"` or `{ path, unit }`), `headers`, `header_env` (header name → env var name), `timeout_s` (10). Paths use the shared `extract.rs` syntax and are checked at config load (errors name the column). `headers` rejects credential-looking names (`auth`, `key`, `token`, `cookie`, ...): secrets must use `header_env`. The variables are read when the source is built and `paneld run` checks they are set at start (`check_environment`); values are `HeaderValue::set_sensitive`, so they never appear in `Debug`, logs or errors.
   - **Writes:** one `apply` batch per successful poll. A path that is missing or `null` in a successful response **removes** its key. A failed poll (transport error, non-2xx, 429, body not JSON) writes nothing, so old values stay.
   - **Polling:** shared with the aircraft source in `sources/poll.rs`: never faster than `interval_s`; `Retry-After` (seconds form, default 60 s, at most 1 h) on 429; otherwise errors back off `interval * 2^n` capped at 5 min; 8 MiB body cap; errors are logged without the URL (API keys often live in query strings); redirects are followed only to the same host and port, so credentials never travel elsewhere.
4. **Prometheus query** (`sources/prometheus.rs`): instant queries (and range queries for sparklines) against a Prometheus URL → `Number`/`Series`. Confirmed against https://prometheus.io/docs/prometheus/latest/querying/api/ (`/api/v1/query`, `/api/v1/query_range`, envelope `status`/`data`/`errorType`/`error`, `resultType` `vector`/`scalar`/`matrix`, samples as `[ts, "string"]`, 400/422/503 for errors). As built:
   - **Config:** `url` (server base, a path prefix is fine), `interval_s` (15), `queries` (key → PromQL or `{ query, unit }`), `range_queries` (key → `{ query, range_s, step_s }`; `step_s <= range_s`, at most 11000 points), `bearer_env`, `timeout_s` (10).
   - **Instant results:** a scalar gives its value; a vector with one series gives its value; an empty vector, `NaN`/`Inf` or a native histogram removes the key; a vector with several series writes nothing for that key (old value stays), logs a warning naming the key and series count, and counts `ctx.error()`. A 400/422 for one query is handled the same way (the server's `errorType: error` text is logged, capped at 200 chars). **Range:** `Series` from the first series, oldest first, non-finite samples skipped.
   - **Execution:** all queries of a round run concurrently, at most 4 at once, and everything that succeeded is written in one batch. If every query fails with a transport error, 5xx or 401/403, the whole source backs off; 429 honours `Retry-After`. Backoff and the other polling rules are the same as for HTTP poll (`sources/poll.rs`).
5. **Clock/system**: time, date, uptime, local CPU temp. Time and date are the `clock` widget. As built (`sources/system.rs`, `type = "system"`):
   - **Keys** (prefixed with the source name, like `aircraft`): `uptime_s` (`s`), `uptime` (Text, two units: `3d 4h`), `cpu_temp_c` (`°C`), `load_1m`, `mem_used_pct` (`%`, `1 − MemAvailable/MemTotal`). Load and memory were added beside the planned two because they come from the same `/proc` reads and are the usual "is the Pi OK" numbers.
   - **Sources of truth:** `proc(5)` for `/proc/uptime`, `/proc/loadavg`, `/proc/meminfo`; the kernel's `sysfs-class-thermal` ABI for `thermal_zone*/type` and `temp` (millidegrees). The CPU zone is the first whose `type` contains `cpu`, `x86_pkg_temp` or `soc` (Pi: `cpu-thermal`), else zone 0; `thermal_zone` overrides it.
   - **Config:** `interval_s` (10, 1–3600), `thermal_zone`.
   - **Failures:** a reading that fails removes its key; if none works (macOS, a container without `/sys/class/thermal`) the source warns once and counts an error each round, but keeps polling. Reads run on `spawn_blocking`. The systemd unit's sandbox leaves `/proc` and `/sys` readable (no `ProcSubset=pid`).
6. **Media folder**: images/GIFs loaded and scaled to zone size on demand (cached). As built (`sources/media.rs`, `type = "media"`), after the docs pass found it missing:
   - **Keys:** `<name>:<file stem>` per `.png`/`.gif` in `dir` (not recursive), `<name>:count`, and with `rotate_s` a `<name>:current` that cycles in file name order and stays on the same image across rescans. Stems must be valid key names; `count` and `current` are reserved.
   - **`Value::Image` now holds `Arc<Media>`** (all frames) instead of one `Bitmap`, so GIFs from the store animate like `path` ones. The image widget compares by pointer, so an unchanged image is never rescaled.
   - **Off the render thread:** decoding and a `max_px` (512) downscale run on `spawn_blocking`. "Scaled to zone size" stays in the widget's existing per-(image, zone) cache, which is now cheap because no stored image is bigger than `max_px`. The source cannot know zone sizes, so it caps the size instead.
   - **Rescan** every `interval_s` (60). A file is decoded again only when its size or mtime changes; removed files remove keys; each scan is one `apply`. Limits: 256 files and 256 MiB of decoded pixels per source, plus the widget's per-file limits. Bad files are warned about once and don't count as source errors; an unreadable folder does.
   - **Config:** `dir` (relative to the config file, must exist at load), `interval_s`, `rotate_s`, `max_px` (16–4096). No new dependency.
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
  - text: `callsign` (trimmed), `registration`, `type`, `desc`, `bearing_compass` (`N`, `NE`, …), `route` (`SYD-AKL`; `route_separator`, default `-` because the built-in ISO-8859-1 fonts have no `→`), `origin` and `destination` (IATA, else ICAO);
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

**Shared core (as built, `control.rs`).** Every control surface uses one cloneable `Control` handle: the HTTP API, the MQTT/HA entities and the auto-dim task. It covers three things:
- **Commands** go over a std channel to the render loop, which applies them at the top of the next frame without blocking:
  - `Notify`, `ClearNotifications`;
  - `Pin(Some(name) | None)`;
  - `SetPower`, `SetBrightness(0–255)`, and `SetDim(0–255)` from auto-dim.
- **`Status`** comes back on a `watch` channel, re-published only when it changes, and is serialised as-is for `GET /api/state`. It holds the scenes, the scene on screen, the pin, the notification queue, on/off, brightness, dim and the actual fps.
- **`OutputLevel { on, level }`** goes to the sinks on another `watch`:
  - `level = brightness × dim / 255`, and each output's configured `brightness` is its calibrated maximum, scaled by `level / 255`;
  - off means black, and hardware brightness 0 where the sink supports it.
- **Notification requests** have the same JSON shape on HTTP and MQTT: `text` (with optional `icon` and `color`) or `scene`, plus `duration_s` (10), `priority` (0, ±100) and `repeat` (1). `NotifyRequest::validate` checks them against the configured scenes and icons.
- **Pinning** is in the scheduler. A pinned scene shows regardless of its condition, while the playlist's own clock keeps running; notifications still interrupt it (§5.5). A pin survives config reloads by scene name, and is released if the scene disappears.

### 7.1 HTTP (axum)
- `GET  /metrics`: Prometheus text format 0.0.4, never authenticated (§10)
- `GET  /preview.png`, `GET /preview`: current frame / tiny HTML page. Never authenticated.
- `GET  /api/state`: current scene, pin, notification queue, on/off, brightness, dim, fps (200)
- `POST /api/notify`: `{ "text": "...", "icon": "...", "color": "#ff0000", "duration_s": 10, "priority": 5 }` or `{ "scene": "..." }` (202; 400 on bad input); `DELETE /api/notify` clears all (202)
- `POST /api/data/{key}`: push a value into the store (200; 400; 409)
- `POST /api/scene/{name}`: pin scene (202; 404 unknown); `DELETE /api/scene`: release (202)
- `POST /api/brightness`: `{ "value": 0-255 }` (202)
- `POST /api/power`: `{ "on": bool }` (202). Added in M4 because HA's `light` entity needs on/off separately from brightness.
- Bind address configurable.

As built (`api.rs`):
- **Errors** are always `{ "error": "..." }`: 400 (bad input), 401 (auth), 404, 409, 413 (body over 64 KiB), 503 (render loop gone). Bodies are parsed with `serde_json` from raw bytes instead of axum's `Json` extractor, so malformed JSON, unknown fields and a missing content type give clear 400s rather than axum's plain-text rejections.
- **Auth:** optional `[http] token_env` (name of an env var). When set, every `/api/*` route needs `Authorization: Bearer <token>`, compared in constant time (hand-written, no crate); otherwise 401 with `WWW-Authenticate: Bearer`. A set but missing/empty variable fails `run` at start-up, naming the variable. `/preview*` and `/metrics` stay open (documented limitation; put a reverse proxy in front if that matters). An unauthenticated API on a non-loopback bind logs a warning. The token is a `Secret` (redacted `Debug`). `token_env` and `data_prefix` are restart-only.
- **`POST /api/data/{key}` namespace (decision):** pushed keys must be `<data_prefix>:<name>` (`[http] data_prefix`, default `push`). Config validation rejects a `data_prefix` that any source uses (source names, `ha`, and prefixes of explicit keys in `static`/`mqtt`/`http_poll`/`prometheus`), so a push can never fight a source, including after a reload. Rationale: sources name their keys freely (`prom:x` under any source name), so "not a source's prefix" can only be decided against a dedicated namespace; a blocklist would break on every config change. As a second guard, a push never overwrites or removes a key whose entry was written by another source (409), and at most 1000 keys can be pushed (409). Values are stamped with source `http`. Body: number, string, bool, `null` (removes the key) or `{ "value": ..., "unit": "W" }`; via `extract::to_value`, so numeric strings become numbers. Arrays and objects are rejected.

### 7.2 MQTT + Home Assistant discovery
Publish discovery configs so HA auto-creates one device with:
- `light.<name>`: on/off + brightness (maps to global brightness / blank)
- `select.<name>_scene`: current/pinned scene (options = configured scenes + "auto")
- `text.<name>_message`: setting it raises a notification
- `binary_sensor.<name>_card`: card detected/responding
- `sensor.<name>_fps`: actual fps

Also accept raw notification JSON on `<prefix>/notify` (same schema as HTTP). Use `rumqttc`. Availability topic + LWT.

**As built (`mqtt_ha.rs`, formats checked against HA's docs: `mqtt_ha/HA_DISCOVERY.md`):**
- **Config:** top-level `[mqtt]` (`host`, `port` 1883, `username`, `password_env`, `discovery_prefix` `homeassistant`, `node_id` `paneld`, `base_topic` default `paneld/<node_id>`, a private namespace per node so two panels never collide). Absent: off. `password_env` is part of the startup env check. Read at startup only (like `[http]`). Plain TCP, same as the MQTT source (§6.2).
- **One tokio task** next to the HTTP server, reusing `ClientSettings`/`Password` from `sources/mqtt.rs`; reconnects with 1 s to 30 s backoff, and on shutdown publishes `offline` itself (a clean DISCONNECT does not fire the will) before disconnecting.
- **Topics** (`B` = base, `P` = prefix, `N` = node id; QoS 0, state and configs retained):

| Entity | Config | State | Command |
|---|---|---|---|
| `light.paneld` (JSON schema, `brightness_scale` 255) | `P/light/N/light/config` | `B/light/state` `{"state","brightness","color_mode"}` | `B/light/set` |
| `select.paneld_scene` | `P/select/N/scene/config` | `B/scene/state` (pin or `auto`) | `B/scene/set` |
| `text.paneld_message` | `P/text/N/message/config` | none | `B/message/set` |
| `binary_sensor.paneld_card` | `P/binary_sensor/N/card/config` | `B/card/state` `ON`/`OFF` | none |
| `sensor.paneld_fps` | `P/sensor/N/fps/config` | `B/fps/state` | none |

  Also `B/availability` (`online`/`offline`, retained; last will `offline`, QoS 1, retained), `B/notify` (raw `NotifyRequest` JSON, validated like HTTP; invalid ones are logged at warn and dropped) and `P/status` (HA's birth message).
- **Commands:** light `{"state": "ON"|"OFF", "brightness": 0-255}` becomes `SetBrightness` then `SetPower`; select becomes `Pin(Some(scene))` or `Pin(None)` for `auto` (unknown names are ignored); text becomes a `Notify` with the default duration, priority and repeat; `B/notify` becomes `Notify`.
- **Light brightness is the user brightness** (`Status.brightness`), not the effective level after auto-dim, so HA's slider shows what it set and does not move when the schedule dims the panel.
- **Republish:** availability, all configs and all states on every ConnAck and when HA publishes `online` on the birth topic; the select config again whenever `Status.scenes` changes (reload); states only when they change, driven by `Control::subscribe()`. FPS is published when it moves by more than 1 or changes after 10 s. With no Colorlight output the card config is replaced by an empty retained payload, which removes a stale entity.
- **Card:** sampled every 5 s from `SinkStats` (`main` passes them in): ON when every sink sent frames since the last sample and none had a send error.
- A scene literally named `auto` is shadowed by the `auto` option.

### 7.3 Config hot reload
Watch the config file. On change: parse + validate fully, build the new scenes and playlist (including image decoding and `Widget::prepare`) off the render thread, then hand the prepared set to the render loop, which swaps it in atomically with a non-blocking check at the top of a frame. Sources whose config changed get restarted, others keep running (see §6, "Hot reload", for how "changed" is decided, what happens to a removed source's keys and the environment-variable check). On error: log, keep the old config, count `config_reload_errors_total` (`ReloadStats::reload_errors`; exported as `paneld_config_reload_errors_total` in M5).

As built (`reload.rs`):
- **Polling instead of the `notify` crate.** Once a second the file is read and its content hashed. This works the same for in-place writes, rename-and-replace, network filesystems and macOS, is independent of mtime granularity, and adds no dependency. The cost is up to ~2 s latency. SIGHUP (unix) reloads immediately.
- **Debounce:** a change is parsed once the same new content has been seen on two consecutive polls. Identical content (a touch) never reloads. Content that failed is not retried until it changes or SIGHUP arrives.
- **Restart-only:** `canvas.fps`, `[http]`, `[mqtt]`, `[quiet_hours]`, `[auto_dim]`, and the output *topology* (adding or removing an `[[output]]`, or changing an output's `interface`; `live_outputs::topology`).
- **Output settings are applied live** (`live_outputs.rs`, added after M7):
  - **Colorlight:** each sink has a settings `watch` channel (`SinkHandle::update` / `SinkUpdater`). After an accepted reload, `OutputReload::apply` matches outputs by interface, compares each table's `Debug` text with what that sink last got, and sends new `SinkSettings` only on a difference.
  - **What the sink thread does with them:** it wakes on the channel and swaps in the brightness plan (gamma, brightness, white point, mode), colour order, sync gap and output map before the next frame.
  - **Firmware:** discovery runs once per sink, and its result is kept, so a changed `firmware_major` is resolved again without re-discovering.
  - **Preview:** `scale` and `grid` sit behind a `watch` (`PreviewControl`).
  - **Why the topology stays restart-only:** sink threads, their `/metrics` series and the HA card sensor are wired up at start-up. An output whose new settings cannot be built (an output-map error that validation missed) keeps its old ones and counts a reload error.
- **Restart-only changes** log a warning naming them, and scene and playlist changes still apply. A changed canvas width/height is different: the whole reload is rejected, because scenes laid out for the new size would not fit the running canvas.
- **`[sources]` is applied live**, per source (§6). It is no longer restart-only.
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
# Output map (§4.4). Without regions the whole canvas goes to card origin (0,0).
# `card_size` is a key of the output, so it goes before the first region.
#   card_size = { w = 172, h = 172 }   # optional; default: bounding box of the regions
#   [[output.region]]        # one card, two panels stacked, the bottom one upside down
#   canvas = { x = 0, y = 0, w = 172, h = 86 }
#   [[output.region]]
#   canvas = { x = 0, y = 86, w = 172, h = 86 }
#   card_offset = { x = 0, y = 86 }    # default 0,0
#   rotate = 180                       # 0 (default), 90, 180, 270, clockwise
#   flip = "none"                      # none (default), horizontal, vertical; applied before rotate

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
queries = { "prom:house_power" = { query = "sum(power_watts)", unit = "W" } }
range_queries = { "prom:house_power_hist" = { query = "sum(power_watts)", range_s = 3600, step_s = 60 } }
interval_s = 10

[sources.weather]
type = "http_poll"
url = "https://example/api/forecast"
interval_s = 600
extract = { "weather:temp" = { path = "$.current.temp", unit = "C" }, "weather:icon" = "$.current.icon" }
header_env = { "X-Api-Key" = "WEATHER_API_KEY" }   # header name -> env var NAME

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
brightness = 20          # dim level 0-255 inside the window (wraps midnight)

# Optional, see section 5.2:
# [auto_dim.sun]
# location = { lat = 51.4769, lon = -0.0005 }
# night = 40
# ramp_min = 30
# [auto_dim.lux]
# key = "ha:sensor.lux"
# curve = [[0, 20], [50, 120], [400, 255]]
# stale_after_s = 600
```

Secrets only via env vars (`*_env` keys). Never commit tokens.

---

## 9. Video zone (M7)
- A `video` widget that owns a zone and pulls frames from an `ffmpeg` subprocess (`-f rawvideo -pix_fmt rgb24 -s WxH`) into a ring buffer, decoded off the render thread.
- Render thread just blits the latest decoded frame, so the frame loop design is unchanged.
- Sources: file, RTSP (e.g. a camera via HA/go2rtc).
- Keep the `Widget` trait and canvas API compatible with this. That's the only requirement now.
- **M7 groundwork (as built, `video/mod.rs`):**
  - `VideoHandle` holds a `FrameSlot`, which keeps only the latest zone-sized RGB888 `Frame`.
  - The widget calls `touch()` on every `update`, which happens only while its scene is shown, and `latest()` to read the newest frame.
  - The decoder starts `ffmpeg` on the first touch and stops it after `IDLE_STOP` (5 s) without a touch. A video scene in a playlist therefore costs nothing while other scenes show.
  - `ffmpeg` scales to the zone, so the widget only copies bytes.
  - Dropping the handle stops everything.
- **M7 decoder (as built, `video/decoder.rs`; the module doc has the details and sources):**
  - **Threads:** one supervisor `std::thread` per zone (no tokio) owns the `ffmpeg` `Child` and polls every 20 ms for stop, idle and reader exit. A reader thread reads exactly `width*height*3` bytes per frame from stdout into `FrameSlot::put` (a partial frame at EOF is discarded). A stderr thread drains the pipe.
  - **Lifecycle:** idle (polls `wanted()` every 100 ms) -> run while `wanted(IDLE_STOP)` -> kill and `wait()` when unwanted (the slot is **cleared**, so a returning scene never shows a stale picture). EOF, a crash or a bad exit counts `stats.errors` and restarts with backoff 1 s doubling to 30 s, reset after a run that delivered frames for 60 s. On failure the last frame is kept up to 10 s, then cleared. A non-looping file that plays to the end keeps its last frame and is not restarted until the zone goes idle. Dropping the guard kills and reaps `ffmpeg` within about a second.
  - **Command** (`ffmpeg_args`, pure): `-nostdin -hide_banner -loglevel error`, then input options, `-i SRC -an -vf <fit>,fps=N,format=rgb24 -f rawvideo -pix_fmt rgb24 pipe:1`. Files: `-re`, plus `-stream_loop -1` when looping. RTSP: `-rtsp_transport tcp -timeout 10000000` (microseconds; `-timeout` is the socket I/O timeout since FFmpeg 5.0, so Debian 12 and 13 both have it). HTTP(S): `-rw_timeout 10000000`. Fit: contain `scale=W:H:force_original_aspect_ratio=decrease,pad=W:H:(ow-iw)/2:(oh-ih)/2:color=black`; cover `scale=...=increase,crop=W:H`; stretch `scale=W:H`; none `pad=max(iw\,W):max(ih\,H):(ow-iw)/2:(oh-ih)/2:color=black,crop=W:H`. Sources: ffmpeg.html, ffmpeg-protocols.html, ffmpeg-filters.html, and `ffmpeg -h` of the local 6.1 binary.
  - **Secrets:** the URL is never logged. stderr lines go through `redact` (everything after `scheme://` up to whitespace becomes `***`, plus an exact-match pass on the URL), are cut at 200 bytes and are limited to 5 per 10 s.
  - **Pi CPU:** decoding cost follows the source resolution, not the zone size. Use a camera sub-stream (about 640x360) where possible; `fps=` only caps the output. A 1080p H.264 stream is roughly one Pi 4 core in software.
  - **Tests:** shell-script fake decoders (ordering, `wanted` gating, backoff, partial frame, drop kills even a hanging process with a grandchild) and real `ffmpeg` runs on generated NUT clips (all four fits, looping, one-shot). The real tests fail, not skip, without `ffmpeg`; CI installs it.
- **M7 widget (as built, `widgets/video.rs`, `config.rs` `VideoConfig`):**
  - Config `{ type = "video" }`: exactly one of `path` (relative to the config dir, must exist at load) or `url_env` (NAME of an env var holding the stream URL); `fit` (default `contain`), `fps` (1 to 60, default 15), `loop` (files, default true), `program` (default `ffmpeg`). A literal `url` is rejected (credentials), and its value is discarded at parse time so it cannot reach an error or `Debug` output.
  - The URL is read from the environment when the widget is built (`VideoConfig::input`, wrapped in the same `Secret` as the Home Assistant token), never stored in the config. `Config::check_video_environment` fails `run` startup and rejects a reload when the variable is unset or empty. `paneld check` only warns (`Config::video_warnings`: `ffmpeg` not on `PATH`, variable unset in this shell), because it often runs on another machine or shell than the service.
  - `prepare(w, h)` creates the `VideoHandle` at zone size (cheap, `ffmpeg` starts on the first touch); if it never ran, `draw` creates it. `update` touches and takes `latest()`; `draw` blits it (`ZoneTarget::blit_rgb`), no scaling. No frame, or a frame whose size is not the zone's, fills the zone black (opaque, ignores `background`, no "no signal" text).
  - Tests use `VideoHandle::detached()` and `shared().slot.put`; golden images `video_frame` and `video_no_frame` (`tests/golden_video.rs`).
  - The systemd unit's sandbox allows `ffmpeg` (see `deploy/README.md`).

---

## 10. Observability
Prometheus metrics at `GET /metrics`, text exposition format 0.0.4 (`Content-Type: text/plain; version=0.0.4; charset=utf-8`), prefix `paneld_`. Names follow the Prometheus naming practices (base units, `_total` on counters, `_timestamp_seconds`, `_build_info`). Full list with types and labels: README "Metrics".

As built (`metrics.rs`):
- **No crate.** The text format is hand-written (`Encoder`): HELP/TYPE, label escaping (`\`, `"`, newline), cumulative histogram buckets ending in `+Inf`, `_sum`, `_count`. Sources: <https://prometheus.io/docs/instrumenting/exposition_formats/>, <https://prometheus.io/docs/practices/naming/>, <https://prometheus.io/docs/concepts/metric_types/>.
- **Histograms** (`Histogram`) are lock-free: one `AtomicU64` per bucket plus an atomic sum in nanoseconds. `observe` is a few relaxed atomic adds, so the render thread and the sink threads record without locks or I/O. The render loop times each frame from the start of the tick's work to the publish (not the sleep); a sink times each frame send. Buckets (seconds): render 0.0005 0.001 0.002 0.005 0.01 0.02 0.033 0.05 0.1; sink send 0.0001 0.0002 0.0005 0.001 0.002 0.005 0.01 0.02 0.05. `_count` and `+Inf` are the total of the buckets read, so they always agree; the sum may lag by one in-flight frame.
- **Scraping never blocks the pipeline.** The handler reads atomics, one `watch` status borrow and a store snapshot (one `Arc` clone). It takes the supervisor's `tokio::sync::Mutex` only to clone the health map, waits at most 100 ms for it, and on timeout (a reload is stopping sources) serves the source list of the previous scrape. The lock is dropped before encoding.
- **`/metrics` is outside the API auth layer** (like `/preview*`), so a Prometheus server needs no token. It exposes names from the config (interfaces, source and scene names) and counts, never URLs, locations, tokens or store values. To restrict it, bind `[http] bind` to loopback and scrape from the same host or through a reverse proxy, or firewall the port.
- **`paneld_card_detected{card}`** reuses the Home Assistant card logic (`mqtt_ha::CardCheck`): 1 if the sink sent frames and had no send error since the previous scrape. Each registry keeps its own previous counters, so scrapes and HA sampling do not disturb each other. With several scrapers the window is the time since any scrape.
- **`paneld_current_scene{scene}`** is an info-style gauge: value 1 for the scene on screen, no sample at all while nothing is eligible (black) or an inline notification shows (`Status.scene` is `None`). Query `count(paneld_current_scene) == 0` for "no scene".
- `paneld_source_last_update_timestamp_seconds{source}` has no sample until the source first succeeds. Per-source counters restart from 0 when a config reload restarts that source (normal counter reset).
- Additions beyond the original list: `paneld_build_info{version}`, `paneld_config_reloads_total`, `paneld_source_restarts_total{source}`, `paneld_fps`, `paneld_brightness` (0-255), `paneld_dim` (0-255), `paneld_power_on`. Names differ from the first sketch only by the unit suffix (`..._timestamp_seconds`).
- Tests: encoder unit tests, a golden scrape (`tests/fixtures/golden/metrics.txt`, `UPDATE_GOLDEN=1`), a parser-based format checker run over every scrape, and HTTP tests with and without an API token.

Logging: `tracing` with env-filter (`RUST_LOG`, default `info`), to stdout. `--log-format text|json` (global flag; env `PANELD_LOG_FORMAT`; default `text`) selects the format; `json` is one object per line (`timestamp`, `level`, `fields`, `target`) via tracing-subscriber's `json` feature (adds only `tracing-serde`). Text is the right choice for journald, which stores plain lines and adds its own metadata; JSON is for log shippers (Vector, Loki). ANSI colours only when stdout is a terminal. Secrets never reach log fields: tokens only come from `*_env` variables, and source URLs are never logged (they may hold query-string keys; reqwest errors are logged with `without_url()`). Implementation: `crates/paneld/src/logging.rs`.

---

## 11. Deployment (Pi)
- Raspberry Pi 4/5, Raspberry Pi OS Lite 64-bit. Onboard Ethernet → LAN; USB 3 Gigabit adapter → Colorlight card (or the reverse; config picks the interface). Wi-Fi for LAN is also fine.
- The card-facing interface gets **no IP config** (disable DHCP on it in NetworkManager/dhcpcd) so the OS doesn't spam it.
- Cross-compile for `aarch64-unknown-linux-gnu`: plain `cargo build --release --locked --target aarch64-unknown-linux-gnu -p paneld` with `gcc-aarch64-linux-gnu` installed (`.cargo/config.toml` sets the linker; `ring` finds the C compiler itself), or `cross` (no `Cross.toml` needed), or build on-device. `[profile.release]` uses thin LTO, `codegen-units = 1`, `strip` and keeps `panic = "unwind"` (thread panics are caught at `join`). About 11.7 MiB before, 6.8 MiB after.
- The binary is dynamic against glibc (`libc`, `libm`, `libgcc_s`) and needs glibc >= 2.34 when linked against Ubuntu 24.04's cross sysroot: fine on Pi OS 12 Bookworm (2.36) and 13 Trixie (2.41), not Pi OS 11 Bullseye (2.31). Versions from packages.debian.org. If a newer sysroot ever raises that, build in an older container or use cargo-zigbuild with an explicit glibc target; not needed now.
- CI (`.github/workflows/release.yml`) builds the binary on pushes to `main` and `v*` tags and uploads `paneld-aarch64-unknown-linux-gnu` (binary plus `.sha256`); a tag also attaches both to a GitHub release (`contents: write` only on that job). It fails if the binary needs glibc newer than 2.36 (Bookworm).
- **systemd unit (as built, `deploy/paneld.service`; install guide in `deploy/README.md`):**
  - `DynamicUser=yes`: paneld writes nothing to disk at runtime (config, fonts and images are only read; the store, route cache and HA series are in memory), so there is no state directory to own, and a config without secrets can be `0644`. A dedicated user would only be needed for persistent state.
  - `AmbientCapabilities=CAP_NET_RAW` and `CapabilityBoundingSet=CAP_NET_RAW`, `NoNewPrivileges=yes`. HTTP binds a port above 1024, so no other capability is needed.
  - `EnvironmentFile=-/etc/paneld/env` (`0600 root:root`, read by systemd): the only place for secrets; the config names variables (`token_env`, `password_env`, `header_env`, `bearer_env`). `deploy/env.example` lists them.
  - Hardening that keeps the raw socket working: `ProtectSystem=strict`, `ProtectHome`, `PrivateTmp`, `PrivateDevices`, `ProtectKernel*`, `ProtectControlGroups`, `ProtectClock`, `ProtectHostname`, `RestrictNamespaces`, `LockPersonality`, `MemoryDenyWriteExecute`, `RestrictRealtime`, `RestrictAddressFamilies=AF_UNIX AF_INET AF_INET6 AF_PACKET AF_NETLINK` (netlink for glibc name resolution), `SystemCallArchitectures=native`, `SystemCallFilter=@system-service` (contains the `@network-io` and `@io-event` groups, which cover the `socket`/`bind`/`setsockopt`/`sendmmsg`/`poll`/`recvfrom` calls of `colorlight-net`). No `PrivateNetwork` (it would hide the NIC).
  - `Restart=on-failure`, `RestartSec=5`, start limit 10 in 5 minutes (a config error should end as `failed`, not loop); `ExecReload=/bin/kill -HUP $MAINPID` (SIGHUP reloads the config, §7.3).
  - `After=`/`Wants=network-online.target` for the LAN sources only. The card NIC has no IP config and the unit does not wait for it: the sink re-opens the socket with backoff (§4.2).
  - The unit passes `systemd-analyze verify`; it has not yet been booted on a Pi (open: see `deploy/README.md`).
- Card-facing NIC: an explicit NetworkManager profile with `ipv4.method disabled` and `ipv6.method disabled` (Bookworm and later use NetworkManager, not dhcpcd), see `deploy/README.md` section 2.
- Card config (panel size/scan) done once in LEDVision from a Windows machine. Document the settings used in `deploy/README.md`.

---

## 12. Testing strategy
- `colorlight-proto`: byte-exact encode tests against fixtures from captures/reference implementations; property tests for row splitting. Region mapping (`output_map.rs`) is tested exhaustively over many small sizes with loops (every rotation × flip: bijection and inverse round-trip, offsets, several regions, uncovered pixels), plus golden images of `render --output`.
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
- `paneld test-pattern --width W --height H [--out png] [--interface eth1]`: colour bars / gradient / panel IDs / grid / solid / walk, for bring-up. With `--interface` it sends continuously at `--fps` (default 30) until Ctrl-C, using the same sink as `run`, with `--brightness` (default 64), `--gamma` (default 1.0), `--color-order`, `--firmware-major` and `--sync-gap-us`. `--config path --output N` takes the canvas size and output map from a colorlight output (patterns are drawn in canvas space and go through the map); `--card-space` draws directly in card coordinates instead.
- `paneld render --config path --scene home --out frame.png [--output N]`: offline render of one scene (uses golden-test path); with `--output` the card-space framebuffer of colorlight output N (canvas through the output map, no post-processing)

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
- ~~Two planned sources were never built (§6 items 5 and 6)~~ **Answered (2026-11-16):** build them. Done as `type = "system"` and `type = "media"`, in their own PR after the docs pass.
