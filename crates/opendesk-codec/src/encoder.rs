use crate::{CELL_TILES, TILE};
use opendesk_proto::video::{FramePacket, Patch, PatchEncoding};
use rayon::prelude::*;

/// Quadro capturado, em BGRA de 8 bits. `stride` é o passo entre linhas em
/// bytes e pode ser maior que `width * 4`.
#[derive(Clone, Copy)]
pub struct FrameRef<'a> {
    pub data: &'a [u8],
    pub width: usize,
    pub height: usize,
    pub stride: usize,
}

#[derive(Debug, Clone)]
pub struct EncoderConfig {
    /// Qualidade JPEG (1–100) das áreas que não comprimem sem perdas.
    pub jpeg_quality: u8,
    /// Uma célula vai sem perdas se o zstd a reduzir a no máximo esta fração
    /// do RGB cru.
    pub lossless_max_ratio: f32,
    pub zstd_level: i32,
    /// Quantas células, no máximo, um refinamento sem limite de bytes reenvia
    /// de cada vez.
    pub refine_cells_per_pass: usize,
}

impl Default for EncoderConfig {
    fn default() -> Self {
        Self { jpeg_quality: 80, lossless_max_ratio: 0.30, zstd_level: 1, refine_cells_per_pass: 16 }
    }
}

/// Um degrau de redução: como encolher uma célula quando a versão boa dela
/// não cabe no que a conexão escoa.
#[derive(Clone, Copy)]
struct Step {
    encoding: PatchEncoding,
    quality: u8,
    /// Tamanho típico em relação à versão boa. Só orienta a primeira escolha;
    /// o que for medido corrige o resto.
    share: f32,
}

/// Do mais nítido ao mais leve. Antes do primeiro degrau está a versão boa:
/// sem perdas onde comprime, JPEG da configuração no resto.
const LADDER: [Step; 6] = [
    Step { encoding: PatchEncoding::Jpeg, quality: 55, share: 0.60 },
    Step { encoding: PatchEncoding::JpegHalf, quality: 60, share: 0.25 },
    Step { encoding: PatchEncoding::JpegHalf, quality: 35, share: 0.16 },
    Step { encoding: PatchEncoding::JpegQuarter, quality: 45, share: 0.07 },
    Step { encoding: PatchEncoding::JpegQuarter, quality: 25, share: 0.04 },
    Step { encoding: PatchEncoding::JpegEighth, quality: 40, share: 0.015 },
];

/// Quando a versão boa passaria tanto assim do alvo, nem vale calculá-la:
/// todas as células vão reduzidas, e o codificador faz metade do trabalho.
const HOPELESS: f32 = 4.0;
/// E só volta a calculá-la quando a previsão cair abaixo disto, para não
/// alternar de um modo para o outro a cada quadro.
const HOPEFUL: f32 = 3.0;

/// Acima disto do alvo, o quadro é refeito um degrau (ou mais) abaixo.
const OVERSHOOT: f32 = 1.4;
/// Um quadro reduzido só é refeito num degrau melhor se couber com esta folga.
const UNDERSHOOT: f32 = 0.8;
/// Bytes por pixel da versão boa de uma tela comum, antes de medir.
const TYPICAL_DENSITY: f32 = 0.25;
/// Bytes por pixel de uma foto sem perdas, antes de medir.
const TYPICAL_EXACT_DENSITY: f32 = 1.5;

/// Como um bloco chegou a quem vê.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Grade {
    /// Idêntico ao original.
    Exact,
    /// O JPEG da versão boa: falta só a versão sem perdas.
    Good,
    /// Um degrau de redução: falta a versão boa, e depois a sem perdas.
    Rough,
}

#[derive(Clone, Copy)]
enum Mode {
    /// A versão boa de cada célula e, com `Some`, também a reduzida.
    Motion(Option<usize>),
    /// Só a versão reduzida, sem calcular a boa.
    Rough(usize),
    /// Sem perdas, custe o que custar.
    Exact,
}

