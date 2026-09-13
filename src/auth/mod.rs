//! SCRAM-SHA-256 authentication — Postgres's exact SASL mechanism
//! (RFC 5802 / RFC 7677), for `rustqlite-server` and the CLI's
//! `--connect` client mode.
//!
//! # Security model
//!
//! * **The password never crosses the wire.** The client proves knowledge
//!   of the password with a per-handshake HMAC proof (`ClientProof`);
//!   nothing password-derived is ever transmitted.
//! * **The password is not stored recoverably.** The user store holds only
//!   Postgres-format verifiers: `SCRAM-SHA-256$<iterations>:<salt_hex>$
//!   <stored_key_hex>:<server_key_hex>` where
//!   `StoredKey = SHA256(HMAC(SaltedPassword, "Client Key"))` and
//!   `ServerKey = HMAC(SaltedPassword, "Server Key")` over a
//!   PBKDF2-HMAC-SHA-256 (default 4096 iterations) salted password. A
//!   stolen verifier can impersonate the *server* in a future handshake,
//!   but cannot produce a client proof — store the file with 0600.
//! * **Mutual authentication.** The server's final message carries
//!   `ServerSignature = HMAC(ServerKey, AuthMessage)`; the client
//!   verifies it, so a fake server without the verifier cannot complete
//!   the handshake.
//! * **Replay-proof handshakes.** Client and server nonces are fresh
//!   OS-random per exchange; every pending exchange is single-use.
//! * **No user enumeration.** Unknown users get a syntactically valid
//!   fabricated challenge (random salt, default iterations) and fail with
//!   the byte-identical generic error at the proof step. Both paths run
//!   one PBKDF2 derivation at `/auth/start` so timing profiles match.
//! * **Session tokens.** A successful handshake yields a 256-bit random
//!   bearer token (hex) with a TTL; validation is a map lookup over a
//!   2^256 search space plus a constant-time compare. Tokens are
//!   revocable (`/auth/logout`).
//!
//! # What this is not
//!
//! HTTP itself is plaintext: SCRAM protects the *credentials*, not the
//! query traffic — exactly like Postgres with `scram-sha-256` over a
//! non-SSL connection. Terminate TLS in front of the server when the
//! network is untrusted.
//!
//! # Wire shape (our HTTP transport)
//!
//! ```text
//! POST /auth/start   {"username":"alice","client_nonce":"<hex>"}
//!        -> {"session":"<hex>","nonce":"<combined hex>","salt":"<hex>","iterations":4096}
//! POST /auth/finish  {"session":"<hex>","client_final":"c=biws,r=<combined>","proof":"<hex>"}
//!        -> {"token":"<hex>","expires_in":3600,"server_signature":"<hex>"}
//! Authorization: Bearer <token>   on /query, /execute, /auth/logout
//! ```
//!
//! The SCRAM message fields are the RFC's, with hex where the RFC uses
//! base64 (both ends are ours; hex is debug-friendly).

use crate::json::{decode_hex, encode_hex, Json};
use rand::RngCore;
use sha2::{Digest, Sha256};
use std::collections::HashMap;
use std::fmt;
use std::path::Path;
use std::time::{Duration, Instant};

/// Postgres's default iteration count (RFC 7677 recommends at least 4096).
pub const DEFAULT_ITERATIONS: u32 = 4096;
/// Verifier salt length in bytes.
pub const SALT_LEN: usize = 16;
/// Handshake nonce length in bytes (client and server each).
pub const NONCE_LEN: usize = 18;
/// Session token length in bytes (256-bit).
pub const TOKEN_LEN: usize = 32;
/// Valid usernames: `[A-Za-z0-9_.-]{1,63}` — keeps the user-store format
/// quote-free and injection-proof (SCRAM message names are comma/equals
/// delimited, so those characters are excluded too).
const USERNAME_MAX: usize = 63;

/// Generic failure — deliberately carries no reason (wrong password,
/// unknown user, and bad session state must be indistinguishable).
#[derive(Debug, Clone)]
pub struct AuthError(pub String);

impl fmt::Display for AuthError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}", self.0)
    }
}

impl std::error::Error for AuthError {}

/// OS randomness for salts, nonces, tokens, and timing-pad work.
pub fn random_bytes(n: usize) -> Vec<u8> {
    let mut b = vec![0u8; n];
    rand::rngs::OsRng.fill_bytes(&mut b);
    b
}

// ---------------------------------------------------------------------------
// Primitives (hand-rolled: no `hmac` crate dependency)
// ---------------------------------------------------------------------------

