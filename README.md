# paneld

A Rust daemon that composes data from Home Assistant, HTTP APIs, Prometheus and pushed messages into scenes, and drives HUB75 LED panels through a Colorlight 5A-75B card over raw Ethernet.

Architecture, config schema and roadmap are in [`DESIGN.md`](DESIGN.md).

## Status

**M0 (skeleton) is done.** It includes the config parsing and validation, canvas, render loop, HTTP preview sink, `clock` and `text` widgets, and the `run`, `check` and `render` commands. It runs on a laptop without hardware.

**M1 (hardware) is code-complete, but untested on a real card.** It adds `colorlight-proto` (packet encoding), `colorlight-net` (Linux raw-socket transport, discovery), the Colorlight sink behind `[[output]] type = "colorlight"`, `paneld probe` and `paneld test-pattern --interface`. Bring-up on the Pi follows [`deploy/README.md`](deploy/README.md); the result (firmware version, port mapping, colour order) is still to be recorded, see `DESIGN.md` §15. The sink only works on Linux; other platforms build a stub.

Scene rotation and conditions (M2) are in; more widgets are still to come (M2), data sources (M3), control API and MQTT (M4). See `DESIGN.md` §14.

## Hardware

One Colorlight 5A-75B driving two 172×86 HUB75 panels (1/43 scan), stacked vertically into a 172×172 canvas. The card's firmware version is still to be confirmed (`DESIGN.md` §15).

## Quick start

```bash
cargo run -p paneld -- run --config config/example.toml
```

Open http://localhost:8080/preview.

```bash
cargo run -p paneld -- check --config config/example.toml
```

```bash
cargo run -p paneld -- render --config config/example.toml --scene home --out frame.png --scale 8
```

```bash
cargo run -p paneld -- test-pattern --width 172 --height 172 --panel-width 172 --panel-height 86 --pattern panel-ids --out pattern.png --scale 4
```

`test-pattern` draws a bring-up pattern for checking the card's mapping: `bars`, `gradient`, `panel-ids` (border colour, index number and a top-left corner marker per panel, row-major from the top-left), `grid` (every 8 px, with each panel's top and left edge in cyan and its scan-half row `panel-height / 2` in yellow), `solid` (`--color "#rrggbb"`, default white) and `walk` (one pixel in raster order, position set by `--frame`). Panels default to the whole canvas, and partial panels at the edges are clipped. `--out` writes a PNG (`--scale` works as for `render`). `--interface <nic>` sends the pattern to the card continuously until Ctrl-C (`walk` animates), with `--fps` (default 30), `--brightness` (default 64), `--gamma` (default 1.0), `--color-order`, `--firmware-major` and `--sync-gap-us`. Give `--out`, `--interface` or both. Sending needs `CAP_NET_RAW`; see [`deploy/README.md`](deploy/README.md).

```bash
paneld probe --interface eth1            # list cards: model, firmware, reported size
paneld test-pattern --width 172 --height 172 --panel-height 86 --pattern panel-ids --interface eth1
```

`probe` prints each receiver's index, model, firmware version, reported width x height and whether it needs double sync, and exits non-zero if nothing answers (`--timeout-ms`, default 500, is the wait per request).

`render` also takes `--time <RFC 3339>` and `--elapsed <seconds>` for reproducible output.

## Config (M2)

See [`config/example.toml`](config/example.toml). Unknown keys are rejected, and validation reports every problem at once.

| Section | Keys |
|---|---|
| `[canvas]` | `width`, `height`, `fps` (1–60, default 30) |
| `[[output]]` | `type = "preview"`, `scale` (1–32, default 8), `grid` (default false) |
| `[[output]]` | `type = "colorlight"`, `interface` (required), `gamma` (1.0–4.0, default 2.2), `brightness` (0–255, default 255), `white_point` (`#rrggbb`, default `#ffffff`), `brightness_mode` (`hardware` default, or `software`), `color_order` (`rgb` default, `rbg`, `grb`, `gbr`, `brg`, `bgr`: what the three wire bytes carry; the right value depends on the panel), `firmware_major` (optional override if discovery fails), `sync_gap_us` (0–20000, default 500). One output per interface. The whole canvas goes to card origin (0,0); multi-region maps come in M6. |
| `[http]` | `bind` (default `127.0.0.1:8080`) |
| `[playlist]` | `default_duration_s` (> 0, at most 86400, default 10): how long a scene is shown when it has no `duration_s` |
| `[[scene]]` | `name`, `duration_s` (optional, same range), `condition` (optional, see below), `[[scene.zone]]` with `rect = { x, y, w, h }`, `widget`, optional `background = "#rrggbb"` |

