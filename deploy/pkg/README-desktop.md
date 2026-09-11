# Optimimer — running the binary directly (macOS, Windows, any Linux)

1. Put `optimimer-backend` (or `optimimer-backend.exe`) and the `agents/` folder in a directory of your own,
   e.g. `~/optimimer`.
2. In that directory create a file named `.env` with two lines:

       TELEGRAM_BOT_TOKEN=<token from @BotFather>
       OPENROUTER_API_KEY=<key from openrouter.ai>

   Optional: `DEFAULT_TZ=Europe/London` and `VAULT_DIR=/path/to/your/Obsidian/vault`.
3. Run `./optimimer-backend` (double-click works on Windows). The log prints a **setup code**; send it to your bot
   from Telegram to pair as the owner. The bot then asks your name, timezone and categories.

The database (`optimimer.db`) and, unless you pointed `VAULT_DIR` elsewhere, the `vault/` folder are created next
to the binary. Keep it running with whatever you already use for background programs: a launchd agent on macOS, a
Task Scheduler "at logon" task on Windows, a user systemd unit on Linux. Full documentation:
https://github.com/arsalikhov/optimimer/tree/main/docs
