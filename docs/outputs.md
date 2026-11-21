# Outputs

The engine renders one logical **canvas** per frame (`[canvas]`, at `fps` frames per second). Each `[[output]]` takes the latest frame. Outputs never slow the render loop down: one that falls behind just skips frames. Keys are listed in the [configuration reference](configuration.md#output).

## Preview

```toml
[[output]]
type = "preview"
scale = 4
grid = true
```

The preview is served on the `[http] bind` address:
- `GET /preview` is a small page that reloads the image at the canvas fps, at most 10 times a second.
- `GET /preview.png` is the current frame, each pixel scaled to `scale × scale`, with dark gaps when `grid` is on.

It shows the true colours the scenes asked for, before gamma, white point, colour order and dimming. It is never behind the API token. At most one preview output is allowed. When an aircraft source is configured, the page shows the data provider's credit.

## Colorlight 5A-75B

```toml
[[output]]
type = "colorlight"
interface = "eth1"
gamma = 2.2
brightness = 180
color_order = "rgb"
```

The card is driven over raw Ethernet on a dedicated network interface. This needs Linux and `CAP_NET_RAW`. On other platforms paneld builds, but the sink can't open the interface and keeps logging an error. Bring-up, the card's LEDVision settings and the systemd unit are in [`deploy/README.md`](../deploy/README.md). The wire format, with sources, is in [`crates/colorlight-proto/PROTOCOL.md`](../crates/colorlight-proto/PROTOCOL.md).

**What happens each frame.**
1. The canvas goes through the output map into card space.
2. Brightness, gamma and the white point are applied (one lookup table), then the colour order.
3. The sink sends, in order:
   1. a brightness packet;
   2. one row packet per card row (rows wider than 497 px are split);
   3. a pause of `sync_gap_us`;
   4. the display frame.

   On firmware 13 and later the brightness and display packets are sent twice.

   If a row fails to send, the display frame is skipped, so the card keeps the last complete picture.

**Discovery.** At start the sink asks the card for its model, firmware and size.
- The firmware decides single or double sync.
- If `firmware_major` is set, it wins: a different reported version only logs a warning.
- If nothing answers and `firmware_major` is not set, the sink assumes firmware below 13 and logs a warning.
- If the reported size differs from the card space, a warning is logged.
- `paneld probe --interface eth1` prints what every receiver on the port reports.

**Failures.** If the interface goes away, the sink reopens it with backoff. `paneld_sink_errors_total`, `paneld_card_detected` and the Home Assistant `card` sensor show its health.

**Stopping.** When paneld stops, it stops sending, and the card keeps showing the last frame it received. Send black first if that matters: `POST /api/power {"on": false}`, wait a frame, then stop.

**Tuning.** Every setting here except `interface` applies live when you save the config, so you can adjust gamma, colour order or the regions while looking at the panel (see [live reload](control.md#live-reload)).
- **`color_order`:** `paneld test-pattern --pattern bars --interface eth1` draws eight bars, left to right: white, yellow, cyan, green, magenta, red, blue, black. If they come out in other colours, try another `--color-order` until they match, then set that in the config.
- **`gamma`:** 2.2 suits most panels.
- **`white_point`:** use it if white looks blue or yellow.
- **`brightness`:** the panel's calibrated maximum.

## Output map

By default the whole canvas goes to the card at (0,0), which is right when the panels are wired as one picture. `[[output.region]]` maps canvas rectangles into **card space** (the card's own rows and columns), optionally rotated and flipped. Use it, for example, when a panel is mounted upside down or the card's panels are not in canvas order.

Two 172×86 panels stacked into a 172×172 canvas, the bottom one upside down:

```toml
[canvas]
width = 172
height = 172

[[output]]
type = "colorlight"
interface = "eth1"
card_size = { w = 172, h = 172 }      # keys of the output go before its regions

[[output.region]]                      # top panel, as it is
canvas = { x = 0, y = 0, w = 172, h = 86 }

[[output.region]]                      # bottom panel, turned 180 degrees
canvas = { x = 0, y = 86, w = 172, h = 86 }
card_offset = { x = 0, y = 86 }
rotate = 180
```

**How a region is placed.**
- A region copies its `canvas` rectangle, applies **`flip` first**, then **`rotate`**, and puts the result with its top-left corner at `card_offset`.
  - `flip = "horizontal"` mirrors left and right, and `"vertical"` top and bottom, of the region as it looks on the canvas.
- `rotate` is **clockwise**:
  - `90` moves the top edge to the right side;
  - `180` turns the region upside down;
  - `270` moves the top edge to the left side.

  With 90 or 270 the footprint on the card is `h × w`.

**Rules.**
- Regions must lie inside the canvas and must not overlap **in card space**. They may overlap on the canvas, so one area can be shown twice.
- Errors name the output and the region, e.g. `output[0] (colorlight) region[1]: overlaps region[0] in card space`.
- `card_size` is the card's whole display space. It defaults to the smallest box from the origin that holds every region.
- Card pixels no region covers are sent **black** every frame, because the card keeps whatever it last received.
- One output is one network interface with one card space. Daisy-chained cards share the coordinate space of the chain (see `PROTOCOL.md`; not yet confirmed on hardware).

**Checking a map:**
- **Without hardware:** `paneld render --config c.toml --scene s --output 0 --out card.png` writes the card-space image.
- **On the card:** `paneld test-pattern --config c.toml --output 0 --interface eth1 --pattern panel-ids`.
  - Add `--card-space` to draw in card coordinates and bypass the regions.
  - If a pattern looks right in card space but wrong through the map, a region is mis-set.

## Brightness and dimming

Four things decide how bright a Colorlight output is:

```text
level           = user brightness × auto-dim / 255
card brightness = configured brightness × level / 255     (0 while powered off)
```

- **Configured `brightness`** (per output, 0–255): the calibrated maximum.
- **User brightness and power**:
  - They are set with [`POST /api/brightness`](control.md#post-apibrightness), [`POST /api/power`](control.md#post-apipower) or the Home Assistant light. They start at 255 and on at every start-up.
  - Power off sends black with brightness 0. Frames keep flowing, so turning back on is instant.
- **Auto-dim** (0–255, 255 = none): the **minimum** of whichever of these are configured:
  - `[quiet_hours]`: its `brightness` inside the window, 255 outside.
  - `[auto_dim.sun]`:
    - 255 by day and `night` at night;
    - between them, a linear ramp over `ramp_min` minutes after sunset and before sunrise;
    - computed with NOAA's solar equations;
    - at polar latitudes, a day without sunrise is `night` and a day without sunset is 255.
  - `[auto_dim.lux]`: the lux reading mapped through `curve`, linear between points and flat beyond the ends. A missing reading, or one older than `stale_after_s`, is ignored, so the other inputs decide.

  It is re-evaluated every 30 s (every 5 s with a lux sensor) and only applied when it changes.

With `brightness_mode = "hardware"` the card brightness goes into the card's packets. With `"software"` the card is told 255 and the colour lookup table is scaled instead.

`GET /api/state` and `/metrics` report the user brightness and the dim factor separately. The Home Assistant light shows the user brightness.
