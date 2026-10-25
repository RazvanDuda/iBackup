//! The look of the desktop window: one palette, one typography scale and a
//! handful of reusable widgets (cards, pills, buttons, choice rows).
//!
//! Everything the window draws goes through this module, so `ibackup ui` keeps
//! a single, calm visual language instead of ad-hoc colours and spacing at each
//! call site. It is a plain egui style + helper functions, no extra crates.

use eframe::egui::{
    self, Color32, InnerResponse, Margin, Response, RichText, Rounding, Shadow, Stroke, Ui, Vec2,
};

// ---------------------------------------------------------------------------
// Palette — a dark, slightly blue-grey shell with one strong accent.
// ---------------------------------------------------------------------------

/// Window canvas behind the cards.
pub const BG: Color32 = Color32::from_rgb(0x0C, 0x10, 0x15);
/// Side, top and bottom panels (a touch deeper than the canvas).
pub const PANEL: Color32 = Color32::from_rgb(0x0A, 0x0D, 0x12);
/// Cards.
pub const SURFACE: Color32 = Color32::from_rgb(0x15, 0x1B, 0x24);
/// Inputs and unselected choice rows.
pub const SURFACE_HI: Color32 = Color32::from_rgb(0x1B, 0x22, 0x2D);
/// Hover state for inputs and choice rows.
pub const SURFACE_HOVER: Color32 = Color32::from_rgb(0x21, 0x2A, 0x36);
pub const BORDER: Color32 = Color32::from_rgb(0x23, 0x2B, 0x37);
pub const BORDER_HI: Color32 = Color32::from_rgb(0x33, 0x3E, 0x4D);

pub const TEXT: Color32 = Color32::from_rgb(0xE8, 0xED, 0xF4);
pub const TEXT_DIM: Color32 = Color32::from_rgb(0x8A, 0x96, 0xA8);

pub const ACCENT: Color32 = Color32::from_rgb(0x4C, 0x8D, 0xFF);
pub const ACCENT_HOVER: Color32 = Color32::from_rgb(0x6B, 0xA2, 0xFF);
pub const ACCENT_DOWN: Color32 = Color32::from_rgb(0x3A, 0x74, 0xDB);
pub const ACCENT_SOFT: Color32 = Color32::from_rgb(0x18, 0x25, 0x3D);
pub const ACCENT_SOFT_HI: Color32 = Color32::from_rgb(0x1D, 0x2D, 0x4A);

pub const SUCCESS: Color32 = Color32::from_rgb(0x3E, 0xD5, 0x8F);
pub const WARN: Color32 = Color32::from_rgb(0xF2, 0xB8, 0x4B);
pub const DANGER: Color32 = Color32::from_rgb(0xF8, 0x71, 0x71);

/// A translucent version of `color`, for tinted backgrounds and borders.
pub fn tint(color: Color32, alpha: u8) -> Color32 {
    Color32::from_rgba_unmultiplied(color.r(), color.g(), color.b(), alpha)
}

// ---------------------------------------------------------------------------
// Global style
// ---------------------------------------------------------------------------

/// Install the palette and metrics on an egui context (called once at startup).
pub fn install(ctx: &egui::Context) {
    let mut style = (*ctx.style()).clone();
    style.visuals = visuals();
    style.animation_time = 0.12;
    style.interaction.selectable_labels = false;

    style.spacing.item_spacing = Vec2::new(8.0, 8.0);
    style.spacing.button_padding = Vec2::new(12.0, 6.0);
    style.spacing.interact_size = Vec2::new(36.0, 26.0);
    style.spacing.indent = 22.0;
    style.spacing.window_margin = Margin::same(16.0);
    style.spacing.menu_margin = Margin::same(6.0);
    style.spacing.combo_width = 220.0;
    style.spacing.text_edit_width = 220.0;
    style.spacing.scroll.floating = true;
    style.spacing.scroll.foreground_color = true;
    style.spacing.scroll.bar_width = 8.0;
    style.spacing.scroll.floating_width = 7.0;
    style.spacing.scroll.handle_min_length = 28.0;

    style.text_styles = [
        (egui::TextStyle::Small, egui::FontId::proportional(11.0)),
        (egui::TextStyle::Body, egui::FontId::proportional(13.0)),
        (egui::TextStyle::Button, egui::FontId::proportional(13.0)),
        (egui::TextStyle::Monospace, egui::FontId::monospace(11.5)),
        (egui::TextStyle::Heading, egui::FontId::proportional(20.0)),
    ]
    .into();

    ctx.set_style(style);
}

