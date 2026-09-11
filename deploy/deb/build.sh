#!/usr/bin/env bash
# Build a Debian package from a static binary:
#   deploy/deb/build.sh <binary> <arm64|amd64> <version> <out-dir>
# Layout: /usr/bin/optimimer-backend, /usr/bin/optimimer-setup, /usr/share/optimimer/{agents,obsidian-sync.service},
# /lib/systemd/system/optimimer.service. Data lives in /var/lib/optimimer, config in /etc/optimimer (created by postinst).
set -euo pipefail
BIN="$1"; ARCH="$2"; VERSION="$3"; OUT="$4"
HERE="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
ROOT="$HERE/../.."
PKG="$(mktemp -d)/optimimer_${VERSION}_${ARCH}"
trap 'rm -rf "$(dirname "$PKG")"' EXIT

install -d "$PKG/DEBIAN" "$PKG/usr/bin" "$PKG/usr/share/optimimer/agents" "$PKG/lib/systemd/system" "$PKG/usr/share/doc/optimimer"
install -m 755 "$BIN" "$PKG/usr/bin/optimimer-backend"
install -m 755 "$ROOT/deploy/optimimer-setup" "$PKG/usr/bin/optimimer-setup"
install -m 644 "$ROOT/backend/agents/"*.json "$PKG/usr/share/optimimer/agents/"
install -m 644 "$ROOT/deploy/obsidian-sync.service" "$PKG/usr/share/optimimer/obsidian-sync.service"
install -m 644 "$ROOT/deploy/pkg/optimimer.service" "$PKG/lib/systemd/system/optimimer.service"
install -m 644 "$ROOT/README.md" "$PKG/usr/share/doc/optimimer/README.md"
install -m 755 "$HERE/postinst" "$HERE/prerm" "$HERE/postrm" "$PKG/DEBIAN/"

SIZE=$(du -sk "$PKG" --exclude=DEBIAN | cut -f1)
cat > "$PKG/DEBIAN/control" <<CTRL
Package: optimimer
Version: $VERSION
Section: net
Priority: optional
Architecture: $ARCH
Maintainer: Arsali <arsalikhov@users.noreply.github.com>
Installed-Size: $SIZE
Depends: systemd, adduser
Recommends: etherwake
Homepage: https://github.com/arsalikhov/optimimer
Description: Private Telegram assistant that writes to your Obsidian vault
 A tool-calling assistant you talk to on Telegram: tasks, notes and voice memos
 into an Obsidian vault, a money ledger, reminders, shopping lists, stock
 watches, Wake-on-LAN and long-term memory. Runs as one static binary with no
 inbound ports. After installing, run: sudo optimimer-setup
CTRL

mkdir -p "$OUT"
dpkg-deb --build --root-owner-group "$PKG" "$OUT/optimimer_${VERSION}_${ARCH}.deb" >/dev/null
echo "$OUT/optimimer_${VERSION}_${ARCH}.deb"
