//! Rede do OpenDesk.
//!
//! * [`identity`] — cada instalação gera um par de chaves e um certificado
//!   autoassinado. A impressão digital (SHA-256) desse certificado **é** a
//!   identidade do dispositivo; o ID numérico é só um apelido derivado dela.
//! * [`node`] — um único socket UDP/QUIC que tanto aceita quanto inicia
//!   conexões. Toda conexão é TLS 1.3 com autenticação mútua, então o tráfego é
//!   cifrado ponta a ponta entre os dois dispositivos e cada lado sabe a
//!   impressão digital do outro. O mesmo socket descobre o endereço público
//!   (STUN) e perfura o NAT.
//! * [`rendezvous`] — cliente do servidor de encontro, que permite conectar
//!   por ID em vez de por endereço.
//! * [`known`] — impressões digitais fixadas dos hosts já acessados.
//!
//! Ainda não implementado (ver `docs/ARCHITECTURE.md`): relay para quando a
//! conexão direta não é possível.

pub mod identity;
pub mod known;
pub mod node;
pub mod rendezvous;
mod socket;
mod stun;

pub use identity::{DeviceId, Fingerprint, Identity};
pub use known::KnownHosts;
pub use node::{Nat, Node, PeerConn, PublicAddress};
pub use rendezvous::{Presence, Rendezvous};

use std::net::{IpAddr, Ipv4Addr, UdpSocket};

/// Servidores STUN públicos usados para descobrir o endereço público. São
/// dois, de operadores diferentes, para que a comparação entre as respostas
/// revele um NAT simétrico. Eles veem o IP público de quem pergunta, como
/// qualquer site veria, e nada mais.
pub const DEFAULT_STUN_SERVERS: [&str; 2] = ["stun.cloudflare.com:3478", "stun.l.google.com:19302"];

/// Servidor de encontro usado quando nenhum outro é configurado. Ainda não há
/// um servidor público do projeto; até lá, a conexão por ID exige apontar para
/// um servidor próprio com a variável `OPENDESK_RENDEZVOUS`.
pub const DEFAULT_RENDEZVOUS: Option<&str> = None;

/// Endereço do servidor de encontro a usar: o da variável de ambiente
/// `OPENDESK_RENDEZVOUS`, senão o padrão. `OPENDESK_RENDEZVOUS=off` desliga.
pub fn rendezvous_url() -> Option<String> {
    match std::env::var("OPENDESK_RENDEZVOUS") {
        Ok(value) if value.trim().eq_ignore_ascii_case("off") => None,
        Ok(value) if !value.trim().is_empty() => Some(value.trim().to_string()),
        _ => DEFAULT_RENDEZVOUS.map(str::to_string),
    }
}

/// IP local pelo qual este computador sai para a rede. Um socket UDP
/// "conectado" não envia nenhum pacote; só faz o sistema escolher a rota.
pub fn local_ip() -> Option<IpAddr> {
    let socket = UdpSocket::bind((Ipv4Addr::UNSPECIFIED, 0)).ok()?;
    socket.connect((Ipv4Addr::new(192, 0, 2, 1), 9)).ok()?;
    let ip = socket.local_addr().ok()?.ip();
    (!ip.is_unspecified()).then_some(ip)
}
