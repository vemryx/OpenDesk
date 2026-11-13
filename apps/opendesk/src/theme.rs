//! Aparência: paleta clara e escura (seguindo o sistema), tipografia, a marca
//! e os poucos componentes visuais reutilizados pelas telas.
//!
//! A direção é neutra e corporativa: cinzas levemente frios, branco suave em
//! vez de branco puro, e o azul da marca reservado para a ação principal.

use eframe::egui::{
    self, Align, Color32, CornerRadius, FontData, FontDefinitions, FontFamily, FontId, Layout, Pos2, Rect, Response,
    RichText, Sense, Stroke, StrokeKind, TextStyle, TextureHandle, TextureOptions, Theme, Vec2, epaint,
};
use std::sync::Arc;

pub const SEMIBOLD: &str = "semibold";

/// O azul da segunda tela no símbolo oficial.
const BRAND_BLUE: Color32 = Color32::from_rgb(0x01, 0x76, 0xFB);

#[derive(Clone, Copy)]
pub struct Palette {
    /// Fundo da área principal.
    pub bg: Color32,
    /// Segunda camada neutra: a coluna lateral.
    pub side: Color32,
    /// Superfícies elevadas: diálogos e barras.
    pub surface: Color32,
    pub field: Color32,
    pub border: Color32,
    pub text: Color32,
    pub muted: Color32,
    /// Preenchimento da ação principal.
    pub accent: Color32,
    pub accent_hover: Color32,
    pub accent_pressed: Color32,
    /// O azul quando é texto ou contorno sobre o fundo.
    pub accent_text: Color32,
    pub on_accent: Color32,
    pub control: Color32,
    pub control_hover: Color32,
    pub danger: Color32,
    pub ok: Color32,
    pub alert_bg: Color32,
    pub alert_text: Color32,
    /// Cor do monitor e do nome na marca: marinho no claro, branco suave no escuro.
    pub brand: Color32,
    /// Fundo atrás da tela remota.
    pub stage: Color32,
}

const LIGHT: Palette = Palette {
    bg: Color32::from_rgb(0xF1, 0xF2, 0xF4),
    side: Color32::from_rgb(0xE6, 0xE8, 0xEB),
    surface: Color32::from_rgb(0xF8, 0xF9, 0xFA),
    field: Color32::from_rgb(0xFB, 0xFB, 0xFC),
    border: Color32::from_rgb(0xCE, 0xD2, 0xD8),
    text: Color32::from_rgb(0x1C, 0x20, 0x28),
    muted: Color32::from_rgb(0x59, 0x61, 0x6D),
    accent: Color32::from_rgb(0x0A, 0x6C, 0xF0),
    accent_hover: Color32::from_rgb(0x08, 0x60, 0xD8),
    accent_pressed: Color32::from_rgb(0x07, 0x54, 0xBE),
    accent_text: Color32::from_rgb(0x0A, 0x5F, 0xD6),
    on_accent: Color32::from_rgb(0xFF, 0xFF, 0xFF),
    control: Color32::from_rgb(0xDF, 0xE2, 0xE6),
    control_hover: Color32::from_rgb(0xD3, 0xD7, 0xDD),
    danger: Color32::from_rgb(0xB4, 0x23, 0x18),
    ok: Color32::from_rgb(0x1A, 0x7F, 0x45),
    alert_bg: Color32::from_rgb(0xFB, 0xEF, 0xC9),
    alert_text: Color32::from_rgb(0x5A, 0x41, 0x00),
    brand: Color32::from_rgb(0x01, 0x21, 0x56),
    stage: Color32::from_rgb(0x1A, 0x1C, 0x1F),
};

