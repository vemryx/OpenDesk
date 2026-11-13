use crate::identity::{Fingerprint, Identity};
use crate::socket::{PUNCH_PACKET, TapSocket};
use crate::stun;
use anyhow::{Context, Result, anyhow, bail};
use quinn::Runtime as _;
use quinn::crypto::rustls::{QuicClientConfig, QuicServerConfig};
use ring::rand::SecureRandom as _;
use rustls::client::danger::{HandshakeSignatureValid, ServerCertVerified, ServerCertVerifier};
use rustls::crypto::{WebPkiSupportedAlgorithms, verify_tls12_signature, verify_tls13_signature};
use rustls::pki_types::{CertificateDer, PrivateKeyDer, PrivatePkcs8KeyDer, ServerName, UnixTime};
use rustls::server::danger::{ClientCertVerified, ClientCertVerifier};
use rustls::{DigitallySignedStruct, DistinguishedName, SignatureScheme};
use std::net::SocketAddr;
use std::sync::Arc;
use std::time::Duration;
use tokio::sync::{Mutex, mpsc};
use tokio::time::Instant;

/// Nome TLS fictício: a autenticação é por impressão digital, não por nome.
const TLS_NAME: &str = "opendesk.device";

/// Quanto o QUIC pode enfileirar antes de bloquear quem envia. Pequeno de
/// propósito: é essa contrapressão que faz o host pular quadros quando a rede
/// não dá conta, em vez de acumular atraso.
const SEND_WINDOW: u32 = 2 * 1024 * 1024;

const KEEP_ALIVE: Duration = Duration::from_secs(2);
const IDLE_TIMEOUT: Duration = Duration::from_secs(8);

/// Estimativa inicial de ida e volta. Menor que o padrão do QUIC para que o
/// primeiro pacote seja reenviado logo: numa perfuração de NAT o primeiro
/// costuma se perder, porque o caminho de volta ainda não está aberto.
const INITIAL_RTT: Duration = Duration::from_millis(100);

const PUNCH_INTERVAL: Duration = Duration::from_millis(150);
const STUN_TIMEOUT: Duration = Duration::from_millis(1500);
const STUN_RESEND: Duration = Duration::from_millis(300);
/// Depois da primeira resposta, quanto ainda esperar pelas outras.
const STUN_GRACE: Duration = Duration::from_millis(400);
const MAX_CANDIDATES: usize = 8;

/// Como o roteador trata este socket, visto de fora.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Nat {
    /// Nenhum servidor STUN respondeu.
    Unknown,
    /// O mesmo endereço público para destinos diferentes: a perfuração
    /// costuma funcionar.
    Stable,
    /// Um endereço público por destino ("NAT simétrico"): a conexão direta
    /// pela internet raramente funciona, e será preciso um relay.
    Symmetric,
}

