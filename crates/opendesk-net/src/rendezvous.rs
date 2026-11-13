//! Cliente do servidor de encontro (ver `rendezvous/` na raiz do repositório).
//!
//! O servidor só apresenta os pares: quem quer conectar manda os seus
//! endereços, o dono do ID responde com os dele, e cada lado começa a enviar
//! pacotes para o outro para abrir o caminho nos roteadores. A sessão em si
//! nunca passa pelo servidor.
//!
//! O servidor não precisa ser confiável. A impressão digital que ele informa
//! é conferida no handshake QUIC, e o visualizador ainda a compara com a que
//! guardou da última vez (ver [`crate::known`]).

use crate::identity::{DeviceId, Fingerprint, Identity};
use crate::node::{Nat, Node};
use anyhow::{Context, Result, anyhow, bail};
use futures_util::{SinkExt, StreamExt};
use serde::Deserialize;
use serde_json::json;
use std::net::SocketAddr;
use std::sync::Arc;
use std::time::Duration;
use thiserror::Error;
use tokio::net::TcpStream;
use tokio::time::timeout;
use tokio_tungstenite::tungstenite::Message;
use tokio_tungstenite::{Connector, MaybeTlsStream, WebSocketStream, connect_async_tls_with_config};

/// Deve ser igual a `MAX_ID_ATTEMPT` no servidor.
const MAX_ID_ATTEMPT: u8 = 15;
const STEP_TIMEOUT: Duration = Duration::from_secs(10);
/// O servidor espera 15 s pela resposta do host.
const ANSWER_TIMEOUT: Duration = Duration::from_secs(16);
const PING_INTERVAL: Duration = Duration::from_secs(25);
/// Por quanto tempo o host fica abrindo caminho para quem pediu conexão.
const PUNCH_FOR: Duration = Duration::from_secs(8);
const RETRY_MIN: Duration = Duration::from_secs(2);
const RETRY_MAX: Duration = Duration::from_secs(60);

type Socket = WebSocketStream<MaybeTlsStream<TcpStream>>;

/// Situação deste dispositivo no servidor de encontro.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Presence {
    Connecting,
    /// Registrado: pode ser encontrado por este ID.
    Online {
        id: DeviceId,
        nat: Nat,
    },
    /// Sem contato com o servidor; uma nova tentativa virá sozinha.
    Offline(String),
}

/// O que o dono do ID respondeu.
#[derive(Debug, Clone)]
pub struct Answer {
    /// Impressão digital com que o host se registrou no servidor.
    pub fingerprint: Fingerprint,
    pub candidates: Vec<SocketAddr>,
}

#[derive(Debug, Error)]
pub enum DialError {
    #[error("Esse computador não está online no OpenDesk agora.")]
    Offline,
    #[error("Esse computador está recebendo pedidos demais. Tente de novo em instantes.")]
    Busy,
    #[error("Esse computador não respondeu ao pedido.")]
    NoAnswer,
    #[error("Não foi possível falar com o servidor de encontro: {0}")]
    Server(String),
}

#[derive(Deserialize)]
#[serde(tag = "t", rename_all = "snake_case")]
enum ServerMsg {
    Challenge { nonce: String },
    Registered {},
    Offer { session: String, candidates: Vec<String> },
    Answer { fingerprint: String, candidates: Vec<String> },
    Error { code: String },
}

pub struct Rendezvous {
    base: String,
    tls: Arc<rustls::ClientConfig>,
}

impl Rendezvous {
    /// `url` é a raiz do servidor: `wss://exemplo.dev` (ou `https://…`,
    /// `ws://…` para um servidor local).
    pub fn new(url: &str) -> Result<Self> {
        let url = url.trim().trim_end_matches('/');
        let base = if let Some(rest) = url.strip_prefix("https://") {
            format!("wss://{rest}")
        } else if let Some(rest) = url.strip_prefix("http://") {
            format!("ws://{rest}")
        } else if url.starts_with("wss://") || url.starts_with("ws://") {
            url.to_string()
        } else {
            bail!("endereço de servidor de encontro inválido: {url}");
        };

        let roots = rustls::RootCertStore { roots: webpki_roots::TLS_SERVER_ROOTS.to_vec() };
        let tls = rustls::ClientConfig::builder_with_provider(Arc::new(rustls::crypto::ring::default_provider()))
            .with_safe_default_protocol_versions()?
            .with_root_certificates(roots)
            .with_no_client_auth();
        Ok(Self { base, tls: Arc::new(tls) })
    }

    pub fn url(&self) -> &str {
        &self.base
    }