const DARK: Palette = Palette {
    bg: Color32::from_rgb(0x25, 0x27, 0x2B),
    side: Color32::from_rgb(0x1E, 0x20, 0x23),
    surface: Color32::from_rgb(0x2C, 0x2F, 0x34),
    field: Color32::from_rgb(0x1A, 0x1C, 0x1F),
    border: Color32::from_rgb(0x3C, 0x40, 0x47),
    text: Color32::from_rgb(0xE3, 0xE5, 0xE8),
    muted: Color32::from_rgb(0x9B, 0xA1, 0xAA),
    accent: Color32::from_rgb(0x0A, 0x6C, 0xF0),
    accent_hover: Color32::from_rgb(0x08, 0x60, 0xD8),
    accent_pressed: Color32::from_rgb(0x07, 0x54, 0xBE),
    accent_text: Color32::from_rgb(0x6C, 0xAB, 0xFF),
    on_accent: Color32::from_rgb(0xFF, 0xFF, 0xFF),
    control: Color32::from_rgb(0x33, 0x37, 0x3D),
    control_hover: Color32::from_rgb(0x3E, 0x43, 0x4A),
    danger: Color32::from_rgb(0xF2, 0x76, 0x6B),
    ok: Color32::from_rgb(0x57, 0xB9, 0x6D),
    alert_bg: Color32::from_rgb(0x3A, 0x30, 0x10),
    alert_text: Color32::from_rgb(0xF1, 0xD4, 0x88),
    brand: Color32::from_rgb(0xE3, 0xE5, 0xE8),
    stage: Color32::from_rgb(0x14, 0x15, 0x17),
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
    // `OPENDESK_THEME=light|dark` fixa o tema; sem isso, vale o do sistema.
    match std::env::var("OPENDESK_THEME").as_deref() {
        Ok("light") => ctx.set_theme(Theme::Light),
        Ok("dark") => ctx.set_theme(Theme::Dark),
        _ => {}
    }

    let mut fonts = FontDefinitions::default();
    let font_dir =
        std::path::PathBuf::from(std::env::var("WINDIR").unwrap_or_else(|_| r"C:\Windows".into())).join("Fonts");
    let mut semibold_family = fonts.families.get(&FontFamily::Proportional).cloned().unwrap_or_default();
    if let Ok(bytes) = std::fs::read(font_dir.join("segoeui.ttf")) {
        fonts.font_data.insert("ui".into(), Arc::new(FontData::from_owned(bytes)));
        fonts.families.entry(FontFamily::Proportional).or_default().insert(0, "ui".into());
        semibold_family.insert(0, "ui".into());
    }
    if let Ok(bytes) = std::fs::read(font_dir.join("seguisb.ttf")) {
        fonts.font_data.insert("ui-semibold".into(), Arc::new(FontData::from_owned(bytes)));
        semibold_family.insert(0, "ui-semibold".into());
    }
    fonts.families.insert(FontFamily::Name(SEMIBOLD.into()), semibold_family);
    ctx.set_fonts(fonts);

    for (theme, p) in [(Theme::Light, LIGHT), (Theme::Dark, DARK)] {
        ctx.style_mut_of(theme, |style| {
            style.text_styles = [
                (TextStyle::Small, FontId::proportional(12.0)),
                (TextStyle::Body, FontId::proportional(13.5)),
                (TextStyle::Button, FontId::proportional(13.5)),
                (TextStyle::Heading, semibold(16.0)),
                (TextStyle::Monospace, FontId::monospace(12.5)),
            ]
            .into();
            style.spacing.item_spacing = Vec2::new(8.0, 8.0);
            style.spacing.button_padding = Vec2::new(12.0, 5.0);
            style.spacing.interact_size.y = 30.0;
            style.spacing.icon_width = 16.0;
            style.spacing.icon_spacing = 7.0;
            style.spacing.menu_margin = egui::Margin::same(14);

            let v = &mut style.visuals;
            v.panel_fill = p.bg;
            v.window_fill = p.surface;
            v.window_stroke = Stroke::new(1.0, p.border);
            v.window_corner_radius = CornerRadius::same(10);
            v.menu_corner_radius = CornerRadius::same(8);
            v.window_shadow =
                epaint::Shadow { offset: [0, 10], blur: 30, spread: 0, color: Color32::from_black_alpha(70) };
            v.popup_shadow =
                epaint::Shadow { offset: [0, 6], blur: 18, spread: 0, color: Color32::from_black_alpha(60) };
            v.extreme_bg_color = p.field;
            v.text_edit_bg_color = Some(p.field);
            v.hyperlink_color = p.accent_text;
            v.error_fg_color = p.danger;
            v.selection.bg_fill = p.accent.gamma_multiply(0.35);
            v.selection.stroke = Stroke::new(1.5, p.accent_text);

            let radius = CornerRadius::same(6);
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
            // Também é o contorno de foco pelo teclado.
            v.widgets.active.bg_stroke = Stroke::new(1.5, p.accent_text);
        });
    }
}

