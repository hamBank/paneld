# Bring-up on the Raspberry Pi

How to get two 172x86 panels lit from a Colorlight 5A-75B. Run `paneld test-pattern` and `paneld probe` from a shell first (sections 1 to 4); only then put the card in `config.toml` and install the daemon as a systemd service (section 5). Section 6 is troubleshooting.

Nothing here has been run against real hardware yet, and the systemd unit has so far only passed `systemd-analyze verify`, not a boot on a Pi. The settings below are what the design assumes, and the procedure is how to confirm them. Write down what you find (see "Record the result").

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

The card sits on its own NIC (a USB 3 Gigabit adapter, or the Pi's onboard port with the LAN on Wi-Fi). Raw Ethernet needs no IP address, and giving it one only lets the OS send DHCP, router-solicitation and mDNS noise to the card.

Raspberry Pi OS Bookworm and later use NetworkManager for networking; it replaced dhcpcd as the default in Bookworm ([Raspberry Pi: "Bookworm - the new version of Raspberry Pi OS"](https://www.raspberrypi.com/news/bookworm-the-new-version-of-raspberry-pi-os/)). On older images that still use dhcpcd, add `denyinterfaces eth1` to `/etc/dhcpcd.conf` instead.

Create an explicit profile for the card NIC with IPv4 and IPv6 turned off. `ipv4.method disabled` and `ipv6.method disabled` are documented values ([nm-settings-nmcli: `ipv4.method`, `ipv6.method`](https://networkmanager.dev/docs/api/latest/nm-settings-nmcli.html); "disabled" - "Disables the IPv4 protocol in this connection profile"). Replace `eth1` with your interface (`nmcli device status` lists them):

```bash
sudo nmcli connection add type ethernet ifname eth1 con-name paneld-card \
    ipv4.method disabled ipv6.method disabled connection.autoconnect yes
sudo nmcli connection up paneld-card
```

If NetworkManager already made an automatic profile for it (usually called "Wired connection 1"), turn that one off so it cannot win:

```bash
sudo nmcli connection modify "Wired connection 1" connection.autoconnect no
nmcli connection show            # paneld-card should be active on eth1
```

Check: `ip -br addr show eth1` shows `UP` and no address, and `ip link show eth1` says `state UP`. If it is `DOWN` with the card powered and cabled, `sudo ip link set eth1 up` (NetworkManager should do this by itself). Use a gigabit link; the card needs one. Syntax and property names are in [`nmcli`](https://networkmanager.dev/docs/api/latest/nmcli.html); NetworkManager does not need to be restarted.

Notes:

- Which interface name you get is not stable across reboots or adapters; `ifname` is only a match. If you plug in two USB NICs, pin the profile to the MAC address instead (`802-3-ethernet.mac-address`), and use whatever name the kernel then gives the interface in `[[output]] interface`.
- The unit does not wait for this NIC (see "How the unit is ordered" below). It only has to appear before paneld can find the card.

## 3. Permissions

Raw sockets need `CAP_NET_RAW`, not root. For a quick test from a shell (not for the service, which gets the capability from the unit):

```bash
sudo setcap cap_net_raw+ep target/release/paneld
```

Redo this after every rebuild, since a new binary loses the capability. Without it, `probe` and the daemon fail with "permission denied opening a raw socket". Build with `cross build --release --target aarch64-unknown-linux-gnu` on a workstation, or on the Pi (see the repository's cross-compile notes for the exact setup).

## 4. Procedure

Use a modest brightness while you work; `test-pattern` defaults to 64 of 255 and gamma 1.0 (raw values). Replace `eth1` with your interface. All `test-pattern` runs send continuously until Ctrl-C.

**a. Probe.**

```bash
paneld probe --interface eth1
```

Expect a table with one row per receiver (index, firmware major.minor, the size it reports, offset), then the card space the port expects and whether it needs double sync. Exit status is non-zero if nothing answers: check the cable, the interface, that the card is powered, and the firmware. The reported size is only a sanity check (FPP warns it is not always accurate).

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

## 5. Install as a service (systemd)

Do the bring-up above first; the service uses the settings you found there.

### What paneld needs on disk

Almost nothing, and nothing is written at runtime:

- the **config file**, which paneld re-reads once a second (hot reload) and on SIGHUP;
- **fonts** (`[[font]] path`) and **images** (`path` of `image` widgets), resolved **relative to the config file's directory**, and read when the config loads or reloads;
- state is in memory only: the data store, the route cache and Home Assistant series (re-fetched after a restart). `paneld render` and `test-pattern --out` write PNGs, but only when you tell them to.

So the service runs with the whole filesystem read-only (`ProtectSystem=strict`) and has no state directory.

Layout:

```
/usr/local/bin/paneld
/etc/systemd/system/paneld.service
/etc/paneld/config.toml       0644 root:root   (no secrets, see below)
/etc/paneld/env               0600 root:root   (secrets; read by systemd, not by paneld)
/etc/paneld/assets/fonts/...  0644             (BDF fonts)
/etc/paneld/assets/*.png      0644             (images used by scenes)
```

`config/example.toml` refers to `../assets/...` because it lives in the repository's `config/` directory. In `/etc/paneld` those paths must point inside the directory (`assets/fonts/...`), or they would resolve to `/etc/assets`. Fonts and images are read by the service user, so they must be world-readable.

### Install

```bash
# 1. binary (built on a workstation with `cross`, or on the Pi with cargo)
sudo install -m 755 target/aarch64-unknown-linux-gnu/release/paneld /usr/local/bin/paneld
#    on the Pi itself: target/release/paneld

# 2. config and assets
sudo install -d -m 755 /etc/paneld /etc/paneld/assets/fonts
sudo cp -r assets/. /etc/paneld/assets/
sudo sed 's#\.\./assets/#assets/#g' config/example.toml | sudo tee /etc/paneld/config.toml >/dev/null
sudoedit /etc/paneld/config.toml      # colorlight output, interface, colour order, gamma, scenes
sudo chmod 644 /etc/paneld/config.toml
paneld check --config /etc/paneld/config.toml

# 3. secrets (only if the config names *_env variables)
sudo install -m 600 -o root -g root deploy/env.example /etc/paneld/env
sudoedit /etc/paneld/env              # real values; delete the lines you do not use

# 4. unit
sudo install -m 644 deploy/paneld.service /etc/systemd/system/paneld.service
sudo systemctl daemon-reload
systemd-analyze verify /etc/systemd/system/paneld.service
sudo systemctl enable --now paneld
```

`deploy/install.sh` does all of this except editing (see "Install script" below). `paneld check` validates the config only; the `*_env` variables are only checked by `run`.

Secrets: the config only contains the **names** of environment variables (`token_env`, `password_env`, `header_env`, `bearer_env`), and `deploy/env.example` lists them. systemd reads `/etc/paneld/env` as root and puts the values in paneld's environment, which is why it can stay `0600 root:root` while the service runs as an unprivileged dynamic user. The line is `EnvironmentFile=-/etc/paneld/env`, so a missing file is not an error by itself; paneld then refuses to start if the config needs a variable that is not set, and the journal names it.

### Day to day

```bash
systemctl status paneld
journalctl -u paneld -f                    # follow the log
journalctl -u paneld -b --since "10 min ago"
sudo systemctl reload paneld               # SIGHUP: re-read config.toml now
sudo systemctl restart paneld              # needed for canvas.fps, [http], [mqtt], [[output]] changes
```

Config changes are picked up by themselves within about 2 seconds; `reload` just makes it immediate (a rejected config is logged and the old one keeps running). `canvas.fps`, `[http]`, `[mqtt]`, `[[output]]` settings and the environment file need a **restart** (`systemctl restart`), because systemd only reads `EnvironmentFile=` at start.

Log level: add `Environment=RUST_LOG=info,paneld=debug` through `sudo systemctl edit paneld` (a drop-in; keeps the shipped unit untouched), then restart.

### Check that CAP_NET_RAW works for the service user

Run `probe` under the same identity and sandbox as the unit, without touching the running service (stop it first if it holds the interface; two raw sockets on the card are fine for `probe`, but a running `paneld` will also keep sending frames):

```bash
sudo systemd-run --pty --wait --collect --quiet \
    -p DynamicUser=yes \
    -p AmbientCapabilities=CAP_NET_RAW -p CapabilityBoundingSet=CAP_NET_RAW \
    -p NoNewPrivileges=yes -p ProtectSystem=strict -p PrivateDevices=yes \
    -p RestrictAddressFamilies="AF_UNIX AF_INET AF_INET6 AF_PACKET AF_NETLINK" \
    -p SystemCallFilter=@system-service -p SystemCallArchitectures=native \
    /usr/local/bin/paneld probe --interface eth1
```

(`systemd-run -p` accepts the same assignments as the unit file, see [`systemd-run`](https://www.freedesktop.org/software/systemd/man/latest/systemd-run.html).) It should print the card as in bring-up step "a". To compare with a plain unprivileged user, drop the two capability lines: `probe` then fails with "permission denied opening a raw socket".

For the real unit, look at the log: the log has one `colorlight: found receiver` line per card (firmware and reported size); a missing capability or a sandbox rule shows up in `journalctl -u paneld`. With MQTT discovery on, the card `binary_sensor` in Home Assistant turns ON once frames are being sent.

### How the unit is ordered

`After=network-online.target` + `Wants=network-online.target` make the unit start once the **LAN** is up, which matters for the Home Assistant, MQTT, HTTP and Prometheus sources. The card NIC has no IP configuration and is not part of what `network-online.target` waits for in a way you can rely on, so the unit does not depend on it: the sink re-opens the raw socket with backoff (1 s up to 10 s) until the interface exists and the card answers, and the card's `binary_sensor` in Home Assistant shows when it does. Starting the daemon before the card is powered is fine; it will pick the card up.

### Upgrading

```bash
sudo install -m 755 new-paneld /usr/local/bin/paneld     # atomic replace of the file
sudo systemctl restart paneld
journalctl -u paneld -n 50 --no-pager                    # check it found the card
```

The panel goes dark for a second during the restart. Keep the previous binary (`paneld.prev`) if you want a one-line rollback. If the unit file changed in the new release: copy it, `sudo systemctl daemon-reload`, then restart. Config compatibility: run `paneld check --config /etc/paneld/config.toml` with the new binary before restarting.

### Install script

`deploy/install.sh` is optional. It copies the binary and the unit, creates `/etc/paneld`, installs the example config and env file **only if they do not exist yet**, and never overwrites your edited files. It does not start anything with secrets, download anything or edit the network configuration.

```bash
sudo deploy/install.sh target/aarch64-unknown-linux-gnu/release/paneld
sudo systemctl enable --now paneld
```

For a dry run into a scratch directory, `DESTDIR=/tmp/root deploy/install.sh ./paneld` (no root, and `systemctl daemon-reload` is skipped).

## 6. Troubleshooting

| Symptom | Likely cause and fix |
|---|---|
| `permission denied opening a raw socket` (EPERM) | Without the service: the binary has no `CAP_NET_RAW` (section 3). In the unit: `AmbientCapabilities=CAP_NET_RAW` missing or edited out, or `RestrictAddressFamilies` lacks `AF_PACKET` (`systemctl cat paneld` shows what is really loaded). |
| Unit dies at once, `status=31/SYS` | A system call is blocked by `SystemCallFilter`. Comment out the `SystemCallFilter=` line (`sudo systemctl edit paneld`), restart, and if it then works, find the blocked call with `strace -f -c` and report it as a bug. |
| `status=203/EXEC` | `/usr/local/bin/paneld` missing or not executable, or wrong architecture (an x86-64 build on the Pi). |
| `status=1/FAILURE` straight away | A config or environment error: `journalctl -u paneld -n 30`. A missing `*_env` variable is named. Check `/etc/paneld/env` has no `export`, quotes or trailing comments. After 10 failed starts in 5 minutes the unit stays `failed` until `systemctl reset-failed paneld`. |
| Fonts or images not found | Paths are relative to the directory of `config.toml`. The service user needs read access on the files (`chmod -R a+rX /etc/paneld/assets`). `ProtectHome=yes` hides `/home`: do not keep assets there. |
| Firmware 13.x: flicker, or nothing on the panel | paneld sends the brightness and display packets twice on firmware >= 13, from the version it detects. That rule comes from FPP's source and has not been seen on hardware (DESIGN.md 4.2). Try `firmware_major = 11` or `13` on the `[[output]]` to force it, or flash 11.04. If the last rows tear, raise `sync_gap_us`. |
| No receivers found (`probe` exits non-zero) | Card unpowered or cable on the wrong NIC; interface not `UP` or still has an IP-less profile conflict (`nmcli connection show --active`); wrong `interface` name; Fast Ethernet link (the card needs gigabit); the USB adapter name changed after a reboot. Running `paneld probe` as the service user (above) separates a permissions problem from a wiring problem. |
| Panels swapped, mirrored, wrong order | Use the `panel-ids` pattern (section 4b), then LEDVision port order and direction. |
| Wrong colours | `bars` pattern (section 4d) and `color_order`; brightness and gamma in 4e. |
| Half the panel shifted or the middle line broken | `grid` pattern (section 4c), scan setting in LEDVision. |
| More than one receiver on the port: `probe` lists fewer than you chained | `probe` stops at the first receiver that doesn't answer, so one that is off, mis-cabled or numbered out of order hides all that follow. See "Several cards on one port". |
| `warning: receivers [...] run firmware >= 13 and receivers [...] older` | One sync count is sent for the whole port. Flash the chain to one firmware version. |

### Several cards on one port

Several 5A-75B cards can be daisy-chained on one NIC (the HK repo's README says to chain them, never to share a switch, because the cards use hardcoded MAC addresses). **Nothing about chains has been seen on hardware**: pixel addressing and latching on a chain are inferences from the protocol references (`crates/colorlight-proto/PROTOCOL.md`, "Multiple receivers"). paneld never configures the cards, so bring them up in this order:

1. Cable the NIC to the first card and the first card's second Ethernet port to the next, and so on. Do not use a switch.
2. In LEDVision, set each receiver's region (width, height and x/y position in the whole display) and save it to that card. paneld sends one coordinate space for the whole port and each card takes its own region from what LEDVision stored. Do this one card at a time if the chain misbehaves.
3. Run `paneld probe --interface eth1`. Check that every card is listed with the index you expect (0 is the first), that the firmware is the same major version (>= 13 vs older is warned about), and that "card space the port expects" is the total display you configured. The reported sizes and the offset column are not fully understood (offsets print as `unknown`); a mismatch is a hint, not proof.
4. Only then run `test-pattern` with the `panel-ids` and `grid` patterns at the total display size (section 4b and 4c) and look at where each card draws its region.

If `probe` stops early, power-cycle the missing card and check its cabling and that its receiver number in LEDVision is the next one in line (whether the number follows the chain position or the stored configuration is not documented).

Journal for one failed start: `journalctl -u paneld -b -e`. To see what the sandbox allows: `systemd-analyze security paneld.service`.
