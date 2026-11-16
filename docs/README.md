# paneld documentation

| Page | Covers |
|---|---|
| [Configuration reference](configuration.md) | Every config section and key: types, defaults, limits, what reloads live. |
| [Sources](sources.md) | The store. The `static`, `home_assistant`, `mqtt`, `http_poll`, `prometheus` and `aircraft` sources: settings, keys written, failure behaviour. |
| [Scenes and widgets](scenes.md) | Layout, zones, the `clock`, `text`, `value`, `sparkline`, `image` and `video` widgets, fonts, icons, templates, number formats, conditions, the playlist and notifications. |
| [Outputs](outputs.md) | The preview, the Colorlight card, the output map (regions, rotation, flip), brightness and auto-dim. |
| [Control interfaces](control.md) | The HTTP API (every endpoint), Home Assistant over MQTT (entities, topics, payloads), metrics, live reload, signals, the CLI and environment variables. |

Elsewhere in the repo:

- [`config/example.toml`](../config/example.toml): a commented example config.
- [`deploy/INSTALL-debian.md`](../deploy/INSTALL-debian.md): installing on Debian, Ubuntu and Raspberry Pi OS.
- [`deploy/README.md`](../deploy/README.md): hardware bring-up and the systemd service.
- [`crates/colorlight-proto/PROTOCOL.md`](../crates/colorlight-proto/PROTOCOL.md): the Colorlight wire protocol, with sources.
- [`crates/paneld/src/sources/HA_PROTOCOL.md`](../crates/paneld/src/sources/HA_PROTOCOL.md): the Home Assistant WebSocket messages used.
- [`crates/paneld/src/mqtt_ha/HA_DISCOVERY.md`](../crates/paneld/src/mqtt_ha/HA_DISCOVERY.md): the Home Assistant MQTT discovery formats used.
- [`DESIGN.md`](../DESIGN.md): architecture, decisions and the milestone history.
