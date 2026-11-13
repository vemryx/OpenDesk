use anyhow::Result;
use opendesk_platform::capture::{Capturer, DisplayInfo, Frame};
use opendesk_platform::input::Injector;
use opendesk_proto::control::InputEvent;

/// Fonte de quadros de uma tela. Vive inteira na thread de captura.
pub trait FrameSource {
    fn display(&self) -> DisplayInfo;
    /// Espera até `timeout_ms` por uma mudança. `Ok(None)` = nada mudou.
    fn frame(&mut self, timeout_ms: u32) -> Result<Option<Frame<'_>>>;
}

/// Destino dos eventos de entrada do visualizador.
pub trait InputSink: Send {
    fn apply(&mut self, event: InputEvent);
    /// Solta tudo o que ficou pressionado.
    fn release_all(&mut self);
}

/// Fábrica dos dois lados de uma sessão no host.
pub trait Backend: Send + Sync + 'static {
    /// Chamado na thread de captura (objetos gráficos não mudam de thread).
    fn open_source(&self) -> Result<Box<dyn FrameSource>>;
    fn open_sink(&self, display: &DisplayInfo) -> Box<dyn InputSink>;
}

/// A tela e a entrada reais deste computador.
pub struct SystemBackend {
    /// Índice do monitor a compartilhar (0 = principal).
    pub display: usize,
}

impl Backend for SystemBackend {
    fn open_source(&self) -> Result<Box<dyn FrameSource>> {
        Ok(Box::new(Capturer::new(self.display)?))
    }

    fn open_sink(&self, display: &DisplayInfo) -> Box<dyn InputSink> {
        Box::new(Injector::new(display))
    }
}

impl FrameSource for Capturer {
    fn display(&self) -> DisplayInfo {
        Capturer::display(self).clone()
    }

    fn frame(&mut self, timeout_ms: u32) -> Result<Option<Frame<'_>>> {
        Capturer::frame(self, timeout_ms)
    }
}

impl InputSink for Injector {
    fn apply(&mut self, event: InputEvent) {
        Injector::apply(self, event);
    }

    fn release_all(&mut self) {
        Injector::release_all(self);
    }
}
