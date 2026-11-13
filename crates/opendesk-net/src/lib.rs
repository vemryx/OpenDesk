//! Rede do OpenDesk.
//!
//! * [`identity`] — cada instalação gera um par de chaves e um certificado
//!   autoassinado. A impressão digital (SHA-256) desse certificado **é** a
//!   identidade do dispositivo; o ID numérico é só um apelido derivado dela.
//! * [`node`] — um único socket UDP/QUIC que tanto aceita quanto inicia
//!   conexões. Toda conexão é TLS 1.3 com autenticação mútua, então o tráfego é
//!   cifrado ponta a ponta entre os dois dispositivos e cada lado sabe a
//!   impressão digital do outro.
//!
//! Ainda não implementado (ver `docs/ARCHITECTURE.md`): servidor de encontro
//! por ID, perfuração de NAT e relay.

pub mod identity;
pub mod node;

pub use identity::{DeviceId, Fingerprint, Identity};
pub use node::{Node, PeerConn};
