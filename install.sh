#!/usr/bin/env bash
# Optimimer installer. On your Raspberry Pi (64-bit OS):
#
#   git clone https://github.com/arsalikhov/optimimer.git
#   cd optimimer && ./install.sh
#
# Copies the prebuilt binary, the bundled agents and the setup script into
# /opt/optimimer, then runs the interactive setup, which asks three things
# (timezone, Telegram bot token, OpenRouter API key) and prints the setup code
# you send to your bot to pair. Re-run after `git pull` to update.
#
# Prefer the .deb package if you are on Raspberry Pi OS / Debian / Ubuntu:
# see https://github.com/arsalikhov/optimimer/releases
#
# Not on 64-bit ARM? With a Rust toolchain installed the script builds from
# source instead (slow on a Pi, fine on a laptop or server).
#
# Flags:  --no-setup   copy files only, don't run the interactive setup
# Env:    OPTIMIMER_DIR=/opt/optimimer
set -euo pipefail

DIR="${OPTIMIMER_DIR:-/opt/optimimer}"
SRC="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
RUN_SETUP=y
[ "${1:-}" = "--no-setup" ] && RUN_SETUP=n

for t in sudo install; do
  command -v "$t" >/dev/null 2>&1 || { echo "Missing '$t' — install it and re-run." >&2; exit 1; }
done
[ -f "$SRC/deploy/optimimer-setup" ] || { echo "Run this from a checkout of the repository (deploy/optimimer-setup not found)." >&2; exit 1; }

# ---- pick a binary: prebuilt for aarch64, otherwise build from source --------
BIN=""
arch="$(uname -m)"
if { [ "$arch" = aarch64 ] || [ "$arch" = arm64 ]; } && [ -f "$SRC/deploy/bin/optimimer-backend-aarch64" ]; then
  BIN="$SRC/deploy/bin/optimimer-backend-aarch64"
elif command -v cargo >/dev/null 2>&1; then
  echo "==> No prebuilt binary for $arch — building from source (this takes a while)"
  (cd "$SRC/backend" && cargo build --release)
  BIN="$SRC/backend/target/release/optimimer-backend"
else
  echo "No prebuilt binary for $arch and no Rust toolchain. Install Rust (https://rustup.rs) and re-run," >&2
  echo "or run this on a 64-bit Raspberry Pi." >&2
  exit 1
fi

# ---- copy into the install dir ---------------------------------------------
echo "==> Installing into $DIR"
sudo mkdir -p "$DIR/agents"
sudo chown "$(id -un):$(id -gn)" "$DIR" "$DIR/agents"
install -m 755 "$BIN" "$DIR/optimimer-backend.new"
mv -f "$DIR/optimimer-backend.new" "$DIR/optimimer-backend"
install -m 644 "$SRC/backend/agents/"*.json "$DIR/agents/"
install -m 755 "$SRC/deploy/optimimer-setup" "$DIR/optimimer-setup"
install -m 644 "$SRC/deploy/obsidian-sync.service" "$DIR/obsidian-sync.service"

# Wake-on-LAN needs etherwake; skipped quietly when the package manager isn't apt.
if command -v apt-get >/dev/null 2>&1 && ! command -v etherwake >/dev/null 2>&1; then
  echo "==> Installing etherwake (Wake-on-LAN)"
  sudo apt-get install -y -qq etherwake >/dev/null 2>&1 || echo "   (couldn't install etherwake — Wake-on-LAN will be unavailable)"
fi

if [ "$RUN_SETUP" = n ]; then
  echo "Files installed. Run $DIR/optimimer-setup to set up the service."
  exit 0
fi

# Hand over to the interactive setup with a real terminal.
exec "$DIR/optimimer-setup" </dev/tty
