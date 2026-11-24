//! Authentication primitives: password hashing and opaque session tokens.
//!
//! These are pure, side-effect-free functions — no database, no HTTP — which
//! makes them easy to test in isolation. The handlers and the session
//! extractor (added in 1C) build on top of these.

// `PasswordHasher`/`PasswordVerifier` are traits: importing them is what makes
// their methods (`hash_password`, `verify_password`) callable on `Argon2`.
// Rust only lets you call a trait method when the trait is in scope.
use argon2::{Argon2, PasswordHash, PasswordHasher, PasswordVerifier};
use base64::Engine;
use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use sha2::{Digest, Sha256};
use std::sync::Arc;
use subtle::ConstantTimeEq;

/// Hash a plaintext password into a PHC string (`$argon2id$v=19$...`) using a
/// fresh random salt. Store the returned string; never store the password.
///
/// Argon2id (the default) is memory-hard, which is what makes it expensive for
/// an attacker to brute-force even with a leaked database.
pub fn hash_password(plain: &str) -> Result<String, argon2::password_hash::Error> {
    // Since argon2 0.6 the hasher draws the 16-byte salt from the OS RNG
    // itself (its `getrandom` feature), so there's no salt to pass in.
    let hash = Argon2::default().hash_password(plain.as_bytes())?;
    Ok(hash.to_string())
}

/// Check a plaintext password against a stored PHC hash. Returns `false` for a
/// wrong password *and* for a malformed hash string — callers never need to
/// distinguish, and collapsing both avoids leaking which one failed.
pub fn verify_password(plain: &str, phc_hash: &str) -> bool {
    match PasswordHash::new(phc_hash) {
        Ok(parsed) => Argon2::default()
            .verify_password(plain.as_bytes(), &parsed)
            .is_ok(),
        Err(_) => false,
    }
}

/// Generate a fresh session token: 32 cryptographically-random bytes encoded as
/// URL-safe base64. This is the secret value handed to the client in a cookie.
pub fn generate_token() -> String {
    URL_SAFE_NO_PAD.encode(random_bytes::<32>())
}

/// `N` bytes straight from the operating system's CSPRNG.
///
/// `<const N: usize>` is a *const generic*: the array length is a compile-time
/// parameter, so `random_bytes::<32>()` returns a `[u8; 32]` on the stack — no
/// allocation, and the caller can't get the length wrong.
///
/// Panics if the OS can't supply randomness. That only happens on a badly
/// broken system, and minting a guessable token instead would be far worse —
/// so failing loudly is the safe choice (the old `OsRng` did the same).
fn random_bytes<const N: usize>() -> [u8; N] {
    let mut bytes = [0u8; N];
    getrandom::fill(&mut bytes).expect("OS random number generator failed");
    bytes
}

/// Hash a token (or invite code) with SHA-256, returned as lowercase hex.
///
/// We store this hash, not the token itself, so a database leak can't be
/// replayed. SHA-256 (not Argon2) is correct here: the input is already
/// high-entropy random, so there's nothing to brute-force and we want lookups
/// to be fast.
pub fn hash_token(token: &str) -> String {
    let digest = Sha256::digest(token.as_bytes());
    hex::encode(digest)
}

/// Alphabet for invite codes: uppercase letters and digits with the easily
/// confused ones removed (`I`, `O`, `0`, `1`). Exactly 32 symbols, and 256 is a
/// multiple of 32, so mapping a random byte through it has *no* modulo bias.
const INVITE_ALPHABET: &[u8] = b"ABCDEFGHJKLMNPQRSTUVWXYZ23456789";

/// Generate a human-typeable single-use invite code, formatted `XXXX-XXXX`.
pub fn generate_invite_code() -> String {
    let chars: String = random_bytes::<8>()
        .iter()
        .map(|b| INVITE_ALPHABET[*b as usize % INVITE_ALPHABET.len()] as char)
        .collect();
    format!("{}-{}", &chars[..4], &chars[4..])
}

