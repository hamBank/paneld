# paneld

A Rust daemon that composes data from Home Assistant, HTTP APIs, Prometheus and pushed messages into scenes, and drives HUB75 LED panels through a Colorlight 5A-75B card over raw Ethernet.

User documentation is in [`docs/`](docs/README.md). Architecture, decisions and the roadmap are in [`DESIGN.md`](DESIGN.md).

## Status

**M0 (skeleton) is done.** It includes the config parsing and validation, canvas, render loop, HTTP preview sink, `clock` and `text` widgets, and the `run`, `check` and `render` commands. It runs on a laptop without hardware.

**M1 (hardware) is code-complete, but untested on a real card.** It adds `colorlight-proto` (packet encoding), `colorlight-net` (Linux raw-socket transport, discovery), the Colorlight sink behind `[[output]] type = "colorlight"`, `paneld probe` and `paneld test-pattern --interface`. Bring-up on the Pi follows [`deploy/README.md`](deploy/README.md), which also covers the systemd service (`deploy/paneld.service`, `deploy/env.example`, `deploy/install.sh`); the result (firmware version, port mapping, colour order) is still to be recorded, see `DESIGN.md` §15. The sink only works on Linux; other platforms build a stub.

**M2 (scenes) is done:**
- playlist rotation with conditions;
- `value`, `sparkline` and `image` (PNG/GIF) widgets;
- BDF fonts with `font_scale`;
- config hot reload.

**M3 (data) is done:** data sources (`static`, `aircraft` via adsb.fi, `home_assistant`, `mqtt` (plain TCP), `http_poll`, `prometheus`) run under a supervisor and restart on failure. Hot reload restarts only the sources whose config changed. Text widgets take templates like `{ha:sensor.x:.1}°C`, and `value` widgets can map weather conditions to icons.

**M4 (control) is done:**
- an HTTP control API (`/api/*`, optional bearer token);
- Home Assistant entities over MQTT discovery;
- a notification queue with interrupt and resume;
- quiet hours and auto-dim (schedule, sun, lux), with live brightness on the Colorlight card.

**M5 (ops) is done:**
- Prometheus `/metrics`;
- a hardened systemd unit, a [Debian/Ubuntu/Pi OS install guide](deploy/INSTALL-debian.md) and a [bring-up and service guide](deploy/README.md);
- Pi cross-compiling with a CI release build;
- `--log-format json`.

