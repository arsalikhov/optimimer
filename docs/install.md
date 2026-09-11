# Install

Optimimer runs as a systemd service on a 64-bit ARM Linux box — a Raspberry Pi 3 or newer is plenty, because the
heavy LLM work happens at OpenRouter. The bot uses long polling, so the Pi needs **no open ports and no public IP**.

## What you need

- A Raspberry Pi (64-bit OS) with `curl` and `sudo`.
- A **Telegram bot token**: talk to [@BotFather](https://t.me/BotFather), `/newbot`, copy the token.
- An **OpenRouter API key** from [openrouter.ai](https://openrouter.ai).

## Pick your platform

Nobody needs a Rust toolchain: every release on the
[Releases page](https://github.com/arsalikhov/optimimer/releases) ships prebuilt binaries.

### Raspberry Pi OS, Debian, Ubuntu (.deb)

```sh
curl -LO https://github.com/arsalikhov/optimimer/releases/latest/download/optimimer-arm64.deb   # amd64 on a PC
sudo apt install ./optimimer-arm64.deb
sudo optimimer-setup
```

The package installs `/usr/bin/optimimer-backend`, a systemd service running as the `optimimer` system user, data in
`/var/lib/optimimer` (database and vault) and config in `/etc/optimimer/optimimer.env`. `optimimer-setup` asks
three things — timezone, Telegram bot token, OpenRouter API key — starts the service and prints the setup code.
Updating is `apt install ./optimimer-arm64.deb` again with the new file; the service restarts on its own.

### Arch Linux

Every release attaches a `PKGBUILD` (package `optimimer-bin`, x86_64 and aarch64) with checksums:

```sh
mkdir optimimer && cd optimimer
curl -LO https://github.com/arsalikhov/optimimer/releases/latest/download/PKGBUILD
makepkg -si
sudo optimimer-setup
```

Same layout as the .deb; the service account and directories come from `sysusers.d` / `tmpfiles.d`.

### Docker (any Linux)

```sh
cat > optimimer.env <<EOF
TELEGRAM_BOT_TOKEN=...
OPENROUTER_API_KEY=...
DEFAULT_TZ=Europe/London
EOF
docker run -d --name optimimer --restart unless-stopped --env-file optimimer.env \
  -v optimimer-data:/var/lib/optimimer --network host --cap-add NET_RAW \
  ghcr.io/arsalikhov/optimimer:latest
docker logs optimimer      # shows the setup code
```

`--network host` and `NET_RAW` are only needed for Wake-on-LAN; drop them otherwise. Mount a host folder instead
of the named volume if you want the vault synced by Obsidian on the same machine.

### macOS and Windows

Download `optimimer-<version>-macos-arm64.tar.gz` (Apple silicon), `-macos-amd64.tar.gz` (Intel) or
`-windows-amd64.zip`, unpack, and follow the `README.md` inside: create a `.env` with the two keys next to the
binary, run it, and send the setup code printed in the log to your bot. Keep it running with a launchd agent or a
Task Scheduler task. macOS may ask you to allow the unsigned binary under Privacy & Security the first time.

### From a git clone (any Linux with systemd)

```sh
git clone https://github.com/arsalikhov/optimimer.git
cd optimimer && ./install.sh
```

Copies the prebuilt aarch64 binary (or builds from source when a Rust toolchain is present) into `/opt/optimimer`
and runs the same setup as the package, as your own user. `./install.sh --no-setup` only copies the files. Update
with `git pull && ./install.sh`.

## Pair your Telegram account

Open your bot in Telegram and send the setup code as a message. That chat becomes the **owner**, and the bot walks
you through a short setup: what to call you, your timezone (share a location pin or type it), your task categories
(keep the defaults or send your own), and optionally a machine to wake over the network. Every step has a one-tap
default, and everything can be changed later just by telling the bot.

Lost the code? It is in `optimimer.env` as `OPTIMIMER_SETUP_CODE`, and the service log prints it on start until
someone pairs: `journalctl -u optimimer -n 50`.

Without git: download the repository as a zip from GitHub, unpack it, and run `./install.sh` from the folder.
`./install.sh --no-setup` only copies the files, for when you want to run `/opt/optimimer/optimimer-setup` later.

Not on a Pi? On any Linux with a Rust toolchain the script builds the binary from source instead of using the
prebuilt one.

## Updating

- `.deb`: download the new file and `sudo apt install ./optimimer-arm64.deb` again; the service restarts itself.
- Arch: fetch the new `PKGBUILD`, `makepkg -si`.
- Docker: `docker pull ghcr.io/arsalikhov/optimimer:latest`, then recreate the container.
- Clone: `git pull && ./install.sh` (answer **Y** to keep the env file).

Your database and vault are never touched by an update.

## Everyday commands

```sh
journalctl -u optimimer -f          # follow the log
sudo systemctl restart optimimer    # restart
sudo optimimer-setup                # re-run setup to change a secret or the timezone
```

(Clone installs: `/opt/optimimer/optimimer-setup`, no sudo.)

## Do I need a secret manager?

No. The env file written by `optimimer-setup` is all the bot needs, and it is readable only by root and the
service user. If you do use one, export the variables before running `optimimer-setup` and it asks nothing; any
variable in the environment beats the file.

## Obsidian sync on the Pi

To have the vault appear on your other devices without running the desktop app on the Pi, install **Obsidian
Headless** (`npm install -g obsidian-headless`, Node 22+), then `ob login` and
`ob sync-setup --vault "<remote vault>" --path /var/lib/optimimer/vault` (as the `optimimer` user for package
installs: prefix both with `sudo -u optimimer -H`). When `ob` is present, the setup script offers
to install a `obsidian-sync` service that runs `ob sync --continuous`. Details in [The vault](vault.md).

## Uninstall

```sh
sudo apt purge optimimer            # .deb (Arch: sudo pacman -Rns optimimer-bin); data stays in /var/lib/optimimer
sudo rm -rf /var/lib/optimimer      # …unless you want the database and vault gone too
# clone install:
sudo systemctl disable --now optimimer obsidian-sync 2>/dev/null
sudo rm -f /etc/systemd/system/optimimer.service /etc/systemd/system/obsidian-sync.service
sudo rm -rf /opt/optimimer
```
