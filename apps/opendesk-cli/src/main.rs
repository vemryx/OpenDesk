//! OpenDesk em linha de comando.
//!
//! Serve para diagnóstico e para testar o caminho completo
//! captura → codec → rede → decodificação sem a interface gráfica.

mod netsim;
mod synthetic;

use anyhow::{Context, Result, bail};
use clap::{Parser, Subcommand};
use opendesk_net::{DeviceId, Identity, Nat, Node, Presence};
use opendesk_proto::control::{InputEvent, MouseButton, Permissions};
use opendesk_session::auth::PasswordAccess;
use opendesk_session::backend::{Backend, SystemBackend};
use opendesk_session::files::{FileEvent, FileState};
use opendesk_session::host::{Host, HostEvent, Unattended};
use opendesk_session::viewer::{InputSender, Lookup, SharedFrame, Target, Viewer, ViewerEvent};
use std::io::Write;
use std::net::{IpAddr, Ipv4Addr, SocketAddr};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

#[derive(Parser)]
#[command(name = "opendesk-cli", version, about = "OpenDesk: acesso remoto P2P de código aberto")]
struct Cli {
    /// Pasta de dados (identidade do dispositivo). Padrão: pasta do usuário.
    #[arg(long, global = true, env = "OPENDESK_DATA_DIR")]
    data_dir: Option<PathBuf>,
    #[command(subcommand)]
    command: Command,
}

#[derive(Subcommand)]
enum Command {
    /// Mostra o ID e a impressão digital deste dispositivo.
    Id,
    /// Lista os monitores que podem ser compartilhados.
    Displays,
    /// Compartilha a tela deste computador.
    Host {
        #[arg(long, default_value_t = opendesk_proto::DEFAULT_PORT)]
        port: u16,
        /// Monitor a compartilhar (índice de `displays`).
        #[arg(long, default_value_t = 0)]
        display: usize,
        /// SÓ PARA TESTES: aceita pedidos sem perguntar. Restringe a escuta a
        /// 127.0.0.1, então nada fora deste computador consegue conectar.
        #[arg(long, value_enum)]
        auto_accept_loopback: Option<AutoAccept>,
        /// Liga o acesso não supervisionado com esta senha (mínimo de 8
        /// caracteres): quem souber a senha entra sem ninguém aprovar.
        #[arg(long, env = "OPENDESK_PASSWORD")]
        password: Option<String>,
        /// Escuta só em 127.0.0.1: nada fora deste computador conecta.
        #[arg(long)]
        loopback: bool,
        /// Pasta onde gravar os arquivos enviados pelos visualizadores. Sem
        /// ela, o host não recebe arquivos.
        #[arg(long)]
        files_dir: Option<PathBuf>,
        /// Em vez da tela de verdade, compartilha uma tela de mentira com
        /// este conteúdo, feita para medir o atraso com `view --measure`.
        #[arg(long, value_enum)]
        synthetic: Option<synthetic::Scene>,
        /// Tamanho da tela de mentira.
        #[arg(long, default_value = "1920x1080", value_name = "LxA")]
        size: String,
    },
    /// Conecta a um host e mede a recepção; opcionalmente salva a tela em PNG.
    View {
        /// ID do host (602 758 973) ou endereço (192.168.0.10:47700).
        target: String,
        /// Por quantos segundos receber.
        #[arg(long, default_value_t = 3.0)]
        seconds: f32,
        /// Salva o último estado da tela remota neste arquivo PNG.
        #[arg(long)]
        out: Option<PathBuf>,
        /// Pede também controle de mouse e teclado.
        #[arg(long)]
        control: bool,
        /// Entra com a senha de acesso não supervisionado do host.
        #[arg(long, env = "OPENDESK_PASSWORD")]
        password: Option<String>,
        /// Envia este arquivo ao host assim que a sessão começar (exige
        /// `--control` e a permissão de arquivos).
        #[arg(long)]
        send: Option<PathBuf>,
        /// Clica com o botão esquerdo neste ponto da tela remota, em pixels
        /// (`1900,1040`), depois do primeiro quadro. Exige `--control`. Serve
        /// para conferir se a entrada chega a uma janela específica.
        #[arg(long, value_name = "X,Y")]
        click: Option<String>,
        /// Pressiona estas teclas no computador remoto, em ordem e separadas
        /// por vírgula: `enter`, `esc`, `tab`, `espaco`, `esquerda`, `direita`,
        /// `cima`, `baixo`. Exige `--control`. Vem depois do clique, se houver.
        #[arg(long, value_name = "TECLAS")]
        press: Option<String>,
        /// Espera estes segundos, já recebendo a tela, antes do clique e das
        /// teclas. Serve para alcançar algo que só aparece no meio da sessão.
        #[arg(long, default_value_t = 0.0)]
        after: f32,
        /// Mede o atraso contra um host `--synthetic`: clica este número de
        /// vezes na sonda e cronometra até a cor mudar na tela recebida, e lê
        /// em cada quadro a hora em que ele foi produzido. Exige `--control`.
        #[arg(long, value_name = "CLIQUES")]
        measure: Option<usize>,
    },
    /// Simula uma conexão ruim entre o visualizador e um host deste
    /// computador: o visualizador conecta em `--listen` e os pacotes chegam
    /// ao host com banda limitada, atraso e perda.
    Netsim {
        /// Porta em que o visualizador conecta (em 127.0.0.1).
        #[arg(long)]
        listen: u16,
        /// Endereço do host de verdade.
        #[arg(long)]
        to: SocketAddr,
        /// Banda, em megabits por segundo.
        #[arg(long, default_value_t = 10.0)]
        mbit: f64,
        /// Atraso de cada sentido, em milissegundos.
        #[arg(long, default_value_t = 15.0)]
        delay_ms: f64,
        /// Perda de pacotes, em porcentagem.
        #[arg(long, default_value_t = 0.0)]
        loss: f64,
        /// Fila do roteador, em milissegundos de banda.
        #[arg(long, default_value_t = 150.0)]
        queue_ms: f64,
    },
}

