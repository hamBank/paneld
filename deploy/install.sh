#!/bin/sh
# Install paneld on this machine: binary, systemd unit, /etc/paneld skeleton.
#
#   sudo deploy/install.sh PATH/TO/paneld
#
# Idempotent. Never overwrites /etc/paneld/config.toml or /etc/paneld/env once they
# exist, never downloads anything, never starts the service, and never touches
# secrets or network settings. Read deploy/README.md first.
#
# DESTDIR=/some/dir installs into a scratch root (no root needed, no systemctl).
set -eu

bin=${1:-}
if [ -z "$bin" ] || [ ! -f "$bin" ]; then
    echo "usage: $0 PATH/TO/paneld   (a release binary built for this machine)" >&2
    exit 2
fi

here=$(cd "$(dirname "$0")" && pwd)
repo=$(dirname "$here")
d=${DESTDIR:-}

if [ -z "$d" ] && [ "$(id -u)" -ne 0 ]; then
    echo "run as root (sudo), or set DESTDIR to a scratch directory" >&2
    exit 1
fi

install -d -m 755 "$d/usr/local/bin" "$d/etc/systemd/system" \
    "$d/etc/paneld" "$d/etc/paneld/assets"

# Binary: install(1) writes a new file and renames it, so a running paneld keeps its old inode.
install -m 755 "$bin" "$d/usr/local/bin/paneld"
install -m 644 "$here/paneld.service" "$d/etc/systemd/system/paneld.service"

# Config: only when missing. The example's ../assets/ paths are made relative to /etc/paneld.
if [ ! -e "$d/etc/paneld/config.toml" ]; then
    sed 's#\.\./assets/#assets/#g' "$repo/config/example.toml" >"$d/etc/paneld/config.toml"
    chmod 644 "$d/etc/paneld/config.toml"
    echo "created /etc/paneld/config.toml from config/example.toml: edit it"
fi
# Assets (fonts, example image): copy missing files only.
if [ -d "$repo/assets" ]; then
    (cd "$repo/assets" && find . -type f) | while IFS= read -r f; do
        [ -e "$d/etc/paneld/assets/$f" ] || install -D -m 644 "$repo/assets/$f" "$d/etc/paneld/assets/$f"
    done
fi
# Secrets file: only when missing, root-only. Placeholder values; edit before use.
if [ ! -e "$d/etc/paneld/env" ]; then
    install -m 600 "$here/env.example" "$d/etc/paneld/env"
    echo "created /etc/paneld/env (mode 600) with placeholders: edit it, delete unused lines"
fi

if [ -z "$d" ]; then
    systemctl daemon-reload
    echo "installed. Next: edit /etc/paneld/config.toml, then: systemctl enable --now paneld"
else
    echo "installed into $d (DESTDIR); systemctl not run"
fi
