# Web control app

A small web page served by paneld itself. It is a front end for the [HTTP control API](control.md#http-control-api): see what the panel is doing, and control it from a phone or laptop. It needs no install and no build step.

Open `http://<host>:<port>/` or `/app`, where `<host>:<port>` is `[http] bind` (default `http://localhost:8080/`). To use it from another device, set `[http] bind` to an address that device can reach, for example `0.0.0.0:8080` or the host's LAN address.

The page works on phones (one column) and desktops (two columns), and follows the system light or dark setting. Every control has a label for screen readers.

## Panels

| Panel | What it does |
|---|---|
| Preview | The live [preview](outputs.md#preview) image, refreshed twice a second while the page is visible. Shown only when a preview output is configured. |
| Status | The scene on screen (or `black`, or `notification`), whether a scene is pinned or the playlist runs (Auto), power, brightness, auto-dim, the effective brightness (brightness × auto-dim ÷ 255), frame rate and the notification queue. The notification showing now is highlighted. |
| Power and brightness | A power switch and a brightness slider, 0–255. The slider shows its value while you drag and sends it when you let go. |
| Scenes | One button per scene, and **Auto**. The pinned scene is highlighted. Pinning keeps a scene on screen until you press **Auto**; notifications still interrupt it. |
| Notify | Queue a [notification](scenes.md#notifications). Choose **Text** (with an icon and a colour) or **Scene**, then set the seconds, priority (−100 to 100) and repeat count (1–100). **Clear all** empties the queue. |
| Push a value | Write a value to the store, like [`POST /api/data/{key}`](control.md#post-apidatakey). The key starts with `<data_prefix>:`. A number is sent as a number, anything else as text. A unit applies to numbers only. **Remove** deletes the key. |

The page asks the daemon for its state once a second while visible, and every 10 seconds when the tab is hidden. A dot in the top right shows whether the last request worked. Errors from paneld appear as short messages at the bottom of the page. After a command the page refreshes at once, so changes show up within a moment.

## Security

The web app is **not authenticated**, like the API without `token_env`. Anyone who can reach `[http] bind` can control the panel and see its preview.
- Keep `[http] bind` on loopback (the default), or on a LAN you trust.
- Do not expose it to the internet. Put an authenticating reverse proxy in front if you need remote access.

Two protections are built in; [control.md](control.md#http-control-api) has the details:
- **Cross-site requests.** A write request that carries an `Origin` header from a different host is refused with 403, so another website can't drive the panel through your browser. `curl` and scripts send no `Origin` and are unaffected.
- **Loopback Host check.** When `[http] bind` is a loopback address, only `localhost`, `127.0.0.1` and `[::1]` are accepted as the `Host`, which blocks DNS rebinding.

The page loads no outside resources and runs under a strict `Content-Security-Policy`: scripts and styles come only from paneld itself.

## Turning it off

Set `app = false` in `[http]` to remove the page's routes. The API, the preview and the metrics stay.

```toml
[http]
bind = "127.0.0.1:8080"
app = false
```

## Limitations

- **No token support yet.** If `[http] token_env` is set, the page shows "This paneld requires an API token; the web app does not support tokens yet." Use the API directly, or leave `token_env` unset on a trusted network.
- **Polling.** The page polls instead of receiving pushes, so a change made elsewhere shows up within a second (10 seconds in a hidden tab).
- **No editing.** It does not edit scenes or the config. Edit the config file; paneld [reloads it live](control.md#live-reload).