/// Rótulo de uma seção: pequeno e discreto, em caixa normal.
pub fn section(ui: &mut egui::Ui, p: &Palette, text: &str) -> Response {
    ui.label(RichText::new(text).font(semibold(12.5)).color(p.muted))
}

pub fn muted(text: impl Into<String>, p: &Palette) -> RichText {
    RichText::new(text).color(p.muted)
}

/// Botão da ação principal da tela. Só deve existir um por contexto.
///
/// Desenhado aqui (e não com `egui::Button`) para ter os estados próprios do
/// azul da marca: repouso, sob o ponteiro, pressionado, desabilitado e foco.
pub fn primary_button(ui: &mut egui::Ui, p: &Palette, text: &str, enabled: bool, min_width: f32) -> Response {
    let galley = ui.painter().layout_no_wrap(text.to_owned(), semibold(13.5), Color32::PLACEHOLDER);
    let size = Vec2::new((galley.size().x + 28.0).max(min_width), ui.spacing().interact_size.y);
    let (rect, response) = ui.allocate_exact_size(size, if enabled { Sense::click() } else { Sense::hover() });
    response.widget_info(|| egui::WidgetInfo::labeled(egui::Role::Button, enabled, text));
    if ui.is_rect_visible(rect) {
        let (fill, ink) = if !enabled {
            (p.control, p.muted)
        } else if response.is_pointer_button_down_on() {
            (p.accent_pressed, p.on_accent)
        } else if response.hovered() {
            (p.accent_hover, p.on_accent)
        } else {
            (p.accent, p.on_accent)
        };
        ui.painter().rect_filled(rect, CornerRadius::same(6), fill);
        if response.has_focus() {
            let ring = Stroke::new(1.5, p.accent_text);
            ui.painter().rect_stroke(rect.expand(2.0), CornerRadius::same(8), ring, StrokeKind::Outside);
        }
        ui.painter().galley(rect.center() - galley.size() / 2.0, galley, ink);
    }
    response
}

/// Ponto de estado, desenhado (sem depender de glifos de ícone).
pub fn dot(ui: &mut egui::Ui, color: Color32) {
    let (rect, _) = ui.allocate_exact_size(Vec2::splat(8.0), Sense::hover());
    ui.painter().circle_filled(rect.center(), 3.5, color);
}

/// Linha "rótulo, valor" usada nos detalhes de identidade.
pub fn detail(ui: &mut egui::Ui, p: &Palette, label: &str, value: &str) {
    ui.label(muted(label, p));
    ui.add(egui::Label::new(RichText::new(value).color(p.text)).selectable(true));
    ui.end_row();
}

/// Aviso em destaque (âmbar): usado onde a pessoa pode ser enganada.
pub fn alert(ui: &mut egui::Ui, p: &Palette, text: &str) {
    egui::Frame::new()
        .fill(p.alert_bg)
        .corner_radius(CornerRadius::same(6))
        .inner_margin(egui::Margin::symmetric(10, 8))
        .show(ui, |ui| {
            ui.set_width(ui.available_width());
            ui.label(RichText::new(text).color(p.alert_text));
        });
}

/// Contorno de foco pelo teclado, para os componentes desenhados aqui.
fn focus_ring(ui: &egui::Ui, p: &Palette, rect: Rect, radius: u8) {
    ui.painter().rect_stroke(
        rect.expand(2.0),
        CornerRadius::same(radius),
        Stroke::new(1.5, p.accent_text),
        StrokeKind::Outside,
    );
}

