//! Vault file format + crypto for the `secrets` plugin.
//!
//! On-disk layout (one vault file per caller):
//!
//! ```text
//! 0..9    magic   b"VYNKORVLT"
//! 9       version u8 = 1
//! 10..22  nonce   [u8; 12]
//! 22..    payload ChaCha20-Poly1305 ciphertext of the secrets JSON map
//!                (the 16-byte AEAD tag is appended by the cipher)
//! ```
//!
//! The payload is the JSON serialization of the whole secrets map
//! (`{"name":"value",...}`). The vault is re-encrypted wholesale on every
//! write — files are small (a handful of credentials) and this keeps the
//! format trivial and auditable.
//!
//! Writes are atomic: encrypt to `{path}.tmp`, fsync, rename over the real
//! file, fsync the parent dir. The file is created with mode 0600. A
//! tampered or corrupted vault fails decryption loudly (AEAD tag mismatch) —
//! it is never silently re-created or returned as an empty map.

use std::collections::HashMap;
use std::fs::{self, File, OpenOptions};
use std::io::Write;
use std::os::unix::fs::OpenOptionsExt;
use std::path::{Path, PathBuf};

use chacha20poly1305::aead::{Aead, KeyInit};
use chacha20poly1305::{ChaCha20Poly1305, Key, Nonce};
use zeroize::Zeroizing;

/// File magic, ASCII "VYNKORVLT".
pub const MAGIC: &[u8; 9] = b"VYNKORVLT";
/// Current vault file version.
pub const VERSION: u8 = 1;
const NONCE_LEN: usize = 12;
const HEADER_LEN: usize = MAGIC.len() + 1 + NONCE_LEN;

/// Errors surfaced as `ACTION_ERROR` strings.
pub type VaultResult<T> = Result<T, String>;

/// An in-memory decrypted vault bound to its file path.
#[derive(Debug)]
pub struct Vault {
    path: PathBuf,
    secrets: HashMap<String, String>,
}

impl Vault {
    /// Load the vault at `path`, or create an empty one in memory if the
    /// file does not exist yet (nothing is written until the first mutation).
    ///
    /// Fails (rather than resets) if the file exists but cannot be decrypted.
    pub fn load_or_create(path: PathBuf, key: &[u8; 32]) -> VaultResult<Self> {
        let secrets = match fs::read(&path) {
            Ok(raw) => decrypt(&raw, key)?,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => HashMap::new(),
            Err(e) => return Err(format!("failed to read vault {}: {e}", path.display())),
        };
        Ok(Self { path, secrets })
    }

    /// Re-encrypt the current secrets map to disk, atomically.
    pub fn persist(&self, key: &[u8; 32]) -> VaultResult<()> {
        let raw = encrypt(&self.secrets, key)?;
        atomic_write(&self.path, &raw)?;
        Ok(())
    }

    pub fn get(&self, name: &str) -> Option<&str> {
        self.secrets.get(name).map(String::as_str)
    }

    pub fn insert(&mut self, name: &str, value: String) {
        self.secrets.insert(name.to_string(), value);
    }

    pub fn remove(&mut self, name: &str) -> bool {
        self.secrets.remove(name).is_some()
    }

    pub fn names(&self) -> Vec<String> {
        let mut names: Vec<String> = self.secrets.keys().cloned().collect();
        names.sort();
        names
    }
}

/// Encrypt a secrets map into the on-disk vault encoding.
fn encrypt(secrets: &HashMap<String, String>, key: &[u8; 32]) -> VaultResult<Vec<u8>> {
    let payload =
        serde_json::to_vec(secrets).map_err(|e| format!("failed to serialize vault: {e}"))?;

    let mut nonce_bytes = [0u8; NONCE_LEN];
    getrandom::fill(&mut nonce_bytes).map_err(|e| format!("failed to generate nonce: {e}"))?;

    let cipher = ChaCha20Poly1305::new(Key::from_slice(key));
    let ciphertext = cipher
        .encrypt(Nonce::from_slice(&nonce_bytes), payload.as_ref())
        .map_err(|_| "encryption failed".to_string())?;

    let mut raw = Vec::with_capacity(HEADER_LEN + ciphertext.len());
    raw.extend_from_slice(MAGIC);
    raw.push(VERSION);
    raw.extend_from_slice(&nonce_bytes);
    raw.extend_from_slice(&ciphertext);
    Ok(raw)
}

