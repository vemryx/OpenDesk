//! Sessão completa host ↔ visualizador sobre QUIC em loopback, com uma tela
//! e um destino de entrada falsos no lugar do sistema operacional.

use anyhow::Result;
use opendesk_net::{Identity, Node};
use opendesk_platform::capture::{DisplayInfo, Frame};
use opendesk_proto::control::{DenyReason, InputEvent, MouseButton, Permissions};
use opendesk_session::Wake;
use opendesk_session::auth::PasswordAccess;
use opendesk_session::backend::{Backend, Clipboard, FrameSource, InputSink};
use opendesk_session::files::{FileEvent, FileState};
use opendesk_session::host::{Host, HostEvent, Unattended};
use opendesk_session::viewer::{Route, Target, Viewer, ViewerEvent};
use std::net::SocketAddr;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;
use tokio::sync::mpsc::UnboundedReceiver;

const W: usize = 400;
const H: usize = 230;

#[derive(Default)]
struct Shared {
    screen: Mutex<Vec<u8>>,
    version: AtomicU64,
    inputs: Mutex<Vec<InputEvent>>,
    released: AtomicU64,
    /// O que "alguém copiou" no host e ainda não foi lido.
    copied: Mutex<Option<String>>,
    /// O que chegou do visualizador para a área de transferência do host.
    pasted: Mutex<Vec<String>>,
    locks: AtomicU64,
    /// Como cada sessão começou: `true` = pela senha.
    started: Mutex<Vec<bool>>,
    password_failures: AtomicU64,
    /// Sessões iniciadas, para o host poder enviar arquivos a elas.
    sessions: Mutex<Vec<u64>>,
    /// Tudo o que o host soube sobre arquivos.
    files: Mutex<Vec<FileEvent>>,
}

struct FakeBackend(Arc<Shared>);
struct FakeSource {
    shared: Arc<Shared>,
    display: usize,
    seen: u64,
    buffer: Vec<u8>,
}
struct FakeSink(Arc<Shared>);
struct FakeClipboard(Arc<Shared>);

/// Um host com dois monitores; o segundo mostra a mesma tela "em negativo".
impl Backend for FakeBackend {
    fn displays(&self) -> usize {
        2
    }
    fn open_source(&self, display: usize) -> Result<Box<dyn FrameSource>> {
        Ok(Box::new(FakeSource { shared: self.0.clone(), display, seen: 0, buffer: Vec::new() }))
    }
    fn open_sink(&self, _display: &DisplayInfo) -> Box<dyn InputSink> {
        Box::new(FakeSink(self.0.clone()))
    }
    fn open_clipboard(&self) -> Option<Box<dyn Clipboard>> {
        Some(Box::new(FakeClipboard(self.0.clone())))
    }
    fn lock(&self) {
        self.0.locks.fetch_add(1, Ordering::SeqCst);
    }
}

impl Clipboard for FakeClipboard {
    fn poll(&mut self) -> Option<String> {
        self.0.copied.lock().unwrap().take()
    }
    fn set(&mut self, text: &str) {
        self.0.pasted.lock().unwrap().push(text.to_string());
    }
}

/// O que o segundo monitor mostra quando o primeiro mostra `bgra`.
fn second_display(mut bgra: Vec<u8>) -> Vec<u8> {
    for pixel in bgra.as_chunks_mut::<4>().0 {
        pixel[0] = !pixel[0];
    }
    bgra
}

impl FrameSource for FakeSource {
    fn display(&self) -> DisplayInfo {
        DisplayInfo { name: "falsa".into(), x: 0, y: 0, width: W as u32, height: H as u32, primary: true }
    }
    fn frame(&mut self, timeout_ms: u32) -> Result<Option<Frame<'_>>> {
        let version = self.shared.version.load(Ordering::SeqCst);
        if version == self.seen {
            std::thread::sleep(Duration::from_millis(timeout_ms.min(5) as u64));
            return Ok(None);
        }
        self.seen = version;
        self.buffer = self.shared.screen.lock().unwrap().clone();
        if self.display == 1 {
            self.buffer = second_display(std::mem::take(&mut self.buffer));
        }
        Ok(Some(Frame { data: &self.buffer, width: W, height: H, stride: W * 4 }))
    }
}