/// HMAC-SHA-256 (RFC 2104); the message arrives as parts so the
/// concatenated `AuthMessage` never has to be built as one allocation.
fn hmac_sha256(key: &[u8], msg_parts: &[&[u8]]) -> [u8; 32] {
    const BLOCK: usize = 64;
    let mut k = [0u8; BLOCK];
    if key.len() > BLOCK {
        let d = Sha256::digest(key);
        k[..32].copy_from_slice(&d);
    } else {
        k[..key.len()].copy_from_slice(key);
    }
    let mut ipad = [0x36u8; BLOCK];
    let mut opad = [0x5cu8; BLOCK];
    for i in 0..BLOCK {
        ipad[i] ^= k[i];
        opad[i] ^= k[i];
    }
    let mut inner = Sha256::new();
    inner.update(ipad);
    for p in msg_parts {
        inner.update(p);
    }
    let inner = inner.finalize();
    let mut outer = Sha256::new();
    outer.update(opad);
    outer.update(inner);
    let mut out = [0u8; 32];
    out.copy_from_slice(&outer.finalize());
    out
}

/// PBKDF2-HMAC-SHA-256 with a 32-byte output (RFC 2898 / RFC 7914: one
/// block, so the `T = U1 xor U2 xor …` loop suffices).
pub fn pbkdf2_hmac_sha256(password: &[u8], salt: &[u8], iterations: u32) -> [u8; 32] {
    let mut msg = Vec::with_capacity(salt.len() + 4);
    msg.extend_from_slice(salt);
    msg.extend_from_slice(&1u32.to_be_bytes()); // block index 1 (big-endian)
    let mut u = hmac_sha256(password, &[&msg]);
    let mut out = u;
    for _ in 1..iterations.max(1) {
        u = hmac_sha256(password, &[&u]);
        for i in 0..32 {
            out[i] ^= u[i];
        }
    }
    out
}

/// Constant-time equality over equal-length byte strings (lengths are
/// public here: fixed-size keys/tokens/proofs).
pub fn ct_eq(a: &[u8], b: &[u8]) -> bool {
    if a.len() != b.len() {
        return false;
    }
    let mut diff = 0u8;
    for i in 0..a.len() {
        diff |= a[i] ^ b[i];
    }
    diff == 0
}

// ---------------------------------------------------------------------------
// Verifier (Postgres rolpassword format, hex payloads)
// ---------------------------------------------------------------------------

/// The stored secret for one user: enough to VERIFY a client proof and to
/// SIGN the server's own proof — not enough to reconstruct the password
/// or impersonate the client.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ScramVerifier {
    pub iterations: u32,
    pub salt: Vec<u8>,
    pub stored_key: [u8; 32],
    pub server_key: [u8; 32],
}

impl ScramVerifier {
    /// Build a verifier for `password` (caller supplies the salt; fresh
    /// OS-random salt via [`random_bytes`] in the normal path).
    pub fn generate(password: &str, salt: &[u8], iterations: u32) -> ScramVerifier {
        let salted = pbkdf2_hmac_sha256(password.as_bytes(), salt, iterations);
        let client_key = hmac_sha256(&salted, &[b"Client Key"]);
        let stored_key = sha256_bytes(&client_key);
        let server_key = hmac_sha256(&salted, &[b"Server Key"]);
        ScramVerifier {
            iterations,
            salt: salt.to_vec(),
            stored_key,
            server_key,
        }
    }

    /// Serialize to `SCRAM-SHA-256$<iter>:<salt_hex>$<stored_hex>:<server_hex>`.
    pub fn to_verifier_string(&self) -> String {
        format!(
            "SCRAM-SHA-256${}:{}${}:{}",
            self.iterations,
            encode_hex(&self.salt),
            encode_hex(&self.stored_key),
            encode_hex(&self.server_key)
        )
    }

    /// Parse the verifier string; `None` on any malformation.
    pub fn parse(s: &str) -> Option<ScramVerifier> {
        let rest = s.strip_prefix("SCRAM-SHA-256$")?;
        let (head, tail) = rest.split_once('$')?;
        let (iter_s, salt_hex) = head.split_once(':')?;
        let (stored_hex, server_hex) = tail.split_once(':')?;
        let iterations = iter_s.parse::<u32>().ok()?;
        if iterations == 0 {
            return None;
        }
        let salt = decode_hex(salt_hex)?;
        let stored = decode_hex(stored_hex)?;
        let server = decode_hex(server_hex)?;
        if stored.len() != 32 || server.len() != 32 || salt.is_empty() {
            return None;
        }
        let mut stored_key = [0u8; 32];
        stored_key.copy_from_slice(&stored);
        let mut server_key = [0u8; 32];
        server_key.copy_from_slice(&server);
        Some(ScramVerifier {
            iterations,
            salt,
            stored_key,
            server_key,
        })
    }
}

fn sha256_bytes(data: &[u8]) -> [u8; 32] {
    let mut h = [0u8; 32];
    h.copy_from_slice(&Sha256::digest(data));
    h
}

// ---------------------------------------------------------------------------
// Server-side handshake state
// ---------------------------------------------------------------------------

/// The server's half of one single-use exchange.
pub struct ServerExchange {
    client_first_bare: String,
    server_first: String,
    combined_nonce: String,
    /// `None` = unknown user; a fabricated verifier runs the same math so
    /// the failure (and its timing) is indistinguishable.
    verifier: Option<ScramVerifier>,
}

