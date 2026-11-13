//! Plataformas ainda sem implementação: tudo falha de forma explícita.

pub fn init_process() {}

const MSG: &str = "esta plataforma ainda não é suportada pelo OpenDesk";

pub mod capture {
    use crate::capture::{DisplayInfo, Frame};
    use anyhow::{Result, bail};

    pub fn displays() -> Result<Vec<DisplayInfo>> {
        bail!(super::MSG)
    }

    pub struct Capturer(());

    impl Capturer {
        pub fn new(_display: usize) -> Result<Self> {
            bail!(super::MSG)
        }
        pub fn display(&self) -> &DisplayInfo {
            unreachable!()
        }
        pub fn frame(&mut self, _timeout_ms: u32) -> Result<Option<Frame<'_>>> {
            bail!(super::MSG)
        }
    }
}

pub mod input {
    use crate::capture::DisplayInfo;
    use opendesk_proto::control::InputEvent;

    pub struct Injector(());

    impl Injector {
        pub fn new(_display: &DisplayInfo) -> Self {
            Self(())
        }
        pub fn apply(&mut self, _event: InputEvent) {}
        pub fn release_all(&mut self) {}
    }
}

pub mod keyboard {
    use crate::keyboard::KeyEvent;
    use anyhow::{Result, bail};
    use std::sync::mpsc::Receiver;

    pub struct KeyboardGrab(());

    impl KeyboardGrab {
        pub fn install() -> Result<(Self, Receiver<KeyEvent>)> {
            bail!(super::MSG)
        }
        pub fn set_active(&self, _active: bool) {}
    }
}
