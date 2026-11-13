use crate::backend::Backend;
use crate::{Wake, describe_close};
use anyhow::{Context, Result, anyhow, bail};
use opendesk_codec::{Encoder, EncoderConfig, FrameRef};
use opendesk_net::{Fingerprint, Node, PeerConn};
use opendesk_platform::capture::DisplayInfo;
use opendesk_proto::PROTOCOL_VERSION;
use opendesk_proto::control::{Decision, DenyReason, DisplayDesc, HostMsg, Permissions, ViewerMsg};
use opendesk_proto::wire::{recv_msg, send_msg, write_frame};
use std::collections::HashMap;
use std::net::SocketAddr;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;
use tokio::sync::{mpsc, oneshot};
use tokio::time::timeout;

const HANDSHAKE_TIMEOUT: Duration = Duration::from_secs(10);
/// Quanto tempo a pessoa no host tem para responder ao pedido.
const APPROVAL_TIMEOUT: Duration = Duration::from_secs(60);
/// Espera máxima por mudança na tela antes de aproveitar para refinar.
const CAPTURE_POLL_MS: u32 = 40;

/// Quem está pedindo acesso.
#[derive(Debug, Clone)]
pub struct PeerInfo {
    pub name: String,
    pub fingerprint: Fingerprint,
    pub addr: SocketAddr,
    /// Preenchido quando a conexão chega por um relay (e `addr` é virtual).
    pub relay: Option<SocketAddr>,
    pub wants: Permissions,
}

pub enum HostEvent {
    /// Alguém quer conectar. Responda com `Some(permissões)` para autorizar
    /// ou `None` para recusar; descartar o `reply` também recusa.
    Request {
        session: u64,
        peer: PeerInfo,
        reply: oneshot::Sender<Option<Permissions>>,
    },
    Started {
        session: u64,
        peer: PeerInfo,
        permissions: Permissions,
    },
    /// Enviado para todo `Request`, tenha a sessão começado ou não.
    Ended {
        session: u64,
        reason: String,
    },
}

pub struct Host {
    node: Arc<Node>,
    backend: Arc<dyn Backend>,
    device_name: Mutex<String>,
    events: mpsc::UnboundedSender<HostEvent>,
    wake: Wake,
    connections: Mutex<HashMap<u64, quinn::Connection>>,
    next_session: AtomicU64,
    /// Há uma sessão transmitindo a tela. O Windows só permite uma captura
    /// por processo, então por enquanto o host atende um visualizador por
    /// vez; vários ao mesmo tempo exigem compartilhar a captura (roteiro).
    streaming: Arc<AtomicBool>,
}

impl Host {
    pub fn new(
        node: Arc<Node>,
        backend: Arc<dyn Backend>,
        device_name: String,
        wake: Wake,
    ) -> (Arc<Self>, mpsc::UnboundedReceiver<HostEvent>) {
        let (events, receiver) = mpsc::unbounded_channel();
        let host = Self {
            node,
            backend,
            device_name: Mutex::new(device_name),
            events,
            wake,
            connections: Mutex::new(HashMap::new()),
            next_session: AtomicU64::new(1),
            streaming: Arc::new(AtomicBool::new(false)),
        };
        (Arc::new(host), receiver)
    }

    /// Aceita conexões até o nó ser fechado.
    pub async fn serve(self: Arc<Self>) {
        while let Some(accepted) = self.node.accept().await {
            let peer = match accepted {
                Ok(peer) => peer,
                Err(err) => {
                    tracing::debug!("handshake de entrada falhou: {err:#}");
                    continue;
                }
            };
            let host = self.clone();
            tokio::spawn(async move {
                let session = host.next_session.fetch_add(1, Ordering::Relaxed);
                host.connections.lock().unwrap().insert(session, peer.conn.clone());
                let mut announced = false;
                let reason = match host.run(session, &peer, &mut announced).await {
                    Ok(reason) => reason,
                    Err(err) => format!("{err:#}"),
                };
                host.connections.lock().unwrap().remove(&session);
                peer.conn.close(0u32.into(), b"host");
                if announced {
                    host.emit(HostEvent::Ended { session, reason });
                }
            });
        }
    }

    /// Encerra uma sessão (ou cancela um pedido pendente).
    /// Troca o nome com que este computador se apresenta nas próximas sessões.
    pub fn set_device_name(&self, name: String) {
        *self.device_name.lock().unwrap() = name;
    }

    pub fn disconnect(&self, session: u64) {
        if let Some(conn) = self.connections.lock().unwrap().get(&session) {
            conn.close(0u32.into(), b"host");
        }
    }

