# Sources

Sources fetch data in the background and write it to the **store**. Widgets, templates, scene conditions and the lux auto-dim read the store. The render loop never waits on a source: it reads a snapshot of the store every frame.

Each `[sources.<name>]` table is one source, picked by `type`:

| `type` | Data | Keys it writes |
|---|---|---|
| [`static`](#static) | fixed values from the config | the keys you list |
| [`home_assistant`](#home_assistant) | HA entity states, attributes, forecasts, series | `ha:…` |
| [`mqtt`](#mqtt) | MQTT topic payloads | the keys you list |
| [`http_poll`](#http_poll) | values picked out of a JSON URL | the keys you list |
| [`prometheus`](#prometheus) | PromQL instant and range queries | the keys you list |
| [`aircraft`](#aircraft) | aircraft near a point (adsb.fi or a local receiver) | `<name>:…` |

There is also one push path: [`POST /api/data/{key}`](control.md#post-apidatakey) writes keys under `[http] data_prefix` (default `push:`).

## The store

The store maps a key to an entry. An entry holds a value, the time it was written and the name of the source that wrote it.

**Keys** look like `source:name` (see [common value types](configuration.md#common-value-types)). The prefix is a convention: `home_assistant` always writes `ha:…`, `aircraft` writes `<source name>:…`, and the other sources write exactly the keys you configure. Two sources must not write the same key.

**Value kinds:**

| Kind | From | Shown by |
|---|---|---|
| Number (with an optional unit) | numeric states, JSON numbers, numeric strings | `value`, templates, conditions |
| Text | other strings | templates, `value` with `condition_icons`, conditions |
| Bool | JSON booleans | templates (`on`/`off`), conditions |
| Series (time, number points) | HA `series`, Prometheus `range_queries` | `sparkline` |
| JSON | arrays and objects | nothing on screen yet (templates show `--`) |
| Image | not produced by any source yet | `image` with `key` |

JSON is turned into values the same way everywhere (`http_poll`, `mqtt`, HA attributes, pushes):
- numbers, and strings that parse as numbers, become Number;
- `true`/`false` become Bool;
- other strings become Text;
- arrays and objects become JSON;
- `null` means "no value", which removes the key.

**Staleness.** A source that loses its connection keeps its last values in the store; they age. `value` widgets with `stale_after_s` show `--` once a value is older than that. The lux auto-dim ignores readings older than its own `stale_after_s`.

## Supervision and health

- **Restarts.** A source that fails hard (lost connection, auth failure) is rebuilt from its config and restarted with backoff: 1 s, doubling to 60 s. The backoff resets once a run lasts 60 s.
- **Soft failures.** A single failed poll is counted as an error but doesn't restart the source. Each source then has its own retry rules, described below.
- **Metrics.** Health shows in [`/metrics`](control.md#metrics):
  - `paneld_source_up`
  - `paneld_source_errors_total`
  - `paneld_source_restarts_total`
  - `paneld_source_last_update_timestamp_seconds`
- **Live reload.** Only sources whose effective config changed are restarted (see [live reload](control.md#live-reload)). A removed source's keys are cleared. A restarted source's old values stay on screen until it writes again. After its first success, keys it no longer produces are removed.
- **One HTTP stack.** All HTTP and WebSocket traffic uses rustls with the system's webpki roots and the User-Agent `paneld/<version>`.

---

## `static`

Writes fixed values once, at start. For demos, and for laying out a scene before the real source exists.

```toml
[sources.demo]
type = "static"
values = { "demo:temp" = 21.5, "demo:status" = "ok", "demo:on" = true }
```

| Key | Type | Default | Valid | Meaning |
|---|---|---|---|---|
| `values` | table of key → number, string or bool | required | valid keys; finite numbers | Values to write. Numbers have no unit. |

---

## `home_assistant`

Entity states, attributes, weather forecasts and rolling series over the [Home Assistant WebSocket API](https://developers.home-assistant.io/docs/api/websocket). Message formats, with links, are in [`crates/paneld/src/sources/HA_PROTOCOL.md`](../crates/paneld/src/sources/HA_PROTOCOL.md).

Setup:
1. In Home Assistant, open your profile, then **Security**, and create a **long-lived access token**.
2. Put it in an environment variable, for example `HA_TOKEN=...` in `/etc/paneld/env` under systemd.
3. Add the source:

```toml
[sources.ha]
type = "home_assistant"
url = "ws://homeassistant.local:8123/api/websocket"
token_env = "HA_TOKEN"
series = ["sensor.outdoor_temperature"]
forecasts = ["weather.home"]
```

| Key | Type | Default | Valid | Meaning |
|---|---|---|---|---|
| `url` | string | required | `ws://` or `wss://` | WebSocket endpoint, normally `…:8123/api/websocket`. |
| `token_env` | string | required | env var name | Variable holding the long-lived token. |
| `entities` | list of entity IDs | `[]` | `domain.object_id` | Extra entities to subscribe to, beyond the ones scenes use. |
| `series` | list of entity IDs | `[]` | `domain.object_id` | Entities that also get a rolling `ha:<id>@series`. |
| `series_points` | integer | `288` | 1–10000 | Points kept per series. |
| `forecasts` | list of entity IDs | `[]` | `weather.*` entities | Weather entities whose forecast is fetched with `weather.get_forecasts`. |
| `forecast_type` | `"daily"` \| `"hourly"` | `"daily"` | | Which forecast to request. |
| `forecast_interval_s` | integer | `1800` | 30–86400 | Seconds between forecast fetches. |
| `forecast_len` | integer | `5` | 1–48 | Forecast periods written. |

**Which entities are subscribed.**
- Every entity used by a key `ha:<entity_id>…` that a widget, template, condition or `[auto_dim.lux]` reads.
- Plus everything in `entities`, `series` and `forecasts`.
- If no entity is referenced, nothing is subscribed.
- A scene edit that starts reading a new entity restarts this source on reload.

**Keys written:**

| Key | Value |
|---|---|
| `ha:<entity_id>` | The state: a Number when it is numeric (unit from the `unit_of_measurement` attribute), else Text. `unavailable` and `unknown` remove the key, so widgets show `--` and conditions are false. |
| `ha:<entity_id>.<attribute>` | Each attribute: Number, Text, Bool, or JSON for lists and objects. `null` attributes are skipped. An attribute that disappears is removed. |
| `ha:<entity_id>@series` | For `series` entities: a Series, one point per numeric state change, at most `series_points`. At start and after each reconnect, it is backfilled from the recorder's last 24 h (downsampled). If the backfill fails, that is logged and ignored. |
| `ha:<entity_id>.forecast.<n>.<field>` | For `forecasts` entities: field `<field>` of forecast period `n` (0 = soonest), e.g. `ha:weather.home.forecast.0.temperature`, `…forecast.1.condition`. |
| `ha:<entity_id>.forecast` | The first `forecast_len` periods as JSON. |

For a weather entity, the state is the condition (`sunny`, `rainy`, …): show it with a `value` widget and `condition_icons`. The current temperature is an attribute: `ha:weather.home.temperature`.

**Connection.**
- One WebSocket per run.
- A ping is sent every 30 s; if no pong arrives within 10 s, the run ends.
- Any failure ends the run and the supervisor reconnects with backoff:
  - connect or auth taking longer than 10 s;
  - `auth_invalid`;
  - the server closing the connection.
- `auth_invalid` is logged naming `token_env`, never the token.

---

## `mqtt`

Subscribes to topics on an MQTT broker (Mosquitto, the Home Assistant add-on, …) and writes their payloads to keys. **Plain TCP only:** MQTT over TLS is not supported, so don't point this at a broker across the internet.

```toml
[sources.mqtt]
type = "mqtt"
host = "homeassistant.local"
username = "paneld"
password_env = "MQTT_PASSWORD"

[sources.mqtt.topics]
"home:solar" = "home/power/solar"
"home:grid" = { topic = "home/meter", path = "$.power.grid_w", unit = "W" }
```

| Key | Type | Default | Valid | Meaning |
|---|---|---|---|---|
| `host` | string | required | host name or IP | Broker. |
| `port` | integer | `1883` | 1–65535 | Broker port. |
| `username` | string | none | not empty | MQTT user. |
| `password_env` | string | none | env var name; needs `username` | Variable holding the password. |
| `client_id` | string | `"paneld-<source name>"` | 1–256 characters | Must be unique on the broker. |
| `keep_alive_s` | integer | `30` | 1–65535 | MQTT keep-alive. |
| `topics` | table of key → topic | required, at least one | | Each entry: an exact topic string, or `{ topic, path, unit }`. |

Topic entry fields:

| Field | Meaning |
|---|---|
| `topic` | The exact topic. Wildcards (`+`, `#`) are rejected, so the list of keys is known when the config loads. Two keys may share a topic with different `path`s. |
| `path` | Optional [JSON path](#json-paths) into the payload. |
| `unit` | Optional unit attached to Number values. |

**Payloads:**
- With `path`, the payload must be JSON and the value at the path is stored. `null` removes the key.
- Without `path`, JSON is tried first (`21.5` is a Number, `true` a Bool, an object a JSON value). Otherwise the trimmed UTF-8 text is stored, so `on` is Text and `"21.5"` is still a Number.
- An empty payload (a deleted retained message) removes the topic's keys. Retained messages count as values.
- Unusable payloads (not JSON when `path` is set, the path is missing, not UTF-8) leave the key unchanged:
  - they are logged once per topic;
  - they are counted as source errors;
  - they don't mark the source down.

**Connection.**
- QoS 0, clean session, re-subscribed after every connect.
- If the broker is down, it retries from 1 s, doubling to 30 s.
- The last values stay in the store meanwhile.

---

## `http_poll`

Fetches a JSON URL on an interval and writes values picked out with JSON paths. Good for weather APIs and anything else that returns JSON.

```toml
[sources.weather]
type = "http_poll"
url = "https://api.example.com/v1/current?city=example"
interval_s = 600
extract = { "weather:temp" = { path = "$.current.temp", unit = "C" }, "weather:icon" = "$.current.icon" }
header_env = { "X-Api-Key" = "WEATHER_API_KEY" }
```

| Key | Type | Default | Valid | Meaning |
|---|---|---|---|---|
| `url` | string | required | `http://` or `https://`, no `user:pass@` | URL returning JSON. A query string (even with an API key) is fine; URLs are never logged. |
| `interval_s` | float | `60` | 1–86400 | Seconds between polls. |
| `extract` | table of key → path or `{ path, unit }` | required, at least one | valid keys and [paths](#json-paths) | Store key to the JSON path to read, optionally with a unit for numbers. |
| `headers` | table of name → value | `{}` | valid header names and values | Non-secret request headers. A name that contains `auth`, `cookie`, `key`, `token`, `secret` or `password` is rejected: use `header_env`. |
| `header_env` | table of name → env var name | `{}` | | Secret headers: the value is read from the named variable. Not both in `headers` and `header_env`. |
| `timeout_s` | float | `10` | 1–120 | Request timeout. |

**Writes.**
- Each successful poll writes every `extract` key in one batch.
- A path that is missing or `null` in a successful response **removes** its key.
- A failed poll writes nothing, so the old values stay. A poll fails on:
  - a transport error;
  - a non-2xx status;
  - a 429;
  - a body that isn't JSON or is over 8 MiB.

**Retries.**
- Polls never come faster than `interval_s`.
- A 429 waits for `Retry-After` (seconds; 60 s if absent, at most 1 h).
- Other errors back off `interval_s × 2ⁿ`, capped at 5 minutes.
- Redirects are followed only to the same host and port, so headers never travel to another site.

---

## `prometheus`

PromQL queries against a Prometheus server (or anything with its HTTP API: Thanos, VictoriaMetrics, Mimir).

```toml
[sources.prom]
type = "prometheus"
url = "http://prometheus.local:9090"
queries = { "prom:house_power" = { query = "sum(power_watts)", unit = "W" }, "prom:up" = "min(up)" }
range_queries = { "prom:power_hist" = { query = "sum(power_watts)", range_s = 3600, step_s = 60 } }
```

| Key | Type | Default | Valid | Meaning |
|---|---|---|---|---|
| `url` | string | required | `http(s)://` server base | `/api/v1/query` and `/api/v1/query_range` are appended. A path prefix is fine. |
| `interval_s` | float | `15` | 1–86400 | Seconds between rounds of queries. |
| `queries` | table of key → PromQL or `{ query, unit }` | `{}` | | Instant queries, written as Numbers. |
| `range_queries` | table of key → `{ query, range_s, step_s }` | `{}` | `0 < step_s ≤ range_s`, `range_s` at most 31 days and at most 11 000 points | Range queries over the last `range_s` seconds, written as a Series (for sparklines). |
| `bearer_env` | string | none | env var name | Variable holding a bearer token, if the server needs one. |
| `timeout_s` | float | `10` | 1–120 | Per-query timeout. |

At least one of `queries` and `range_queries` is required, and a key can't be in both.

**Results, per key:**
- An instant query must give one number: a scalar, or a vector with exactly one series.
- An empty vector, `NaN`/`Inf` or a native histogram removes the key.
- A vector with two or more series writes nothing for that key (the old value stays) and logs a warning naming the key and the series count. Aggregate in PromQL (`sum(...)`, `max(...)`).
- A query the server rejects (400/422, bad PromQL) is handled the same way, with the server's error text logged.
- A range query uses the **first** series, oldest point first, skipping non-finite samples. With no data the key is removed.

**Execution and retries.**
- All queries of a round run at the same time, at most 4 at once.
- Everything that succeeded is written in one batch.
- Transport errors, 5xx and 401/403 back off the whole source, like `http_poll`.
- A 429 waits for `Retry-After`.

---

## `aircraft`

Aircraft near a point, from [adsb.fi](https://adsb.fi)'s open API by default, with routes (origin and destination) from [adsbdb](https://www.adsbdb.com). Any API with the ADS-B Exchange v2 response format works, including a local receiver (readsb, tar1090).

```toml
[sources.aircraft]
type = "aircraft"
location = { lat = -33.94, lon = 151.18 }   # Sydney airport; use yours in config/local.toml
radius_nm = 10
max_altitude_ft = 15000
```

| Key | Type | Default | Valid | Meaning |
|---|---|---|---|---|
| `location` | `{ lat, lon }` | required | lat −90–90, lon −180–180 | The point to measure from. |
| `url` | string | `https://opendata.adsb.fi/api/v2/lat/{lat}/lon/{lon}/dist/{radius_nm}` | `http(s)://` | URL template; `{lat}`, `{lon}` and `{radius_nm}` are filled in. A local readsb: `http://receiver.local/tar1090/data/aircraft.json` (no placeholders needed). |
| `location_decimals` | integer | `2` | 0–6 | Decimals the location is rounded to **before it is sent**; 2 is about 1 km. |
| `radius_nm` | integer | `10` | 1–250 | Search radius in nautical miles. |
| `max_altitude_ft` | float | none | finite | Ignore aircraft above this barometric altitude. |
| `include_ground` | bool | `false` | | Keep aircraft reporting they are on the ground. |
| `interval_s` | float | `15` | 1–86400 | Seconds between polls. adsb.fi allows 1 request per second. |
| `routes` | bool | `true` | | Look up routes on adsbdb for the aircraft in the list (cached per callsign). |
| `list_len` | integer | `5` | 1–20 | How many aircraft go into `<name>:list`. |
| `route_separator` | string | `"-"` | at most 8 characters | Between origin and destination in `route` (`SYD-AKL`). The built-in fonts have no `→`; use it only if your font has it. |

**Privacy.**
- Only the rounded location is sent. The request radius grows by the most the rounding can move the point, so no aircraft in range is missed.
- Distance and bearing are computed locally from the exact location, which never leaves the process.
- The URL and the location are not logged above debug level.

**Keys written,** prefixed with the source's name, so `[sources.aircraft]` writes `aircraft:*` and `[sources.planes]` writes `planes:*`:

| Key | Value |
|---|---|
| `<name>:count` | Number of aircraft in range after the filters. |
| `<name>:nearest.callsign` | Text, e.g. `QFA1`. |
| `<name>:nearest.registration` | Text, e.g. `VH-OQA`. |
| `<name>:nearest.type` | Text, ICAO type code, e.g. `A388`. |
| `<name>:nearest.desc` | Text, type description, e.g. `AIRBUS A-380-800`. |
| `<name>:nearest.route` | Text, `<origin><separator><destination>`, e.g. `SYD-LHR` (needs `routes`). |
| `<name>:nearest.origin`, `<name>:nearest.destination` | Text, airport codes (needs `routes`). |
| `<name>:nearest.alt_ft` | Number, barometric altitude (geometric if there is no barometric value). |
| `<name>:nearest.speed_kt` | Number, ground speed. |
| `<name>:nearest.dist_nm` | Number, distance from the exact location, 2 decimals. |
| `<name>:nearest.bearing` | Number, degrees from the location to the aircraft, 1 decimal. |
| `<name>:nearest.bearing_compass` | Text, `N`, `NE`, `E`, … |
| `<name>:list` | JSON array of the nearest `list_len` aircraft, closest first, with the same fields. |

A field the provider doesn't report is left out. Every poll replaces all `nearest.*` keys in one batch, so with nothing in range there are no `nearest.*` keys (widgets show `--`) and `count` is 0. A failed poll leaves the old values. Polling and retries follow the `http_poll` rules.

The adsb.fi terms ask for attribution: when an aircraft source is configured, the preview page shows the credit. adsb.fi is for personal, non-commercial use.

---

## JSON paths

`http_poll` `extract` and `mqtt` `path` use a small path syntax. `$` is the root, followed by any of:
- `.name`;
- `["name"]` (for names with dots or spaces);
- `[index]` (a non-negative array index).

Examples: `$.current.temp`, `$.list[0].main.temp`, `$["odd.key"]`. Wildcards, filters and slices are not supported. Paths are checked when the config loads; an error names the column.

## Not built yet

DESIGN.md §6 also lists a **clock/system** source (uptime, CPU temperature) and a **media folder** source (images in the store, for `image` widgets with `key`). Neither exists yet. The `clock` widget covers time and date.