impl Mode {
    fn step(self) -> Option<usize> {
        match self {
            Mode::Motion(step) => step,
            Mode::Rough(step) => Some(step),
            Mode::Exact => None,
        }
    }

    /// O lado das células, em blocos. Uma célula reduzida cobre mais tela,
    /// para a imagem enviada ter sempre por volta do mesmo tamanho: senão o
    /// cabeçalho de cada JPEG, que é fixo, pesaria mais que a imagem.
    fn cell_tiles(self) -> usize {
        CELL_TILES * self.step().map_or(1, |step| LADDER[step].encoding.scale())
    }
}

/// Uma célula codificada: a versão boa e, quando pedida e de fato menor, a
/// reduzida.
struct Coded {
    best: (PatchEncoding, Vec<u8>),
    small: Option<(PatchEncoding, Vec<u8>)>,
    /// Em [`Mode::Rough`] não há versão boa: `best` é a reduzida.
    rough: bool,
}

impl Coded {
    /// A versão boa, com a nota que ela merece.
    fn into_best(self) -> (PatchEncoding, Vec<u8>, Grade) {
        let grade = match self.best.0 {
            _ if self.rough => Grade::Rough,
            PatchEncoding::ZstdRgb => Grade::Exact,
            _ => Grade::Good,
        };
        (self.best.0, self.best.1, grade)
    }

    /// A versão reduzida, se houver; senão a boa.
    fn into_small(self) -> (PatchEncoding, Vec<u8>, Grade) {
        match self.small {
            Some((encoding, data)) => (encoding, data, Grade::Rough),
            None => self.into_best(),
        }
    }
}

/// Retângulo em unidades de bloco.
#[derive(Clone, Copy)]
struct TileRect {
    tx: usize,
    ty: usize,
    tw: usize,
    th: usize,
}

pub struct Encoder {
    cfg: EncoderConfig,
    width: usize,
    height: usize,
    tiles_x: usize,
    tiles_y: usize,
    /// Último quadro enviado, BGRA sem padding.
    reference: Vec<u8>,
    /// Como a última versão enviada de cada bloco chegou.
    grades: Vec<Grade>,
    /// Bytes por pixel da versão boa do conteúdo recente.
    density: f32,
    /// Quanto cada degrau reduziu o conteúdo recente, em relação à versão boa.
    shares: [f32; LADDER.len()],
    /// Bytes por pixel do refinamento sem perdas recente.
    exact_density: f32,
    /// O conteúdo recente pesa tanto para a conexão que a versão boa nem
    /// está sendo calculada.
    rough: bool,
    /// `density` já veio de um quadro de verdade, e não do palpite inicial.
    measured: bool,
    seq: u32,
}

impl Encoder {
    pub fn new(cfg: EncoderConfig) -> Self {
        Self {
            cfg,
            width: 0,
            height: 0,
            tiles_x: 0,
            tiles_y: 0,
            reference: Vec::new(),
            grades: Vec::new(),
            density: TYPICAL_DENSITY,
            shares: LADDER.map(|step| step.share),
            exact_density: TYPICAL_EXACT_DENSITY,
            rough: false,
            measured: false,
            seq: 0,
        }
    }

    /// Codifica o que mudou desde o último quadro enviado, na melhor
    /// qualidade. Devolve `None` se nada mudou.
    pub fn encode(&mut self, frame: &FrameRef<'_>) -> Option<FramePacket> {
        self.encode_within(frame, usize::MAX)
    }

