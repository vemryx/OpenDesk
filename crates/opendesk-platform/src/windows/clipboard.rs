//! Texto da área de transferência do Windows.

use anyhow::{Context, Result, bail};
use std::time::Duration;
use windows::Win32::Foundation::{GlobalFree, HANDLE, HGLOBAL};
use windows::Win32::System::DataExchange::{
    CloseClipboard, EmptyClipboard, GetClipboardData, GetClipboardSequenceNumber, OpenClipboard, SetClipboardData,
};
use windows::Win32::System::Memory::{GMEM_MOVEABLE, GlobalAlloc, GlobalLock, GlobalSize, GlobalUnlock};

/// Formato "texto Unicode" (UTF-16 terminado em zero).
const CF_UNICODETEXT: u32 = 13;

/// Muda toda vez que o conteúdo da área de transferência muda. Barato de
/// consultar: não abre a área de transferência.
pub fn sequence() -> u32 {
    unsafe { GetClipboardSequenceNumber() }
}

/// Fecha a área de transferência ao sair de escopo.
struct Open;

impl Open {
    /// Outro programa pode estar com ela aberta por um instante: tenta de novo.
    fn new() -> Result<Self> {
        for _ in 0..5 {
            if unsafe { OpenClipboard(None) }.is_ok() {
                return Ok(Self);
            }
            std::thread::sleep(Duration::from_millis(10));
        }
        bail!("a área de transferência está em uso por outro programa")
    }
}

impl Drop for Open {
    fn drop(&mut self) {
        let _ = unsafe { CloseClipboard() };
    }
}

/// O texto na área de transferência, se o conteúdo atual for texto.
pub fn text() -> Option<String> {
    let _open = Open::new().ok()?;
    unsafe {
        let handle = GetClipboardData(CF_UNICODETEXT).ok()?;
        let global = HGLOBAL(handle.0);
        let units = GlobalSize(global) / 2;
        let data = GlobalLock(global) as *const u16;
        if data.is_null() {
            return None;
        }
        let wide = std::slice::from_raw_parts(data, units);
        let len = wide.iter().position(|unit| *unit == 0).unwrap_or(units);
        let text = String::from_utf16_lossy(&wide[..len]);
        let _ = GlobalUnlock(global);
        Some(text)
    }
}

/// Põe `text` na área de transferência.
pub fn set_text(text: &str) -> Result<()> {
    let wide: Vec<u16> = text.encode_utf16().chain([0]).collect();
    let _open = Open::new()?;
    unsafe {
        EmptyClipboard().context("esvaziar a área de transferência")?;
        let global = GlobalAlloc(GMEM_MOVEABLE, wide.len() * 2).context("reservar memória")?;
        let data = GlobalLock(global) as *mut u16;
        if data.is_null() {
            let _ = GlobalFree(Some(global));
            bail!("não foi possível acessar a memória reservada");
        }
        std::ptr::copy_nonoverlapping(wide.as_ptr(), data, wide.len());
        let _ = GlobalUnlock(global);
        // Dando certo, a memória passa a ser do sistema; dando errado, é nossa.
        if let Err(err) = SetClipboardData(CF_UNICODETEXT, Some(HANDLE(global.0))) {
            let _ = GlobalFree(Some(global));
            return Err(err).context("gravar na área de transferência");
        }
    }
    Ok(())
}
