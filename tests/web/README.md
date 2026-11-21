# Web app smoke test

A Playwright test for the web control app (`/app`). It starts a real `paneld` with `smoke.toml` on `127.0.0.1:18090`, so it needs no root, hardware or outside network.

```bash
cargo build -p paneld
cd tests/web
npm ci
npx playwright install chromium     # once
node smoke.mjs                      # from the repo root: node tests/web/smoke.mjs
```

`PANELD_BIN` sets the binary (default `target/debug/paneld`, relative to the current directory). CI runs this as the `web-smoke` job.
