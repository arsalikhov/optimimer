//! First-run experience for people who just downloaded the binary (macOS,
//! Windows, a bare Linux tarball): find the `.env`, and when there is none and
//! someone is at the keyboard, ask the three questions right here instead of
//! making them write a file by hand. Package installs never reach the wizard
//! because `optimimer-setup` already wrote the env file.

use std::io::{self, BufRead, IsTerminal, Write};
use std::path::{Path, PathBuf};

fn exe_dir() -> Option<PathBuf> {
    std::env::current_exe().ok()?.parent().map(|p| p.to_path_buf())
}

/// Load `.env` from the working directory, else from next to the executable
/// (double-clicking on Windows/macOS doesn't set a useful working directory).
/// When the file next to the binary is the one used, also make its folder the
/// working directory so the database and vault land beside it.
pub fn load_env() {
    if Path::new(".env").exists() {
        dotenvy::dotenv().ok();
        return;
    }
    if let Some(dir) = exe_dir() {
        let p = dir.join(".env");
        if p.exists() {
            dotenvy::from_path(&p).ok();
            let _ = std::env::set_current_dir(&dir);
        }
    }
}

fn have(key: &str) -> bool {
    std::env::var(key).map(|v| !v.trim().is_empty()).unwrap_or(false)
}

fn ask(prompt: &str, default: &str) -> String {
    let mut out = io::stdout();
    if default.is_empty() {
        let _ = write!(out, "  {prompt}: ");
    } else {
        let _ = write!(out, "  {prompt} [{default}]: ");
    }
    let _ = out.flush();
    let mut line = String::new();
    let _ = io::stdin().lock().read_line(&mut line);
    let v = line.trim().to_string();
    if v.is_empty() { default.to_string() } else { v }
}

/// If the bot token is missing and someone is at the keyboard, walk them
/// through it and write `.env` next to the binary. Returns true when it did.
pub fn first_run_wizard() -> bool {
    if have("TELEGRAM_BOT_TOKEN") || !io::stdin().is_terminal() || !io::stdout().is_terminal() {
        return false;
    }
    let dir = exe_dir().unwrap_or_else(|| PathBuf::from("."));
    println!();
    println!("  Optimimer - first-time setup");
    println!("  ----------------------------");
    println!("  Three things, then you're done. Saved to {}", dir.join(".env").display());
    println!();
    let token = loop {
        let t = ask("Telegram bot token (from @BotFather)", "");
        if !t.is_empty() {
            break t;
        }
        println!("  The token is required: open Telegram, talk to @BotFather, send /newbot, and paste the token here.");
    };
    let key = ask("OpenRouter API key (openrouter.ai/keys; blank = offline mock mode)", "");
    let tz_default = iana_time_zone::get_timezone().unwrap_or_else(|_| "UTC".into());
    let tz = ask("Your timezone", &tz_default);
    let vault = ask("Folder for the Obsidian vault", &dir.join("vault").display().to_string());

    let contents = format!(
        "# Written by Optimimer's first-run setup. Delete this file to run it again.\n\
         TELEGRAM_BOT_TOKEN={token}\n\
         OPENROUTER_API_KEY={key}\n\
         DEFAULT_TZ={tz}\n\
         VAULT_DIR={vault}\n"
    );
    let path = dir.join(".env");
    if let Err(e) = std::fs::write(&path, &contents) {
        eprintln!("  Couldn't write {}: {e}", path.display());
        return false;
    }
    let _ = std::env::set_current_dir(&dir);
    std::env::set_var("TELEGRAM_BOT_TOKEN", &token);
    std::env::set_var("OPENROUTER_API_KEY", &key);
    std::env::set_var("DEFAULT_TZ", &tz);
    std::env::set_var("VAULT_DIR", &vault);
    println!();
    println!("  Saved. Starting up - watch for the setup code below.");
    println!();
    true
}

/// Big, unmissable box for the pairing code. Plain ASCII so it survives any
/// console, including the legacy Windows one.
pub fn print_setup_code(code: &str) {
    let line = "=".repeat(60);
    println!();
    println!("{line}");
    println!("  Open Telegram, message your bot, and send this code:");
    println!();
    println!("        {code}");
    println!();
    println!("  That pairs you as the owner. The bot then asks your name,");
    println!("  timezone, categories and models. Keep this window open.");
    println!("{line}");
    println!();
}

/// Colour only when the console can show it. On Windows this switches the
/// console into VT mode first; the legacy console (no VT) gets plain text.
pub fn ansi_ok() -> bool {
    io::stdout().is_terminal() && enable_ansi_support::enable_ansi_support().is_ok()
}
