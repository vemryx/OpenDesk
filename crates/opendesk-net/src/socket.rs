//! O socket UDP do nó, com derivações para o que não é QUIC puro.
//!
//! Descobrir o endereço público (STUN) e perfurar o NAT só funcionam se
//! saírem **do mesmo socket** que o QUIC usa, porque é o mapeamento desse
//! socket no roteador que interessa. Este invólucro fica entre o quinn e o
//! socket real e faz três coisas:
//!
//! * separa respostas STUN e pacotes de perfuração do tráfego QUIC;
//! * envia datagramas crus (pedidos STUN, perfuração, apresentação ao relay);
//! * **rotas de relay**: para o quinn, um par alcançado por relay tem um
//!   endereço virtual. Na saída, os pacotes para esse endereço são
//!   embrulhados com o token da sessão e enviados ao relay; na entrada, o
//!   envelope é removido e o pacote é entregue como se viesse do endereço
//!   virtual. O QUIC não fica sabendo que há um relay no caminho.
//!
//! A separação não tem ambiguidade: STUN, perfuração e envelope de relay
//! começam com os dois bits altos zerados, e todo pacote QUIC v1 tem o segundo
//! bit mais alto ligado. Isso só vale porque o nó desliga o "greasing" desse
//! bit (RFC 9287), que o quinn liga por padrão; ver `Node::bind`.

use crate::stun;
use opendesk_proto::relay::{self, HEADER_LEN, Token};
use quinn::udp::{RecvMeta, Transmit};
use quinn::{AsyncUdpSocket, UdpPoller};
use std::collections::HashMap;
use std::io::{self, IoSliceMut};
use std::net::{Ipv4Addr, SocketAddr};
use std::pin::Pin;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, RwLock};
use std::task::{Context, Poll, ready};
use std::time::{Duration, Instant};
use tokio::sync::mpsc;

/// Conteúdo dos pacotes de perfuração. Só servem para abrir o mapeamento no
/// NAT de quem envia; quem recebe os descarta.
pub(crate) const PUNCH_PACKET: &[u8] = b"\x00opendesk-punch";

/// Uma rota de relay sem tráfego por este tempo é esquecida.
const ROUTE_IDLE: Duration = Duration::from_secs(120);

struct Route {
    relay: SocketAddr,
    token: Token,
    virtual_addr: SocketAddr,
    /// Milissegundos (desde `TapSocket::epoch`) do último pacote recebido.
    last_seen: AtomicU64,
}

#[derive(Default)]
struct Routes {
    by_virtual: HashMap<SocketAddr, Arc<Route>>,
    by_token: HashMap<Token, Arc<Route>>,
    next: u32,
}

pub(crate) struct TapSocket {
    inner: Arc<dyn AsyncUdpSocket>,
    stun: mpsc::UnboundedSender<Vec<u8>>,
    routes: RwLock<Routes>,
    /// Evita tomar a trava no caminho quente quando não há relay em uso.
    has_routes: AtomicBool,
    epoch: Instant,
}

impl std::fmt::Debug for TapSocket {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("TapSocket").field("inner", &self.inner).finish_non_exhaustive()
    }
}

impl TapSocket {
    pub(crate) fn new(inner: Arc<dyn AsyncUdpSocket>) -> (Arc<Self>, mpsc::UnboundedReceiver<Vec<u8>>) {
        let (stun, responses) = mpsc::unbounded_channel();
        let socket =
            Self { inner, stun, routes: RwLock::default(), has_routes: AtomicBool::new(false), epoch: Instant::now() };
        (Arc::new(socket), responses)
    }

    /// Envia um datagrama cru pelo socket do QUIC. Melhor esforço: se o
    /// sistema não puder enviar agora, o pacote é simplesmente perdido.
    pub(crate) fn send_raw(&self, destination: SocketAddr, contents: &[u8]) {
        let transmit = Transmit { destination, ecn: None, contents, segment_size: None, src_ip: None };
        if let Err(err) = self.inner.try_send(&transmit) {
            tracing::trace!("datagrama cru para {destination} não enviado: {err}");
        }
    }

    /// Cria (ou reaproveita) a rota para a sessão `token` no relay `relay` e
    /// devolve o endereço virtual que a representa para o QUIC.
    pub(crate) fn add_route(&self, relay: SocketAddr, token: Token) -> SocketAddr {
        let now = self.now_ms();
        let mut routes = self.routes.write().unwrap();
        let idle = ROUTE_IDLE.as_millis() as u64;
        let stale: Vec<Token> = routes
            .by_token
            .values()
            .filter(|route| now.saturating_sub(route.last_seen.load(Ordering::Relaxed)) > idle)
            .map(|route| route.token)
            .collect();
        for token in stale {
            if let Some(route) = routes.by_token.remove(&token) {
                routes.by_virtual.remove(&route.virtual_addr);
            }
        }
        if let Some(existing) = routes.by_token.get(&token) {
            return existing.virtual_addr;
        }
        // Endereços 240.0.0.0/4 são reservados e nunca aparecem na internet,
        // então não colidem com um par de verdade.
        routes.next = routes.next.wrapping_add(1);
        let ip = Ipv4Addr::from(0xF000_0000 | (routes.next & 0x0FFF_FFFF));
        let virtual_addr = SocketAddr::from((ip, relay::DEFAULT_PORT));
        let route = Arc::new(Route { relay, token, virtual_addr, last_seen: AtomicU64::new(now) });
        routes.by_virtual.insert(virtual_addr, route.clone());
        routes.by_token.insert(token, route);
        self.has_routes.store(true, Ordering::Release);
        virtual_addr
    }