/// Generate the one-time first-run setup token, formatted
/// `XXXX-XXXX-XXXX-XXXX` from the same unambiguous alphabet as invite codes
/// (16 symbols × 5 bits = 80 bits of entropy).
pub fn generate_setup_token() -> String {
    let chars: Vec<char> = random_bytes::<16>()
        .iter()
        .map(|b| INVITE_ALPHABET[*b as usize % INVITE_ALPHABET.len()] as char)
        .collect();
    // `chunks(4)` yields slices of 4 chars; join them with dashes.
    chars
        .chunks(4)
        .map(|c| c.iter().collect::<String>())
        .collect::<Vec<_>>()
        .join("-")
}

/// Minimum length of a `PERKELE_SETUP_TOKEN` override, counted after
/// canonicalization (letters and digits only).
pub const MIN_SETUP_TOKEN_LEN: usize = 12;

/// Who may call `POST /api/setup`.
#[derive(Debug, Clone)]
pub enum SetupGate {
    /// Only a caller presenting this token (stored canonicalized).
    Token(Arc<str>),
    /// No token needed. `#[cfg(test)]` on a variant means it only EXISTS in
    /// test builds, so a release binary cannot be configured into it: the
    /// dozens of existing handler tests keep calling setup without a token.
    #[cfg(test)]
    Open,
}

impl SetupGate {
    /// Build the gate from `PERKELE_SETUP_TOKEN` (if set and non-blank) or a
    /// freshly generated token. Returns the gate and the token to show the
    /// operator, formatted as they should type it.
    pub fn from_env_or_random(env: Option<&str>) -> Result<(Self, String), String> {
        let display = match env.map(str::trim).filter(|s| !s.is_empty()) {
            Some(t) => {
                if canonicalize_invite_code(t).len() < MIN_SETUP_TOKEN_LEN {
                    return Err(format!(
                        "PERKELE_SETUP_TOKEN must contain at least {MIN_SETUP_TOKEN_LEN} \
                         letters/digits (only those count; case is ignored)"
                    ));
                }
                t.to_owned()
            }
            None => generate_setup_token(),
        };
        let gate = SetupGate::Token(canonicalize_invite_code(&display).into());
        Ok((gate, display))
    }

    /// Does `provided` open the gate? Compared in constant time, so response
    /// timing can't be used to guess the token one character at a time.
    pub fn accepts(&self, provided: &str) -> bool {
        match self {
            SetupGate::Token(expected) => {
                let provided = canonicalize_invite_code(provided);
                // `ct_eq` returns a `subtle::Choice`, not a bool — that type
                // exists to stop the compiler optimising the comparison into an
                // early-exit one. `.into()` converts it at the very end.
                !provided.is_empty() && bool::from(provided.as_bytes().ct_eq(expected.as_bytes()))
            }
            #[cfg(test)]
            SetupGate::Open => true,
        }
    }
}

