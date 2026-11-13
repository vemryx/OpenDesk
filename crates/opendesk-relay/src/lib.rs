//! Relay do OpenDesk.
//!
//! Quando os roteadores dos dois lados não deixam a conexão direta acontecer,
//! os dois dispositivos mandam seus pacotes para um relay, que os repassa. A
//! sessão continua sendo QUIC com TLS de ponta a ponta **entre os
//! dispositivos**: o relay vê só bytes cifrados, endereços e volumes. Ele não
//! tem chave nenhuma e não consegue ler nem alterar a sessão.
//!
//! Qualquer pessoa pode rodar um relay. Os limites abaixo existem para que um
//! relay aberto não vire banda grátis sem controle.

use opendesk_proto::relay::{self, Token};
use std::collections::HashMap;
use std::io;
use std::net::{IpAddr, SocketAddr};
use std::time::{Duration, Instant};
use tokio::net::UdpSocket;

#[derive(Debug, Clone)]
pub struct Config {
    /// Sessões simultâneas, no máximo.
    pub max_sessions: usize,
    /// Teto de banda por sessão, somando os dois sentidos (bytes por segundo).
    pub bytes_per_second: u64,
    /// Uma sessão sem tráfego por este tempo é esquecida.
    pub idle_timeout: Duration,
    /// Sessões novas que um mesmo IP pode abrir por minuto.
    pub new_sessions_per_minute: u32,
}

impl Default for Config {
    fn default() -> Self {
        Self {
            max_sessions: 256,
            bytes_per_second: 2_500_000, // 20 Mbit/s
            idle_timeout: Duration::from_secs(30),
            new_sessions_per_minute: 30,
        }
    }
}

struct Session {
    peers: [Option<SocketAddr>; 2],
    last_seen: Instant,
    /// Balde de fichas: bytes que ainda podem passar agora.
    allowance: f64,
    refilled: Instant,
}

/// Estado do relay, separado da E/S para poder ser testado sozinho.
pub struct Relay {
    config: Config,
    sessions: HashMap<Token, Session>,
    recent: HashMap<IpAddr, (Instant, u32)>,
    pub forwarded_bytes: u64,
    pub dropped: u64,
}

impl Relay {
    pub fn new(config: Config) -> Self {
        Self { config, sessions: HashMap::new(), recent: HashMap::new(), forwarded_bytes: 0, dropped: 0 }
    }

    pub fn sessions(&self) -> usize {
        self.sessions.len()
    }

    /// Decide o que fazer com um datagrama: devolve para onde repassá-lo,
    /// ou `None` se ele deve ser descartado (ou era só uma apresentação).
    pub fn route(&mut self, datagram: &[u8], from: SocketAddr, now: Instant) -> Option<SocketAddr> {
        let (token, payload) = relay::parse(datagram)?;

        if !self.sessions.contains_key(&token) {
            if self.sessions.len() >= self.config.max_sessions || !self.may_open(from.ip(), now) {
                self.dropped += 1;
                return None;
            }
            let burst = self.config.bytes_per_second as f64;
            self.sessions
                .insert(token, Session { peers: [None, None], last_seen: now, allowance: burst, refilled: now });
        }
        let session = self.sessions.get_mut(&token)?;

        // Os dois primeiros endereços que apresentarem o token formam o par.
        let me = match session.peers.iter().position(|peer| *peer == Some(from)) {
            Some(index) => index,
            None => match session.peers.iter().position(Option::is_none) {
                Some(free) => {
                    session.peers[free] = Some(from);
                    free
                }
                None => {
                    self.dropped += 1;
                    return None;
                }
            },
        };
        session.last_seen = now;
        if payload.is_empty() {
            return None;
        }
        let other = session.peers[1 - me]?;

        let rate = self.config.bytes_per_second as f64;
        let elapsed = now.saturating_duration_since(session.refilled).as_secs_f64();
        session.allowance = (session.allowance + elapsed * rate).min(rate);
        session.refilled = now;
        if session.allowance < datagram.len() as f64 {
            // Acima do teto: descarta. O controle de congestionamento do QUIC
            // nos dispositivos reduz o ritmo sozinho.
            self.dropped += 1;
            return None;
        }
        session.allowance -= datagram.len() as f64;
        self.forwarded_bytes += datagram.len() as u64;
        Some(other)
    }

    /// Esquece sessões paradas.
    pub fn expire(&mut self, now: Instant) {
        let idle = self.config.idle_timeout;
        self.sessions.retain(|_, session| now.saturating_duration_since(session.last_seen) < idle);
        self.recent.retain(|_, (since, _)| now.saturating_duration_since(*since) < Duration::from_secs(60));
    }

    fn may_open(&mut self, ip: IpAddr, now: Instant) -> bool {
        let entry = self.recent.entry(ip).or_insert((now, 0));
        if now.saturating_duration_since(entry.0) >= Duration::from_secs(60) {
            *entry = (now, 0);
        }
        entry.1 += 1;
        entry.1 <= self.config.new_sessions_per_minute
    }
}

