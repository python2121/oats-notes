//! Design tokens and the egui style install — the dark charcoal look of the SwiftInvert shell
//! (ported nearly verbatim): the chrome values are our own, in the family of the lightcraft
//! Charcoal theme, and the UI is set in the operating system's own font (SF Pro on macOS, with
//! its optical-size and grade axes tuned) so the window reads as native. Dark is the default; a
//! light set exists so a toggle has somewhere to go, but nothing persists it yet.

use egui::{Color32, CornerRadius, FontFamily, FontId, Stroke, Visuals};

/// Colour and metric tokens. Fetched per frame with [`Tokens::get`].
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Tokens {
    /// Panels, menu bar, control bar.
    pub chrome: Color32,
    /// Behind the canvas when no canvas colour is chosen (never shown in practice).
    pub canvas: Color32,
    pub divider: Color32,
    pub inset: Color32,
    pub field: Color32,
    pub button: Color32,
    pub button_border: Color32,
    pub hover: Color32,
    pub pressed: Color32,
    pub text: Color32,
    pub text_secondary: Color32,
    pub text_tertiary: Color32,
    pub text_disabled: Color32,
    pub track: Color32,
    pub knob: Color32,
    pub knob_border: Color32,
    pub accent: Color32,
    pub danger: Color32,
    /// Default width of the left sidebar (Views + Categories).
    pub sidebar_w: f32,
    /// The listen control bar at the top of the transcript pane.
    pub control_bar_h: f32,
    /// The inset status strip at the bottom of the transcript pane.
    pub status_h: f32,
    /// Sidebar row height.
    pub row_h: f32,
    pub menu_bar_h: f32,
    pub dark: bool,
}

impl Default for Tokens {
    fn default() -> Self {
        Self::dark()
    }
}

impl Tokens {
    /// The default dark set.
    pub fn dark() -> Self {
        let rgb = |v: u32| Color32::from_rgb((v >> 16) as u8, (v >> 8) as u8, v as u8);
        Tokens {
            chrome: rgb(0x2a2a2a),
            canvas: rgb(0x1f1f1f),
            divider: rgb(0x1a1a1a),
            inset: rgb(0x222222),
            field: rgb(0x1e1e1e),
            button: rgb(0x343434),
            button_border: rgb(0x474747),
            hover: rgb(0x3d3d3d),
            pressed: rgb(0x454545),
            text: rgb(0xe4e4e4),
            text_secondary: rgb(0xa8a8a8),
            text_tertiary: rgb(0x7a7a7a),
            text_disabled: rgb(0x585858),
            track: rgb(0x4a4a4a),
            knob: rgb(0xd8d8d8),
            knob_border: rgb(0x202020),
            accent: rgb(0x5b9bd5),
            danger: rgb(0xd9534f),
            sidebar_w: 220.0,
            control_bar_h: 34.0,
            status_h: 18.0,
            row_h: 22.0,
            menu_bar_h: 24.0,
            dark: true,
        }
    }

    /// A light set (the toggle target; the shell does not persist a choice yet).
    pub fn light() -> Self {
        let rgb = |v: u32| Color32::from_rgb((v >> 16) as u8, (v >> 8) as u8, v as u8);
        Tokens {
            chrome: rgb(0xececec),
            canvas: rgb(0xd0d0d0),
            divider: rgb(0xc8c8c8),
            inset: rgb(0xe2e2e2),
            field: rgb(0xf7f7f7),
            button: rgb(0xf4f4f4),
            button_border: rgb(0xb8b8b8),
            hover: rgb(0xdedede),
            pressed: rgb(0xd0d0d0),
            text: rgb(0x1c1c1c),
            text_secondary: rgb(0x555555),
            text_tertiary: rgb(0x8a8a8a),
            text_disabled: rgb(0xb0b0b0),
            track: rgb(0xb4b4b4),
            knob: rgb(0xffffff),
            knob_border: rgb(0x808080),
            accent: rgb(0x2f74c0),
            danger: rgb(0xc0392b),
            dark: false,
            ..Self::dark()
        }
    }

    /// The tokens installed on `ctx` (dark when none were installed).
    pub fn get(ctx: &egui::Context) -> Tokens {
        ctx.data(|d| d.get_temp::<Tokens>(egui::Id::NULL))
            .unwrap_or_default()
    }

    /// `accent` at `alpha` (0…1).
    pub fn accent_alpha(&self, alpha: f32) -> Color32 {
        self.accent.gamma_multiply(alpha)
    }

    /// Primary text at `alpha`.
    pub fn text_alpha(&self, alpha: f32) -> Color32 {
        self.text.gamma_multiply(alpha)
    }

    /// Body font.
    pub fn font(&self, size: f32) -> FontId {
        FontId::new(size, FontFamily::Proportional)
    }

    /// Monospaced digits for value read-outs.
    pub fn mono(&self, size: f32) -> FontId {
        FontId::new(size, FontFamily::Monospace)
    }
}

