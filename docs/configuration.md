# Configuration reference

paneld reads one TOML file, given with `--config`. This page lists every section and key the parser accepts. A working example with comments is [`config/example.toml`](../config/example.toml).

Related pages:

- [Sources](sources.md): every `[sources.<name>]` type and the keys it writes.
- [Scenes and widgets](scenes.md): zones, widgets, fonts, templates, conditions, the playlist and notifications.
- [Outputs](outputs.md): the preview, the Colorlight card, the output map and brightness.
- [Control interfaces](control.md): the HTTP API, MQTT and Home Assistant, metrics, signals and the CLI.

## General rules

- **Unknown keys are errors.** A typo is reported instead of being silently ignored.
- **All problems at once.** Validation collects every error and prints them together, for example `scene "home" zone[2]: rect { x = 0, y = 160, w = 172, h = 22 } does not fit the 172x172 canvas`. `paneld check --config FILE` runs the same checks without starting anything.
- **Relative paths** (`[[font]] path`, image `path`, video `path`, media `dir`) are resolved against the directory of the config file.
- **Files are read when the config loads**, never on the render thread. A missing or broken font or image is a config error naming the file.
- **Secrets never go in the file.** Every token or password is given as the **name** of an environment variable (`token_env`, `password_env`, `bearer_env`, `header_env`, `url_env`). `paneld run` refuses to start if one of those variables is unset or empty, naming the variable. The values are never logged. A variable name must be letters, digits and `_`.
- **Keep personal settings** (your location, entity IDs, API URLs) in a git-ignored `config/local.toml`.

### Common value types

| Type | Written as | Notes |
|---|---|---|
| colour | `"#rrggbb"` | The `#` is required; hex digits in either case. |
| rect | `{ x = 0, y = 0, w = 172, h = 86 }` | Canvas pixels, origin top-left. `w` and `h` must be > 0. |
| store key | `"source:name"` | A prefix, `:`, then a name. The prefix starts with a letter or `_` and holds letters, digits, `_` and `-`. The name holds letters, digits, `_`, `-`, `.` and `@`. Quote keys in TOML tables: `"ha:sensor.x" = ...`. |
| duration | seconds as a number | `duration_s`, `interval_s` and so on. Fractions are allowed unless noted. |
| time of day | `"HH:MM"` | 24 h, local time of the machine (`TZ` / `/etc/localtime`). |

## What reloads live