    pub fn disconnect_all(&self) {
        for conn in self.connections.lock().unwrap().values() {
            conn.close(0u32.into(), b"host");
        }
    }

    fn emit(&self, event: HostEvent) {
        let _ = self.events.send(event);
        (self.wake)();
    }

    async fn run(&self, session: u64, peer: &PeerConn, announced: &mut bool) -> Result<String> {
        let conn = &peer.conn;
        let (mut tx, mut rx) = timeout(HANDSHAKE_TIMEOUT, conn.accept_bi())
            .await
            .map_err(|_| anyhow!("o visualizador não abriu o canal de controle"))??;
        let hello = match timeout(HANDSHAKE_TIMEOUT, recv_msg::<_, ViewerMsg>(&mut rx)).await {
            Ok(Ok(ViewerMsg::Hello(hello))) => hello,
            Ok(Ok(_)) => bail!("o visualizador não se apresentou"),
            Ok(Err(err)) => return Err(err).context("ler apresentação"),
            Err(_) => bail!("o visualizador não se apresentou a tempo"),
        };
        let device_name = self.device_name.lock().unwrap().clone();
        send_msg(
            &mut tx,
            &HostMsg::Welcome {
                protocol: PROTOCOL_VERSION,
                device_name,
                app_version: env!("CARGO_PKG_VERSION").into(),
            },
        )
        .await?;

        if hello.protocol != PROTOCOL_VERSION {
            deny(&mut tx, conn, DenyReason::VersionMismatch).await;
            bail!("versão de protocolo incompatível ({} ≠ {PROTOCOL_VERSION})", hello.protocol);
        }

        // Ocupado: nem incomoda a pessoa com um pedido que não pode atender.
        if self.streaming.load(Ordering::Acquire) {
            deny(&mut tx, conn, DenyReason::Busy).await;
            bail!("já há uma sessão em andamento");
        }

        // Nomes vêm da rede e vão parar na tela de autorização: limita o
        // tamanho e tira caracteres de controle.
        let name: String = hello.device_name.chars().filter(|c| !c.is_control()).take(64).collect();
        let info = PeerInfo {
            name: if name.trim().is_empty() { "(sem nome)".into() } else { name },
            fingerprint: peer.fingerprint,
            addr: conn.remote_address(),
            relay: self.node.via_relay(conn.remote_address()),
            wants: hello.wants,
        };

        // A decisão é sempre de quem está no computador.
        let (reply, answer) = oneshot::channel();
        *announced = true;
        self.emit(HostEvent::Request { session, peer: info.clone(), reply });
        let granted = tokio::select! {
            answer = timeout(APPROVAL_TIMEOUT, answer) => match answer {
                Ok(Ok(Some(permissions))) => permissions.intersect(hello.wants),
                Ok(_) => {
                    deny(&mut tx, conn, DenyReason::Rejected).await;
                    return Ok("Pedido recusado.".into());
                }
                Err(_) => {
                    deny(&mut tx, conn, DenyReason::Timeout).await;
                    return Ok("Pedido expirou sem resposta.".into());
                }
            },
            closed = conn.closed() => return Ok(describe_close(&closed)),
        };

        // Dois pedidos aprovados ao mesmo tempo: só o primeiro leva.
        if self.streaming.compare_exchange(false, true, Ordering::AcqRel, Ordering::Acquire).is_err() {
            deny(&mut tx, conn, DenyReason::Busy).await;
            return Ok("Já havia outra sessão em andamento.".into());
        }
        let _free_on_exit = ClearOnDrop(self.streaming.clone());

        // Sobe a captura antes de responder, para já informar a resolução.
        let stop = Arc::new(AtomicBool::new(false));
        let keyframe = Arc::new(AtomicBool::new(false));
        let (packets_tx, mut packets) = mpsc::channel::<Vec<u8>>(1);
        let (ready_tx, ready) = oneshot::channel::<Result<DisplayInfo>>();
        let _stop_on_exit = StopOnDrop(stop.clone());
        spawn_capture(self.backend.clone(), stop.clone(), keyframe.clone(), packets_tx, ready_tx)?;
        let display = match timeout(HANDSHAKE_TIMEOUT, ready).await {
            Ok(Ok(Ok(display))) => display,
            other => {
                deny(&mut tx, conn, DenyReason::CaptureUnavailable).await;
                return match other {
                    Ok(Ok(Err(err))) => Err(err.context("iniciar captura de tela")),
                    _ => Err(anyhow!("a captura de tela não iniciou")),
                };
            }
        };

        send_msg(
            &mut tx,
            &HostMsg::Decision(Decision::Granted {
                permissions: granted,
                display: DisplayDesc {
                    width: display.width as u16,
                    height: display.height as u16,
                    name: display.name.clone(),
                },
            }),
        )
        .await?;
        self.emit(HostEvent::Started { session, peer: info, permissions: granted });

        let mut sink = self.backend.open_sink(&display);
        let video = async {
            let mut stream = conn.open_uni().await.context("abrir canal de vídeo")?;
            while let Some(packet) = packets.recv().await {
                write_frame(&mut stream, &packet).await.context("enviar quadro")?;
            }
            anyhow::Ok("A captura de tela parou.".to_string())
        };
        let control = async {
            loop {
                match recv_msg::<_, ViewerMsg>(&mut rx).await {
                    // A permissão é conferida aqui, no host: um visualizador
                    // modificado não ganha controle só por mandar eventos.
                    Ok(ViewerMsg::Input(event)) if granted.control => sink.apply(event),
                    Ok(ViewerMsg::Input(_)) | Ok(ViewerMsg::Hello(_)) => {}
                    Ok(ViewerMsg::RequestKeyframe) => keyframe.store(true, Ordering::Relaxed),
                    Ok(ViewerMsg::Bye) => return "O visualizador encerrou a sessão.".to_string(),
                    // A leitura falha no mesmo instante em que a conexão cai;
                    // o motivo real está na conexão.
                    Err(_) => {
                        return match conn.close_reason() {
                            Some(closed) => describe_close(&closed),
                            None => "O canal de controle foi fechado.".to_string(),
                        };
                    }
                }
            }
        };
        let reason = tokio::select! {
            result = video => result.unwrap_or_else(|err| format!("{err:#}")),
            reason = control => reason,
            closed = conn.closed() => describe_close(&closed),
        };
        sink.release_all();
        Ok(reason)
    }
}

