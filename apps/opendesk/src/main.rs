//! OpenDesk — aplicativo gráfico.

// Sem janela de console nas versões de distribuição.
#![cfg_attr(all(windows, not(debug_assertions)), windows_subsystem = "windows")]

mod app;
mod config;
mod theme;

use eframe::egui;

fn main() -> eframe::Result {
    let args: Vec<String> = std::env::args().skip(1).collect();
    if let Some(code) = background_mode(&args) {
        std::process::exit(code);
    }
    tracing_subscriber::fmt().with_writer(std::io::stderr).init();
    opendesk_platform::init_process();

    let mut viewport = egui::ViewportBuilder::default()
        .with_title("OpenDesk")
        .with_icon(icon())
        .with_inner_size(app::HOME_SIZE)
        .with_min_inner_size([720.0, 460.0]);
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

/// Os modos sem janela do mesmo executável: o serviço do sistema, o agente
/// que ele mantém, e a instalação e a remoção do serviço (que o aplicativo
/// dispara pedindo permissão de administrador). `None` para o uso normal.
fn background_mode(args: &[String]) -> Option<i32> {
    use opendesk_platform::{agent, service};
    type Job = Box<dyn FnOnce() -> anyhow::Result<()>>;
    let (job, interactive): (Job, bool) = match args.first()?.as_str() {
        "--service" => (Box::new(service::run), false),
        "--agent" => {
            let parent = args.iter().position(|arg| arg == "--parent").and_then(|at| args.get(at + 1));
            let parent = parent.and_then(|pid| pid.parse().ok());
            (Box::new(move || agent::run(parent)), false)
        }
        "--install-service" => (Box::new(service::install), true),
        "--uninstall-service" => (Box::new(service::uninstall), true),
        _ => return None,
    };
    log_to_file(service::log_path());
    match job() {
        Ok(()) => Some(0),
        Err(err) => {
            tracing::error!("{err:#}");
            // Quem pediu a instalação está olhando para a tela e não tem
            // como ler a saída deste processo.
            if interactive {
                rfd::MessageDialog::new()
                    .set_level(rfd::MessageLevel::Error)
                    .set_title("OpenDesk")
                    .set_description(format!("{err:#}"))
                    .show();
            }
            Some(1)
        }
    }
}

/// O serviço e o agente não têm console: registram num arquivo ao lado do
/// executável instalado, numa pasta em que só administradores escrevem.
fn log_to_file(path: Option<std::path::PathBuf>) {
    const LIMIT: u64 = 512 * 1024;
    let file = path.and_then(|path| {
        if std::fs::metadata(&path).is_ok_and(|meta| meta.len() > LIMIT) {
            let _ = std::fs::remove_file(&path);
        }
        std::fs::File::options().create(true).append(true).open(path).ok()
    });
    match file {
        Some(file) => tracing_subscriber::fmt().with_ansi(false).with_writer(std::sync::Mutex::new(file)).init(),
        None => tracing_subscriber::fmt().with_writer(std::io::stderr).init(),
    }
}

/// Ícone da janela: o oficial, embutido no executável (o mesmo desenho do
/// `icon.ico` que o Windows mostra para o arquivo).
fn icon() -> egui::IconData {
    let (rgba, [width, height], _) = theme::decode_png(include_bytes!("../assets/icon.png"));
    egui::IconData { rgba, width: width as u32, height: height as u32 }
}
