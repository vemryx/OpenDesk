//! OpenDesk em linha de comando.
//!
//! Serve para diagnóstico e para testar o caminho completo
//! captura → codec → rede → decodificação sem a interface gráfica.

use anyhow::{Context, Result, bail};
use clap::{Parser, Subcommand};
use opendesk_net::{DeviceId, Identity, Nat, Node, Presence};
use opendesk_proto::control::Permissions;
use opendesk_session::auth::PasswordAccess;
use opendesk_session::backend::SystemBackend;
use opendesk_session::host::{Host, HostEvent, Unattended};
use opendesk_session::viewer::{Lookup, Target, Viewer, ViewerEvent};
use std::io::Write;
use std::net::{IpAddr, Ipv4Addr, SocketAddr};
use std::path::{Path, PathBuf};
use std::sync::Arc;
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
        Command::Host { port, display, auto_accept_loopback, password, loopback } => {
            let identity = Arc::new(Identity::load_or_create(&data_dir)?);
            runtime()?.block_on(host(identity, &data_dir, port, display, auto_accept_loopback, password, loopback))
        }
        Command::View { target, seconds, out, control, password } => {
            let identity = Arc::new(Identity::load_or_create(&data_dir)?);
            runtime()?.block_on(view(identity, &data_dir, &target, seconds, out, control, password))
        }
    }
}

fn runtime() -> Result<tokio::runtime::Runtime> {
    tokio::runtime::Builder::new_multi_thread().enable_all().build().context("iniciar runtime")
}

fn device_name() -> String {
    std::env::var("COMPUTERNAME").or_else(|_| std::env::var("HOSTNAME")).unwrap_or_else(|_| "dispositivo".into())
}

async fn host(
    identity: Arc<Identity>,
    data_dir: &Path,
    port: u16,
    display: usize,
    auto: Option<AutoAccept>,
    password: Option<String>,
    loopback: bool,
) -> Result<()> {
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

    let (host, mut events) = Host::new(node, Arc::new(SystemBackend { display }), device_name(), Arc::new(|| {}));
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

async fn view(
    identity: Arc<Identity>,
    data_dir: &Path,
    target: &str,
    seconds: f32,
    out: Option<PathBuf>,
    control: bool,
    password: Option<String>,
) -> Result<()> {
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
        Viewer::connect(&tokio::runtime::Handle::current(), node, target, device_name(), wants, Arc::new(|| {}));

    loop {
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
            ViewerEvent::Granted { permissions, display, index, displays } => {
                let mode = if permissions.control { "com controle" } else { "somente visualização" };
                println!(
                    "Autorizado ({mode}). Tela remota: {} {}x{} (monitor {} de {displays})",
                    display.name,
                    display.width,
                    display.height,
                    index + 1
                );
                break;
            }
            ViewerEvent::PasswordRejected { retry } => {
                bail!(
                    "A senha não foi aceita{}.",
                    if retry { "" } else { " e o host bloqueou novas tentativas por enquanto" }
                )
            }
            ViewerEvent::Denied(_) | ViewerEvent::Clipboard(_) | ViewerEvent::Display { .. } => {}
            ViewerEvent::Closed(reason) => bail!("{reason}"),
        }
    }

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
    tokio::time::sleep(Duration::from_secs_f32(seconds.max(0.0))).await;

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