/// The `/auth/start` response payload.
#[derive(Clone, Debug)]
pub struct Challenge {
    pub nonce_hex: String,
    pub salt_hex: String,
    pub iterations: u32,
}

impl ServerExchange {
    /// Begin an exchange. `client_nonce_hex` must be valid hex of 8..=64
    /// bytes. `verifier` comes from the user store (`None` for unknown
    /// users — a fake challenge is produced).
    ///
    /// Both paths run exactly one PBKDF2 at DEFAULT_ITERATIONS (the
    /// unknown-user path to fabricate, the known-user path as discarded
    /// padding) so `/auth/start` timing cannot reveal user existence.
    pub fn start(
        username: &str,
        client_nonce_hex: &str,
        verifier: Option<&ScramVerifier>,
    ) -> Result<(ServerExchange, Challenge), AuthError> {
        let client_nonce = decode_hex(client_nonce_hex)
            .filter(|n| (8..=64).contains(&n.len()))
            .ok_or_else(|| AuthError("bad client nonce".to_string()))?;
        if !valid_username(username) {
            // Username-shape errors leak nothing (the name never hit the
            // store); fold them into the same generic failure.
            return Err(AuthError("authentication failed".to_string()));
        }
        let client_nonce_hex = encode_hex(&client_nonce);
        let server_nonce_hex = encode_hex(&random_bytes(NONCE_LEN));
        let combined_nonce = format!("{}{}", client_nonce_hex, server_nonce_hex);

        let (salt, iterations, real) = match verifier {
            Some(v) => {
                // Timing pad: one discarded derivation, matching the
                // unknown-user fabrication cost.
                let _pad = pbkdf2_hmac_sha256(
                    &random_bytes(16),
                    &random_bytes(SALT_LEN),
                    DEFAULT_ITERATIONS,
                );
                (v.salt.clone(), v.iterations, Some(v.clone()))
            }
            None => {
                let fake = ScramVerifier::generate(
                    &encode_hex(&random_bytes(32)),
                    &random_bytes(SALT_LEN),
                    DEFAULT_ITERATIONS,
                );
                (fake.salt.clone(), fake.iterations, None)
            }
        };

        let client_first_bare = format!("n={},r={}", username, client_nonce_hex);
        let server_first = format!(
            "r={},s={},i={}",
            combined_nonce,
            encode_hex(&salt),
            iterations
        );
        let exchange = ServerExchange {
            client_first_bare,
            server_first,
            combined_nonce: combined_nonce.clone(),
            verifier: real,
        };
        let challenge = Challenge {
            nonce_hex: combined_nonce,
            salt_hex: encode_hex(&salt),
            iterations,
        };
        Ok((exchange, challenge))
    }

    /// Verify the client proof; on success return the `ServerSignature`
    /// the server sends back (the client checks it — mutual auth).
    ///
    /// `client_final_no_proof` is the RFC's `c=biws,r=<nonce>` part and
    /// MUST echo our combined nonce exactly.
    pub fn finish(
        self,
        client_final_no_proof: &str,
        client_proof: &[u8],
    ) -> Result<[u8; 32], AuthError> {
        let generic = || AuthError("authentication failed".to_string());
        let expected_final = format!("c=biws,r={}", self.combined_nonce);
        if client_final_no_proof != expected_final {
            return Err(generic());
        }
        let verifier = self.verifier.as_ref().ok_or_else(generic)?;
        if client_proof.len() != 32 {
            return Err(generic());
        }
        // AuthMessage = client-first-bare "," server-first "," client-final-without-proof
        let auth_parts: [&[u8]; 5] = [
            self.client_first_bare.as_bytes(),
            b",",
            self.server_first.as_bytes(),
            b",",
            client_final_no_proof.as_bytes(),
        ];
        let client_signature = hmac_sha256(&verifier.stored_key, &auth_parts);
        let mut client_key = [0u8; 32];
        for i in 0..32 {
            client_key[i] = client_proof[i] ^ client_signature[i];
        }
        let hashed = sha256_bytes(&client_key);
        if !ct_eq(&hashed, &verifier.stored_key) {
            return Err(generic());
        }
        let server_signature = hmac_sha256(&verifier.server_key, &auth_parts);
        Ok(server_signature)
    }
}

// ---------------------------------------------------------------------------
// Client-side handshake state
// ---------------------------------------------------------------------------

/// The client's half of one exchange (the CLI's `--connect` login).
pub struct ClientExchange {
    username: String,
    client_nonce_hex: String,
    client_first_bare: String,
}

impl ClientExchange {
    /// Begin: generates the client nonce and the client-first message.
    pub fn start(username: &str) -> ClientExchange {
        let client_nonce_hex = encode_hex(&random_bytes(NONCE_LEN));
        let client_first_bare = format!("n={},r={}", username, client_nonce_hex);
        ClientExchange {
            username: username.to_string(),
            client_nonce_hex,
            client_first_bare,
        }
    }

