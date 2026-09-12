# Optimimer on macOS, Windows or a plain Linux folder

1. Keep `optimimer-backend` (or `optimimer-backend.exe`) and the `agents` folder together, anywhere you like —
   for example a folder called `optimimer` in your home directory.
2. Run the binary (double-click on Windows; `./optimimer-backend` in a terminal on macOS or Linux — macOS may ask you
   to allow it under Privacy & Security the first time).
3. The first run asks three questions right in the window: your Telegram bot token (from @BotFather), your OpenRouter
   API key (free models work with no credits; leave blank for offline mock mode), and your timezone. The answers are
   saved to a `.env` file next to the binary; delete that file to answer again.
4. A **setup code** is printed in a box. Send it to your bot from Telegram to pair as the owner; the bot then asks
   your name, timezone, categories and which models to use.

The database (`optimimer.db`) and the `vault` folder are created next to the binary unless you pointed the vault
question at your Obsidian vault. Keep the window open, or run it in the background with a launchd agent (macOS), a
Task Scheduler "at logon" task (Windows) or a user systemd unit (Linux).

Full documentation: https://github.com/arsalikhov/optimimer/tree/main/docs