    /// Pede ao dono de `id` os endereços dele, entregando os nossos.
    pub async fn dial(&self, id: DeviceId, me: Fingerprint, candidates: &[SocketAddr]) -> Result<Answer, DialError> {
        let server = |err: anyhow::Error| DialError::Server(format!("{err:#}"));
        let mut ws = self.open(&format!("/v1/dial/{}", id.0)).await.map_err(server)?;
        let offer = json!({
            "t": "offer",
            "fingerprint": me.hex(),
            "candidates": candidates.iter().map(SocketAddr::to_string).collect::<Vec<_>>(),
        });
        // Se o host estiver offline o servidor já respondeu e fechou; o erro
        // que interessa é o que vier na leitura.
        let _ = ws.send(Message::text(offer.to_string())).await;

        match next(&mut ws, ANSWER_TIMEOUT).await {
            Ok(ServerMsg::Answer { fingerprint, candidates }) => {
                let fingerprint = Fingerprint::from_hex(&fingerprint)
                    .ok_or_else(|| DialError::Server("resposta com impressão digital inválida".into()))?;
                Ok(Answer { fingerprint, candidates: parse_candidates(&candidates) })
            }
            Ok(ServerMsg::Error { code }) => Err(match code.as_str() {
                "offline" => DialError::Offline,
                "busy" => DialError::Busy,
                "timeout" => DialError::NoAnswer,
                other => DialError::Server(format!("o servidor recusou o pedido ({other})")),
            }),
            Ok(_) => Err(DialError::Server("resposta inesperada".into())),
            Err(err) => Err(server(err)),
        }
    }

    /// Mantém este dispositivo registrado e respondendo a pedidos, para
    /// sempre: se a conexão com o servidor cair, tenta de novo com espera
    /// crescente. `status` é chamado a cada mudança de situação.
    pub async fn serve(&self, node: Arc<Node>, stun_servers: Vec<String>, status: impl Fn(Presence)) {
        let mut wait = RETRY_MIN;
        status(Presence::Connecting);
        loop {
            let mut was_online = false;
            let err = self
                .session(&node, &stun_servers, |presence| {
                    was_online = true;
                    status(presence);
                })
                .await;
            if was_online {
                wait = RETRY_MIN;
            }
            tracing::debug!("servidor de encontro: {err:#}");
            status(Presence::Offline(format!("{err:#}")));
            tokio::time::sleep(wait).await;
            wait = (wait * 2).min(RETRY_MAX);
        }
    }

    /// Uma conexão com o servidor, do registro até cair.
    async fn session(&self, node: &Node, stun_servers: &[String], mut online: impl FnMut(Presence)) -> anyhow::Error {
        let (mut ws, id) = match self.register(node.identity()).await {
            Ok(registered) => registered,
            Err(err) => return err,
        };
        let (_, nat) = node.candidates(stun_servers).await;
        online(Presence::Online { id, nat });

        let mut ping = tokio::time::interval(PING_INTERVAL);
        loop {
            let message = tokio::select! {
                _ = ping.tick() => {
                    if let Err(err) = ws.send(Message::text("ping")).await {
                        return anyhow!(err).context("a conexão com o servidor caiu");
                    }
                    continue;
                }
                message = ws.next() => message,
            };
            let text = match message {
                Some(Ok(Message::Text(text))) => text,
                Some(Ok(Message::Close(_))) | None => return anyhow!("o servidor encerrou a conexão"),
                Some(Ok(_)) => continue,
                Some(Err(err)) => return anyhow!(err).context("a conexão com o servidor caiu"),
            };
            if text.as_str() == "pong" {
                continue;
            }
            match serde_json::from_str::<ServerMsg>(text.as_str()) {
                Ok(ServerMsg::Offer { session, candidates }) => {
                    // Abre o caminho antes de responder, para o buraco já
                    // existir quando o visualizador começar a tentar.
                    node.punch(&parse_candidates(&candidates), PUNCH_FOR);
                    let (mine, _) = node.candidates(stun_servers).await;
                    let answer = json!({
                        "t": "answer",
                        "session": session,
                        "candidates": mine.iter().map(SocketAddr::to_string).collect::<Vec<_>>(),
                    });
                    if let Err(err) = ws.send(Message::text(answer.to_string())).await {
                        return anyhow!(err).context("a conexão com o servidor caiu");
                    }
                }
                Ok(ServerMsg::Error { code }) if code == "replaced" => {
                    return anyhow!("outro OpenDesk com esta mesma identidade entrou no lugar deste");
                }
                Ok(_) => {}
                Err(err) => tracing::debug!("mensagem do servidor ignorada: {err}"),
            }
        }
    }

