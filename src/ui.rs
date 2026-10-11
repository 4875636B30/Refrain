//! Reusable look-and-feel: theme, artwork tiles and list rows.

use std::collections::HashMap;

use egui::{Align, Color32, Layout, RichText};

use crate::ytm::SearchItem;

/// Accent used for selections and highlights.
pub const ACCENT: Color32 = Color32::from_rgb(0x1d, 0xb9, 0x54);

/// Used for warnings (missing tools, failed actions).
pub const WARN: Color32 = Color32::from_rgb(0xe6, 0xb4, 0x64);

/// Apply the app's dark or light styling.
pub fn apply_theme(ctx: &egui::Context, light: bool) {
    let theme = if light {
        egui::Theme::Light
    } else {
        egui::Theme::Dark
    };
    ctx.set_theme(if light {
        egui::ThemePreference::Light
    } else {
        egui::ThemePreference::Dark
    });

    let mut visuals = if light {
        egui::Visuals::light()
    } else {
        egui::Visuals::dark()
    };

    visuals.selection.bg_fill = ACCENT;
    visuals.selection.stroke = egui::Stroke::new(1.0, Color32::WHITE);
    visuals.hyperlink_color = ACCENT;
    visuals.widgets.hovered.bg_fill = if light {
        Color32::from_rgb(0xdd, 0xdd, 0xe2)
    } else {
        Color32::from_rgb(0x2b, 0x2b, 0x30)
    };
    visuals.widgets.hovered.weak_bg_fill = visuals.widgets.hovered.bg_fill;
    visuals.panel_fill = if light {
        Color32::from_rgb(0xf6, 0xf6, 0xf8)
    } else {
        Color32::from_rgb(0x12, 0x12, 0x14)
    };
    visuals.window_fill = visuals.panel_fill;
    visuals.extreme_bg_color = if light {
        Color32::WHITE
    } else {
        Color32::from_rgb(0x1c, 0x1c, 0x1f)
    };
    visuals.faint_bg_color = if light {
        Color32::from_rgb(0xee, 0xee, 0xf1)
    } else {
        Color32::from_rgb(0x1a, 0x1a, 0x1d)
    };

    ctx.set_visuals_of(theme, visuals);

    ctx.all_styles_mut(|style| {
        style.spacing.item_spacing = egui::vec2(8.0, 6.0);
        style.spacing.button_padding = egui::vec2(10.0, 5.0);
        // Rows and tiles handle their own clicks; without this, hovering the
        // text inside shows a text cursor and can swallow the click.
        style.interaction.selectable_labels = false;
        let radius = egui::CornerRadius::same(6);
        for widget in [
            &mut style.visuals.widgets.inactive,
            &mut style.visuals.widgets.hovered,
            &mut style.visuals.widgets.active,
            &mut style.visuals.widgets.open,
        ] {
            widget.corner_radius = radius;
        }
    });
}

/// A circular transport button; `primary` gets the accent fill.
pub fn round_button(
    ui: &mut egui::Ui,
    glyph: &str,
    diameter: f32,
    primary: bool,
) -> egui::Response {
    let (rect, response) =
        ui.allocate_exact_size(egui::vec2(diameter, diameter), egui::Sense::click());
    let hovered = response.hovered();
    let visuals = ui.visuals();

    let (fill, color) = if primary {
        (
            if hovered {
                ACCENT.gamma_multiply(1.25)
            } else {
                ACCENT
            },
            Color32::WHITE,
        )
    } else if hovered {
        (visuals.widgets.hovered.bg_fill, visuals.text_color())
    } else {
        (Color32::TRANSPARENT, visuals.weak_text_color())
    };

    if fill != Color32::TRANSPARENT {
        ui.painter()
            .circle_filled(rect.center(), diameter / 2.0, fill);
    }
    // Optically centre the play triangle.
    let nudge = if glyph == "▶" { diameter * 0.04 } else { 0.0 };
    ui.painter().text(
        rect.center() + egui::vec2(nudge, 0.0),
        egui::Align2::CENTER_CENTER,
        glyph,
        egui::FontId::proportional(diameter * 0.44),
        color,
    );

    if hovered {
        ui.ctx().set_cursor_icon(egui::CursorIcon::PointingHand);
    }
    response
}

