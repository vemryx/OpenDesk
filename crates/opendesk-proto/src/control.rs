//! Mensagens do canal de controle.

use serde::{Deserialize, Serialize};

/// O que o visualizador pode fazer além de ver a tela.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
pub struct Permissions {
    /// Enviar mouse e teclado.
    pub control: bool,
    /// Sincronizar a área de transferência.
    pub clipboard: bool,
}

impl Permissions {
    pub const VIEW_ONLY: Self = Self { control: false, clipboard: false };
    pub const FULL: Self = Self { control: true, clipboard: true };

    /// Nunca concede mais do que foi pedido.
    pub fn intersect(self, other: Self) -> Self {
        Self { control: self.control && other.control, clipboard: self.clipboard && other.clipboard }
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
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum Decision {
    Granted { permissions: Permissions, display: DisplayDesc },
    Denied { reason: DenyReason },
}

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
}

/// Host → visualizador.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum HostMsg {
    /// Enviada logo após o `Hello`, antes de a pessoa decidir.
    Welcome {
        protocol: u16,
        device_name: String,
        app_version: String,
    },
    Decision(Decision),
    Bye(String),
}