    /// Codifica o que mudou, num quadro que pese por volta de `budget` bytes.
    ///
    /// Se a versão boa cabe, é ela que vai, igual a [`encode`](Self::encode).
    /// Se não cabe, as células são reduzidas (JPEG mais forte, depois metade
    /// e um quarto da resolução) e o espaço que sobrar devolve a versão boa
    /// às que custam menos: o texto continua nítido e é a foto que borra. O
    /// refinamento conserta o resto quando a tela parar. O que nunca acontece
    /// é um quadro levar segundos para atravessar a rede.
    pub fn encode_within(&mut self, frame: &FrameRef<'_>, budget: usize) -> Option<FramePacket> {
        assert!(frame.stride >= frame.width * 4, "stride menor que a largura");
        assert!(frame.data.len() >= frame.stride * (frame.height - 1) + frame.width * 4, "quadro truncado");

        let resized = frame.width != self.width || frame.height != self.height;
        if resized {
            self.width = frame.width;
            self.height = frame.height;
            self.tiles_x = frame.width.div_ceil(TILE);
            self.tiles_y = frame.height.div_ceil(TILE);
            self.reference = vec![0; frame.width * frame.height * 4];
            self.grades = vec![Grade::Exact; self.tiles_x * self.tiles_y];
        }

        let (width, height, tiles_x) = (self.width, self.height, self.tiles_x);
        let row_bytes = width * 4;
        let mut dirty = vec![resized; self.tiles_x * self.tiles_y];

        // Compara e atualiza a referência numa única passada, uma faixa de
        // blocos por tarefa: cada faixa é dona de um trecho disjunto de
        // `reference` e de `dirty`.
        self.reference.par_chunks_mut(row_bytes * TILE).zip(dirty.par_chunks_mut(tiles_x)).enumerate().for_each(
            |(ty, (ref_band, dirty_row))| {
                let y0 = ty * TILE;
                let band_h = TILE.min(height - y0);
                for (tx, flag) in dirty_row.iter_mut().enumerate() {
                    let x0 = tx * TILE * 4;
                    let x1 = ((tx + 1) * TILE).min(width) * 4;
                    let changed = *flag
                        || (0..band_h).any(|r| {
                            let src = (y0 + r) * frame.stride;
                            frame.data[src + x0..src + x1] != ref_band[r * row_bytes + x0..r * row_bytes + x1]
                        });
                    if changed {
                        for r in 0..band_h {
                            let src = (y0 + r) * frame.stride;
                            ref_band[r * row_bytes + x0..r * row_bytes + x1]
                                .copy_from_slice(&frame.data[src + x0..src + x1]);
                        }
                    }
                    *flag = changed;
                }
            },
        );

        if !dirty.iter().any(|&d| d) {
            return None;
        }
        Some(self.encode_fitting(&dirty, resized, budget))
    }

    /// Reenvia a tela inteira a partir da referência, sem nova captura. Serve
    /// para ressincronizar um visualizador. `None` se ainda não houve quadro.
    pub fn keyframe(&mut self) -> Option<FramePacket> {
        self.keyframe_within(usize::MAX)
    }

    /// Como [`keyframe`](Self::keyframe), cabendo em `budget` bytes.
    pub fn keyframe_within(&mut self, budget: usize) -> Option<FramePacket> {
        if self.reference.is_empty() {
            return None;
        }
        let all = vec![true; self.tiles_x * self.tiles_y];
        Some(self.encode_fitting(&all, true, budget))
    }

    /// Melhora parte do que foi enviado com perdas. Chame quando a tela
    /// estiver parada, até devolver `None`.
    pub fn refine(&mut self) -> Option<FramePacket> {
        self.refine_within(usize::MAX)
    }