#[derive(Clone, Copy, clap::ValueEnum)]
enum AutoAccept {
    /// Só visualização.
    View,
    /// Visualização e controle.
    Control,
}

fn main() -> Result<()> {
    tracing_subscriber::fmt().with_writer(std::io::stderr).init();
    opendesk_platform::init_process();
    let cli = Cli::parse();
    let data_dir = match cli.data_dir {
        Some(dir) => dir,
        None => directories::ProjectDirs::from("org", "OpenDesk", "OpenDesk")
            .context("não foi possível localizar a pasta de dados do usuário")?
            .data_dir()
            .to_path_buf(),
    };

    match cli.command {
        Command::Id => {
            let identity = Identity::load_or_create(&data_dir)?;
            println!("ID:                {}", identity.device_id());
            println!("Impressão digital: {}", identity.fingerprint().hex());
            println!("Dados em:          {}", data_dir.display());
            Ok(())
        }
        Command::Displays => {
            for (i, d) in opendesk_platform::capture::displays()?.iter().enumerate() {
                let primary = if d.primary { " (principal)" } else { "" };
                println!("{i}: {} {}x{} em ({}, {}){primary}", d.name, d.width, d.height, d.x, d.y);
            }
            Ok(())
        }
        Command::Host { port, display, auto_accept_loopback, password, loopback, files_dir, synthetic, size } => {
            let identity = Arc::new(Identity::load_or_create(&data_dir)?);
            let synthetic = synthetic
                .map(|scene| {
                    let (width, height) = size.split_once('x').context("--size espera LxA, como 1920x1080")?;
                    anyhow::Ok((scene, width.trim().parse()?, height.trim().parse()?))
                })
                .transpose()
                .context("tamanho de --size inválido")?;
            let options =
                HostOptions { port, display, auto: auto_accept_loopback, password, loopback, files_dir, synthetic };
            runtime()?.block_on(host(identity, &data_dir, options))
        }
        Command::Netsim { listen, to, mbit, delay_ms, loss, queue_ms } => {
            netsim::run(listen, to, netsim::Link { mbit, delay_ms, loss_pct: loss, queue_ms })
        }
        Command::View { target, seconds, out, control, password, send, click, press, after, measure } => {
            let identity = Arc::new(Identity::load_or_create(&data_dir)?);
            let click = click
                .map(|point| {
                    let (x, y) = point.split_once(',').context("--click espera X,Y, como 1900,1040")?;
                    anyhow::Ok((x.trim().parse()?, y.trim().parse()?))
                })
                .transpose()
                .context("ponto de --click inválido")?;
            let press = press.as_deref().map(parse_keys).transpose()?.unwrap_or_default();
            let options = ViewOptions { seconds, out, control, password, send, click, press, after, measure };
            runtime()?.block_on(view(identity, &data_dir, &target, options))
        }
    }
}

