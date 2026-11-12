# paneld

A Rust daemon that composes data from Home Assistant, HTTP APIs, Prometheus and pushed messages into scenes, and drives HUB75 LED panels through a Colorlight 5A-75B card over raw Ethernet.

Architecture, config schema and roadmap are in [`DESIGN.md`](DESIGN.md).

## Status

**M0 (skeleton) is done.** It includes the config parsing and validation, canvas, render loop, HTTP preview sink, `clock` and `text` widgets, and the `run`, `check` and `render` commands. It runs on a laptop without hardware.

Not built yet: Colorlight output (M1), scene rotation and more widgets (M2), data sources (M3), control API and MQTT (M4). See `DESIGN.md` §14.

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

`render` also takes `--time <RFC 3339>` and `--elapsed <seconds>` for reproducible output.

## Config (M0)

See [`config/example.toml`](config/example.toml). Unknown keys are rejected, and validation reports every problem at once.

| Section | Keys |
|---|---|
| `[canvas]` | `width`, `height`, `fps` (1–60, default 30) |
| `[[output]]` | `type = "preview"`, `scale` (1–32, default 8), `grid` (default false) |
| `[[output]]` | `type = "colorlight"`, `interface` (required), `gamma` (1.0–4.0, default 2.2), `brightness` (0–255, default 255), `white_point` (`#rrggbb`, default `#ffffff`), `brightness_mode` (`hardware` default, or `software`), `firmware_major` (optional override), `sync_gap_us` (0–20000, default 500). One output per interface. The sink is not implemented yet: the output is parsed and validated, then ignored with a warning. Multi-region maps come in M6. |
| `[http]` | `bind` (default `127.0.0.1:8080`) |
| `[[scene]]` | `name`, `[[scene.zone]]` with `rect = { x, y, w, h }`, `widget`, optional `background = "#rrggbb"` |

Widgets:
- `clock`: `format` (strftime, default `%H:%M`), `font`, `color`, `align`
- `text`: `text`, `font`, `color`, `align` (`left`/`center`/`right`), `scroll_px_per_s` (scrolls right to left when set)

Fonts: `4x6`, `5x7`, `5x8`, `6x9`, `6x10` (default), `6x12`, `6x13`, `7x13`, `7x14`, `8x13`, `9x15`, `9x18`, `10x20`, plus `_bold` variants of most. All are ISO-8859-1, so `°` works.

`run` shows `--scene` or the first scene. Rotation arrives in M2.

## Development

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
