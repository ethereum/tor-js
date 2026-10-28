//! Per-key stream isolation for `fetch`.
//!
//! By default every stream a `TorClient` opens may share circuits, so requests
//! made seconds apart leave the same exit and can be linked by whoever answers
//! them. A caller that passes `anonOptions.isolationKey` gets one Arti
//! isolation group per distinct key: requests with the same key may share
//! circuits; requests with different keys never do.
//!
//! The key is the caller's opaque label (an anon-rpc host might derive it from
//! the account a request is about). It is never logged or sent anywhere.
//!
//! A circuit is not the only thing requests could share. Each group also
//! carries state of its own that must not cross keys — for `fetch`, its TLS
//! session cache (tls.rs) — created with the group and forgotten with it.

use std::collections::HashMap;

use arti_client::IsolationToken;

/// Longest accepted key. Keys are labels, not payloads.
pub(crate) const MAX_KEY_LEN: usize = 256;

/// Bound on remembered keys. Past it the map starts over: a key seen again gets
/// a fresh group, which is still isolated from every other key.
const MAX_KEYS: usize = 4096;

/// Why an isolation key was refused.
#[derive(Debug, PartialEq, Eq)]
pub(crate) enum KeyError {
    Empty,
    TooLong,
}

/// One key's isolation group: its Arti isolation token, and `T`, the
/// per-group state that must not be shared with any other key.
#[derive(Clone)]
pub(crate) struct Group<T> {
    pub(crate) token: IsolationToken,
    pub(crate) state: T,
}

/// Maps isolation keys to isolation groups for one client.
pub(crate) struct IsolationGroups<T> {
    groups: HashMap<String, Group<T>>,
}

impl<T> Default for IsolationGroups<T> {
    fn default() -> Self {
        Self { groups: HashMap::new() }
    }
}

impl<T: Clone> IsolationGroups<T> {
    /// The isolation group for `key`, created on first use with a fresh token
    /// and `new_state()`.
    pub(crate) fn group_for(&mut self, key: &str, new_state: impl FnOnce() -> T) -> Result<Group<T>, KeyError> {
        if key.is_empty() {
            // An empty key is almost certainly a caller bug; sharing circuits
            // silently would be the unsafe reading of it.
            return Err(KeyError::Empty);
        }
        if key.len() > MAX_KEY_LEN {
            return Err(KeyError::TooLong);
        }
        if let Some(group) = self.groups.get(key) {
            return Ok(group.clone());
        }
        if self.groups.len() >= MAX_KEYS {
            self.groups.clear();
        }
        let group = Group {
            token: IsolationToken::new(),
            state: new_state(),
        };
        self.groups.insert(key.to_owned(), group.clone());
        Ok(group)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A group's token, with a state that records nothing.
    fn token(groups: &mut IsolationGroups<()>, key: &str) -> Result<IsolationToken, KeyError> {
        groups.group_for(key, || ()).map(|g| g.token)
    }

    #[test]
    fn same_key_same_group_different_keys_different_groups() {
        let mut groups = IsolationGroups::default();
        let a1 = token(&mut groups, "account-a").unwrap();
        let a2 = token(&mut groups, "account-a").unwrap();
        let b = token(&mut groups, "account-b").unwrap();
        assert_eq!(a1, a2);
        assert_ne!(a1, b);
    }

    #[test]
    fn groups_are_per_client() {
        let mut one = IsolationGroups::default();
        let mut two = IsolationGroups::default();
        assert_ne!(token(&mut one, "k").unwrap(), token(&mut two, "k").unwrap());
    }

    #[test]
    fn rejects_empty_and_oversized_keys() {
        let mut groups = IsolationGroups::default();
        assert_eq!(token(&mut groups, ""), Err(KeyError::Empty));
        assert_eq!(
            token(&mut groups, &"x".repeat(MAX_KEY_LEN + 1)),
            Err(KeyError::TooLong)
        );
        assert!(token(&mut groups, &"x".repeat(MAX_KEY_LEN)).is_ok());
    }

    #[test]
    fn starting_over_still_isolates() {
        let mut groups = IsolationGroups::default();
        let first = token(&mut groups, "k").unwrap();
        for i in 0..MAX_KEYS {
            token(&mut groups, &format!("filler-{i}")).unwrap();
        }
        let again = token(&mut groups, "k").unwrap();
        assert_ne!(
            first, again,
            "a forgotten key gets a fresh group, never another key's"
        );
    }

    #[test]
    fn state_is_made_once_per_group_and_never_shared() {
        let mut groups = IsolationGroups::default();
        let mut made = 0;
        let mut group = |groups: &mut IsolationGroups<u32>, key: &str| {
            groups
                .group_for(key, || {
                    made += 1;
                    made
                })
                .unwrap()
                .state
        };
        let a = group(&mut groups, "a");
        assert_eq!(group(&mut groups, "a"), a, "same key, same state");
        assert_ne!(group(&mut groups, "b"), a, "another key gets its own");
        assert!(groups.group_for("", || 0).is_err(), "a refused key makes no state");
    }

    #[test]
    fn a_forgotten_key_gets_fresh_state_too() {
        let mut groups = IsolationGroups::default();
        let first = groups.group_for("k", || 1).unwrap().state;
        for i in 0..MAX_KEYS {
            groups.group_for(&format!("filler-{i}"), || 0).unwrap();
        }
        assert_ne!(groups.group_for("k", || 2).unwrap().state, first);
    }
}
