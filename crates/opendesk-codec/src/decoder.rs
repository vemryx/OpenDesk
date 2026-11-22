use opendesk_proto::video::{FramePacket, Patch, PatchEncoding};
use rayon::prelude::*;
use thiserror::Error;
use zune_jpeg::JpegDecoder;
use zune_jpeg::zune_core::colorspace::ColorSpace;
use zune_jpeg::zune_core::options::DecoderOptions;

#[derive(Debug, Error)]
pub enum DecodeError {
    #[error("patch sem perdas corrompido: {0}")]
    Zstd(String),
    #[error("patch JPEG corrompido: {0}")]
    Jpeg(String),
    #[error("patch com tamanho inesperado: esperava {expected} bytes, veio {got}")]
    SizeMismatch { expected: usize, got: usize },
}

/// Retângulo decodificado em RGBA opaco, pronto para copiar no framebuffer.
#[derive(Debug, Clone)]
pub struct DecodedPatch {
    pub x: usize,
    pub y: usize,
    pub w: usize,
    pub h: usize,
    pub rgba: Vec<u8>,
}

/// Decodifica todos os patches de um pacote, em paralelo.
///
/// O pacote vem da rede: os limites geométricos já foram validados em
/// [`FramePacket::decode`], e aqui o tamanho de cada corpo decodificado é
/// conferido contra a geometria declarada antes de ser devolvido.
pub fn decode_packet(packet: &FramePacket) -> Result<Vec<DecodedPatch>, DecodeError> {
    packet.patches.par_iter().map(decode_patch).collect()
}

fn decode_patch(p: &Patch) -> Result<DecodedPatch, DecodeError> {
    let (w, h) = (p.w as usize, p.h as usize);
    let rgba = match p.encoding {
        PatchEncoding::ZstdRgb => {
            let expected = w * h * 3;
            let rgb = zstd::bulk::decompress(&p.data, expected).map_err(|e| DecodeError::Zstd(e.to_string()))?;
            if rgb.len() != expected {
                return Err(DecodeError::SizeMismatch { expected, got: rgb.len() });
            }
            let mut out = Vec::with_capacity(w * h * 4);
            for px in rgb.as_chunks::<3>().0 {
                out.extend_from_slice(&[px[0], px[1], px[2], 255]);
            }
            out
        }
        PatchEncoding::Jpeg | PatchEncoding::JpegHalf | PatchEncoding::JpegQuarter | PatchEncoding::JpegEighth => {
            // A imagem que veio pode ser menor que a região: o emissor a
            // reduziu para caber na conexão, e aqui ela é ampliada de volta.
            let scale = p.encoding.scale();
            let (small_w, small_h) = (w.div_ceil(scale), h.div_ceil(scale));
            let options = DecoderOptions::default()
                .jpeg_set_out_colorspace(ColorSpace::RGBA)
                .set_max_width(small_w)
                .set_max_height(small_h);
            let mut decoder = JpegDecoder::new_with_options(&p.data[..], options);
            let small = decoder.decode().map_err(|e| DecodeError::Jpeg(format!("{e:?}")))?;
            let expected = small_w * small_h * 4;
            if small.len() != expected {
                return Err(DecodeError::SizeMismatch { expected, got: small.len() });
            }
            if scale == 1 { small } else { enlarge(&small, small_w, small_h, w, h, scale) }
        }
    };
    Ok(DecodedPatch { x: p.x as usize, y: p.y as usize, w, h, rgba })
}

/// Amplia `scale` vezes com interpolação bilinear, para `w`×`h` pixels.
/// Cada pixel reduzido representa o centro do bloco que ele substituiu.
fn enlarge(small: &[u8], small_w: usize, small_h: usize, w: usize, h: usize, scale: usize) -> Vec<u8> {
    /// Para cada posição ampliada: os dois vizinhos reduzidos e o peso do
    /// segundo, de 0 a 256.
    fn taps(len: usize, small_len: usize, scale: usize) -> Vec<(usize, usize, u32)> {
        (0..len)
            .map(|i| {
                // Em unidades de 1/(2·scale) de pixel reduzido, para ficar em inteiros.
                let position = (2 * i + 1).saturating_sub(scale);
                let first = (position / (2 * scale)).min(small_len - 1);
                let second = (first + 1).min(small_len - 1);
                let weight = if 2 * i + 1 < scale { 0 } else { (position % (2 * scale)) * 256 / (2 * scale) };
                (first, second, weight as u32)
            })
            .collect()
    }
    let (columns, rows) = (taps(w, small_w, scale), taps(h, small_h, scale));
    let mut out = vec![255u8; w * h * 4];
    for (y, &(top, bottom, wy)) in rows.iter().enumerate() {
        let (top, bottom) = (&small[top * small_w * 4..], &small[bottom * small_w * 4..]);
        let line = &mut out[y * w * 4..(y + 1) * w * 4];
        for (pixel, &(left, right, wx)) in line.as_chunks_mut::<4>().0.iter_mut().zip(&columns) {
            for channel in 0..3 {
                let mix =
                    |row: &[u8]| row[left * 4 + channel] as u32 * (256 - wx) + row[right * 4 + channel] as u32 * wx;
                pixel[channel] = ((mix(top) * (256 - wy) + mix(bottom) * wy + (1 << 15)) >> 16) as u8;
            }
        }
    }
    out
}