/// O endereço deste nó visto da internet.
#[derive(Debug, Clone, Copy)]
pub struct PublicAddress {
    pub addr: Option<SocketAddr>,
    pub nat: Nat,
}

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
    socket: Arc<TapSocket>,
    stun_responses: Mutex<mpsc::UnboundedReceiver<Vec<u8>>>,
    bound: SocketAddr,
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
        transport.initial_rtt(INITIAL_RTT);
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

        let udp = std::net::UdpSocket::bind(addr).with_context(|| format!("abrir UDP em {addr}"))?;
        let bound = udp.local_addr()?;
        let runtime = Arc::new(quinn::TokioRuntime);
        let (socket, stun_responses) = TapSocket::new(runtime.wrap_udp_socket(udp)?);
        // Sem greasing do bit fixo: é ele que distingue QUIC de STUN e dos
        // pacotes de perfuração que dividem este socket (ver `socket.rs`).
        let mut endpoint_config = quinn::EndpointConfig::default();
        endpoint_config.grease_quic_bit(false);
        let mut endpoint =
            quinn::Endpoint::new_with_abstract_socket(endpoint_config, Some(server), socket.clone(), runtime)?;
        endpoint.set_default_client_config(client);
        Ok(Self { endpoint, identity, socket, stun_responses: Mutex::new(stun_responses), bound })
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
        dial(&self.endpoint, addr, expected).await
    }

    /// Tenta todos os endereços ao mesmo tempo e fica com o primeiro que
    /// completar o handshake. Devolve também qual endereço venceu.
    pub async fn connect_any(
        &self,
        candidates: &[SocketAddr],
        expected: Option<Fingerprint>,
        limit: Duration,
    ) -> Result<(PeerConn, SocketAddr)> {
        let mut attempts = tokio::task::JoinSet::new();
        for &addr in candidates.iter().take(MAX_CANDIDATES) {
            let endpoint = self.endpoint.clone();
            attempts.spawn(async move { dial(&endpoint, addr, expected).await.map(|peer| (peer, addr)) });
        }
        let race = async move {
            let mut last = anyhow!("nenhum endereço para tentar");
            // Ao sair daqui o JoinSet é descartado e cancela as outras tentativas.
            while let Some(joined) = attempts.join_next().await {
                match joined {
                    Ok(Ok(found)) => return Ok(found),
                    Ok(Err(err)) => last = err,
                    Err(_) => {}
                }
            }
            Err(last)
        };
        match tokio::time::timeout(limit, race).await {
            Ok(result) => result,
            Err(_) => bail!("nenhum dos endereços respondeu a tempo"),
        }
    }

    /// Endereços deste nó na rede local.
    pub fn local_candidates(&self) -> Vec<SocketAddr> {
        if !self.bound.ip().is_unspecified() {
            return vec![self.bound];
        }
        crate::local_ip().map(|ip| SocketAddr::new(ip, self.bound.port())).into_iter().collect()
    }

    /// Pergunta a servidores STUN como este socket aparece na internet.
    /// Consultar dois servidores permite notar um NAT simétrico.
    pub async fn public_address(&self, stun_servers: &[String]) -> PublicAddress {
        let mut responses = self.stun_responses.lock().await;
        while responses.try_recv().is_ok() {}

        let rng = ring::rand::SystemRandom::new();
        let mut pending = Vec::new();
        for server in stun_servers {
            let resolved = tokio::time::timeout(Duration::from_secs(1), tokio::net::lookup_host(server.as_str())).await;
            let Ok(Ok(mut addrs)) = resolved else {
                tracing::debug!("servidor STUN {server} não resolvido");
                continue;
            };
            if let Some(addr) = addrs.find(SocketAddr::is_ipv4) {
                let mut transaction = [0u8; 12];
                if rng.fill(&mut transaction).is_ok() {
                    pending.push((transaction, addr));
                }
            }
        }

        let mut seen: Vec<SocketAddr> = Vec::new();
        let mut deadline = Instant::now() + STUN_TIMEOUT;
        let mut resend = tokio::time::interval(STUN_RESEND);
        while !pending.is_empty() {
            tokio::select! {
                _ = resend.tick() => {
                    for (transaction, server) in &pending {
                        self.socket.send_raw(*server, &stun::binding_request(transaction));
                    }
                }
                Some(data) = responses.recv() => {
                    if let Some((transaction, mapped)) = stun::parse_binding_response(&data)
                        && let Some(index) = pending.iter().position(|(t, _)| *t == transaction)
                    {
                        pending.swap_remove(index);
                        seen.push(mapped);
                        deadline = deadline.min(Instant::now() + STUN_GRACE);
                    }
                }
                _ = tokio::time::sleep_until(deadline) => break,
            }
        }

        let nat = match seen.as_slice() {
            [] => Nat::Unknown,
            [first, rest @ ..] if rest.iter().all(|other| other == first) => Nat::Stable,
            _ => Nat::Symmetric,
        };
        PublicAddress { addr: seen.first().copied(), nat }
    }

    /// Endereços onde este nó pode ser alcançado: os locais e o público.
    pub async fn candidates(&self, stun_servers: &[String]) -> (Vec<SocketAddr>, Nat) {
        let mut list = self.local_candidates();
        // Um nó preso ao loopback não tem endereço público.
        let public = if self.bound.ip().is_loopback() {
            PublicAddress { addr: None, nat: Nat::Unknown }
        } else {
            self.public_address(stun_servers).await
        };
        if let Some(addr) = public.addr
            && !list.contains(&addr)
        {
            list.push(addr);
        }
        (list, public.nat)
    }

    /// Prepara o caminho por um relay para a sessão `token` e devolve o
    /// endereço (virtual) a usar para alcançar o par por ele: conectar nesse
    /// endereço, ou aceitar uma conexão vinda dele, passa pelo relay sem que o
    /// QUIC perceba. Também se apresenta ao relay por alguns segundos, para
    /// ele saber para onde repassar.
    pub fn relay_route(&self, relay: SocketAddr, token: opendesk_proto::relay::Token) -> SocketAddr {
        let virtual_addr = self.socket.add_route(relay, token);
        let socket = self.socket.clone();
        let hello = opendesk_proto::relay::wrap(&token, &[]);
        tokio::spawn(async move {
            let mut tick = tokio::time::interval(Duration::from_millis(200));
            for _ in 0..25 {
                tick.tick().await;
                socket.send_raw(relay, &hello);
            }
        });
        virtual_addr
    }

    /// Se `addr` é o endereço virtual de um par alcançado por relay, o
    /// endereço real do relay.
    pub fn via_relay(&self, addr: SocketAddr) -> Option<SocketAddr> {
        self.socket.relay_of(addr)
    }

    /// Envia pacotes vazios de sentido para `targets` por `duration`, só para
    /// o roteador abrir o caminho de volta para eles.
    pub fn punch(&self, targets: &[SocketAddr], duration: Duration) {
        let socket = self.socket.clone();
        let targets: Vec<SocketAddr> = targets
            .iter()
            .copied()
            .filter(|addr| match addr.ip() {
                std::net::IpAddr::V4(ip) => {
                    addr.port() != 0 && !ip.is_unspecified() && !ip.is_broadcast() && !ip.is_multicast()
                }
                std::net::IpAddr::V6(_) => false,
            })
            .take(MAX_CANDIDATES)
            .collect();
        tokio::spawn(async move {
            let end = Instant::now() + duration;
            let mut tick = tokio::time::interval(PUNCH_INTERVAL);
            while Instant::now() < end {
                tick.tick().await;
                for target in &targets {
                    socket.send_raw(*target, PUNCH_PACKET);
                }
            }
        });
    }

    /// Encerra todas as conexões e para de aceitar novas.
    pub fn close(&self) {
        self.endpoint.close(0u32.into(), b"shutdown");
    }
}