fn visuals() -> egui::Visuals {
    let mut v = egui::Visuals::dark();
    v.dark_mode = true;
    v.panel_fill = BG;
    v.window_fill = SURFACE;
    v.window_stroke = Stroke::new(1.0_f32, BORDER);
    v.window_rounding = Rounding::same(14.0);
    v.menu_rounding = Rounding::same(10.0);
    v.window_shadow = Shadow {
        offset: Vec2::new(0.0, 10.0),
        blur: 30.0,
        spread: 0.0,
        color: Color32::from_black_alpha(140),
    };
    v.popup_shadow = v.window_shadow;
    v.extreme_bg_color = Color32::from_rgb(0x0E, 0x13, 0x1A);
    v.faint_bg_color = SURFACE_HI;
    v.code_bg_color = Color32::from_rgb(0x11, 0x16, 0x1D);
    v.override_text_color = Some(TEXT);
    v.selection.bg_fill = ACCENT;
    v.selection.stroke = Stroke::new(1.0_f32, Color32::WHITE);
    v.hyperlink_color = ACCENT;
    v.warn_fg_color = WARN;
    v.error_fg_color = DANGER;
    v.indent_has_left_vline = false;
    v.collapsing_header_frame = false;
    v.button_frame = true;
    v.striped = false;
    v.slider_trailing_fill = true;

    let widget = |bg: Color32, border: Color32, fg: Color32| egui::style::WidgetVisuals {
        bg_fill: bg,
        weak_bg_fill: bg,
        bg_stroke: Stroke::new(1.0_f32, border),
        rounding: Rounding::same(9.0),
        fg_stroke: Stroke::new(1.0_f32, fg),
        expansion: 0.0,
    };
    v.widgets.noninteractive = widget(SURFACE_HI, BORDER, TEXT);
    v.widgets.inactive = widget(SURFACE_HI, BORDER, TEXT);
    v.widgets.hovered = widget(SURFACE_HOVER, BORDER_HI, Color32::WHITE);
    v.widgets.active = widget(SURFACE_HOVER, ACCENT, Color32::WHITE);
    v.widgets.open = widget(SURFACE_HI, BORDER_HI, TEXT);

    v
}

// ---------------------------------------------------------------------------
// Building blocks
// ---------------------------------------------------------------------------

/// A rounded surface that fills the available width — the container every part
/// of the window is built from.
pub fn card<R>(ui: &mut Ui, add: impl FnOnce(&mut Ui) -> R) -> InnerResponse<R> {
    egui::Frame::none()
        .fill(SURFACE)
        .stroke(Stroke::new(1.0_f32, BORDER))
        .rounding(Rounding::same(14.0))
        .inner_margin(Margin::same(16.0))
        .show(ui, |ui| {
            let width = ui.available_width();
            if width.is_finite() && width > 0.0 {
                ui.set_width(width);
            }
            add(ui)
        })
}

/// Two-line card header: a strong title and an optional dim subtitle.
pub fn card_heading(ui: &mut Ui, title: &str, subtitle: &str) {
    ui.vertical(|ui| {
        ui.spacing_mut().item_spacing.y = 2.0;
        ui.label(RichText::new(title).size(15.0).strong().color(TEXT));
        if !subtitle.is_empty() {
            ui.label(RichText::new(subtitle).size(11.5).color(TEXT_DIM));
        }
    });
}

/// A hairline rule.
pub fn divider(ui: &mut Ui) {
    ui.add_space(4.0);
    let (rect, _) = ui.allocate_exact_size(Vec2::new(ui.available_width(), 1.0), egui::Sense::hover());
    ui.painter().rect_filled(rect, Rounding::ZERO, BORDER);
    ui.add_space(4.0);
}

/// A small rounded status chip.
pub fn pill(ui: &mut Ui, text: &str, color: Color32) {
    egui::Frame::none()
        .fill(tint(color, 26))
        .stroke(Stroke::new(1.0_f32, tint(color, 64)))
        .rounding(Rounding::same(99.0))
        .inner_margin(Margin::symmetric(8.0, 3.0))
        .show(ui, |ui| {
            // Extend, never wrap: inside a wrapped row a wrapping label reports a
            // tiny width and the chip degenerates into a single column of glyphs.
            ui.add(
                egui::Label::new(RichText::new(text).size(11.0).color(color).strong())
                    .wrap_mode(egui::TextWrapMode::Extend),
            );
        });
}

