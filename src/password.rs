//! Startup password generation: a short, readable code shown on boot.

use rand::Rng;

/// Unambiguous charset — no 0/O, 1/I/l — so it's easy to read off the screen.
const CHARSET: &[u8] = b"abcdefghjkmnpqrstuvwxyz23456789";

/// Generate a random `len`-character password.
pub fn generate(len: usize) -> String {
    let mut rng = rand::rng();
    (0..len)
        .map(|_| CHARSET[rng.random_range(0..CHARSET.len())] as char)
        .collect()
}

/// Representation of a configured password (either plaintext or Argon2id hash).
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum PasswordMatcher {
    Plain(String),
    Hash(String),
}

impl PasswordMatcher {
    /// Verify a plaintext guess against this password representation.
    pub fn verify(&self, guess: &str) -> bool {
        match self {
            Self::Plain(plain) => {
                constant_time_eq::constant_time_eq(guess.as_bytes(), plain.as_bytes())
            }
            Self::Hash(hash) => {
                if let Ok(parsed_hash) = argon2::PasswordHash::new(hash) {
                    use argon2::PasswordVerifier;
                    argon2::Argon2::default()
                        .verify_password(guess.as_bytes(), &parsed_hash)
                        .is_ok()
                } else {
                    tracing::error!("Invalid password hash configured; authentication will fail.");
                    false
                }
            }
        }
    }
}

/// Hash a plaintext password into a PHC-formatted Argon2id hash.
pub fn hash(plain: &str) -> Result<String, String> {
    use argon2::{
        password_hash::{rand_core::OsRng, SaltString},
        Argon2, PasswordHasher,
    };
    let salt = SaltString::generate(&mut OsRng);
    Argon2::default()
        .hash_password(plain.as_bytes(), &salt)
        .map(|h| h.to_string())
        .map_err(|e| e.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn has_requested_length() {
        assert_eq!(generate(5).len(), 5);
    }

    #[test]
    fn only_uses_safe_charset() {
        let pw = generate(64);
        assert!(pw.bytes().all(|b| CHARSET.contains(&b)));
    }

    #[test]
    fn verify_plain_password() {
        let matcher = PasswordMatcher::Plain("secret".to_string());
        assert!(matcher.verify("secret"));
        assert!(!matcher.verify("wrong"));
    }

    #[test]
    fn verify_hashed_password() {
        let hashed = hash("mysecret").unwrap();
        let matcher = PasswordMatcher::Hash(hashed);
        assert!(matcher.verify("mysecret"));
        assert!(!matcher.verify("wrong"));
    }
}