`paneld run` watches the config file (see [Control interfaces: live reload](control.md#live-reload)).

| Section | On reload |
|---|---|
| `[[scene]]`, `[playlist]`, `[[font]]`, `[notifications]` | applied live |
| `[sources.*]` | applied live, per source: only sources whose config changed are restarted |
| `canvas.width`, `canvas.height` | a change rejects the whole reload; restart to resize |
| `[[output]]` settings: preview `scale` and `grid`; Colorlight `gamma`, `brightness`, `white_point`, `brightness_mode`, `color_order`, `firmware_major`, `sync_gap_us`, `card_size` and `[[output.region]]` | applied live, from the next frame. A Colorlight output is matched to the running one by `interface`. |
| adding or removing an `[[output]]`, or changing an output's `interface` | needs a restart; a warning is logged, and the settings of the outputs that still match are applied |
| `canvas.fps`, `[http]`, `[mqtt]`, `[quiet_hours]`, `[auto_dim]` | need a restart; a difference logs a warning, and the rest of the reload still applies |

## `[canvas]` (required)

The logical picture the scenes are drawn on.

| Key | Type | Default | Valid | Meaning |
|---|---|---|---|---|
| `width` | integer | required | > 0 | Canvas width in pixels. |
| `height` | integer | required | > 0 | Canvas height in pixels. |
| `fps` | integer | `30` | 1–60 | Frames rendered (and sent) per second. |

## `[[output]]`

Where frames go. Repeatable, selected by `type`. With no outputs the daemon still renders (useful for the API and metrics). Details: [Outputs](outputs.md).

### `type = "preview"`

At most one. Served on the `[http] bind` address at `/preview` and `/preview.png`.

| Key | Type | Default | Valid | Meaning |
|---|---|---|---|---|
| `scale` | integer | `8` | 1–32 | Each canvas pixel becomes a `scale × scale` block. |
| `grid` | bool | `false` | | Dark gaps between pixels, to look more like a panel. |

### `type = "colorlight"`

A Colorlight 5A-75B receiver card on a network interface. Linux only; needs `CAP_NET_RAW`.

| Key | Type | Default | Valid | Meaning |
|---|---|---|---|---|
| `interface` | string | required | not empty, one output per interface | NIC the card is plugged into, e.g. `eth1`. |
| `gamma` | float | `2.2` | 1.0–4.0 | Exponent of the colour lookup table. |
| `brightness` | integer | `255` | 0–255 | This output's calibrated maximum; user brightness and auto-dim scale it down. |
| `white_point` | colour | `"#ffffff"` | | What full white becomes; scales R, G and B to correct the panels' colour temperature. |
| `brightness_mode` | `"hardware"` \| `"software"` | `"hardware"` | | Where brightness is applied: in the card's display frame, or in the colour LUT (the card is then told 255). |
| `color_order` | `"rgb"` `"rbg"` `"grb"` `"gbr"` `"brg"` `"bgr"` | `"rgb"` | | What the three colour bytes on the wire carry, in order. Depends on the panel; find it with `paneld test-pattern --pattern bars`. |
| `firmware_major` | integer | none | 0–255 | Card firmware major version. When set it overrides what discovery reports (a mismatch logs a warning). It decides single or double sync: firmware 13 and later need two packets. |
| `sync_gap_us` | integer | `500` | 0–20000 | Pause in µs between the last row packet and the display frame. |
| `card_size` | `{ w, h }` | bounding box of the regions | holds every region | The card's whole display space. Card pixels no region covers are sent black. A key of the output, so it goes before the first `[[output.region]]`. |
| `[[output.region]]` | tables | one region: the whole canvas at card (0,0) | see below | Map canvas rectangles into card space. |

`[[output.region]]` keys:

| Key | Type | Default | Valid | Meaning |
|---|---|---|---|---|
| `canvas` | rect | required | inside the canvas | The canvas rectangle to show. |
| `card_offset` | `{ x, y }` | `{ x = 0, y = 0 }` | | Where the (flipped, rotated) region's top-left corner lands in card space. |
| `rotate` | integer | `0` | `0`, `90`, `180`, `270` | Clockwise rotation. 90 and 270 make the footprint `h × w`. |
| `flip` | `"none"` \| `"horizontal"` \| `"vertical"` | `"none"` | | Mirroring, applied **before** the rotation. |

Regions may overlap on the canvas (show one area twice) but not in card space. The card space is limited to 4 194 304 pixels.

## `[http]`

The HTTP server for the control API, the web app, the preview and `/metrics`. Restart-only.

When `bind` is a loopback address, every route also requires a `Host` header of `localhost`, `127.0.0.1` or `[::1]` (see [Conventions](control.md#conventions)).

| Key | Type | Default | Valid | Meaning |
|---|---|---|---|---|
| `bind` | `"ip:port"` | `"127.0.0.1:8080"` | a socket address | Listen address. `0.0.0.0:8080` for the whole network. |
| `token_env` | string | none (API open) | env var name | Name of the variable holding the bearer token every `/api/*` request must send. A warning is logged if the API is open on a non-loopback address. |
| `data_prefix` | string | `"push"` | letters, digits, `_`, `-`; not used by any source | Key namespace for `POST /api/data/{key}`: pushed keys must be `<data_prefix>:<name>`. |
| `app` | bool | `true` | | Serve the [web control app](control.md#web-control-app) at `/` and `/app`. `false` leaves only the API, preview and metrics. The app itself needs no token; it is as open as the API is without `token_env`. |

## `[mqtt]`

Optional. Makes paneld a Home Assistant device over MQTT discovery (see [Control interfaces](control.md#home-assistant-over-mqtt)). Absent: nothing connects. Separate from `type = "mqtt"` sources, which only read topics. Plain TCP. Restart-only.

| Key | Type | Default | Valid | Meaning |
|---|---|---|---|---|
| `host` | string | required | host name or IP | Broker address. |
| `port` | integer | `1883` | 1–65535 | Broker port. |
| `username` | string | none | not empty | MQTT user. |
| `password_env` | string | none | env var name; needs `username` | Name of the variable holding the password. |
| `discovery_prefix` | string | `"homeassistant"` | an MQTT topic | Must match HA's MQTT discovery prefix. |
| `node_id` | string | `"paneld"` | 1–64 of letters, digits, `_`, `-` | Device identifier, unique id prefix and client id. Give each panel its own. |
| `base_topic` | string | `"paneld/<node_id>"` | an MQTT topic | Prefix of the state, command, availability and notify topics. |

## `[playlist]`

| Key | Type | Default | Valid | Meaning |
|---|---|---|---|---|
| `default_duration_s` | float | `10` | > 0, at most 86400 | How long a scene without its own `duration_s` stays on screen. |

## `[notifications]`

How inline (text and icon) notifications look. Scene notifications use their scene and ignore this. All optional.

| Key | Type | Default | Valid | Meaning |
|---|---|---|---|---|
| `font` | string | the largest built-in font that fits (at least 13 px tall) | a built-in or `[[font]]` name | Notification font. |
| `font_scale` | integer | the largest that fits | 1–8 | Integer scale of the font. |
| `background` | colour | `"#000000"` | | Full-screen background. |

Text too wide to fit scrolls past a fixed icon, in the largest font no taller than a sixth of the canvas height, at about 3 characters per second.

## `[quiet_hours]`

Optional. A fixed dim level during a daily window. Restart-only. How dim levels combine: [Outputs: brightness](outputs.md#brightness-and-dimming).

| Key | Type | Default | Valid | Meaning |
|---|---|---|---|---|
| `start` | `"HH:MM"` | required | 24 h | Start, local time. |
| `end` | `"HH:MM"` | required | 24 h, differs from `start` | End, local time. Earlier than `start` means the window wraps midnight. |
| `brightness` | integer | required | 0–255 | Dim level inside the window (255 = no dimming). |

## `[auto_dim]`

Optional, two optional subsections. Restart-only.

`[auto_dim.sun]`: dim at night, from sunrise and sunset at a location (NOAA solar equations).

| Key | Type | Default | Valid | Meaning |
|---|---|---|---|---|
| `location` | `{ lat, lon }` | required | lat −90–90, lon −180–180 | Where to compute the sun for. |
| `night` | integer | `40` | 0–255 | Dim level at night. |
| `ramp_min` | float | `30` | 0–180 | Minutes of linear ramp after sunset and before sunrise. |

`[auto_dim.lux]`: dim from a light sensor that some source writes to the store.

| Key | Type | Default | Valid | Meaning |
|---|---|---|---|---|
| `key` | store key | required | | Key holding the lux reading, e.g. `ha:sensor.lux`. |
| `curve` | `[[lux, dim], ...]` | required | at least one point; lux strictly increasing; dim 0–255 | Piecewise linear map from lux to dim level, flat beyond the ends. |
| `stale_after_s` | float | `600` | > 0 | Readings older than this are ignored. |

## `[[font]]`

A BDF font file made available by name. Repeatable. Details: [Scenes: fonts](scenes.md#fonts).

| Key | Type | Default | Valid | Meaning |
|---|---|---|---|---|
| `name` | string | required | unique, not a built-in name | The name widgets use in `font = "..."`. |
| `path` | path | required | a readable BDF file | Relative to the config file. |

## `[[scene]]`

At least one is required. Scenes rotate in file order. Details: [Scenes and widgets](scenes.md).

| Key | Type | Default | Valid | Meaning |
|---|---|---|---|---|
| `name` | string | required | unique, not empty | Used by `--scene`, the API, MQTT and metrics. |
| `duration_s` | float | `[playlist] default_duration_s` | > 0, at most 86400 | Seconds on screen per turn. |
| `condition` | string | none (always eligible) | a [condition](scenes.md#conditions) | Only show the scene while this is true. |
| `[[scene.zone]]` | tables | none | | The zones, drawn in file order. |

`[[scene.zone]]` keys:

| Key | Type | Default | Valid | Meaning |
|---|---|---|---|---|
| `rect` | rect | required | inside the canvas | Where the zone is. |
| `widget` | table | required | see [widgets](scenes.md#widgets) | What it shows, selected by `type`: `clock`, `text`, `value`, `sparkline`, `image`, `video`. |
| `background` | colour | none (transparent) | | Fill behind the widget. |

## `[sources.<name>]`

Data sources, one table per source, selected by `type`: `static`, `home_assistant`, `mqtt`, `http_poll`, `prometheus`, `aircraft`, `system`, `media`. The name may use letters, digits, `_` and `-`. Every key, default and limit is in [Sources](sources.md).