struct ClearOnDrop(Arc<AtomicBool>);

impl Drop for ClearOnDrop {
    fn drop(&mut self) {
        self.0.store(false, Ordering::Release);
    }
}

struct StopOnDrop(Arc<AtomicBool>);

impl Drop for StopOnDrop {
    fn drop(&mut self) {
        self.0.store(true, Ordering::Relaxed);
    }
}

async fn deny(tx: &mut quinn::SendStream, conn: &quinn::Connection, reason: DenyReason) {
    let _ = send_msg(tx, &HostMsg::Decision(Decision::Denied { reason })).await;
    let _ = tx.finish();
    // Dá tempo de a recusa chegar antes de a conexão ser fechada.
    let _ = timeout(Duration::from_secs(2), conn.closed()).await;
}

/// Thread de captura e codificação. O canal de capacidade 1 é a
/// contrapressão: enquanto a rede não escoa o quadro anterior, esta thread
/// fica parada no envio e as mudanças de tela se acumulam num único quadro
/// seguinte, em vez de virarem fila e atraso.
fn spawn_capture(
    backend: Arc<dyn Backend>,
    stop: Arc<AtomicBool>,
    keyframe: Arc<AtomicBool>,
    packets: mpsc::Sender<Vec<u8>>,
    ready: oneshot::Sender<Result<DisplayInfo>>,
) -> Result<()> {
    std::thread::Builder::new()
        .name("opendesk-capture".into())
        .spawn(move || {
            let mut source = match backend.open_source() {
                Ok(source) => source,
                Err(err) => {
                    let _ = ready.send(Err(err));
                    return;
                }
            };
            if ready.send(Ok(source.display())).is_err() {
                return;
            }
            let mut encoder = Encoder::new(EncoderConfig::default());
            while !stop.load(Ordering::Relaxed) {
                let packet = match source.frame(CAPTURE_POLL_MS) {
                    Ok(Some(frame)) => {
                        let frame = FrameRef {
                            data: frame.data,
                            width: frame.width,
                            height: frame.height,
                            stride: frame.stride,
                        };
                        let delta = encoder.encode(&frame);
                        // Um pedido de quadro completo substitui o delta.
                        if keyframe.swap(false, Ordering::Relaxed) { encoder.keyframe() } else { delta }
                    }
                    Ok(None) if keyframe.swap(false, Ordering::Relaxed) => encoder.keyframe(),
                    // Tela parada: aproveita para reenviar sem perdas o que
                    // foi como JPEG.
                    Ok(None) => encoder.refine(),
                    Err(err) => {
                        tracing::warn!("captura de tela falhou: {err:#}");
                        break;
                    }
                };
                if let Some(packet) = packet
                    && packets.blocking_send(packet.encode()).is_err()
                {
                    break;
                }
            }
        })
        .context("criar thread de captura")?;
    Ok(())
}
