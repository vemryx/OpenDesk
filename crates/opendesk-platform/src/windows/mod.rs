pub mod agent;
pub mod capture;
pub mod clipboard;
pub mod input;
pub mod keyboard;
pub mod service;
pub mod system;

use windows::Win32::Foundation::{CloseHandle, HANDLE};
use windows::Win32::UI::HiDpi::{DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE_V2, SetProcessDpiAwarenessContext};

/// Um handle do Windows que se fecha sozinho.
struct Owned(HANDLE);

// Handles do núcleo valem para o processo inteiro, não para uma thread.
unsafe impl Send for Owned {}

impl Drop for Owned {
    fn drop(&mut self) {
        if !self.0.is_invalid() {
            unsafe {
                let _ = CloseHandle(self.0);
            }
        }
    }
}

/// Texto em UTF-16 com terminador nulo, como as funções `W` do Windows esperam.
fn wide(text: &str) -> Vec<u16> {
    text.encode_utf16().chain([0]).collect()
}

pub fn init_process() {
    // Falha se a ciência de DPI já foi definida (por manifesto ou pela
    // biblioteca de interface); nesse caso não há o que fazer.
    unsafe {
        let _ = SetProcessDpiAwarenessContext(DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE_V2);
    }
}