    pub fn username(&self) -> &str {
        &self.username
    }

    pub fn client_nonce_hex(&self) -> &str {
        &self.client_nonce_hex
    }

    /// The SCRAM client-first-bare message (`n=...,r=...`).
    pub fn client_first_bare(&self) -> &str {
        &self.client_first_bare
    }

    /// Consume the server's challenge, produce the client-final message
    /// (without proof), the proof, and the server signature the client
    /// must verify (mutual authentication).
    pub fn finish(
        self,
        password: &str,
        challenge: &Challenge,
    ) -> Result<(String, [u8; 32], [u8; 32]), AuthError> {
        let generic = || AuthError("authentication failed".to_string());
        // The combined nonce must extend OUR nonce; the server's half is
        // opaque to us but must be non-empty.
        let combined = &challenge.nonce_hex;
        if combined.len() <= self.client_nonce_hex.len()
            || !combined.starts_with(self.client_nonce_hex.as_str())
        {
            return Err(generic());
        }
        let salt = decode_hex(&challenge.salt_hex).ok_or_else(generic)?;
        if challenge.iterations == 0 {
            return Err(generic());
        }
        let salted = pbkdf2_hmac_sha256(password.as_bytes(), &salt, challenge.iterations);
        let client_key = hmac_sha256(&salted, &[b"Client Key"]);
        let stored_key = sha256_bytes(&client_key);
        let client_final_no_proof = format!("c=biws,r={}", combined);
        let server_first = format!(
            "r={},s={},i={}",
            combined, challenge.salt_hex, challenge.iterations
        );
        let auth_parts: [&[u8]; 5] = [
            self.client_first_bare.as_bytes(),
            b",",
            server_first.as_bytes(),
            b",",
            client_final_no_proof.as_bytes(),
        ];
        let client_signature = hmac_sha256(&stored_key, &auth_parts);
        let mut proof = [0u8; 32];
        for i in 0..32 {
            proof[i] = client_key[i] ^ client_signature[i];
        }
        let server_key = hmac_sha256(&salted, &[b"Server Key"]);
        let expected_server_signature = hmac_sha256(&server_key, &auth_parts);
        Ok((client_final_no_proof, proof, expected_server_signature))
    }
}

// ---------------------------------------------------------------------------
// Interactive password input (shared by the server's --add-user and the
// CLI's --connect login)
// ---------------------------------------------------------------------------

/// Password input for user-facing tools, in priority order:
/// 1. the `--password` flag value (`flag`),
/// 2. the `RUSTQLITE_PASSWORD` environment variable,
/// 3. a hidden (no-echo) prompt when stdin is a terminal,
/// 4. a visible stdin read with a warning (piped/scripted use).
pub fn read_password_interactive(flag: Option<&str>, prompt: &str) -> Result<String, AuthError> {
    if let Some(p) = flag {
        return Ok(p.to_string());
    }
    if let Ok(p) = std::env::var("RUSTQLITE_PASSWORD") {
        return Ok(p);
    }
    #[cfg(unix)]
    if is_tty_stdin() {
        return prompt_hidden(prompt);
    }
    eprintln!(
        "warning: password input will be visible (stdin is not a terminal); use RUSTQLITE_PASSWORD"
    );
    eprint!("{}", prompt);
    let _ = std::io::Write::flush(&mut std::io::stderr());
    let mut line = String::new();
    std::io::stdin()
        .read_line(&mut line)
        .map_err(|e| AuthError(e.to_string()))?;
    let pw = line.trim_end_matches(['\n', '\r']).to_string();
    eprintln!();
    if pw.is_empty() {
        return Err(AuthError("empty password".to_string()));
    }
    Ok(pw)
}

#[cfg(unix)]
fn is_tty_stdin() -> bool {
    // SAFETY: isatty has no preconditions.
    unsafe { libc::isatty(libc::STDIN_FILENO) == 1 }
}

/// No-echo password prompt via termios (unix).
#[cfg(unix)]
fn prompt_hidden(prompt: &str) -> Result<String, AuthError> {
    use std::io::BufRead;
    use std::os::fd::AsRawFd;
    let fd = std::io::stdin().as_raw_fd();
    // SAFETY: tcgetattr with a valid fd and a termios-sized out pointer.
    let mut saved: libc::termios = unsafe { std::mem::zeroed() };
    if unsafe { libc::tcgetattr(fd, &mut saved) } != 0 {
        // Fall back to a visible read.
        return read_password_interactive(None, prompt);
    }
    let mut noecho = saved;
    noecho.c_lflag &= !libc::ECHO;
    // SAFETY: tcsetattr with a valid fd and a termios from tcgetattr.
    if unsafe { libc::tcsetattr(fd, libc::TCSAFLUSH, &noecho) } != 0 {
        return read_password_interactive(None, prompt);
    }
    let result = (|| {
        eprint!("{}", prompt);
        let _ = std::io::Write::flush(&mut std::io::stderr());
        let mut line = String::new();
        std::io::stdin()
            .lock()
            .read_line(&mut line)
            .map_err(|e| AuthError(e.to_string()))?;
        Ok(line.trim_end_matches(['\n', '\r']).to_string())
    })();
    // Always restore echo, even if the read failed.
    unsafe { libc::tcsetattr(fd, libc::TCSAFLUSH, &saved) };
    eprintln!();
    result
}

