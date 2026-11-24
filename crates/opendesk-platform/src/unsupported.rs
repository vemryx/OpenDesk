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

pub mod system {
    use anyhow::{Result, bail};

    pub fn autostart_command() -> Option<String> {
        None
    }

    pub fn set_autostart(_command: Option<&str>) -> Result<()> {
        bail!(super::MSG)
    }

    pub fn lock_workstation() {}

    /// Sem acesso ao fuso do sistema, devolve a hora em UTC.
    pub fn local_time() -> String {
        let secs = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map_or(0, |d| d.as_secs());
        let (days, rest) = ((secs / 86_400) as i64, secs % 86_400);
        // Dias desde 1970 para data civil (algoritmo de Howard Hinnant).
        let z = days + 719_468;
        let era = z.div_euclid(146_097);
        let doe = z.rem_euclid(146_097);
        let yoe = (doe - doe / 1_460 + doe / 36_524 - doe / 146_096) / 365;
        let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
        let mp = (5 * doy + 2) / 153;
        let (day, month) = (doy - (153 * mp + 2) / 5 + 1, if mp < 10 { mp + 3 } else { mp - 9 });
        let year = yoe + era * 400 + i64::from(month <= 2);
        format!("{year:04}-{month:02}-{day:02} {:02}:{:02} UTC", rest / 3_600, rest % 3_600 / 60)
    }
}

pub mod clipboard {
    use anyhow::{Result, bail};

    pub fn sequence() -> u32 {
        0
    }

    pub fn text() -> Option<String> {
        None
    }

    pub fn set_text(_text: &str) -> Result<()> {
        bail!(super::MSG)
    }
}

pub mod agent {
    use crate::capture::{DisplayInfo, Frame};
    use anyhow::{Result, bail};
    use opendesk_proto::control::InputEvent;

    pub fn available() -> bool {
        false
    }

    pub fn run(_parent: Option<u32>) -> Result<()> {
        bail!(super::MSG)
    }

    pub fn serve_unverified(_pipe: &str) -> Result<()> {
        bail!(super::MSG)
    }

    pub struct RemoteCapturer(());

    impl RemoteCapturer {
        pub fn open(_display: usize) -> Result<Self> {
            bail!(super::MSG)
        }
        pub fn open_unverified(_pipe: &str, _display: usize) -> Result<Self> {
            bail!(super::MSG)
        }
        pub fn display(&self) -> &DisplayInfo {
            unreachable!()
        }
        pub fn frame(&mut self, _timeout_ms: u32) -> Result<Option<Frame<'_>>> {
            bail!(super::MSG)
        }
        pub fn advance(&mut self, _timeout_ms: u32) -> Result<bool> {
            bail!(super::MSG)
        }
        pub fn current(&self) -> Frame<'_> {
            unreachable!()
        }
    }

    pub struct RemoteInjector(());

    impl RemoteInjector {
        pub fn open(_display: &DisplayInfo) -> Result<Self> {
            bail!(super::MSG)
        }
        pub fn open_unverified(_pipe: &str, _display: &DisplayInfo) -> Result<Self> {
            bail!(super::MSG)
        }
        pub fn apply(&mut self, _event: InputEvent) -> Result<()> {
            bail!(super::MSG)
        }
        pub fn release_all(&mut self) -> Result<()> {
            bail!(super::MSG)
        }
    }
}

pub mod service {
    use anyhow::{Result, bail};
    use std::path::PathBuf;

    #[derive(Debug, Clone, Default, PartialEq, Eq)]
    pub struct ServiceState {
        pub installed: bool,
        pub running: bool,
        pub path: Option<String>,
        pub current: bool,
        pub packaged: bool,
    }

    pub fn state() -> ServiceState {
        ServiceState::default()
    }
    pub fn install() -> Result<()> {
        bail!(super::MSG)
    }
    pub fn uninstall() -> Result<()> {
        bail!(super::MSG)
    }
    pub fn elevate(_argument: &str) -> Result<()> {
        bail!(super::MSG)
    }
    pub fn run() -> Result<()> {
        bail!(super::MSG)
    }
    pub fn log_path() -> Option<PathBuf> {
        None
    }
}