    /// Como [`refine`](Self::refine), em passos de por volta de `budget`
    /// bytes: uma mudança nova na tela nunca espera mais que um passo.
    ///
    /// Primeiro devolve a versão boa ao que foi reduzido (o texto volta a
    /// ficar nítido), depois reenvia sem perdas o que ainda é JPEG.
    pub fn refine_within(&mut self, budget: usize) -> Option<FramePacket> {
        let target = [Grade::Rough, Grade::Good].into_iter().find(|grade| self.grades.contains(grade))?;
        let marked: Vec<bool> = self.grades.iter().map(|&grade| grade == target).collect();
        let mut cells = self.cells_covering(&marked, CELL_TILES);

        let (mode, density) = match target {
            Grade::Rough => (Mode::Motion(None), self.density),
            _ => (Mode::Exact, self.exact_density),
        };
        let count = if budget == usize::MAX {
            self.cfg.refine_cells_per_pass
        } else {
            // Quantas células cabem, pelo que o conteúdo recente pesou.
            let mut predicted = 0.0;
            cells
                .iter()
                .take_while(|cell| {
                    let fits = predicted < budget as f32;
                    predicted += self.pixels(cell) as f32 * density;
                    fits
                })
                .count()
        };
        cells.truncate(count.max(1));

        let pixels: usize = cells.iter().map(|cell| self.pixels(cell)).sum();
        let coded = self.compress(&cells, mode);
        let bytes: usize = coded.iter().map(|cell| cell.best.1.len()).sum();
        match mode {
            Mode::Exact => self.exact_density = blend(self.exact_density, bytes as f32 / pixels as f32),
            _ => self.density = blend(self.density, bytes as f32 / pixels as f32),
        }
        let chosen = coded.into_iter().map(Coded::into_best).collect();
        Some(self.finish(&cells, chosen, false))
    }

    /// Há áreas aguardando refinamento?
    pub fn has_lossy(&self) -> bool {
        self.grades.iter().any(|&grade| grade != Grade::Exact)
    }

    fn pixels(&self, cell: &TileRect) -> usize {
        let (_, _, w, h) = self.bounds(cell);
        w * h
    }

    /// Onde a célula fica na tela, em pixels: (x, y, largura, altura).
    fn bounds(&self, cell: &TileRect) -> (usize, usize, usize, usize) {
        let (x, y) = (cell.tx * TILE, cell.ty * TILE);
        (x, y, (cell.tw * TILE).min(self.width - x), (cell.th * TILE).min(self.height - y))
    }

    /// O degrau mais nítido cujo tamanho previsto cabe em `budget`; `None` se
    /// a versão boa já deve caber.
    fn pick(&self, pixels: usize, budget: f32) -> Option<usize> {
        let good = pixels as f32 * self.density;
        if good <= budget {
            return None;
        }
        Some((0..LADDER.len()).find(|&step| good * self.shares[step] <= budget).unwrap_or(LADDER.len() - 1))
    }