/// Chave liga/desliga. `label` é o nome lido por leitores de tela.
pub fn toggle(ui: &mut egui::Ui, p: &Palette, on: &mut bool, label: &str) -> Response {
    let (rect, mut response) = ui.allocate_exact_size(Vec2::new(36.0, 20.0), Sense::click());
    if response.clicked() {
        *on = !*on;
        response.mark_changed();
    }
    response.widget_info(|| egui::WidgetInfo::selected(egui::Role::Switch, true, *on, label));
    if ui.is_rect_visible(rect) {
        let t = ui.ctx().animate_bool_responsive(response.id, *on);
        let off = if response.hovered() { p.control_hover } else { p.control };
        let track = lerp_color(off, if response.hovered() { p.accent_hover } else { p.accent }, t);
        ui.painter().rect_filled(rect, CornerRadius::same(10), track);
        if t < 1.0 {
            let edge = Stroke::new(1.0, p.border.gamma_multiply(1.0 - t));
            ui.painter().rect_stroke(rect, CornerRadius::same(10), edge, StrokeKind::Inside);
        }
        let x = egui::lerp(rect.left() + 10.0..=rect.right() - 10.0, t);
        ui.painter().circle_filled(Pos2::new(x, rect.center().y), 6.5, lerp_color(p.muted, p.on_accent, t));
        if response.has_focus() {
            focus_ring(ui, p, rect, 12);
        }
    }
    response
}

fn lerp_color(a: Color32, b: Color32, t: f32) -> Color32 {
    let mix = |x: u8, y: u8| (x as f32 + (y as f32 - x as f32) * t).round() as u8;
    Color32::from_rgb(mix(a.r(), b.r()), mix(a.g(), b.g()), mix(a.b(), b.b()))
}

/// Uma configuração: título e explicação à esquerda, o controle à direita.
pub fn setting(ui: &mut egui::Ui, p: &Palette, title: &str, description: &str, control: impl FnOnce(&mut egui::Ui)) {
    ui.horizontal(|ui| {
        ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
            control(ui);
            ui.add_space(16.0);
            ui.with_layout(Layout::top_down(Align::Min), |ui| {
                ui.spacing_mut().item_spacing.y = 2.0;
                ui.label(RichText::new(title).color(p.text));
                if !description.is_empty() {
                    ui.label(muted(description, p).small());
                }
            });
        });
    });
}

/// Aba de uma barra de abas: a selecionada ganha um traço na cor de destaque.
pub fn tab(ui: &mut egui::Ui, p: &Palette, text: &str, selected: bool) -> Response {
    let galley = ui.painter().layout_no_wrap(text.to_owned(), semibold(13.5), Color32::PLACEHOLDER);
    let (rect, response) = ui.allocate_exact_size(Vec2::new(galley.size().x, 32.0), Sense::click());
    response.widget_info(|| egui::WidgetInfo::selected(egui::Role::Tab, true, selected, text));
    if ui.is_rect_visible(rect) {
        let ink = if selected || response.hovered() { p.text } else { p.muted };
        ui.painter().galley(Pos2::new(rect.left(), rect.center().y - galley.size().y / 2.0), galley, ink);
        if selected {
            ui.painter().hline(rect.x_range(), rect.bottom() - 1.0, Stroke::new(2.0, p.accent_text));
        }
        if response.has_focus() {
            focus_ring(ui, p, rect.expand2(Vec2::new(4.0, -4.0)), 6);
        }
    }
    response
}