Widgets:
- `clock`: `format` (strftime, default `%H:%M`), `font`, `color`, `align`
- `text`: `text`, `font`, `color`, `align` (`left`/`center`/`right`), `scroll_px_per_s` (scrolls right to left when set)
- `image`: a PNG or GIF scaled into the zone. Set exactly one of `path` (relative to the config file's directory; read and decoded when the config loads, so a bad file is a config error naming it) or `key` (a store key holding an image, filled by sources from M3). Options: `fit` (`contain` default, `cover`, `stretch`, `none`), `align` (`center` default, `top`, `bottom`, `left`, `right`, `top_left`, `top_right`, `bottom_left`, `bottom_right`), `smooth` (average pixels when shrinking, good for photos; default off, which keeps pixel art sharp), `loop` (GIF: default true, false stops on the last frame). Transparency blends over the zone's `background`. GIF frames follow the scene's elapsed time; frame delays under 20 ms are treated as 100 ms. Images are limited to 4096 px per side and 16 MiB per file.

- `value`: shows a number from the store (`Value::Number`).
  - `key` (required), `format` (default `{}`), `unit` (default none), `font`, `color` (default white), `align` (`center` default; aligns icon and text together), `icon` (optional), `thresholds`, `stale_after_s` (optional, > 0).
  - `format` has exactly one placeholder: `{}` (plain), `{:.N}` (N decimals), `{:>W.N}` / `{:<W.N}` (padded to width W, N optional). `{{` and `}}` are literal braces. Anything else is a config error. Examples: `"{:.1}°"`, `"{:.0} W"`, `"[{:>6.1}]"`.
  - `unit` is appended verbatim after the formatted number (include the space yourself: `unit = " W"`). The unit stored with the value is ignored.
  - `thresholds = [{ above = 25.0, color = "#ff4000" }, …]`: the value takes the colour of the highest `above` it is strictly greater than, else `color`. Any order, `above` values must be unique.
  - `icon`: one of `thermometer`, `droplet`, `bolt`, `sun`, `cloud`, `rain`, `home`, `wind` (12x12, drawn left of the text in the text colour).
  - Placeholder: `--` in dim grey (`#505050`, icon too) when the key is missing, the value is not a finite number (text, bool, series), or it is older than `stale_after_s` at the frame's time. Without `stale_after_s` a value never goes stale.
- `sparkline`: a line graph of a `Value::Series`, newest point at the right edge.
  - `key` (required), `color` (default white), `fill` (`true` = dim version of `color`, or a `"#rrggbb"` colour; default none), `min` / `max` (optional fixed scale; each end not given comes from the data), `points` (most recent points to show, at least 2; default the zone width).
  - Auto scale uses the shown points' range plus a 10% margin; flat data is drawn in the middle. Values outside a fixed scale are clamped to the edge. Non-finite points are skipped.
  - Fewer points than `points`: the graph grows in from the right edge. A single point is a dot. Missing key, wrong value type or empty series: nothing is drawn.

Fonts: `4x6`, `5x7`, `5x8`, `6x9`, `6x10` (default), `6x12`, `6x13`, `7x13`, `7x14`, `8x13`, `9x15`, `9x18`, `10x20`, plus `_bold` variants of most. All are ISO-8859-1, so `°` works.

### Playlist and conditions

`run` rotates through the scenes in config order, each for its `duration_s`, skipping scenes whose `condition` is false. Scene changes are logged at info level. `run --scene NAME` shows only that scene, ignoring its condition, so you can check a layout before its data exists. If no scene is eligible the panel goes black, and that is logged once per transition. A scene whose condition turns false while it is on screen is left immediately. If the render loop stalls, the playlist moves at most one scene per frame and never catches up on missed time. `render --scene NAME` is unchanged: one frame of one scene, conditions ignored.

A `condition` is a small expression over store keys:
- Comparison: `key op literal` with `>`, `>=`, `<`, `<=`, `==`, `!=`; literals are numbers, `true`/`false` and quoted strings (`"..."` or `'...'`).
- A bare `key` is truthy when it is `true`, a non-zero number or a non-empty string.
- `!`, `&&`, `||` (`&&` binds tighter) and parentheses.
- Keys are `source:name`, e.g. `ha:sensor.outside_temp`, `prom:house_power`, `weather:temp`.
- A missing key, or a comparison between mismatched types (text vs number, ...), is **false** for every operator, `!=` included. So `!(ha:x > 5)` is true while `ha:x` is missing.

Conditions are parsed when the config loads; errors name the scene and the expression.

## Development

CI (`.github/workflows/ci.yml`) runs all of the following on every push to `main` and every PR, plus an aarch64 check for the Pi, a macOS check and an MSRV (Rust 1.85) check.

```bash
cargo test --workspace
```

```bash
cargo clippy --workspace --all-targets -- -D warnings
```

```bash
cargo fmt --all
```

```bash
UPDATE_GOLDEN=1 cargo test -p paneld golden
```

Golden images live in `tests/fixtures/golden/` at 1:1 scale. Review any changes before committing. A failing golden test writes `<name>.actual.png` next to the golden for comparison.
