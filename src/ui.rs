//! Reusable look-and-feel: theme, artwork tiles and list rows.

use std::collections::{HashMap, HashSet};

use egui::{Align, Color32, Layout, RichText};

use crate::ytm::SearchItem;

/// Tint behind the row or tile that is currently playing. Deliberately faint:
/// it only has to say "this one", and the accent equaliser drawn on top of it
/// needs to stay legible.
pub const PLAYING_TINT: Color32 = Color32::from_rgba_premultiplied(34, 0, 0, 34);

/// Accent used for selections and highlights.
/// YouTube's red. This is a YouTube Music client, so the accent is the brand
/// red — it used to be Spotify green, which read as a Spotify clone.
pub const ACCENT: Color32 = Color32::from_rgb(0xff, 0x00, 0x00);

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

/// YouTube Music's headings are Roboto Medium. egui has no notion of font
/// weight, so the heavier face is registered as its own family and picked
/// explicitly via [`title`].
pub const TITLE_FAMILY: &str = "roboto-medium";

/// Install the bundled typeface.
///
/// Roboto is what YouTube Music uses, and bundling it rather than relying on
/// the system keeps the app looking the same everywhere — the default egui
/// stack is whatever the desktop happens to provide.
///
/// Roboto goes *first* and egui's own stack stays behind it: that stack carries
/// the emoji and the transport glyphs the UI draws, and covers scripts this
/// file may not.
pub fn install_fonts(ctx: &egui::Context) {
    let mut fonts = egui::FontDefinitions::default();

    fonts.font_data.insert(
        "roboto".to_owned(),
        std::sync::Arc::new(egui::FontData::from_static(include_bytes!(
            "../assets/fonts/Roboto-Regular.ttf"
        ))),
    );
    fonts.font_data.insert(
        TITLE_FAMILY.to_owned(),
        std::sync::Arc::new(egui::FontData::from_static(include_bytes!(
            "../assets/fonts/Roboto-Medium.ttf"
        ))),
    );

    let proportional = fonts
        .families
        .entry(egui::FontFamily::Proportional)
        .or_default();
    proportional.insert(0, "roboto".to_owned());

    let mut heading = vec![TITLE_FAMILY.to_owned()];
    heading.extend(proportional.iter().cloned());
    fonts
        .families
        .insert(egui::FontFamily::Name(TITLE_FAMILY.into()), heading);

    ctx.set_fonts(fonts);
}

