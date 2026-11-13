use crate::auth::{HostExchange, MAX_FAILURES_PER_CONNECTION, PasswordAccess, Throttle};
use crate::backend::Backend;
use crate::files::{self, FileEvent, FileState};
use crate::{Wake, describe_close};
use anyhow::{Context, Result, anyhow, bail};
use opendesk_codec::{Encoder, EncoderConfig, FrameRef};
use opendesk_net::{Fingerprint, Node, PeerConn};
use opendesk_platform::capture::DisplayInfo;
use opendesk_proto::PROTOCOL_VERSION;
use opendesk_proto::control::{Decision, DenyReason, DisplayDesc, HostMsg, MAX_CLIPBOARD_TEXT, Permissions, ViewerMsg};
use opendesk_proto::wire::{send_msg, write_frame};
use std::collections::HashMap;
use std::net::SocketAddr;
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};
use tokio::sync::{mpsc, oneshot};
use tokio::time::timeout;

const HANDSHAKE_TIMEOUT: Duration = Duration::from_secs(10);
/// Quanto tempo a pessoa no host tem para responder ao pedido.
const APPROVAL_TIMEOUT: Duration = Duration::from_secs(60);
/// Espera máxima por mudança na tela antes de aproveitar para refinar.
const CAPTURE_POLL_MS: u32 = 40;
/// De quanto em quanto tempo o host olha se algo novo foi copiado.
const CLIPBOARD_POLL: Duration = Duration::from_millis(400);

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
    Request { session: u64, peer: PeerInfo, reply: oneshot::Sender<Option<Permissions>> },
    Started {
        session: u64,
        peer: PeerInfo,
        permissions: Permissions,
        /// A sessão foi liberada pela senha de acesso não supervisionado, e
        /// não por alguém neste computador.
        by_password: bool,
    },
    /// Alguém errou a senha de acesso não supervisionado.
    PasswordFailed { session: u64, peer: PeerInfo },
    /// Um arquivo chegando ou saindo numa sessão.
    File { session: u64, transfer: FileEvent },
    /// Enviado para todo `Request`, tenha a sessão começado ou não.
    Ended { session: u64, reason: String },
}

/// Acesso não supervisionado: a senha e o que ela libera.
#[derive(Debug, Clone)]
pub struct Unattended {
    pub access: PasswordAccess,
    pub permissions: Permissions,
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
    /// `None` (o padrão): toda sessão depende de alguém aprovar aqui.
    unattended: Mutex<Option<Unattended>>,
    throttle: Mutex<Throttle>,
    /// Onde gravar o que os visualizadores enviam. `None`: não recebe.
    files_dir: Mutex<Option<PathBuf>>,
    /// Sessões em andamento que podem trocar arquivos.
    sharing: Mutex<HashMap<u64, quinn::Connection>>,
    next_file: AtomicU64,
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
            unattended: Mutex::new(None),
            throttle: Mutex::new(Throttle::default()),
            files_dir: Mutex::new(None),
            sharing: Mutex::new(HashMap::new()),
            next_file: AtomicU64::new(1),
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

    /// Troca o nome com que este computador se apresenta nas próximas sessões.
    pub fn set_device_name(&self, name: String) {
        *self.device_name.lock().unwrap() = name;
    }

    /// Liga (com `Some`) ou desliga o acesso não supervisionado. Vale para os
    /// próximos pedidos; sessões em andamento não mudam.
    pub fn set_unattended(&self, unattended: Option<Unattended>) {
        *self.unattended.lock().unwrap() = unattended;
    }

    /// Define a pasta onde ficam os arquivos recebidos.
    pub fn set_files_dir(&self, dir: Option<PathBuf>) {
        *self.files_dir.lock().unwrap() = dir;
    }

