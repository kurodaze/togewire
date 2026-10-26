use parking_lot::Mutex;
use sha2::{Digest, Sha256};
use std::collections::HashMap;
use std::sync::LazyLock;
use std::time::{Duration, Instant};
use subtle::ConstantTimeEq;

/// Browsers cap cookie lifetime at 400 days.
const COOKIE_MAX_AGE_SECS: u64 = 400 * 86_400;
const MAX_LOGIN_ATTEMPTS: usize = 5;
const RATE_LIMIT_WINDOW: Duration = Duration::from_mins(30);
/// Caps the map so a flood of logins cannot grow it without bound.
const MAX_ENTRIES: usize = 1_000;

pub const SESSION_COOKIE: &str = "togewire_session";

/// Client IP → (attempts, window start).
static ATTEMPTS: LazyLock<Mutex<HashMap<String, (usize, Instant)>>> = LazyLock::new(Mutex::default);
static SETUP_TOKEN: Mutex<String> = Mutex::new(String::new());
/// Hash of the setup password awaiting its confirm step, so the page never echoes it back.
static PENDING_HASH: Mutex<String> = Mutex::new(String::new());

/// Drop expired entries, then the oldest, until one insert fits under `MAX_ENTRIES`.
fn make_room(map: &mut HashMap<String, (usize, Instant)>) {
    map.retain(|_, (_, start)| start.elapsed() < RATE_LIMIT_WINDOW);
    while map.len() >= MAX_ENTRIES
        && let Some(oldest) = map
            .iter()
            .min_by_key(|(_, (_, start))| *start)
            .map(|(k, _)| k.clone())
    {
        map.remove(&oldest);
    }
}

/// Admin logins never expire on purpose. The token is derived from the secret bcrypt hash,
/// so it survives restarts and is revoked only by changing the password hash.
#[must_use]
pub fn session_token(admin_hash: &str) -> String {
    crate::hex(&Sha256::digest(admin_hash))
}

#[must_use]
pub fn is_valid_session(admin_hash: &str, token: &str) -> bool {
    !admin_hash.is_empty()
        && bool::from(session_token(admin_hash).as_bytes().ct_eq(token.as_bytes()))
}

/// bcrypt is deliberately slow, so it runs on the blocking pool, not a runtime worker.
pub async fn hash_password(password: String) -> Option<String> {
    tokio::task::spawn_blocking(move || bcrypt::hash(password, bcrypt::DEFAULT_COST).ok())
        .await
        .ok()
        .flatten()
}

pub async fn verify_password(hash: String, password: String) -> bool {
    if password.len() > 72 {
        return false;
    }
    tokio::task::spawn_blocking(move || bcrypt::verify(password, &hash).unwrap_or(false))
        .await
        .unwrap_or(false)
}

/// Count a login attempt from `ip`; false once it is over the limit for this window.
#[must_use]
pub fn register_login_attempt(ip: &str) -> bool {
    let mut attempts = ATTEMPTS.lock();
    if !attempts.contains_key(ip) && attempts.len() >= MAX_ENTRIES {
        make_room(&mut attempts);
    }
    let entry = attempts
        .entry(ip.to_string())
        .or_insert((0, Instant::now()));
    if entry.1.elapsed() > RATE_LIMIT_WINDOW {
        *entry = (0, Instant::now());
    }
    entry.0 += 1;
    entry.0 <= MAX_LOGIN_ATTEMPTS
}

pub fn clear_failed_attempts(ip: &str) {
    ATTEMPTS.lock().remove(ip);
}

#[must_use]
pub fn session_cookie(secure: bool, token: &str) -> String {
    let secure = if secure { "; Secure" } else { "" };
    format!(
        "{SESSION_COOKIE}={token}; Path=/; Max-Age={COOKIE_MAX_AGE_SECS}; HttpOnly{secure}; SameSite=Lax"
    )
}

#[must_use]
pub fn generate_setup_token() -> String {
    use rand::RngExt;
    // 32 symbols, so `byte % 32` has no modulo bias.
    const CHARS: &[u8; 32] = b"ABCDEFGHJKLMNPQRSTUVWXYZ23456789";
    let mut bytes = [0u8; 8];
    rand::rng().fill(&mut bytes[..]);
    let token: String = bytes
        .iter()
        .map(|b| CHARS[*b as usize % CHARS.len()] as char)
        .collect();
    *SETUP_TOKEN.lock() = token.clone();
    token
}

#[must_use]
pub fn is_setup_token_valid(token: &str) -> bool {
    let stored = SETUP_TOKEN.lock();
    !stored.is_empty() && bool::from(stored.as_bytes().ct_eq(token.as_bytes()))
}

pub fn clear_setup_token() {
    SETUP_TOKEN.lock().clear();
    PENDING_HASH.lock().clear();
}

pub fn set_pending_hash(hash: String) {
    *PENDING_HASH.lock() = hash;
}

#[must_use]
pub fn take_pending_hash() -> String {
    std::mem::take(&mut *PENDING_HASH.lock())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sessions_and_login_rate_limit() {
        let token = session_token("$2b$12$hash");
        assert!(is_valid_session("$2b$12$hash", &token));
        assert!(!is_valid_session("$2b$12$hash", "forged"));
        assert!(!is_valid_session("$2b$12$other", &token));
        assert!(!is_valid_session("", &session_token("")));

        let ip = "198.51.100.99";
        assert!((0..MAX_LOGIN_ATTEMPTS).all(|_| register_login_attempt(ip)));
        assert!(!register_login_attempt(ip));
        clear_failed_attempts(ip);
        assert!(register_login_attempt(ip));
    }
}