async fn dial(endpoint: &quinn::Endpoint, addr: SocketAddr, expected: Option<Fingerprint>) -> Result<PeerConn> {
    let connecting = endpoint.connect(addr, TLS_NAME).context("iniciar conexão")?;
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
    async fn connect_any_picks_the_candidate_that_answers_despite_punch_noise() {
        let (host, viewer) = (node(), node());
        let host_addr = host.local_addr().unwrap();
        let host_fp = host.identity().fingerprint();
        assert_eq!(host.local_candidates(), vec![host_addr]);

        // Os dois lados perfuram, como numa conexão por ID. Os pacotes de
        // perfuração chegam no socket do QUIC e têm de ser ignorados.
        host.punch(&[viewer.local_addr().unwrap()], Duration::from_millis(600));
        viewer.punch(&[host_addr], Duration::from_millis(600));
        tokio::spawn(async move {
            while let Some(Ok(peer)) = host.accept().await {
                tokio::spawn(async move { peer.conn.closed().await });
            }
        });

        // Uma porta do loopback onde ninguém escuta, e o host de verdade.
        let dead: SocketAddr = "127.0.0.1:9".parse().unwrap();
        let (peer, chosen) =
            viewer.connect_any(&[dead, host_addr], Some(host_fp), Duration::from_secs(5)).await.unwrap();
        assert_eq!(chosen, host_addr);
        assert_eq!(peer.fingerprint, host_fp);

        // A conexão continua de pé depois da rajada de perfuração.
        tokio::time::sleep(Duration::from_millis(700)).await;
        assert!(peer.conn.close_reason().is_none());
    }

    /// Dois nós que só se alcançam por um relay: o QUIC inteiro (handshake
    /// com autenticação mútua e dados) atravessa o envelope, nos dois sentidos.
    #[tokio::test]
    async fn peers_connect_through_a_relay_and_exchange_data() {
        let relay_socket = tokio::net::UdpSocket::bind("127.0.0.1:0").await.unwrap();
        let relay_addr = relay_socket.local_addr().unwrap();
        tokio::spawn(opendesk_relay::serve(relay_socket, opendesk_relay::Config::default()));

        let (host, viewer) = (node(), node());
        let (host_fp, viewer_fp) = (host.identity().fingerprint(), viewer.identity().fingerprint());
        let token = [42u8; opendesk_proto::relay::TOKEN_LEN];
        let host_side = host.relay_route(relay_addr, token);
        let viewer_side = viewer.relay_route(relay_addr, token);
        assert_eq!(host.via_relay(host_side), Some(relay_addr));
        assert_eq!(host.via_relay(relay_addr), None);
        // Pedir a mesma rota de novo devolve o mesmo endereço virtual.
        assert_eq!(viewer.relay_route(relay_addr, token), viewer_side);

        let server = tokio::spawn(async move {
            let peer = host.accept().await.unwrap().unwrap();
            // Para o host, o par "mora" no endereço virtual da rota.
            assert_eq!(peer.conn.remote_address(), host_side);
            let (mut tx, mut rx) = peer.conn.accept_bi().await.unwrap();
            let got = read_frame(&mut rx, 1 << 20).await.unwrap();
            write_frame(&mut tx, &got).await.unwrap();
            tx.finish().unwrap();
            peer.conn.closed().await;
            peer.fingerprint
        });

        let peer = viewer.connect(viewer_side, Some(host_fp)).await.unwrap();
        // Grande o bastante para ocupar muitos pacotes e envios agrupados.
        let payload: Vec<u8> = (0..300_000u32).map(|i| (i * 31 % 251) as u8).collect();
        let (mut tx, mut rx) = peer.conn.open_bi().await.unwrap();
        write_frame(&mut tx, &payload).await.unwrap();
        assert_eq!(read_frame(&mut rx, 1 << 20).await.unwrap(), payload);
        peer.conn.close(0u32.into(), b"fim");
        assert_eq!(server.await.unwrap(), viewer_fp);
    }

    /// Um relay que não repassa nada não pode travar a conexão para sempre.
    #[tokio::test]
    async fn relay_route_to_a_dead_relay_just_fails() {
        let viewer = node();
        let dead: SocketAddr = "127.0.0.1:9".parse().unwrap();
        let virtual_addr = viewer.relay_route(dead, [1; opendesk_proto::relay::TOKEN_LEN]);
        let attempt = viewer.connect_any(&[virtual_addr], None, Duration::from_millis(500)).await;
        assert!(attempt.is_err());
    }

    #[tokio::test]
    async fn connect_any_reports_failure_when_nobody_answers() {
        let viewer = node();
        let dead: SocketAddr = "127.0.0.1:9".parse().unwrap();
        assert!(viewer.connect_any(&[dead], None, Duration::from_millis(400)).await.is_err());
        assert!(viewer.connect_any(&[], None, Duration::from_millis(400)).await.is_err());
    }

    #[tokio::test]
    async fn loopback_node_has_no_public_candidate() {
        let n = node();
        let (candidates, nat) = n.candidates(&["stun.invalid:3478".to_string()]).await;
        assert_eq!(candidates, vec![n.local_addr().unwrap()]);
        assert_eq!(nat, Nat::Unknown);
    }

    /// Precisa de internet: consulta servidores STUN públicos de verdade.
    #[tokio::test]
    #[ignore = "usa a rede"]
    async fn discovers_public_address_from_real_stun_servers() {
        let n = Node::bind(Arc::new(Identity::generate().unwrap()), "0.0.0.0:0".parse().unwrap()).unwrap();
        let servers: Vec<String> = crate::DEFAULT_STUN_SERVERS.iter().map(|s| s.to_string()).collect();
        let public = n.public_address(&servers).await;
        println!("público: {:?}, NAT: {:?}", public.addr.map(|a| a.port()), public.nat);
        let addr = public.addr.expect("nenhum servidor STUN respondeu");
        assert!(!addr.ip().is_unspecified() && addr.port() != 0);
        assert_ne!(public.nat, Nat::Unknown);
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
