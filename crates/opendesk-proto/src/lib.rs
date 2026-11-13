//! Protocolo de fio do OpenDesk.
//!
//! Uma sessão usa uma conexão QUIC com dois canais:
//!
//! * **controle** — stream bidirecional aberto pelo visualizador. Carrega
//!   [`control::ViewerMsg`] num sentido e [`control::HostMsg`] no outro,
//!   serializados com postcard e prefixados pelo tamanho.
//! * **vídeo** — stream unidirecional aberto pelo host depois de a sessão ser
//!   autorizada. Carrega [`video::FramePacket`] em formato binário próprio.
//!
//! Este crate não faz E/S de rede nem toca no sistema operacional: só define
//! os bytes. Tudo aqui deve tratar a entrada como hostil.

pub mod control;
pub mod relay;
pub mod video;
pub mod wire;

/// Identificador ALPN negociado no handshake TLS do QUIC.
pub const ALPN: &[u8] = b"opendesk/1";

/// Versão do protocolo de sessão. Mude ao quebrar compatibilidade.
pub const PROTOCOL_VERSION: u16 = 2;

/// Porta UDP padrão em que um OpenDesk escuta conexões diretas.
pub const DEFAULT_PORT: u16 = 47700;
