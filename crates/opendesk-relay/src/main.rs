//! `opendesk-relay`: rode numa máquina com IP público e porta UDP aberta.

use anyhow::{Context, Result};
use clap::Parser;
use opendesk_relay::{Config, serve};
use std::net::{Ipv4Addr, SocketAddr};
use std::time::Duration;

#[derive(Parser)]
#[command(
    name = "opendesk-relay",
    version,
    about = "Relay do OpenDesk: repassa sessões cifradas que não conseguem conexão direta"
)]
struct Args {
    /// Porta UDP de escuta.
    #[arg(long, default_value_t = opendesk_proto::relay::DEFAULT_PORT)]
    port: u16,
    /// Sessões simultâneas, no máximo.
    #[arg(long, default_value_t = 256)]
    max_sessions: usize,
    /// Teto de banda por sessão, em Mbit/s (soma dos dois sentidos).
    #[arg(long, default_value_t = 20.0)]
    mbps_per_session: f64,
    /// Sessões novas que um mesmo IP pode abrir por minuto.
    #[arg(long, default_value_t = 30)]
    new_sessions_per_minute: u32,
}

#[tokio::main]
async fn main() -> Result<()> {
    tracing_subscriber::fmt().init();
    let args = Args::parse();
    let addr = SocketAddr::from((Ipv4Addr::UNSPECIFIED, args.port));
    let socket = tokio::net::UdpSocket::bind(addr).await.with_context(|| format!("abrir UDP em {addr}"))?;
    let config = Config {
        max_sessions: args.max_sessions,
        bytes_per_second: (args.mbps_per_session * 125_000.0) as u64,
        idle_timeout: Duration::from_secs(30),
        new_sessions_per_minute: args.new_sessions_per_minute,
    };
    println!(
        "opendesk-relay {} em {addr} — até {} sessões, {} Mbit/s cada",
        env!("CARGO_PKG_VERSION"),
        config.max_sessions,
        args.mbps_per_session
    );
    println!("O relay só repassa pacotes cifrados; ele não consegue ler as sessões.");
    serve(socket, config).await.context("o relay parou")
}
