//! Glue: validate an `email_send` request, resolve the SMTP credential
//! vault-first, build a `lettre` message, and send it. A
//! `EMAIL_PLUGIN_SMTP_STUB=true` process env switches the send into stub mode
//! (no network), which keeps the fake-kernel tests offline and lets an
//! operator smoke-test the plugin without an SMTP account. The resolved
//! password is only ever handed to `lettre`'s `Credentials`; it never appears
//! in an error string or log line.

use vynkor_sdk::VynkorClient;

use crate::request::{self, EmailSendParams};

trait ImapStream: std::io::Read + std::io::Write {}
impl<T: std::io::Read + std::io::Write> ImapStream for T {}

/// When set to the exact string `true`, `handle_email_send` skips the real
/// SMTP send and returns a successful, clearly-marked stub response.
const SMTP_STUB_ENV: &str = "EMAIL_PLUGIN_SMTP_STUB";
const IMAP_STUB_ENV: &str = "EMAIL_PLUGIN_IMAP_STUB";

fn unix_millis() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0)
}

fn smtp_stub_enabled() -> bool {
    std::env::var(SMTP_STUB_ENV).as_deref() == Ok("true")
}

fn imap_stub_enabled() -> bool {
    std::env::var(IMAP_STUB_ENV).as_deref() == Ok("true")
        || smtp_stub_enabled()
}

/// Build the `lettre` message from parsed params. `lettre`'s `Address` parser
/// re-validates the addresses (stricter than `request::is_valid_email`), so a
/// caller-provided address that slips past the parse-time check still fails
/// cleanly here without leaking anything.
fn build_message(params: &EmailSendParams) -> Result<lettre::Message, String> {
    use lettre::message::header::ContentType;
    use lettre::message::Mailbox;
    use lettre::Address;

    let from: Address = params
        .from
        .parse()
        .map_err(|e| format!("invalid from address: {e}"))?;
    let to: Address = params
        .to
        .parse()
        .map_err(|e| format!("invalid recipient address: {e}"))?;

    let content_type = if params.is_html {
        ContentType::TEXT_HTML
    } else {
        ContentType::TEXT_PLAIN
    };

    let mut builder = lettre::Message::builder()
        .from(Mailbox::new(None, from))
        .to(Mailbox::new(None, to));

    for addr in &params.cc {
        let cc: Address = addr
            .parse()
            .map_err(|e| format!("invalid cc address '{addr}': {e}"))?;
        builder = builder.cc(Mailbox::new(None, cc));
    }
    for addr in &params.bcc {
        let bcc: Address = addr
            .parse()
            .map_err(|e| format!("invalid bcc address '{addr}': {e}"))?;
        builder = builder.bcc(Mailbox::new(None, bcc));
    }
    if let Some(reply_to) = &params.reply_to {
        let reply_to: Address = reply_to
            .parse()
            .map_err(|e| format!("invalid reply_to address '{reply_to}': {e}"))?;
        builder = builder.reply_to(Mailbox::new(None, reply_to));
    }

    builder
        .subject(params.subject.clone())
        .header(content_type)
        .body(params.body.clone())
        .map_err(|e| format!("failed to build email message: {e}"))
}

/// Handle one `email_send` action end to end. Returns the JSON to place in
/// `ActionResponse.data_json` on success, or a human-readable error (never
/// containing the resolved password) on failure.
pub async fn handle_email_send(
    client: &mut VynkorClient,
    params_json: &[u8],
) -> Result<Vec<u8>, String> {
    let params = request::parse_request(params_json)?;

    let allowed_cred_envs = request::parse_allowed_cred_envs(
        &std::env::var(request::ALLOWED_CRED_ENVS_ENV).unwrap_or_default(),
    );
    if !request::is_allowed_cred_env(&params.credentials_env, &allowed_cred_envs) {
        return Err(format!(
            "credentials_env '{}' is not in the operator's {} allowlist",
            params.credentials_env,
            request::ALLOWED_CRED_ENVS_ENV
        ));
    }

    // Vault-first resolution. The value is bound here and only ever moved
    // into `Credentials` below — never formatted into an error or log.
    let password = crate::key_resolve::resolve_secret(client, &params.credentials_env).await?;

    if smtp_stub_enabled() {
        let message_id = format!("stub-{}", unix_millis());
        return serde_json::to_vec(&serde_json::json!({
            "message_id": message_id,
            "to": params.to,
            "subject": params.subject,
            "stubbed": true,
            "smtp_host": params.smtp_host,
            "smtp_port": params.smtp_port,
        }))
        .map_err(|e| format!("failed to encode stub response: {e}"));
    }

    use lettre::transport::smtp::authentication::Credentials;
    use lettre::{AsyncSmtpTransport, AsyncTransport, Tokio1Executor};

    let email = build_message(&params)?;
    // Correlation id for the caller. Deliberately our own, not lettre's
    // generated Message-Id header (lettre 0.11 has no getter for it); it is
    // returned verbatim so the caller can correlate the reply with this send.
    let message_id = format!("email-{}", unix_millis());

    let creds = Credentials::new(params.smtp_user.clone(), password);

    let mailer = AsyncSmtpTransport::<Tokio1Executor>::relay(&params.smtp_host)
        .map_err(|e| {
            format!(
                "failed to configure SMTP relay for host '{}': {e}",
                params.smtp_host
            )
        })?
        .port(params.smtp_port)
        .credentials(creds)
        .timeout(Some(std::time::Duration::from_millis(params.timeout_ms)))
        .build();

    mailer
        .send(email)
        .await
        .map_err(|e| format!("SMTP send failed: {e}"))?;

    serde_json::to_vec(&serde_json::json!({
        "message_id": message_id,
        "to": params.to,
        "subject": params.subject,
        "stubbed": false,
    }))
    .map_err(|e| format!("failed to encode response: {e}"))
}

