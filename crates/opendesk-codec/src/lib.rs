//! Codec de tela "tiles v1".
//!
//! A tela é dividida em blocos de [`TILE`]×[`TILE`] pixels. A cada quadro o
//! codificador compara os blocos com a referência (o último quadro enviado) e
//! só transmite o que mudou, agrupado em células de até
//! [`CELL_TILES`]×[`CELL_TILES`] blocos.
//!
//! Cada célula é comprimida primeiro sem perdas (RGB + zstd). Interface, texto
//! e cores chapadas comprimem muito bem assim e chegam perfeitos. Se a célula
//! não comprimir (foto, vídeo, gradiente com ruído), ela vai como JPEG e fica
//! marcada como "com perdas"; quando a tela para de mudar, [`Encoder::refine`]
//! reenvia essas áreas sem perdas. O resultado é imagem nítida em repouso e
//! banda baixa em movimento.
//!
//! O codificador não depende de quadros anteriores do lado receptor além do
//! que ele mesmo já enviou, então o emissor pode pular quadros à vontade
//! quando a rede não dá conta: a diferença é sempre contra o último enviado.

mod decoder;
mod encoder;

pub use decoder::{DecodeError, DecodedPatch, decode_packet};
pub use encoder::{Encoder, EncoderConfig, FrameRef};

/// Lado do bloco de comparação, em pixels.
pub const TILE: usize = 64;
/// Lado máximo de uma célula de codificação, em blocos.
pub const CELL_TILES: usize = 4;

#[cfg(test)]
mod tests {
    use super::*;
    use opendesk_proto::video::PatchEncoding;

    /// Quadro BGRA sintético que parece interface: faixas chapadas.
    fn ui_frame(w: usize, h: usize) -> Vec<u8> {
        let mut px = vec![0u8; w * h * 4];
        for y in 0..h {
            for x in 0..w {
                let i = (y * w + x) * 4;
                let band = ((x / 97) + (y / 53)) as u8;
                px[i] = band.wrapping_mul(31);
                px[i + 1] = band.wrapping_mul(17);
                px[i + 2] = band.wrapping_mul(7);
                px[i + 3] = 255;
            }
        }
        px
    }

    /// Ruído determinístico: o pior caso para compressão sem perdas.
    fn noise_frame(w: usize, h: usize) -> Vec<u8> {
        let mut state = 0x9E37_79B9_7F4A_7C15u64;
        let mut px = vec![255u8; w * h * 4];
        for chunk in px.as_chunks_mut::<4>().0 {
            state ^= state << 13;
            state ^= state >> 7;
            state ^= state << 17;
            chunk[..3].copy_from_slice(&state.to_le_bytes()[..3]);
        }
        px
    }

    fn apply(fb: &mut [u8], width: usize, patches: &[DecodedPatch]) {
        for p in patches {
            for row in 0..p.h {
                let dst = ((p.y + row) * width + p.x) * 4;
                let src = row * p.w * 4;
                fb[dst..dst + p.w * 4].copy_from_slice(&p.rgba[src..src + p.w * 4]);
            }
        }
    }

    fn bgra_to_rgba(bgra: &[u8]) -> Vec<u8> {
        bgra.as_chunks::<4>().0.iter().flat_map(|p| [p[2], p[1], p[0], 255]).collect()
    }