/// Install the default (dark) theme on `ctx` with the BUNDLED font (Inter) — deterministic, for
/// tests and the selftest. The app itself calls [`install_with`] with [`FontChoice::System`].
pub fn install(ctx: &egui::Context) {
    install_tokens(ctx, Tokens::dark());
    let _ = install_fonts(ctx, FontChoice::Bundled);
}

/// Which typeface the UI sets in.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum FontChoice {
    /// The operating system's own UI font (SF Pro, the desktop's font, Segoe UI), read at
    /// runtime; Inter when it can't be loaded.
    System,
    /// Inter, bundled — the same everywhere (tests, the selftest's screenshots).
    Bundled,
}

/// The default theme with the given font; returns a description of the face in use.
pub fn install_with(ctx: &egui::Context, font: FontChoice) -> String {
    install_tokens(ctx, Tokens::dark());
    install_fonts(ctx, font)
}

/// SF Pro `GRAD` axis value (400 = the font's default, 1000 = max) compensating for egui's
/// lighter rasterization.
const SYSTEM_FONT_GRADE: f32 = 600.0;

/// SF Pro `wght` for the medium family (400 regular, 510 Medium, 590 Semibold): a slight step up
/// for toggle labels and panel titles.
const SYSTEM_FONT_MEDIUM_WEIGHT: f32 = 510.0;

/// The medium-weight proportional family (see [`Tokens::font_medium`]).
pub fn medium_family() -> FontFamily {
    FontFamily::Name("medium".into())
}

/// The medium weight at `size` — the regular face where the font has no weight axis, and plain
/// proportional before the theme's fonts are installed (egui panics on an unknown family).
pub fn font_medium(ctx: &egui::Context, size: f32) -> FontId {
    let family = medium_family();
    if ctx.fonts(|f| f.definitions().families.contains_key(&family)) {
        FontId::new(size, family)
    } else {
        FontId::proportional(size)
    }
}

/// Inter Regular (SIL OFL 1.1, `assets/fonts/`): the fallback UI font.
const INTER: &[u8] = include_bytes!("../../../assets/fonts/Inter-Regular.ttf");

/// The proportional family, in priority order: the system UI font (when chosen and found), then
/// bundled Inter, then egui's own faces (Ubuntu Light, Noto Emoji, its emoji/icon font), then
/// Hack at the very end — it carries the plain symbols the UI draws (`↶ ↷ ▸ ⇧`, geometric
/// shapes) that the text faces lack, so they don't render as boxes. Monospace is untouched.
fn install_fonts(ctx: &egui::Context, choice: FontChoice) -> String {
    let mut fonts = egui::FontDefinitions::default();
    let mut front: Vec<String> = Vec::new();
    let mut medium_front: Vec<String> = Vec::new();
    let mut description = "Inter (bundled)".to_owned();
    if choice == FontChoice::System
        && let Some(sys) = crate::system_font::discover()
    {
        let mut data = egui::FontData {
            font: std::borrow::Cow::Owned(sys.bytes),
            index: sys.index,
            tweak: Default::default(),
        };
        // SF Pro is a variable font with an optical-size axis: pin it to text size so UI labels
        // get the "SF Text" letterforms AppKit uses at these sizes.
        let opsz = data
            .variation_axes()
            .into_iter()
            .find(|a| a.tag == egui::epaint::text::Tag::new(b"opsz"))
            .map(|a| 13.0f32.clamp(a.range.min, a.range.max));
        if let Some(v) = opsz {
            data.tweak.coords.push(b"opsz", v);
        }
        // egui rasterizes without CoreText's stem darkening, so the same face reads visibly
        // lighter than in an AppKit app. SF Pro's grade axis thickens strokes WITHOUT changing
        // advance widths (unlike `wght`), so layout is untouched. OATS_FONT_GRADE
        // overrides for tuning.
        let grade = std::env::var("OATS_FONT_GRADE")
            .ok()
            .and_then(|v| v.parse::<f32>().ok())
            .unwrap_or(SYSTEM_FONT_GRADE);
        if let Some(axis) = data
            .variation_axes()
            .into_iter()
            .find(|a| a.tag == egui::epaint::text::Tag::new(b"GRAD"))
        {
            data.tweak
                .coords
                .push(b"GRAD", grade.clamp(axis.range.min, axis.range.max));
        }
        // The medium face: the same font with its weight axis raised (egui has no bold —
        // `RichText::strong` only changes colour).
        if data
            .variation_axes()
            .into_iter()
            .any(|a| a.tag == egui::epaint::text::Tag::new(b"wght"))
        {
            let mut medium = data.clone();
            medium.tweak.coords.push(b"wght", SYSTEM_FONT_MEDIUM_WEIGHT);
            fonts
                .font_data
                .insert("system-ui-medium".to_owned(), std::sync::Arc::new(medium));
            medium_front.push("system-ui-medium".to_owned());
        }
        fonts
            .font_data
            .insert("system-ui".to_owned(), std::sync::Arc::new(data));
        front.push("system-ui".to_owned());
        description = sys.description;
    }
    fonts.font_data.insert(
        "Inter".to_owned(),
        std::sync::Arc::new(egui::FontData::from_static(INTER)),
    );
    front.push("Inter".to_owned());
    let hack = fonts
        .families
        .get(&FontFamily::Monospace)
        .and_then(|f| f.first())
        .cloned();
    if let Some(prop) = fonts.families.get_mut(&FontFamily::Proportional) {
        for (i, name) in front.into_iter().enumerate() {
            prop.insert(i, name);
        }
        if let Some(hack) = hack
            && !prop.contains(&hack)
        {
            prop.push(hack);
        }
    }
    // The medium family always exists (an unknown family panics in egui): the medium face when
    // there is one, then the whole proportional chain.
    let mut medium_chain = medium_front;
    if let Some(prop) = fonts.families.get(&FontFamily::Proportional) {
        medium_chain.extend(prop.iter().cloned());
    }
    fonts.families.insert(medium_family(), medium_chain);
    ctx.set_fonts(fonts);
    description
}

