pub mod capture;
pub mod input;
pub mod keyboard;

use windows::Win32::UI::HiDpi::{DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE_V2, SetProcessDpiAwarenessContext};

pub fn init_process() {
    // Falha se a ciência de DPI já foi definida (por manifesto ou pela
    // biblioteca de interface); nesse caso não há o que fazer.
    unsafe {
        let _ = SetProcessDpiAwarenessContext(DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE_V2);
    }
}
