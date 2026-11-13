use crate::identity::{Fingerprint, Identity};
use anyhow::{Context, Result, anyhow, bail};
use quinn::crypto::rustls::{QuicClientConfig, QuicServerConfig};
use rustls::client::danger::{HandshakeSignatureValid, ServerCertVerified, ServerCertVerifier};
use rustls::crypto::{WebPkiSupportedAlgorithms, verify_tls12_signature, verify_tls13_signature};
use rustls::pki_types::{CertificateDer, PrivateKeyDer, PrivatePkcs8KeyDer, ServerName, UnixTime};
use rustls::server::danger::{ClientCertVerified, ClientCertVerifier};
use rustls::{DigitallySignedStruct, DistinguishedName, SignatureScheme};
use std::net::SocketAddr;
use std::sync::Arc;
use std::time::Duration;

/// Nome TLS fictício: a autenticação é por impressão digital, não por nome.
const TLS_NAME: &str = "opendesk.device";

/// Quanto o QUIC pode enfileirar antes de bloquear quem envia. Pequeno de
/// propósito: é essa contrapressão que faz o host pular quadros quando a rede
/// não dá conta, em vez de acumular atraso.
const SEND_WINDOW: u32 = 2 * 1024 * 1024;

const KEEP_ALIVE: Duration = Duration::from_secs(2);
const IDLE_TIMEOUT: Duration = Duration::from_secs(8);

/// Conexão estabelecida e autenticada com outro dispositivo.
pub struct PeerConn {
    pub conn: quinn::Connection,
    /// Impressão digital do certificado que o par provou possuir no handshake.
    pub fingerprint: Fingerprint,
}

/// Ponto de rede deste dispositivo: aceita e inicia conexões pelo mesmo
/// socket UDP (o que mais tarde permite perfuração de NAT).
pub struct Node {
    endpoint: quinn::Endpoint,
    identity: Arc<Identity>,
}

impl Node {
    /// Abre o socket. Precisa ser chamado dentro de um runtime tokio.
    pub fn bind(identity: Arc<Identity>, addr: SocketAddr) -> Result<Self> {
        let provider = Arc::new(rustls::crypto::ring::default_provider());
        let verifier = Arc::new(AnyDeviceCert(provider.signature_verification_algorithms));
        let chain = vec![CertificateDer::from(identity.cert_der.clone())];
        let key = || PrivateKeyDer::Pkcs8(PrivatePkcs8KeyDer::from(identity.key_pkcs8_der.clone()));

        let mut transport = quinn::TransportConfig::default();
        // Um par que some sem se despedir (queda de rede, processo morto) é
        // dado como perdido em no máximo IDLE_TIMEOUT + KEEP_ALIVE.
        transport.keep_alive_interval(Some(KEEP_ALIVE));
        transport.max_idle_timeout(Some(IDLE_TIMEOUT.try_into()?));
        transport.send_window(SEND_WINDOW as u64);
        let transport = Arc::new(transport);

        let mut server_tls = rustls::ServerConfig::builder_with_provider(provider.clone())
            .with_protocol_versions(&[&rustls::version::TLS13])?
            .with_client_cert_verifier(verifier.clone())
            .with_single_cert(chain.clone(), key())
            .context("configuração TLS de servidor")?;
        server_tls.alpn_protocols = vec![opendesk_proto::ALPN.to_vec()];
        let mut server = quinn::ServerConfig::with_crypto(Arc::new(QuicServerConfig::try_from(server_tls)?));
        server.transport_config(transport.clone());

        let mut client_tls = rustls::ClientConfig::builder_with_provider(provider)
            .with_protocol_versions(&[&rustls::version::TLS13])?
            .dangerous()
            .with_custom_certificate_verifier(verifier)
            .with_client_auth_cert(chain, key())
            .context("configuração TLS de cliente")?;
        client_tls.alpn_protocols = vec![opendesk_proto::ALPN.to_vec()];
        let mut client = quinn::ClientConfig::new(Arc::new(QuicClientConfig::try_from(client_tls)?));
        client.transport_config(transport);

        let mut endpoint = quinn::Endpoint::server(server, addr).with_context(|| format!("abrir UDP em {addr}"))?;
        endpoint.set_default_client_config(client);
        Ok(Self { endpoint, identity })
    }

    pub fn identity(&self) -> &Arc<Identity> {
        &self.identity
    }

    pub fn local_addr(&self) -> Result<SocketAddr> {
        Ok(self.endpoint.local_addr()?)
    }

    /// Espera a próxima conexão de entrada e conclui o handshake. `None`
    /// quando o nó foi fechado. Um handshake que falha devolve `Some(Err)`.
    pub async fn accept(&self) -> Option<Result<PeerConn>> {
        let incoming = self.endpoint.accept().await?;
        Some(async move { authenticated(incoming.await.context("handshake de entrada")?) }.await)
    }

    /// Conecta a outro dispositivo. Se `expected` for informado, a conexão só
    /// é aceita se o par apresentar exatamente essa impressão digital.
    pub async fn connect(&self, addr: SocketAddr, expected: Option<Fingerprint>) -> Result<PeerConn> {
        let connecting = self.endpoint.connect(addr, TLS_NAME).context("iniciar conexão")?;
        let peer = authenticated(connecting.await.with_context(|| format!("conectar a {addr}"))?)?;
        if let Some(expected) = expected
            && peer.fingerprint != expected
        {
            peer.conn.close(1u32.into(), b"fingerprint mismatch");
            bail!(
                "o dispositivo em {addr} apresentou a identidade {} em vez de {}",
                peer.fingerprint.short(),
                expected.short()
            );
        }
        Ok(peer)
    }

