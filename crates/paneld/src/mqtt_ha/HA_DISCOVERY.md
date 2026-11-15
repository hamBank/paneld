# Home Assistant MQTT discovery: what `mqtt_ha.rs` relies on

Checked against the official Home Assistant documentation (fetched 2026-11-15).
Nothing here is from memory; where a page does not say something, that is
noted instead of assumed.

## MQTT integration, discovery
<https://www.home-assistant.io/integrations/mqtt/>

- **Topic:** `<discovery_prefix>/<component>/[<node_id>/]<object_id>/config`.
  The prefix defaults to `homeassistant`; `<node_id>` is optional. The page's
  best practice is `<object_id>` = the `unique_id` with no node id. We keep the
  node id in the topic (`homeassistant/light/<node_id>/light/config`) so that
  several paneld nodes on one broker never share a topic, and give each entity
  a `unique_id` of `<node_id>_<object>`.
- **Retained configs:** "A discovery payload can be sent with a retain flag
  set", and the broker then replays it when the MQTT integration starts. The
  page warns that retained configs can leave ghost entities and prefers
  (re)sending discovery when the birth message arrives. We do both: retained,
  and resent on the birth message and on every reconnect.
- **Birth message:** published by HA on `homeassistant/status` (the default),
  payloads `online` and `offline`. We subscribe to `<discovery_prefix>/status`
  and republish everything on `online`.
- **`device` block** (abbreviation `dev`): keys `identifiers`, `name`,
  `manufacturer`, `model`, `sw_version`, among others. "At least one of
  `identifiers` or `connections` must be set." "For entities, the device
  information is only used when the entity has a `unique_id`", so every entity
  has one. Entities that carry the same `identifiers` land on one device.
- **`origin` block** (`o`): `name` required, `sw_version`, `support_url`.
  Recommended for all discovery, required for device-based discovery. We send
  it on every entity.
- **Availability:** `availability_topic`, `payload_available` (default
  `online`), `payload_not_available` (default `offline`). Not combined with the
  `availability` list.
- **Removal:** publishing an empty (retained) payload to the config topic
  removes the entity. We use it to remove a stale `card` entity when the config
  has no Colorlight output.
- Not used: device-based discovery (`cmps`), abbreviations (we write full key
  names).

## `light`, JSON schema
<https://www.home-assistant.io/integrations/light.mqtt/>

- Only `command_topic` is required. `unique_id` is required with device-based
  discovery (and we always send it). `schema: json` selects the JSON schema.
- Command payload (to us): JSON "omitting fields when they aren't needed",
  with `state` and `brightness` among the fields; `color_mode` is not sent to
  the light. We read `state` (`ON`/`OFF`) and `brightness` and ignore the rest
  (`transition`, ...).
- State payload (from us): JSON that "may contain" `state` (`ON`, `OFF` or
  null), `brightness`, `color_mode`, ... ; `color_mode` is optional and must be
  one of `supported_color_modes`.
- `brightness_scale` defaults to 255, which is our scale, so no conversion.
- `supported_color_modes`: we send `["brightness"]` (dimmable, no colour).
  The page lists the key; the value `brightness` is the documented
  "brightness only" colour mode of the light entity.
- The page has no JSON-schema discovery example (only YAML), so the payload is
  assembled from the key descriptions.

## `select`
<https://www.home-assistant.io/integrations/select.mqtt/>

- Required: `command_topic`, `options` (a list; may be empty or one item).
- `state_topic` optional: the payload is the option; an empty payload is
  ignored. With a `state_topic`, `optimistic` defaults to false.
- Command payload is the selected option string (no template).

## `text`
<https://www.home-assistant.io/integrations/text.mqtt/>

- Only `command_topic` required. `min` defaults to 0 and `max` to 255 (the
  maximum allowed). `mode` is `text` (default) or `password`. We send `min: 1`,
  `max: 255`.
- `state_topic` optional; we do not publish a state (a notification is an
  action, not a value), so HA shows the entity as unknown until set.

## `binary_sensor`
<https://www.home-assistant.io/integrations/binary_sensor.mqtt/>

- `state_topic` required; valid states `ON` and `OFF` (`payload_on` and
  `payload_off` default to those). `unique_id` required for device-based
  discovery.
- The MQTT page only shows `device_class: opening`. The classes (`connectivity`
  is the one we use) are in the binary sensor class list; they are not on the
  MQTT page, so that one is taken from the Home Assistant binary sensor docs
  (<https://www.home-assistant.io/integrations/binary_sensor/#device-class>)
  and is worth a glance if HA ever rejects the config.

## `sensor`
<https://www.home-assistant.io/integrations/sensor.mqtt/>

- `state_topic` (values), `unit_of_measurement`, `state_class`,
  `suggested_display_precision` ("the number of decimals which should be used
  in the sensor's state after rounding"), `entity_category` (for sensors it
  "must be `diagnostic`"), `expire_after`, `name`, `unique_id`. We do not use
  `expire_after`; availability covers the offline case.

## Names
With a device named `paneld`, an entity whose `name` is `null` takes the device
name (this is what the MQTT page documents for `name`: "can be set to `null`
if only the device name is relevant"), so the light is `light.paneld`; the
others are named `Scene`, `Message`, `Card` and `FPS` and show up as
`select.paneld_scene`, `text.paneld_message`, `binary_sensor.paneld_card` and
`sensor.paneld_fps`.