impl InputSink for FakeSink {
    fn apply(&mut self, event: InputEvent) {
        self.0.inputs.lock().unwrap().push(event);
    }
    fn release_all(&mut self) {
        self.0.released.fetch_add(1, Ordering::SeqCst);
    }
}

/// Tela BGRA cujo conteúdo depende de `seed`.
fn screen(seed: u8) -> Vec<u8> {
    let mut px = vec![0u8; W * H * 4];
    for (i, p) in px.as_chunks_mut::<4>().0.iter_mut().enumerate() {
        let (x, y) = (i % W, i / W);
        p.copy_from_slice(&[(x / 40) as u8 ^ seed, (y / 30) as u8, seed.wrapping_mul(3), 255]);
    }
    px
}

fn as_rgba(bgra: &[u8]) -> Vec<u8> {
    bgra.as_chunks::<4>().0.iter().flat_map(|p| [p[2], p[1], p[0], 255]).collect()
}

fn show(shared: &Shared, seed: u8) {
    *shared.screen.lock().unwrap() = screen(seed);
    shared.version.fetch_add(1, Ordering::SeqCst);
}

fn node() -> Arc<Node> {
    Arc::new(Node::bind(Arc::new(Identity::generate().unwrap()), "127.0.0.1:0".parse().unwrap()).unwrap())
}

fn no_wake() -> Wake {
    Arc::new(|| {})
}

/// Sobe um host cuja "pessoa" responde a todo pedido com `answer`.
fn start_host(answer: Option<Permissions>) -> (Arc<Shared>, Arc<Host>, SocketAddr, UnboundedReceiver<String>) {
    start_host_with(Some(answer), None)
}

/// `answer: None` é um computador sem ninguém na frente: o pedido fica na
/// tela, sem resposta. `unattended` liga a senha de acesso não supervisionado.
fn start_host_with(
    answer: Option<Option<Permissions>>,
    unattended: Option<Unattended>,
) -> (Arc<Shared>, Arc<Host>, SocketAddr, UnboundedReceiver<String>) {
    let shared = Arc::new(Shared::default());
    show(&shared, 1);
    let node = node();
    let addr = node.local_addr().unwrap();
    let (host, mut events) = Host::new(node, Arc::new(FakeBackend(shared.clone())), "host-de-teste".into(), no_wake());
    host.set_unattended(unattended);
    tokio::spawn(host.clone().serve());
    let (ended_tx, ended) = tokio::sync::mpsc::unbounded_channel();
    let probe = shared.clone();
    tokio::spawn(async move {
        // Descartar a resposta equivale a recusar; sem ninguém, ela fica guardada.
        let mut unanswered = Vec::new();
        while let Some(event) = events.recv().await {
            match event {
                HostEvent::Request { peer, reply, .. } => {
                    assert_eq!(peer.name, "visualizador-de-teste");
                    match answer {
                        Some(answer) => drop(reply.send(answer)),
                        None => unanswered.push(reply),
                    }
                }
                HostEvent::Started { session, by_password, .. } => {
                    probe.started.lock().unwrap().push(by_password);
                    probe.sessions.lock().unwrap().push(session);
                }
                HostEvent::File { transfer, .. } => probe.files.lock().unwrap().push(transfer),
                HostEvent::PasswordFailed { .. } => {
                    probe.password_failures.fetch_add(1, Ordering::SeqCst);
                }
                HostEvent::Ended { reason, .. } => {
                    let _ = ended_tx.send(reason);
                }
            }
        }
    });
    (shared, host, addr, ended)
}

