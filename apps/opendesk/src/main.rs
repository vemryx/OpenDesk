//! OpenDesk — aplicativo gráfico.

// Sem janela de console nas versões de distribuição.
#![cfg_attr(all(windows, not(debug_assertions)), windows_subsystem = "windows")]

mod app;
mod theme;

use eframe::egui;

fn main() -> eframe::Result {
    tracing_subscriber::fmt().with_writer(std::io::stderr).init();
    opendesk_platform::init_process();

    let mut viewport = egui::ViewportBuilder::default()
        .with_title("OpenDesk")
        .with_icon(icon())
        .with_inner_size(app::HOME_SIZE)
        .with_min_inner_size([640.0, 340.0]);
    // Para desenvolvimento e testes: `OPENDESK_WINDOW_POS=x,y` abre a janela
    // nessa posição e sem tomar o foco de quem está usando o computador.
    let position = std::env::var("OPENDESK_WINDOW_POS").ok().and_then(|value| {
        let (x, y) = value.split_once(',')?;
        Some([x.trim().parse::<f32>().ok()?, y.trim().parse::<f32>().ok()?])
    });
    if let Some(position) = position {
        viewport = viewport.with_position(position).with_active(false);
    }
    let options = eframe::NativeOptions { viewport, ..Default::default() };
    eframe::run_native("OpenDesk", options, Box::new(|cc| Ok(Box::new(app::App::new(cc)?))))
}

/// Ícone da janela: o oficial, embutido no executável (o mesmo desenho do
/// `icon.ico` que o Windows mostra para o arquivo).
fn icon() -> egui::IconData {
    let (rgba, [width, height], _) = theme::decode_png(include_bytes!("../assets/icon.png"));
    egui::IconData { rgba, width: width as u32, height: height as u32 }
}
