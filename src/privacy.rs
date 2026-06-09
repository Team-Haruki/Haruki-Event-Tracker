use sha2::{Digest, Sha256};

use crate::model::enums::SekaiServerRegion;

#[derive(Debug, Clone, Default)]
pub struct UidAnonymizer {
    enabled: bool,
    salt: String,
}

impl UidAnonymizer {
    pub fn disabled() -> Self {
        Self::default()
    }

    pub fn enabled(salt: impl Into<String>) -> Self {
        Self {
            enabled: true,
            salt: salt.into(),
        }
    }

    pub fn is_enabled(&self) -> bool {
        self.enabled
    }

    pub fn public_user_id(
        &self,
        server: SekaiServerRegion,
        event_id: i64,
        user_id: &str,
    ) -> String {
        if !self.enabled {
            return user_id.to_owned();
        }
        unique_user_id(server, event_id, user_id, &self.salt)
    }
}

pub fn unique_user_id(
    server: SekaiServerRegion,
    event_id: i64,
    user_id: &str,
    salt: &str,
) -> String {
    let input = format!("{server}-event-{event_id}-{user_id}-{salt}");
    let hash = Sha256::digest(input.as_bytes());
    let mut out = String::with_capacity(64);
    for byte in hash {
        use std::fmt::Write as _;
        write!(&mut out, "{byte:02x}").expect("writing to String cannot fail");
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn unique_id_is_stable_lowercase_sha256_hex() {
        let a = unique_user_id(SekaiServerRegion::Jp, 123, "456", "salt");
        let b = unique_user_id(SekaiServerRegion::Jp, 123, "456", "salt");
        assert_eq!(a, b);
        assert_eq!(a.len(), 64);
        assert!(a.chars().all(|c| c.is_ascii_hexdigit()));
        assert_eq!(a, a.to_ascii_lowercase());
    }

    #[test]
    fn unique_id_changes_with_inputs() {
        let base = unique_user_id(SekaiServerRegion::Jp, 123, "456", "salt");
        assert_ne!(
            base,
            unique_user_id(SekaiServerRegion::En, 123, "456", "salt")
        );
        assert_ne!(
            base,
            unique_user_id(SekaiServerRegion::Jp, 124, "456", "salt")
        );
        assert_ne!(
            base,
            unique_user_id(SekaiServerRegion::Jp, 123, "457", "salt")
        );
        assert_ne!(
            base,
            unique_user_id(SekaiServerRegion::Jp, 123, "456", "other")
        );
    }

    #[test]
    fn disabled_anonymizer_preserves_uid() {
        let anonymizer = UidAnonymizer::disabled();
        assert_eq!(
            anonymizer.public_user_id(SekaiServerRegion::Cn, 1, "100"),
            "100"
        );
    }
}
