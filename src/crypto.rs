use std::fs;
use std::io::{self, Write};
use std::path::Path;

use base64::engine::general_purpose::STANDARD;
use base64::Engine;
use ring::aead::{Aad, LessSafeKey, Nonce, UnboundKey, AES_256_GCM, NONCE_LEN};
use ring::rand::{SecureRandom, SystemRandom};
use ring::{digest, hmac};

use crate::config::Config;

const KEY_FILE: &str = "secret.key";

/// AES-256-GCM key used to seal secrets stored in the database.
pub struct SecretKey(LessSafeKey);

impl SecretKey {
    /// Uses `SOURCELENS_SECRET_KEY`, else `<data_dir>/secret.key` (base64), creating it on first start.
    pub fn load(cfg: &Config) -> io::Result<Self> {
        if let Some(bytes) = cfg.secret_key {
            return Ok(Self::from_bytes(bytes));
        }
        let path = cfg.data_dir.join(KEY_FILE);
        match fs::read_to_string(&path) {
            Ok(text) => decode_key_file(&path, &text).map(Self::from_bytes),
            Err(err) if err.kind() == io::ErrorKind::NotFound => {
                let mut bytes = [0u8; 32];
                fill_random(&mut bytes);
                write_key_file(&path, &STANDARD.encode(bytes))?;
                Ok(Self::from_bytes(bytes))
            }
            Err(err) => Err(err),
        }
    }

    pub fn from_bytes(bytes: [u8; 32]) -> Self {
        let unbound = match UnboundKey::new(&AES_256_GCM, &bytes) {
            Ok(key) => key,
            Err(_) => unreachable!("AES-256-GCM accepts every 32-byte key"),
        };
        Self(LessSafeKey::new(unbound))
    }

    /// Returns `nonce (12 bytes) ‖ ciphertext ‖ tag`.
    pub fn seal(&self, plain: &[u8]) -> Vec<u8> {
        let mut nonce_bytes = [0u8; NONCE_LEN];
        fill_random(&mut nonce_bytes);
        let mut buffer = plain.to_vec();
        let sealed = self.0.seal_in_place_append_tag(
            Nonce::assume_unique_for_key(nonce_bytes),
            Aad::empty(),
            &mut buffer,
        );
        if sealed.is_err() {
            unreachable!("AES-256-GCM sealing fails only for inputs larger than 64 GiB");
        }
        let mut out = Vec::with_capacity(NONCE_LEN + buffer.len());
        out.extend_from_slice(&nonce_bytes);
        out.extend_from_slice(&buffer);
        out
    }

    pub fn open(&self, sealed: &[u8]) -> Option<Vec<u8>> {
        if sealed.len() < NONCE_LEN + AES_256_GCM.tag_len() {
            return None;
        }
        let (nonce_bytes, ciphertext) = sealed.split_at(NONCE_LEN);
        let nonce = Nonce::try_assume_unique_for_key(nonce_bytes).ok()?;
        let mut buffer = ciphertext.to_vec();
        let plain_len = self
            .0
            .open_in_place(nonce, Aad::empty(), &mut buffer)
            .ok()?
            .len();
        buffer.truncate(plain_len);
        Some(buffer)
    }
}

fn decode_key_file(path: &Path, text: &str) -> io::Result<[u8; 32]> {
    STANDARD
        .decode(text.trim())
        .ok()
        .and_then(|bytes| <[u8; 32]>::try_from(bytes).ok())
        .ok_or_else(|| {
            io::Error::new(
                io::ErrorKind::InvalidData,
                format!("{} must contain base64 of exactly 32 bytes", path.display()),
            )
        })
}

fn write_key_file(path: &Path, contents: &str) -> io::Result<()> {
    let mut options = fs::OpenOptions::new();
    options.write(true).create_new(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(0o600);
    }
    let mut file = options.open(path)?;
    file.write_all(contents.as_bytes())?;
    file.write_all(b"\n")?;
    file.sync_all()
}

fn fill_random(buf: &mut [u8]) {
    if SystemRandom::new().fill(buf).is_err() {
        panic!("the operating system random number generator failed");
    }
}

pub fn sha256_hex(data: &[u8]) -> String {
    hex(digest::digest(&digest::SHA256, data).as_ref())
}

/// Constant-time check of a hex HMAC-SHA256 signature; a leading `sha256=` is accepted.
pub fn hmac_sha256_verify(secret: &[u8], body: &[u8], hex_sig: &str) -> bool {
    let hex_sig = hex_sig.trim();
    let hex_sig = hex_sig.strip_prefix("sha256=").unwrap_or(hex_sig);
    let Some(signature) = unhex(hex_sig) else {
        return false;
    };
    let key = hmac::Key::new(hmac::HMAC_SHA256, secret);
    hmac::verify(&key, body, &signature).is_ok()
}

/// Git object id of a blob: `sha1("blob {len}\0" + content)`, hex.
pub fn git_blob_sha1(content: &[u8]) -> String {
    let mut ctx = digest::Context::new(&digest::SHA1_FOR_LEGACY_USE_ONLY);
    ctx.update(format!("blob {}\0", content.len()).as_bytes());
    ctx.update(content);
    hex(ctx.finish().as_ref())
}

pub fn random_hex(bytes: usize) -> String {
    let mut buf = vec![0u8; bytes];
    fill_random(&mut buf);
    hex(&buf)
}

pub fn hex(bytes: &[u8]) -> String {
    const DIGITS: &[u8; 16] = b"0123456789abcdef";
    let mut out = String::with_capacity(bytes.len() * 2);
    for byte in bytes {
        out.push(DIGITS[usize::from(byte >> 4)] as char);
        out.push(DIGITS[usize::from(byte & 0x0f)] as char);
    }
    out
}

pub fn unhex(s: &str) -> Option<Vec<u8>> {
    if !s.len().is_multiple_of(2) {
        return None;
    }
    fn nibble(c: u8) -> Option<u8> {
        match c {
            b'0'..=b'9' => Some(c - b'0'),
            b'a'..=b'f' => Some(c - b'a' + 10),
            b'A'..=b'F' => Some(c - b'A' + 10),
            _ => None,
        }
    }
    s.as_bytes()
        .chunks_exact(2)
        .map(|pair| Some(nibble(pair[0])? << 4 | nibble(pair[1])?))
        .collect()
}