pub async fn handle_email_list(
    client: &mut VynkorClient,
    params_json: &[u8],
) -> Result<Vec<u8>, String> {
    let params = request::parse_email_list_request(params_json)?;

    let allowed_cred_envs = request::parse_allowed_cred_envs(
        &std::env::var(request::ALLOWED_CRED_ENVS_ENV).unwrap_or_default(),
    );
    if !request::is_allowed_cred_env(&params.credentials_env, &allowed_cred_envs) {
        return Err(format!(
            "credentials_env '{}' is not in the operator's {} allowlist",
            params.credentials_env,
            request::ALLOWED_CRED_ENVS_ENV
        ));
    }

    let _password = crate::key_resolve::resolve_secret(client, &params.credentials_env).await?;

    if imap_stub_enabled() {
        let stub_emails: Vec<serde_json::Value> = (1..=params.limit.min(3))
            .map(|i| {
                serde_json::json!({
                    "uid": i,
                    "from": "alice@example.com",
                    "to": params.imap_user,
                    "subject": format!("Stub email {}", i),
                    "date": "2024-01-01T00:00:00Z",
                    "snippet": "This is a stub email for offline testing"
                })
            })
            .collect();
        let count = stub_emails.len();
        return serde_json::to_vec(&serde_json::json!({
            "emails": stub_emails,
            "mailbox": params.mailbox,
            "count": count,
            "stubbed": true,
            "imap_host": params.imap_host,
            "imap_port": params.imap_port
        }))
        .map_err(|e| format!("failed to encode stub response: {e}"));
    }

    let imap_host = params.imap_host.clone();
    let imap_port = params.imap_port;
    let imap_user = params.imap_user.clone();
    let mailbox = params.mailbox.clone();
    let limit = params.limit;
    let timeout_ms = params.timeout_ms;
    let password_clone = _password.clone();

    let fetched = tokio::task::spawn_blocking(move || {
        fetch_via_imap_sync(
            &imap_host,
            imap_port,
            &imap_user,
            &password_clone,
            &mailbox,
            limit,
            timeout_ms,
        )
    })
    .await
    .map_err(|e| format!("IMAP task join failed: {e}"))??;

    serde_json::to_vec(&fetched).map_err(|e| format!("failed to encode response: {e}"))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn base_params() -> EmailSendParams {
        EmailSendParams {
            to: "user@example.com".to_string(),
            from: "sender@example.com".to_string(),
            subject: "Hello".to_string(),
            body: "Hi there".to_string(),
            is_html: false,
            credentials_env: "EMAIL_SMTP_PASS".to_string(),
            smtp_host: "localhost".to_string(),
            smtp_port: 587,
            smtp_user: "sender@example.com".to_string(),
            timeout_ms: 30000,
            cc: vec![],
            bcc: vec![],
            reply_to: None,
        }
    }

    fn formatted(params: &EmailSendParams) -> String {
        String::from_utf8(build_message(params).unwrap().formatted()).unwrap()
    }

    #[test]
    fn message_has_no_cc_bcc_reply_to_headers_by_default() {
        let raw = formatted(&base_params());
        assert!(!raw.contains("Cc:"), "raw was: {raw}");
        assert!(!raw.contains("Bcc:"), "raw was: {raw}");
        assert!(!raw.contains("Reply-To:"), "raw was: {raw}");
    }

    #[test]
    fn message_includes_cc_header_when_set() {
        let mut params = base_params();
        params.cc = vec!["cc1@example.com".to_string(), "cc2@example.com".to_string()];
        let raw = formatted(&params);
        assert!(raw.contains("Cc: cc1@example.com, cc2@example.com"), "raw was: {raw}");
    }

    #[test]
    fn message_includes_bcc_recipients_without_bcc_header() {
        // Bcc must never appear in the sent headers (that's the whole point
        // of blind copy) — lettre's `.bcc()` adds an envelope recipient only.
        let mut params = base_params();
        params.bcc = vec!["hidden@example.com".to_string()];
        let raw = formatted(&params);
        assert!(!raw.contains("hidden@example.com"), "raw was: {raw}");
    }

    #[test]
    fn message_includes_reply_to_header_when_set() {
        let mut params = base_params();
        params.reply_to = Some("reply@example.com".to_string());
        let raw = formatted(&params);
        assert!(raw.contains("Reply-To: reply@example.com"), "raw was: {raw}");
    }
}