/// Atende no socket até dar erro de E/S.
pub async fn serve(socket: UdpSocket, config: Config) -> io::Result<()> {
    let mut relay = Relay::new(config);
    let mut buf = vec![0u8; 65536];
    let mut housekeeping = tokio::time::interval(Duration::from_secs(5));
    let mut reported = Instant::now();
    loop {
        tokio::select! {
            received = socket.recv_from(&mut buf) => {
                let (len, from) = match received {
                    Ok(received) => received,
                    // No Windows, um destino que sumiu aparece como erro na
                    // leitura seguinte. Não é motivo para parar.
                    Err(err) if err.kind() == io::ErrorKind::ConnectionReset => continue,
                    Err(err) => return Err(err),
                };
                if let Some(to) = relay.route(&buf[..len], from, Instant::now()) {
                    let _ = socket.send_to(&buf[..len], to).await;
                }
            }
            _ = housekeeping.tick() => {
                relay.expire(Instant::now());
                if reported.elapsed() >= Duration::from_secs(60) {
                    reported = Instant::now();
                    tracing::info!(
                        sessoes = relay.sessions(),
                        repassado_mb = relay.forwarded_bytes / 1_000_000,
                        descartados = relay.dropped,
                        "relay ativo"
                    );
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn addr(port: u16) -> SocketAddr {
        SocketAddr::from(([203, 0, 113, 1], port))
    }

    fn packet(token: u8, payload: &[u8]) -> Vec<u8> {
        relay::wrap(&[token; relay::TOKEN_LEN], payload)
    }

    #[test]
    fn pairs_two_peers_and_forwards_between_them() {
        let mut relay = Relay::new(Config::default());
        let now = Instant::now();
        // Apresentações não são repassadas.
        assert_eq!(relay.route(&packet(1, b""), addr(1), now), None);
        // Sem o segundo par ainda, não há para onde mandar.
        assert_eq!(relay.route(&packet(1, b"oi"), addr(1), now), None);
        assert_eq!(relay.route(&packet(1, b""), addr(2), now), None);
        assert_eq!(relay.route(&packet(1, b"oi"), addr(1), now), Some(addr(2)));
        assert_eq!(relay.route(&packet(1, b"ola"), addr(2), now), Some(addr(1)));
        assert_eq!(relay.sessions(), 1);
    }

    #[test]
    fn third_party_cannot_join_or_receive() {
        let mut relay = Relay::new(Config::default());
        let now = Instant::now();
        relay.route(&packet(1, b""), addr(1), now);
        relay.route(&packet(1, b""), addr(2), now);
        assert_eq!(relay.route(&packet(1, b"intruso"), addr(3), now), None);
        // E o par original continua intacto.
        assert_eq!(relay.route(&packet(1, b"x"), addr(1), now), Some(addr(2)));
        // Outro token é outra sessão, sem vazamento entre elas.
        relay.route(&packet(2, b""), addr(3), now);
        assert_eq!(relay.route(&packet(2, b"x"), addr(3), now), None);
    }

    #[test]
    fn ignores_what_is_not_relay_traffic() {
        let mut relay = Relay::new(Config::default());
        assert_eq!(relay.route(b"\x40pacote quic qualquer............", addr(1), Instant::now()), None);
        assert_eq!(relay.route(b"", addr(1), Instant::now()), None);
        assert_eq!(relay.sessions(), 0);
    }

    #[test]
    fn enforces_bandwidth_cap_and_recovers() {
        let config = Config { bytes_per_second: 1000, ..Config::default() };
        let mut relay = Relay::new(config);
        let start = Instant::now();
        relay.route(&packet(1, b""), addr(1), start);
        relay.route(&packet(1, b""), addr(2), start);
        let big = packet(1, &[0u8; 583]); // 600 bytes com o cabeçalho
        assert!(relay.route(&big, addr(1), start).is_some());
        assert!(relay.route(&big, addr(1), start).is_none(), "segundo pacote estoura o teto de 1000 B/s");
        assert!(relay.route(&big, addr(1), start + Duration::from_secs(1)).is_some(), "o balde enche de novo");
    }

    #[test]
    fn limits_sessions_and_expires_idle_ones() {
        let config = Config { max_sessions: 2, ..Config::default() };
        let mut relay = Relay::new(config);
        let start = Instant::now();
        relay.route(&packet(1, b""), addr(1), start);
        relay.route(&packet(2, b""), addr(1), start);
        relay.route(&packet(3, b""), addr(1), start);
        assert_eq!(relay.sessions(), 2);
        relay.expire(start + Duration::from_secs(31));
        assert_eq!(relay.sessions(), 0);
    }

    #[test]
    fn limits_new_sessions_per_source() {
        let config = Config { new_sessions_per_minute: 3, ..Config::default() };
        let mut relay = Relay::new(config);
        let start = Instant::now();
        for token in 0..10u8 {
            relay.route(&packet(token, b""), addr(1), start);
        }
        assert_eq!(relay.sessions(), 3);
        // Outro IP não é afetado.
        relay.route(&packet(50, b""), SocketAddr::from(([198, 51, 100, 2], 9)), start);
        assert_eq!(relay.sessions(), 4);
    }
}