/// Decrypt a vault file. Fails loudly on any structural or AEAD violation.
fn decrypt(raw: &[u8], key: &[u8; 32]) -> VaultResult<HashMap<String, String>> {
    if raw.len() < HEADER_LEN {
        return Err("vault file too short".to_string());
    }
    if &raw[0..MAGIC.len()] != MAGIC {
        return Err("vault file has invalid magic".to_string());
    }
    if raw[MAGIC.len()] != VERSION {
        return Err(format!("unsupported vault version {}", raw[MAGIC.len()]));
    }

    let nonce = &raw[MAGIC.len() + 1..HEADER_LEN];
    let ciphertext = &raw[HEADER_LEN..];

    let cipher = ChaCha20Poly1305::new(Key::from_slice(key));
    let payload = cipher
        .decrypt(Nonce::from_slice(nonce), ciphertext)
        .map_err(|_| "vault decryption failed (wrong master key or tampered file)".to_string())?;

    serde_json::from_slice(&payload)
        .map_err(|e| format!("vault payload is not a valid secrets map: {e}"))
}

/// Write `raw` to `path` atomically: temp file in the same dir → fsync →
/// rename → fsync dir. Mode 0600 on creation.
fn atomic_write(path: &Path, raw: &[u8]) -> VaultResult<()> {
    let dir = path
        .parent()
        .ok_or_else(|| format!("vault path {} has no parent dir", path.display()))?;

    let tmp_path = path.with_extension("vault.tmp");
    {
        let mut f = OpenOptions::new()
            .write(true)
            .create(true)
            .truncate(true)
            .mode(0o600)
            .open(&tmp_path)
            .map_err(|e| format!("failed to create {}: {e}", tmp_path.display()))?;
        f.write_all(raw)
            .map_err(|e| format!("failed to write {}: {e}", tmp_path.display()))?;
        f.sync_all()
            .map_err(|e| format!("failed to fsync {}: {e}", tmp_path.display()))?;
    }
    fs::rename(&tmp_path, path).map_err(|e| {
        format!(
            "failed to rename {} -> {}: {e}",
            tmp_path.display(),
            path.display()
        )
    })?;

    if let Ok(d) = File::open(dir) {
        let _ = d.sync_all();
    }
    Ok(())
}

/// Parse the master key env value: 64 hex chars or 44 base64 chars, both
/// decoding to exactly 32 bytes. The raw string is zeroized after parsing.
pub fn parse_master_key(raw: &str) -> VaultResult<[u8; 32]> {
    let raw = Zeroizing::new(raw.trim().to_string());
    let bytes = if raw.len() == 64 && raw.bytes().all(|b| b.is_ascii_hexdigit()) {
        decode_hex(raw.as_bytes()).ok_or("invalid hex master key")?
    } else if raw.len() == 44 {
        use base64::Engine as _;
        base64::engine::general_purpose::STANDARD
            .decode(raw.as_bytes())
            .map_err(|_| "invalid base64 master key")?
    } else {
        return Err(format!(
            "master key must be 64 hex chars or 44 base64 chars (32 bytes), got {} chars",
            raw.len()
        ));
    };

    let mut key = [0u8; 32];
    if bytes.len() != 32 {
        return Err(format!(
            "master key decodes to {} bytes, expected 32",
            bytes.len()
        ));
    }
    key.copy_from_slice(&bytes);
    Ok(key)
}

fn decode_hex(s: &[u8]) -> Option<Vec<u8>> {
    let mut out = Vec::with_capacity(s.len() / 2);
    let mut i = 0;
    while i + 1 < s.len() {
        let hi = hex_val(s[i])?;
        let lo = hex_val(s[i + 1])?;
        out.push((hi << 4) | lo);
        i += 2;
    }
    Some(out)
}

