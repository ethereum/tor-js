//! Per-key stream isolation for `fetch`.
//!
//! By default every stream a `TorClient` opens may share circuits, so requests
//! made seconds apart leave the same exit and can be linked by whoever answers
//! them. A caller that passes `isolationKey` gets one Arti isolation group per
//! distinct key: requests with the same key may share circuits; requests with
//! different keys never do.
//!
//! The key is the caller's opaque label (an anon-rpc host might derive it from
//! the account a request is about). It is never logged or sent anywhere.

use std::collections::HashMap;

use arti_client::IsolationToken;

/// Longest accepted key. Keys are labels, not payloads.
pub(crate) const MAX_KEY_LEN: usize = 256;

/// Bound on remembered keys. Past it the map starts over: a key seen again gets
/// a fresh group, which is still isolated from every other key.
const MAX_KEYS: usize = 4096;

/// Why an `isolationKey` was refused.
#[derive(Debug, PartialEq, Eq)]
pub(crate) enum KeyError {
    Empty,
    TooLong,
}

/// Maps isolation keys to Arti isolation groups for one client.
#[derive(Default)]
pub(crate) struct IsolationGroups {
    groups: HashMap<String, IsolationToken>,
}

impl IsolationGroups {
    /// The isolation group for `key`, created on first use.
    pub(crate) fn token_for(&mut self, key: &str) -> Result<IsolationToken, KeyError> {
        if key.is_empty() {
            // An empty key is almost certainly a caller bug; sharing circuits
            // silently would be the unsafe reading of it.
            return Err(KeyError::Empty);
        }
        if key.len() > MAX_KEY_LEN {
            return Err(KeyError::TooLong);
        }
        if let Some(token) = self.groups.get(key) {
            return Ok(*token);
        }
        if self.groups.len() >= MAX_KEYS {
            self.groups.clear();
        }
        let token = IsolationToken::new();
        self.groups.insert(key.to_owned(), token);
        Ok(token)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn same_key_same_group_different_keys_different_groups() {
        let mut groups = IsolationGroups::default();
        let a1 = groups.token_for("account-a").unwrap();
        let a2 = groups.token_for("account-a").unwrap();
        let b = groups.token_for("account-b").unwrap();
        assert_eq!(a1, a2);
        assert_ne!(a1, b);
    }

    #[test]
    fn groups_are_per_client() {
        let mut one = IsolationGroups::default();
        let mut two = IsolationGroups::default();
        assert_ne!(one.token_for("k").unwrap(), two.token_for("k").unwrap());
    }

    #[test]
    fn rejects_empty_and_oversized_keys() {
        let mut groups = IsolationGroups::default();
        assert_eq!(groups.token_for(""), Err(KeyError::Empty));
        assert_eq!(
            groups.token_for(&"x".repeat(MAX_KEY_LEN + 1)),
            Err(KeyError::TooLong)
        );
        assert!(groups.token_for(&"x".repeat(MAX_KEY_LEN)).is_ok());
    }

    #[test]
    fn starting_over_still_isolates() {
        let mut groups = IsolationGroups::default();
        let first = groups.token_for("k").unwrap();
        for i in 0..MAX_KEYS {
            groups.token_for(&format!("filler-{i}")).unwrap();
        }
        let again = groups.token_for("k").unwrap();
        assert_ne!(
            first, again,
            "a forgotten key gets a fresh group, never another key's"
        );
    }
}