    /// Codifica os blocos marcados para caber em `budget` e confere o
    /// resultado.
    ///
    /// O conteúdo de uma tela muda de natureza de um quadro para o outro
    /// (texto, depois uma foto), então a previsão do degrau erra. Se o quadro
    /// passou muito do alvo, é refeito mais abaixo; se sobrou muito espaço, é
    /// refeito num degrau melhor. Uma correção só: o que foi medido orienta
    /// os quadros seguintes.
    fn encode_fitting(&mut self, marked: &[bool], keyframe: bool, budget: usize) -> FramePacket {
        let pixels: usize = self.cells_covering(marked, CELL_TILES).iter().map(|cell| self.pixels(cell)).sum();
        let limit = budget as f32;
        // Quando a versão boa não tem a menor chance de caber, ela nem é
        // calculada: é o trabalho mais caro do quadro, e seria jogado fora.
        // Só vale com o peso medido num quadro anterior: por um palpite, o
        // codificador borraria uma tela que caberia inteira.
        let predicted = pixels as f32 * self.density;
        self.rough = self.measured && predicted > limit * if self.rough { HOPEFUL } else { HOPELESS };
        let mode = |step: Option<usize>, rough: bool| match step {
            Some(step) if rough => Mode::Rough(step),
            step => Mode::Motion(step),
        };

        let mut step = self.pick(pixels, limit);
        let mut cells = self.cells_covering(marked, mode(step, self.rough).cell_tiles());
        let mut coded = self.compress(&cells, mode(step, self.rough));
        let mut plan = Plan::fit(&coded, budget);

        let over = plan.bytes as f32 > limit * OVERSHOOT;
        let retry = match step {
            // A previsão dizia que a versão boa cabia, e não coube.
            None if over => {
                self.density = plan.good as f32 / pixels as f32;
                self.pick(pixels, limit).or(Some(0))
            }
            // Pelo menos um degrau abaixo, pelo que este acabou de render.
            Some(at) if over && at + 1 < LADDER.len() => {
                let wanted = self.shares[at] * limit / plan.bytes as f32;
                Some((at + 1..LADDER.len()).find(|&next| self.shares[next] <= wanted).unwrap_or(LADDER.len() - 1))
            }
            // O que ficou reduzido caberia num degrau melhor?
            Some(at) if !over && plan.reduced > 0 => {
                let fixed = (plan.bytes - plan.reduced) as f32;
                (0..at).find(|&better| {
                    fixed + plan.reduced as f32 * self.shares[better] / self.shares[at] <= limit * UNDERSHOOT
                })
            }
            _ => None,
        };
        if let Some(retry) = retry {
            step = Some(retry);
            cells = self.cells_covering(marked, mode(step, self.rough).cell_tiles());
            coded = self.compress(&cells, mode(step, self.rough));
            plan = Plan::fit(&coded, budget);
        }

        // O que este conteúdo pesa, para prever o próximo quadro.
        let encoded: usize = cells.iter().map(|cell| self.pixels(cell)).sum();
        match step {
            // Sem a versão boa em mãos, o peso dela sai do que o degrau rendeu.
            Some(at) if self.rough => {
                self.density = blend(self.density, plan.bytes as f32 / encoded as f32 / self.shares[at]);
            }
            _ => {
                // A primeira medida substitui o palpite; as seguintes o acompanham.
                let good = plan.good as f32 / encoded as f32;
                self.density = if self.measured { blend(self.density, good) } else { good };
                self.measured = true;
                if let Some(at) = step {
                    self.shares[at] = blend(self.shares[at], plan.smallest as f32 / plan.good.max(1) as f32);
                }
            }
        }
        let chosen = coded
            .into_iter()
            .zip(&plan.reduce)
            .map(|(cell, &reduce)| if reduce { cell.into_small() } else { cell.into_best() })
            .collect();
        self.finish(&cells, chosen, keyframe)
    }

    /// Para cada célula da grade (de `cell_tiles` blocos de lado) que contém
    /// algum bloco marcado, a caixa envolvente dos blocos marcados dentro dela.
    fn cells_covering(&self, marked: &[bool], cell_tiles: usize) -> Vec<TileRect> {
        let mut cells = Vec::new();
        for cy in (0..self.tiles_y).step_by(cell_tiles) {
            for cx in (0..self.tiles_x).step_by(cell_tiles) {
                let (mut x0, mut y0, mut x1, mut y1) = (usize::MAX, usize::MAX, 0, 0);
                for ty in cy..(cy + cell_tiles).min(self.tiles_y) {
                    for tx in cx..(cx + cell_tiles).min(self.tiles_x) {
                        if marked[ty * self.tiles_x + tx] {
                            x0 = x0.min(tx);
                            y0 = y0.min(ty);
                            x1 = x1.max(tx + 1);
                            y1 = y1.max(ty + 1);
                        }
                    }
                }
                if x0 != usize::MAX {
                    cells.push(TileRect { tx: x0, ty: y0, tw: x1 - x0, th: y1 - y0 });
                }
            }
        }
        cells
    }

    fn compress(&self, cells: &[TileRect], mode: Mode) -> Vec<Coded> {
        cells
            .par_iter()
            .map(|cell| {
                let (x, y, w, h) = self.bounds(cell);
                encode_region(&self.reference, self.width, x, y, w, h, &self.cfg, mode)
            })
            .collect()
    }

