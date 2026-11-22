//! Uma tela de mentira, para medir o OpenDesk sem tocar na tela de verdade.
//!
//! Produz conteúdo que se comporta como o uso real: uma página de texto
//! parada, a mesma página rolando, ou um vídeo tocando no meio dela. Em todo
//! quadro vão duas marcas que o visualizador sabe ler:
//!
//! * o **carimbo**: a hora em que o quadro foi produzido, em blocos pretos e
//!   brancos no canto. Comparando com o relógio ao receber, sai o atraso da
//!   imagem, de ponta a ponta;
//! * a **sonda**: um quadrado que troca de cor a cada clique recebido. O
//!   tempo entre mandar o clique e ver a cor mudar é o atraso que a pessoa
//!   sente.
//!
//! O host e o visualizador rodam na mesma máquina, então o relógio é o mesmo.

use anyhow::Result;
use opendesk_platform::capture::{DisplayInfo, Frame};
use opendesk_proto::control::{InputEvent, MouseButton};
use opendesk_session::backend::{Backend, FrameSource, InputSink};
use std::sync::{Arc, Condvar, Mutex};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

/// Lado de cada bloco do carimbo, em pixels.
pub const STAMP_BLOCK: usize = 12;
pub const STAMP_BITS: usize = 32;
/// O quadrado da sonda: canto e lado.
pub const PROBE_AT: (usize, usize) = (40, 60);
pub const PROBE_SIDE: usize = 160;
/// Cores da sonda (R, G, B), bem distantes para sobreviver ao JPEG.
pub const PROBE_OFF: [u8; 3] = [0, 170, 0];
pub const PROBE_ON: [u8; 3] = [220, 0, 220];

const TICKS_PER_SECOND: f64 = 60.0;
const SCROLL_PIXELS_PER_SECOND: f64 = 900.0;
const VIDEO_FRAMES_PER_SECOND: f64 = 30.0;
const VIDEO_FRAMES: usize = 12;

#[derive(Debug, Clone, Copy, PartialEq, Eq, clap::ValueEnum)]
pub enum Scene {
    /// Página de texto parada: só a sonda muda.
    Parado,
    /// A página rola sem parar: a tela inteira muda a cada quadro.
    Rolagem,
    /// Um vídeo toca no meio da página parada.
    Video,
}

/// Milissegundos do relógio do sistema, nos 32 bits que cabem no carimbo.
pub fn clock_ms() -> u32 {
    SystemTime::now().duration_since(UNIX_EPOCH).unwrap_or_default().as_millis() as u32
}

#[derive(Default)]
struct Shared {
    /// (sonda ligada, quantas vezes mudou).
    probe: Mutex<(bool, u64)>,
    changed: Condvar,
}

pub struct SyntheticBackend {
    scene: Scene,
    width: usize,
    height: usize,
    shared: Arc<Shared>,
}

impl SyntheticBackend {
    pub fn new(scene: Scene, width: usize, height: usize) -> Self {
        Self { scene, width, height, shared: Arc::default() }
    }
}

impl Backend for SyntheticBackend {
    fn open_source(&self, _display: usize) -> Result<Box<dyn FrameSource>> {
        let (width, height) = (self.width, self.height);
        let page_height = height * 3;
        let video = match self.scene {
            Scene::Video => Some(Video::new(width, height)),
            _ => None,
        };
        Ok(Box::new(Source {
            scene: self.scene,
            width,
            height,
            page: page(width, page_height),
            page_height,
            video,
            buffer: vec![0; width * height * 4],
            shared: self.shared.clone(),
            started: Instant::now(),
            last_tick: None,
            seen: u64::MAX,
        }))
    }

    fn open_sink(&self, _display: &DisplayInfo) -> Box<dyn InputSink> {
        Box::new(Sink(self.shared.clone()))
    }
}

struct Sink(Arc<Shared>);

impl InputSink for Sink {
    fn apply(&mut self, event: InputEvent) {
        if let InputEvent::MouseButton { button: MouseButton::Left, down: true, .. } = event {
            let mut probe = self.0.probe.lock().unwrap();
            *probe = (!probe.0, probe.1 + 1);
            self.0.changed.notify_all();
        }
    }

    fn release_all(&mut self) {}
}

