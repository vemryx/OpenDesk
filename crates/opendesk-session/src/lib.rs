//! Sessões do OpenDesk.
//!
//! * [`host`] — aceita conexões, **pede autorização a quem está no
//!   computador** e, se concedida, transmite a tela e aplica a entrada
//!   recebida. Não existe caminho de código que inicie uma sessão sem passar
//!   por essa decisão.
//! * [`viewer`] — conecta a um host, mantém o framebuffer remoto e envia
//!   mouse e teclado.
//! * [`backend`] — de onde vêm os quadros e para onde vai a entrada. A
//!   implementação real usa `opendesk-platform`; os testes usam uma falsa.

pub mod backend;
pub mod host;
pub mod viewer;

use std::sync::Arc;

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
