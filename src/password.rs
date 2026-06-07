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
}