fn hex_val(b: u8) -> Option<u8> {
    match b {
        b'0'..=b'9' => Some(b - b'0'),
        b'a'..=b'f' => Some(b - b'a' + 10),
        b'A'..=b'F' => Some(b - b'A' + 10),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn test_key() -> [u8; 32] {
        [7u8; 32]
    }

    #[test]
    fn roundtrip_encrypt_decrypt() {
        let mut map = HashMap::new();
        map.insert("api_key".to_string(), "sk-ant-secret".to_string());
        map.insert("token".to_string(), "abc123".to_string());
        let raw = encrypt(&map, &test_key()).unwrap();
        let dec = decrypt(&raw, &test_key()).unwrap();
        assert_eq!(dec, map);
    }

    #[test]
    fn fresh_nonce_each_encryption() {
        let mut map = HashMap::new();
        map.insert("a".to_string(), "b".to_string());
        let r1 = encrypt(&map, &test_key()).unwrap();
        let r2 = encrypt(&map, &test_key()).unwrap();
        assert_ne!(r1, r2, "nonce must be fresh per encryption");
    }

    #[test]
    fn wrong_key_fails() {
        let mut map = HashMap::new();
        map.insert("a".to_string(), "b".to_string());
        let raw = encrypt(&map, &test_key()).unwrap();
        let wrong = [9u8; 32];
        assert!(decrypt(&raw, &wrong).is_err());
    }

    #[test]
    fn tampered_payload_fails() {
        let mut map = HashMap::new();
        map.insert("a".to_string(), "b".to_string());
        let mut raw = encrypt(&map, &test_key()).unwrap();
        let last = raw.len() - 1;
        raw[last] ^= 0x01;
        assert!(decrypt(&raw, &test_key()).is_err());
    }

    #[test]
    fn rejects_bad_magic_and_version() {
        let mut map = HashMap::new();
        map.insert("a".to_string(), "b".to_string());
        let mut raw = encrypt(&map, &test_key()).unwrap();

        raw[0] = b'X';
        assert!(decrypt(&raw, &test_key()).is_err());

        let raw = encrypt(&map, &test_key()).unwrap();
        let mut raw2 = raw.clone();
        raw2[MAGIC.len()] = 99;
        assert!(decrypt(&raw2, &test_key()).is_err());
    }

    #[test]
    fn rejects_short_input() {
        assert!(decrypt(b"VYNKORVLT", &test_key()).is_err());
    }

    #[test]
    fn parse_master_key_hex_and_base64() {
        let hex = "0102030405060708090a0b0c0d0e0f101112131415161718191a1b1c1d1e1f20";
        let key = parse_master_key(hex).unwrap();
        assert_eq!(key[0], 0x01);
        assert_eq!(key[31], 0x20);

        let b64 = "AQIDBAUGBwgJCgsMDQ4PEBESExQVFhcYGRobHB0eHyA=";
        assert_eq!(parse_master_key(b64).unwrap(), key);

        assert!(parse_master_key("too-short").is_err());
        assert!(parse_master_key(&"z".repeat(64)).is_err()); // 'z' is not hex
    }

    #[test]
    fn vault_load_missing_creates_empty() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("caller.vault");
        let v = Vault::load_or_create(path.clone(), &test_key()).unwrap();
        assert!(v.names().is_empty());
        assert!(!path.exists(), "no file written until first mutation");
    }

    #[test]
    fn vault_persist_roundtrip_atomic() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("caller.vault");

        let mut v = Vault::load_or_create(path.clone(), &test_key()).unwrap();
        v.insert("k", "v".to_string());
        v.persist(&test_key()).unwrap();

        let v2 = Vault::load_or_create(path.clone(), &test_key()).unwrap();
        assert_eq!(v2.get("k"), Some("v"));
        assert!(
            !dir.path().join("caller.vault.tmp").exists(),
            "tmp file cleaned up"
        );
    }

    #[test]
    fn vault_persist_creates_0600() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("caller.vault");
        let mut v = Vault::load_or_create(path.clone(), &test_key()).unwrap();
        v.insert("k", "v".to_string());
        v.persist(&test_key()).unwrap();

        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mode = fs::metadata(&path).unwrap().permissions().mode();
            assert_eq!(mode & 0o777, 0o600, "vault must be 0600");
        }
    }

    #[test]
    fn tampered_vault_file_fails_load() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("caller.vault");
        let mut v = Vault::load_or_create(path.clone(), &test_key()).unwrap();
        v.insert("k", "v".to_string());
        v.persist(&test_key()).unwrap();

        let mut raw = fs::read(&path).unwrap();
        let last = raw.len() - 1;
        raw[last] ^= 0x01;
        fs::write(&path, raw).unwrap();

        assert!(Vault::load_or_create(path.clone(), &test_key()).is_err());
    }
}