    /// Envia um arquivo a quem está na sessão `session`. O andamento chega
    /// como `HostEvent::File`.
    pub fn send_file(self: &Arc<Self>, runtime: &tokio::runtime::Handle, session: u64, path: PathBuf) {
        let id = self.next_file.fetch_add(1, Ordering::Relaxed);
        let host = self.clone();
        runtime.spawn(async move {
            let report = |name: &str, size: u64, state: FileState| {
                let transfer = FileEvent { id, incoming: false, name: name.to_string(), size, state };
                host.emit(HostEvent::File { session, transfer });
            };
            let shown = path.file_name().map(|name| name.to_string_lossy().into_owned()).unwrap_or_default();
            let (name, size) = match files::describe(&path) {
                Ok(described) => described,
                Err(err) => return report(&shown, 0, FileState::Failed(format!("{err:#}"))),
            };
            let conn = host.sharing.lock().unwrap().get(&session).cloned();
            let Some(conn) = conn else {
                return report(&name, size, FileState::Failed("Esta sessão não permite enviar arquivos.".into()));
            };
            report(&name, size, FileState::Progress(0));
            let sent =
                files::send(&conn, &path, &name, size, |done| report(&name, size, FileState::Progress(done))).await;
            report(
                &name,
                size,
                match sent {
                    Ok(files::Sent::Delivered) => FileState::Done(path.clone()),
                    Ok(files::Sent::Refused) => FileState::Refused,
                    Err(err) => FileState::Failed(format!("{err:#}")),
                },
            );
        });
    }

    /// Recebe um arquivo vindo do visualizador da sessão `session`. Quem tem
    /// permissão de arquivos envia sem perguntar: já controla este computador.
    async fn receive_file(
        self: Arc<Self>,
        session: u64,
        allowed: bool,
        mut tx: quinn::SendStream,
        mut rx: quinn::RecvStream,
    ) {
        let Ok((name, size)) = files::read_offer(&mut rx).await else { return };
        let dir = self.files_dir.lock().unwrap().clone();
        let (true, Some(dir)) = (allowed, dir) else { return files::refuse(&mut tx).await };
        let id = self.next_file.fetch_add(1, Ordering::Relaxed);
        let report = |state: FileState| {
            let transfer = FileEvent { id, incoming: true, name: name.clone(), size, state };
            self.emit(HostEvent::File { session, transfer });
        };
        report(FileState::Progress(0));
        let saved = files::save(&mut tx, &mut rx, &dir, &name, size, |done| report(FileState::Progress(done))).await;
        report(match saved {
            Ok(path) => FileState::Done(path),
            Err(err) => FileState::Failed(format!("{err:#}")),
        });
    }

    /// Encerra uma sessão (ou cancela um pedido pendente).
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

