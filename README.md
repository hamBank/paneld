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
| `[[font]]` | `name`, `path`: a BDF font file (path relative to the config file) usable as `font = "name"`; see Fonts below. Repeatable. |
| `[[scene]]` | `name`, `duration_s` (optional, same range), `condition` (optional, see below), `[[scene.zone]]` with `rect = { x, y, w, h }`, `widget`, optional `background = "#rrggbb"` |

Widgets:
- `clock`: `format` (strftime, default `%H:%M`), `font`, `font_scale`, `color`, `align`
- `text`: `text`, `font`, `font_scale`, `color`, `align` (`left`/`center`/`right`), `scroll_px_per_s` (scrolls right to left when set). `text` may be a template, see Text templates below.
- `image`: a PNG or GIF scaled into the zone. Set exactly one of `path` (relative to the config file's directory; read and decoded when the config loads, so a bad file is a config error naming it) or `key` (a store key holding an image, filled by sources from M3). Options: `fit` (`contain` default, `cover`, `stretch`, `none`), `align` (`center` default, `top`, `bottom`, `left`, `right`, `top_left`, `top_right`, `bottom_left`, `bottom_right`), `smooth` (average pixels when shrinking, good for photos; default off, which keeps pixel art sharp), `loop` (GIF: default true, false stops on the last frame). Transparency blends over the zone's `background`. GIF frames follow the scene's elapsed time; frame delays under 20 ms are treated as 100 ms. Images are limited to 4096 px per side and 16 MiB per file.

- `value`: shows a number from the store (`Value::Number`).
  - `key` (required), `format` (default `{}`), `unit` (default none), `font`, `font_scale`, `color` (default white), `align` (`center` default; aligns icon and text together), `icon` (optional), `thresholds`, `stale_after_s` (optional, > 0).
  - `format` has exactly one placeholder: `{}` (plain), `{:.N}` (N decimals), `{:>W.N}` / `{:<W.N}` (padded to width W, N optional). `{{` and `}}` are literal braces. Anything else is a config error. Examples: `"{:.1}°"`, `"{:.0} W"`, `"[{:>6.1}]"`.
  - `unit` is appended verbatim after the formatted number (include the space yourself: `unit = " W"`). The unit stored with the value is ignored.
  - `thresholds = [{ above = 25.0, color = "#ff4000" }, …]`: the value takes the colour of the highest `above` it is strictly greater than, else `color`. Any order, `above` values must be unique.
  - `icon`: one of `thermometer`, `droplet`, `bolt`, `sun`, `cloud`, `rain`, `home`, `wind`, `snow`, `moon`, `fog` (12x12, drawn left of the text in the text colour).
  - `condition_icons` (optional table) and `show_text` (default true): weather condition icons, see below.
  - Placeholder: `--` in dim grey (`#505050`, icon too) when the key is missing, the value is not a finite number (text, bool, series), or it is older than `stale_after_s` at the frame's time. Without `stale_after_s` a value never goes stale.
- `sparkline`: a line graph of a `Value::Series`, newest point at the right edge.
  - `key` (required), `color` (default white), `fill` (`true` = dim version of `color`, or a `"#rrggbb"` colour; default none), `min` / `max` (optional fixed scale; each end not given comes from the data), `points` (most recent points to show, at least 2; default the zone width).
  - Auto scale uses the shown points' range plus a 10% margin; flat data is drawn in the middle. Values outside a fixed scale are clamped to the edge. Non-finite points are skipped.
  - Fewer points than `points`: the graph grows in from the right edge. A single point is a dot. Missing key, wrong value type or empty series: nothing is drawn.

### Fonts

`font` takes a built-in name or the name of a `[[font]]`:
- Built-in: `4x6`, `5x7`, `5x8`, `6x9`, `6x10` (default), `6x12`, `6x13`, `7x13`, `7x14`, `8x13`, `9x15`, `9x18`, `10x20`, plus `_bold` variants of most. All are ISO-8859-1, so `°` works.
- BDF fonts: declare each with `[[font]] name = "big", path = "../assets/fonts/spleen-32x64.bdf"` and use `font = "big"`. The file is read and parsed when the config loads (never on the render thread), so a missing or malformed file is a config error naming the file and, for a parse error, the line. Names must be unique and must not be a built-in name; an unknown `font` lists every known name. `assets/fonts/` ships Spleen 12x24, 16x32 and 32x64 (see `assets/fonts/README.md`).
- `font_scale` (1–8, default 1) on `clock`, `text` and `value` draws every font pixel as an N×N block, for big crisp digits from any font: `font = "10x20", font_scale = 3` is a 30x60 font.
- BDF support covers the common subset (`FONTBOUNDINGBOX`, `FONT_ASCENT`/`FONT_DESCENT`, `ENCODING`, `DWIDTH`, `BBX`, `BITMAP`), including proportional widths. `ENCODING` is read as a Unicode code point. A character the font lacks is drawn as `?` if the font has one, otherwise it leaves a blank of the font's widest-cell width. Built-in fonts also show `?`.
- Layout is the same for every font: the line box (`ascent + descent` for BDF, the cell height for built-ins, times `font_scale`) is centred vertically in the zone, and alignment uses the measured text width.

### Text templates

A `text` widget's `text` is literal text with placeholders filled from the store, e.g. `"{ha:sensor.outdoor_temperature:.1}°"` or `"{aircraft:nearest.callsign} {aircraft:nearest.route}"`.

- `{key}` or `{key:spec}`. `key` is `source:name` (`ha:sensor.outdoor_temperature`); the name may contain letters, digits, `_`, `.`, `-` and `@`. `:` also starts the spec, so the split is positional: the first `:` ends the source prefix, the name runs to the second `:`, and everything after the second `:` is the spec. A key needs its source prefix; no whitespace inside the braces.
- `spec` is the number format used by `value`: `.N` decimals, `>W` / `<W` / `W` width (right-aligned unless `<`), e.g. `{ha:sensor.power:>6.0}`. Default `{}`.
- Numbers use the spec. Text is inserted as is (width pads it, `.N` truncates it to N characters). Booleans are `on` / `off`. A missing key, a non-finite number or a series, image or JSON value renders as `--` (padded to the width).
- `{{` and `}}` are literal braces. Text without any braces is plain text. A template error is a config error naming the scene, the zone and the column.
- Alignment and scrolling use the rendered width, so they follow the values as they change. The template is parsed once, and only re-rendered on frames where a value it shows has changed.
- Every key in a template is subscribed by the sources like any other referenced key. `clock` formats are strftime and take no templates.

### Weather condition icons

Give a `value` widget a `condition_icons` table and it shows a `Text` value (such as a Home Assistant `weather.*` entity's state) with an icon chosen from it:

```toml
widget = { type = "value", key = "ha:weather.home", condition_icons = {}, font = "9x15" }
widget = { type = "value", key = "ha:weather.home", condition_icons = { rainy = "droplet" }, show_text = false, color = "#ffd000" }
```

The condition is looked up in `condition_icons` first (condition text to icon name), then in the built-in map of Home Assistant's documented conditions, so `condition_icons = {}` means "the built-in map only":

| condition | icon | condition | icon |
|---|---|---|---|
| `sunny` | `sun` | `clear-night` | `moon` |
| `cloudy`, `partlycloudy` | `cloud` | `fog` | `fog` |
| `rainy`, `pouring` | `rain` | `snowy`, `snowy-rainy`, `hail` | `snow` |
| `lightning`, `lightning-rainy` | `bolt` | `windy`, `windy-variant` | `wind` |
| `exceptional` | `thermometer` | | |

The text itself is shown beside the icon; `show_text = false` shows only the icon (a condition with no icon still shows its text). `icon` is used for texts that are in neither map. Without `condition_icons` a `Text` value shows `--`, as before. `stale_after_s` applies as usual. `show_text = false` without `condition_icons` is a config error.

### Playlist and conditions

`run` rotates through the scenes in config order, each for its `duration_s`, skipping scenes whose `condition` is false. Scene changes are logged at info level. `run --scene NAME` shows only that scene, ignoring its condition, so you can check a layout before its data exists. If no scene is eligible the panel goes black, and that is logged once per transition. A scene whose condition turns false while it is on screen is left immediately. If the render loop stalls, the playlist moves at most one scene per frame and never catches up on missed time. `render --scene NAME` is unchanged: one frame of one scene, conditions ignored.

A `condition` is a small expression over store keys:
- Comparison: `key op literal` with `>`, `>=`, `<`, `<=`, `==`, `!=`; literals are numbers, `true`/`false` and quoted strings (`"..."` or `'...'`).
- A bare `key` is truthy when it is `true`, a non-zero number or a non-empty string.
- `!`, `&&`, `||` (`&&` binds tighter) and parentheses.
- Keys are `source:name`, e.g. `ha:sensor.outside_temp`, `prom:house_power`, `weather:temp`.
- A missing key, or a comparison between mismatched types (text vs number, ...), is **false** for every operator, `!=` included. So `!(ha:x > 5)` is true while `ha:x` is missing.

Conditions are parsed when the config loads; errors name the scene and the expression.

### Data sources

Sources (`[sources.<name>]`, each with a `type`) write store keys that widgets and conditions read. They run in the background and are restarted with backoff if they fail. The render loop never waits on them.

| `type` | What it does |
|---|---|
| `static` | Writes fixed values once, e.g. `values = { "demo:temp" = 21.5, "demo:on" = true }`. Useful for laying out scenes. |
| `aircraft` | Aircraft near a point, from adsb.fi (or any ADS-B Exchange v2 compatible URL, e.g. a local readsb), with routes from adsbdb. Writes `<name>:count`, `<name>:nearest.*` (`callsign`, `registration`, `type`, `desc`, `route`, `origin`, `destination`, `bearing_compass`, `alt_ft`, `speed_kt`, `dist_nm`, `bearing`) and `<name>:list` (JSON), where `<name>` is the source's name: `[sources.aircraft]` gives `aircraft:*`. Fields: `location = { lat, lon }` (required); `url` (template with `{lat}` `{lon}` `{radius_nm}`, default adsb.fi); `location_decimals` (0-6, default 2, about 1 km: the location is rounded to this before it is sent); `radius_nm` (1-250, default 10); `max_altitude_ft` (optional); `include_ground` (default false); `interval_s` (at least 1, default 15); `routes` (default true); `list_len` (1-20, default 5); `route_separator` (default `-`, giving `SYD-AKL`; the built-in fonts have no `→`, so set it only if your font does). Nothing in range removes `nearest.*`; a failed poll leaves the old values. |
| `http_poll` | Fetches a JSON URL and writes values picked out by JSON paths. Fields: `url` (`http(s)://`, no `user:pass@`; an API key in the query string is fine and is never logged); `interval_s` (at least 1, default 60); `extract` (required; store key to a path like `$.current.temp`, `$.list[0].main.t` or `$["odd.key"]`, or `{ path = "...", unit = "C" }`); `headers` (non-secret request headers); `header_env` (header name to the **name** of an environment variable holding its value, for `Authorization` or API keys; the paneld run fails at start if one is unset); `timeout_s` (1-120, default 10). Numbers and numeric strings become numbers, booleans and strings stay, arrays and objects become JSON. All keys are written in one batch per poll. A path that is missing or `null` in a successful response **removes** its key; a failed poll (error, non-2xx, 429, not JSON) keeps the old values. 429 honours `Retry-After`; errors back off exponentially up to 5 minutes; redirects are followed only within the same host. |
| `prometheus` | PromQL against a Prometheus server (`/api/v1/query`, `/api/v1/query_range`). Fields: `url` (server base, e.g. `http://prometheus.local:9090`); `interval_s` (at least 1, default 15); `queries` (store key to an instant query, or `{ query = "...", unit = "W" }`); `range_queries` (store key to `{ query, range_s, step_s }`, written as a `Series` of the last `range_s` seconds for sparklines; at most 11000 points); `bearer_env` (name of an environment variable holding a bearer token); `timeout_s` (1-120, default 10). An instant query must give one number: a scalar, or a vector with exactly one series. An empty vector or `NaN`/`Inf` removes the key; a vector with several series writes nothing for that key, logs a warning naming the key and the series count, and counts a source error (aggregate with `sum()` or `max()`). A range query uses the first series. Up to 4 queries run at once and all results go in one batch. Backoff and `Retry-After` as for `http_poll`. |
| `home_assistant` | Entity states, weather forecasts and rolling series over the HA WebSocket API. See below. |
| `mqtt` | Subscribes to MQTT topics and writes their payloads to keys, with optional JSON path extraction. See below. |

Keep personal settings (location, entity IDs) in a git-ignored `config/local.toml`.

#### Home Assistant setup

1. In Home Assistant, open your profile, then Security, and create a **long-lived access token**.
2. Put it in an environment variable, for example `export HA_TOKEN=...` or an `EnvironmentFile=` in the systemd unit. The config only names the variable (`token_env = "HA_TOKEN"`); the token is never read from the config, logged or shown. `paneld run` refuses to start if the variable is not set.
3. Add the source:

```toml
[sources.ha]
type = "home_assistant"
url = "ws://homeassistant.local:8123/api/websocket"   # ws:// or wss://
token_env = "HA_TOKEN"
series = ["sensor.outdoor_temperature"]   # optional: also keep ha:<id>@series
forecasts = ["weather.home"]              # optional: weather.get_forecasts
```

Only the entities your scenes use are subscribed (every `ha:<entity_id>…` key in a widget or condition), plus `entities`, `series` and `forecasts`. Optional settings: `entities` (extra entity IDs), `series_points` (288), `forecast_type` (`daily` or `hourly`), `forecast_interval_s` (1800), `forecast_len` (5).

Keys: `ha:<entity_id>` is a number (with the entity's `unit_of_measurement`) or text; `unavailable`/`unknown` remove it. `ha:<entity_id>.<attribute>` holds an attribute (number, text, bool, or JSON for lists and objects). `ha:<entity_id>@series` is a rolling series for sparklines. `ha:<entity_id>.forecast.<n>.<field>` (`n = 0` soonest) and `ha:<entity_id>.forecast` (JSON) hold forecasts. If the connection drops, the last values stay and show as stale; the source reconnects with backoff. Changing the source config or the entities your scenes use restarts just this source on reload.

#### MQTT setup

Subscribe to topics on any MQTT broker (Mosquitto, Home Assistant's add-on, ...). The connection is plain TCP (MQTT over TLS is a follow-up), which is what a broker on a home LAN normally offers.

```toml
[sources.mqtt]
type = "mqtt"
host = "homeassistant.local"
# port = 1883                      # default
# username = "paneld"
# password_env = "MQTT_PASSWORD"   # NAME of the env var holding the password (needs username)

[sources.mqtt.topics]              # store key = topic, or a table
"home:solar" = "home/power/solar"
"home:grid" = { topic = "home/meter", path = "$.power.grid_w", unit = "W" }
```

Each entry maps one store key to one exact topic. Keys look like `source:name` (quote them in TOML) and are used as written. Two keys may share a topic with different paths. Wildcards (`+`, `#`) are not allowed: a wildcard would need a key per matching topic, and keeping one key per entry means the keys are known when the config loads. Other fields: `client_id` (default `paneld-<source name>`; it must be unique on the broker) and `keep_alive_s` (default 30).

Payloads:
- With `path` (the same `$.a.b[0]` syntax as other sources), the payload must be JSON; the value at the path is stored. A JSON `null` removes the key.
- Without `path`, JSON is tried first (`21.5` is a number, `true` a bool, an object or array is JSON); otherwise the payload is its UTF-8 text, so `on` is text. Text that looks like a number is a number. `unit` is attached to numbers.
- Retained messages count as values. An empty payload (a deleted retained message) removes the keys of that topic.
- A payload that cannot be used (not JSON when `path` is set, path not found, not UTF-8) is logged once per topic, counted as an error and leaves the key unchanged.

The password is read from the environment variable named by `password_env` (`paneld run` refuses to start if it is not set) and is never logged. Subscriptions use QoS 0. If the broker drops, the last values stay and show as stale; the source reconnects with backoff (1 s to 30 s) and subscribes again. Changing the source config takes a restart.

### Live reload

`paneld run` picks up edits to the config file without a restart. The file is polled once a second; a change is applied once the file has had the same new content on two consecutive polls (so a half-written save is not parsed, and touching the file without changing it does nothing), which means a reload takes about two seconds. On unix, `kill -HUP <pid>` reloads immediately. Editors that save in place or by rename-and-replace both work. The new config is fully parsed, validated and prepared (including decoding images) off the render thread, then swapped in between two frames; the playlist restarts at its first eligible scene and widget state (scroll position, GIF frame) starts over.

- **Applied live:** scenes, zones, widgets, conditions, the playlist and `duration_s`, and `[sources]`. Only the sources whose config changed are restarted; the others keep running. Adding, removing or editing a `[sources.<name>]` table acts on that source alone, and so does a scene edit that makes it read different entities (for Home Assistant: a scene that starts showing a new `ha:` entity restarts the Home Assistant source). A removed source's values are cleared. A restarted source's values stay on screen until it writes again; keys it no longer produces are then removed. If a changed or added source needs an environment variable that is not set (a token), it is not started, the old one keeps running, and the reload counts an error; the scene changes still apply.
- **Needs a restart:** `canvas.fps`, `[http]`, and every `[[output]]` (the preview and the Colorlight settings gamma, brightness, white point, colour order, sync gap and interface). If these differ from the running values, a warning names them and the scene and playlist changes are still applied.
- **Rejected:** a changed `canvas` width or height, since scenes laid out for a new size would not fit the running canvas. Restart to resize.
- **Errors never take the panel down.** A parse or validation error, an unreadable image, a canvas size change, or (with `run --scene X`) a config that no longer has scene `X` is logged with the full message, counted in `reload_errors`, and the old config keeps running. Fix the file and the next change is picked up. With `--scene X`, scene `X` stays on screen across reloads.
- **Limitations:** image files referenced by the config are not watched; after replacing one, make any edit to the config or send SIGHUP.

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

## Credits

Aircraft data: [adsb.fi](https://adsb.fi) (the `aircraft` source's default provider; personal, non-commercial use, see its terms). Routes: [adsbdb](https://www.adsbdb.com). The preview page shows the adsb.fi credit whenever an aircraft source is configured.