struct Source {
    scene: Scene,
    width: usize,
    height: usize,
    /// A página inteira, mais alta que a tela, em BGRA.
    page: Vec<u8>,
    page_height: usize,
    video: Option<Video>,
    buffer: Vec<u8>,
    shared: Arc<Shared>,
    started: Instant,
    last_tick: Option<u64>,
    /// A versão da sonda no último quadro entregue.
    seen: u64,
}

impl FrameSource for Source {
    fn display(&self) -> DisplayInfo {
        DisplayInfo {
            name: "sintética".into(),
            x: 0,
            y: 0,
            width: self.width as u32,
            height: self.height as u32,
            primary: true,
        }
    }

    fn frame(&mut self, timeout_ms: u32) -> Result<Option<Frame<'_>>> {
        let deadline = Instant::now() + Duration::from_millis(timeout_ms as u64);
        // Espera a tela "mudar": um clique na sonda, ou o próximo instante da
        // animação. O conteúdo depende da hora, não de quantas vezes foi
        // pedido: quem pede menos quadros vê a mesma rolagem, só com saltos.
        let (probe_on, version, tick) = loop {
            let (on, version) = *self.shared.probe.lock().unwrap();
            let tick = (self.started.elapsed().as_secs_f64() * TICKS_PER_SECOND) as u64;
            let animated = self.scene != Scene::Parado && self.last_tick != Some(tick);
            if version != self.seen || animated {
                break (on, version, tick);
            }
            let now = Instant::now();
            if now >= deadline {
                return Ok(None);
            }
            let next_tick = match self.scene {
                Scene::Parado => deadline,
                _ => self.started + Duration::from_secs_f64((tick + 1) as f64 / TICKS_PER_SECOND),
            };
            let wait = next_tick.min(deadline).saturating_duration_since(now).max(Duration::from_micros(200));
            let probe = self.shared.probe.lock().unwrap();
            if probe.1 == self.seen {
                drop(self.shared.changed.wait_timeout(probe, wait).unwrap());
            }
        };
        self.seen = version;
        self.last_tick = Some(tick);

        let (width, height) = (self.width, self.height);
        let row = width * 4;
        let seconds = tick as f64 / TICKS_PER_SECOND;
        let offset = match self.scene {
            Scene::Rolagem => (seconds * SCROLL_PIXELS_PER_SECOND) as usize % (self.page_height - height),
            _ => 0,
        };
        self.buffer.copy_from_slice(&self.page[offset * row..(offset + height) * row]);
        if let Some(video) = &self.video {
            video.draw(&mut self.buffer, width, (seconds * VIDEO_FRAMES_PER_SECOND) as usize);
        }

        let color = if probe_on { PROBE_ON } else { PROBE_OFF };
        fill(&mut self.buffer, width, PROBE_AT.0, PROBE_AT.1, PROBE_SIDE, PROBE_SIDE, color);
        let stamp = clock_ms();
        for bit in 0..STAMP_BITS {
            let level = if stamp >> bit & 1 == 1 { 255 } else { 0 };
            fill(&mut self.buffer, width, bit * STAMP_BLOCK, 0, STAMP_BLOCK, STAMP_BLOCK, [level; 3]);
        }
        Ok(Some(Frame { data: &self.buffer, width, height, stride: row }))
    }
}

/// Pinta um retângulo de cor chapada num quadro BGRA.
fn fill(frame: &mut [u8], width: usize, x: usize, y: usize, w: usize, h: usize, rgb: [u8; 3]) {
    for row in y..y + h {
        for pixel in frame[(row * width + x) * 4..(row * width + x + w) * 4].as_chunks_mut::<4>().0 {
            *pixel = [rgb[2], rgb[1], rgb[0], 255];
        }
    }
}

struct Random(u64);

impl Random {
    fn next(&mut self) -> u64 {
        self.0 ^= self.0 << 13;
        self.0 ^= self.0 >> 7;
        self.0 ^= self.0 << 17;
        self.0
    }

    fn below(&mut self, limit: u64) -> u64 {
        self.next() % limit
    }
}