/// Linha clicável de uma lista: título, detalhe ao lado e uma nota à direita.
pub fn list_row(ui: &mut egui::Ui, p: &Palette, width: f32, title: &str, detail: &str, trailing: &str) -> Response {
    let (rect, response) = ui.allocate_exact_size(Vec2::new(width, 34.0), Sense::click());
    response.widget_info(|| egui::WidgetInfo::labeled(egui::Role::Button, true, format!("{title}, {detail}")));
    if ui.is_rect_visible(rect) {
        if response.is_pointer_button_down_on() || response.hovered() {
            let fill = if response.is_pointer_button_down_on() { p.control_hover } else { p.control };
            ui.painter().rect_filled(rect, CornerRadius::same(6), fill);
        }
        if response.has_focus() {
            focus_ring(ui, p, rect, 8);
        }
        let painter = ui.painter().with_clip_rect(rect.shrink2(Vec2::new(10.0, 0.0)));
        let middle = |galley: &egui::Galley| rect.center().y - galley.size().y / 2.0;
        let note = painter.layout_no_wrap(trailing.to_owned(), FontId::proportional(12.0), p.muted);
        let note_x = rect.right() - 10.0 - note.size().x;
        painter.galley(Pos2::new(note_x, middle(&note)), note, p.muted);

        // O título e o detalhe não invadem a nota.
        let painter = painter.with_clip_rect(Rect::from_min_max(rect.min, Pos2::new(note_x - 12.0, rect.max.y)));
        let name = painter.layout_no_wrap(title.to_owned(), semibold(13.5), p.text);
        let detail_x = rect.left() + 10.0 + name.size().x + 10.0;
        painter.galley(Pos2::new(rect.left() + 10.0, middle(&name)), name, p.text);
        let detail = painter.layout_no_wrap(detail.to_owned(), FontId::proportional(13.5), p.muted);
        painter.galley(Pos2::new(detail_x, middle(&detail)), detail, p.muted);
    }
    response
}

// ───────────────────────────────── Marca ─────────────────────────────────

/// O símbolo oficial cabe numa caixa de 824 × 745 unidades; as medidas abaixo
/// são as mesmas de `assets/brand/make_assets.py`.
const MARK: Vec2 = Vec2::new(824.0, 745.0);

/// Desenha o símbolo: o monitor em `front` e a segunda tela, atrás, em `back`.
fn paint_mark(painter: &egui::Painter, rect: Rect, front: Color32, back: Color32) {
    let unit = rect.width() / MARK.x;
    let at = |x: f32, y: f32| rect.min + Vec2::new(x, y) * unit;
    let stroke = 80.0 * unit;

    // Tela de trás: um contorno aberto, que para antes de encostar no monitor.
    let mut points = vec![at(301.0, 162.0)];
    for (cx, cy, from) in [(371.0, 110.0, 180.0_f32), (714.0, 110.0, 270.0), (714.0, 363.0, 0.0)] {
        for step in 0..=10 {
            let angle = (from + 9.0 * step as f32).to_radians();
            points.push(at(cx + 70.0 * angle.cos(), cy + 70.0 * angle.sin()));
        }
    }
    points.push(at(686.0, 433.0));
    painter.add(epaint::PathShape::line(points, Stroke::new(stroke, back)));

    // Monitor da frente e o pé.
    let screen = Rect::from_min_max(at(40.0, 236.0), at(612.0, 608.0));
    painter.rect_stroke(screen, 65.0 * unit, Stroke::new(stroke, front), StrokeKind::Middle);
    painter.rect_filled(Rect::from_min_max(at(194.0, 685.0), at(468.0, 745.0)), 30.0 * unit, front);
}

/// A marca completa (símbolo + nome), como no arquivo oficial.
///
/// O símbolo é vetorial. O nome vem de uma máscara em tons de cinza embutida no
/// executável, reduzida para o tamanho exato em pixels da tela atual e tingida
/// com a cor do tema: nítido em qualquer escala, sem embutir uma fonte.
pub struct Logo {
    mask: Vec<u8>,
    size: [usize; 2],
    texture: Option<(usize, TextureHandle)>,
}

impl Logo {
    pub fn load() -> Self {
        let (mask, size, color) = decode_png(include_bytes!("../assets/wordmark.png"));
        assert_eq!(color, png::ColorType::Grayscale, "wordmark.png deve ser uma máscara em tons de cinza");
        Self { mask, size, texture: None }
    }

