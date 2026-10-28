//! The TLS configurations a client fetches with.
//!
//! A rustls configuration carries a session cache, and a server recognises a
//! resumed session: the ticket it issued earlier comes back. Resuming a session
//! under one isolation key that was made under another would let the server
//! link requests that left on separate circuits. So each key gets a
//! configuration with a session cache of its own, made with the key's isolation
//! group (isolation.rs), and requests without a key share one more. Within a
//! key, sessions resume as usual.

use std::sync::Arc;

use futures_rustls::rustls::{client::Resumption, ClientConfig};

/// Sessions remembered per key. A key is typically one identity talking to a
/// few servers; rustls's default for a whole client is 256.
const SESSIONS_PER_KEY: usize = 16;

pub(crate) struct TlsConfigs {
    base: ClientConfig,
    unkeyed: Arc<ClientConfig>,
}

impl TlsConfigs {
    pub(crate) fn new(base: ClientConfig) -> Self {
        Self {
            unkeyed: Arc::new(base.clone()),
            base,
        }
    }

    /// The configuration requests without an isolation key share.
    pub(crate) fn unkeyed(&self) -> Arc<ClientConfig> {
        Arc::clone(&self.unkeyed)
    }

    /// A configuration with an empty session cache of its own, for one
    /// isolation key's requests.
    pub(crate) fn for_new_key(&self) -> Arc<ClientConfig> {
        let mut config = self.base.clone();
        // A cloned configuration shares the original's session store; give
        // this one its own.
        config.resumption = Resumption::in_memory_sessions(SESSIONS_PER_KEY);
        Arc::new(config)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use futures_rustls::rustls::{
        pki_types::{CertificateDer, PrivateKeyDer, PrivatePkcs8KeyDer, ServerName},
        ClientConnection, HandshakeKind, RootCertStore, ServerConfig, ServerConnection,
    };

    const CERT: &[u8] = include_bytes!("testdata/localhost.cert.der");
    const KEY: &[u8] = include_bytes!("testdata/localhost.key.der");

    fn configs() -> (TlsConfigs, Arc<ServerConfig>) {
        let provider = Arc::new(rustls_rustcrypto::provider());
        let mut roots = RootCertStore::empty();
        roots.add(CertificateDer::from(CERT)).unwrap();
        let client = ClientConfig::builder_with_provider(provider.clone())
            .with_safe_default_protocol_versions()
            .unwrap()
            .with_root_certificates(roots)
            .with_no_client_auth();
        let server = ServerConfig::builder_with_provider(provider)
            .with_safe_default_protocol_versions()
            .unwrap()
            .with_no_client_auth()
            .with_single_cert(
                vec![CertificateDer::from(CERT)],
                PrivateKeyDer::Pkcs8(PrivatePkcs8KeyDer::from(KEY)),
            )
            .unwrap();
        (TlsConfigs::new(client), Arc::new(server))
    }

    /// A full TLS exchange in memory, through the session tickets the server
    /// sends after the handshake.
    fn handshake(client: Arc<ClientConfig>, server: &Arc<ServerConfig>) -> HandshakeKind {
        let mut client = ClientConnection::new(client, ServerName::try_from("localhost").unwrap()).unwrap();
        let mut server = ServerConnection::new(Arc::clone(server)).unwrap();
        for _ in 0..4 {
            let mut bytes = Vec::new();
            client.write_tls(&mut bytes).unwrap();
            server.read_tls(&mut bytes.as_slice()).unwrap();
            server.process_new_packets().unwrap();
            let mut bytes = Vec::new();
            server.write_tls(&mut bytes).unwrap();
            client.read_tls(&mut bytes.as_slice()).unwrap();
            client.process_new_packets().unwrap();
        }
        assert!(!client.is_handshaking());
        client.handshake_kind().unwrap()
    }

    #[test]
    fn sessions_resume_within_a_key_and_never_across_keys() {
        let (tls, server) = configs();
        let (a, b) = (tls.for_new_key(), tls.for_new_key());

        // Requests without a key resume among themselves.
        assert_eq!(handshake(tls.unkeyed(), &server), HandshakeKind::Full);
        assert_eq!(handshake(tls.unkeyed(), &server), HandshakeKind::Resumed);
        // A key has none of their sessions, then resumes its own.
        assert_eq!(handshake(Arc::clone(&a), &server), HandshakeKind::Full);
        assert_eq!(handshake(Arc::clone(&a), &server), HandshakeKind::Resumed);
        // Another key has neither theirs nor key a's.
        assert_eq!(handshake(Arc::clone(&b), &server), HandshakeKind::Full);
        assert_eq!(handshake(b, &server), HandshakeKind::Resumed);
    }

    #[test]
    fn a_keys_sessions_do_not_reach_requests_without_a_key() {
        let (tls, server) = configs();
        let a = tls.for_new_key();
        assert_eq!(handshake(Arc::clone(&a), &server), HandshakeKind::Full);
        assert_eq!(handshake(a, &server), HandshakeKind::Resumed);
        assert_eq!(handshake(tls.unkeyed(), &server), HandshakeKind::Full);
    }
}