/// Pills laid out in rows that wrap by *painted* width. `horizontal_wrapped`
/// cannot do this for framed widgets: it decides the wrap from the child's size
/// before the frame is placed, so a chip that does not fit overflows the panel.
pub fn chips(ui: &mut Ui, items: &[(String, Color32)]) {
    let avail = ui.available_width();
    let font = egui::FontId::proportional(11.0);
    let mut rows: Vec<Vec<usize>> = Vec::new();
    let mut row: Vec<usize> = Vec::new();
    let mut used = 0.0_f32;
    for (i, (text, color)) in items.iter().enumerate() {
        let text_w = ui
            .fonts(|f| f.layout_no_wrap(text.clone(), font.clone(), *color).size().x);
        // inner margin (8 + 8) plus the 1px stroke on both sides
        let chip_w = text_w + 18.0;
        if !row.is_empty() && used + chip_w > avail {
            rows.push(std::mem::take(&mut row));
            used = 0.0;
        }
        used += chip_w + 6.0;
        row.push(i);
    }
    if !row.is_empty() {
        rows.push(row);
    }
    for row in rows {
        ui.horizontal(|ui| {
            ui.spacing_mut().item_spacing.x = 6.0;
            for i in row {
                let (text, color) = &items[i];
                pill(ui, text, *color);
            }
        });
    }
}

/// "LABEL above value" block used by the plan panel.
pub fn field(ui: &mut Ui, key: &str, value: &str, color: Color32) {
    ui.label(RichText::new(key).size(10.0).color(TEXT_DIM));
    ui.add(egui::Label::new(RichText::new(value).size(13.0).color(color)).wrap());
    ui.add_space(6.0);
}

/// A tinted callout with a title, a wrapped body and room for actions.
pub fn banner<R>(
    ui: &mut Ui,
    color: Color32,
    title: &str,
    body: &str,
    add: impl FnOnce(&mut Ui) -> R,
) -> InnerResponse<R> {
    egui::Frame::none()
        .fill(tint(color, 22))
        .stroke(Stroke::new(1.0_f32, tint(color, 70)))
        .rounding(Rounding::same(12.0))
        .inner_margin(Margin::same(14.0))
        .show(ui, |ui| {
            let width = ui.available_width();
            if width.is_finite() && width > 0.0 {
                ui.set_width(width);
            }
            ui.label(RichText::new(title).size(13.0).strong().color(color));
            ui.add_space(2.0);
            ui.add(egui::Label::new(RichText::new(body).size(11.5).color(TEXT_DIM)).wrap());
            add(ui)
        })
}

// ---------------------------------------------------------------------------
// Buttons
// ---------------------------------------------------------------------------

fn accent_style(ui: &mut Ui) {
    let widgets = &mut ui.style_mut().visuals.widgets;
    for w in [
        &mut widgets.inactive,
        &mut widgets.hovered,
        &mut widgets.active,
        &mut widgets.open,
    ] {
        w.weak_bg_fill = ACCENT;
        w.bg_fill = ACCENT;
        w.bg_stroke = Stroke::NONE;
        w.fg_stroke = Stroke::new(1.0_f32, Color32::WHITE);
        w.rounding = Rounding::same(10.0);
        w.expansion = 0.0;
    }
    widgets.hovered.weak_bg_fill = ACCENT_HOVER;
    widgets.hovered.bg_fill = ACCENT_HOVER;
    widgets.active.weak_bg_fill = ACCENT_DOWN;
    widgets.active.bg_fill = ACCENT_DOWN;
    widgets.noninteractive.weak_bg_fill = SURFACE_HI;
    widgets.noninteractive.bg_fill = SURFACE_HI;
    widgets.noninteractive.bg_stroke = Stroke::new(1.0_f32, BORDER);
    widgets.noninteractive.fg_stroke = Stroke::new(1.0_f32, TEXT_DIM);
    widgets.noninteractive.rounding = Rounding::same(10.0);
}

/// Accent button sized to its label (dialogs, inline actions).
pub fn accent_button(ui: &mut Ui, label: &str, enabled: bool) -> Response {
    ui.scope(|ui| {
        accent_style(ui);
        ui.add_enabled(
            enabled,
            egui::Button::new(RichText::new(label).size(12.5).strong()),
        )
    })
    .inner
}

/// Accent button that fills the width and is tall enough to be the main CTA.
pub fn primary_button(ui: &mut Ui, label: &str, enabled: bool) -> Response {
    let width = ui.available_width();
    ui.scope(|ui| {
        accent_style(ui);
        ui.add_enabled(
            enabled,
            egui::Button::new(RichText::new(label).size(14.5).strong())
                .min_size(Vec2::new(width, 42.0)),
        )
    })
    .inner
}

