# Home Assistant WebSocket protocol, as used by `ha.rs`

Checked on 2026-11-14. Entity IDs in this repo are generic examples.

## Sources

- Docs (the old `developers.home-assistant.io/docs/api/websocket` redirects here):
  <https://contributors.home-assistant.io/core/internals/websocket-api/>
- Weather forecasts: <https://www.home-assistant.io/integrations/weather/>
  (the `weather.get_forecasts` action).
- Core source (`dev` branch), read where the docs have no verbatim example:
  - `homeassistant/components/websocket_api/commands.py`: `call_service`,
    `subscribe_entities`, `ping`
  - `homeassistant/components/websocket_api/messages.py`: the `a`/`c`/`r`
    diff messages (`_state_diff_event`)
  - `homeassistant/components/websocket_api/auth.py`: `auth_*` messages
  - `homeassistant/core.py` (`State.as_compressed_state`), `const.py`
    (`COMPRESSED_STATE_*`): the short keys `s a c lc lu`
  - `homeassistant/components/history/websocket_api.py` and
    `homeassistant/components/recorder/history/modern.py`: the history command

## Confirmed in the docs

- **Auth.** Server sends `{"type":"auth_required","ha_version":…}`. Client replies
  `{"type":"auth","access_token":"<token>"}`. Server answers `auth_ok` (with
  `ha_version`) or `auth_invalid` (with `message`) and then closes.
- **Commands** carry an integer `id` that must increase per connection.
  Replies: `{"id","type":"result","success":true,"result":…}` or
  `{"id","type":"result","success":false,"error":{"code","message"}}`.
- **`ping`** `{"id","type":"ping"}` is answered by `{"id","type":"pong"}`
  (not a `result`).
- **`subscribe_entities`** `{"id","type":"subscribe_entities","entity_ids":[…]}`
  (optional `entity_ids` limits it). Events are `{"id","type":"event","event":{…}}`
  with up to three keys: `a` (added entities, full compressed state), `c`
  (changed entities, diff with changed values under `+` and removed attributes
  under `-`) and `r` (removed entity IDs). Compressed state keys: `s` state,
  `a` attributes, `c` context, `lc` last_changed, `lu` last_updated.
- **`call_service`** `{"id","type":"call_service","domain","service","service_data","target","return_response":true}`;
  the result is `{"context":{…},"response":…}`.
- **`weather.get_forecasts`** takes `type` (`daily`, `hourly`, `twice_daily`)
  and an entity target; the response is keyed by entity ID, each with a
  `forecast` list. Fields seen in the docs: `datetime`, `condition`,
  `precipitation_probability`, `temperature`, `wind_bearing`, `dew_point`,
  `wind_speed`, `precipitation`, `humidity`. Which fields exist depends on the
  weather integration.

## Confirmed only from core source (the docs page has no verbatim example)

- The exact nesting of `a`/`c`/`r` (`messages.py`):
  - `{"a": {"<entity_id>": {"s","a","c","lc"[,"lu"]}}}`; `lu` is omitted when it equals `lc`; `c` is a bare string when it only has an id.
  - `{"c": {"<entity_id>": {"+": {"s"?,"lc"?,"lu"?,"c"?,"a"?:{changed attrs}}, "-": {"a":[removed attr names]}}}}`.
    **`s` is absent when the state did not change**, so the client must keep the previous state (the source tracks it).
  - `{"r": ["<entity_id>"]}`.
- `subscribe_entities` replies with the `result` first, then one `event` with only `a`
  holding the current states. Empty or missing `entity_ids` means *every* entity,
  so the source never sends an empty list (it does not subscribe at all).
- **History backfill** is `{"type":"history/history_during_period","start_time":"<ISO 8601>","entity_ids":[…],"include_start_time_state":true,"significant_changes_only":false,"minimal_response":true,"no_attributes":true}`
  (`end_time` optional). The result is `{"<entity_id>": [{"s": state, "lu": unix seconds[, "lc": …]}]}`
  (`websocket_api.py`, `_history_compressed_state`; timestamps are floats).
  The `history` integration is part of `default_config`; if it is not loaded
  the command fails with `unknown_command`, which the source logs and ignores.

## Not confirmed

- No example of the `a`/`c`/`r` events or of the history result appears in the
  developer docs; test fixtures are built from the core source above, not from
  a recorded session against a real instance.
- The docs page does not say how long the server waits before dropping an
  idle connection; the source pings every 30 s regardless.
- Nothing documents a limit on the number of `entity_ids`.
