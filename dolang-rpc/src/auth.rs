//! Optional shared secret authentication.
//!
//! Supplying [`AuthKey`] to [`Builder::key`](crate::Builder::key) performs mutual authentication
//! during session negotiation.
//!
//! This is provided mainly for use with one-off Unix socket or Windows named pipe connections where
//! controlling access to the endpoint or consistently discerning peer identity may be difficult,
//! e.g. when crossing container boundaries with unknown or misconfigured identity mappings.
//!
//! **This does not provide message integrity or privacy**; the protocol remains unencrypted and
//! unsigned, so it must be used over a private channel such as a Unix socket, or tunneled over a
//! protocol that provides privacy and integrity, like SSH or TLS.
//!
//! Authentication is a simple derived key exchange with no nonce, so **it is not
//! replay-resistant**. It is intended for single-use keys minted per session and exchanged
//! beforehand over a secure side channel. It must carry sufficient entropy on its own.
//!
//! Each side advertises a digest derived from the key with a role-specific BLAKE3 key-derivation
//! context, and checks the digest derived from the *other* role. Because the digests are one-way,
//! an impostor that connects first and harvests the server's advertisement cannot derive the
//! client's, and an impostor that binds the socket first cannot produce the server's. Both digests
//! ride the existing symmetric negotiation exchange, so authentication costs no additional round
//! trips.

use std::fmt;

use crate::Error;

/// Minimum accepted length, in bytes, of a pre-shared authentication key.
///
/// Derivation happily turns a one-byte key into a respectable-looking 32-byte
/// digest, so the floor is enforced rather than left to callers.
pub const MIN_KEY_LEN: usize = 16;

// Hardcoded, globally unique, and never built dynamically, per BLAKE3's
// guidance for key-derivation contexts. The role suffix is what keeps a
// harvested advertisement from being replayed back in the other direction, so
// the two must never be collapsed into one context.
const CLIENT_CONTEXT: &str = "dolang-rpc 2026-08-13 session auth client";
const SERVER_CONTEXT: &str = "dolang-rpc 2026-08-13 session auth server";

/// A pre-shared key, reduced to the pair of digests negotiation exchanges.
///
/// Constructing one derives both digests and discards the key material, so the
/// secret itself is not retained for the life of the session.
#[derive(Clone, Copy)]
pub struct AuthKey {
    client: blake3::Hash,
    server: blake3::Hash,
}

impl AuthKey {
    /// Derives the client and server digests from `key`.
    ///
    /// # Errors
    ///
    /// Returns [`Error::Auth`] if `key` is shorter than [`MIN_KEY_LEN`].
    pub fn new(key: &[u8]) -> Result<Self, Error> {
        if key.len() < MIN_KEY_LEN {
            return Err(Error::Auth(format!(
                "authentication key must be at least {MIN_KEY_LEN} bytes"
            )));
        }
        Ok(Self {
            client: derive(CLIENT_CONTEXT, key),
            server: derive(SERVER_CONTEXT, key),
        })
    }

    /// Returns the digests to send and expect when acting as the client.
    pub(crate) fn as_client(&self) -> Auth {
        Auth {
            send: self.client,
            expect: self.server,
        }
    }

    /// Returns the digests to send and expect when acting as the server.
    pub(crate) fn as_server(&self) -> Auth {
        Auth {
            send: self.server,
            expect: self.client,
        }
    }
}

// A derived digest authenticates its side as effectively as the key does, so
// printing one is equivalent to printing the secret.
impl fmt::Debug for AuthKey {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("AuthKey(<redacted>)")
    }
}

fn derive(context: &str, key: &[u8]) -> blake3::Hash {
    // Going through the hasher rather than `blake3::derive_key` yields a
    // `Hash`, whose `PartialEq` is constant-time; the bare `[u8; 32]` that the
    // convenience function returns compares in variable time.
    let mut hasher = blake3::Hasher::new_derive_key(context);
    hasher.update(key);
    hasher.finalize()
}

/// One side's view of an [`AuthKey`]: what to advertise, and what to require.
#[derive(Clone, Copy)]
pub(crate) struct Auth {
    pub(crate) send: blake3::Hash,
    pub(crate) expect: blake3::Hash,
}

