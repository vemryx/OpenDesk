//! O socket UDP do nó, com uma derivação para o que não é QUIC.
//!
//! Descobrir o endereço público (STUN) e perfurar o NAT só funcionam se
//! saírem **do mesmo socket** que o QUIC usa, porque é o mapeamento desse
//! socket no roteador que interessa. Este invólucro fica entre o quinn e o
//! socket real: deixa passar tudo o que é QUIC e separa o resto.
//!
//! A separação não tem ambiguidade: STUN e os nossos pacotes de perfuração
//! começam com os dois bits altos zerados, e todo pacote QUIC v1 tem o segundo
//! bit mais alto ligado. Isso só vale porque o nó desliga o "greasing" desse
//! bit (RFC 9287), que o quinn liga por padrão; ver `Node::bind`.

use crate::stun;
use quinn::udp::{RecvMeta, Transmit};
use quinn::{AsyncUdpSocket, UdpPoller};
use std::io::{self, IoSliceMut};
use std::net::SocketAddr;
use std::pin::Pin;
use std::sync::Arc;
use std::task::{Context, Poll, ready};
use tokio::sync::mpsc;

/// Conteúdo dos pacotes de perfuração. Só servem para abrir o mapeamento no
/// NAT de quem envia; quem recebe os descarta.
pub(crate) const PUNCH_PACKET: &[u8] = b"\x00opendesk-punch";

#[derive(Debug)]
pub(crate) struct TapSocket {
    inner: Arc<dyn AsyncUdpSocket>,
    stun: mpsc::UnboundedSender<Vec<u8>>,
}

impl TapSocket {
    pub(crate) fn new(inner: Arc<dyn AsyncUdpSocket>) -> (Arc<Self>, mpsc::UnboundedReceiver<Vec<u8>>) {
        let (stun, responses) = mpsc::unbounded_channel();
        (Arc::new(Self { inner, stun }), responses)
    }

    /// Envia um datagrama cru pelo socket do QUIC. Melhor esforço: se o
    /// sistema não puder enviar agora, o pacote é simplesmente perdido.
    pub(crate) fn send_raw(&self, destination: SocketAddr, contents: &[u8]) {
        let transmit = Transmit { destination, ecn: None, contents, segment_size: None, src_ip: None };
        if let Err(err) = self.inner.try_send(&transmit) {
            tracing::trace!("datagrama cru para {destination} não enviado: {err}");
        }
    }
}

impl AsyncUdpSocket for TapSocket {
    fn create_io_poller(self: Arc<Self>) -> Pin<Box<dyn UdpPoller>> {
        self.inner.clone().create_io_poller()
    }

    fn try_send(&self, transmit: &Transmit) -> io::Result<()> {
        self.inner.try_send(transmit)
    }

    fn poll_recv(
        &self,
        cx: &mut Context,
        bufs: &mut [IoSliceMut<'_>],
        meta: &mut [RecvMeta],
    ) -> Poll<io::Result<usize>> {
        let count = ready!(self.inner.poll_recv(cx, bufs, meta))?;
        for (buf, meta) in bufs.iter().zip(meta.iter_mut()).take(count) {
            let data = &buf[..meta.len];
            // Só sai do caminho do QUIC o que é reconhecido com certeza;
            // qualquer outra coisa segue para o quinn decidir.
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