/// Mediana, 90% e pior caso de uma lista de atrasos em milissegundos.
fn summary(samples: &[f64]) -> String {
    if samples.is_empty() {
        return "sem amostras".into();
    }
    let mut sorted = samples.to_vec();
    sorted.sort_by(f64::total_cmp);
    let at = |fraction: f64| sorted[((sorted.len() - 1) as f64 * fraction).round() as usize];
    format!(
        "mediana {:.0} ms, 90% abaixo de {:.0} ms, pior {:.0} ms ({} amostras)",
        at(0.5),
        at(0.9),
        sorted[sorted.len() - 1],
        sorted.len()
    )
}

/// Mede, contra um host `--synthetic`, o atraso que a pessoa sente (do clique
/// até a sonda mudar de cor na tela recebida) e o atraso da imagem (a hora
/// carimbada em cada quadro contra o relógio ao recebê-lo). Devolve as duas
/// listas, em milissegundos.
fn measure_lag(
    frame: &Mutex<SharedFrame>,
    input: &InputSender,
    clicks: usize,
    duration: Duration,
) -> Result<(Vec<f64>, Vec<f64>)> {
    use synthetic::{PROBE_AT, PROBE_OFF, PROBE_ON, PROBE_SIDE, STAMP_BITS, STAMP_BLOCK, clock_ms};
    /// Um clique sem resposta neste tempo conta como este tempo.
    const GIVE_UP: Duration = Duration::from_secs(20);
    let center = (PROBE_AT.0 + PROBE_SIDE / 2, PROBE_AT.1 + PROBE_SIDE / 2);
    let distance = |a: [u8; 3], b: [u8; 3]| (0..3).map(|i| (a[i] as i32 - b[i] as i32).abs()).sum::<i32>();
    let pixel = |frame: &SharedFrame, x: usize, y: usize| -> Option<[u8; 3]> {
        let i = (y * frame.width + x) * 4;
        frame.rgba.get(i..i + 3).map(|p| [p[0], p[1], p[2]])
    };

    let (width, height) = {
        let frame = frame.lock().unwrap();
        let color = pixel(&frame, center.0, center.1).context("a tela remota é menor que a sonda")?;
        if distance(color, PROBE_OFF).min(distance(color, PROBE_ON)) > 150 {
            bail!("não achei a sonda na tela remota: o host precisa estar com --synthetic");
        }
        (frame.width, frame.height)
    };
    let normalize = |value: usize, size: usize| (value as u64 * 65535 / (size.max(2) as u64 - 1)) as u16;
    let (nx, ny) = (normalize(center.0, width), normalize(center.1, height));

    let started = Instant::now();
    let (mut click_ms, mut image_ms) = (Vec::new(), Vec::new());
    let mut seen_frames = 0;
    let mut last_stamp = None;
    let mut waiting: Option<(Instant, [u8; 3])> = None;
    let mut next_click = started + Duration::from_millis(500);
    let mut random = 0x2545_F491u64;
    loop {
        let now = Instant::now();
        let done = click_ms.len() >= clicks && waiting.is_none();
        if done && now - started >= duration {
            break;
        }
        let mut click = false;
        {
            let frame = frame.lock().unwrap();
            if frame.frames != seen_frames {
                seen_frames = frame.frames;
                let bit = |index: usize| pixel(&frame, index * STAMP_BLOCK + STAMP_BLOCK / 2, STAMP_BLOCK / 2);
                let stamp = (0..STAMP_BITS)
                    .try_fold(0u32, |stamp, index| Some(stamp | u32::from(bit(index)?[0] > 127) << index));
                // Um passo de refinamento melhora a imagem que já estava lá:
                // o carimbo é o mesmo, e não conta como imagem nova.
                let fresh = stamp.filter(|&stamp| last_stamp.replace(stamp) != Some(stamp));
                let lag = fresh.map(|stamp| clock_ms().wrapping_sub(stamp));
                // Um carimbo ilegível (ou de outro relógio) dá um número absurdo.
                if let Some(lag) = lag.filter(|&lag| lag < 120_000) {
                    image_ms.push(lag as f64);
                }
            }
            let color = pixel(&frame, center.0, center.1).unwrap_or(PROBE_OFF);
            match waiting {
                Some((sent, before)) if distance(color, before) > 150 => {
                    click_ms.push(sent.elapsed().as_secs_f64() * 1000.0);
                    waiting = None;
                    // Intervalo irregular, para não entrar em fase com a animação.
                    random ^= random << 13;
                    random ^= random >> 7;
                    random ^= random << 17;
                    next_click = now + Duration::from_millis(250 + random % 250);
                }
                Some((sent, _)) if sent.elapsed() > GIVE_UP => {
                    click_ms.push(GIVE_UP.as_secs_f64() * 1000.0);
                    waiting = None;
                    next_click = now;
                }
                None if click_ms.len() < clicks && now >= next_click => {
                    waiting = Some((now, color));
                    click = true;
                }
                _ => {}
            }
        }
        if click {
            input.send(InputEvent::MouseButton { button: MouseButton::Left, down: true, x: nx, y: ny });
            input.send(InputEvent::MouseButton { button: MouseButton::Left, down: false, x: nx, y: ny });
        }
        std::thread::sleep(Duration::from_micros(300));
    }
    Ok((click_ms, image_ms))
}