impl Auth {
    /// Returns the digest to place in the outgoing handshake.
    pub(crate) fn advertise(&self) -> [u8; 32] {
        *self.send.as_bytes()
    }
}

/// Checks a peer's advertised digest against local configuration.
///
/// Every combination other than "both keyed and matching" or "neither keyed" is
/// rejected, so a configuration mistake fails closed rather than silently
/// dropping authentication. Note that a keyed peer talking to an unkeyed one is
/// refused from both ends independently: the keyed side finds nothing to check,
/// and the unkeyed side refuses to ignore a proof it cannot evaluate.
pub(crate) fn verify(local: Option<Auth>, peer: Option<[u8; 32]>) -> Result<(), Error> {
    match (local, peer) {
        (None, None) => Ok(()),
        (Some(local), Some(peer)) => {
            // `blake3::Hash` compares in constant time; `[u8; 32]` does not.
            if local.expect == blake3::Hash::from(peer) {
                Ok(())
            } else {
                // Deliberately says nothing about the expected or received
                // digest: either one authenticates its side.
                Err(Error::Auth("peer failed authentication".into()))
            }
        }
        (Some(_), None) => Err(Error::Auth(
            "peer did not authenticate but a key is configured".into(),
        )),
        (None, Some(_)) => Err(Error::Auth(
            "peer authenticated but no key is configured".into(),
        )),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const KEY: &[u8] = b"0123456789abcdef";

    #[test]
    fn keys_shorter_than_the_minimum_are_rejected() {
        let short = &KEY[..MIN_KEY_LEN - 1];
        let error = AuthKey::new(short).unwrap_err();
        assert!(matches!(error, Error::Auth(ref msg) if msg.contains("at least")));
        assert!(AuthKey::new(KEY).is_ok());
    }

    #[test]
    fn client_and_server_digests_differ_for_the_same_key() {
        let key = AuthKey::new(KEY).unwrap();
        assert_ne!(key.as_client().advertise(), key.as_server().advertise());
        // Each side expects what the other sends.
        assert_eq!(
            key.as_client().advertise(),
            *key.as_server().expect.as_bytes()
        );
        assert_eq!(
            key.as_server().advertise(),
            *key.as_client().expect.as_bytes()
        );
    }

    #[test]
    fn verify_accepts_matching_peers_in_both_directions() {
        let key = AuthKey::new(KEY).unwrap();
        let client = key.as_client();
        let server = key.as_server();
        verify(Some(client), Some(server.advertise())).unwrap();
        verify(Some(server), Some(client.advertise())).unwrap();
    }

    #[test]
    fn verify_rejects_a_replayed_advertisement() {
        // A peer that harvested the server's advertisement cannot use it to
        // authenticate as the client, which is what keeps "connect first" from
        // being worth anything.
        let key = AuthKey::new(KEY).unwrap();
        let server = key.as_server();
        let error = verify(Some(server), Some(server.advertise())).unwrap_err();
        assert!(matches!(error, Error::Auth(_)));
    }

    #[test]
    fn verify_rejects_a_different_key() {
        let key = AuthKey::new(KEY).unwrap();
        let other = AuthKey::new(b"fedcba9876543210").unwrap();
        let error = verify(Some(key.as_client()), Some(other.as_server().advertise())).unwrap_err();
        assert!(matches!(error, Error::Auth(_)));
    }

    #[test]
    fn verify_rejects_mismatched_configuration_in_both_directions() {
        let key = AuthKey::new(KEY).unwrap();
        let error = verify(Some(key.as_client()), None).unwrap_err();
        assert!(matches!(error, Error::Auth(ref msg) if msg.contains("did not authenticate")));
        let error = verify(None, Some(key.as_client().advertise())).unwrap_err();
        assert!(matches!(error, Error::Auth(ref msg) if msg.contains("no key is configured")));
        verify(None, None).unwrap();
    }

    #[test]
    fn debug_does_not_expose_derived_digests() {
        let key = AuthKey::new(KEY).unwrap();
        assert_eq!(format!("{key:?}"), "AuthKey(<redacted>)");
    }
}
