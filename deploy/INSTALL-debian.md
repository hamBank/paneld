# Installing paneld on a Debian-based system

How to install paneld as a service on Debian, Ubuntu or Raspberry Pi OS, on arm64 (Raspberry Pi 4/5) or amd64 (any PC).

This guide covers getting the binary, the card's network port, the files and the service. [`README.md`](README.md) in this directory covers the Colorlight side:
- the one-off LEDVision card settings (§1);
- bring-up with test patterns (§4);
- the service in more depth (§5);
- troubleshooting (§6).

## Supported systems

| System | glibc | systemd | Rust from apt | Notes |
|---|---|---|---|---|
| Debian 12 (bookworm), Raspberry Pi OS (bookworm) | 2.36 | 252 | 1.63: too old, use rustup | |
| Debian 13 (trixie) | 2.41 | 257 | 1.85.1: new enough | |
| Ubuntu 22.04 (jammy) | 2.35 | 249 | 1.75: too old, use rustup | |
| Ubuntu 24.04 (noble) | 2.39 | 255 | 1.75: too old, use rustup | |

Versions were checked on 2026-11-15 on packages.debian.org and packages.ubuntu.com. They matter for three reasons:

- **Building from source:** paneld needs Rust 1.85 or newer, its minimum supported version.
- **Prebuilt arm64 binaries** from the release workflow need glibc 2.34 or newer. CI refuses to publish one that needs more than 2.36 (bookworm).
- **The systemd unit:** its newest directive is `ProtectClock=`, added in systemd 245. That version comes from the "Added in version" notes in the [systemd.exec](https://www.freedesktop.org/software/systemd/man/latest/systemd.exec.html) man page. Every system in the table has a newer systemd, so the unit works unchanged on all of them.

Older releases may work but are not covered: Debian 11 has glibc 2.31, which is too old for the prebuilt binary.

You also need:
- a **dedicated Ethernet port** for the card. A USB 3 gigabit adapter works; the card needs a gigabit link.
- a second connection for the LAN: onboard Ethernet or Wi-Fi.
- a card already set up once in LEDVision (see [`README.md`](README.md) §1).

## 1. Packages

```bash
sudo apt update
sudo apt install -y ca-certificates curl git libcap2-bin
```

Optional, only if the config has `video` zones ([video widget](../docs/scenes.md#video)):

```bash
sudo apt install -y ffmpeg
```

To build from source on this machine, also install a C compiler and linker. `ring`, the TLS library, compiles some C code.

```bash
sudo apt install -y build-essential
```

`libcap2-bin` provides `setcap`/`getcap`, which are only needed if you want to run paneld by hand as a normal user. The service gets its capability from systemd.

## 2. Get the binary

Pick one of these.

### a) Prebuilt (arm64 only)

The release workflow builds `paneld-aarch64-unknown-linux-gnu` and a `.sha256` file:
- for every `v*` tag, as a GitHub release;
- for every push to `main`, as a workflow artifact.

Download both files into one directory, then:

```bash
sha256sum -c paneld-aarch64-unknown-linux-gnu.sha256
chmod +x paneld-aarch64-unknown-linux-gnu
./paneld-aarch64-unknown-linux-gnu --version
```

If you see `GLIBC_2.xx not found`, the OS is older than the table above. Build on the machine instead (b).

### b) Build on the machine (arm64 or amd64)

Debian 12 and Ubuntu ship a Rust older than 1.85, so install the toolchain with [rustup](https://rustup.rs). This installs into your home directory and needs no root:

```bash
curl --proto '=https' --tlsv1.2 -sSf https://sh.rustup.rs | sh -s -- -y --profile minimal
. "$HOME/.cargo/env"
```

On Debian 13 you can use the distribution's Rust instead: `sudo apt install -y rustc cargo` (1.85.1).

Then build:

```bash
git clone https://github.com/hamBank/paneld.git
cd paneld
cargo build --release --locked -p paneld
./target/release/paneld --version
```

The binary is `target/release/paneld`. The release profile uses link-time optimisation, so the final link takes a while and uses a fair amount of memory. On a Pi with little RAM, cross-compiling (c) is the faster option.

### c) Cross-compile for the Pi from a Debian/Ubuntu PC

```bash
sudo apt install -y build-essential gcc-aarch64-linux-gnu
rustup target add aarch64-unknown-linux-gnu
cargo build --release --locked --target aarch64-unknown-linux-gnu -p paneld
scp target/aarch64-unknown-linux-gnu/release/paneld pi@<pi-address>:
```

The repository's `.cargo/config.toml` already sets the linker to `aarch64-linux-gnu-gcc`. `cross build --release --target aarch64-unknown-linux-gnu` also works, using Docker.

## 3. The card's network port

The card speaks raw Ethernet, not IP. Its port must get **no IP configuration**, so the OS doesn't send DHCP and IPv6 traffic at it. It must still be **up**.

Find the port's name with `ip -br link`. USB adapters often get names like `enx001122334455` rather than `eth1`. Use the same name in `[[output]] interface` in the config.

Which tool configures the network depends on the system:

```bash
systemctl is-active NetworkManager systemd-networkd networking
ls /etc/netplan 2>/dev/null
```

The examples below use `eth1`. Replace it with your port's name.

### NetworkManager (Raspberry Pi OS bookworm, Ubuntu Desktop, Debian with a desktop)

```bash
sudo nmcli connection add type ethernet ifname eth1 con-name paneld-card \
    ipv4.method disabled ipv6.method disabled connection.autoconnect yes
sudo nmcli connection up paneld-card
```

`disabled` is a documented value of both properties; see [nm-settings-nmcli](https://networkmanager.dev/docs/api/latest/nm-settings-nmcli.html). If an automatic "Wired connection" profile also matches the port, turn it off. [`README.md`](README.md) §2 has the details.

### netplan with systemd-networkd (Ubuntu Server)

Create `/etc/netplan/60-paneld-card.yaml`:

```yaml
network:
  version: 2
  renderer: networkd
  ethernets:
    eth1:
      dhcp4: false
      dhcp6: false
      link-local: []     # an empty list turns off IPv4 and IPv6 link-local addresses
      accept-ra: false   # ignore IPv6 router advertisements
      optional: true     # don't hold up boot waiting for this port
```

Apply it:

```bash
sudo chmod 600 /etc/netplan/60-paneld-card.yaml
sudo netplan try      # reverts after 120 s unless you confirm
```

The keys are described in the [netplan YAML reference](https://netplan.readthedocs.io/en/stable/netplan-yaml/).

### ifupdown (Debian server or minimal installs, `/etc/network/interfaces`)

Create `/etc/network/interfaces.d/paneld-card`:

```
allow-hotplug eth1
iface eth1 inet manual
    up ip link set dev $IFACE up
    down ip link set dev $IFACE down
```

The `manual` method configures no address ([interfaces(5)](https://manpages.debian.org/bookworm/ifupdown/interfaces.5.en.html)), so the `up` and `down` lines bring the link up themselves.

Keep IPv6 off that port too. Create `/etc/sysctl.d/60-paneld-card.conf`:

```
net.ipv6.conf.eth1.disable_ipv6 = 1
```

Then apply both:

```bash
sudo sysctl --system
sudo ifup eth1
```

Make sure `/etc/network/interfaces` contains `source /etc/network/interfaces.d/*`, as Debian's default file does.

### Check

```bash
ip -br addr show eth1     # UP, with no inet/inet6 addresses
```

With the card powered and cabled, the link should be up at 1000 Mb/s. `cat /sys/class/net/eth1/speed` prints `1000`.

## 4. Install the files

From the repository checkout, pass the binary from step 2:

```bash
sudo deploy/install.sh target/release/paneld
```

The script is idempotent and safe to re-run on upgrade. It does the following:
- installs the binary as `/usr/local/bin/paneld`;
- installs the unit as `/etc/systemd/system/paneld.service`;
- creates `/etc/paneld/config.toml` from `config/example.toml`, only if it doesn't exist;
- copies fonts and images into `/etc/paneld/assets/`, skipping files that already exist;
- creates `/etc/paneld/env` with mode 600 and placeholder secrets, only if it doesn't exist;
- runs `systemctl daemon-reload`.

It never starts the service, downloads anything or touches the network. The manual steps are in [`README.md`](README.md) §5.

## 5. Configure

```bash
sudoedit /etc/paneld/config.toml
paneld check --config /etc/paneld/config.toml
```

Settings to change at minimum:

- **The Colorlight output:** uncomment the `[[output]] type = "colorlight"` block and set `interface` to the port from step 3. Colour order and gamma come from bring-up ([`README.md`](README.md) §4).
- **`[http] bind`:** the default is `127.0.0.1:8080`, which is only reachable from this machine. See step 8 before opening it to the LAN.
- **Personal settings:** your sources (Home Assistant, location for the aircraft source, and so on). They stay on this machine.

Put secrets only in `/etc/paneld/env`, never in the config:

```bash
sudoedit /etc/paneld/env
```

The config names variables with `token_env`, `password_env`, `header_env` and `bearer_env`, and `env` holds their values. Delete the lines you don't use. `paneld check` validates the config. The env variables are checked when the service starts, and the journal names any that are missing.

## 6. Check the raw socket, then start

Before enabling the service, check that paneld can see the card:

```bash
sudo paneld probe --interface eth1
```

This lists each receiver with its firmware and reported size. To check under the exact sandbox and identity the service uses, see "Check that CAP_NET_RAW works" in [`README.md`](README.md) §5.

Then start the service:

```bash
sudo systemctl enable --now paneld
systemctl status paneld
journalctl -u paneld -f
curl -s http://127.0.0.1:8080/api/state
```

The log shows `render loop started`, one `colorlight: found receiver` line per card, and `http listening`.

To reload after editing the config, do nothing: paneld polls the file once a second. You can also run `sudo systemctl reload paneld`, which sends SIGHUP. Settings marked restart-only in the [configuration reference](../docs/configuration.md#what-reloads-live) need `sudo systemctl restart paneld`.

## 7. Running by hand (optional)

To run paneld outside systemd as a normal user, give the binary the one capability it needs:

```bash
sudo setcap cap_net_raw+ep /usr/local/bin/paneld
getcap /usr/local/bin/paneld      # cap_net_raw=ep
paneld run --config /etc/paneld/config.toml
```

The service doesn't need this, because systemd grants the capability itself. Replacing the binary drops the file capability, so run `setcap` again after an upgrade if you rely on it.

## 8. Remote access and firewall

These routes are open:
- `/preview` and `/preview.png` (the live preview);
- `/metrics` (Prometheus);
- `/api/*`, unless you set `[http] token_env`.

To use them from the LAN, follow these steps:

1. Set `[http] bind = "0.0.0.0:8080"`. This needs a restart.
2. Set `[http] token_env = "PANELD_API_TOKEN"` and put a long random value in `/etc/paneld/env`, e.g. from `openssl rand -hex 32`. paneld warns at start-up if the API is reachable from the network without a token.
3. Allow the port only from your LAN. With `ufw`:

   ```bash
   sudo ufw allow from 192.168.1.0/24 to any port 8080 proto tcp   # your LAN's subnet
   ```

   Without ufw, add an equivalent nftables rule.

## 9. Upgrading

```bash
cd paneld && git pull
cargo build --release --locked -p paneld        # or download the new release binary
sudo deploy/install.sh target/release/paneld    # keeps your config, env and assets
sudo systemctl restart paneld
```

The unit file is updated by the script. Check the release notes for config changes, and run `paneld check --config /etc/paneld/config.toml` before restarting.

## 10. Uninstalling

```bash
sudo systemctl disable --now paneld
sudo rm /etc/systemd/system/paneld.service /usr/local/bin/paneld
sudo systemctl daemon-reload
sudo rm -r /etc/paneld      # config, assets and secrets: only if you no longer need them
```

Then remove the card port's network configuration, whichever you created:

```bash
sudo nmcli connection delete paneld-card
sudo rm /etc/netplan/60-paneld-card.yaml && sudo netplan apply
sudo rm /etc/network/interfaces.d/paneld-card /etc/sysctl.d/60-paneld-card.conf
```

## Troubleshooting (Debian-specific)

| Symptom | Fix |
|---|---|
| `error: rustc 1.63.0 is not supported` (or 1.75) | The apt Rust is too old. Install rustup (step 2b). |
| `linker 'cc' not found`, or `ring` fails to build | `sudo apt install build-essential` |
| Cross build: `failed to find tool "aarch64-linux-gnu-gcc"` | `sudo apt install gcc-aarch64-linux-gnu` |
| `GLIBC_2.xx not found` when starting a downloaded binary | The OS is older than the supported table. Build on the machine (step 2b). |
| `permission denied opening a raw socket` | When run by hand: step 7. As the service: check that the unit is the one from `deploy/`, with `AmbientCapabilities=CAP_NET_RAW`. |
| No receivers found | Is the port up? Does it have no IP? Is the link gigabit? (step 3). Then see [`README.md`](README.md) §6. |
| The interface name changed after a reboot (USB adapter) | Use the stable `enx…` name from `ip -br link` in both the network config and `[[output]] interface`. |

## What has been verified

- **Ubuntu 24.04 (amd64):**
  - step 2b builds with `--locked`;
  - `deploy/install.sh` installs into a scratch root, with `env` at mode 600 and asset paths rewritten;
  - `paneld check` accepts the installed config;
  - `systemd-analyze verify` passes on the unit.
- **Version table:** from the distribution package sites, and the unit's directives from the systemd man pages, as noted above.
- **The netplan and ifupdown snippets** follow their reference docs.
- **Not yet verified:** they have not been run on a real system with a card. Nor has the full service start, which needs a card on a raw port.