fn unattended(password: &str, permissions: Permissions) -> Option<Unattended> {
    Some(Unattended { access: PasswordAccess::create(password).unwrap(), permissions })
}

fn start_viewer(addr: SocketAddr, wants: Permissions) -> (Viewer, UnboundedReceiver<ViewerEvent>) {
    start_viewer_saving_to(addr, wants, None)
}

fn start_viewer_saving_to(
    addr: SocketAddr,
    wants: Permissions,
    files_dir: Option<PathBuf>,
) -> (Viewer, UnboundedReceiver<ViewerEvent>) {
    let (target, name) = (Target::Address(addr), "visualizador-de-teste".into());
    Viewer::connect(&tokio::runtime::Handle::current(), node(), target, name, wants, files_dir, no_wake())
}

/// Pasta temporária vazia, só deste teste.
fn temp_dir(name: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!("opendesk-sessao-{name}-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    dir
}

/// Um arquivo de conteúdo conhecido, maior que um bloco de envio.
fn sample_file(dir: &Path, name: &str, seed: u8) -> (PathBuf, Vec<u8>) {
    let content: Vec<u8> = (0..300_000u32).map(|i| (i % 251) as u8 ^ seed).collect();
    let path = dir.join(name);
    std::fs::write(&path, &content).unwrap();
    (path, content)
}

/// O próximo evento de arquivo que não seja só progresso.
async fn next_file(events: &mut UnboundedReceiver<ViewerEvent>) -> FileEvent {
    loop {
        match next(events).await {
            ViewerEvent::File(FileEvent { state: FileState::Progress(_), .. }) => {}
            ViewerEvent::File(transfer) => return transfer,
            other => panic!("esperava um evento de arquivo, veio {other:?}"),
        }
    }
}

async fn next(events: &mut UnboundedReceiver<ViewerEvent>) -> ViewerEvent {
    tokio::time::timeout(Duration::from_secs(10), events.recv()).await.expect("evento a tempo").expect("canal aberto")
}

async fn until(what: &str, mut condition: impl FnMut() -> bool) {
    for _ in 0..500 {
        if condition() {
            return;
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    panic!("tempo esgotado esperando: {what}");
}

#[tokio::test(flavor = "multi_thread")]
async fn granted_session_streams_screen_and_delivers_input() {
    let (shared, host, addr, mut ended) = start_host(Some(Permissions::FULL));
    let (viewer, mut events) = start_viewer(addr, Permissions::FULL);

    assert!(matches!(
        next(&mut events).await,
        ViewerEvent::Connected { host_name, route: Route::Address, password: false, .. } if host_name == "host-de-teste"
    ));
    assert!(viewer.rtt().is_some_and(|rtt| rtt < Duration::from_secs(1)));
    match next(&mut events).await {
        ViewerEvent::Granted { permissions, display, index, displays } => {
            assert!(permissions.control);
            assert_eq!((display.width as usize, display.height as usize), (W, H));
            assert_eq!((index, displays), (0, 2));
        }
        other => panic!("esperava Granted, veio {other:?}"),
    }

    let expect = as_rgba(&screen(1));
    until("primeiro quadro", || viewer.frame.lock().unwrap().rgba == expect).await;

    // A tela muda no host e o visualizador acompanha, pixel a pixel.
    show(&shared, 9);
    let expect = as_rgba(&screen(9));
    until("quadro atualizado", || viewer.frame.lock().unwrap().rgba == expect).await;
    assert!(viewer.frame.lock().unwrap().take_dirty().is_some());

    // Entrada chega em ordem; movimentos consecutivos podem ser colapsados,
    // mas cliques e teclas nunca.
    viewer.send_input(InputEvent::MouseMove { x: 10, y: 20 });
    viewer.send_input(InputEvent::MouseButton { button: MouseButton::Left, down: true, x: 10, y: 20 });
    viewer.send_input(InputEvent::MouseButton { button: MouseButton::Left, down: false, x: 10, y: 20 });
    viewer.send_input(InputEvent::Key { scancode: 0x1E, extended: false, down: true });
    viewer.send_input(InputEvent::Key { scancode: 0x1E, extended: false, down: false });
    until("entrada entregue", || shared.inputs.lock().unwrap().len() >= 4).await;
    let got = shared.inputs.lock().unwrap().clone();
    let tail = &got[got.len() - 4..];
    assert_eq!(tail[0], InputEvent::MouseButton { button: MouseButton::Left, down: true, x: 10, y: 20 });
    assert_eq!(tail[3], InputEvent::Key { scancode: 0x1E, extended: false, down: false });

    // O host derruba a sessão: o visualizador fica sabendo e as teclas são soltas.
    host.disconnect_all();
    loop {
        if let ViewerEvent::Closed(reason) = next(&mut events).await {
            assert!(reason.contains("remoto encerrou"), "{reason}");
            break;
        }
    }
    tokio::time::timeout(Duration::from_secs(10), ended.recv()).await.unwrap().unwrap();
    assert_eq!(shared.released.load(Ordering::SeqCst), 1);
}

#[tokio::test(flavor = "multi_thread")]
async fn view_only_session_ignores_input() {
    // O visualizador pede controle total, mas a pessoa no host só deixa ver.
    let (shared, _host, addr, _ended) = start_host(Some(Permissions::VIEW_ONLY));
    let (viewer, mut events) = start_viewer(addr, Permissions::FULL);
    next(&mut events).await;
    assert!(matches!(next(&mut events).await, ViewerEvent::Granted { permissions, .. } if !permissions.control));

    let expect = as_rgba(&screen(1));
    until("primeiro quadro", || viewer.frame.lock().unwrap().rgba == expect).await;

    viewer.send_input(InputEvent::Key { scancode: 0x1E, extended: false, down: true });
    viewer.request_keyframe();
    let before = viewer.frame.lock().unwrap().frames;
    // O quadro completo pedido depois da tecla prova que o host já processou a tecla.
    until("quadro completo", || viewer.frame.lock().unwrap().frames > before).await;
    assert!(shared.inputs.lock().unwrap().is_empty(), "host aplicou entrada sem permissão de controle");
}

#[tokio::test(flavor = "multi_thread")]
async fn never_grants_more_than_the_viewer_asked_for() {
    let (_shared, _host, addr, _ended) = start_host(Some(Permissions::FULL));
    let (_viewer, mut events) = start_viewer(addr, Permissions::VIEW_ONLY);
    next(&mut events).await;
    assert!(
        matches!(next(&mut events).await, ViewerEvent::Granted { permissions, .. } if permissions == Permissions::VIEW_ONLY)
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn rejected_request_never_shares_the_screen() {
    let (shared, _host, addr, mut ended) = start_host(None);
    let (viewer, mut events) = start_viewer(addr, Permissions::FULL);
    next(&mut events).await;
    assert!(matches!(next(&mut events).await, ViewerEvent::Denied(DenyReason::Rejected)));
    assert!(matches!(next(&mut events).await, ViewerEvent::Closed(reason) if reason.contains("recusado")));
    tokio::time::timeout(Duration::from_secs(10), ended.recv()).await.unwrap().unwrap();
    assert_eq!(viewer.frame.lock().unwrap().frames, 0);
    assert!(shared.inputs.lock().unwrap().is_empty());
}

#[tokio::test(flavor = "multi_thread")]
async fn second_viewer_is_told_the_host_is_busy_then_can_connect_later() {
    let (_shared, _host, addr, mut ended) = start_host(Some(Permissions::VIEW_ONLY));
    let (first, mut first_events) = start_viewer(addr, Permissions::VIEW_ONLY);
    next(&mut first_events).await;
    assert!(matches!(next(&mut first_events).await, ViewerEvent::Granted { .. }));

    let (_second, mut second_events) = start_viewer(addr, Permissions::VIEW_ONLY);
    next(&mut second_events).await;
    assert!(matches!(next(&mut second_events).await, ViewerEvent::Denied(DenyReason::Busy)));
    assert!(matches!(next(&mut second_events).await, ViewerEvent::Closed(reason) if reason.contains("outra sessão")));

    // Quando a primeira sessão termina, o host volta a aceitar.
    drop(first);
    tokio::time::timeout(Duration::from_secs(10), ended.recv()).await.unwrap().unwrap();
    let (_third, mut third_events) = start_viewer(addr, Permissions::VIEW_ONLY);
    next(&mut third_events).await;
    assert!(matches!(next(&mut third_events).await, ViewerEvent::Granted { .. }));
}

#[tokio::test(flavor = "multi_thread")]
async fn viewer_disconnect_ends_the_host_session() {
    let (_shared, _host, addr, mut ended) = start_host(Some(Permissions::FULL));
    let (viewer, mut events) = start_viewer(addr, Permissions::FULL);
    next(&mut events).await;
    next(&mut events).await;
    drop(viewer);
    let reason = tokio::time::timeout(Duration::from_secs(10), ended.recv()).await.unwrap().unwrap();
    assert!(reason.contains("isualizador"), "{reason}");
}

// ─────────────────────── Acesso não supervisionado ───────────────────────

const PASSWORD: &str = "correta cavalo bateria";

#[tokio::test(flavor = "multi_thread")]
async fn right_password_opens_a_session_with_nobody_at_the_host() {
    let (shared, _host, addr, _ended) = start_host_with(None, unattended(PASSWORD, Permissions::FULL));
    let (viewer, mut events) = start_viewer(addr, Permissions::FULL);
    assert!(matches!(next(&mut events).await, ViewerEvent::Connected { password: true, .. }));

    viewer.submit_password(PASSWORD.into());
    assert!(
        matches!(next(&mut events).await, ViewerEvent::Granted { permissions, .. } if permissions == Permissions::FULL)
    );
    let expect = as_rgba(&screen(1));
    until("primeiro quadro", || viewer.frame.lock().unwrap().rgba == expect).await;
    assert_eq!(*shared.started.lock().unwrap(), [true], "a sessão deve constar como liberada pela senha");
    assert_eq!(shared.password_failures.load(Ordering::SeqCst), 0);
}

#[tokio::test(flavor = "multi_thread")]
async fn password_never_grants_more_than_it_was_configured_to() {
    let (shared, _host, addr, _ended) = start_host_with(None, unattended(PASSWORD, Permissions::VIEW_ONLY));
    let (viewer, mut events) = start_viewer(addr, Permissions::FULL);
    next(&mut events).await;
    viewer.submit_password(PASSWORD.into());
    assert!(
        matches!(next(&mut events).await, ViewerEvent::Granted { permissions, .. } if permissions == Permissions::VIEW_ONLY)
    );
    let expect = as_rgba(&screen(1));
    until("primeiro quadro", || viewer.frame.lock().unwrap().rgba == expect).await;
    viewer.send_input(InputEvent::Key { scancode: 0x1E, extended: false, down: true });
    viewer.lock_remote();
    viewer.request_keyframe();
    let before = viewer.frame.lock().unwrap().frames;
    // O quadro completo pedido por último prova que o host já processou o resto.
    until("quadro completo", || viewer.frame.lock().unwrap().frames > before).await;
    assert!(shared.inputs.lock().unwrap().is_empty());
    assert_eq!(shared.locks.load(Ordering::SeqCst), 0, "sem permissão de controle não há bloqueio remoto");
}

#[tokio::test(flavor = "multi_thread")]
async fn wrong_passwords_are_refused_counted_and_end_up_locking_the_door() {
    let (shared, _host, addr, _ended) = start_host_with(None, unattended(PASSWORD, Permissions::FULL));

    // Três erros na mesma conexão: o terceiro já não deixa tentar de novo...
    let (first, mut events) = start_viewer(addr, Permissions::FULL);
    next(&mut events).await;
    for expect_retry in [true, true, false] {
        first.submit_password("palpite errado".into());
        assert!(matches!(next(&mut events).await, ViewerEvent::PasswordRejected { retry } if retry == expect_retry));
    }
    // ...nem com a senha certa.
    first.submit_password(PASSWORD.into());
    assert!(matches!(next(&mut events).await, ViewerEvent::PasswordRejected { retry: false }));
    assert_eq!(shared.password_failures.load(Ordering::SeqCst), 3);

    // Outra conexão continua a contagem: no quinto erro, somando todas, a
    // senha deixa de ser aceita por um tempo.
    let (second, mut events) = start_viewer(addr, Permissions::FULL);
    next(&mut events).await;
    second.submit_password("outro palpite".into());
    assert!(matches!(next(&mut events).await, ViewerEvent::PasswordRejected { retry: true }));
    second.submit_password("mais um".into());
    assert!(matches!(next(&mut events).await, ViewerEvent::PasswordRejected { retry: false }));
    assert_eq!(shared.password_failures.load(Ordering::SeqCst), 5);

    // Com a porta fechada, nem a senha certa numa conexão nova entra.
    let (third, mut events) = start_viewer(addr, Permissions::FULL);
    next(&mut events).await;
    third.submit_password(PASSWORD.into());
    assert!(matches!(next(&mut events).await, ViewerEvent::PasswordRejected { retry: false }));

    assert!(shared.started.lock().unwrap().is_empty(), "nenhuma sessão pode ter começado");
    assert_eq!(first.frame.lock().unwrap().frames + third.frame.lock().unwrap().frames, 0);
}

#[tokio::test(flavor = "multi_thread")]
async fn someone_at_the_host_can_still_approve_while_a_password_is_possible() {
    let (shared, _host, addr, _ended) =
        start_host_with(Some(Some(Permissions::VIEW_ONLY)), unattended(PASSWORD, Permissions::FULL));
    let (_viewer, mut events) = start_viewer(addr, Permissions::FULL);
    assert!(matches!(next(&mut events).await, ViewerEvent::Connected { password: true, .. }));
    assert!(
        matches!(next(&mut events).await, ViewerEvent::Granted { permissions, .. } if permissions == Permissions::VIEW_ONLY)
    );
    until("sessão registrada", || !shared.started.lock().unwrap().is_empty()).await;
    assert_eq!(*shared.started.lock().unwrap(), [false]);
}

// ─────────────────── Área de transferência, monitores, bloqueio ───────────────────

#[tokio::test(flavor = "multi_thread")]
async fn clipboard_text_travels_both_ways() {
    let (shared, _host, addr, _ended) = start_host(Some(Permissions::FULL));
    let (viewer, mut events) = start_viewer(addr, Permissions::FULL);
    next(&mut events).await;
    assert!(matches!(next(&mut events).await, ViewerEvent::Granted { permissions, .. } if permissions.clipboard));

    *shared.copied.lock().unwrap() = Some("copiado no host".into());
    assert!(matches!(next(&mut events).await, ViewerEvent::Clipboard(text) if text == "copiado no host"));

    viewer.send_clipboard("copiado por quem vê".into());
    until("texto colado no host", || !shared.pasted.lock().unwrap().is_empty()).await;
    assert_eq!(*shared.pasted.lock().unwrap(), ["copiado por quem vê"]);
}

#[tokio::test(flavor = "multi_thread")]
async fn clipboard_stays_put_without_the_permission() {
    let (shared, _host, addr, _ended) = start_host(Some(Permissions::VIEW_ONLY));
    let (viewer, mut events) = start_viewer(addr, Permissions::FULL);
    next(&mut events).await;
    assert!(matches!(next(&mut events).await, ViewerEvent::Granted { permissions, .. } if !permissions.clipboard));
    let expect = as_rgba(&screen(1));
    until("primeiro quadro", || viewer.frame.lock().unwrap().rgba == expect).await;

    *shared.copied.lock().unwrap() = Some("segredo do host".into());
    viewer.send_clipboard("não deveria chegar".into());
    // Dá tempo de mais de uma consulta à área de transferência do host.
    tokio::time::sleep(Duration::from_millis(1_000)).await;
    assert!(shared.pasted.lock().unwrap().is_empty());
    assert!(shared.copied.lock().unwrap().is_some(), "o host nem deve ler a área de transferência");
    assert!(events.try_recv().is_err(), "nada deve ter chegado ao visualizador");
}

#[tokio::test(flavor = "multi_thread")]
async fn switching_display_shows_the_other_monitor_and_back() {
    let (shared, _host, addr, _ended) = start_host(Some(Permissions::FULL));
    let (viewer, mut events) = start_viewer(addr, Permissions::FULL);
    next(&mut events).await;
    assert!(matches!(next(&mut events).await, ViewerEvent::Granted { index: 0, displays: 2, .. }));
    let first = as_rgba(&screen(1));
    let second = as_rgba(&second_display(screen(1)));
    until("primeiro monitor", || viewer.frame.lock().unwrap().rgba == first).await;

    viewer.select_display(7); // não existe: ignorado
    viewer.select_display(1);
    assert!(matches!(next(&mut events).await, ViewerEvent::Display { index: 1, .. }));
    until("segundo monitor", || viewer.frame.lock().unwrap().rgba == second).await;

    // A tela continua acompanhando no monitor novo, e dá para voltar.
    show(&shared, 4);
    let second = as_rgba(&second_display(screen(4)));
    until("segundo monitor atualizado", || viewer.frame.lock().unwrap().rgba == second).await;
    viewer.select_display(0);
    assert!(matches!(next(&mut events).await, ViewerEvent::Display { index: 0, .. }));
    let first = as_rgba(&screen(4));
    until("de volta ao primeiro", || viewer.frame.lock().unwrap().rgba == first).await;
}

#[tokio::test(flavor = "multi_thread")]
async fn lock_request_reaches_the_host_when_control_is_allowed() {
    let (shared, _host, addr, _ended) = start_host(Some(Permissions::FULL));
    let (viewer, mut events) = start_viewer(addr, Permissions::FULL);
    next(&mut events).await;
    next(&mut events).await;
    viewer.lock_remote();
    until("bloqueio pedido", || shared.locks.load(Ordering::SeqCst) == 1).await;
}

// ───────────────────────────── Arquivos ─────────────────────────────

#[tokio::test(flavor = "multi_thread")]
async fn viewer_sends_files_and_the_host_saves_them_without_overwriting() {
    let (source, inbox) = (temp_dir("envio-origem"), temp_dir("envio-destino"));
    let (shared, host, addr, _ended) = start_host(Some(Permissions::FULL));
    host.set_files_dir(Some(inbox.clone()));
    let (viewer, mut events) = start_viewer(addr, Permissions::FULL);
    next(&mut events).await;
    assert!(matches!(next(&mut events).await, ViewerEvent::Granted { permissions, .. } if permissions.files));

    let (path, content) = sample_file(&source, "relatório.bin", 7);
    viewer.send_file(path.clone());
    let sent = next_file(&mut events).await;
    assert_eq!((sent.incoming, sent.name.as_str(), sent.size), (false, "relatório.bin", content.len() as u64));
    assert_eq!(sent.state, FileState::Done(path.clone()));
    assert_eq!(std::fs::read(inbox.join("relatório.bin")).unwrap(), content);

    // O mesmo nome de novo não apaga o primeiro.
    std::fs::write(&path, b"segunda versao").unwrap();
    viewer.send_file(path);
    assert!(matches!(next_file(&mut events).await.state, FileState::Done(_)));
    assert_eq!(std::fs::read(inbox.join("relatório (1).bin")).unwrap(), b"segunda versao");
    assert_eq!(std::fs::read(inbox.join("relatório.bin")).unwrap(), content);

    // O host soube dos dois, e não sobrou arquivo pela metade.
    until("host avisado", || {
        shared.files.lock().unwrap().iter().filter(|f| f.incoming && matches!(f.state, FileState::Done(_))).count() == 2
    })
    .await;
    assert_eq!(std::fs::read_dir(&inbox).unwrap().count(), 2);
    for dir in [source, inbox] {
        std::fs::remove_dir_all(dir).unwrap();
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn host_files_reach_the_viewer_only_when_accepted() {
    let (source, inbox) = (temp_dir("oferta-origem"), temp_dir("oferta-destino"));
    let (shared, host, addr, _ended) = start_host(Some(Permissions::FULL));
    let (viewer, mut events) = start_viewer_saving_to(addr, Permissions::FULL, Some(inbox.clone()));
    next(&mut events).await;
    next(&mut events).await;
    until("sessão registrada", || !shared.sessions.lock().unwrap().is_empty()).await;
    let session = shared.sessions.lock().unwrap()[0];
    let runtime = tokio::runtime::Handle::current();

    // Aceito: chega inteiro.
    let (path, content) = sample_file(&source, "do-host.bin", 3);
    host.send_file(&runtime, session, path);
    let offer = next_file(&mut events).await;
    assert_eq!((offer.incoming, offer.name.as_str(), &offer.state), (true, "do-host.bin", &FileState::Offered));
    assert!(!inbox.join("do-host.bin").exists(), "nada é gravado antes de a pessoa aceitar");
    viewer.answer_file(offer.id, true);
    assert_eq!(next_file(&mut events).await.state, FileState::Done(inbox.join("do-host.bin")));
    assert_eq!(std::fs::read(inbox.join("do-host.bin")).unwrap(), content);

    // Recusado: nada é gravado, e o host fica sabendo.
    let (path, _) = sample_file(&source, "indesejado.bin", 5);
    host.send_file(&runtime, session, path);
    let offer = next_file(&mut events).await;
    viewer.answer_file(offer.id, false);
    assert_eq!(next_file(&mut events).await.state, FileState::Refused);
    until("host soube da recusa", || {
        shared.files.lock().unwrap().iter().any(|f| f.name == "indesejado.bin" && f.state == FileState::Refused)
    })
    .await;
    assert_eq!(std::fs::read_dir(&inbox).unwrap().count(), 1);
    for dir in [source, inbox] {
        std::fs::remove_dir_all(dir).unwrap();
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn files_do_not_move_without_the_permission() {
    let (source, inbox) = (temp_dir("negado-origem"), temp_dir("negado-destino"));
    // A pessoa no host só deixou ver: sem arquivos em nenhum sentido.
    let (shared, host, addr, _ended) = start_host(Some(Permissions::VIEW_ONLY));
    host.set_files_dir(Some(inbox.clone()));
    let (viewer, mut events) = start_viewer_saving_to(addr, Permissions::FULL, Some(inbox.clone()));
    next(&mut events).await;
    assert!(matches!(next(&mut events).await, ViewerEvent::Granted { permissions, .. } if !permissions.files));
    until("sessão registrada", || !shared.sessions.lock().unwrap().is_empty()).await;
    let session = shared.sessions.lock().unwrap()[0];

    let (path, _) = sample_file(&source, "barrado.bin", 9);
    viewer.send_file(path.clone());
    assert!(matches!(next_file(&mut events).await.state, FileState::Failed(_)));
    host.send_file(&tokio::runtime::Handle::current(), session, path);
    until("host recusou enviar", || {
        shared.files.lock().unwrap().iter().any(|f| !f.incoming && matches!(f.state, FileState::Failed(_)))
    })
    .await;
    assert_eq!(std::fs::read_dir(&inbox).unwrap().count(), 0);
    for dir in [source, inbox] {
        std::fs::remove_dir_all(dir).unwrap();
    }
}