    /// Registra como cada bloco ficou e monta o pacote.
    fn finish(
        &mut self,
        cells: &[TileRect],
        chosen: Vec<(PatchEncoding, Vec<u8>, Grade)>,
        keyframe: bool,
    ) -> FramePacket {
        let mut patches = Vec::with_capacity(cells.len());
        for (cell, (encoding, data, grade)) in cells.iter().zip(chosen) {
            // Uma célula cobre também blocos que não mudaram dentro da caixa
            // envolvente; todos eles passam a ter a qualidade dela.
            for ty in cell.ty..cell.ty + cell.th {
                for tx in cell.tx..cell.tx + cell.tw {
                    self.grades[ty * self.tiles_x + tx] = grade;
                }
            }
            let (x, y, w, h) = self.bounds(cell);
            patches.push(Patch { x: x as u16, y: y as u16, w: w as u16, h: h as u16, encoding, data });
        }

        let seq = self.seq;
        self.seq = self.seq.wrapping_add(1);
        FramePacket { seq, width: self.width as u16, height: self.height as u16, keyframe, patches }
    }
}

/// Quais células de um quadro vão reduzidas para ele caber no alvo.
struct Plan {
    reduce: Vec<bool>,
    /// Tamanho do quadro com essa escolha.
    bytes: usize,
    /// Quanto desse tamanho é de células reduzidas.
    reduced: usize,
    /// Tamanho se todas fossem na versão boa.
    good: usize,
    /// Tamanho se todas as que podem fossem reduzidas.
    smallest: usize,
}

impl Plan {
    fn fit(coded: &[Coded], budget: usize) -> Self {
        let size = |cell: &Coded, reduce: bool| match (&cell.small, reduce) {
            (Some(small), true) => small.1.len(),
            _ => cell.best.1.len(),
        };
        let good: usize = coded.iter().map(|cell| size(cell, false)).sum();
        let smallest: usize = coded.iter().map(|cell| size(cell, true)).sum();
        let mut reduce: Vec<bool> = coded.iter().map(|cell| cell.small.is_some()).collect();
        let mut bytes = smallest;
        if good <= budget {
            reduce.fill(false);
            bytes = good;
        } else {
            // Com o espaço que sobra, a versão boa volta para as células em
            // que ela custa menos a mais: o texto antes da foto.
            let mut upgrades: Vec<(usize, usize)> = coded
                .iter()
                .enumerate()
                .filter(|(_, cell)| cell.small.is_some())
                .map(|(index, cell)| (size(cell, false) - size(cell, true), index))
                .collect();
            upgrades.sort_unstable();
            for (extra, index) in upgrades {
                if bytes + extra > budget {
                    break;
                }
                bytes += extra;
                reduce[index] = false;
            }
        }
        let reduced = coded.iter().zip(&reduce).filter(|&(_, &reduce)| reduce).map(|(cell, _)| size(cell, true)).sum();
        Self { reduce, bytes, reduced, good, smallest }
    }
}

/// Média que acompanha rápido uma mudança de conteúdo.
fn blend(previous: f32, sample: f32) -> f32 {
    if sample.is_finite() { previous * 0.5 + sample * 0.5 } else { previous }
}