/// Nomes de teclas de `--press` para o scancode físico (conjunto 1) e o
/// prefixo estendido, que é o que a sessão transporta.
fn parse_keys(list: &str) -> Result<Vec<(u16, bool)>> {
    list.split(',')
        .map(|name| {
            Ok(match name.trim().to_lowercase().as_str() {
                "esc" => (0x01, false),
                "tab" => (0x0F, false),
                "enter" => (0x1C, false),
                "espaco" | "espaço" => (0x39, false),
                "esquerda" => (0x4B, true),
                "direita" => (0x4D, true),
                "cima" => (0x48, true),
                "baixo" => (0x50, true),
                other => bail!("tecla desconhecida em --press: \"{other}\""),
            })
        })
        .collect()
}

/// Uma linha por novidade de um arquivo (o progresso não é impresso).
fn print_file(transfer: &FileEvent) {
    let direction = if transfer.incoming { "Recebido" } else { "Enviado" };
    match &transfer.state {
        FileState::Done(path) => {
            println!("{direction}: {} ({} bytes) em {}", transfer.name, transfer.size, path.display());
        }
        FileState::Refused => println!("Arquivo recusado: {}", transfer.name),
        FileState::Failed(why) => println!("Arquivo {} falhou: {why}", transfer.name),
        FileState::Offered | FileState::Progress(_) => {}
    }
}

fn runtime() -> Result<tokio::runtime::Runtime> {
    tokio::runtime::Builder::new_multi_thread().enable_all().build().context("iniciar runtime")
}

fn device_name() -> String {
    std::env::var("COMPUTERNAME").or_else(|_| std::env::var("HOSTNAME")).unwrap_or_else(|_| "dispositivo".into())
}

struct HostOptions {
    port: u16,
    display: usize,
    auto: Option<AutoAccept>,
    password: Option<String>,
    loopback: bool,
    files_dir: Option<PathBuf>,
    /// Tela de mentira: (cena, largura, altura).
    synthetic: Option<(synthetic::Scene, usize, usize)>,
}

