//! OpenDesk — aplicativo gráfico.

// Sem janela de console nas versões de distribuição.
#![cfg_attr(all(windows, not(debug_assertions)), windows_subsystem = "windows")]

mod app;
mod theme;

use eframe::egui;

fn main() -> eframe::Result {
    tracing_subscriber::fmt().with_writer(std::io::stderr).init();
    opendesk_platform::init_process();

    let options = eframe::NativeOptions {
        viewport: egui::ViewportBuilder::default()
            .with_title("OpenDesk")
            .with_icon(icon())
            .with_inner_size([960.0, 620.0])
            .with_min_inner_size([780.0, 540.0]),
        ..Default::default()
    };
    eframe::run_native("OpenDesk", options, Box::new(|cc| Ok(Box::new(app::App::new(cc)?))))
}

/// Ícone da janela, desenhado por código: um monitor branco sobre a cor de
/// destaque. Evita embutir um arquivo de imagem só para isso.
fn icon() -> egui::IconData {
    const N: u32 = 64;
    const ACCENT: [f32; 3] = [11.0, 116.0, 82.0];
    // Distância com sinal até um retângulo de cantos arredondados.
    let rounded = |x: f32, y: f32, cx: f32, cy: f32, half_w: f32, half_h: f32, r: f32| {
        let (qx, qy) = ((x - cx).abs() - (half_w - r), (y - cy).abs() - (half_h - r));
        qx.max(0.0).hypot(qy.max(0.0)) + qx.max(qy).min(0.0) - r
    };
    let coverage = |distance: f32| (0.5 - distance).clamp(0.0, 1.0);

    let mut rgba = Vec::with_capacity((N * N * 4) as usize);
    for py in 0..N {
        for px in 0..N {
            let (x, y) = (px as f32 + 0.5, py as f32 + 0.5);
            let background = coverage(rounded(x, y, 32.0, 32.0, 30.0, 30.0, 14.0));
            let frame = rounded(x, y, 32.0, 27.0, 20.0, 14.0, 4.5).max(-rounded(x, y, 32.0, 27.0, 15.5, 9.5, 1.5));
            let stand = rounded(x, y, 32.0, 48.5, 9.0, 2.2, 2.2);
            let white = coverage(frame.min(stand));
            for channel in ACCENT {
                rgba.push((channel + (255.0 - channel) * white) as u8);
            }
            rgba.push((background * 255.0) as u8);
        }
    }
    egui::IconData { rgba, width: N, height: N }
}