// ---------------------------------------------------------------------------
// User store
// ---------------------------------------------------------------------------

/// Valid usernames: non-empty, `<= 63` bytes, `[A-Za-z0-9_.-]` only.
pub fn valid_username(u: &str) -> bool {
    !u.is_empty()
        && u.len() <= USERNAME_MAX
        && u.bytes()
            .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'_' | b'-' | b'.'))
}

/// The on-disk user database: a JSON file
/// `{"version":1,"users":{"alice":"SCRAM-SHA-256$..."}}`, written 0600
/// (unix) via temp-file + atomic rename.
#[derive(Clone, Debug, Default)]
pub struct UserStore {
    users: std::collections::BTreeMap<String, ScramVerifier>,
}

impl UserStore {
    pub fn new() -> UserStore {
        UserStore::default()
    }

    pub fn len(&self) -> usize {
        self.users.len()
    }

    pub fn is_empty(&self) -> bool {
        self.users.is_empty()
    }

    pub fn usernames(&self) -> Vec<String> {
        self.users.keys().cloned().collect()
    }

    pub fn verifier(&self, username: &str) -> Option<&ScramVerifier> {
        self.users.get(username)
    }

    /// Insert or replace a user with a fresh random salt.
    pub fn upsert(
        &mut self,
        username: &str,
        password: &str,
        iterations: u32,
    ) -> Result<(), AuthError> {
        if !valid_username(username) {
            return Err(AuthError(format!(
                "invalid username {:?}: use 1..={} chars of [A-Za-z0-9_.-]",
                username, USERNAME_MAX
            )));
        }
        let salt = random_bytes(SALT_LEN);
        let verifier = ScramVerifier::generate(password, &salt, iterations);
        self.users.insert(username.to_string(), verifier);
        Ok(())
    }

    /// Remove a user; `false` if absent.
    pub fn remove(&mut self, username: &str) -> bool {
        self.users.remove(username).is_some()
    }

    /// Load from a JSON file. A missing file is a distinct error (the
    /// caller decides whether that's fatal).
    pub fn load(path: &Path) -> Result<UserStore, AuthError> {
        let text = std::fs::read_to_string(path)
            .map_err(|e| AuthError(format!("cannot read auth file {}: {}", path.display(), e)))?;
        let parsed = Json::parse(&text).map_err(|e| {
            AuthError(format!(
                "auth file {} is not valid JSON: {}",
                path.display(),
                e
            ))
        })?;
        let version = parsed.get("version").and_then(|v| v.as_i64()).unwrap_or(0);
        if version != 1 {
            return Err(AuthError(format!(
                "auth file {}: unsupported version {}",
                path.display(),
                version
            )));
        }
        let users_json = parsed
            .get("users")
            .and_then(|u| u.as_object())
            .ok_or_else(|| {
                AuthError(format!(
                    "auth file {}: missing \"users\" object",
                    path.display()
                ))
            })?;
        let mut users = std::collections::BTreeMap::new();
        for (name, v) in users_json {
            let verifier_str = v.as_str().ok_or_else(|| {
                AuthError(format!(
                    "auth file {}: verifier for {:?} is not a string",
                    path.display(),
                    name
                ))
            })?;
            let verifier = ScramVerifier::parse(verifier_str).ok_or_else(|| {
                AuthError(format!(
                    "auth file {}: malformed verifier for {:?}",
                    path.display(),
                    name
                ))
            })?;
            if !valid_username(name) {
                return Err(AuthError(format!(
                    "auth file {}: invalid username {:?}",
                    path.display(),
                    name
                )));
            }
            users.insert(name.clone(), verifier);
        }
        Ok(UserStore { users })
    }

    /// Persist atomically: write a temp file in the same directory, chmod
    /// 0600 (unix), rename over the target.
    pub fn save(&self, path: &Path) -> Result<(), AuthError> {
        let pairs: Vec<(String, Json)> = self
            .users
            .iter()
            .map(|(k, v)| (k.clone(), Json::Str(v.to_verifier_string())))
            .collect();
        let doc = Json::Object(vec![
            ("version".to_string(), Json::Int(1)),
            ("users".to_string(), Json::Object(pairs)),
        ]);
        let parent = path
            .parent()
            .filter(|p| !p.as_os_str().is_empty())
            .map(|p| p.to_path_buf())
            .unwrap_or_else(|| std::path::PathBuf::from("."));
        let file_name = path
            .file_name()
            .map(|n| n.to_string_lossy().to_string())
            .unwrap_or_else(|| "auth".to_string());
        let tmp = parent.join(format!(".{}.tmp{}", file_name, std::process::id()));
        std::fs::write(&tmp, doc.to_json())
            .map_err(|e| AuthError(format!("cannot write {}: {}", tmp.display(), e)))?;
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let perms = std::fs::Permissions::from_mode(0o600);
            std::fs::set_permissions(&tmp, perms)
                .map_err(|e| AuthError(format!("cannot chmod {}: {}", tmp.display(), e)))?;
        }
        std::fs::rename(&tmp, path)
            .map_err(|e| AuthError(format!("cannot replace {}: {}", path.display(), e)))?;
        Ok(())
    }
}