    /// O relay por trás de um endereço virtual, se for um.
    pub(crate) fn relay_of(&self, addr: SocketAddr) -> Option<SocketAddr> {
        self.routes.read().unwrap().by_virtual.get(&addr).map(|route| route.relay)
    }

    fn now_ms(&self) -> u64 {
        self.epoch.elapsed().as_millis() as u64
    }

    /// Remove os envelopes de relay de um buffer recebido, no lugar. Um
    /// buffer pode trazer vários datagramas de mesmo tamanho (`stride`).
    fn unwrap_relayed(&self, buf: &mut [u8], meta: &mut RecvMeta) {
        let (len, stride) = (meta.len, if meta.stride == 0 { meta.len } else { meta.stride });
        let routes = self.routes.read().unwrap();
        let (mut written, mut new_stride, mut source) = (0usize, 0usize, None::<SocketAddr>);
        let mut offset = 0;
        while offset < len {
            let end = (offset + stride).min(len);
            let accepted = relay::parse(&buf[offset..end]).and_then(|(token, payload)| {
                let route = routes.by_token.get(&token)?;
                // Só do relay combinado, e um único par por buffer.
                let same_peer = source.is_none_or(|addr| addr == route.virtual_addr);
                (route.relay == meta.addr && same_peer && !payload.is_empty()).then(|| (route.clone(), payload.len()))
            });
            if let Some((route, payload_len)) = accepted {
                buf.copy_within(offset + HEADER_LEN..end, written);
                written += payload_len;
                if new_stride == 0 {
                    new_stride = payload_len;
                }
                source = Some(route.virtual_addr);
                route.last_seen.store(self.now_ms(), Ordering::Relaxed);
            }
            offset = end;
        }
        meta.len = written;
        meta.stride = new_stride.max(1);
        if let Some(addr) = source {
            meta.addr = addr;
        }
    }
}

impl AsyncUdpSocket for TapSocket {
    fn create_io_poller(self: Arc<Self>) -> Pin<Box<dyn UdpPoller>> {
        self.inner.clone().create_io_poller()
    }

    fn try_send(&self, transmit: &Transmit) -> io::Result<()> {
        if self.has_routes.load(Ordering::Acquire) {
            let route = self.routes.read().unwrap().by_virtual.get(&transmit.destination).cloned();
            if let Some(route) = route {
                // Um envio pode trazer vários pacotes de `segment_size`
                // bytes; cada um ganha o seu envelope.
                let segment = transmit.segment_size.unwrap_or(transmit.contents.len()).max(1);
                for packet in transmit.contents.chunks(segment) {
                    let wrapped = relay::wrap(&route.token, packet);
                    self.inner.try_send(&Transmit {
                        destination: route.relay,
                        ecn: transmit.ecn,
                        contents: &wrapped,
                        segment_size: None,
                        src_ip: None,
                    })?;
                }
                return Ok(());
            }
        }
        self.inner.try_send(transmit)
    }

    fn poll_recv(
        &self,
        cx: &mut Context,
        bufs: &mut [IoSliceMut<'_>],
        meta: &mut [RecvMeta],
    ) -> Poll<io::Result<usize>> {
        let count = ready!(self.inner.poll_recv(cx, bufs, meta))?;
        let relaying = self.has_routes.load(Ordering::Acquire);
        for (buf, meta) in bufs.iter_mut().zip(meta.iter_mut()).take(count) {
            if meta.len == 0 {
                continue;
            }
            if relaying && buf[0] == relay::MAGIC {
                self.unwrap_relayed(&mut buf[..], meta);
                continue;
            }
            // Só sai do caminho do QUIC o que é reconhecido com certeza;
            // qualquer outra coisa segue para o quinn decidir.
            let data = &buf[..meta.len];
            let is_stun = stun::looks_like_stun(data);
            if is_stun {
                let _ = self.stun.send(data.to_vec());
            }
            if is_stun || data == PUNCH_PACKET {
                // Comprimento zero: o quinn pula este datagrama.
                meta.len = 0;
            }
        }
        Poll::Ready(Ok(count))
    }

    fn local_addr(&self) -> io::Result<SocketAddr> {
        self.inner.local_addr()
    }

    fn max_transmit_segments(&self) -> usize {
        self.inner.max_transmit_segments()
    }

    fn max_receive_segments(&self) -> usize {
        self.inner.max_receive_segments()
    }

    fn may_fragment(&self) -> bool {
        self.inner.may_fragment()
    }
}