    /// Mostra a marca com o símbolo na altura `height`.
    pub fn show(&mut self, ui: &mut egui::Ui, p: &Palette, height: f32) -> Response {
        let scale = ui.ctx().pixels_per_point();
        let mark = Vec2::new(height * MARK.x / MARK.y, height);
        // Proporções do arranjo oficial: o nome tem 61% da altura do símbolo e
        // começa a 18% do topo, separado por um respiro de 14%.
        let word_h = ((height * 0.61 * scale).round() as usize).clamp(1, self.size[1]);
        let word_w = (self.size[0] * word_h + self.size[1] / 2) / self.size[1];
        let word = Vec2::new(word_w as f32, word_h as f32) / scale;
        let gap = height * 0.14;

        let (rect, response) = ui.allocate_exact_size(Vec2::new(mark.x + gap + word.x, height), Sense::hover());
        response.widget_info(|| egui::WidgetInfo::labeled(egui::Role::Label, true, "OpenDesk"));
        if !ui.is_rect_visible(rect) {
            return response;
        }
        paint_mark(ui.painter(), Rect::from_min_size(rect.min, mark), p.brand, BRAND_BLUE);

        if self.texture.as_ref().is_none_or(|(h, _)| *h != word_h) {
            let coverage = downscale(&self.mask, self.size, [word_w, word_h]);
            let pixels = coverage.into_iter().map(Color32::from_white_alpha).collect();
            let image = egui::ColorImage::new([word_w, word_h], pixels);
            self.texture = Some((word_h, ui.ctx().load_texture("marca-nome", image, TextureOptions::LINEAR)));
        }
        if let Some((_, texture)) = &self.texture {
            // Um texel por pixel: alinha à grade para não borrar.
            let snap = |v: f32| (v * scale).round() / scale;
            let min = Pos2::new(snap(rect.min.x + mark.x + gap), snap(rect.min.y + height * 0.18));
            let uv = Rect::from_min_max(Pos2::ZERO, Pos2::new(1.0, 1.0));
            ui.painter().image(texture.id(), Rect::from_min_size(min, word), uv, p.brand);
        }
        response
    }
}

/// Decodifica um PNG de 8 bits embutido no executável.
pub fn decode_png(bytes: &[u8]) -> (Vec<u8>, [usize; 2], png::ColorType) {
    let mut reader = png::Decoder::new(std::io::Cursor::new(bytes)).read_info().expect("PNG embutido válido");
    let mut pixels = vec![0; reader.output_buffer_size().expect("PNG embutido pequeno")];
    let info = reader.next_frame(&mut pixels).expect("PNG embutido válido");
    pixels.truncate(info.buffer_size());
    (pixels, [info.width as usize, info.height as usize], info.color_type)
}

/// Reduz uma imagem de um canal pela média da área de cada pixel de destino.
fn downscale(src: &[u8], [sw, sh]: [usize; 2], [dw, dh]: [usize; 2]) -> Vec<u8> {
    let mut out = Vec::with_capacity(dw * dh);
    for y in 0..dh {
        let (y0, y1) = (y * sh / dh, ((y + 1) * sh / dh).max(y * sh / dh + 1));
        for x in 0..dw {
            let (x0, x1) = (x * sw / dw, ((x + 1) * sw / dw).max(x * sw / dw + 1));
            let sum: usize =
                (y0..y1).map(|row| src[row * sw + x0..row * sw + x1].iter().map(|v| *v as usize).sum::<usize>()).sum();
            out.push((sum / ((y1 - y0) * (x1 - x0))) as u8);
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn embedded_images_decode() {
        let (mask, [w, h], color) = decode_png(include_bytes!("../assets/wordmark.png"));
        assert_eq!((color, mask.len()), (png::ColorType::Grayscale, w * h));
        let (icon, [w, h], color) = decode_png(include_bytes!("../assets/icon.png"));
        assert_eq!((color, icon.len()), (png::ColorType::Rgba, w * h * 4));
    }

    #[test]
    fn downscale_averages_each_block() {
        let src = [0, 100, 200, 200, 0, 100, 200, 200];
        assert_eq!(downscale(&src, [4, 2], [2, 1]), [50, 200]);
        // Nunca lê fora da imagem, mesmo sem redução.
        assert_eq!(downscale(&src, [4, 2], [4, 2]), src);
    }
}
