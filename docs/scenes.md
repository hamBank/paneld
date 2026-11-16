# Scenes and widgets

A **scene** is one screenful: a list of **zones**, each a rectangle of the canvas with one **widget** in it. Scenes take turns on the panel (the [playlist](#playlist)); [notifications](#notifications) interrupt them.

```toml
[[scene]]
name = "home"
duration_s = 20
condition = "ha:sun.sun == 'above_horizon'"   # optional

  [[scene.zone]]
  rect = { x = 0, y = 0, w = 172, h = 64 }
  widget = { type = "clock", format = "%H:%M", font = "10x20", font_scale = 3, color = "#ffaa00" }

  [[scene.zone]]
  rect = { x = 0, y = 64, w = 172, h = 22 }
  background = "#101030"
  widget = { type = "text", text = "Out {ha:sensor.outdoor_temperature:.1}°C", font = "9x15" }
```

The scene and zone keys are listed in the [configuration reference](configuration.md#scene). Any `font` key in a widget takes a [font name](#fonts).

## Layout

- **Coordinates** are canvas pixels, origin top-left, x to the right, y down. The canvas size is `[canvas] width × height`. On two stacked 172×86 panels, the top panel is `y` 0–85 and the bottom panel is `y` 86–171. How the canvas reaches the physical panels is the [output map](outputs.md#output-map).
- **A zone's `rect` must lie inside the canvas** and have a non-zero size.
- **Drawing order.** Each frame starts black. Zones are drawn in the order they appear in the file: first the zone's `background` (if set), then the widget. A later zone draws over an earlier one where they overlap, so overlapping zones work as layers.
- **Clipping.** A widget can't draw outside its zone. Text wider than its zone is cut off, unless it scrolls.
- **Transparency.** Without `background` a zone is transparent: whatever is under it (black, or an earlier zone) shows through around the widget. Transparent image pixels blend over the zone's background.
- **Vertical placement.** Text, clocks and values are centred vertically in their zone. Horizontal placement follows `align`.

## Widgets

`widget = { type = "...", ... }` picks the widget. Every widget table rejects unknown keys.

### `clock`

The current local time (the machine's time zone, `TZ` or `/etc/localtime`).

| Key | Type | Default | Valid | Meaning |
|---|---|---|---|---|
| `format` | string | `"%H:%M"` | a [chrono strftime](https://docs.rs/chrono/latest/chrono/format/strftime/index.html) pattern | e.g. `"%H:%M:%S"`, `"%a %d %b"`. Not a template. |
| `font` | string | `"6x10"` | font name | |
| `font_scale` | integer | `1` | 1–8 | Each font pixel becomes an N×N block. |
| `color` | colour | `"#ffffff"` | | |
| `align` | `"left"` \| `"center"` \| `"right"` | `"center"` | | |

### `text`

Literal text or a [template](#text-templates), static or scrolling.

| Key | Type | Default | Valid | Meaning |
|---|---|---|---|---|
| `text` | string | required | a valid template | The text. `{source:name}` placeholders are filled from the store; `{{` and `}}` are literal braces. |
| `font`, `font_scale`, `color` | | as for `clock` | | |
| `align` | `"left"` \| `"center"` \| `"right"` | `"center"` | | Ignored while scrolling. |
| `scroll_px_per_s` | float | none (static) | > 0 | Scroll right to left at this speed. |

A scrolling text starts just off the right edge, moves left until it has left the zone, and starts again. Its position follows the scene's elapsed time, so dropped frames don't slow it down, and it restarts each time the scene comes back.

### `value`

A number from the store, formatted, with an optional icon and threshold colours. Can also show a weather condition as an icon (see [`condition_icons`](#weather-condition-icons) below).

| Key | Type | Default | Valid | Meaning |
|---|---|---|---|---|
| `key` | store key | required | | e.g. `ha:sensor.outdoor_temperature`. |
| `format` | string | `"{}"` | a [number format](#number-formats) | e.g. `"{:.1}"`, `"{:>5.0}"`. |
| `unit` | string | none | | Appended verbatim after the number; put the space in yourself (`" W"`). The unit stored with the value is **not** shown. |
| `font`, `font_scale` | | as for `clock` | | |
| `color` | colour | `"#ffffff"` | | Colour below every threshold. |
| `align` | `"left"` \| `"center"` \| `"right"` | `"center"` | | Aligns icon and text together. |
| `icon` | string | none | a [built-in icon](#icons) | Drawn left of the text, in the text colour. |
| `thresholds` | `[{ above, color }, ...]` | `[]` | unique `above` values, any order | The value takes the colour of the highest `above` it is strictly greater than. |
| `stale_after_s` | float | none (never stale) | > 0 | Show the placeholder when the value is older than this. |
| `condition_icons` | table of text → icon name | none (off) | icon names | Turns on condition icons; `{}` uses the built-in map only. |
| `show_text` | bool | `true` | needs `condition_icons` | With a condition icon: `false` shows the icon alone. |

The **placeholder** `--`, in dim grey `#505050` (icon too), shows when:
- the key is missing;
- the value is not a finite number (and not a text with `condition_icons` on);
- the value is older than `stale_after_s`.

#### Weather condition icons

With `condition_icons` set, a **Text** value, such as a Home Assistant `weather.*` state, is shown with an icon. The icon is looked up in this order:
1. your `condition_icons` table;
2. the built-in map below;
3. `icon`.

The text is shown beside the icon unless `show_text = false`. A condition with no icon still shows its text.

```toml
widget = { type = "value", key = "ha:weather.home", condition_icons = {}, font = "9x15" }
widget = { type = "value", key = "ha:weather.home", condition_icons = { rainy = "droplet" }, show_text = false }
```

| Condition | Icon | Condition | Icon |
|---|---|---|---|
| `sunny` | `sun` | `clear-night` | `moon` |
| `cloudy`, `partlycloudy` | `cloud` | `fog` | `fog` |
| `rainy`, `pouring` | `rain` | `snowy`, `snowy-rainy`, `hail` | `snow` |
| `lightning`, `lightning-rainy` | `bolt` | `windy`, `windy-variant` | `wind` |
| `exceptional` | `thermometer` | | |

These are the 15 conditions in Home Assistant's weather entity docs.

### `sparkline`

A line graph of a **Series** value (`ha:<id>@series`, a Prometheus `range_queries` key), newest point at the right edge.

| Key | Type | Default | Valid | Meaning |
|---|---|---|---|---|
| `key` | store key | required | | A Series. |
| `color` | colour | `"#ffffff"` | | Line colour. |
| `fill` | `true` \| `false` \| colour | none | | `true` fills under the line with a dim `color`; a colour fills with that colour. |
| `min`, `max` | float | from the data | `min < max` when both are set | Fixed ends of the scale. |
| `points` | integer | the zone width (one point per column) | ≥ 2 | How many of the newest points to show. |

- **Auto scale.** Without `min`/`max` the scale is the shown points' range plus a 10 % margin. Flat data is drawn in the middle. Values outside a fixed scale are clamped to the edge, and non-finite points are skipped.
- **Few points.** With fewer points than `points`, the graph grows in from the right. A single point is a dot.
- **Nothing to draw.** If the key is missing, holds the wrong type or holds an empty series, nothing is drawn.

### `image`

A PNG or GIF (animated GIFs play), scaled into the zone.

| Key | Type | Default | Valid | Meaning |
|---|---|---|---|---|
| `path` | path | | exactly one of `path`, `key` | Image file relative to the config file. Read and decoded when the config loads. At most 4096 px per side and 16 MiB. |
| `key` | store key | | | A store key holding an image, written by a [`media` source](sources.md#media), e.g. `photos:current`. Shows nothing while the key is missing. Animated GIFs play. |
| `fit` | `"contain"` \| `"cover"` \| `"stretch"` \| `"none"` | `"contain"` | | `contain`: fit entirely, keep the aspect ratio. `cover`: fill the zone, keep the aspect ratio, crop. `stretch`: fill the zone exactly. `none`: native size, cropped. |
| `align` | `"center"` `"top"` `"bottom"` `"left"` `"right"` `"top_left"` `"top_right"` `"bottom_left"` `"bottom_right"` | `"center"` | | Where the image sits when it doesn't fill the zone. |
| `smooth` | bool | `false` | | Average pixels when shrinking (photos). Off keeps pixel art sharp. |
| `loop` | bool | `true` | | GIFs: restart after the last frame, or stop on it. |

- **GIF timing.** GIF frames follow the scene's elapsed time. Frame delays under 20 ms count as 100 ms, as in browsers.
- **Reloads.** Image files aren't watched for changes: after replacing one, edit the config or send `SIGHUP`.

### `video`

A video file or a live stream (an RTSP camera, an HTTP stream), decoded by an `ffmpeg` subprocess. Needs `ffmpeg` installed (`sudo apt install ffmpeg`).

| Key | Type | Default | Valid | Meaning |
|---|---|---|---|---|
| `path` | path | | exactly one of `path`, `url_env` | Video file relative to the config file; must exist when the config loads. |
| `url_env` | string | | env var name | **Name** of the variable holding the stream URL. A literal `url = "..."` is rejected, because stream URLs usually carry a password. |
| `fit` | `"contain"` \| `"cover"` \| `"stretch"` \| `"none"` | `"contain"` | | As for `image`, done by `ffmpeg`. |
| `fps` | integer | `15` | 1–60 | Frames per second delivered. The video's own rate is capped to this, never raised. |
| `loop` | bool | `true` | | Files: restart at the end, or hold the last frame. |
| `program` | path | `ffmpeg` from `PATH` | | The `ffmpeg` executable. |

```toml
widget = { type = "video", path = "assets/clip.mp4", fps = 15 }
widget = { type = "video", url_env = "CAM_URL", fit = "cover", fps = 10 }   # CAM_URL=rtsp://user:pass@cam/stream in /etc/paneld/env
```

**When `ffmpeg` runs.**
- It runs only while the zone's scene is on screen, and stops 5 s after the scene leaves. A playlist scene with a video costs nothing while other scenes show.
- Decoding is off the render thread. Until the first frame arrives (starting up, stream down, `ffmpeg` missing), the zone is black.
- When a stream drops, the last frame stays up to 10 s, then the zone goes black. `ffmpeg` is restarted with backoff, 1 s doubling to 30 s.
- The URL is never logged.

**Startup checks.**
- `paneld run` refuses to start, and a reload is refused, if `url_env` is unset or empty.
- `paneld check` only warns about that (it may run in another shell), and warns when `ffmpeg` is not found.

**CPU on a Pi.** `ffmpeg` decodes the whole stream before scaling, so the cost follows the **source** resolution and bitrate, not the zone size.
- Use a camera's low-resolution sub-stream (e.g. 640×360) and `fps` 5–15.
- Cameras without RTSP can be re-streamed with [go2rtc](https://github.com/AlexxIT/go2rtc).
- Under the systemd unit, keep video files outside `/home` (the unit hides it), e.g. in `/etc/paneld/assets`.

## Fonts

`font` takes a built-in name or the `name` of a `[[font]]`.

- **Built-in** (ISO-8859-1, so `°` works): `4x6`, `5x7`, `5x8`, `6x9`, `6x10` (default), `6x12`, `6x13`, `6x13_bold`, `7x13`, `7x13_bold`, `7x14`, `7x14_bold`, `8x13`, `8x13_bold`, `9x15`, `9x15_bold`, `9x18`, `9x18_bold`, `10x20`.
- **BDF files**:
  - **Declaring:** `[[font]] name = "big"` with `path = "../assets/fonts/spleen-32x64.bdf"`. Names must be unique and must not clash with a built-in. An unknown `font` is a config error that lists every known name.
  - **Included:** `assets/fonts/` ships Spleen 12x24, 16x32 and 32x64 (BSD-2-Clause, see `assets/fonts/README.md`).
  - **Supported BDF:** `FONTBOUNDINGBOX`, `FONT_ASCENT`/`FONT_DESCENT`, `ENCODING` (read as a Unicode code point), `DWIDTH`, `BBX` and `BITMAP`, so proportional fonts work.
- **`font_scale`** (1–8) draws each font pixel as an N×N block: `font = "10x20", font_scale = 3` is a 30×60 font.
- **Missing characters** are drawn as `?` if the font has one, else left blank.
- **Layout** is the same for every font. The line box (cell height, or ascent + descent, times the scale) is centred vertically. Alignment uses the measured width.

## Icons

Built-in 12×12 icons for `value` (`icon`, `condition_icons`) and inline notifications:

`thermometer`, `droplet`, `bolt`, `sun`, `cloud`, `rain`, `home`, `wind`, `snow`, `moon`, `fog`.

## Text templates

A `text` widget's `text` can hold placeholders filled from the store:

```toml
widget = { type = "text", text = "{ha:sensor.outdoor_temperature:.1}°C  {aircraft:nearest.callsign} {aircraft:nearest.route}" }
```

- **Placeholders** are `{key}` or `{key:spec}`, with no whitespace inside the braces. The split is positional:
  - the first `:` ends the source prefix;
  - the name runs to the second `:`;
  - anything after the second `:` is the spec.

  So `{ha:sensor.x:.1}` is the key `ha:sensor.x` with the spec `.1`. A key needs its prefix: `{x}` is an error.
- **Spec:** the [number format](#number-formats) without the braces and colon: `.1`, `>6.0`, `<8`.
- **Rendering by value kind:**

  | Value | Shown as |
  |---|---|
  | Number | Formatted with the spec. |
  | Text | As is. Width pads it; `.N` cuts it to N characters. |
  | Bool | `on` / `off`. |
  | missing key, non-finite number, Series, image, JSON | `--`, padded to the width. |

- **Errors:** a template error is a config error naming the scene, the zone and the column.
- **Cost:** templates are parsed once and re-rendered only when a value they show changes. Alignment and scrolling follow the rendered width.
- **Subscriptions:** every key in a template counts as used, so Home Assistant subscribes to it.

## Number formats

`value` `format` and template specs use one placeholder:

| Placeholder | Meaning |
|---|---|
| `{}` | The number as Rust prints an `f64`: `21`, `21.5`. |
| `{:.N}` | N decimals: `{:.1}` gives `21.5`. |
| `{:>W}` `{:<W}` `{:W}` | Padded to width W, right-aligned unless `<`. |
| `{:>W.N}` `{:<W.N}` | Both. |

`{{` and `}}` are literal braces; text around the placeholder is kept (`"[{:>6.1}]"`). Anything else is a config error.

## Conditions

A scene's `condition` is a boolean expression over store keys. The scene is only shown while it is true.

```text
ha:binary_sensor.door == 'on'
prom:house_power > 0 && !ha:binary_sensor.away
(aircraft:count >= 1 || push:alert) && ha:sun.sun != "below_horizon"
```

- **Comparisons:** `key op literal`, with `>`, `>=`, `<`, `<=`, `==`, `!=`. Literals are numbers, `true`/`false`, and quoted strings (`'...'` or `"..."`, with `\\`, `\'`, `\"` escapes).
- **Bare key:** a bare `key` is true when it is `true`, a non-zero number or a non-empty text.
- **Operators:** `!`, `&&` and `||`, where `&&` binds tighter than `||`, plus parentheses.
- **Missing keys and type mismatches.** A missing key, or a comparison between different kinds (text against a number, …), is **false** for every operator, `!=` included. So `!(ha:x > 5)` is true while `ha:x` is missing; write `ha:x && !(ha:x > 5)` if that is not wanted.
- **Comparison rules by kind:**
  - Numbers compare numerically, and anything involving NaN is false.
  - Text compares byte-wise.
  - Bools support only `==` and `!=`.

Conditions are parsed when the config loads, and errors name the scene. They are re-checked every frame.

## Playlist

- **Rotation.** Scenes rotate in file order, each for its `duration_s` (default `[playlist] default_duration_s`, 10 s), skipping scenes whose condition is false.
- **Condition turns false.** A scene whose condition turns false while it is on screen is left at once.
- **Only one eligible scene.** It just stays on screen.
- **Nothing eligible.** The panel shows black, logged once per change. When a scene becomes eligible again, rotation continues after the last scene shown.
- **Stalls.** If the render loop stalls, the playlist moves at most one scene per frame and never catches up on missed time.
- **Restarts.** Widget state (scroll position, GIF frame) restarts each time a scene comes on screen.
- **Pinning.** A scene can be pinned from the [HTTP API](control.md#post-apiscenename) or [Home Assistant](control.md#home-assistant-over-mqtt). A pinned scene stays on screen whatever its condition, until it is released or a notification interrupts it.
- **`paneld run --scene NAME`** shows only that scene, ignoring its condition, for checking a layout before its data exists.

## Notifications

A notification is a short alert shown over everything else, raised from the [HTTP API](control.md#post-apinotify) or [MQTT](control.md#home-assistant-over-mqtt). It has a priority, a duration per showing, a repeat count, and either a configured `scene` or inline `text` (with optional `icon` and `color`).

**Order and interruption:**
- The highest priority shows first; equal priorities go in arrival order.
- A **strictly** higher priority interrupts the one on screen. The interrupted one goes back to the queue and later continues with the time it had left.
- `repeat = N` shows it N times back to back.

**Effect on the playlist:**
- While a notification shows, the playlist is frozen. When the queue empties, the playlist resumes on the same scene with the same time left.
- A pinned scene is interrupted too, and comes back afterwards.

**Queue limits and lifetime:**
- Clearing empties the queue and ends the one showing.
- The queue holds 50. When it is full, the lowest-priority waiting notification is dropped with a warning.
- A config reload keeps the queue but drops scene notifications whose scene is gone.

Inline notifications are drawn full-screen:
- The icon and text are centred in the given colour over `[notifications] background`.
- The font is the largest that fits; see [`[notifications]`](configuration.md#notifications).
- Text too wide even then scrolls past the icon at about 3 characters per second, in the largest font no taller than a sixth of the canvas height.
- Braces in notification text are shown as typed; it is not a template.
