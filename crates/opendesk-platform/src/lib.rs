//! Tudo o que depende do sistema operacional fica aqui, atrás de uma API
//! pequena. Hoje só o Windows está implementado; nas outras plataformas as
//! funções devolvem erro, e portar o OpenDesk é implementar este crate.
//!
//! * [`capture`] — captura de tela (Windows: DXGI Desktop Duplication).
//! * [`input`] — injeção de mouse e teclado no host (Windows: `SendInput`).
//! * [`keyboard`] — captura de teclado no visualizador, para que atalhos como
//!   Alt+Tab e a tecla Windows cheguem ao computador remoto.
//! * [`clipboard`] — texto da área de transferência.
//! * [`system`] — abrir junto com o sistema, bloquear a estação, hora local.

#[cfg(windows)]
#[path = "windows/mod.rs"]
mod imp;

#[cfg(not(windows))]
#[path = "unsupported.rs"]
mod imp;

pub mod capture {
    pub use crate::imp::capture::{Capturer, displays};

    /// Um monitor ligado à área de trabalho.
    #[derive(Debug, Clone)]
    pub struct DisplayInfo {
        pub name: String,
        /// Posição na área de trabalho virtual, em pixels físicos.
        pub x: i32,
        pub y: i32,
        pub width: u32,
        pub height: u32,
        pub primary: bool,
    }

    /// Quadro capturado em BGRA. Vale até a próxima chamada ao capturador.
    pub struct Frame<'a> {
        pub data: &'a [u8],
        pub width: usize,
        pub height: usize,
        pub stride: usize,
    }
}

pub mod input {
    pub use crate::imp::input::Injector;
}

pub mod keyboard {
    pub use crate::imp::keyboard::KeyboardGrab;

    /// Tecla física pressionada ou solta no visualizador.
    #[derive(Debug, Clone, Copy, PartialEq, Eq)]
    pub struct KeyEvent {
        pub scancode: u16,
        pub extended: bool,
        pub down: bool,
    }
}

/// Texto da área de transferência do sistema.
pub mod clipboard {
    pub use crate::imp::clipboard::{sequence, set_text, text};
}

pub mod system {
    pub use crate::imp::system::{autostart_command, local_time, lock_workstation, set_autostart};
}

/// Declara o processo como ciente de DPI por monitor, para que coordenadas e
/// tamanhos sejam sempre em pixels físicos. Chame uma vez, no início.
pub fn init_process() {
    imp::init_process();
}