/// A slim draggable progress bar. Returns a new position when the user seeks.
pub fn progress_bar(ui: &mut egui::Ui, pos: f32, total: f32) -> Option<f32> {
    let width = ui.available_width();
    let (rect, response) =
        ui.allocate_exact_size(egui::vec2(width, 16.0), egui::Sense::click_and_drag());
    let thickness = 4.0;
    let track = egui::Rect::from_min_size(
        egui::pos2(rect.left(), rect.center().y - thickness / 2.0),
        egui::vec2(width, thickness),
    );
    let active = response.hovered() || response.dragged();
    let visuals = ui.visuals();

    ui.painter().rect_filled(
        track,
        thickness / 2.0,
        if active {
            visuals.widgets.hovered.bg_fill
        } else {
            visuals.widgets.inactive.bg_fill
        },
    );

    let fraction = if total > 0.0 {
        (pos / total).clamp(0.0, 1.0)
    } else {
        0.0
    };
    let handle_x = track.left() + width * fraction;
    ui.painter().rect_filled(
        egui::Rect::from_min_max(track.min, egui::pos2(handle_x, track.max.y)),
        thickness / 2.0,
        ACCENT,
    );
    if active {
        ui.painter()
            .circle_filled(egui::pos2(handle_x, track.center().y), 6.0, ACCENT);
    }

    if total > 0.0
        && (response.dragged() || response.clicked())
        && let Some(pointer) = response.interact_pointer_pos()
    {
        let fraction = ((pointer.x - track.left()) / width).clamp(0.0, 1.0);
        return Some(fraction * total);
    }
    None
}

pub fn fmt_time(secs: f32) -> String {
    let secs = secs.max(0.0) as u32;
    format!("{}:{:02}", secs / 60, secs % 60)
}

/// Draw a cached thumbnail, or a neutral placeholder while it loads.
pub fn draw_thumbnail(
    ui: &mut egui::Ui,
    uri: Option<&str>,
    thumbs: &HashMap<String, egui::TextureHandle>,
    size: egui::Vec2,
    radius: f32,
) {
    match uri.and_then(|u| thumbs.get(u)) {
        Some(texture) => {
            ui.add(
                egui::Image::from_texture(texture)
                    .fit_to_exact_size(size)
                    .corner_radius(radius),
            );
        }
        None => {
            let (rect, _) = ui.allocate_exact_size(size, egui::Sense::hover());
            ui.painter()
                .rect_filled(rect, radius, ui.visuals().widgets.inactive.bg_fill);
        }
    }
}

/// One clickable list row with a thumbnail.
#[allow(clippy::too_many_arguments)]
pub fn list_row(
    ui: &mut egui::Ui,
    thumbs: &HashMap<String, egui::TextureHandle>,
    id: impl std::hash::Hash + std::fmt::Debug,
    thumb: Option<&str>,
    title: &str,
    subtitle: &str,
    trailing: Option<&str>,
    index: Option<usize>,
    selected: bool,
    playable: bool,
    navigable: bool,
) -> egui::Response {
    let width = ui.available_width();
    let (rect, response) = ui.allocate_exact_size(egui::vec2(width, 54.0), egui::Sense::click());
    let hovered = response.hovered();
    if selected {
        ui.painter().rect_filled(
            rect,
            6.0,
            ui.visuals().selection.bg_fill.gamma_multiply(0.35),
        );
    } else if hovered {
        ui.painter()
            .rect_filled(rect, 6.0, ui.visuals().widgets.hovered.bg_fill);
    }

    let inner = rect.shrink2(egui::vec2(8.0, 5.0));
    ui.scope_builder(
        egui::UiBuilder::new()
            .max_rect(inner)
            .id_salt(id)
            .layout(Layout::left_to_right(Align::Center)),
        |ui| {
            // Show a track number until hovered, then a play affordance.
            let gutter = 22.0;
            let (slot, _) = ui.allocate_exact_size(egui::vec2(gutter, 40.0), egui::Sense::hover());
            if playable && hovered {
                ui.painter().text(
                    slot.center(),
                    egui::Align2::CENTER_CENTER,
                    "▶",
                    egui::FontId::proportional(16.0),
                    ui.visuals().text_color(),
                );
            } else if let Some(n) = index {
                ui.painter().text(
                    slot.center(),
                    egui::Align2::CENTER_CENTER,
                    (n + 1).to_string(),
                    egui::FontId::proportional(13.0),
                    ui.visuals().weak_text_color(),
                );
            }

            draw_thumbnail(ui, thumb, thumbs, egui::vec2(40.0, 40.0), 5.0);
            ui.add_space(8.0);
            ui.vertical(|ui| {
                ui.label(RichText::new(title).strong());
                if !subtitle.is_empty() {
                    ui.label(RichText::new(subtitle).weak().small());
                }
            });
            ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                if let Some(t) = trailing {
                    ui.label(RichText::new(t).weak().small());
                }
                if hovered && navigable {
                    ui.label(RichText::new("›").weak().size(18.0));
                }
            });
        },
    );

    if hovered {
        ui.ctx().set_cursor_icon(egui::CursorIcon::PointingHand);
    }
    response
}

