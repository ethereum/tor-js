//! The TLS configurations a client fetches with.
//!
//! A rustls configuration carries a session cache, and a server can recognise
//! a resumed session. Requests with an `isolationKey` therefore resume none:
//! a ticket issued to one key would otherwise be presented under another key,
//! or with none, and link requests that left on separate circuits.

use std::sync::Arc;

use futures_rustls::rustls::{client::Resumption, ClientConfig};

pub(crate) struct TlsConfigs {
    shared: Arc<ClientConfig>,
    isolated: Arc<ClientConfig>,
}

impl TlsConfigs {
    pub(crate) fn new(shared: ClientConfig) -> Self {
        let mut isolated = shared.clone();
        isolated.resumption = Resumption::disabled();
        Self {
            shared: Arc::new(shared),
            isolated: Arc::new(isolated),
        }
    }

    /// The configuration for a request, `isolated` when it has an isolation key.
    pub(crate) fn for_request(&self, isolated: bool) -> Arc<ClientConfig> {
        Arc::clone(if isolated { &self.isolated } else { &self.shared })
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
    fn requests_with_an_isolation_key_resume_no_session() {
        let (tls, server) = configs();
        let shared = || handshake(tls.for_request(false), &server);
        let isolated = || handshake(tls.for_request(true), &server);

        assert_eq!(shared(), HandshakeKind::Full);
        assert_eq!(shared(), HandshakeKind::Resumed);
        assert_eq!(isolated(), HandshakeKind::Full);
        assert_eq!(isolated(), HandshakeKind::Full);
        assert_eq!(shared(), HandshakeKind::Resumed);
    }
}