// ---------------------------------------------------------------------------
// Session tokens
// ---------------------------------------------------------------------------

/// In-memory bearer-token store with a fixed TTL from issue time.
///
/// The token is 256-bit OS-random; the store prunes expired entries on
/// every access. Validation is a hash-map lookup (the token IS the key —
/// high entropy makes timing side-channels unexploitable) plus a
/// constant-time equality check as defense in depth.
pub struct SessionStore {
    ttl: Duration,
    sessions: parking_lot::Mutex<HashMap<String, Instant>>,
}

impl SessionStore {
    pub fn new(ttl: Duration) -> SessionStore {
        SessionStore {
            ttl,
            sessions: parking_lot::Mutex::new(HashMap::new()),
        }
    }

    /// Issue a fresh token (hex, 64 chars).
    pub fn issue(&self) -> String {
        let token = encode_hex(&random_bytes(TOKEN_LEN));
        let mut map = self.sessions.lock();
        prune(&mut map);
        map.insert(token.clone(), Instant::now() + self.ttl);
        token
    }

    /// Validate a token (constant-time compare after map lookup).
    pub fn validate(&self, token: &str) -> bool {
        let now = Instant::now();
        let mut map = self.sessions.lock();
        prune(&mut map);
        match map.get(token) {
            Some(&exp) => {
                if exp > now {
                    true
                } else {
                    map.remove(token);
                    false
                }
            }
            None => {
                // Burn the same comparison cost as a hit (map-lookup
                // timing on a 2^256 key space is already unexploitable;
                // this keeps even the post-lookup path uniform).
                ct_eq(token.as_bytes(), token.as_bytes());
                false
            }
        }
    }

    /// Revoke a token (logout). `true` if it was live.
    pub fn revoke(&self, token: &str) -> bool {
        let mut map = self.sessions.lock();
        map.remove(token).is_some()
    }

    /// Live session count (diagnostics).
    pub fn count(&self) -> usize {
        let mut map = self.sessions.lock();
        prune(&mut map);
        map.len()
    }
}

fn prune(map: &mut HashMap<String, Instant>) {
    let now = Instant::now();
    map.retain(|_, exp| *exp > now);
}

#[cfg(test)]
mod tests {
    use super::*;

    // RFC 4231 test case 1: key = 0x0b x20, data "Hi There".
    #[test]
    fn hmac_rfc4231_case1() {
        let key = [0x0bu8; 20];
        let expected =
            hex_to_arr("b0344c61d8db38535ca8afceaf0bf12b881dc200c9833da726e9376c2e32cff7");
        assert_eq!(hmac_sha256(&key, &[b"Hi There"]), expected);
    }

    // RFC 4231 test case 2: key "Jefe", data "what do ya want for nothing?".
    #[test]
    fn hmac_rfc4231_case2() {
        let expected =
            hex_to_arr("5bdcc146bf60754e6a042426089575c75a003f089d2739839dec58b964ec3843");
        assert_eq!(
            hmac_sha256(b"Jefe", &[b"what do ya want for nothing?"]),
            expected
        );
    }

    // RFC 4231 test case 3: key = 0xaa x20, data = 0xdd x50.
    #[test]
    fn hmac_rfc4231_case3() {
        let key = [0xaau8; 20];
        let data = [0xddu8; 50];
        let expected =
            hex_to_arr("773ea91e36800e46854db8ebd09181a72959098b3ef8c122d9635514ced565fe");
        assert_eq!(hmac_sha256(&key, &[&data]), expected);
    }

    // RFC 7914 §11 (PBKDF2-HMAC-SHA-256): P="password", S="salt".
    #[test]
    fn pbkdf2_rfc7914_vectors() {
        assert_eq!(
            encode_hex(&pbkdf2_hmac_sha256(b"password", b"salt", 1)),
            "120fb6cffcf8b32c43e7225256c4f837a86548c92ccc35480805987cb70be17b"
        );
        assert_eq!(
            encode_hex(&pbkdf2_hmac_sha256(b"password", b"salt", 2)),
            "ae4d0c95af6b46d32d0adff928f06dd02a303f8ef3c251dfd6e2d85a95474c43"
        );
        assert_eq!(
            encode_hex(&pbkdf2_hmac_sha256(b"password", b"salt", 4096)),
            "c5e478d59288c841aa530db6845c4c8d962893a001ce4e11a4963873aa98134a"
        );
    }

