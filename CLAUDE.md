# CLAUDE.md

This repo is `paneld`: a Rust daemon that composes data from Home Assistant, HTTP APIs, Prometheus and pushed messages into scenes and drives HUB75 LED panels through a Colorlight 5A-75B card over raw Ethernet.

**Read `DESIGN.md` before making changes.** It is the source of truth for architecture, config schema and milestones. If you have to deviate from it, update `DESIGN.md` in the same change and say why.

## Working order
- Follow the milestones in `DESIGN.md` §14 in order. Don't start a later milestone's features early.
- Each milestone is its own branch and PR. While the previous milestone's PR is open, branch from it and target the PR at its branch, so the diff shows one milestone. Retarget to `main` once the previous PR merges.
- Check §15 (open questions) and ask rather than guess when a question there blocks the work.

## Hard rules
- **Colorlight protocol:** never write packet layouts from memory. Confirm them against the references in `DESIGN.md` §4.1, record them in `crates/colorlight-proto/PROTOCOL.md` with links, and cover them with byte-exact tests.
- The render loop must never block on I/O. Sources write to the store; the render loop only reads snapshots.
- No test may need root, a network interface or hardware. Raw-socket code sits behind `cfg(target_os = "linux")` and has a stub on other platforms.
- Secrets (HA token, MQTT password) only come from env vars named in config (`*_env`). Never log them or commit them.
- Keep dependencies lean. Add a crate only when it clearly earns its place, and note why in the PR/commit message.

## Commands
```bash
cargo build
cargo test --workspace
cargo clippy --workspace --all-targets -- -D warnings
cargo fmt --all
UPDATE_GOLDEN=1 cargo test -p paneld golden   # regenerate golden images, then review the diffs
cargo run -p paneld -- run --config config/example.toml   # preview at http://localhost:8080/preview
```

Cross-compile for the Pi: `cross build --release --target aarch64-unknown-linux-gnu`.

## Conventions
- Rust stable, edition 2021 or newer. `anyhow` in the binary, `thiserror` in library crates.
- Async: `tokio`. HTTP: `axum`. MQTT: `rumqttc`. Graphics: `embedded-graphics`. Logging: `tracing`.
- Every change keeps `fmt`, `clippy -D warnings` and tests green.
- When adding config fields, update `config/example.toml`, the validation code and the config tests together.
- Golden image changes must be intentional. Look at them before committing.
