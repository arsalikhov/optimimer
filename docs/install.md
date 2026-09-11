# Install

Optimimer runs as a systemd service on a 64-bit ARM Linux box — a Raspberry Pi 3 or newer is plenty, because the
heavy LLM work happens at OpenRouter. The bot uses long polling, so the Pi needs **no open ports and no public IP**.

## What you need

- A Raspberry Pi (64-bit OS) with `curl` and `sudo`.
- A **Telegram bot token**: talk to [@BotFather](https://t.me/BotFather), `/newbot`, copy the token.
- An **OpenRouter API key** from [openrouter.ai](https://openrouter.ai).

## Clone and run one script

On the Pi:

```sh
git clone https://github.com/arsalikhov/optimimer.git
cd optimimer && ./install.sh
```

The script copies the prebuilt binary and bundled agents into `/opt/optimimer`, installs `etherwake` for
Wake-on-LAN when `apt` is available, and runs the interactive setup, which asks for:

1. your default timezone (IANA name, e.g. `Europe/London`),
2. the Telegram bot token,
3. the OpenRouter API key.

It writes a private `/opt/optimimer/optimimer.env` (mode 600), installs and starts the `optimimer` service, and ends
with a **setup code** like `K7QD-3MXP`.

## Pair your Telegram account

Open your bot in Telegram and send the setup code as a message. That chat becomes the **owner**, and the bot walks
you through a short setup: what to call you, your timezone (share a location pin or type it), your task categories
(keep the defaults or send your own), and optionally a machine to wake over the network. Every step has a one-tap
default, and everything can be changed later just by telling the bot.

Lost the code? It is in `optimimer.env` as `OPTIMIMER_SETUP_CODE`, and the service log prints it on start until
someone pairs: `journalctl -u optimimer -n 50`.

Without git: download the repository as a zip from GitHub, unpack it, and run `./install.sh` from the folder.
`./install.sh --no-setup` only copies the files, for when you want to run `/opt/optimimer/install.sh` later.

Not on a Pi? On any Linux with a Rust toolchain the script builds the binary from source instead of using the
prebuilt one.

## Updating

```sh
cd optimimer && git pull && ./install.sh
```

It replaces the binary and agents, keeps `optimimer.env` (answer **Y** to "reuse it as-is"), and restarts the
service. Your database and vault are untouched.

## Everyday commands on the Pi

```sh
journalctl -u optimimer -f          # follow the log
sudo systemctl restart optimimer    # restart
/opt/optimimer/install.sh           # re-run setup (change secrets or timezone)
```

## Secrets without typing them

The setup script uses any of these already present in its environment and skips the prompt: `TELEGRAM_BOT_TOKEN`,
`OPENROUTER_API_KEY`, `DEFAULT_TZ`, `VAULT_DIR`, `OPTIMIMER_SETUP_CODE`, `TELEGRAM_ALLOWED_CHAT_IDS`,
`RENT_AMOUNTS`, `RESEND_API_KEY`, `EMAIL_FROM`. With the `infisical` CLI on the Pi you can run
`infisical run --env=prod -- /opt/optimimer/install.sh`, and the script offers to start the service through
`infisical run` so secrets are injected at each start and never written to disk. From a dev machine,
`make deploy PI=user@host INFISICAL_ENV=dev` does the same with your local Infisical (see
[Development](development.md)).

## Obsidian sync on the Pi

To have the vault appear on your other devices without running the desktop app on the Pi, install **Obsidian
Headless** (`npm install -g obsidian-headless`, Node 22+), then `ob login` and
`ob sync-setup --vault "<remote vault>" --path /opt/optimimer/vault`. When `ob` is present, the setup script offers
to install a `obsidian-sync` service that runs `ob sync --continuous`. Details in [The vault](vault.md).

## Uninstall

```sh
sudo systemctl disable --now optimimer obsidian-sync 2>/dev/null
sudo rm -f /etc/systemd/system/optimimer.service /etc/systemd/system/obsidian-sync.service
sudo rm -rf /opt/optimimer          # includes the database and, unless you moved it, the vault
```
