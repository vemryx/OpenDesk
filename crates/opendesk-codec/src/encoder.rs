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
    /// Quantas células, no máximo, um refinamento reenvia de cada vez.
    pub refine_cells_per_pass: usize,
}

impl Default for EncoderConfig {
    fn default() -> Self {
        Self { jpeg_quality: 80, lossless_max_ratio: 0.30, zstd_level: 1, refine_cells_per_pass: 16 }
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
    /// Blocos cuja última versão enviada foi com perdas.
    lossy: Vec<bool>,
    seq: u32,
}

impl Encoder {
    pub fn new(cfg: EncoderConfig) -> Self {
        Self { cfg, width: 0, height: 0, tiles_x: 0, tiles_y: 0, reference: Vec::new(), lossy: Vec::new(), seq: 0 }
    }

    /// Codifica o que mudou desde o último quadro enviado. Devolve `None` se
    /// nada mudou.
    pub fn encode(&mut self, frame: &FrameRef<'_>) -> Option<FramePacket> {
        assert!(frame.stride >= frame.width * 4, "stride menor que a largura");
        assert!(frame.data.len() >= frame.stride * (frame.height - 1) + frame.width * 4, "quadro truncado");

        let resized = frame.width != self.width || frame.height != self.height;
        if resized {
            self.width = frame.width;
            self.height = frame.height;
            self.tiles_x = frame.width.div_ceil(TILE);
            self.tiles_y = frame.height.div_ceil(TILE);
            self.reference = vec![0; frame.width * frame.height * 4];
            self.lossy = vec![false; self.tiles_x * self.tiles_y];
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
        let cells = self.cells_covering(&dirty);
        Some(self.encode_cells(&cells, resized, false))
    }

    /// Reenvia a tela inteira a partir da referência, sem nova captura. Serve
    /// para ressincronizar um visualizador. `None` se ainda não houve quadro.
    pub fn keyframe(&mut self) -> Option<FramePacket> {
        if self.reference.is_empty() {
            return None;
        }
        let all = vec![true; self.tiles_x * self.tiles_y];
        let cells = self.cells_covering(&all);
        Some(self.encode_cells(&cells, true, false))
    }

    /// Reenvia sem perdas parte das áreas que foram como JPEG. Chame quando a
    /// tela estiver parada, até devolver `None`.
    pub fn refine(&mut self) -> Option<FramePacket> {
        if !self.lossy.iter().any(|&l| l) {
            return None;
        }
        let mut cells = self.cells_covering(&self.lossy);
        cells.truncate(self.cfg.refine_cells_per_pass.max(1));
        Some(self.encode_cells(&cells, false, true))
    }

    /// Há áreas aguardando refinamento?
    pub fn has_lossy(&self) -> bool {
        self.lossy.iter().any(|&l| l)
    }

    /// Para cada célula da grade que contém algum bloco marcado, a caixa
    /// envolvente dos blocos marcados dentro dela.
    fn cells_covering(&self, marked: &[bool]) -> Vec<TileRect> {
        let mut cells = Vec::new();
        for cy in (0..self.tiles_y).step_by(CELL_TILES) {
            for cx in (0..self.tiles_x).step_by(CELL_TILES) {
                let (mut x0, mut y0, mut x1, mut y1) = (usize::MAX, usize::MAX, 0, 0);
                for ty in cy..(cy + CELL_TILES).min(self.tiles_y) {
                    for tx in cx..(cx + CELL_TILES).min(self.tiles_x) {
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

    fn encode_cells(&mut self, cells: &[TileRect], keyframe: bool, force_lossless: bool) -> FramePacket {
        let (width, height) = (self.width, self.height);
        let reference = &self.reference;
        let cfg = &self.cfg;

        let patches: Vec<Patch> = cells
            .par_iter()
            .map(|cell| {
                let x = cell.tx * TILE;
                let y = cell.ty * TILE;
                let w = (cell.tw * TILE).min(width - x);
                let h = (cell.th * TILE).min(height - y);
                let (encoding, data) = encode_region(reference, width, x, y, w, h, cfg, force_lossless);
                Patch { x: x as u16, y: y as u16, w: w as u16, h: h as u16, encoding, data }
            })
            .collect();

        // Uma célula JPEG cobre também blocos que não mudaram dentro da caixa
        // envolvente; todos eles passam a precisar de refinamento.
        for (cell, patch) in cells.iter().zip(&patches) {
            let is_lossy = patch.encoding == PatchEncoding::Jpeg;
            for ty in cell.ty..cell.ty + cell.th {
                for tx in cell.tx..cell.tx + cell.tw {
                    self.lossy[ty * self.tiles_x + tx] = is_lossy;
                }
            }
        }

        let seq = self.seq;
        self.seq = self.seq.wrapping_add(1);
        FramePacket { seq, width: width as u16, height: height as u16, keyframe, patches }
    }
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
    force_lossless: bool,
) -> (PatchEncoding, Vec<u8>) {
    let rows = (0..h).map(|r| {
        let start = ((y + r) * width + x) * 4;
        &reference[start..start + w * 4]
    });

    let mut rgb = Vec::with_capacity(w * h * 3);
    for row in rows.clone() {
        for px in row.as_chunks::<4>().0 {
            rgb.extend_from_slice(&[px[2], px[1], px[0]]);
        }
    }
    let level = if force_lossless { cfg.zstd_level.max(3) } else { cfg.zstd_level };
    let lossless = zstd::bulk::compress(&rgb, level).expect("zstd em memória não falha");
    let budget = (rgb.len() as f32 * cfg.lossless_max_ratio) as usize;
    if force_lossless || lossless.len() <= budget {
        return (PatchEncoding::ZstdRgb, lossless);
    }

    let mut bgra = Vec::with_capacity(w * h * 4);
    for row in rows {
        bgra.extend_from_slice(row);
    }
    let mut jpeg = Vec::with_capacity(rgb.len() / 8);
    let encoder = jpeg_encoder::Encoder::new(&mut jpeg, cfg.jpeg_quality);
    match encoder.encode(&bgra, w as u16, h as u16, jpeg_encoder::ColorType::Bgra) {
        // Só vale a pena perder qualidade se o JPEG for de fato menor.
        Ok(()) if jpeg.len() < lossless.len() => (PatchEncoding::Jpeg, jpeg),
        _ => (PatchEncoding::ZstdRgb, lossless),
    }
}