    async fn run(self: &Arc<Self>, session: u64, peer: &PeerConn, announced: &mut bool) -> Result<String> {
        let conn = &peer.conn;
        let (mut tx, rx) = timeout(HANDSHAKE_TIMEOUT, conn.accept_bi())
            .await
            .map_err(|_| anyhow!("o visualizador não abriu o canal de controle"))??;
        let mut inbox = crate::inbox::<ViewerMsg>(rx);
        let hello = match timeout(HANDSHAKE_TIMEOUT, inbox.recv()).await {
            Ok(Some(ViewerMsg::Hello(hello))) => hello,
            Ok(Some(_)) => bail!("o visualizador não se apresentou"),
            Ok(None) => bail!("o canal de controle fechou antes da apresentação"),
            Err(_) => bail!("o visualizador não se apresentou a tempo"),
        };
        let device_name = self.device_name.lock().unwrap().clone();
        let unattended = self.unattended.lock().unwrap().clone();
        send_msg(
            &mut tx,
            &HostMsg::Welcome {
                protocol: PROTOCOL_VERSION,
                device_name,
                app_version: env!("CARGO_PKG_VERSION").into(),
                password: unattended.as_ref().map(|unattended| unattended.access.challenge()),
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

        // Dois caminhos levam a uma sessão, e só eles: alguém neste computador
        // aprova o pedido, ou o visualizador prova que sabe a senha de acesso
        // não supervisionado (se ela estiver ligada). O pedido aparece na tela
        // nos dois casos.
        let (reply, mut answer) = oneshot::channel();
        *announced = true;
        self.emit(HostEvent::Request { session, peer: info.clone(), reply });
        let deadline = tokio::time::sleep(APPROVAL_TIMEOUT);
        tokio::pin!(deadline);
        let mut exchange: Option<HostExchange> = None;
        let mut failures = 0;
        let (granted, by_password) = loop {
            tokio::select! {
                answer = &mut answer => match answer {
                    Ok(Some(permissions)) => break (permissions.intersect(hello.wants), false),
                    _ => {
                        deny(&mut tx, conn, DenyReason::Rejected).await;
                        return Ok("Pedido recusado.".into());
                    }
                },
                _ = &mut deadline => {
                    deny(&mut tx, conn, DenyReason::Timeout).await;
                    return Ok("Pedido expirou sem resposta.".into());
                }
                closed = conn.closed() => return Ok(describe_close(&closed)),
                msg = inbox.recv() => match msg {
                    Some(ViewerMsg::Auth(message)) => {
                        let open = failures < MAX_FAILURES_PER_CONNECTION
                            && !self.throttle.lock().unwrap().locked(Instant::now());
                        let started = match &unattended {
                            Some(unattended) if open => {
                                let me = self.node.identity().fingerprint();
                                HostExchange::respond(&unattended.access, peer.fingerprint, me, &message).ok()
                            }
                            _ => None,
                        };
                        match started {
                            Some((state, reply)) => {
                                exchange = Some(state);
                                send_msg(&mut tx, &HostMsg::Auth(reply)).await?;
                            }
                            // Senha desligada, tentativas esgotadas ou mensagem inválida.
                            None => send_msg(&mut tx, &HostMsg::AuthFailed { retry: false }).await?,
                        }
                    }
                    Some(ViewerMsg::AuthConfirm(tag)) => {
                        let (Some(state), Some(unattended)) = (exchange.take(), &unattended) else { continue };
                        if let Some(proof) = state.check(&tag) {
                            send_msg(&mut tx, &HostMsg::AuthOk(proof)).await?;
                            break (unattended.permissions.intersect(hello.wants), true);
                        }
                        failures += 1;
                        let retry = {
                            let mut throttle = self.throttle.lock().unwrap();
                            throttle.record_failure(Instant::now());
                            failures < MAX_FAILURES_PER_CONNECTION && !throttle.locked(Instant::now())
                        };
                        self.emit(HostEvent::PasswordFailed { session, peer: info.clone() });
                        send_msg(&mut tx, &HostMsg::AuthFailed { retry }).await?;
                    }
                    Some(ViewerMsg::Bye) => return Ok("O visualizador desistiu do pedido.".into()),
                    Some(_) => {}
                    None => {
                        return Ok(match conn.close_reason() {
                            Some(closed) => describe_close(&closed),
                            None => "O canal de controle foi fechado.".into(),
                        });
                    }
                },
            }
        };

        // Dois pedidos aprovados ao mesmo tempo: só o primeiro leva.
        if self.streaming.compare_exchange(false, true, Ordering::AcqRel, Ordering::Acquire).is_err() {
            deny(&mut tx, conn, DenyReason::Busy).await;
            return Ok("Já havia outra sessão em andamento.".into());
        }
        let _free_on_exit = ClearOnDrop(self.streaming.clone());

        // Sobe a captura antes de responder, para já informar a resolução.
        let displays = self.backend.displays().clamp(1, u8::MAX as usize);
        let mut index = self.backend.default_display().min(displays - 1);
        // Cada troca de monitor começa uma geração nova de quadros; os da
        // geração anterior que ainda estiverem a caminho são descartados.
        let generation = Arc::new(AtomicU64::new(0));
        let (packets_tx, mut packets) = mpsc::channel::<Captured>(1);
        let (mut capture, display) = match start_capture(self.backend.clone(), index, 0, packets_tx.clone()).await {
            Ok(started) => started,
            Err(err) => {
                deny(&mut tx, conn, DenyReason::CaptureUnavailable).await;
                return Err(err.context("iniciar captura de tela"));
            }
        };

        send_msg(
            &mut tx,
            &HostMsg::Decision(Decision::Granted {
                permissions: granted,
                display: describe(&display),
                index: index as u8,
                displays: displays as u8,
            }),
        )
        .await?;
        self.emit(HostEvent::Started { session, peer: info, permissions: granted, by_password });

        let mut sink = self.backend.open_sink(&display);
        let mut clipboard = if granted.clipboard { self.backend.open_clipboard() } else { None };
        let current = generation.clone();
        let video = async {
            let mut stream = conn.open_uni().await.context("abrir canal de vídeo")?;
            while let Some(captured) = packets.recv().await {
                match captured {
                    Captured::Frame(of, packet) if of == current.load(Ordering::Acquire) => {
                        write_frame(&mut stream, &packet).await.context("enviar quadro")?;
                    }
                    Captured::Stopped(of) if of == current.load(Ordering::Acquire) => break,
                    Captured::Frame(..) | Captured::Stopped(_) => {}
                }
            }
            anyhow::Ok("A captura de tela parou.".to_string())
        };
        let control = async {
            let mut clipboard_tick = tokio::time::interval(CLIPBOARD_POLL);
            loop {
                tokio::select! {
                    msg = inbox.recv() => match msg {
                        // A permissão é conferida aqui, no host: um visualizador
                        // modificado não ganha controle só por mandar eventos.
                        Some(ViewerMsg::Input(event)) if granted.control => sink.apply(event),
                        Some(ViewerMsg::RequestKeyframe) => capture.keyframe.store(true, Ordering::Relaxed),
                        Some(ViewerMsg::Bye) => return "O visualizador encerrou a sessão.".to_string(),
                        Some(ViewerMsg::Clipboard(text)) if text.len() <= MAX_CLIPBOARD_TEXT => {
                            if let Some(clipboard) = &mut clipboard {
                                clipboard.set(&text);
                            }
                        }
                        Some(ViewerMsg::Lock) if granted.control => self.backend.lock(),
                        Some(ViewerMsg::SelectDisplay(wanted)) => {
                            let wanted = wanted as usize;
                            if wanted >= displays || wanted == index {
                                continue;
                            }
                            // Solta o que estiver pressionado e para a captura
                            // antiga antes de abrir a nova.
                            sink.release_all();
                            capture.stop().await;
                            let next = generation.fetch_add(1, Ordering::AcqRel) + 1;
                            match start_capture(self.backend.clone(), wanted, next, packets_tx.clone()).await {
                                Ok((started, display)) => {
                                    capture = started;
                                    index = wanted;
                                    sink = self.backend.open_sink(&display);
                                    let msg = HostMsg::Display { index: index as u8, display: describe(&display) };
                                    if send_msg(&mut tx, &msg).await.is_err() {
                                        return "O canal de controle foi fechado.".to_string();
                                    }
                                }
                                Err(err) => return format!("Não foi possível mostrar o monitor {}: {err:#}", wanted + 1),
                            }
                        }
                        Some(_) => {}
                        // A leitura falha no mesmo instante em que a conexão cai;
                        // o motivo real está na conexão.
                        None => {
                            return match conn.close_reason() {
                                Some(closed) => describe_close(&closed),
                                None => "O canal de controle foi fechado.".to_string(),
                            };
                        }
                    },
                    _ = clipboard_tick.tick(), if clipboard.is_some() => {
                        let copied = clipboard.as_mut().and_then(|clipboard| clipboard.poll());
                        if let Some(text) = copied.filter(|text| text.len() <= MAX_CLIPBOARD_TEXT)
                            && send_msg(&mut tx, &HostMsg::Clipboard(text)).await.is_err()
                        {
                            return "O canal de controle foi fechado.".to_string();
                        }
                    }
                }
            }
        };
        // Cada arquivo que o visualizador envia abre um stream próprio.
        let incoming_files = async {
            while let Ok((tx, rx)) = conn.accept_bi().await {
                tokio::spawn(self.clone().receive_file(session, granted.files, tx, rx));
            }
            std::future::pending::<()>().await
        };
        if granted.files {
            self.sharing.lock().unwrap().insert(session, conn.clone());
        }
        let reason = tokio::select! {
            result = video => result.unwrap_or_else(|err| format!("{err:#}")),
            reason = control => reason,
            () = incoming_files => unreachable!("só termina com a sessão"),
            closed = conn.closed() => describe_close(&closed),
        };
        self.sharing.lock().unwrap().remove(&session);
        sink.release_all();
        Ok(reason)
    }
}

fn describe(display: &DisplayInfo) -> DisplayDesc {
    DisplayDesc { width: display.width as u16, height: display.height as u16, name: display.name.clone() }
}

struct ClearOnDrop(Arc<AtomicBool>);

impl Drop for ClearOnDrop {
    fn drop(&mut self) {
        self.0.store(false, Ordering::Release);
    }
}

async fn deny(tx: &mut quinn::SendStream, conn: &quinn::Connection, reason: DenyReason) {
    let _ = send_msg(tx, &HostMsg::Decision(Decision::Denied { reason })).await;
    let _ = tx.finish();
    // Dá tempo de a recusa chegar antes de a conexão ser fechada.
    let _ = timeout(Duration::from_secs(2), conn.closed()).await;
}

/// O que a thread de captura entrega, marcado com a geração a que pertence.
enum Captured {
    Frame(u64, Vec<u8>),
    /// A captura parou sozinha (erro), sem ninguém pedir.
    Stopped(u64),
}

/// Uma captura em andamento. Descartar o valor manda a thread parar.
struct Capture {
    stop: Arc<AtomicBool>,
    keyframe: Arc<AtomicBool>,
    done: Option<oneshot::Receiver<()>>,
}

impl Capture {
    /// Para a captura e espera a thread soltar a tela: o Windows não deixa
    /// duplicar a mesma saída duas vezes no mesmo processo.
    async fn stop(&mut self) {
        self.stop.store(true, Ordering::Relaxed);
        if let Some(done) = self.done.take() {
            let _ = timeout(Duration::from_secs(2), done).await;
        }
    }
}

impl Drop for Capture {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Relaxed);
    }
}

