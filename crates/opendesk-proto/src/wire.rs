//! Enquadramento: cada mensagem é `u32` little-endian com o tamanho, seguido
//! do corpo. O limite de tamanho é sempre verificado antes de alocar.

use serde::{Serialize, de::DeserializeOwned};
use std::io;
use tokio::io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt};

/// Mensagens de controle são pequenas; isto barra um par malicioso.
pub const MAX_CONTROL_FRAME: usize = 256 * 1024;
/// Um quadro 8K sem compressão caberia aqui com folga.
pub const MAX_VIDEO_FRAME: usize = 96 * 1024 * 1024;

pub async fn write_frame<W: AsyncWrite + Unpin>(w: &mut W, payload: &[u8]) -> io::Result<()> {
    let len = u32::try_from(payload.len())
        .map_err(|_| io::Error::new(io::ErrorKind::InvalidInput, "quadro grande demais"))?;
    w.write_all(&len.to_le_bytes()).await?;
    w.write_all(payload).await
}

pub async fn read_frame<R: AsyncRead + Unpin>(r: &mut R, max: usize) -> io::Result<Vec<u8>> {
    let mut len = [0u8; 4];
    r.read_exact(&mut len).await?;
    let len = u32::from_le_bytes(len) as usize;
    if len > max {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            format!("quadro de {len} bytes excede o limite de {max}"),
        ));
    }
    let mut buf = vec![0u8; len];
    r.read_exact(&mut buf).await?;
    Ok(buf)
}

pub async fn send_msg<W: AsyncWrite + Unpin, T: Serialize>(w: &mut W, msg: &T) -> io::Result<()> {
    let bytes = postcard::to_stdvec(msg).map_err(|e| io::Error::new(io::ErrorKind::InvalidInput, e))?;
    write_frame(w, &bytes).await
}

pub async fn recv_msg<R: AsyncRead + Unpin, T: DeserializeOwned>(r: &mut R) -> io::Result<T> {
    let bytes = read_frame(r, MAX_CONTROL_FRAME).await?;
    postcard::from_bytes(&bytes).map_err(|e| io::Error::new(io::ErrorKind::InvalidData, e))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::control::{InputEvent, ViewerMsg};

    #[tokio::test]
    async fn control_roundtrip() {
        let (mut a, mut b) = tokio::io::duplex(1024);
        let msg = ViewerMsg::Input(InputEvent::Key { scancode: 0x1E, extended: false, down: true });
        send_msg(&mut a, &msg).await.unwrap();
        let got: ViewerMsg = recv_msg(&mut b).await.unwrap();
        assert!(matches!(got, ViewerMsg::Input(InputEvent::Key { scancode: 0x1E, extended: false, down: true })));
    }

    #[tokio::test]
    async fn rejects_oversized_frame() {
        let (mut a, mut b) = tokio::io::duplex(64);
        a.write_all(&u32::MAX.to_le_bytes()).await.unwrap();
        let err = read_frame(&mut b, MAX_CONTROL_FRAME).await.unwrap_err();
        assert_eq!(err.kind(), io::ErrorKind::InvalidData);
    }
}
