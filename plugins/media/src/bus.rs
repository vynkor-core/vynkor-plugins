//! Session-bus connection that also works inside the kernel's plugin
//! sandbox.
//!
//! The sandbox runs the plugin in a user namespace mapping inner uid 0 to
//! the real uid (`vynkor/src/plugins/shim.rs`). zbus 4 authenticates with
//! `AUTH EXTERNAL <hex(geteuid())>`, i.e. claims uid 0, while the bus
//! daemon sees the socket peer as the real uid and rejects the mismatch
//! ("Exhausted available AUTH mechanisms"). sd-bus avoids this by sending
//! EXTERNAL with an *empty* authorization identity, which tells the daemon
//! to just use the peer credentials — this module does the same handshake
//! by hand and gives zbus the already-authenticated socket.

use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::UnixStream;
use zbus::Connection;

/// Standard zbus connect first (unchanged behaviour outside a sandbox);
/// on failure, the empty-identity EXTERNAL handshake over the unix socket.
pub async fn session() -> Result<Connection, String> {
    let first = match Connection::session().await {
        Ok(c) => return Ok(c),
        Err(e) => e.to_string(),
    };
    let path = session_socket_path()
        .ok_or_else(|| format!("ERR_MEDIA_BUS_UNAVAILABLE: {first} (and no unix:path session bus address to retry)"))?;
    connect_external_anonymous_id(&path)
        .await
        .map_err(|e| format!("ERR_MEDIA_BUS_UNAVAILABLE: {first}; empty-identity EXTERNAL retry: {e}"))
}

/// `unix:path=/run/user/1000/bus[,guid=...]` -> the path. Other transports
/// (abstract sockets, tcp) are left to zbus's own error.
fn session_socket_path() -> Option<String> {
    let addr = std::env::var("DBUS_SESSION_BUS_ADDRESS").ok().or_else(|| {
        std::env::var("XDG_RUNTIME_DIR").ok().map(|d| format!("unix:path={d}/bus"))
    })?;
    parse_unix_path(&addr)
}

fn parse_unix_path(addr: &str) -> Option<String> {
    // Several `;`-separated addresses may be listed; take the first unix:path.
    addr.split(';').find_map(|a| {
        let rest = a.strip_prefix("unix:")?;
        rest.split(',').find_map(|kv| kv.strip_prefix("path=")).map(unescape)
    })
}

/// D-Bus address values percent-escape bytes outside a safe set.
fn unescape(v: &str) -> String {
    let bytes = v.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'%' && i + 2 < bytes.len() {
            if let Ok(b) = u8::from_str_radix(&v[i + 1..i + 3], 16) {
                out.push(b);
                i += 3;
                continue;
            }
        }
        out.push(bytes[i]);
        i += 1;
    }
    String::from_utf8_lossy(&out).into_owned()
}

async fn read_line(stream: &mut UnixStream) -> Result<String, String> {
    // Byte-at-a-time so nothing past the server's reply is consumed: after
    // BEGIN the socket belongs to zbus.
    let mut line = Vec::new();
    loop {
        let b = stream.read_u8().await.map_err(|e| format!("read: {e}"))?;
        line.push(b);
        if line.ends_with(b"\r\n") {
            line.truncate(line.len() - 2);
            return Ok(String::from_utf8_lossy(&line).into_owned());
        }
        if line.len() > 512 {
            return Err("SASL line too long".into());
        }
    }
}

async fn send(stream: &mut UnixStream, cmd: &[u8]) -> Result<(), String> {
    stream.write_all(cmd).await.map_err(|e| format!("write: {e}"))
}

async fn connect_external_anonymous_id(path: &str) -> Result<Connection, String> {
    let mut stream = UnixStream::connect(path).await.map_err(|e| format!("connect {path}: {e}"))?;
    let fut = async {
        // Leading NUL byte is required by the protocol (credentials byte).
        send(&mut stream, b"\0AUTH EXTERNAL\r\n").await?;
        let mut reply = read_line(&mut stream).await?;
        if reply == "DATA" {
            send(&mut stream, b"DATA\r\n").await?;
            reply = read_line(&mut stream).await?;
        }
        let guid = reply
            .strip_prefix("OK ")
            .ok_or_else(|| format!("server rejected EXTERNAL: {reply:?}"))?
            .trim()
            .to_string();
        // zbus assumes fd passing on unix sockets; ask for it so the server
        // agrees (media never sends fds, so a refusal is harmless).
        send(&mut stream, b"NEGOTIATE_UNIX_FD\r\n").await?;
        let _ = read_line(&mut stream).await?;
        send(&mut stream, b"BEGIN\r\n").await?;
        Ok::<String, String>(guid)
    };
    let guid = tokio::time::timeout(std::time::Duration::from_secs(5), fut)
        .await
        .map_err(|_| "SASL handshake timed out".to_string())??;
    let conn = zbus::connection::Builder::authenticated_socket(stream, guid.as_str())
        .map_err(|e| format!("bad server guid {guid:?}: {e}"))?
        .build()
        .await
        .map_err(|e| format!("connection setup failed: {e}"))?;
    // A pre-authenticated socket skips zbus's own Hello; the bus refuses
    // every other call until it has been made.
    conn.call_method(
        Some("org.freedesktop.DBus"),
        "/org/freedesktop/DBus",
        Some("org.freedesktop.DBus"),
        "Hello",
        &(),
    )
    .await
    .map_err(|e| format!("Hello failed: {e}"))?;
    Ok(conn)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_plain_and_guid_addresses() {
        assert_eq!(parse_unix_path("unix:path=/run/user/1000/bus").as_deref(), Some("/run/user/1000/bus"));
        assert_eq!(
            parse_unix_path("unix:path=/tmp/b,guid=0123456789abcdef").as_deref(),
            Some("/tmp/b")
        );
        assert_eq!(
            parse_unix_path("unix:abstract=/tmp/x;unix:path=/run/bus").as_deref(),
            Some("/run/bus")
        );
    }

    #[test]
    fn non_path_addresses_are_skipped() {
        assert_eq!(parse_unix_path("unix:abstract=/tmp/x"), None);
        assert_eq!(parse_unix_path("tcp:host=localhost,port=1"), None);
    }

    #[test]
    fn percent_escapes_are_decoded() {
        assert_eq!(parse_unix_path("unix:path=/tmp/a%20b").as_deref(), Some("/tmp/a b"));
        assert_eq!(unescape("/tmp/%2"), "/tmp/%2", "truncated escape kept verbatim");
    }
}