async fn host(identity: Arc<Identity>, data_dir: &Path, options: HostOptions) -> Result<()> {
    let HostOptions { port, display, auto, password, loopback, files_dir, synthetic } = options;
    let local_only = auto.is_some() || loopback;
    let ip = if local_only { IpAddr::V4(Ipv4Addr::LOCALHOST) } else { IpAddr::V4(Ipv4Addr::UNSPECIFIED) };
    let node = Arc::new(Node::bind(identity.clone(), SocketAddr::new(ip, port))?);
    println!("OpenDesk {}", env!("CARGO_PKG_VERSION"));
    println!("Escutando em {}", node.local_addr()?);
    if auto.is_some() {
        println!("MODO DE TESTE: aceitando automaticamente, somente a partir deste computador.");
    }

    // Com um servidor de encontro configurado, este computador também pode
    // ser achado pelo ID.
    match Lookup::from_env(data_dir)? {
        Some(lookup) => {
            let node = node.clone();
            tokio::spawn(async move {
                let servers = lookup.stun_servers.clone();
                lookup.rendezvous.serve(node, servers, lookup.relay.clone(), print_presence).await;
            });
        }
        None => println!("Sem servidor de encontro: só é possível conectar pelo endereço acima."),
    }

    let backend: Arc<dyn Backend> = match synthetic {
        Some((scene, width, height)) => {
            println!("TELA DE MENTIRA: {scene:?}, {width}x{height}. Meça com `view --control --measure 20`.");
            Arc::new(synthetic::SyntheticBackend::new(scene, width, height))
        }
        None => Arc::new(SystemBackend { display }),
    };
    let (host, mut events) = Host::new(node, backend, device_name(), Arc::new(|| {}));
    if let Some(dir) = &files_dir {
        println!("Arquivos recebidos vão para {}", dir.display());
    }
    host.set_files_dir(files_dir);
    // Com senha, este é um host sem ninguém na frente: os pedidos ficam
    // aguardando (até expirar) em vez de perguntar no terminal.
    let headless = password.is_some();
    let mut unanswered = Vec::new();
    if let Some(password) = password {
        let access = PasswordAccess::create(&password)?;
        host.set_unattended(Some(Unattended { access, permissions: Permissions::FULL }));
        println!("Acesso não supervisionado LIGADO: quem souber a senha entra sem aprovação.");
    }
    tokio::spawn(host.clone().serve());

    while let Some(event) = events.recv().await {
        match event {
            HostEvent::Request { peer, reply, .. } => {
                println!(
                    "\nPedido de conexão de \"{}\" — ID {} ({}) em {}",
                    peer.name,
                    peer.fingerprint.device_id(),
                    peer.fingerprint.short(),
                    peer.addr
                );
                let answer = match auto {
                    Some(AutoAccept::View) => Some(Permissions::VIEW_ONLY),
                    Some(AutoAccept::Control) => Some(Permissions::FULL),
                    None if headless => {
                        println!("Aguardando a senha de acesso (ninguém para aprovar).");
                        unanswered.push(reply);
                        continue;
                    }
                    None => tokio::task::spawn_blocking(move || ask(peer.wants)).await??,
                };
                let _ = reply.send(answer);
            }
            HostEvent::Started { peer, permissions, by_password, .. } => {
                let mode = if permissions.control { "com controle" } else { "somente visualização" };
                let how = if by_password { ", pela senha" } else { "" };
                println!("Sessão iniciada com \"{}\" ({mode}{how}). Ctrl+C encerra.", peer.name);
            }
            HostEvent::PasswordFailed { peer, .. } => {
                println!("Senha de acesso errada vinda de \"{}\" ({}).", peer.name, peer.fingerprint.short());
            }
            HostEvent::File { transfer, .. } => print_file(&transfer),
            HostEvent::Ended { reason, .. } => println!("Sessão encerrada: {reason}"),
        }
    }
    Ok(())
}

fn print_presence(presence: Presence) {
    match presence {
        Presence::Connecting => println!("Conectando ao servidor de encontro…"),
        Presence::Online { id, nat } => {
            let note = match nat {
                Nat::Symmetric => " (NAT simétrico: conexões pela internet podem falhar sem relay)",
                Nat::Stable | Nat::Unknown => "",
            };
            println!("Online. ID deste computador: {id}{note}");
        }
        Presence::Offline(reason) => println!("Fora do servidor de encontro: {reason}"),
    }
}

fn ask(wants: Permissions) -> Result<Option<Permissions>> {
    let options =
        if wants.control { "[v] só ver  [c] ver e controlar  [n] recusar" } else { "[v] permitir ver  [n] recusar" };
    print!("{options} > ");
    std::io::stdout().flush()?;
    let mut line = String::new();
    std::io::stdin().read_line(&mut line)?;
    Ok(match line.trim().to_lowercase().as_str() {
        "v" => Some(Permissions::VIEW_ONLY),
        "c" if wants.control => Some(Permissions::FULL),
        _ => None,
    })
}