    fn frame(data: &[u8], width: usize, height: usize) -> FrameRef<'_> {
        FrameRef { data, width, height, stride: width * 4 }
    }

    #[test]
    fn keyframe_roundtrips_losslessly_for_ui_content() {
        // Dimensões que não são múltiplas do bloco, de propósito.
        let (w, h) = (700, 410);
        let src = ui_frame(w, h);
        let mut enc = Encoder::new(EncoderConfig::default());
        let pkt = enc.encode(&frame(&src, w, h)).expect("primeiro quadro é sempre enviado");
        assert!(pkt.keyframe);
        assert!(pkt.patches.iter().all(|p| p.encoding == PatchEncoding::ZstdRgb));

        let mut fb = vec![0u8; w * h * 4];
        apply(&mut fb, w, &decode_packet(&pkt).unwrap());
        assert_eq!(fb, bgra_to_rgba(&src));
    }

    #[test]
    fn unchanged_frame_produces_nothing() {
        let (w, h) = (640, 360);
        let src = ui_frame(w, h);
        let mut enc = Encoder::new(EncoderConfig::default());
        assert!(enc.encode(&frame(&src, w, h)).is_some());
        assert!(enc.encode(&frame(&src, w, h)).is_none());
    }

    #[test]
    fn small_change_sends_only_the_touched_tile() {
        let (w, h) = (640, 360);
        let mut src = ui_frame(w, h);
        let mut enc = Encoder::new(EncoderConfig::default());
        enc.encode(&frame(&src, w, h)).unwrap();

        // Um pixel dentro do bloco (2, 1).
        let (x, y) = (2 * TILE + 5, TILE + 9);
        src[(y * w + x) * 4] ^= 0xFF;
        let pkt = enc.encode(&frame(&src, w, h)).unwrap();
        assert!(!pkt.keyframe);
        assert_eq!(pkt.patches.len(), 1);
        let p = &pkt.patches[0];
        assert_eq!((p.x as usize, p.y as usize, p.w as usize, p.h as usize), (2 * TILE, TILE, TILE, TILE));
    }

    #[test]
    fn stride_padding_is_ignored() {
        let (w, h) = (130, 70);
        let tight = ui_frame(w, h);
        let stride = w * 4 + 24;
        let mut padded = vec![0xABu8; stride * h];
        for y in 0..h {
            padded[y * stride..y * stride + w * 4].copy_from_slice(&tight[y * w * 4..(y + 1) * w * 4]);
        }
        let mut enc = Encoder::new(EncoderConfig::default());
        let pkt = enc.encode(&FrameRef { data: &padded, width: w, height: h, stride }).unwrap();
        let mut fb = vec![0u8; w * h * 4];
        apply(&mut fb, w, &decode_packet(&pkt).unwrap());
        assert_eq!(fb, bgra_to_rgba(&tight));
    }

    #[test]
    fn noisy_content_goes_lossy_then_refines_to_exact() {
        let (w, h) = (512, 256);
        let src = noise_frame(w, h);
        let mut enc = Encoder::new(EncoderConfig::default());
        let pkt = enc.encode(&frame(&src, w, h)).unwrap();
        assert!(pkt.patches.iter().all(|p| p.encoding == PatchEncoding::Jpeg));
        let lossy_bytes: usize = pkt.patches.iter().map(|p| p.data.len()).sum();
        assert!(lossy_bytes < w * h * 3, "JPEG deve ser menor que o RGB cru");

        let mut fb = vec![0u8; w * h * 4];
        apply(&mut fb, w, &decode_packet(&pkt).unwrap());
        assert_ne!(fb, bgra_to_rgba(&src), "JPEG de ruído não pode ser exato");

        // Tela parada: os refinamentos devem convergir para o quadro exato.
        let mut rounds = 0;
        while let Some(pkt) = enc.refine() {
            assert!(pkt.patches.iter().all(|p| p.encoding == PatchEncoding::ZstdRgb));
            apply(&mut fb, w, &decode_packet(&pkt).unwrap());
            rounds += 1;
            assert!(rounds < 100, "refinamento não termina");
        }
        assert!(rounds > 0);
        assert_eq!(fb, bgra_to_rgba(&src));
    }

    #[test]
    fn resize_forces_a_keyframe() {
        let mut enc = Encoder::new(EncoderConfig::default());
        let a = ui_frame(320, 200);
        enc.encode(&frame(&a, 320, 200)).unwrap();
        let b = ui_frame(400, 240);
        let pkt = enc.encode(&frame(&b, 400, 240)).unwrap();
        assert!(pkt.keyframe);
        assert_eq!((pkt.width, pkt.height), (400, 240));
    }

    #[test]
    fn forced_keyframe_resends_everything_without_new_capture() {
        let (w, h) = (300, 200);
        let src = ui_frame(w, h);
        let mut enc = Encoder::new(EncoderConfig::default());
        enc.encode(&frame(&src, w, h)).unwrap();
        let pkt = enc.keyframe().expect("há referência");
        assert!(pkt.keyframe);
        let mut fb = vec![0u8; w * h * 4];
        apply(&mut fb, w, &decode_packet(&pkt).unwrap());
        assert_eq!(fb, bgra_to_rgba(&src));
    }

    #[test]
    fn decoder_rejects_garbage_payloads() {
        use opendesk_proto::video::{FramePacket, Patch};
        for encoding in [PatchEncoding::ZstdRgb, PatchEncoding::Jpeg] {
            let pkt = FramePacket {
                seq: 0,
                width: 64,
                height: 64,
                keyframe: false,
                patches: vec![Patch { x: 0, y: 0, w: 64, h: 64, encoding, data: vec![0x5A; 100] }],
            };
            assert!(decode_packet(&pkt).is_err());
        }
    }
}