    /// Registra a identidade e devolve o ID obtido. Se o primeiro ID já for
    /// de outro dispositivo, tenta os alternativos.
    async fn register(&self, identity: &Identity) -> Result<(Socket, DeviceId)> {
        let fingerprint = identity.fingerprint();
        for attempt in 0..=MAX_ID_ATTEMPT {
            let id = fingerprint.device_id_attempt(attempt);
            let mut ws = self.open(&format!("/v1/listen/{}", id.0)).await?;
            let ServerMsg::Challenge { nonce } = next(&mut ws, STEP_TIMEOUT).await? else {
                bail!("o servidor não enviou o desafio");
            };
            let signature = identity.sign(format!("opendesk-rendezvous-v1:register:{}:{nonce}", id.0).as_bytes())?;
            let register = json!({
                "t": "register",
                "cert": hex(identity.certificate()),
                "attempt": attempt,
                "sig": hex(&signature),
            });
            ws.send(Message::text(register.to_string())).await.context("enviar registro")?;
            match next(&mut ws, STEP_TIMEOUT).await? {
                ServerMsg::Registered {} => return Ok((ws, id)),
                ServerMsg::Error { code } if code == "id_taken" => continue,
                ServerMsg::Error { code } => bail!("o servidor recusou o registro ({code})"),
                _ => bail!("resposta inesperada ao registro"),
            }
        }
        bail!("não há ID livre para este dispositivo")
    }

    async fn open(&self, path: &str) -> Result<Socket> {
        let url = format!("{}{path}", self.base);
        let connector = Connector::Rustls(self.tls.clone());
        let connect = connect_async_tls_with_config(url.as_str(), None, false, Some(connector));
        let (ws, _) = timeout(STEP_TIMEOUT, connect)
            .await
            .map_err(|_| anyhow!("o servidor de encontro não respondeu"))?
            .with_context(|| format!("conectar a {}", self.base))?;
        Ok(ws)
    }
}

async fn next(ws: &mut Socket, limit: Duration) -> Result<ServerMsg> {
    let read = async {
        loop {
            match ws.next().await {
                Some(Ok(Message::Text(text))) if text.as_str() != "pong" => {
                    return serde_json::from_str(text.as_str()).context("mensagem inválida do servidor");
                }
                Some(Ok(Message::Close(_))) | None => bail!("o servidor encerrou a conexão"),
                Some(Ok(_)) => {}
                Some(Err(err)) => return Err(anyhow!(err).context("a conexão com o servidor caiu")),
            }
        }
    };
    timeout(limit, read).await.map_err(|_| anyhow!("o servidor de encontro demorou demais"))?
}

/// Endereços vindos da rede: o que não for `ip:porta` IPv4 utilizável é
/// descartado em silêncio.
fn parse_candidates(list: &[String]) -> Vec<SocketAddr> {
    list.iter()
        .filter_map(|text| text.parse::<SocketAddr>().ok())
        .filter(|addr| addr.is_ipv4() && addr.port() != 0 && !addr.ip().is_unspecified())
        .take(8)
        .collect()
}

fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn normalizes_server_urls() {
        assert_eq!(Rendezvous::new("https://encontro.example/").unwrap().url(), "wss://encontro.example");
        assert_eq!(Rendezvous::new(" http://127.0.0.1:8787 ").unwrap().url(), "ws://127.0.0.1:8787");
        assert_eq!(Rendezvous::new("wss://a.b").unwrap().url(), "wss://a.b");
        assert!(Rendezvous::new("encontro.example").is_err());
    }

    #[test]
    fn drops_unusable_candidates() {
        let list = ["192.168.0.5:47700", "lixo", "0.0.0.0:1", "10.0.0.1:0", "[::1]:5", "203.0.113.9:40000"];
        let parsed = parse_candidates(&list.map(String::from));
        assert_eq!(parsed, vec!["192.168.0.5:47700".parse().unwrap(), "203.0.113.9:40000".parse().unwrap()]);
    }

    #[test]
    fn understands_server_messages() {
        let offer: ServerMsg =
            serde_json::from_str(r#"{"t":"offer","session":"ab","fingerprint":"00","candidates":["1.2.3.4:5"]}"#)
                .unwrap();
        assert!(matches!(offer, ServerMsg::Offer { session, .. } if session == "ab"));
        let registered: ServerMsg = serde_json::from_str(r#"{"t":"registered","id":"123456789"}"#).unwrap();
        assert!(matches!(registered, ServerMsg::Registered {}));
        assert!(serde_json::from_str::<ServerMsg>(r#"{"t":"desconhecida"}"#).is_err());
    }
}
