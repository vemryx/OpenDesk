//! Sessões do OpenDesk.
//!
//! * [`host`] — aceita conexões, **pede autorização a quem está no
//!   computador** e, se concedida, transmite a tela e aplica a entrada
//!   recebida. O único outro caminho para uma sessão é a senha de acesso não
//!   supervisionado, que vem desligada e precisa ser definida de propósito.
//! * [`viewer`] — conecta a um host, mantém o framebuffer remoto e envia
//!   mouse e teclado.
//! * [`auth`] — a senha de acesso não supervisionado (SPAKE2).
//! * [`files`] — transferência de arquivos durante a sessão.
//! * [`backend`] — de onde vêm os quadros e para onde vai a entrada. A
//!   implementação real usa `opendesk-platform`; os testes usam uma falsa.

pub mod auth;
pub mod backend;
pub mod files;
pub mod host;
pub mod viewer;

use std::sync::Arc;
use tokio::sync::mpsc;

/// Lê as mensagens de um canal de controle numa tarefa própria e as entrega
/// por uma fila. Ler direto num `select!` perderia pedaços de mensagem quando
/// outro ramo vencesse no meio da leitura; receber da fila não tem esse risco.
/// A fila fecha quando o canal fecha ou chega algo inválido.
pub(crate) fn inbox<M>(mut rx: quinn::RecvStream) -> mpsc::Receiver<M>
where
    M: serde::de::DeserializeOwned + Send + 'static,
{
    let (tx, messages) = mpsc::channel(256);
    tokio::spawn(async move {
        while let Ok(msg) = opendesk_proto::wire::recv_msg::<_, M>(&mut rx).await {
            if tx.send(msg).await.is_err() {
                break;
            }
        }
    });
    messages
}

/// Chamado sempre que há algo novo para a interface (um evento ou um quadro).
/// A interface usa isto para pedir um redesenho.
pub type Wake = Arc<dyn Fn() + Send + Sync>;

pub(crate) fn describe_close(err: &quinn::ConnectionError) -> String {
    use quinn::ConnectionError::*;
    match err {
        ApplicationClosed(close) => match close.reason.as_ref() {
            b"host" => "O computador remoto encerrou a sessão.".into(),
            b"viewer" => "O visualizador encerrou a sessão.".into(),
            b"shutdown" => "O outro lado fechou o OpenDesk.".into(),
            _ => "A sessão foi encerrada pelo outro lado.".into(),
        },
        LocallyClosed => "Sessão encerrada.".into(),
        TimedOut => "A conexão caiu (sem resposta do outro lado).".into(),
        other => format!("A conexão foi perdida: {other}"),
    }
}
