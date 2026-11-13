//! Sessão completa host ↔ visualizador sobre QUIC em loopback, com uma tela
//! e um destino de entrada falsos no lugar do sistema operacional.

use anyhow::Result;
use opendesk_net::{Identity, Node};
use opendesk_platform::capture::{DisplayInfo, Frame};
use opendesk_proto::control::{DenyReason, InputEvent, MouseButton, Permissions};
use opendesk_session::Wake;
use opendesk_session::backend::{Backend, FrameSource, InputSink};
use opendesk_session::host::{Host, HostEvent};
use opendesk_session::viewer::{Route, Target, Viewer, ViewerEvent};
use std::net::SocketAddr;
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
}

struct FakeBackend(Arc<Shared>);
struct FakeSource {
    shared: Arc<Shared>,
    seen: u64,
    buffer: Vec<u8>,
}
struct FakeSink(Arc<Shared>);

impl Backend for FakeBackend {
    fn open_source(&self) -> Result<Box<dyn FrameSource>> {
        Ok(Box::new(FakeSource { shared: self.0.clone(), seen: 0, buffer: Vec::new() }))
    }
    fn open_sink(&self, _display: &DisplayInfo) -> Box<dyn InputSink> {
        Box::new(FakeSink(self.0.clone()))
    }
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
    let shared = Arc::new(Shared::default());
    show(&shared, 1);
    let node = node();
    let addr = node.local_addr().unwrap();
    let (host, mut events) = Host::new(node, Arc::new(FakeBackend(shared.clone())), "host-de-teste".into(), no_wake());
    tokio::spawn(host.clone().serve());
    let (ended_tx, ended) = tokio::sync::mpsc::unbounded_channel();
    tokio::spawn(async move {
        while let Some(event) = events.recv().await {
            match event {
                HostEvent::Request { peer, reply, .. } => {
                    assert_eq!(peer.name, "visualizador-de-teste");
                    let _ = reply.send(answer);
                }
                HostEvent::Started { .. } => {}
                HostEvent::Ended { reason, .. } => {
                    let _ = ended_tx.send(reason);
                }
            }
        }
    });
    (shared, host, addr, ended)
}

fn start_viewer(addr: SocketAddr, wants: Permissions) -> (Viewer, UnboundedReceiver<ViewerEvent>) {
    let (target, name) = (Target::Address(addr), "visualizador-de-teste".into());
    Viewer::connect(&tokio::runtime::Handle::current(), node(), target, name, wants, no_wake())
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
        ViewerEvent::Connected { host_name, route: Route::Address, .. } if host_name == "host-de-teste"
    ));
    assert!(viewer.rtt().is_some_and(|rtt| rtt < Duration::from_secs(1)));
    match next(&mut events).await {
        ViewerEvent::Granted { permissions, display } => {
            assert!(permissions.control);
            assert_eq!((display.width as usize, display.height as usize), (W, H));
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
async fn viewer_disconnect_ends_the_host_session() {
    let (_shared, _host, addr, mut ended) = start_host(Some(Permissions::FULL));
    let (viewer, mut events) = start_viewer(addr, Permissions::FULL);
    next(&mut events).await;
    next(&mut events).await;
    drop(viewer);
    let reason = tokio::time::timeout(Duration::from_secs(10), ended.recv()).await.unwrap().unwrap();
    assert!(reason.contains("isualizador"), "{reason}");
}