fn fetch_via_imap_sync(
    host: &str,
    port: u16,
    user: &str,
    password: &str,
    mailbox: &str,
    limit: usize,
    timeout_ms: u64,
) -> Result<serde_json::Value, String> {
    use std::net::{TcpStream, ToSocketAddrs};
    use std::time::Duration;

    let addr = format!("{host}:{port}");
    let socket_addr = addr
        .to_socket_addrs()
        .map_err(|e| format!("IMAP DNS failed for {host}:{port}: {e}"))?
        .next()
        .ok_or_else(|| format!("IMAP DNS no addr for {host}:{port}"))?;
    let tcp = TcpStream::connect_timeout(&socket_addr, Duration::from_millis(timeout_ms))
        .map_err(|e| format!("IMAP connect failed to {host}:{port}: {e}"))?;
    tcp.set_read_timeout(Some(Duration::from_millis(timeout_ms)))
        .map_err(|e| format!("IMAP set timeout failed: {e}"))?;
    tcp.set_write_timeout(Some(Duration::from_millis(timeout_ms)))
        .map_err(|e| format!("IMAP set timeout failed: {e}"))?;

    let fetch_and_build = |mut session: imap::Session<Box<dyn ImapStream + Send>>| {
        session
            .select(mailbox)
            .map_err(|e| format!("IMAP SELECT {mailbox} failed: {e}"))?;
        let search: std::collections::HashSet<u32> = session
            .search("ALL")
            .map_err(|e| format!("IMAP SEARCH failed: {e}"))?;
        if search.is_empty() {
            let _ = session.logout();
            return Ok(serde_json::json!({
                "emails": [],
                "mailbox": mailbox,
                "count": 0,
                "stubbed": false
            }));
        }
        let mut seqs: Vec<u32> = search.into_iter().collect();
        seqs.sort_unstable();
        if seqs.len() > limit {
            seqs = seqs[seqs.len() - limit..].to_vec();
        }
        let seq_set = seqs
            .iter()
            .map(|n| n.to_string())
            .collect::<Vec<_>>()
            .join(",");
        let fetches = session
            .fetch(seq_set, "(UID ENVELOPE)")
            .map_err(|e| format!("IMAP FETCH failed: {e}"))?;
        let mut emails = Vec::new();
        for fetch in fetches.iter() {
            let uid = fetch.uid.unwrap_or(0);
            let env = fetch.envelope();
            let (from, subject, date) = if let Some(e) = env {
                let from = e
                    .from
                    .as_ref()
                    .and_then(|v| v.first())
                    .map(|a| {
                        let mbox = a
                            .mailbox
                            .as_ref()
                            .map(|m| String::from_utf8_lossy(m).to_string())
                            .unwrap_or_default();
                        let host = a
                            .host
                            .as_ref()
                            .map(|h| String::from_utf8_lossy(h).to_string())
                            .unwrap_or_default();
                        if mbox.is_empty() && host.is_empty() {
                            "unknown@example.com".to_string()
                        } else if host.is_empty() {
                            mbox
                        } else {
                            format!("{mbox}@{host}")
                        }
                    })
                    .unwrap_or_else(|| "unknown@example.com".to_string());
                let subject = e
                    .subject
                    .as_ref()
                    .map(|s| String::from_utf8_lossy(s).to_string())
                    .unwrap_or_default();
                let date = e
                    .date
                    .as_ref()
                    .map(|d| String::from_utf8_lossy(d).to_string())
                    .unwrap_or_default();
                (from, subject, date)
            } else {
                ("unknown@example.com".to_string(), String::new(), String::new())
            };
            emails.push(serde_json::json!({
                "uid": uid,
                "from": from,
                "to": user,
                "subject": subject,
                "date": date,
                "snippet": subject
            }));
        }
        let count = emails.len();
        let _ = session.logout();
        Ok(serde_json::json!({
            "emails": emails,
            "mailbox": mailbox,
            "count": count,
            "stubbed": false
        }))
    };

    if port == 993 {
        let tls = native_tls::TlsConnector::new()
            .map_err(|e| format!("TLS init failed: {e}"))?;
        let tls_stream = tls
            .connect(host, tcp)
            .map_err(|e| format!("TLS connect failed to {host}:{port}: {e}"))?;
        let tls_box: Box<dyn ImapStream + Send> = Box::new(tls_stream);
        let client = imap::Client::new(tls_box);
        let session = client
            .login(user, password)
            .map_err(|(e, _)| format!("IMAP login failed: {e}"))?;
        let boxed: imap::Session<Box<dyn ImapStream + Send>> = session;
        fetch_and_build(boxed)
    } else {
        let plain_box: Box<dyn ImapStream + Send> = Box::new(tcp);
        let client = imap::Client::new(plain_box);
        let session = client
            .login(user, password)
            .map_err(|(e, _)| format!("IMAP login failed: {e}"))?;
        fetch_and_build(session)
    }
}
