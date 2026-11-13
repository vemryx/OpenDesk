//! Pacote de vídeo: um conjunto de retângulos ("patches") a aplicar sobre o
//! quadro anterior. O formato é binário e escrito à mão porque este é o
//! caminho quente e os corpos são blobs grandes.
//!
//! ```text
//! seq u32 | width u16 | height u16 | flags u8 | n u16
//! n × ( x u16 | y u16 | w u16 | h u16 | enc u8 | len u32 | data[len] )
//! ```

use thiserror::Error;

const HEADER_LEN: usize = 4 + 2 + 2 + 1 + 2;
const PATCH_HEADER_LEN: usize = 2 * 4 + 1 + 4;
const FLAG_KEYFRAME: u8 = 1;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[repr(u8)]
pub enum PatchEncoding {
    /// RGB de 24 bits comprimido com zstd. Sem perdas.
    ZstdRgb = 0,
    /// JPEG baseline. Com perdas; o host reenvia sem perdas quando a tela para.
    Jpeg = 1,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Patch {
    pub x: u16,
    pub y: u16,
    pub w: u16,
    pub h: u16,
    pub encoding: PatchEncoding,
    pub data: Vec<u8>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FramePacket {
    pub seq: u32,
    pub width: u16,
    pub height: u16,
    /// Verdadeiro quando os patches cobrem a tela inteira.
    pub keyframe: bool,
    pub patches: Vec<Patch>,
}

#[derive(Debug, Error, PartialEq, Eq)]
pub enum DecodeError {
    #[error("pacote de vídeo truncado")]
    Truncated,
    #[error("codificação de patch desconhecida: {0}")]
    UnknownEncoding(u8),
    #[error("patch fora dos limites do quadro")]
    OutOfBounds,
    #[error("dimensões de quadro inválidas")]
    BadDimensions,
}

impl FramePacket {
    pub fn encode(&self) -> Vec<u8> {
        let body: usize = self.patches.iter().map(|p| PATCH_HEADER_LEN + p.data.len()).sum();
        let mut out = Vec::with_capacity(HEADER_LEN + body);
        out.extend_from_slice(&self.seq.to_le_bytes());
        out.extend_from_slice(&self.width.to_le_bytes());
        out.extend_from_slice(&self.height.to_le_bytes());
        out.push(if self.keyframe { FLAG_KEYFRAME } else { 0 });
        out.extend_from_slice(&(self.patches.len() as u16).to_le_bytes());
        for p in &self.patches {
            out.extend_from_slice(&p.x.to_le_bytes());
            out.extend_from_slice(&p.y.to_le_bytes());
            out.extend_from_slice(&p.w.to_le_bytes());
            out.extend_from_slice(&p.h.to_le_bytes());
            out.push(p.encoding as u8);
            out.extend_from_slice(&(p.data.len() as u32).to_le_bytes());
            out.extend_from_slice(&p.data);
        }
        out
    }

    pub fn decode(buf: &[u8]) -> Result<Self, DecodeError> {
        let mut r = Reader { buf };
        let seq = r.u32()?;
        let width = r.u16()?;
        let height = r.u16()?;
        if width == 0 || height == 0 {
            return Err(DecodeError::BadDimensions);
        }
        let flags = r.u8()?;
        let count = r.u16()? as usize;
        let mut patches = Vec::with_capacity(count.min(4096));
        for _ in 0..count {
            let (x, y, w, h) = (r.u16()?, r.u16()?, r.u16()?, r.u16()?);
            let encoding = match r.u8()? {
                0 => PatchEncoding::ZstdRgb,
                1 => PatchEncoding::Jpeg,
                other => return Err(DecodeError::UnknownEncoding(other)),
            };
            let len = r.u32()? as usize;
            let data = r.bytes(len)?.to_vec();
            let fits = w > 0 && h > 0 && x as u32 + w as u32 <= width as u32 && y as u32 + h as u32 <= height as u32;
            if !fits {
                return Err(DecodeError::OutOfBounds);
            }
            patches.push(Patch { x, y, w, h, encoding, data });
        }
        Ok(Self { seq, width, height, keyframe: flags & FLAG_KEYFRAME != 0, patches })
    }
}

struct Reader<'a> {
    buf: &'a [u8],
}

impl<'a> Reader<'a> {
    fn bytes(&mut self, n: usize) -> Result<&'a [u8], DecodeError> {
        if self.buf.len() < n {
            return Err(DecodeError::Truncated);
        }
        let (head, tail) = self.buf.split_at(n);
        self.buf = tail;
        Ok(head)
    }
    fn u8(&mut self) -> Result<u8, DecodeError> {
        Ok(self.bytes(1)?[0])
    }
    fn u16(&mut self) -> Result<u16, DecodeError> {
        Ok(u16::from_le_bytes(self.bytes(2)?.try_into().unwrap()))
    }
    fn u32(&mut self) -> Result<u32, DecodeError> {
        Ok(u32::from_le_bytes(self.bytes(4)?.try_into().unwrap()))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sample() -> FramePacket {
        FramePacket {
            seq: 7,
            width: 1920,
            height: 1080,
            keyframe: true,
            patches: vec![
                Patch { x: 0, y: 0, w: 256, h: 256, encoding: PatchEncoding::ZstdRgb, data: vec![1, 2, 3] },
                Patch { x: 1664, y: 824, w: 256, h: 256, encoding: PatchEncoding::Jpeg, data: vec![9; 40] },
            ],
        }
    }

    #[test]
    fn roundtrip() {
        let pkt = sample();
        assert_eq!(FramePacket::decode(&pkt.encode()).unwrap(), pkt);
    }

    #[test]
    fn truncated_is_rejected_at_every_length() {
        let bytes = sample().encode();
        for cut in 0..bytes.len() {
            assert_eq!(FramePacket::decode(&bytes[..cut]), Err(DecodeError::Truncated), "corte {cut}");
        }
    }

    #[test]
    fn out_of_bounds_patch_is_rejected() {
        let mut pkt = sample();
        pkt.patches[1].x = 1700;
        assert_eq!(FramePacket::decode(&pkt.encode()), Err(DecodeError::OutOfBounds));
    }
}
