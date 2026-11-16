# Control interfaces

How to drive, watch and operate a running paneld. Contents:
- the HTTP control API;
- Home Assistant over MQTT;
- the metrics endpoint;
- live reload and signals;
- the command line;
- environment variables.

All the HTTP endpoints share one server on `[http] bind` (default `127.0.0.1:8080`):

| Path | Purpose | Auth |
|---|---|---|
| `/api/*` | control API | bearer token when `[http] token_env` is set |
| `/preview`, `/preview.png` | [preview](outputs.md#preview) | never |
| `/metrics` | Prometheus metrics | never |

Every control surface feeds the same commands to the render loop, which applies them at the start of the next frame:
- notify, and clear notifications;
- pin a scene, and release the pin;
- power;
- brightness.

HTTP and MQTT can be mixed freely. Brightness and power always start at 255 and on; they are not saved across restarts.

---

## HTTP control API

### Conventions

- **Bodies** are JSON. The `Content-Type` is not checked, so `curl -d '{...}'` works. Unknown fields are rejected. Bodies are limited to 64 KiB (413).
- **Commands** answer `202 {"status": "accepted"}` once they are queued for the render loop. If the render loop has died, they answer 503.
- **Errors** are always `{"error": "<message>"}`, with 400 (bad input), 401, 404, 409, 413 or 503.
- **Authentication.** With `[http] token_env = "PANELD_API_TOKEN"`, every `/api/*` request needs `Authorization: Bearer <token>`, compared in constant time. Without it the request gets 401 with `WWW-Authenticate: Bearer`. Without `token_env` the API is open; a warning is logged if it is open on a non-loopback address.

The examples below use:

```bash
H=http://localhost:8080
A="Authorization: Bearer $PANELD_API_TOKEN"   # drop -H "$A" when there is no token
```

### `GET /api/state`

What the panel is doing. `200`:

```json
{
  "scenes": ["home", "banner", "image"],
  "scene": "home",
  "pinned": null,
  "notifications": [
    { "summary": "Washing done", "priority": 5, "repeats_left": 1, "showing": true }
  ],
  "on": true,
  "brightness": 255,
  "dim": 255,
  "fps": 30.0
}
```

| Field | Meaning |
|---|---|
| `scenes` | Every configured scene, in playlist order. |
| `scene` | The scene on screen. `null` while nothing is eligible (black) or an inline notification shows. |
| `pinned` | The pinned scene, or `null` when following the playlist. |
| `notifications` | Queued and showing notifications. `summary` is the text, or `scene:<name>` for a scene notification. `repeats_left` includes the current one. |
| `on` | Power. |
| `brightness` | User brightness, 0–255. |
| `dim` | Auto-dim factor, 0–255 (255 = none). |
| `fps` | Frames rendered in the last second. |

```bash
curl -H "$A" $H/api/state
```

### `POST /api/notify`

Queue a [notification](scenes.md#notifications). Give either inline `text` or a `scene`, not both:

| Field | Type | Default | Valid | Meaning |
|---|---|---|---|---|
| `text` | string | | 1–500 characters | Inline text, full screen. |
| `icon` | string | none | a [built-in icon](scenes.md#icons); `text` only | Icon beside the text. |
| `color` | `"#rrggbb"` | `"#ffffff"` | `text` only | Text and icon colour. |
| `scene` | string | | a configured scene | Show this scene instead. |
| `duration_s` | number | `10` | > 0, at most 3600 | Seconds on screen per showing. |
| `priority` | integer | `0` | −100 to 100 | Higher shows first, and a strictly higher one interrupts. |
| `repeat` | integer | `1` | 1–100 | Times to show it, back to back. |

`202`, or `400` naming the problem.

```bash
curl -H "$A" -d '{"text": "Washing done", "icon": "bolt", "color": "#00ff80", "priority": 5}' $H/api/notify
curl -H "$A" -d '{"scene": "alert", "duration_s": 30, "repeat": 2}' $H/api/notify
```

### `DELETE /api/notify`

Empty the queue and end the notification on screen. `202`.

```bash
curl -H "$A" -X DELETE $H/api/notify
```

### `POST /api/data/{key}`

Write a value into the store, for scripts and automations. The key must be `<data_prefix>:<name>`:
- `data_prefix` is `[http] data_prefix`, default `push`;
- the name is 1–128 letters, digits, `_`, `.` and `-`.

The body is one of these:

| Body | Effect |
|---|---|
| `42`, `21.5`, `"21.5"` | Number (numeric strings become numbers). |
| `"text"` | Text. |
| `true` / `false` | Bool. |
| `{"value": 12.5, "unit": "W"}` | Number with a unit (`unit` only applies to numbers). |
| `null` or `{"value": null}` | Remove the key. |

Arrays and objects are rejected (400).

Answers:
- `200 {"status": "ok"}` on success.
- `409` if the key was written by a real source, or if 1000 pushed keys already exist (remove some with `null`).

Values are stamped with the source name `http` and age like any other value, so `stale_after_s` works. They are not saved across restarts.

```bash
curl -H "$A" -d '{"value": 12.5, "unit": "W"}' $H/api/data/push:power
curl -H "$A" -d '"open"' $H/api/data/push:garage
curl -H "$A" -d 'null' $H/api/data/push:garage
```

Use them like any key: `{push:power:.0} W` in a text widget, or `condition = "push:garage == 'open'"` on a scene. Validation rejects a `data_prefix` that any source uses, so pushes can't collide with source data.

### `POST /api/scene/{name}`

Pin a scene: it stays on screen whatever its condition, until released. Notifications still interrupt it. The pin survives a config reload while the scene exists. `202`, or `404` for an unknown scene.

```bash
curl -H "$A" -X POST $H/api/scene/home
```

### `DELETE /api/scene`

Release the pin and return to the playlist. `202`.

```bash
curl -H "$A" -X DELETE $H/api/scene
```

### `POST /api/brightness`

`{"value": 0-255}`, the user brightness. It scales every output's configured brightness, before auto-dim (see [brightness](outputs.md#brightness-and-dimming)). `202`.

```bash
curl -H "$A" -d '{"value": 128}' $H/api/brightness
```

### `POST /api/power`

`{"on": false}` blanks the panel, and `{"on": true}` restores it. Rendering continues, and black frames keep going to the card. `202`.

```bash
curl -H "$A" -d '{"on": false}' $H/api/power
```

---

## Home Assistant over MQTT

With a top-level [`[mqtt]`](configuration.md#mqtt) section, paneld connects to the broker and appears in Home Assistant as one device, through [MQTT discovery](https://www.home-assistant.io/integrations/mqtt/#mqtt-discovery). HA's MQTT integration must use the same broker. Without `[mqtt]`, nothing connects. The formats were checked against HA's docs; see [`crates/paneld/src/mqtt_ha/HA_DISCOVERY.md`](../crates/paneld/src/mqtt_ha/HA_DISCOVERY.md).

```toml
[mqtt]
host = "homeassistant.local"
username = "paneld"
password_env = "PANELD_MQTT_PASSWORD"
```

### Entities

The entity IDs below are for the default `node_id = "paneld"`:

| Entity | Does |
|---|---|
| `light.paneld` | On/off and brightness 0–255, the user brightness. Auto-dim is applied on top and isn't shown, so the slider doesn't jump when the schedule dims. |
| `select.paneld_scene` | Pin a scene, or `auto` to follow the playlist. The options follow config reloads. A scene literally named `auto` can't be pinned here. |
| `text.paneld_message` | Typing a message shows it as a notification (10 s, priority 0). |
| `binary_sensor.paneld_card` | Card connectivity: on while every Colorlight output sent frames without errors in the last 5 s. Only published when there is a Colorlight output. |
| `sensor.paneld_fps` | Frames rendered per second. Published on a change of more than 1, or at most every 10 s. |

### Topics

With `B` = `base_topic` (default `paneld/<node_id>`), `P` = `discovery_prefix` (default `homeassistant`) and `N` = `node_id`:

| Entity | Discovery config (retained) | State (retained) | Command |
|---|---|---|---|
| light | `P/light/N/light/config` | `B/light/state` | `B/light/set` |
| select | `P/select/N/scene/config` | `B/scene/state` | `B/scene/set` |
| text | `P/text/N/message/config` | none | `B/message/set` |
| binary_sensor | `P/binary_sensor/N/card/config` | `B/card/state` | none |
| sensor | `P/sensor/N/fps/config` | `B/fps/state` | none |

Other topics:
- `B/availability`: `online` while paneld runs, `offline` otherwise. Retained; also the MQTT last will (QoS 1), and published explicitly on a clean shutdown.
- `B/notify`: raw notification JSON, the same body as [`POST /api/notify`](#post-apinotify), e.g. `{"text": "Door open", "icon": "bolt", "priority": 5}`. Invalid messages are logged and ignored.
- `P/status`: HA's birth message. When HA publishes `online` there, paneld sends everything again.

Payloads:

| Topic | Payload |
|---|---|
| `B/light/state` | `{"state": "ON", "brightness": 255, "color_mode": "brightness"}` |
| `B/light/set` | JSON with `state` (`"ON"`/`"OFF"`) and/or `brightness` (0–255); other fields such as `transition` are ignored. Brightness is applied first, so turning on at a level lights up at that level. |
| `B/scene/state` | the pinned scene, or `auto` |
| `B/scene/set` | a scene name, or `auto` to release the pin |
| `B/message/set` | plain text, shown as a notification with the defaults |
| `B/card/state` | `ON` / `OFF` |
| `B/fps/state` | a number, e.g. `30.0` |

**Delivery.**
- Everything uses QoS 0.
- Configs, states and availability are retained.
- After every reconnect, and whenever HA says it is online, paneld publishes everything again.
- Reconnects back off from 1 s to 30 s.
- Plain TCP only. `[mqtt]` is read at startup; changing it needs a restart.

To run two panels on one broker, give each its own `node_id`.

---

## Metrics

`GET /metrics` serves the Prometheus text format (0.0.4). It isn't behind the API token. It only holds counts and names from the config (interface, source and scene names). To keep it off the network, bind `[http]` to `127.0.0.1` or firewall the port.

| Metric | Type | Labels | Meaning |
|---|---|---|---|
| `paneld_build_info` | gauge | `version` | Always 1. |
| `paneld_frames_rendered_total` | counter | | Frames rendered. |
| `paneld_frames_skipped_total` | counter | | Ticks skipped because rendering overran. |
| `paneld_render_duration_seconds` | histogram | | Time to build a frame (buckets 0.5 ms to 100 ms). |
| `paneld_fps` | gauge | | Frames per second over the last second. |
| `paneld_sink_send_duration_seconds` | histogram | `sink` (interface) | Time to send a frame to the card (buckets 0.1 ms to 50 ms). |
| `paneld_sink_errors_total` | counter | `sink` | Frames that failed to send. |
| `paneld_card_detected` | gauge | `card` (interface) | 1 if frames went out without error since the previous scrape. |
| `paneld_source_up` | gauge | `source` | 1 if the source's last attempt succeeded. |
| `paneld_source_errors_total` | counter | `source` | Errors; reset when a reload restarts the source. |
| `paneld_source_restarts_total` | counter | `source` | Supervisor restarts after failures. |
| `paneld_source_last_update_timestamp_seconds` | gauge | `source` | Unix time of the last success; no sample before the first. |
| `paneld_store_keys` | gauge | | Keys in the store. |
| `paneld_notifications_queued` | gauge | | Queued or showing notifications. |
| `paneld_config_reloads_total` | counter | | Accepted reloads. |
| `paneld_config_reload_errors_total` | counter | | Rejected reloads. |
| `paneld_current_scene` | gauge | `scene` | 1 for the scene on screen; no sample while black or an inline notification shows. |
| `paneld_power_on` | gauge | | 1 if the panel is on. |
| `paneld_brightness` | gauge | | User brightness, 0–255. |
| `paneld_dim` | gauge | | Auto-dim factor, 0–255 (255 = none). |

```yaml
scrape_configs:
  - job_name: paneld
    static_configs:
      - targets: ["paneld.local:8080"]
```

Useful alerts:
- `paneld_source_up == 0`
- `time() - paneld_source_last_update_timestamp_seconds > 600`
- `paneld_card_detected == 0`
- `increase(paneld_config_reload_errors_total[1h]) > 0`

---

## Live reload

`paneld run` picks up edits to its config file without a restart.

**How a change is picked up.**
- The file is read once a second. A change is applied once the file has had the same new content on two reads in a row, so a half-written save is never parsed and a reload takes about 2 s.
- Touching the file without changing it does nothing.
- `kill -HUP <pid>` (or `systemctl reload paneld`) reloads at once.
- Editors that save in place and editors that rename and replace both work.

**Applying it.** The new config is fully parsed, validated and prepared (fonts and images decoded) off the render thread. It is then swapped in between two frames. The playlist restarts at its first eligible scene.

**What applies when.**
- **Applied live:**
  - scenes, zones, widgets, conditions, fonts, `[notifications]` and `[playlist]`;
  - `[sources]`, per source. Only sources whose effective config changed are restarted, and that includes a Home Assistant source whose scenes now read different entities. A removed source's keys are cleared. A source that needs an unset environment variable isn't started, the old one keeps running and the reload counts an error.
  - The notification queue and the pin are kept.
- **Needs a restart:** `canvas.fps`, `[http]`, `[mqtt]`, `[quiet_hours]`, `[auto_dim]` and every `[[output]]`. A difference logs a warning naming it, and the rest of the reload applies.
- **Rejected:** a changed canvas width or height.

**Errors never take the panel down.** Any of these is logged and counted in `paneld_config_reload_errors_total`, and the old config keeps running:
- a parse or validation error;
- an unreadable file;
- a canvas size change;
- with `run --scene X`, scene `X` disappearing.

Fix the file and the next change is picked up.

**Limitation:** image, font and video files are not watched. After replacing one, edit the config or send `SIGHUP`.

## Signals

| Signal | Effect |
|---|---|
| `SIGHUP` | Reload the config now (see above). |
| `SIGTERM`, `SIGINT` (Ctrl-C) | Clean shutdown, in this order: <ol><li>stop the HTTP server;</li><li>publish MQTT `offline` and disconnect;</li><li>stop the sources (5 s grace);</li><li>stop the render loop;</li><li>stop the sinks.</li></ol> `ffmpeg` children are killed. The card keeps showing the last frame. |

---

## Command line

```text
paneld [--log-format text|json] <command> [options]
```

| Command | What it does |
|---|---|
| `run --config FILE [--scene NAME]` | Run the daemon. `--scene` shows only that scene, ignoring its condition. |
| `check --config FILE` | Validate a config and exit. Prints `FILE: ok (WxH canvas, N scene(s))`, plus warnings (video `url_env` unset, `ffmpeg` not found) on stderr. Exits non-zero, listing every error, if the config is invalid. Environment variables for secrets are not required here. |
| `render --config FILE --scene NAME --out PNG` | Render one frame of a scene to a PNG, conditions ignored. Options below. |
| `test-pattern …` | Draw a bring-up test pattern to a PNG and/or send it to the card. Options below. |
| `probe --interface NIC [--timeout-ms 500]` | Ask the Colorlight receivers on a port what they are. Prints index, model, firmware, reported size and whether double sync is needed, plus warnings for mixed firmware or sizes. Exits non-zero if nothing answers. Needs `CAP_NET_RAW`. |

`render` options:

| Option | Default | Meaning |
|---|---|---|
| `--time RFC3339` | now | Time to render, e.g. `2026-01-02T13:37:00+01:00`. |
| `--elapsed SECONDS` | `0` | Seconds since the scene started (scroll position, GIF frame). |
| `--scale N` | `1` | Pixel scale of the PNG. |
| `--output N` | | Write the card-space framebuffer of `[[output]]` number N instead of the canvas: the canvas through that output's map, before gamma, white point and colour order. |

Store-backed widgets render with an empty store (placeholders).

`test-pattern` options:

| Option | Default | Meaning |
|---|---|---|
| `--pattern P` | `bars` | One of the patterns below. |
| `--width`, `--height` | the config's canvas with `--config` | Canvas size. |
| `--panel-width`, `--panel-height` | the canvas size | One panel's size, for `panel-ids` and `grid`. |
| `--config FILE --output N` | | Take the canvas size and output map from that config's colorlight output; the pattern is drawn on the canvas and goes through the map. |
| `--card-space` | off | With `--config`: draw directly in card coordinates, bypassing the regions. |
| `--color "#rrggbb"` | `#ffffff` | Colour for `solid`. |
| `--frame N` | `0` | Position for `walk`. |
| `--out PNG`, `--scale N` | | Write a PNG. |
| `--interface NIC` | | Send to the card until Ctrl-C (`walk` animates). Needs `CAP_NET_RAW`. |
| `--fps` | `30` | `--interface` only. |
| `--brightness` | `64` | `--interface` only; low so bring-up isn't blinding. |
| `--gamma` | `1.0` | `--interface` only; raw values. |
| `--color-order` | `rgb` | `--interface` only. |
| `--firmware-major` | discovered | `--interface` only. |
| `--sync-gap-us` | `500` | `--interface` only. |

At least one of `--out` and `--interface` is required.

Patterns:

| Pattern | Shows |
|---|---|
| `bars` | Eight vertical bars: white, yellow, cyan, green, magenta, red, blue, black. Use it to find the colour order and spot column mirroring. |
| `gradient` | Red, green, blue and grey ramps. |
| `panel-ids` | Per panel: a border colour, an index number and a top-left corner marker, numbered row-major from the top-left. |
| `grid` | Lines every 8 px. Each panel's top and left edges are cyan, and its scan-half row (`panel-height / 2`) is yellow. |
| `solid` | One colour (`--color`). |
| `walk` | One pixel in raster order, at `--frame`. |

## Environment variables

| Variable | Used for |
|---|---|
| `RUST_LOG` | Log filter, default `info`, e.g. `RUST_LOG=warn,paneld::sources=debug`. |
| `PANELD_LOG_FORMAT` | `text` (default) or `json`, same as `--log-format`; the flag wins. `text` suits journald and terminals. `json` writes one object per line (`timestamp`, `level`, `fields`, `target`) for log shippers. |
| `TZ` | Time zone for clocks and quiet hours (default: the system's). |
| the names in `*_env` keys | Secrets and stream URLs, read at start-up. Under systemd, keep them in `/etc/paneld/env` (see [`deploy/README.md`](../deploy/README.md)). |

Secrets are never logged, and neither are source URLs (they may hold API keys).
