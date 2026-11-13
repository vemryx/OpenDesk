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
        PatchEncoding::Jpeg => {
            let options =
                DecoderOptions::default().jpeg_set_out_colorspace(ColorSpace::RGBA).set_max_width(w).set_max_height(h);
            let mut decoder = JpegDecoder::new_with_options(&p.data[..], options);
            let out = decoder.decode().map_err(|e| DecodeError::Jpeg(format!("{e:?}")))?;
            let expected = w * h * 4;
            if out.len() != expected {
                return Err(DecodeError::SizeMismatch { expected, got: out.len() });
            }
            out
        }
    };
    Ok(DecodedPatch { x: p.x as usize, y: p.y as usize, w, h, rgba })
}