/// A heading in Roboto Medium.
pub fn title(text: impl Into<String>) -> egui::RichText {
    egui::RichText::new(text).family(egui::FontFamily::Name(TITLE_FAMILY.into()))
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
        // YouTube Music's play button is a light disc with an inverted glyph,
        // not an accent-coloured one.
        (
            if hovered {
                visuals.widgets.hovered.fg_stroke.color
            } else {
                visuals.text_color()
            },
            visuals.panel_fill,
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

/// A transport toggle (shuffle, repeat) with a fixed footprint.
///
/// The size is reserved up front so the neighbouring buttons can never move,
/// and the `badge` is drawn as plain text because repeat-one needs a state
/// marker and the `🔂` emoji is absent from many fonts — it renders as blank,
/// which makes the button look like it did nothing.
pub fn transport_toggle(
    ui: &mut egui::Ui,
    glyph: &str,
    badge: Option<&str>,
    active: bool,
    size: f32,
) -> egui::Response {
    let (rect, response) = ui.allocate_exact_size(egui::vec2(size, size), egui::Sense::click());
    let visuals = ui.visuals();
    let tint = if active {
        ACCENT
    } else {
        visuals.weak_text_color()
    };

    if response.hovered() {
        ui.painter()
            .rect_filled(rect, 6.0, visuals.widgets.hovered.bg_fill);
        ui.ctx().set_cursor_icon(egui::CursorIcon::PointingHand);
    }
    ui.painter().text(
        rect.center(),
        egui::Align2::CENTER_CENTER,
        glyph,
        egui::FontId::proportional(size * 0.62),
        tint,
    );
    if let Some(badge) = badge {
        ui.painter().text(
            egui::pos2(rect.right(), rect.top()),
            egui::Align2::RIGHT_TOP,
            badge,
            egui::FontId::proportional(size * 0.46),
            tint,
        );
    }
    response
}

/// A list/queue icon drawn with the painter rather than a glyph, so it cannot
/// go missing from the bundled fonts.
pub fn queue_icon(ui: &mut egui::Ui, active: bool, size: f32) -> egui::Response {
    let (rect, response) = ui.allocate_exact_size(egui::vec2(size, size), egui::Sense::click());
    let visuals = ui.visuals();
    let tint = if active {
        ACCENT
    } else {
        visuals.weak_text_color()
    };

    if response.hovered() {
        ui.painter()
            .rect_filled(rect, 6.0, visuals.widgets.hovered.bg_fill);
        ui.ctx().set_cursor_icon(egui::CursorIcon::PointingHand);
    }
    let width = size * 0.52;
    let left = rect.center().x - width / 2.0;
    for line in 0..3 {
        let y = rect.center().y + (line as f32 - 1.0) * (size * 0.19);
        ui.painter().rect_filled(
            egui::Rect::from_min_size(egui::pos2(left, y - 1.0), egui::vec2(width, 2.0)),
            1.0,
            tint,
        );
    }
    response
}

/// The height every control in the top bar shares, so they line up.
pub const CONTROL_HEIGHT: f32 = 34.0;

/// An icon drawn with the painter rather than a font glyph.
///
/// Glyphs turned out to be a liability here: `▾` exists only in the monospace
/// face, and a glyph missing from every font in the fallback chain renders as
/// nothing at all. These shapes are simple enough to draw, and drawing them
/// keeps the stroke weight consistent between the chevrons and the magnifier.
#[derive(Clone, Copy)]
pub enum Icon {
    ChevronLeft,
    ChevronDown,
    Close,
    Search,
    /// For anything not worth drawing, like the gear.
    Glyph(&'static str),
}

fn draw_icon(ui: &egui::Ui, icon: Icon, rect: egui::Rect, color: Color32) {
    let painter = ui.painter();
    let stroke = egui::Stroke::new(1.7, color);
    let centre = rect.center();

    match icon {
        Icon::ChevronLeft => {
            let reach = rect.height() * 0.20;
            let tip = egui::pos2(centre.x - reach * 0.5, centre.y);
            painter.line_segment(
                [egui::pos2(centre.x + reach * 0.6, centre.y - reach), tip],
                stroke,
            );
            painter.line_segment(
                [tip, egui::pos2(centre.x + reach * 0.6, centre.y + reach)],
                stroke,
            );
        }
        Icon::ChevronDown => {
            let reach = rect.height() * 0.19;
            let tip = egui::pos2(centre.x, centre.y + reach * 0.55);
            painter.line_segment(
                [egui::pos2(centre.x - reach, centre.y - reach * 0.45), tip],
                stroke,
            );
            painter.line_segment(
                [tip, egui::pos2(centre.x + reach, centre.y - reach * 0.45)],
                stroke,
            );
        }
        Icon::Close => {
            let reach = rect.height() * 0.17;
            painter.line_segment(
                [
                    centre + egui::vec2(-reach, -reach),
                    centre + egui::vec2(reach, reach),
                ],
                stroke,
            );
            painter.line_segment(
                [
                    centre + egui::vec2(reach, -reach),
                    centre + egui::vec2(-reach, reach),
                ],
                stroke,
            );
        }
        Icon::Search => {
            let radius = rect.height() * 0.21;
            let centre = centre - egui::vec2(radius * 0.3, radius * 0.3);
            painter.circle_stroke(centre, radius, stroke);
            let handle = radius * 0.72;
            painter.line_segment(
                [
                    centre + egui::vec2(handle, handle),
                    centre + egui::vec2(handle * 2.1, handle * 2.1),
                ],
                stroke,
            );
        }
        Icon::Glyph(glyph) => {
            painter.text(
                centre,
                egui::Align2::CENTER_CENTER,
                glyph,
                egui::FontId::proportional(rect.height() * 0.56),
                color,
            );
        }
    }
}

/// A circular icon button — the shape YouTube Music uses for toolbar icons.
pub fn icon_button(
    ui: &mut egui::Ui,
    icon: Icon,
    size: f32,
    enabled: bool,
    tooltip: &str,
) -> egui::Response {
    let sense = if enabled {
        egui::Sense::click()
    } else {
        egui::Sense::hover()
    };
    let (rect, response) = ui.allocate_exact_size(egui::vec2(size, size), sense);
    let visuals = ui.visuals();
    let hovered = enabled && response.hovered();

    if hovered {
        ui.painter()
            .circle_filled(rect.center(), size / 2.0, visuals.widgets.hovered.bg_fill);
        ui.ctx().set_cursor_icon(egui::CursorIcon::PointingHand);
    }
    // Resting icons are already the strongest text colour: the hover feedback
    // is the circle behind them, so dimming them at rest would only make them
    // look disabled.
    let color = if !enabled {
        visuals.weak_text_color().gamma_multiply(0.45)
    } else {
        visuals.strong_text_color()
    };
    draw_icon(ui, icon, rect, color);

    if enabled {
        response.on_hover_text(tooltip)
    } else {
        response
    }
}

/// Run `contents` with the theme's widget visuals overridden.
///
/// The standard widgets already get interaction states right; this is how they
/// are given the pill shapes and fills this design uses, instead of
/// reimplementing buttons and their hover handling.
pub fn with_visuals<R>(
    ui: &mut egui::Ui,
    style: impl FnOnce(&mut egui::Visuals),
    contents: impl FnOnce(&mut egui::Ui) -> R,
) -> R {
    let saved = ui.visuals().clone();
    style(ui.visuals_mut());
    let result = contents(ui);
    *ui.visuals_mut() = saved;
    result
}

/// Give the widgets in a scope a flat pill look.
fn pill(visuals: &mut egui::Visuals, fill: Color32, hover: Color32, fg: Color32, radius: u8) {
    for state in [
        &mut visuals.widgets.inactive,
        &mut visuals.widgets.hovered,
        &mut visuals.widgets.active,
        &mut visuals.widgets.open,
    ] {
        state.corner_radius = egui::CornerRadius::same(radius);
        state.bg_stroke = egui::Stroke::NONE;
        state.fg_stroke = egui::Stroke::new(1.0, fg);
    }
    visuals.widgets.inactive.bg_fill = fill;
    visuals.widgets.inactive.weak_bg_fill = fill;
    visuals.widgets.hovered.bg_fill = hover;
    visuals.widgets.hovered.weak_bg_fill = hover;
    visuals.widgets.active.bg_fill = hover;
    visuals.widgets.active.weak_bg_fill = hover;
    visuals.widgets.open.bg_fill = hover;
    visuals.widgets.open.weak_bg_fill = hover;
}

/// The fill of a quiet pill (the filter, the search field).
fn chip_fill(visuals: &egui::Visuals) -> Color32 {
    visuals.widgets.inactive.weak_bg_fill
}

/// A pill-shaped dropdown: the current choice, a drawn chevron, and a menu.
///
/// Hand-rolled rather than a styled `ComboBox` because the chevron has to sit
/// against the label: a combo box reserves a fixed width and pins its arrow to
/// the far edge, which is what made the old one look disconnected.
pub fn pill_dropdown<T: PartialEq + Copy>(
    ui: &mut egui::Ui,
    id: &str,
    selected: T,
    options: &[T],
    label: impl Fn(T) -> &'static str,
) -> Option<T> {
    let visuals = ui.visuals().clone();
    let text = label(selected);
    let galley = ui.painter().layout_no_wrap(
        text.to_string(),
        egui::FontId::proportional(14.0),
        visuals.text_color(),
    );
    let width = galley.size().x + 40.0;
    let (rect, response) =
        ui.allocate_exact_size(egui::vec2(width, CONTROL_HEIGHT), egui::Sense::click());

    let hovered = response.hovered();
    if hovered {
        ui.ctx().set_cursor_icon(egui::CursorIcon::PointingHand);
    }
    let fill = if hovered {
        visuals.widgets.hovered.bg_fill
    } else {
        chip_fill(&visuals)
    };
    ui.painter().rect_filled(
        rect,
        egui::CornerRadius::same((CONTROL_HEIGHT / 2.0) as u8),
        fill,
    );
    ui.painter().galley(
        egui::pos2(rect.left() + 14.0, rect.center().y - galley.size().y / 2.0),
        galley,
        visuals.text_color(),
    );
    draw_icon(
        ui,
        Icon::ChevronDown,
        egui::Rect::from_center_size(
            egui::pos2(rect.right() - 14.0, rect.center().y),
            egui::vec2(14.0, 14.0),
        ),
        visuals.weak_text_color(),
    );

    let mut chosen = None;
    egui::Popup::menu(&response)
        .id(egui::Id::new(id))
        .show(|ui| {
            ui.set_min_width(rect.width());
            for option in options {
                if ui
                    .selectable_label(*option == selected, label(*option))
                    .clicked()
                {
                    chosen = Some(*option);
                }
            }
        });
    chosen
}

/// A pill-shaped action button, in the accent colour.
pub fn pill_button(ui: &mut egui::Ui, label: &str) -> egui::Response {
    let radius = (CONTROL_HEIGHT / 2.0) as u8;
    with_visuals(
        ui,
        |visuals| {
            pill(
                visuals,
                ACCENT,
                ACCENT.gamma_multiply(1.25),
                Color32::WHITE,
                radius,
            )
        },
        |ui| ui.button(RichText::new(label).color(Color32::WHITE)),
    )
}

/// Lay `contents` out inside a region of exactly `size`.
///
/// `allocate_ui_with_layout` shrinks to fit its content, so a section holding a
/// short title comes out narrower than the same section holding a long one —
/// which drags everything after it around the bar. Reserving the space up front
/// keeps the layout still, which matters most for the transport controls.
pub fn fixed_section(
    ui: &mut egui::Ui,
    size: egui::Vec2,
    layout: egui::Layout,
    contents: impl FnOnce(&mut egui::Ui),
) {
    let (rect, _) = ui.allocate_exact_size(size, egui::Sense::hover());
    let mut child = ui.new_child(egui::UiBuilder::new().max_rect(rect).layout(layout));
    contents(&mut child);
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
/// Where artwork drawn on screen this frame is collected, so only that is
/// fetched.
///
/// Kept in egui's memory rather than threaded through every widget signature:
/// the draw code is the only place that knows what is actually visible, and
/// adding a parameter to each of them would be a lot of plumbing for a set.
const VISIBLE_THUMBS: &str = "visible-thumbnails";

fn mark_visible(ui: &egui::Ui, uri: &str) {
    ui.ctx().memory_mut(|memory| {
        memory
            .data
            .get_temp_mut_or_default::<HashSet<String>>(egui::Id::new(VISIBLE_THUMBS))
            .insert(uri.to_owned());
    });
}

/// Take the artwork drawn since this was last called.
pub fn take_visible_thumbnails(ctx: &egui::Context) -> HashSet<String> {
    ctx.memory_mut(|memory| {
        std::mem::take(
            memory
                .data
                .get_temp_mut_or_default::<HashSet<String>>(egui::Id::new(VISIBLE_THUMBS)),
        )
    })
}

pub fn draw_thumbnail(
    ui: &mut egui::Ui,
    uri: Option<&str>,
    thumbs: &HashMap<String, egui::TextureHandle>,
    size: egui::Vec2,
    radius: f32,
) {
    let Some(uri) = uri else {
        // No artwork to show and nothing to fetch.
        let (rect, _) = ui.allocate_exact_size(size, egui::Sense::hover());
        ui.painter()
            .rect_filled(rect, radius, ui.visuals().widgets.inactive.bg_fill);
        return;
    };

    match thumbs.get(uri) {
        Some(texture) => {
            let response = ui.add(
                egui::Image::from_texture(texture)
                    .fit_to_exact_size(size)
                    .corner_radius(radius),
            );
            if ui.is_rect_visible(response.rect) {
                mark_visible(ui, uri);
            }
        }
        None => {
            let (rect, _) = ui.allocate_exact_size(size, egui::Sense::hover());
            ui.painter()
                .rect_filled(rect, radius, ui.visuals().widgets.inactive.bg_fill);
            // A placeholder still counts as on screen, and its artwork still
            // needs fetching. This is also what bootstraps the very first
            // frame, when nothing has been drawn and so nothing is visible yet.
            if ui.is_rect_visible(rect) {
                mark_visible(ui, uri);
            }
        }
    }
}

/// What happened when a list row was drawn.
pub struct RowOutcome {
    pub clicked: bool,
    pub hovered: bool,
    /// A linked name inside the row (artist or album) was clicked.
    pub link: Option<String>,
    /// "Add to playlist" was chosen from the row's context menu.
    pub add_to_playlist: bool,
    /// "Remove from this playlist" was chosen from the row's context menu.
    pub remove_from_playlist: bool,
}

/// YouTube Music marks the playing row with a small equaliser where the track
/// number would be. The bars animate while playback runs and hold still when it
/// is paused, so the row still reads as "this one" either way.
fn draw_equaliser(ui: &egui::Ui, rect: egui::Rect, animating: bool) {
    const BARS: usize = 3;
    let (bar_w, gap) = (3.0, 2.0);
    let total = BARS as f32 * bar_w + (BARS - 1) as f32 * gap;
    let left = rect.center().x - total / 2.0;
    let height = rect.height().min(16.0);
    let time = ui.ctx().input(|i| i.time) as f32;

    for bar in 0..BARS {
        // A per-bar phase offset stops them moving as one block.
        let phase = time * 3.2 + bar as f32 * 1.1;
        let scale = if animating {
            0.35 + 0.65 * (phase.sin() * 0.5 + 0.5)
        } else {
            0.55
        };
        let x = left + bar as f32 * (bar_w + gap);
        ui.painter().rect_filled(
            egui::Rect::from_center_size(
                egui::pos2(x + bar_w / 2.0, rect.center().y),
                egui::vec2(bar_w, height * scale),
            ),
            1.0,
            ACCENT,
        );
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
    segments: &[(String, Option<String>)],
    trailing: Option<&str>,
    index: Option<usize>,
    selected: bool,
    // `active` is playback actually running, which animates the marker.
    active: bool,
    // `can_remove` offers "remove from this playlist" in the context menu.
    can_remove: bool,
    playable: bool,
    navigable: bool,
) -> RowOutcome {
    let width = ui.available_width();
    let (rect, response) = ui.allocate_exact_size(egui::vec2(width, 54.0), egui::Sense::click());
    let hovered = response.hovered();
    let mut link: Option<String> = None;
    if selected {
        ui.painter().rect_filled(rect, 6.0, PLAYING_TINT);
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
            // Show a track number until hovered, then a play affordance — or an
            // equaliser, if this is the track that is playing.
            let gutter = 22.0;
            let (slot, _) = ui.allocate_exact_size(egui::vec2(gutter, 40.0), egui::Sense::hover());
            if selected {
                draw_equaliser(ui, slot, active);
            } else if playable && hovered {
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
                if !segments.is_empty() {
                    ui.horizontal(|ui| {
                        // The parts carry their own separators, so don't add
                        // extra spacing between them.
                        ui.spacing_mut().item_spacing.x = 0.0;
                        // Linked names read as ordinary text — not the accent
                        // colour — but still respond to clicks.
                        let weak = ui.visuals().weak_text_color();
                        ui.visuals_mut().hyperlink_color = weak;
                        for (text, target) in segments {
                            match target {
                                Some(browse_id) => {
                                    if ui.link(RichText::new(text).small()).clicked() {
                                        link = Some(browse_id.clone());
                                    }
                                }
                                None => {
                                    ui.label(RichText::new(text).weak().small());
                                }
                            }
                        }
                    });
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

    // Right-click menu. Kept to what the app can actually do: a menu that
    // offers things which do nothing is worse than no menu.
    let mut add_to_playlist = false;
    let mut remove_from_playlist = false;
    response.context_menu(|ui| {
        if ui.button("Add to playlist…").clicked() {
            add_to_playlist = true;
            ui.close();
        }
        if can_remove && ui.button("Remove from this playlist").clicked() {
            remove_from_playlist = true;
            ui.close();
        }
    });

    RowOutcome {
        clicked: response.clicked(),
        hovered,
        link,
        add_to_playlist,
        remove_from_playlist,
    }
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
    // Square art plus one line each for title and subtitle, with room for the
    // tile's own padding so nothing overflows the highlight.
    let height = art + 52.0;
    let (rect, response) = ui.allocate_exact_size(egui::vec2(width, height), egui::Sense::click());
    let hovered = response.hovered();
    if selected {
        ui.painter().rect_filled(rect, 8.0, PLAYING_TINT);
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

/// Stable id for the search field, so keyboard shortcuts can focus it.
pub const SEARCH_ID: &str = "refrain_search_field";

/// A rounded search pill. Returns `true` when the user submits (Enter).
pub fn search_bar(ui: &mut egui::Ui, query: &mut String, width: f32) -> bool {
    let focused = ui.memory(|memory| memory.has_focus(egui::Id::new(SEARCH_ID)));
    let visuals = ui.visuals().clone();
    let radius = egui::CornerRadius::same((CONTROL_HEIGHT / 2.0) as u8);

    // The pill is drawn here rather than by a Frame so its height is exact and
    // it can show focus without the text jumping.
    let (rect, _) = ui.allocate_exact_size(egui::vec2(width, CONTROL_HEIGHT), egui::Sense::hover());
    let fill = if focused {
        visuals.extreme_bg_color
    } else {
        chip_fill(&visuals)
    };
    ui.painter().rect_filled(rect, radius, fill);
    if focused {
        ui.painter().rect_stroke(
            rect,
            radius,
            egui::Stroke::new(1.0, ACCENT),
            egui::StrokeKind::Inside,
        );
    }

    let mut submitted = false;
    let mut clear = false;
    let inner = rect.shrink2(egui::vec2(13.0, 0.0));
    ui.scope_builder(
        egui::UiBuilder::new()
            .max_rect(inner)
            .layout(Layout::left_to_right(Align::Center)),
        |ui| {
            draw_icon(
                ui,
                Icon::Search,
                egui::Rect::from_center_size(
                    egui::pos2(inner.left() + 8.0, inner.center().y),
                    egui::vec2(17.0, 17.0),
                ),
                visuals.weak_text_color(),
            );
            ui.add_space(25.0);

            // Room is reserved for the clear button whether or not it is drawn,
            // so the text does not shift as you type.
            let text_width = (inner.width() - 25.0 - 24.0).max(40.0);
            let edit = ui.add(
                egui::TextEdit::singleline(query)
                    .frame(egui::Frame::NONE)
                    .id(egui::Id::new(SEARCH_ID))
                    .hint_text("Search songs, albums, artists…")
                    .desired_width(text_width),
            );
            if edit.lost_focus() && ui.input(|i| i.key_pressed(egui::Key::Enter)) {
                submitted = true;
            }
            ui.add_space(2.0);
            if !query.is_empty() && icon_button(ui, Icon::Close, 22.0, true, "Clear").clicked() {
                clear = true;
            }
        },
    );

    if clear {
        query.clear();
        ui.memory_mut(|memory| memory.request_focus(egui::Id::new(SEARCH_ID)));
    }
    submitted
}

/// A single horizontally-scrolling row of cover tiles.
///
/// YouTube Music shows its shelves as carousels — one row that scrolls
/// sideways — rather than a wrapping grid, which is what keeps a feed
/// scannable instead of a wall of artwork.
pub fn cover_row(
    ui: &mut egui::Ui,
    thumbs: &HashMap<String, egui::TextureHandle>,
    items: &[SearchItem],
    tile_width: f32,
    selected: Option<&str>,
    id: impl std::hash::Hash + std::fmt::Debug,
) -> Option<usize> {
    let mut clicked = None;
    egui::ScrollArea::horizontal().id_salt(id).show(ui, |ui| {
        ui.horizontal(|ui| {
            for (index, item) in items.iter().enumerate() {
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
    });
    clicked
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