/// Install `t` as the active theme: visuals for BOTH egui themes so a system light/dark follow
/// can never swap in egui's stock look, plus spacing and text styles.
pub fn install_tokens(ctx: &egui::Context, t: Tokens) {
    ctx.options_mut(|o| {
        // ⌘+ / ⌘- must never resize egui itself.
        o.zoom_with_keyboard = false;
        // A wheel notch scrolls the transcript a comfortable distance.
        o.input_options.line_scroll_speed = 60.0;
    });
    ctx.data_mut(|d| d.insert_temp(egui::Id::NULL, t));
    let mut v = if t.dark {
        Visuals::dark()
    } else {
        Visuals::light()
    };
    v.panel_fill = t.chrome;
    v.window_fill = t.chrome;
    v.extreme_bg_color = t.field;
    v.faint_bg_color = t.inset;
    v.window_stroke = Stroke::new(1.0, t.button_border);
    v.window_corner_radius = CornerRadius::same(6);
    v.menu_corner_radius = CornerRadius::same(6);
    v.selection.bg_fill = t.accent.gamma_multiply(0.55);
    v.selection.stroke = Stroke::new(1.0, t.accent);
    v.override_text_color = Some(t.text);
    v.hyperlink_color = t.accent;
    v.popup_shadow = egui::epaint::Shadow {
        offset: [0, 4],
        blur: 14,
        spread: 0,
        color: Color32::from_black_alpha(if t.dark { 140 } else { 50 }),
    };
    v.window_shadow = v.popup_shadow;
    let w = &mut v.widgets;
    craft_ui::theme::WidgetPalette {
        noninteractive: t.chrome,
        inactive: t.button,
        hovered: t.hover,
        active: t.pressed,
        open: t.hover,
    }
    .apply_to(w, CornerRadius::same(4), Stroke::new(1.0, t.text));
    w.noninteractive.bg_stroke = Stroke::new(1.0, t.divider);
    w.noninteractive.fg_stroke = Stroke::new(1.0, t.text_secondary);
    w.inactive.bg_stroke = Stroke::new(1.0, t.button_border);
    w.inactive.fg_stroke = Stroke::new(1.0, t.text);
    w.hovered.bg_stroke = Stroke::new(1.0, t.button_border);
    w.active.bg_stroke = Stroke::new(1.0, t.accent);
    ctx.set_visuals_of(egui::Theme::Dark, v.clone());
    ctx.set_visuals_of(egui::Theme::Light, v);
    ctx.set_theme(if t.dark {
        egui::Theme::Dark
    } else {
        egui::Theme::Light
    });
    ctx.all_styles_mut(|s| {
        s.spacing.item_spacing = egui::vec2(6.0, 4.0);
        s.spacing.button_padding = egui::vec2(8.0, 3.0);
        s.spacing.interact_size.y = 20.0;
        s.spacing.menu_margin = egui::Margin::same(6);
        s.text_styles
            .insert(egui::TextStyle::Body, FontId::proportional(12.5));
        s.text_styles
            .insert(egui::TextStyle::Button, FontId::proportional(12.5));
        s.text_styles
            .insert(egui::TextStyle::Small, FontId::proportional(10.5));
        s.text_styles
            .insert(egui::TextStyle::Monospace, FontId::monospace(11.5));
        s.text_styles
            .insert(egui::TextStyle::Heading, FontId::proportional(15.0));
        s.animation_time = 0.12;
    });
}

/// sRGB float triple → `Color32`.
pub fn rgb_f(c: [f32; 3]) -> Color32 {
    let q = |v: f32| (v.clamp(0.0, 1.0) * 255.0 + 0.5) as u8;
    Color32::from_rgb(q(c[0]), q(c[1]), q(c[2]))
}

/// `Color(white: w)`.
pub fn gray_f(w: f32) -> Color32 {
    rgb_f([w, w, w])
}