**M6 (scale) is done:**
- an output map from canvas regions to card space, with rotation and flip per region, e.g. an upside-down panel (see [Outputs: output map](docs/outputs.md#output-map));
- `render --output` and `test-pattern --card-space` for checking a map without hardware;
- multi-receiver discovery, with a `probe` table and layout warnings.

Addressing of daisy-chained cards is inferred from the protocol references, not confirmed on hardware; see `crates/colorlight-proto/PROTOCOL.md`.

**M7 (video) is done:**
- a `video` widget playing a file or a camera stream (`url_env`), decoded by an `ffmpeg` subprocess off the render thread;
- `ffmpeg` runs only while the scene is on screen.

**After M7:** a `system` source (uptime, CPU temperature, load, memory) and a `media` folder source for `image` widgets, both planned in `DESIGN.md` §6 but left out of the milestones.

**M8 (web control app) is done:**
- a small browser page at `/app`, served by paneld, with a live preview, status, power and brightness, scene pinning, notifications and value pushes (`docs/web-app.md`);
- unauthenticated like the API without `token_env`, with cross-site request and DNS rebinding protection.

All milestones in `DESIGN.md` §14 are built. What is left is bring-up and tuning on the real hardware (`DESIGN.md` §15).

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

`test-pattern` draws bring-up patterns (`bars`, `gradient`, `panel-ids`, `grid`, `solid`, `walk`) to a PNG with `--out`, or to the card with `--interface`; every option is in [the CLI reference](docs/control.md#command-line).

```bash
paneld probe --interface eth1            # list cards: model, firmware, reported size
paneld test-pattern --width 172 --height 172 --panel-height 86 --pattern panel-ids --interface eth1
```

`probe` prints each receiver's index, model, firmware version, reported width x height and whether it needs double sync, and exits non-zero if nothing answers (`--timeout-ms`, default 500, is the wait per request).

`render` also takes `--time <RFC 3339>` and `--elapsed <seconds>` for reproducible output.

`render --output <n>` writes the card-space framebuffer of colorlight `[[output]]` number `n` (counting every `[[output]]`, as in config error messages) instead of the canvas: what the card is sent, before gamma, white point and colour order, like the preview. Use it to check an output map without hardware.

## Documentation

The full reference is in [`docs/`](docs/README.md):

- [Configuration reference](docs/configuration.md): every section and key, with defaults, limits and what reloads live.
- [Sources](docs/sources.md): Home Assistant, MQTT, HTTP poll, Prometheus, aircraft and static values, and the keys each one writes.
- [Scenes and widgets](docs/scenes.md): layout, widgets, fonts, templates, conditions, the playlist and notifications.
- [Outputs](docs/outputs.md): the preview, the Colorlight card, the output map and brightness.
- [Control interfaces](docs/control.md): the HTTP API, Home Assistant over MQTT, metrics, live reload, signals, the CLI and environment variables.

Keep personal settings (your location, entity IDs) in a git-ignored `config/local.toml`.

## Building for the Pi

The target is `aarch64-unknown-linux-gnu` (Raspberry Pi OS 64-bit). Release builds use thin LTO, one codegen unit and stripped symbols (see `[profile.release]` in `Cargo.toml`), which takes the binary from about 11.7 MiB to 6.8 MiB. Build times are longer than a default release build.

**Plain cargo** on Debian or Ubuntu (an x86_64 or arm64 host):

```bash
sudo apt-get install gcc-aarch64-linux-gnu
rustup target add aarch64-unknown-linux-gnu
cargo build --release --locked --target aarch64-unknown-linux-gnu -p paneld
# -> target/aarch64-unknown-linux-gnu/release/paneld
```

`.cargo/config.toml` sets the linker to `aarch64-linux-gnu-gcc`. The cross C compiler is also needed by `ring` (TLS), whose build script finds it by itself; no `CC_*` variable is required.

**`cross`** (Docker or Podman, no host toolchain needed):

```bash
cargo install cross --locked
cross build --release --locked --target aarch64-unknown-linux-gnu -p paneld
```

No `Cross.toml` is needed: cross's aarch64 image already contains the cross gcc ([cross-rs/cross](https://github.com/cross-rs/cross)).

**glibc.** The binary links dynamically against glibc (`libc.so.6`, `libm.so.6`, `libgcc_s.so.1`; no OpenSSL) and needs glibc 2.34 or newer when built with Ubuntu 24.04's cross toolchain. Raspberry Pi OS 12 (Bookworm, glibc 2.36) and 13 (Trixie, glibc 2.41) are fine; Raspberry Pi OS 11 (Bullseye, glibc 2.31) is too old. Check a binary with:

```bash
aarch64-linux-gnu-readelf -V target/aarch64-unknown-linux-gnu/release/paneld | grep -o 'GLIBC_[0-9.]*' | sort -uV | tail -1
```

You can also build on the Pi itself with a plain `cargo build --release --locked -p paneld`.

### Release artifacts

`.github/workflows/release.yml` builds the aarch64 binary on every push to `main` and every `v*` tag (not on PRs). The workflow run has an artifact `paneld-aarch64-unknown-linux-gnu` with the binary (also named `paneld-aarch64-unknown-linux-gnu`) and `paneld-aarch64-unknown-linux-gnu.sha256`. For a `v*` tag, which must equal `v` plus the `paneld` crate version, both files are also attached to the GitHub release. The build fails if the binary needs a glibc newer than 2.36.

```bash
sha256sum -c paneld-aarch64-unknown-linux-gnu.sha256
chmod +x paneld-aarch64-unknown-linux-gnu   # artifact downloads lose the execute bit
```

## Logging

Logs go to stdout. `RUST_LOG` sets the level (default `info`). `--log-format json` (or `PANELD_LOG_FORMAT=json`) writes one JSON object per line for log shippers; keep the default `text` under journald. Secrets and source URLs are never logged. See [Control interfaces: environment variables](docs/control.md#environment-variables).

## Development

CI (`.github/workflows/ci.yml`) runs all of the following on every push to `main` and every PR, plus an aarch64 check for the Pi, a macOS check and an MSRV (Rust 1.85) check. `release.yml` builds the Pi binary (see above).

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
