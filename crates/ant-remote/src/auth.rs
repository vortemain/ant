//! Bearer-token authentication. Tokens are read from files (never from the
//! command line, where other users could see them), never logged or echoed,
//! and compared in constant time.

use std::fmt;
use std::path::Path;

/// A secret bearer token. Deliberately not `Display`; `Debug` is redacted.
#[derive(Clone)]
pub struct Token(String);

impl fmt::Debug for Token {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("Token(<redacted>)")
    }
}

impl Token {
    /// Read a token from a file (surrounding whitespace trimmed). Rejects
    /// tokens shorter than 32 characters, which are too guessable.
    pub fn from_file(path: &Path) -> Result<Self, String> {
        let text = std::fs::read_to_string(path)
            .map_err(|error| format!("cannot read token file {}: {error}", path.display()))?;
        Self::new(text.trim())
    }

    pub fn new(value: &str) -> Result<Self, String> {
        if value.len() < 32 || value.chars().any(|c| c.is_whitespace() || c.is_control()) {
            return Err("token must be at least 32 non-whitespace characters".to_owned());
        }
        Ok(Self(value.to_owned()))
    }

    pub(crate) fn header_value(&self) -> String {
        format!("Bearer {}", self.0)
    }

    /// Constant-time comparison against a presented `Authorization` value.
    pub(crate) fn matches_header(&self, presented: &str) -> bool {
        let Some(candidate) = presented.strip_prefix("Bearer ") else {
            return false;
        };
        constant_time_eq(self.0.as_bytes(), candidate.trim().as_bytes())
    }
}

fn constant_time_eq(left: &[u8], right: &[u8]) -> bool {
    // Length is not secret-dependent beyond what the attacker chooses.
    if left.len() != right.len() {
        return false;
    }
    left.iter()
        .zip(right)
        .fold(0u8, |acc, (a, b)| acc | (a ^ b))
        == 0
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn tokens_are_validated_redacted_and_compared_exactly() {
        assert!(Token::new("short").is_err());
        assert!(
            Token::new(&format!("{} x", "a".repeat(32))).is_err(),
            "whitespace"
        );
        let token = Token::new(&"t".repeat(40)).expect("token");
        assert_eq!(format!("{token:?}"), "Token(<redacted>)");
        assert!(token.matches_header(&format!("Bearer {}", "t".repeat(40))));
        assert!(!token.matches_header(&format!("Bearer {}", "t".repeat(39))));
        assert!(!token.matches_header(&format!("Bearer {}u", "t".repeat(39))));
        assert!(!token.matches_header(&"t".repeat(40)), "scheme required");
    }
}