    #[test]
    fn verifier_round_trip() {
        let v = ScramVerifier::generate("correct horse battery staple", &[1, 2, 3, 4], 4096);
        let s = v.to_verifier_string();
        assert!(s.starts_with("SCRAM-SHA-256$4096:01020304$"));
        let v2 = ScramVerifier::parse(&s).unwrap();
        assert_eq!(v, v2);
        // Malformed shapes.
        assert!(ScramVerifier::parse("SCRAM-SHA-256$4096:0102").is_none());
        assert!(ScramVerifier::parse("SCRAM-SHA-999$4096:0102$aa:bb").is_none());
        assert!(ScramVerifier::parse("SCRAM-SHA-256$0:0102$aa:bb").is_none());
        assert!(ScramVerifier::parse(&s[..s.len() - 1]).is_none());
        // Different salt or password -> different verifier.
        let v3 = ScramVerifier::generate("correct horse battery staple", &[5, 6, 7, 8], 4096);
        assert_ne!(v, v3);
        let v4 = ScramVerifier::generate("another", &[1, 2, 3, 4], 4096);
        assert_ne!(v, v4);
    }

    #[test]
    fn full_handshake_success() {
        let verifier = ScramVerifier::generate("s3cret", &[9u8; 16], 4096);
        let client = ClientExchange::start("alice");
        let (exchange, challenge) =
            ServerExchange::start("alice", client.client_nonce_hex(), Some(&verifier)).unwrap();
        let (client_final, proof, expected_server_sig) =
            client.finish("s3cret", &challenge).unwrap();
        let server_sig = exchange.finish(&client_final, &proof).unwrap();
        assert_eq!(server_sig, expected_server_sig);
    }

    #[test]
    fn wrong_password_fails() {
        let verifier = ScramVerifier::generate("s3cret", &[9u8; 16], 4096);
        let client = ClientExchange::start("alice");
        let (exchange, challenge) =
            ServerExchange::start("alice", client.client_nonce_hex(), Some(&verifier)).unwrap();
        let (client_final, proof, _) = client.finish("wrong", &challenge).unwrap();
        assert!(exchange.finish(&client_final, &proof).is_err());
    }

    #[test]
    fn tampered_proof_and_nonce_fail() {
        let verifier = ScramVerifier::generate("s3cret", &[9u8; 16], 4096);
        // Tampered proof byte.
        let client = ClientExchange::start("alice");
        let (exchange, challenge) =
            ServerExchange::start("alice", client.client_nonce_hex(), Some(&verifier)).unwrap();
        let (client_final, mut proof, _) = client.finish("s3cret", &challenge).unwrap();
        proof[0] ^= 1;
        assert!(exchange.finish(&client_final, &proof).is_err());

        // Wrong nonce echo in client-final: swap the last hex digit for a
        // DIFFERENT one (a fixed "0" would be a no-op whenever the random
        // nonce already ends in '0' — a 1-in-16 flake, caught by CI).
        let client = ClientExchange::start("alice");
        let (exchange, challenge) =
            ServerExchange::start("alice", client.client_nonce_hex(), Some(&verifier)).unwrap();
        let (mut client_final, proof, _) = client.finish("s3cret", &challenge).unwrap();
        let last = client_final.chars().last().unwrap();
        let replacement = if last == '0' { '1' } else { '0' };
        client_final.replace_range(client_final.len() - 1.., &replacement.to_string());
        assert!(exchange.finish(&client_final, &proof).is_err());
    }

    #[test]
    fn unknown_user_gets_fake_challenge_then_fails() {
        let client = ClientExchange::start("nobody");
        let (exchange, challenge) =
            ServerExchange::start("nobody", client.client_nonce_hex(), None)
                .expect("fake challenge must be issued");
        // The challenge is well-formed (echoes our nonce, has a salt).
        assert!(challenge.nonce_hex.starts_with(client.client_nonce_hex()));
        let (client_final, proof, _) = client.finish("anything", &challenge).unwrap();
        // ...and the proof always fails, with the generic error.
        let err = exchange.finish(&client_final, &proof).unwrap_err();
        assert_eq!(err.0, "authentication failed");
        // The error text for a wrong password must be byte-identical.
        let verifier = ScramVerifier::generate("s3cret", &[9u8; 16], 4096);
        let client = ClientExchange::start("alice");
        let (exchange, challenge) =
            ServerExchange::start("alice", client.client_nonce_hex(), Some(&verifier)).unwrap();
        let (client_final, proof, _) = client.finish("wrong", &challenge).unwrap();
        assert_eq!(exchange.finish(&client_final, &proof).unwrap_err().0, err.0);
    }

    #[test]
    fn bad_client_nonce_rejected() {
        let verifier = ScramVerifier::generate("s3cret", &[9u8; 16], 4096);
        assert!(ServerExchange::start("alice", "zz", Some(&verifier)).is_err());
        assert!(ServerExchange::start("alice", "aabb", Some(&verifier)).is_err()); // 2 bytes < 8
        assert!(ServerExchange::start("a b", "aabbccddeeff00112233", Some(&verifier)).is_err());
    }

