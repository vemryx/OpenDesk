//! Mensagens do canal de controle.

use serde::{Deserialize, Serialize};

/// O que o visualizador pode fazer além de ver a tela.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
pub struct Permissions {
    /// Enviar mouse e teclado.
    pub control: bool,
    /// Sincronizar a área de transferência.
    pub clipboard: bool,
    /// Enviar e receber arquivos.
    pub files: bool,
}

impl Permissions {
    pub const VIEW_ONLY: Self = Self { control: false, clipboard: false, files: false };
    pub const FULL: Self = Self { control: true, clipboard: true, files: true };

    /// Nunca concede mais do que foi pedido.
    pub fn intersect(self, other: Self) -> Self {
        Self {
            control: self.control && other.control,
            clipboard: self.clipboard && other.clipboard,
            files: self.files && other.files,
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Hello {
    pub protocol: u16,
    pub device_name: String,
    pub app_version: String,
    pub wants: Permissions,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DisplayDesc {
    pub width: u16,
    pub height: u16,
    pub name: String,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum DenyReason {
    /// A pessoa no computador remoto recusou.
    Rejected,
    /// Ninguém respondeu a tempo.
    Timeout,
    /// Versões de protocolo incompatíveis.
    VersionMismatch,
    /// O host não conseguiu capturar a tela.
    CaptureUnavailable,
    /// O host já está em outra sessão.
    Busy,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum Decision {
    Granted {
        permissions: Permissions,
        display: DisplayDesc,
        /// Qual dos monitores do host está sendo mostrado, e quantos há.
        index: u8,
        displays: u8,
    },
    Denied {
        reason: DenyReason,
    },
}

/// O que o visualizador precisa para tentar a senha de acesso não
/// supervisionado: o sal e o custo da derivação da senha. Não revela nada
/// sobre a senha em si.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct PasswordChallenge {
    pub salt: [u8; 16],
    pub iterations: u32,
}

/// Maior texto de área de transferência que atravessa a sessão, em bytes.
pub const MAX_CLIPBOARD_TEXT: usize = 100_000;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum MouseButton {
    Left,
    Right,
    Middle,
    Back,
    Forward,
}

/// Evento de entrada. Posições são normalizadas em `0..=65535` sobre a tela
/// compartilhada, para não depender da resolução ou da escala de quem vê.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum InputEvent {
    MouseMove {
        x: u16,
        y: u16,
    },
    MouseButton {
        button: MouseButton,
        down: bool,
        x: u16,
        y: u16,
    },
    /// Em unidades de `WHEEL_DELTA` (120 = um "clique" da roda).
    Wheel {
        dx: i16,
        dy: i16,
    },
    /// Tecla física: scancode do conjunto 1 e o prefixo estendido (E0).
    Key {
        scancode: u16,
        extended: bool,
        down: bool,
    },
}

/// Visualizador → host.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum ViewerMsg {
    Hello(Hello),
    Input(InputEvent),
    /// Pede um quadro completo (por exemplo, após perder sincronia).
    RequestKeyframe,
    Bye,
    /// Tentativa de entrar com a senha de acesso não supervisionado: a
    /// mensagem do visualizador na troca SPAKE2. A senha nunca trafega.
    Auth(Vec<u8>),
    /// Prova de que o visualizador chegou à mesma chave que o host.
    AuthConfirm([u8; 32]),
    /// Texto copiado no computador de quem vê.
    Clipboard(String),
    /// Pede para ver outro monitor do host.
    SelectDisplay(u8),
    /// Pede para bloquear a estação do host (exige permissão de controle).
    Lock,
}

/// Host → visualizador.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum HostMsg {
    /// Enviada logo após o `Hello`, antes de a pessoa decidir.
    Welcome {
        protocol: u16,
        device_name: String,
        app_version: String,
        /// Presente se o host aceita senha de acesso não supervisionado.
        password: Option<PasswordChallenge>,
    },
    Decision(Decision),
    Bye(String),
    /// A mensagem do host na troca SPAKE2.
    Auth(Vec<u8>),
    /// A senha conferiu; prova de que o host também a conhece. A decisão
    /// (`Decision::Granted`) vem em seguida.
    AuthOk([u8; 32]),
    /// A senha não conferiu. `retry` diz se dá para tentar de novo agora.
    AuthFailed {
        retry: bool,
    },
    /// Texto copiado no host.
    Clipboard(String),
    /// O monitor mostrado mudou.
    Display {
        index: u8,
        display: DisplayDesc,
    },
}
