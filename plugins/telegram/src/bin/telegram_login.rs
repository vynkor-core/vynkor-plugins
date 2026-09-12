//! Interactive login script — creates new Grammers sessions.
//! Usage: cargo run --bin telegram_login

use std::io::{self, Write};

use grammers_client::{Client, Config as GrammersConfig, InitParams, SignInError};
use grammers_session::Session;

const API_ID: i32 = 26488263;
const API_HASH: &str = "fc09d6a2617121fcd947375b92f344bb";

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env().unwrap_or_else(|_| "info".into()),
        )
        .init();

    println!("=== Telegram Grammers Session Creator ===\n");

    // Get account name
    print!("Account name (e.g. loner42): ");
    io::stdout().flush()?;
    let mut account_name = String::new();
    io::stdin().read_line(&mut account_name)?;
    let account_name = account_name.trim().to_string();

    if account_name.is_empty() {
        eprintln!("Error: account name cannot be empty");
        std::process::exit(1);
    }

    // Get session directory
    let session_dir = std::env::var("TELEGRAM_PLUGIN_SESSION_DIR").unwrap_or_else(|_| {
        let home = std::env::var("HOME").unwrap_or_else(|_| "/tmp".into());
        format!("{home}/.local/share/vyn/telegram")
    });

    std::fs::create_dir_all(&session_dir)?;

    // Strip leading @ if present (user might type @loner42)
    let clean_name = account_name.trim_start_matches('@');
    let session_path = format!("{session_dir}/{clean_name}.session");
    println!("Session file: {session_path}\n");

    // Create or load session
    let session = Session::load_file_or_create(&session_path)?;
    let client = Client::connect(GrammersConfig {
        session,
        api_id: API_ID,
        api_hash: API_HASH.to_string(),
        params: InitParams::default(),
    })
    .await?;

    // Check if already signed in
    if client.is_authorized().await? {
        let me = client.get_me().await?;
        println!(
            "Already signed in as: {} (@{})",
            me.first_name(),
            me.username().unwrap_or("no_username")
        );
        println!("User ID: {}", me.id());

        print!("\nDo you want to re-login? (y/N): ");
        io::stdout().flush()?;
        let mut answer = String::new();
        io::stdin().read_line(&mut answer)?;
        if answer.trim().to_lowercase() != "y" {
            println!("Keeping existing session.");
            return Ok(());
        }
    }

    // Get phone number
    print!("Phone number (with +, e.g. +998912345678): ");
    io::stdout().flush()?;
    let mut phone = String::new();
    io::stdin().read_line(&mut phone)?;
    let phone = phone.trim().to_string();

    if phone.is_empty() || !phone.starts_with('+') {
        eprintln!("Error: phone must start with +");
        std::process::exit(1);
    }

    // Send verification code
    println!("\nSending verification code...");
    let login_token = client.request_login_code(&phone).await?;

    // Get code from user
    print!("Enter the verification code: ");
    io::stdout().flush()?;
    let mut code = String::new();
    io::stdin().read_line(&mut code)?;
    let code = code.trim().to_string();

    // Sign in
    println!("Signing in...");
    match client.sign_in(&login_token, &code).await {
        Ok(me) => {
            println!("\n✓ Successfully signed in!");
            println!(
                "  Name: {} {}",
                me.first_name(),
                me.last_name().unwrap_or("")
            );
            println!("  Username: @{}", me.username().unwrap_or("no_username"));
            println!("  User ID: {}", me.id());
            println!("  Phone: {}", me.phone().unwrap_or("hidden"));

            // Save session
            client.session().save_to_file(&session_path)?;
            println!("\n  Session saved to: {session_path}");
        }
        Err(SignInError::PasswordRequired(token)) => {
            println!("\nTwo-factor authentication required.");
            print!("Enter your 2FA password: ");
            io::stdout().flush()?;
            let mut password = String::new();
            io::stdin().read_line(&mut password)?;
            let password = password.trim().to_string();

            let me = client.check_password(token, password.as_bytes()).await?;
            println!("\n✓ Successfully signed in with 2FA!");
            println!(
                "  Name: {} {}",
                me.first_name(),
                me.last_name().unwrap_or("")
            );
            println!("  Username: @{}", me.username().unwrap_or("no_username"));
            println!("  User ID: {}", me.id());

            client.session().save_to_file(&session_path)?;
            println!("\n  Session saved to: {session_path}");
        }
        Err(e) => {
            eprintln!("\n✗ Sign in failed: {e}");
            std::process::exit(1);
        }
    }

    println!("\nDone. You can now use this session with the telegram plugin.");
    Ok(())
}
