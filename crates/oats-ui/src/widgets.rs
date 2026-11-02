//! Small shared widgets whose job is alignment: a checkbox centred on the first line of the
//! text beside it, and the cap-height centre that both use.

use egui::{Color32, FontId, Rect, Response, Ui, UiBuilder, pos2, vec2};

/// The cap-height centre of one line of `font`, measured from the top of its row (points).
/// Read from the laid-out glyph of "H", so it holds for any face (SF Pro, Inter).
pub fn cap_center(ui: &Ui, font: &FontId) -> f32 {
    ink_center(ui, "H", font).unwrap_or(font.size * 0.6)
}

/// The vertical centre of the ink of `glyph` (its first character) set in `font`, measured
/// from the top of its row (points). `None` for a glyph without ink.
pub fn ink_center(ui: &Ui, glyph: &str, font: &FontId) -> Option<f32> {
    let galley = ui
        .ctx()
        .fonts_mut(|f| f.layout_no_wrap(glyph.to_owned(), font.clone(), Color32::WHITE));
    galley
        .rows
        .first()
        .and_then(|r| {
            r.glyphs
                .first()
                .map(|g| r.pos.y + g.pos.y + g.uv_rect.offset.y + g.uv_rect.size.y / 2.0)
        })
        .filter(|y| y.is_finite() && *y > 0.0)
}

/// Paint `glyph` in `font` so the centre of its ink sits at `center` (x: its advance width
/// centred). For icon-like characters ("×") that must line up with a line of text.
pub fn paint_glyph_centered(
    ui: &Ui,
    center: egui::Pos2,
    glyph: &str,
    font: &FontId,
    color: Color32,
) {
    let galley = ui
        .ctx()
        .fonts_mut(|f| f.layout_no_wrap(glyph.to_owned(), font.clone(), color));
    let dy = ink_center(ui, glyph, font).unwrap_or(galley.size().y / 2.0);
    let pos = pos2(center.x - galley.size().x / 2.0, center.y - dy);
    ui.painter().galley(pos, galley, color);
}

/// A checkbox for a row whose text is set in `font`: it takes a slot as tall as one line of
/// that font (so a label beside it, top- or centre-aligned, starts on the same top) and paints
/// the box centred on the line's cap height. True in `changed()` when the user toggled it.
pub fn check_box(ui: &mut Ui, checked: &mut bool, font: &FontId) -> Response {
    let row_h = ui.ctx().fonts_mut(|f| f.row_height(font));
    let cap = cap_center(ui, font);
    let side = ui.spacing().interact_size.y;
    let (slot, _) = ui.allocate_exact_size(vec2(side, row_h), egui::Sense::hover());
    let box_rect =
        Rect::from_center_size(pos2(slot.center().x, slot.top() + cap), vec2(side, side));
    let mut child = ui.new_child(
        UiBuilder::new()
            .max_rect(box_rect)
            .layout(egui::Layout::left_to_right(egui::Align::Center)),
    );
    child.spacing_mut().interact_size.y = side;
    child.add(egui::Checkbox::without_text(checked))
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The box's centre and the label's cap-height centre are on the same line (±1 pt), in a
    /// top-aligned and a centre-aligned row, for the text sizes the app uses.
    #[test]
    fn the_box_is_centred_on_the_labels_cap_height() {
        let ctx = egui::Context::default();
        crate::theme::install(&ctx);
        for size in [12.5_f32, 13.0, 15.0] {
            for centred in [false, true] {
                let mut out = None;
                let mut output = ctx.run_ui(egui::RawInput::default(), |ui| {
                    let font = FontId::proportional(size);
                    let layout = if centred {
                        egui::Layout::left_to_right(egui::Align::Center)
                    } else {
                        egui::Layout::left_to_right(egui::Align::Min)
                    };
                    ui.with_layout(layout, |ui| {
                        let mut on = true;
                        let b = check_box(ui, &mut on, &font);
                        let l = ui.label(egui::RichText::new("Hello").font(font.clone()));
                        let cap = cap_center(ui, &font);
                        out = Some((b.rect.center().y, l.rect.top() + cap));
                    });
                });
                output.textures_delta.clear();
                let (bx, cap) = out.expect("laid out");
                assert!(
                    (bx - cap).abs() <= 1.0,
                    "size {size}: box {bx} vs cap {cap}"
                );
            }
        }
    }
}