/// Sobe a thread de captura e codificação do monitor `display` e espera ela
/// estar pronta. O canal de capacidade 1 é a contrapressão: enquanto a rede
/// não escoa o quadro anterior, a thread fica parada no envio e as mudanças de
/// tela se acumulam num único quadro seguinte, em vez de virarem fila e atraso.
async fn start_capture(
    backend: Arc<dyn Backend>,
    display: usize,
    generation: u64,
    packets: mpsc::Sender<Captured>,
) -> Result<(Capture, DisplayInfo)> {
    let stop = Arc::new(AtomicBool::new(false));
    let keyframe = Arc::new(AtomicBool::new(false));
    let (ready_tx, ready) = oneshot::channel::<Result<DisplayInfo>>();
    let (done_tx, done) = oneshot::channel::<()>();
    let capture = Capture { stop: stop.clone(), keyframe: keyframe.clone(), done: Some(done) };
    std::thread::Builder::new()
        .name("opendesk-capture".into())
        .spawn(move || {
            // Avisa que a tela foi solta, qualquer que seja a saída.
            let _done = done_tx;
            let mut source = match backend.open_source(display) {
                Ok(source) => source,
                Err(err) => {
                    let _ = ready_tx.send(Err(err));
                    return;
                }
            };
            if ready_tx.send(Ok(source.display())).is_err() {
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
                        let _ = packets.blocking_send(Captured::Stopped(generation));
                        break;
                    }
                };
                if let Some(packet) = packet
                    && packets.blocking_send(Captured::Frame(generation, packet.encode())).is_err()
                {
                    break;
                }
            }
        })
        .context("criar thread de captura")?;
    match timeout(HANDSHAKE_TIMEOUT, ready).await {
        Ok(Ok(Ok(display))) => Ok((capture, display)),
        Ok(Ok(Err(err))) => Err(err),
        _ => Err(anyhow!("a captura de tela não iniciou")),
    }
}
