//! Standalone test binary — connects to Telegram directly, lists recent messages.

use telegram_plugin::mtproto::session::SessionPool;
use telegram_plugin::Config;

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env().unwrap_or_else(|_| "info".into()),
        )
        .init();

    let config = Config::from_env();
    if config.accounts.is_empty() {
        eprintln!("No accounts configured. Set TELEGRAM_PLUGIN_ACCOUNTS and per-account env vars.");
        std::process::exit(1);
    }

    println!("Connecting {} account(s)...", config.accounts.len());

    let pool = SessionPool::new();
    for account in &config.accounts {
        println!("  Connecting {}...", account.id);
        match pool.connect(account).await {
            Ok(()) => println!("  ✓ {} connected", account.id),
            Err(e) => eprintln!("  ✗ {} failed: {e}", account.id),
        }
    }

    println!("\n--- Dialogs (all accounts) ---\n");

    for account in &config.accounts {
        let client = match pool.get(&account.id) {
            Some(c) => c,
            None => {
                eprintln!("No client for account {}", account.id);
                continue;
            }
        };

        println!("=== Account: {} ===", account.id);

        let mut dialogs = client.iter_dialogs();
        let mut dialog_list = Vec::new();
        let mut count = 0;
        while let Some(dialog) = dialogs.next().await? {
            if count >= 10 {
                break;
            }
            count += 1;

            let chat_name = dialog.chat.name();
            let chat_id = dialog.chat.id();
            let top_msg = dialog
                .last_message
                .as_ref()
                .map(|m| m.text().to_string())
                .unwrap_or_else(|| "(no text)".into());

            println!("  [{chat_id}] {chat_name}");
            println!("    last: {top_msg}");
            dialog_list.push(dialog);
        }

        // Fetch recent messages from the first dialog
        if !dialog_list.is_empty() {
            println!("\n--- Recent messages (first dialog) ---\n");
            let first = &dialog_list[0];
            let peer = first.chat.clone();
            let mut messages = client.iter_messages(&peer);
            let mut msg_count = 0;
            while let Some(msg) = messages.next().await? {
                if msg_count >= 5 {
                    break;
                }
                msg_count += 1;
                let from = match msg.sender() {
                    Some(chat) => chat.name().to_string(),
                    None => "unknown".into(),
                };
                let text = msg.text();
                let date = msg.date().format("%Y-%m-%d %H:%M:%S");
                let id = msg.id();
                println!("  #{id} [{date}] {from}: {text}");
            }
        }

        println!();
    }

    println!("Done.");
    Ok(())
}