    /// Encerra todas as conexões e para de aceitar novas.
    pub fn close(&self) {
        self.endpoint.close(0u32.into(), b"shutdown");
    }
}

fn authenticated(conn: quinn::Connection) -> Result<PeerConn> {
    let certs = conn
        .peer_identity()
        .and_then(|any| any.downcast::<Vec<CertificateDer<'static>>>().ok())
        .ok_or_else(|| anyhow!("o par não apresentou certificado"))?;
    let leaf = certs.first().ok_or_else(|| anyhow!("cadeia de certificados vazia"))?;
    Ok(PeerConn { fingerprint: Fingerprint::of_cert(leaf), conn })
}

/// Aceita qualquer certificado de dispositivo, mas **verifica a assinatura do
/// handshake**: o par prova que possui a chave privada do certificado que
/// apresentou. Quem decide se aquele dispositivo é confiável é a camada de
/// cima, pela impressão digital (aprovação humana no host; conferência ou
/// fixação no visualizador). Não há autoridade certificadora no OpenDesk.
#[derive(Debug)]
struct AnyDeviceCert(WebPkiSupportedAlgorithms);

impl ServerCertVerifier for AnyDeviceCert {
    fn verify_server_cert(
        &self,
        _end_entity: &CertificateDer<'_>,
        _intermediates: &[CertificateDer<'_>],
        _server_name: &ServerName<'_>,
        _ocsp_response: &[u8],
        _now: UnixTime,
    ) -> Result<ServerCertVerified, rustls::Error> {
        Ok(ServerCertVerified::assertion())
    }

    fn verify_tls12_signature(
        &self,
        message: &[u8],
        cert: &CertificateDer<'_>,
        dss: &DigitallySignedStruct,
    ) -> Result<HandshakeSignatureValid, rustls::Error> {
        verify_tls12_signature(message, cert, dss, &self.0)
    }

    fn verify_tls13_signature(
        &self,
        message: &[u8],
        cert: &CertificateDer<'_>,
        dss: &DigitallySignedStruct,
    ) -> Result<HandshakeSignatureValid, rustls::Error> {
        verify_tls13_signature(message, cert, dss, &self.0)
    }

    fn supported_verify_schemes(&self) -> Vec<SignatureScheme> {
        self.0.supported_schemes()
    }
}

impl ClientCertVerifier for AnyDeviceCert {
    fn root_hint_subjects(&self) -> &[DistinguishedName] {
        &[]
    }

    fn verify_client_cert(
        &self,
        _end_entity: &CertificateDer<'_>,
        _intermediates: &[CertificateDer<'_>],
        _now: UnixTime,
    ) -> Result<ClientCertVerified, rustls::Error> {
        Ok(ClientCertVerified::assertion())
    }

    fn verify_tls12_signature(
        &self,
        message: &[u8],
        cert: &CertificateDer<'_>,
        dss: &DigitallySignedStruct,
    ) -> Result<HandshakeSignatureValid, rustls::Error> {
        verify_tls12_signature(message, cert, dss, &self.0)
    }

    fn verify_tls13_signature(
        &self,
        message: &[u8],
        cert: &CertificateDer<'_>,
        dss: &DigitallySignedStruct,
    ) -> Result<HandshakeSignatureValid, rustls::Error> {
        verify_tls13_signature(message, cert, dss, &self.0)
    }

    fn supported_verify_schemes(&self) -> Vec<SignatureScheme> {
        self.0.supported_schemes()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use opendesk_proto::wire::{read_frame, write_frame};

    fn node() -> Node {
        Node::bind(Arc::new(Identity::generate().unwrap()), "127.0.0.1:0".parse().unwrap()).unwrap()
    }

    #[tokio::test]
    async fn peers_authenticate_each_other_and_exchange_data() {
        let (host, viewer) = (node(), node());
        let host_addr = host.local_addr().unwrap();
        let (host_fp, viewer_fp) = (host.identity().fingerprint(), viewer.identity().fingerprint());

        let server = tokio::spawn(async move {
            let peer = host.accept().await.unwrap().unwrap();
            let (mut tx, mut rx) = peer.conn.accept_bi().await.unwrap();
            let got = read_frame(&mut rx, 1024).await.unwrap();
            write_frame(&mut tx, &got).await.unwrap();
            tx.finish().unwrap();
            peer.conn.closed().await;
            peer.fingerprint
        });

        let peer = viewer.connect(host_addr, Some(host_fp)).await.unwrap();
        assert_eq!(peer.fingerprint, host_fp);
        let (mut tx, mut rx) = peer.conn.open_bi().await.unwrap();
        write_frame(&mut tx, b"ola").await.unwrap();
        assert_eq!(read_frame(&mut rx, 1024).await.unwrap(), b"ola");
        peer.conn.close(0u32.into(), b"fim");

        assert_eq!(server.await.unwrap(), viewer_fp, "o host enxerga a identidade do visualizador");
    }

    #[tokio::test]
    async fn connect_fails_when_fingerprint_does_not_match() {
        let (host, viewer) = (node(), node());
        let host_addr = host.local_addr().unwrap();
        tokio::spawn(async move {
            while let Some(peer) = host.accept().await {
                if let Ok(peer) = peer {
                    peer.conn.closed().await;
                }
            }
        });
        let wrong = Fingerprint([0xEE; 32]);
        let err = viewer.connect(host_addr, Some(wrong)).await.err().expect("deve recusar");
        assert!(err.to_string().contains("identidade"), "{err}");
    }
}