#[allow(clippy::too_many_arguments)]
fn encode_region(
    reference: &[u8],
    width: usize,
    x: usize,
    y: usize,
    w: usize,
    h: usize,
    cfg: &EncoderConfig,
    mode: Mode,
) -> Coded {
    let rows = (0..h).map(|r| {
        let start = ((y + r) * width + x) * 4;
        &reference[start..start + w * 4]
    });
    let jpeg = |bgra: &[u8], w: usize, h: usize, quality: u8| {
        let mut out = Vec::with_capacity(w * h / 2);
        let encoder = jpeg_encoder::Encoder::new(&mut out, quality);
        encoder.encode(bgra, w as u16, h as u16, jpeg_encoder::ColorType::Bgra).ok().map(|()| out)
    };
    let full = || {
        let mut bgra = Vec::with_capacity(w * h * 4);
        for row in rows.clone() {
            bgra.extend_from_slice(row);
        }
        bgra
    };
    // A célula num degrau de redução.
    let reduce = |step: usize| {
        let Step { encoding, quality, .. } = LADDER[step];
        let scale = encoding.scale();
        let reduced = if scale == 1 {
            jpeg(&full(), w, h, quality)
        } else {
            let (bgra, small_w, small_h) = shrink(reference, width, x, y, w, h, scale);
            jpeg(&bgra, small_w, small_h, quality)
        };
        reduced.map(|reduced| (encoding, reduced))
    };

    if let Mode::Rough(step) = mode
        && let Some(reduced) = reduce(step)
    {
        return Coded { best: reduced, small: None, rough: true };
    }

    let mut rgb = Vec::with_capacity(w * h * 3);
    for row in rows.clone() {
        for px in row.as_chunks::<4>().0 {
            rgb.extend_from_slice(&[px[2], px[1], px[0]]);
        }
    }
    let level = match mode {
        Mode::Exact => cfg.zstd_level.max(3),
        _ => cfg.zstd_level,
    };
    let lossless = zstd::bulk::compress(&rgb, level).expect("zstd em memória não falha");
    if let Mode::Exact = mode {
        return Coded { best: (PatchEncoding::ZstdRgb, lossless), small: None, rough: false };
    }

    // A versão boa: o que comprime bem vai sem perdas mesmo que um JPEG fosse
    // menor (é texto e interface, e ali a nitidez importa mais); o resto vai
    // como JPEG, se ele for de fato menor.
    let compresses = lossless.len() <= (rgb.len() as f32 * cfg.lossless_max_ratio) as usize;
    let best = if compresses {
        (PatchEncoding::ZstdRgb, lossless)
    } else {
        match jpeg(&full(), w, h, cfg.jpeg_quality) {
            Some(jpeg) if jpeg.len() < lossless.len() => (PatchEncoding::Jpeg, jpeg),
            _ => (PatchEncoding::ZstdRgb, lossless),
        }
    };
    // Só vale reduzir se o resultado for de fato menor: cor chapada e desenho
    // simples ficam sem perdas em qualquer degrau.
    let small = mode.step().and_then(reduce).filter(|reduced| reduced.1.len() < best.1.len());
    Coded { best, small, rough: false }
}

/// A região reduzida `scale` vezes em cada eixo, cada pixel sendo a média do
/// bloco que ele substitui. Devolve o BGRA e o tamanho reduzido.
fn shrink(
    reference: &[u8],
    width: usize,
    x: usize,
    y: usize,
    w: usize,
    h: usize,
    scale: usize,
) -> (Vec<u8>, usize, usize) {
    let (small_w, small_h) = (w.div_ceil(scale), h.div_ceil(scale));
    let mut out = Vec::with_capacity(small_w * small_h * 4);
    for sy in 0..small_h {
        let rows = (sy * scale)..((sy + 1) * scale).min(h);
        for sx in 0..small_w {
            let columns = (sx * scale)..((sx + 1) * scale).min(w);
            let mut sum = [0u32; 3];
            for row in rows.clone() {
                let start = ((y + row) * width + x + columns.start) * 4;
                for px in reference[start..start + columns.len() * 4].as_chunks::<4>().0 {
                    sum[0] += px[0] as u32;
                    sum[1] += px[1] as u32;
                    sum[2] += px[2] as u32;
                }
            }
            let count = (rows.len() * columns.len()) as u32;
            out.extend_from_slice(&[
                ((sum[0] + count / 2) / count) as u8,
                ((sum[1] + count / 2) / count) as u8,
                ((sum[2] + count / 2) / count) as u8,
                255,
            ]);
        }
    }
    (out, small_w, small_h)
}
