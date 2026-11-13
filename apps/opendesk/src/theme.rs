//! Aparência: paleta clara e escura (seguindo o sistema), tipografia e os
//! poucos componentes visuais reutilizados pelas telas.

use eframe::egui::{
    self, Color32, CornerRadius, FontData, FontDefinitions, FontFamily, FontId, Margin, RichText, Stroke, TextStyle,
    Theme, Vec2,
};
use std::sync::Arc;

pub const SEMIBOLD: &str = "semibold";

#[derive(Clone, Copy)]
pub struct Palette {
    pub bg: Color32,
    pub surface: Color32,
    pub field: Color32,
    pub border: Color32,
    pub text: Color32,
    pub muted: Color32,
    pub accent: Color32,
    pub on_accent: Color32,
    pub control: Color32,
    pub control_hover: Color32,
    pub danger: Color32,
    pub alert_bg: Color32,
    pub alert_text: Color32,
}

const LIGHT: Palette = Palette {
    bg: Color32::from_rgb(0xF3, 0xF4, 0xF1),
    surface: Color32::from_rgb(0xFF, 0xFF, 0xFF),
    field: Color32::from_rgb(0xFA, 0xFB, 0xF9),
    border: Color32::from_rgb(0xDD, 0xE1, 0xDB),
    text: Color32::from_rgb(0x15, 0x1A, 0x17),
    muted: Color32::from_rgb(0x58, 0x62, 0x5C),
    accent: Color32::from_rgb(0x0B, 0x74, 0x52),
    on_accent: Color32::from_rgb(0xFF, 0xFF, 0xFF),
    control: Color32::from_rgb(0xEA, 0xED, 0xE8),
    control_hover: Color32::from_rgb(0xDF, 0xE3, 0xDC),
    danger: Color32::from_rgb(0xB4, 0x23, 0x18),
    alert_bg: Color32::from_rgb(0xFF, 0xF1, 0xCC),
    alert_text: Color32::from_rgb(0x5C, 0x3F, 0x00),
};

const DARK: Palette = Palette {
    bg: Color32::from_rgb(0x10, 0x13, 0x12),
    surface: Color32::from_rgb(0x19, 0x1D, 0x1B),
    field: Color32::from_rgb(0x12, 0x15, 0x14),
    border: Color32::from_rgb(0x2B, 0x31, 0x2E),
    text: Color32::from_rgb(0xEC, 0xEF, 0xEA),
    muted: Color32::from_rgb(0x9C, 0xA7, 0xA1),
    accent: Color32::from_rgb(0x3D, 0xD9, 0xA0),
    on_accent: Color32::from_rgb(0x04, 0x24, 0x18),
    control: Color32::from_rgb(0x26, 0x2B, 0x28),
    control_hover: Color32::from_rgb(0x30, 0x36, 0x33),
    danger: Color32::from_rgb(0xF9, 0x70, 0x66),
    alert_bg: Color32::from_rgb(0x3B, 0x2E, 0x08),
    alert_text: Color32::from_rgb(0xF6, 0xD9, 0x8B),
};

impl Palette {
    pub fn of(ctx: &egui::Context) -> Self {
        match ctx.theme() {
            Theme::Dark => DARK,
            Theme::Light => LIGHT,
        }
    }
}

pub fn semibold(size: f32) -> FontId {
    FontId::new(size, FontFamily::Name(SEMIBOLD.into()))
}

