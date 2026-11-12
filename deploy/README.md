# Bring-up on the Raspberry Pi

How to get two 172x86 panels lit from a Colorlight 5A-75B. Run `paneld test-pattern` and `paneld probe` from a shell first; only then put the card in `config.toml` and run the daemon.

Nothing here has been run against real hardware yet. The settings below are what the design assumes, and the procedure is how to confirm them. Write down what you find (see "Record the result").

## 1. Card settings (LEDVision, once, from Windows)

Set these in LEDVision and save them to the card. The daemon only sends pixels; it never configures the card.

| Setting | Value | Notes |
|---|---|---|
| Panel (cabinet) size | 172 x 86 | Per panel |
| Scan | 1/43 | 86 rows / 2 halves of 43 |
| Firmware | **11.04** preferred | 13.x also works, see below |
| Panel to HUB75 port | **one panel per port**, not chained | See "Why not chain" |
| Card size / layout | write down what LEDVision lets you set for two ports | Unconfirmed: whether it is 172x172 stacked or 172x86 per port. `probe` prints what the card reports. |

Also write down the data-group / colour wiring options for the panel if LEDVision shows them, since they decide the colour order found below.

**Firmware.** Raw senders were written against 11.x and older. Firmware 13.x needs the `0x0A` brightness and `0x01` display frames sent twice. `paneld` detects the version at startup and does this by itself, but that rule rests on FPP's source alone and has not been seen on hardware. Staying on 11.04 avoids the question. If discovery fails on your card, set `firmware_major` in the output config.

**Why not chain the two panels on one port.** Refresh rate: a chain pushes both panels' pixels through one set of data lines, so the time per refresh doubles, and with it flicker on camera and low-brightness banding. Fault isolation: in a chain, one bad panel or ribbon cable blanks or corrupts everything after it, which is hard to tell apart from a software bug. With one panel per port, a dark panel points straight at its own port.

## 2. The network interface

The card sits on its own NIC (a USB 3 Gigabit adapter, or the Pi's onboard port with the LAN on Wi-Fi). Raw Ethernet needs no IP address, and giving it one only lets the OS send DHCP and mDNS noise to the card.

- NetworkManager: `nmcli con mod "<connection name>" ipv4.method disabled ipv6.method disabled`, then `nmcli con up "<connection name>"`.
- dhcpcd: add `denyinterfaces eth1` to `/etc/dhcpcd.conf`.

The link must be up without an address: check `ip link show eth1` says `state UP` (otherwise `sudo ip link set eth1 up`). Use a gigabit link; the card needs one.

## 3. Permissions

Raw sockets need `CAP_NET_RAW`, not root:

```bash
sudo setcap cap_net_raw+ep target/release/paneld
```

Redo this after every rebuild, since a new binary loses the capability. Without it, `probe` and the daemon fail with "permission denied opening a raw socket". Cross-build with `cross build --release --target aarch64-unknown-linux-gnu` or build on the Pi.

## 4. Procedure

Use a modest brightness while you work; `test-pattern` defaults to 64 of 255 and gamma 1.0 (raw values). Replace `eth1` with your interface. All `test-pattern` runs send continuously until Ctrl-C.

**a. Probe.**

```bash
paneld probe --interface eth1
```

Expect one line per receiver with firmware major.minor, the size the card reports, and whether it needs double sync. Exit status is non-zero if nothing answers: check the cable, the interface, that the card is powered, and the firmware. The reported size is only a sanity check (FPP warns it is not always accurate).

**b. Panel order and mirroring: `panel-ids`.**

```bash
paneld test-pattern --width 172 --height 172 --panel-height 86 --pattern panel-ids --interface eth1
```

Each panel gets its own border colour, its index (0 = top, 1 = below it) and a white square in its top-left corner. Check that panel 0 is the top one, that the numbers read normally (not mirrored or upside down), and that the marker is top-left. If the panels are swapped or mirrored, fix it in LEDVision (port order, direction) before touching the config; the card offset in the output map comes in M6.

**c. Row placement and scan halves: `grid`.**

```bash
paneld test-pattern --width 172 --height 172 --panel-height 86 --pattern grid --interface eth1
```

Grey lines every 8 px, cyan lines on each panel's top and left edge, and a **yellow line at row 43 of each panel** (the boundary between the two scan halves). The yellow line must be one straight line on each panel. If it breaks into two offset pieces, or the top and bottom halves look swapped, the scan setting in LEDVision is wrong.

**d. Colour order: `bars`.**

```bash
paneld test-pattern --width 172 --height 172 --panel-height 86 --pattern bars --interface eth1 --color-order rgb
```

The bars run white, yellow, cyan, green, magenta, red, blue, black from left to right. Compare. If a bar has the wrong colour (red shown as blue, say), try the other orders (`rbg`, `grb`, `gbr`, `brg`, `bgr`) until all eight match. Put the winner in the config as `color_order`.

**e. Gamma: `gradient`.**

```bash
paneld test-pattern --width 172 --height 172 --panel-height 86 --pattern gradient --interface eth1 --color-order <found> --gamma 1.0
paneld test-pattern --width 172 --height 172 --panel-height 86 --pattern gradient --interface eth1 --color-order <found> --gamma 2.2
```

The pattern has red, green, blue and white ramps from black. At gamma 1.0 the ramps look too bright and squeezed to the left; pick the gamma at which the steps look even, usually between 1.8 and 2.6. Then put it into the config as `gamma`.

**f. Run it.**

Uncomment the colorlight output in `config/example.toml` (or your own config), set `interface`, `color_order`, `gamma` and `brightness`, and:

```bash
paneld run --config config.toml
```

The log says which receivers were found and whether double sync is on. If the picture flickers on firmware 11, check that discovery did not report 13; if the last rows look partly lit or torn, raise `sync_gap_us` (the display frame may be latching before the last row arrives; the default is 500).

## Record the result

Once it works, write these down in `DESIGN.md` §15 (they are open questions until then):

- firmware version of the card (from `probe`)
- which HUB75 port each panel is on, and anything LEDVision needed to get the order right
- the colour order and gamma that look right

## systemd unit (M5, not written yet)

A unit file with `AmbientCapabilities=CAP_NET_RAW`, `CapabilityBoundingSet=CAP_NET_RAW`, a dedicated or dynamic user, `EnvironmentFile=/etc/paneld/env` and `Restart=on-failure` is planned for milestone M5 (`DESIGN.md` §11, §14). Until then run `paneld` from a shell.