/// A cover-art tile for grids.
fn cover_tile(
    ui: &mut egui::Ui,
    thumbs: &HashMap<String, egui::TextureHandle>,
    item: &SearchItem,
    width: f32,
    selected: bool,
) -> egui::Response {
    let art = width - 10.0;
    // Square art plus one line each for title and subtitle. A fixed height keeps
    // rows aligned however long the text is.
    let height = art + 46.0;
    let (rect, response) = ui.allocate_exact_size(egui::vec2(width, height), egui::Sense::click());
    let hovered = response.hovered();
    if selected {
        ui.painter().rect_filled(
            rect,
            8.0,
            ui.visuals().selection.bg_fill.gamma_multiply(0.35),
        );
    } else if hovered {
        ui.painter()
            .rect_filled(rect, 8.0, ui.visuals().widgets.hovered.bg_fill);
    }

    let inner = rect.shrink2(egui::vec2(5.0, 5.0));
    ui.scope_builder(
        egui::UiBuilder::new()
            .max_rect(inner)
            .id_salt(("tile", item.title.as_str()))
            .layout(Layout::top_down(Align::Min)),
        |ui| {
            ui.set_max_width(art);
            draw_thumbnail(
                ui,
                item.thumbnail.as_deref(),
                thumbs,
                egui::vec2(art, art),
                7.0,
            );
            ui.add_space(6.0);
            ui.add(egui::Label::new(RichText::new(&item.title).strong()).truncate());
            if !item.subtitle.is_empty() {
                ui.add(egui::Label::new(RichText::new(&item.subtitle).weak().small()).truncate());
            }
        },
    );

    if hovered {
        ui.ctx().set_cursor_icon(egui::CursorIcon::PointingHand);
    }
    response
}

/// A rounded search pill. Returns `true` when the user submits (Enter).
pub fn search_bar(ui: &mut egui::Ui, query: &mut String, width: f32) -> bool {
    let mut submitted = false;
    let visuals = ui.visuals();
    let frame = egui::Frame::new()
        .fill(visuals.extreme_bg_color)
        .stroke(egui::Stroke::new(
            1.0,
            visuals.widgets.inactive.bg_stroke.color,
        ))
        .corner_radius(egui::CornerRadius::same(15))
        .inner_margin(egui::Margin::symmetric(12, 3));

    frame.show(ui, |ui| {
        ui.horizontal(|ui| {
            ui.label(RichText::new("🔍").weak());
            let edit = ui.add(
                egui::TextEdit::singleline(query)
                    // Transparent so the pill provides the background.
                    .frame(egui::Frame::new().inner_margin(egui::Margin::symmetric(0, 3)))
                    .hint_text("Search songs, albums, artists…")
                    .desired_width(width),
            );
            if edit.lost_focus() && ui.input(|i| i.key_pressed(egui::Key::Enter)) {
                submitted = true;
            }
        });
    });
    submitted
}

/// Lay out items as a wrapping grid of cover tiles.
///
/// `selected` is the `video_id` / `browse_id` of the currently-playing item, if
/// any. Returns the index of a clicked tile.
pub fn cover_grid(
    ui: &mut egui::Ui,
    thumbs: &HashMap<String, egui::TextureHandle>,
    items: &[SearchItem],
    tile_width: f32,
    selected: Option<&str>,
) -> Option<usize> {
    let gap = ui.spacing().item_spacing.x;
    let available = ui.available_width();
    let columns = (((available + gap) / (tile_width + gap)).floor() as usize).max(1);
    let mut clicked = None;

    for (row_index, row) in items.chunks(columns).enumerate() {
        ui.horizontal(|ui| {
            for (column, item) in row.iter().enumerate() {
                let index = row_index * columns + column;
                let is_selected = selected
                    .map(|key| {
                        item.video_id.as_deref() == Some(key)
                            || item.browse_id.as_deref() == Some(key)
                    })
                    .unwrap_or(false);
                if cover_tile(ui, thumbs, item, tile_width, is_selected).clicked() {
                    clicked = Some(index);
                }
            }
        });
        ui.add_space(4.0);
    }
    clicked
}
