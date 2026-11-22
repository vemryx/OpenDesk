use anyhow::Result;
use opendesk_platform::agent::{RemoteCapturer, RemoteInjector};
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
    /// A entrada alcança o que o sistema nega a um programa comum (janelas de
    /// administrador, aviso de permissão, tela de bloqueio).
    fn full_control(&self) -> bool {
        false
    }
}

/// A tela e a entrada reais deste computador.
///
/// Com o serviço do OpenDesk instalado, a captura e a entrada passam pelo
/// agente privilegiado, que alcança janelas de administrador, o aviso do UAC e
/// a tela de bloqueio. Sem ele (ou se ele cair no meio da sessão), são feitas
/// aqui mesmo, com os limites que o Windows impõe a um programa comum.
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
        match RemoteCapturer::open(display) {
            Ok(agent) => {
                tracing::info!("captura pelo agente: controle total");
                Ok(Box::new(AgentSource { index: display, agent: Some(agent), local: None }))
            }
            Err(err) => {
                tracing::debug!("captura direta, sem o agente: {err:#}");
                Ok(Box::new(Capturer::new(display)?))
            }
        }
    }

    fn open_clipboard(&self) -> Option<Box<dyn Clipboard>> {
        Some(Box::new(SystemClipboard::new()))
    }

    fn lock(&self) {
        opendesk_platform::system::lock_workstation();
    }

    fn full_control(&self) -> bool {
        opendesk_platform::agent::available()
    }

    fn open_sink(&self, display: &DisplayInfo) -> Box<dyn InputSink> {
        match RemoteInjector::open(display) {
            Ok(agent) => Box::new(AgentSink { display: display.clone(), agent: Some(agent), local: None }),
            Err(err) => {
                tracing::debug!("entrada direta, sem o agente: {err:#}");
                Box::new(Injector::new(display))
            }
        }
    }
}

/// Captura pelo agente; se ele sumir (serviço parado ou removido durante a
/// sessão), continua pela captura direta em vez de derrubar a sessão.
struct AgentSource {
    index: usize,
    agent: Option<RemoteCapturer>,
    local: Option<Capturer>,
}

impl FrameSource for AgentSource {
    fn display(&self) -> DisplayInfo {
        match (&self.agent, &self.local) {
            (Some(agent), _) => agent.display().clone(),
            (None, Some(local)) => local.display().clone(),
            (None, None) => unreachable!("sempre há uma das duas capturas"),
        }
    }

    fn frame(&mut self, timeout_ms: u32) -> Result<Option<Frame<'_>>> {
        if let Some(agent) = &mut self.agent {
            match agent.advance(timeout_ms) {
                Ok(true) => return Ok(self.agent.as_ref().map(RemoteCapturer::current)),
                Ok(false) => return Ok(None),
                Err(err) => {
                    tracing::warn!("o agente parou de capturar ({err:#}); seguindo com a captura direta");
                    self.agent = None;
                    self.local = Some(Capturer::new(self.index)?);
                }
            }
        }
        self.local.as_mut().expect("criada ao perder o agente").frame(timeout_ms)
    }
}

/// Entrada pelo agente, com a mesma queda para a injeção direta.
struct AgentSink {
    display: DisplayInfo,
    agent: Option<RemoteInjector>,
    local: Option<Injector>,
}

impl AgentSink {
    fn fall_back(&mut self) -> &mut Injector {
        if self.agent.take().is_some() {
            tracing::warn!("o agente parou de receber entrada; seguindo com a injeção direta");
        }
        self.local.get_or_insert_with(|| Injector::new(&self.display))
    }
}

impl InputSink for AgentSink {
    fn apply(&mut self, event: InputEvent) {
        if let Some(agent) = &mut self.agent
            && agent.apply(event).is_ok()
        {
            return;
        }
        self.fall_back().apply(event);
    }

    fn release_all(&mut self) {
        if let Some(agent) = &mut self.agent
            && agent.release_all().is_ok()
        {
            return;
        }
        self.fall_back().release_all();
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