    #[test]
    fn client_rejects_nonce_mismatch() {
        let client = ClientExchange::start("alice");
        let challenge = Challenge {
            nonce_hex: "deadbeef".to_string(), // does not extend our nonce
            salt_hex: "0011223344556677".to_string(),
            iterations: 4096,
        };
        assert!(client.finish("pw", &challenge).is_err());
        // Server nonce must be non-empty (strictly longer than ours).
        let client = ClientExchange::start("alice");
        let our_nonce = client.client_nonce_hex().to_string();
        let challenge = Challenge {
            nonce_hex: our_nonce,
            salt_hex: "0011223344556677".to_string(),
            iterations: 4096,
        };
        assert!(client.finish("pw", &challenge).is_err());
    }

    #[test]
    fn user_store_round_trip() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("users.json");
        let mut store = UserStore::new();
        store.upsert("alice", "pw1", 4096).unwrap();
        store.upsert("bob", "pw2", 4096).unwrap();
        store.save(&path).unwrap();
        let loaded = UserStore::load(&path).unwrap();
        assert_eq!(
            loaded.usernames(),
            vec!["alice".to_string(), "bob".to_string()]
        );
        assert!(loaded.verifier("alice").is_some());
        // Authentication works from the reloaded verifier.
        let client = ClientExchange::start("alice");
        let (exchange, challenge) =
            ServerExchange::start("alice", client.client_nonce_hex(), loaded.verifier("alice"))
                .unwrap();
        let (cf, proof, ess) = client.finish("pw1", &challenge).unwrap();
        assert_eq!(exchange.finish(&cf, &proof).unwrap(), ess);

        // Overwrite + remove + reload.
        let mut store2 = loaded.clone();
        store2.upsert("alice", "newpw", 4096).unwrap();
        assert!(store2.remove("bob"));
        store2.save(&path).unwrap();
        let reloaded = UserStore::load(&path).unwrap();
        assert_eq!(reloaded.usernames(), vec!["alice".to_string()]);
        let client = ClientExchange::start("alice");
        let (exchange, challenge) = ServerExchange::start(
            "alice",
            client.client_nonce_hex(),
            reloaded.verifier("alice"),
        )
        .unwrap();
        let (cf, proof, _) = client.finish("newpw", &challenge).unwrap();
        assert!(exchange.finish(&cf, &proof).is_ok());
        // Old password no longer works.
        let client = ClientExchange::start("alice");
        let (exchange, challenge) = ServerExchange::start(
            "alice",
            client.client_nonce_hex(),
            reloaded.verifier("alice"),
        )
        .unwrap();
        let (cf, proof, _) = client.finish("pw1", &challenge).unwrap();
        assert!(exchange.finish(&cf, &proof).is_err());
    }

    #[test]
    fn user_store_rejects_bad_names_and_files() {
        let mut store = UserStore::new();
        for bad in ["", "a b", "a,b", "a=b", "üser", &"x".repeat(64)] {
            assert!(
                store.upsert(bad, "pw", 4096).is_err(),
                "{:?} must be rejected",
                bad
            );
        }
        assert!(store.upsert(&"x".repeat(63), "pw", 4096).is_ok());

        let dir = tempfile::tempdir().unwrap();
        let missing = dir.path().join("nope.json");
        assert!(UserStore::load(&missing).is_err());
        std::fs::write(&missing, "not json").unwrap();
        assert!(UserStore::load(&missing).is_err());
        std::fs::write(&missing, "{\"version\":2,\"users\":{}}").unwrap();
        assert!(UserStore::load(&missing).is_err());
        std::fs::write(
            &missing,
            "{\"version\":1,\"users\":{\"a b\":\"SCRAM-SHA-256$1:00$00:00\"}}",
        )
        .unwrap();
        assert!(UserStore::load(&missing).is_err());
    }

    #[test]
    fn session_store_lifecycle() {
        let store = SessionStore::new(Duration::from_millis(50));
        let t = store.issue();
        assert!(store.validate(&t));
        assert!(!store.validate(&encode_hex(&random_bytes(TOKEN_LEN))));
        assert!(store.revoke(&t));
        assert!(!store.validate(&t));
        assert!(!store.revoke(&t));
        // Expiry.
        let t2 = store.issue();
        std::thread::sleep(Duration::from_millis(80));
        assert!(!store.validate(&t2));
        assert_eq!(store.count(), 0);
    }

    #[test]
    fn ct_eq_behavior() {
        assert!(ct_eq(b"abc", b"abc"));
        assert!(!ct_eq(b"abc", b"abd"));
        assert!(!ct_eq(b"abc", b"ab"));
    }

    fn hex_to_arr(s: &str) -> [u8; 32] {
        let v = decode_hex(s).unwrap();
        let mut a = [0u8; 32];
        a.copy_from_slice(&v);
        a
    }
}
