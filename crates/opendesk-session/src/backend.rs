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

/// A área de transferência de um dos lados da sessão.
pub trait Clipboard: Send {
    /// O texto copiado desde a última consulta, se houve cópia nova.
    fn poll(&mut self) -> Option<String>;
    /// Põe o texto vindo do outro lado. Não deve reaparecer em `poll`.
    fn set(&mut self, text: &str);
}

/// Fábrica dos dois lados de uma sessão no host.
pub trait Backend: Send + Sync + 'static {
    /// Quantos monitores podem ser mostrados.
    fn displays(&self) -> usize {
        1
    }
    /// O monitor mostrado quando a sessão começa.
    fn default_display(&self) -> usize {
        0
    }
    /// Chamado na thread de captura (objetos gráficos não mudam de thread).
    fn open_source(&self, display: usize) -> Result<Box<dyn FrameSource>>;
    fn open_sink(&self, display: &DisplayInfo) -> Box<dyn InputSink>;
    /// `None` quando não há área de transferência a sincronizar.
    fn open_clipboard(&self) -> Option<Box<dyn Clipboard>> {
        None
    }
    /// Bloqueia a estação, a pedido de quem controla.
    fn lock(&self) {}
}

/// A tela e a entrada reais deste computador.
pub struct SystemBackend {
    /// Índice do monitor mostrado no início da sessão (0 = principal).
    pub display: usize,
}

impl Backend for SystemBackend {
    fn displays(&self) -> usize {
        opendesk_platform::capture::displays().map_or(1, |displays| displays.len().max(1))
    }

    fn default_display(&self) -> usize {
        self.display
    }

    fn open_source(&self, display: usize) -> Result<Box<dyn FrameSource>> {
        Ok(Box::new(Capturer::new(display)?))
    }

    fn open_clipboard(&self) -> Option<Box<dyn Clipboard>> {
        Some(Box::new(SystemClipboard::new()))
    }

    fn lock(&self) {
        opendesk_platform::system::lock_workstation();
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

/// A área de transferência do sistema. Só repassa o que for copiado depois de
/// criada: o que já estava lá antes da sessão não é enviado.
pub struct SystemClipboard {
    seen: u32,
    /// O último texto que passou por aqui, em qualquer direção. Evita mandar
    /// de volta o que acabou de chegar (ou reenviar a mesma cópia).
    last: Option<String>,
}

impl SystemClipboard {
    pub fn new() -> Self {
        Self { seen: opendesk_platform::clipboard::sequence(), last: None }
    }
}

impl Default for SystemClipboard {
    fn default() -> Self {
        Self::new()
    }
}

impl Clipboard for SystemClipboard {
    fn poll(&mut self) -> Option<String> {
        let now = opendesk_platform::clipboard::sequence();
        if now == self.seen {
            return None;
        }
        self.seen = now;
        let text = opendesk_platform::clipboard::text().filter(|text| !text.is_empty())?;
        if self.last.as_deref() == Some(text.as_str()) {
            return None;
        }
        self.last = Some(text.clone());
        Some(text)
    }

    fn set(&mut self, text: &str) {
        self.last = Some(text.to_string());
        if let Err(err) = opendesk_platform::clipboard::set_text(text) {
            tracing::debug!("não foi possível gravar na área de transferência: {err:#}");
        }
        // O que acabamos de gravar não é uma cópia nova a repassar de volta.
        self.seen = opendesk_platform::clipboard::sequence();
    }
}