struct ViewOptions {
    seconds: f32,
    out: Option<PathBuf>,
    control: bool,
    password: Option<String>,
    send: Option<PathBuf>,
    click: Option<(u32, u32)>,
    /// Teclas a pressionar: (scancode, estendida).
    press: Vec<(u16, bool)>,
    after: f32,
    measure: Option<usize>,
}

async fn view(identity: Arc<Identity>, data_dir: &Path, target: &str, options: ViewOptions) -> Result<()> {
    let ViewOptions { seconds, out, control, password, send, click, press, after, measure } = options;
    let target = if let Some(id) = DeviceId::parse(target) {
        let lookup = Lookup::from_env(data_dir)?
            .context("conectar por ID exige um servidor de encontro (defina OPENDESK_RENDEZVOUS)")?;
        println!("Procurando {id} em {}…", lookup.rendezvous.url());
        Target::Id { id, lookup }
    } else {
        Target::Address(target.parse().with_context(|| format!("\"{target}\" não é um ID nem um endereço ip:porta"))?)
    };
    let node = Arc::new(Node::bind(identity, SocketAddr::new(IpAddr::V4(Ipv4Addr::UNSPECIFIED), 0))?);
    let wants = if control { Permissions::FULL } else { Permissions::VIEW_ONLY };
    let (viewer, mut events) =
        Viewer::connect(&tokio::runtime::Handle::current(), node, target, device_name(), wants, None, Arc::new(|| {}));

    let screen = loop {
        match events.recv().await.context("a sessão terminou sem aviso")? {
            ViewerEvent::Connected { host_name, fingerprint, route, password: accepts_password } => {
                println!("Conectado a \"{host_name}\" ({}). Rota: {}", fingerprint.short(), route.label());
                match &password {
                    Some(password) if accepts_password => {
                        println!("Entrando com a senha…");
                        viewer.submit_password(password.clone());
                    }
                    Some(_) => println!("O host não aceita senha. Aguardando autorização no computador remoto…"),
                    None => println!("Aguardando autorização no computador remoto…"),
                }
            }
            ViewerEvent::Granted { permissions, display, index, displays, full_control } => {
                let mode = match (permissions.control, full_control) {
                    (true, true) => "com controle total",
                    (true, false) => "com controle, menos janelas de administrador e o aviso de permissão do Windows",
                    (false, _) => "somente visualização",
                };
                println!(
                    "Autorizado ({mode}). Tela remota: {} {}x{} (monitor {} de {displays})",
                    display.name,
                    display.width,
                    display.height,
                    index + 1
                );
                break (display.width as u32, display.height as u32, permissions.control);
            }
            ViewerEvent::PasswordRejected { retry } => {
                bail!(
                    "A senha não foi aceita{}.",
                    if retry { "" } else { " e o host bloqueou novas tentativas por enquanto" }
                )
            }
            ViewerEvent::Denied(_) | ViewerEvent::Clipboard(_) | ViewerEvent::Display { .. } | ViewerEvent::File(_) => {
            }
            ViewerEvent::Closed(reason) => bail!("{reason}"),
        }
    };

    let started = Instant::now();
    let first_frame = loop {
        if viewer.frame.lock().unwrap().frames > 0 {
            break started.elapsed();
        }
        if started.elapsed() > Duration::from_secs(10) {
            bail!("nenhum quadro recebido em 10 s");
        }
        tokio::time::sleep(Duration::from_millis(2)).await;
    };
    let (width, height, may_control) = screen;
    if (click.is_some() || !press.is_empty() || measure.is_some()) && !may_control {
        bail!("clique, teclas e medição exigem controle: use --control e confira a permissão no host");
    }
    tokio::time::sleep(Duration::from_secs_f32(after.max(0.0))).await;
    let input = viewer.input_sender();
    if let Some((x, y)) = click {
        if x >= width || y >= height {
            bail!("o ponto {x},{y} está fora da tela remota ({width}x{height})");
        }
        // Posição normalizada sobre a tela remota, como a interface envia.
        let normalize = |value: u32, size: u32| (value as u64 * 65535 / (size.max(2) as u64 - 1)) as u16;
        let (nx, ny) = (normalize(x, width), normalize(y, height));
        input.send(InputEvent::MouseMove { x: nx, y: ny });
        tokio::time::sleep(Duration::from_millis(150)).await;
        input.send(InputEvent::MouseButton { button: MouseButton::Left, down: true, x: nx, y: ny });
        tokio::time::sleep(Duration::from_millis(60)).await;
        input.send(InputEvent::MouseButton { button: MouseButton::Left, down: false, x: nx, y: ny });
        println!("Clique enviado em {x},{y}");
    }
    for &(scancode, extended) in &press {
        input.send(InputEvent::Key { scancode, extended, down: true });
        tokio::time::sleep(Duration::from_millis(60)).await;
        input.send(InputEvent::Key { scancode, extended, down: false });
        tokio::time::sleep(Duration::from_millis(200)).await;
    }
    if !press.is_empty() {
        println!("{} tecla(s) enviada(s)", press.len());
    }
    // Com um arquivo a enviar, espera a transferência terminar.
    if let Some(path) = send {
        viewer.send_file(path);
        loop {
            match events.recv().await.context("a sessão terminou durante o envio")? {
                ViewerEvent::File(transfer) => {
                    print_file(&transfer);
                    match transfer.state {
                        FileState::Done(_) => break,
                        FileState::Refused => bail!("o host recusou o arquivo"),
                        FileState::Failed(why) => bail!("o envio falhou: {why}"),
                        FileState::Offered | FileState::Progress(_) => {}
                    }
                }
                ViewerEvent::Closed(reason) => bail!("{reason}"),
                _ => {}
            }
        }
    }
    let measured = match measure {
        Some(clicks) => {
            let (frame, input) = (viewer.frame.clone(), viewer.input_sender());
            let duration = Duration::from_secs_f32(seconds.max(0.0));
            Some(tokio::task::spawn_blocking(move || measure_lag(&frame, &input, clicks, duration)).await??)
        }
        None => {
            tokio::time::sleep(Duration::from_secs_f32(seconds.max(0.0))).await;
            None
        }
    };

    {
        // A trava do quadro não pode atravessar um `await`.
        let frame = viewer.frame.lock().unwrap();
        let elapsed = started.elapsed().as_secs_f64();
        println!("Primeiro quadro em {:.0} ms", first_frame.as_secs_f64() * 1000.0);
        if let Some(rtt) = viewer.rtt() {
            println!("Latência (ida e volta): {:.1} ms", rtt.as_secs_f64() * 1000.0);
        }
        println!(
            "{} quadros em {:.1} s ({:.1} qps), {:.2} MB recebidos ({:.2} Mbit/s)",
            frame.frames,
            elapsed,
            frame.frames as f64 / elapsed,
            frame.bytes as f64 / 1e6,
            frame.bytes as f64 * 8.0 / 1e6 / elapsed
        );
        if let Some((clicks, images)) = &measured {
            println!("Atraso do clique até a tela: {}", summary(clicks));
            println!("Atraso da imagem:            {}", summary(images));
        }
        if let Some(path) = out {
            let file = std::fs::File::create(&path).with_context(|| format!("criar {}", path.display()))?;
            let mut encoder = png::Encoder::new(std::io::BufWriter::new(file), frame.width as u32, frame.height as u32);
            encoder.set_color(png::ColorType::Rgba);
            encoder.set_depth(png::BitDepth::Eight);
            encoder.set_compression(png::Compression::Fast);
            encoder.write_header()?.write_image_data(&frame.rgba)?;
            println!("Tela remota salva em {}", path.display());
        }
    }
    // Despede-se do host em vez de simplesmente sumir, para ele liberar a
    // sessão na hora.
    viewer.disconnect();
    let _ = tokio::time::timeout(Duration::from_secs(1), async {
        while let Some(event) = events.recv().await {
            if matches!(event, ViewerEvent::Closed(_)) {
                break;
            }
        }
    })
    .await;
    tokio::time::sleep(Duration::from_millis(100)).await;
    Ok(())
}