/// Normalize a code as typed by a user (lowercase, missing/extra dashes,
/// spaces) into the canonical form we hash — keep only alphanumerics, uppercase
/// them. So `perk-tk9z`, `PERKTK9Z`, and `PERK TK9Z` all match.
pub fn canonicalize_invite_code(code: &str) -> String {
    code.chars()
        .filter(|c| c.is_ascii_alphanumeric())
        .map(|c| c.to_ascii_uppercase())
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn password_round_trips() {
        let hash = hash_password("correct horse battery staple").unwrap();
        assert!(verify_password("correct horse battery staple", &hash));
        assert!(!verify_password("wrong password", &hash));
    }

    #[test]
    fn same_password_hashes_differently_each_time() {
        // A fresh random salt per hash means identical passwords produce
        // different stored hashes — yet both still verify.
        let a = hash_password("hunter2").unwrap();
        let b = hash_password("hunter2").unwrap();
        assert_ne!(a, b);
        assert!(verify_password("hunter2", &a));
        assert!(verify_password("hunter2", &b));
    }

    /// A hash produced by the argon2 0.5 crate (PERKELE ≤ 1.0.7). Every
    /// deployment's `users.password_hash` column is full of strings like this,
    /// so a dependency upgrade that stopped verifying them would lock every
    /// user out. Pinned as a literal so the test can't regenerate it with
    /// whatever version is current.
    const ARGON2_0_5_HASH: &str = "$argon2id$v=19$m=19456,t=2,p=1$yyfriy+m5wh4isztVUUTZw$LtYlGZBoE0Ko13BlcAsxk8FEGZjXB7UoGG0xBUPJ11o";

    #[test]
    fn hashes_from_earlier_releases_still_verify() {
        assert!(verify_password(
            "correct horse battery staple",
            ARGON2_0_5_HASH
        ));
        assert!(!verify_password(
            "correct horse battery stapl",
            ARGON2_0_5_HASH
        ));
    }

    #[test]
    fn new_hashes_are_argon2id_with_owasp_minimum_cost() {
        // Parse the PHC string we produce and check its parameters, so a
        // dependency bump that quietly weakened the defaults (or switched
        // variant) would fail here instead of shipping.
        let hash = hash_password("hunter2").unwrap();
        let parsed = PasswordHash::new(&hash).unwrap();
        assert_eq!(parsed.algorithm.as_str(), "argon2id");
        assert_eq!(parsed.version, Some(0x13)); // v=19, the current spec
        let param = |name: &str| -> u32 {
            parsed
                .params
                .get_decimal(name)
                .unwrap_or_else(|| panic!("missing {name}"))
        };
        // OWASP's argon2id floor: 19 MiB memory, 2 iterations, 1 lane.
        assert!(param("m") >= 19_456);
        assert!(param("t") >= 2);
        assert!(param("p") >= 1);
    }

    #[test]
    fn verify_rejects_garbage_hash() {
        assert!(!verify_password("anything", "not-a-valid-phc-string"));
    }

    #[test]
    fn tokens_are_unique_and_url_safe() {
        let a = generate_token();
        let b = generate_token();
        assert_ne!(a, b);
        // 32 bytes in base64 (no padding) is 43 characters.
        assert_eq!(a.len(), 43);
        assert!(
            a.chars()
                .all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_')
        );
    }

    #[test]
    fn token_hash_is_deterministic_and_hides_the_token() {
        let token = generate_token();
        assert_eq!(hash_token(&token), hash_token(&token));
        assert_ne!(hash_token(&token), token);
        assert_eq!(hash_token(&token).len(), 64); // SHA-256 = 32 bytes = 64 hex chars
    }

    #[test]
    fn invite_codes_are_formatted_and_unambiguous() {
        let code = generate_invite_code();
        assert_eq!(code.len(), 9); // XXXX-XXXX
        assert_eq!(code.chars().nth(4), Some('-'));
        // No ambiguous characters.
        assert!(!code.contains(['I', 'O', '0', '1']));
        assert_ne!(generate_invite_code(), generate_invite_code());
    }

    #[test]
    fn setup_tokens_are_long_and_unambiguous() {
        let t = generate_setup_token();
        assert_eq!(t.len(), 19); // XXXX-XXXX-XXXX-XXXX
        assert_eq!(t.matches('-').count(), 3);
        assert!(!t.contains(['I', 'O', '0', '1']));
        assert_ne!(generate_setup_token(), generate_setup_token());
    }

    #[test]
    fn setup_gate_accepts_only_its_token_forgivingly() {
        let (gate, shown) = SetupGate::from_env_or_random(None).unwrap();
        assert!(gate.accepts(&shown));
        assert!(gate.accepts(&shown.to_lowercase().replace('-', " ")));
        assert!(!gate.accepts(""));
        assert!(!gate.accepts("AAAA-AAAA-AAAA-AAAA"));
    }

    #[test]
    fn setup_gate_env_override() {
        let (gate, shown) = SetupGate::from_env_or_random(Some(" my-automation-token ")).unwrap();
        assert_eq!(shown, "my-automation-token");
        assert!(gate.accepts("MYAUTOMATIONTOKEN"));
        // Blank means "not set": a random token is generated instead.
        let (_, shown) = SetupGate::from_env_or_random(Some("  ")).unwrap();
        assert_eq!(shown.len(), 19);
        // Too short to be a secret.
        assert!(SetupGate::from_env_or_random(Some("abc")).is_err());
    }

    #[test]
    fn invite_canonicalization_is_forgiving() {
        // Lowercase, missing dash, and stray spaces all canonicalize the same.
        assert_eq!(canonicalize_invite_code("perk-tk9z"), "PERKTK9Z");
        assert_eq!(canonicalize_invite_code("PERKTK9Z"), "PERKTK9Z");
        assert_eq!(canonicalize_invite_code(" perk tk9z "), "PERKTK9Z");
    }
}