/// Neutral outlined button for secondary actions.
pub fn secondary_button(ui: &mut Ui, label: &str, enabled: bool) -> Response {
    ui.add_enabled(
        enabled,
        egui::Button::new(RichText::new(label).size(12.5))
            .rounding(Rounding::same(9.0))
            .min_size(Vec2::new(0.0, 28.0)),
    )
}

/// Text-only button for tertiary actions (panel toggles).
pub fn ghost_button(ui: &mut Ui, label: &str) -> Response {
    ui.add(
        egui::Button::new(RichText::new(label).size(12.0).color(TEXT_DIM))
            .frame(false)
            .min_size(Vec2::new(0.0, 24.0)),
    )
}

// ---------------------------------------------------------------------------
// Choice rows
// ---------------------------------------------------------------------------

/// A full-width, clickable choice row: custom checkbox, bold title, dim
/// description. Returns `true` when the row toggled `checked` this frame.
///
/// The checkbox is drawn by hand so the whole row can be the hit target
/// without the checkbox and the row fighting over the click.
pub fn option_row(ui: &mut Ui, checked: &mut bool, title: &str, desc: &str, enabled: bool) -> bool {
    let fill = if *checked {
        if enabled {
            ACCENT_SOFT
        } else {
            tint(ACCENT_SOFT, 150)
        }
    } else {
        SURFACE_HI
    };
    let stroke = if *checked {
        Stroke::new(1.0_f32, tint(ACCENT, 110))
    } else {
        Stroke::new(1.0_f32, BORDER)
    };

    let frame = egui::Frame::none()
        .fill(fill)
        .stroke(stroke)
        .rounding(Rounding::same(12.0))
        .inner_margin(Margin::symmetric(12.0, 10.0));

    let mut prepared = frame.begin(ui);
    {
        let ui = &mut prepared.content_ui;
        let width = ui.available_width();
        if width.is_finite() && width > 0.0 {
            ui.set_width(width);
        }
        ui.horizontal(|ui| {
            ui.spacing_mut().item_spacing.x = 11.0;
            checkbox_glyph(ui, *checked, enabled);
            ui.vertical(|ui| {
                ui.spacing_mut().item_spacing.y = 1.0;
                let title_color = if enabled { TEXT } else { TEXT_DIM };
                ui.label(RichText::new(title).size(13.5).strong().color(title_color));
                ui.add(
                    egui::Label::new(RichText::new(desc).size(11.5).color(TEXT_DIM)).wrap(),
                );
            });
        });
    }
    let response = prepared.allocate_space(ui);

    let mut toggled = false;
    if enabled {
        let click = response
            .interact(egui::Sense::click())
            .on_hover_cursor(egui::CursorIcon::PointingHand);
        if click.hovered() {
            prepared.frame.fill = if *checked { ACCENT_SOFT_HI } else { SURFACE_HOVER };
            prepared.frame.stroke = Stroke::new(1.0_f32, if *checked { tint(ACCENT, 160) } else { BORDER_HI });
        }
        if click.clicked() {
            *checked = !*checked;
            toggled = true;
        }
    }
    prepared.paint(ui);
    toggled
}

/// A hand-drawn checkbox, so the row owns the click and the look stays rounded.
fn checkbox_glyph(ui: &mut Ui, checked: bool, enabled: bool) {
    let (rect, _) = ui.allocate_exact_size(Vec2::splat(18.0), egui::Sense::hover());
    let rounding = Rounding::same(5.0);
    let painter = ui.painter();
    if checked {
        let fill = if enabled { ACCENT } else { tint(ACCENT, 110) };
        painter.rect_filled(rect, rounding, fill);
        let mark = rect.shrink(4.5);
        let stroke = Stroke::new(1.8_f32, Color32::WHITE);
        painter.line_segment(
            [
                egui::pos2(mark.left(), mark.center().y),
                egui::pos2(mark.center().x - 0.5, mark.bottom()),
            ],
            stroke,
        );
        painter.line_segment(
            [
                egui::pos2(mark.center().x - 0.5, mark.bottom()),
                egui::pos2(mark.right(), mark.top()),
            ],
            stroke,
        );
    } else {
        let border = if enabled { BORDER_HI } else { BORDER };
        painter.rect_stroke(rect, rounding, Stroke::new(1.4_f32, border));
    }
}

/// A smaller checkbox indented under a choice row (stage sub-options).
pub fn sub_checkbox(ui: &mut Ui, checked: &mut bool, label: &str, enabled: bool) {
    ui.horizontal(|ui| {
        ui.add_space(29.0);
        ui.add_enabled(
            enabled,
            egui::Checkbox::new(checked, RichText::new(label).size(12.0).color(TEXT_DIM)),
        );
    });
}