/// Uma página que comprime como uma página de verdade: parágrafos de "texto"
/// (desenhos pequenos e repetidos, em tons de cinza sobre fundo claro), e de
/// vez em quando uma "foto" (degradê com ruído), que é o que não comprime sem
/// perdas.
fn page(width: usize, height: usize) -> Vec<u8> {
    const GLYPH: (usize, usize) = (7, 11);
    const CELL: (usize, usize) = (9, 18);
    let mut random = Random(0x2545_F491_4F6C_DD1D);
    let mut page = vec![0u8; width * height * 4];
    for pixel in page.as_chunks_mut::<4>().0 {
        *pixel = [250, 250, 250, 255];
    }

    // Um alfabeto de 64 desenhos, com três tons para imitar a suavização.
    let glyphs: Vec<Vec<u8>> = (0..64)
        .map(|_| {
            (0..GLYPH.0 * GLYPH.1)
                .map(|_| match random.below(10) {
                    0..=4 => 250,
                    5 => 170,
                    6 => 110,
                    _ => 40,
                })
                .collect()
        })
        .collect();

    let margin = 80.min(width / 10);
    let columns = (width - 2 * margin) / CELL.0;
    let mut y = 40;
    let mut paragraph = 0;
    while y + CELL.1 < height {
        paragraph += 1;
        if paragraph % 5 == 0 && y + 280 < height {
            // Uma foto.
            let (w, h) = (480.min(width - 2 * margin), 260);
            for row in 0..h {
                for column in 0..w {
                    let i = ((y + row) * width + margin + column) * 4;
                    let noise = random.below(24) as usize;
                    page[i] = (60 + column * 150 / w + noise) as u8;
                    page[i + 1] = (90 + row * 120 / h + noise) as u8;
                    page[i + 2] = (200 - column * 100 / w + noise) as u8;
                }
            }
            y += h + 30;
            continue;
        }
        for _line in 0..3 + random.below(6) {
            if y + CELL.1 >= height {
                break;
            }
            let mut column = 0;
            while column < columns {
                let word = 2 + random.below(9) as usize;
                for letter in 0..word.min(columns - column) {
                    let glyph = &glyphs[random.below(64) as usize];
                    let x = margin + (column + letter) * CELL.0;
                    for (index, &shade) in glyph.iter().enumerate() {
                        let i = ((y + index / GLYPH.0) * width + x + index % GLYPH.0) * 4;
                        page[i..i + 3].fill(shade);
                    }
                }
                column += word + 1;
            }
            y += CELL.1;
        }
        y += CELL.1;
    }
    page
}

/// Um "vídeo": quadros de manchas coloridas que mudam inteiras a cada quadro,
/// como uma cena em movimento.
struct Video {
    x: usize,
    y: usize,
    width: usize,
    height: usize,
    frames: Vec<Vec<u8>>,
}

impl Video {
    fn new(screen_width: usize, screen_height: usize) -> Self {
        let (width, height) = (screen_width * 2 / 3, screen_height * 2 / 3);
        let mut random = Random(0x9E37_79B9_7F4A_7C15);
        let frames = (0..VIDEO_FRAMES)
            .map(|frame| {
                let phase = frame as f32 * std::f32::consts::TAU / VIDEO_FRAMES as f32;
                let mut pixels = vec![255u8; width * height * 4];
                for (index, pixel) in pixels.as_chunks_mut::<4>().0.iter_mut().enumerate() {
                    let (x, y) = ((index % width) as f32, (index / width) as f32);
                    let wave = |a: f32, b: f32, c: f32| 127.0 + 100.0 * (x * a + y * b + phase * c).sin();
                    let noise = random.below(16) as f32;
                    pixel[0] = (wave(0.011, 0.007, 1.0) + noise) as u8;
                    pixel[1] = (wave(0.004, 0.013, 2.0) + noise) as u8;
                    pixel[2] = (wave(0.009, -0.006, 3.0) + noise) as u8;
                }
                pixels
            })
            .collect();
        Self { x: screen_width / 6, y: screen_height / 5, width, height, frames }
    }

    fn draw(&self, screen: &mut [u8], screen_width: usize, index: usize) {
        let frame = &self.frames[index % self.frames.len()];
        for row in 0..self.height {
            let to = ((self.y + row) * screen_width + self.x) * 4;
            screen[to..to + self.width * 4].copy_from_slice(&frame[row * self.width * 4..(row + 1) * self.width * 4]);
        }
    }
}