/// Instala fontes e estilos. No Windows usa a Segoe UI do sistema, para o app
/// parecer nativo sem embutir fontes; em outros sistemas fica a fonte padrão.
pub fn install(ctx: &egui::Context) {
    let mut fonts = FontDefinitions::default();
    let font_dir =
        std::path::PathBuf::from(std::env::var("WINDIR").unwrap_or_else(|_| r"C:\Windows".into())).join("Fonts");
    let mut semibold = fonts.families.get(&FontFamily::Proportional).cloned().unwrap_or_default();
    if let Ok(bytes) = std::fs::read(font_dir.join("segoeui.ttf")) {
        fonts.font_data.insert("ui".into(), Arc::new(FontData::from_owned(bytes)));
        fonts.families.entry(FontFamily::Proportional).or_default().insert(0, "ui".into());
        semibold.insert(0, "ui".into());
    }
    if let Ok(bytes) = std::fs::read(font_dir.join("seguisb.ttf")) {
        fonts.font_data.insert("ui-semibold".into(), Arc::new(FontData::from_owned(bytes)));
        semibold.insert(0, "ui-semibold".into());
    }
    fonts.families.insert(FontFamily::Name(SEMIBOLD.into()), semibold);
    ctx.set_fonts(fonts);

    for (theme, p) in [(Theme::Light, LIGHT), (Theme::Dark, DARK)] {
        ctx.style_mut_of(theme, |style| {
            style.text_styles = [
                (TextStyle::Small, FontId::proportional(12.5)),
                (TextStyle::Body, FontId::proportional(14.5)),
                (TextStyle::Button, FontId::proportional(14.5)),
                (TextStyle::Heading, semibold_id(21.0)),
                (TextStyle::Monospace, FontId::monospace(13.5)),
            ]
            .into();
            style.spacing.item_spacing = Vec2::new(10.0, 10.0);
            style.spacing.button_padding = Vec2::new(16.0, 8.0);
            style.spacing.interact_size.y = 36.0;
            style.spacing.icon_width = 18.0;

            let v = &mut style.visuals;
            v.panel_fill = p.bg;
            v.window_fill = p.surface;
            v.window_stroke = Stroke::new(1.0, p.border);
            v.window_corner_radius = CornerRadius::same(14);
            v.extreme_bg_color = p.field;
            v.text_edit_bg_color = Some(p.field);
            v.hyperlink_color = p.accent;
            v.error_fg_color = p.danger;
            v.selection.bg_fill = p.accent.gamma_multiply(0.30);
            v.selection.stroke = Stroke::new(1.5, p.accent);

            let radius = CornerRadius::same(8);
            v.widgets.noninteractive.bg_stroke = Stroke::new(1.0, p.border);
            v.widgets.noninteractive.fg_stroke = Stroke::new(1.0, p.text);
            v.widgets.noninteractive.corner_radius = radius;
            for (widget, fill) in [
                (&mut v.widgets.inactive, p.control),
                (&mut v.widgets.hovered, p.control_hover),
                (&mut v.widgets.active, p.control_hover),
                (&mut v.widgets.open, p.control_hover),
            ] {
                widget.bg_fill = fill;
                widget.weak_bg_fill = fill;
                widget.bg_stroke = Stroke::new(1.0, p.border);
                widget.fg_stroke = Stroke::new(1.0, p.text);
                widget.corner_radius = radius;
                widget.expansion = 0.0;
            }
            v.widgets.hovered.bg_stroke = Stroke::new(1.0, p.muted);
            v.widgets.active.bg_stroke = Stroke::new(1.0, p.accent);
        });
    }
}

fn semibold_id(size: f32) -> FontId {
    semibold(size)
}

/// Rótulo de seção: pequeno, em maiúsculas, discreto.
pub fn caption(ui: &mut egui::Ui, p: &Palette, text: &str) {
    ui.label(RichText::new(text).font(semibold(11.5)).color(p.muted).extra_letter_spacing(0.9));
}

pub fn muted(text: impl Into<String>, p: &Palette) -> RichText {
    RichText::new(text).color(p.muted)
}

/// Superfície elevada onde vive um grupo de conteúdo.
pub fn card(p: &Palette) -> egui::Frame {
    egui::Frame::new()
        .fill(p.surface)
        .stroke(Stroke::new(1.0, p.border))
        .corner_radius(CornerRadius::same(14))
        .inner_margin(Margin::same(24))
}

/// Botão da ação principal da tela. Só deve existir um por contexto.
pub fn primary_button<'a>(p: &Palette, text: &'a str) -> egui::Button<'a> {
    egui::Button::new(RichText::new(text).font(semibold(14.5)).color(p.on_accent))
        .fill(p.accent)
        .stroke(Stroke::NONE)
        .min_size(Vec2::new(0.0, 38.0))
}

/// Ponto de estado, desenhado (sem depender de glifos de ícone).
pub fn dot(ui: &mut egui::Ui, color: Color32) {
    let (rect, _) = ui.allocate_exact_size(Vec2::splat(10.0), egui::Sense::hover());
    ui.painter().circle_filled(rect.center(), 4.0, color);
}

/// Linha "rótulo — valor" usada nos detalhes de identidade.
pub fn detail(ui: &mut egui::Ui, p: &Palette, label: &str, value: &str) {
    ui.label(muted(label, p));
    ui.add(egui::Label::new(RichText::new(value).color(p.text)).selectable(true));
    ui.end_row();
}
