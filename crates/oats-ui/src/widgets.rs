//! Small shared widgets whose job is alignment: a checkbox centred on the first line of the
//! text beside it, and the cap-height centre that both use; and the reflow helpers every view
//! uses so a narrow window wraps or truncates instead of running off the right edge
//! ([`inset_wrapped`], [`truncated_galley`]).

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
///
/// The tick pops: on the change to checked the check mark scales in over [`TICK_POP`] with a
/// slight overshoot (`back_out`) and settles; unchecking fades it out over [`TICK_FADE`]. At
/// rest (and with motion off) it is egui's own `Checkbox`, pixel for pixel.
pub fn check_box(
    ui: &mut Ui,
    checked: &mut bool,
    font: &FontId,
    key: impl std::hash::Hash + std::fmt::Debug,
) -> Response {
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
    // What egui's `Checkbox::without_text` allocates (an `interact_size` square, the icon at
    // its left) and paints (its `checkbox_style`), drawn here by hand from the same style
    // values, so ONE painting path serves rest and motion: the mark is scaled in / faded out
    // in place, with nothing swapped underneath it (a second, bare checkbox under a painted
    // mark flickered at both ends and the mark landed elsewhere).
    let (rect, mut resp) = child.allocate_exact_size(egui::Vec2::splat(side), egui::Sense::click());
    if resp.clicked() {
        *checked = !*checked;
        resp.mark_changed();
    }
    let enabled = child.is_enabled();
    let now_checked = *checked;
    resp.widget_info(|| {
        egui::WidgetInfo::selected(egui::WidgetType::Checkbox, enabled, now_checked, "")
    });
    let ctx = child.ctx().clone();
    let time = match (motion(&ctx), *checked) {
        (false, _) => 0.0,
        (true, true) => TICK_POP,
        (true, false) => TICK_FADE,
    };
    // linear progress; the easing is applied when painting (`back_out` overshoots past 1).
    // Keyed on the ITEM (`key`), not on the widget's positional id: when a row's slot goes
    // away every checkbox below it takes the next id, and an unchecked box inheriting a
    // checked one's state flashed a fading mark.
    let p = ctx.animate_bool_with_time(egui::Id::new(("tick-pop", key)), *checked, time);
    if !child.is_rect_visible(rect) {
        return resp;
    }
    let cs = child
        .style()
        .checkbox_style(&egui::widget_style::Classes::default(), resp.widget_state());
    // egui's atom layout centres the icon in the square (a left-to-right layout's main
    // alignment is Center), so the box sits in the middle of `rect`, not at its left
    let big = Rect::from_center_size(rect.center(), egui::Vec2::splat(cs.checkbox_size));
    let small = Rect::from_center_size(big.center(), egui::Vec2::splat(cs.check_size));
    child.painter().add(egui::epaint::RectShape::new(
        big.expand(cs.checkbox_frame.inner_margin.left.into()),
        cs.checkbox_frame.corner_radius,
        cs.checkbox_frame.fill,
        cs.checkbox_frame.stroke,
        egui::StrokeKind::Inside,
    ));
    let (scale, alpha) = if *checked {
        (egui::emath::easing::back_out(p), 1.0)
    } else {
        (1.0, p)
    };
    if scale > 0.0 && alpha > 0.0 {
        let mut stroke = cs.check_stroke;
        stroke.color = stroke.color.gamma_multiply(alpha);
        let small = Rect::from_center_size(small.center(), small.size() * scale);
        child.painter().add(egui::Shape::line(
            vec![
                pos2(small.left(), small.center().y),
                pos2(small.center().x, small.bottom()),
                pos2(small.right(), small.top()),
            ],
            stroke,
        ));
    }
    resp
}

/// The tick's pop when a box is checked (seconds, `back_out`).
pub const TICK_POP: f32 = 0.15;
/// The tick's fade when a box is unchecked (seconds, linear).
pub const TICK_FADE: f32 = 0.1;

/// Whether anything moves: egui's `animation_time` of 0 (tests, a reduced-motion setup) turns
/// every animation here into an instant change.
pub fn motion(ctx: &egui::Context) -> bool {
    ctx.global_style().animation_time > 0.0
}

/// [`reveal`]'s opening time (seconds, `cubic_out`: a fast start that settles).
pub const REVEAL_OPEN: f32 = 0.22;
/// [`reveal`]'s closing time (seconds, `cubic_in_out`).
pub const REVEAL_CLOSE: f32 = 0.18;

/// What [`reveal`] remembers per slot between frames.
#[derive(Clone, Copy, Debug)]
struct RevealMemo {
    /// The content's full height (the cursor advance it takes when shown in full).
    height: Option<f32>,
    /// The frame the slot was last laid out (or asked about).
    frame: u64,
}

/// An animated slot in a top-down layout: `add` laid out in full when `open` and settled
/// (exactly as if called on `ui`), nothing when closed and settled (`None`), and in between
/// clipped to `openness × full height` with the content sliding — it moves down into place
/// while opening and up out of the slot while closing, so the rows below follow smoothly.
/// Opening takes [`REVEAL_OPEN`] (`cubic_out`), closing [`REVEAL_CLOSE`] (`cubic_in_out`);
/// egui's animation requests repaints only while the value moves, so a settled slot costs
/// nothing.
///
/// A slot that was not laid out in the previous frame (first sight, a view just opened, a
/// list scrolled in from another page) shows its state at once — nothing animates on arrival.
/// So does every slot while [`motion`] is off.
pub fn reveal<R>(
    ui: &mut Ui,
    id: egui::Id,
    open: bool,
    add: impl FnOnce(&mut Ui) -> R,
) -> Option<R> {
    let ctx = ui.ctx().clone();
    let memo_id = id.with("reveal-memo");
    let frame = ctx.cumulative_frame_nr();
    let memo = ctx.data(|d| d.get_temp::<RevealMemo>(memo_id));
    let arrived = memo.is_none_or(|m| m.frame + 1 < frame);
    let time = if !motion(&ctx) || arrived {
        0.0
    } else if open {
        REVEAL_OPEN
    } else {
        REVEAL_CLOSE
    };
    let easing = if open {
        egui::emath::easing::cubic_out
    } else {
        egui::emath::easing::cubic_in_out
    };
    let t = ctx.animate_bool_with_time_and_easing(id, open, time, easing);
    let height = memo.and_then(|m| m.height);
    let save = |height: Option<f32>| {
        ctx.data_mut(|d| d.insert_temp(memo_id, RevealMemo { height, frame }));
    };
    if t <= 0.0 {
        save(height);
        return None;
    }
    if t >= 1.0 {
        let top = ui.cursor().top();
        let r = add(ui);
        save(Some(ui.cursor().top() - top));
        return Some(r);
    }
    // Part way: the slot is `t` of the last full height; the content's bottom sits on the
    // slot's bottom (so it slides), clipped to the slot. A height not measured yet (the first
    // frame of a slot never shown in full) shows nothing this frame and is measured.
    let full = height.unwrap_or(0.0);
    let shown = (t * full).round().max(0.0);
    let avail = ui.available_rect_before_wrap();
    let top = avail.top();
    let content_top = top + shown - full;
    let mut child = ui.new_child(
        UiBuilder::new()
            .id_salt(id.with("reveal-body"))
            .max_rect(Rect::from_min_max(
                pos2(avail.left(), content_top),
                pos2(avail.right(), avail.bottom().max(content_top)),
            ))
            .layout(*ui.layout()),
    );
    let mut clip = ui.clip_rect();
    clip.min.y = clip.min.y.max(top);
    clip.max.y = clip.max.y.min(top + shown);
    child.set_clip_rect(clip);
    let r = add(&mut child);
    save(Some(child.cursor().top() - content_top));
    let used = child.min_rect();
    ui.add_space(shown);
    if shown > 0.0 {
        ui.expand_to_include_rect(Rect::from_min_max(
            pos2(used.left().max(avail.left()), top),
            pos2(used.right().min(avail.right()), top + shown),
        ));
    }
    Some(r)
}

/// Give the slot `to` the full height last measured for `from` when it has none yet, so a slot
/// opening for the first time (a row's new place) slides from its first frame.
pub fn reveal_seed_height(ctx: &egui::Context, from: egui::Id, to: egui::Id) {
    ctx.data_mut(|d| {
        let from = d.get_temp::<RevealMemo>(from.with("reveal-memo"));
        let to_id = to.with("reveal-memo");
        let has = d
            .get_temp::<RevealMemo>(to_id)
            .is_some_and(|m| m.height.is_some());
        if let Some(RevealMemo {
            height: Some(h),
            frame,
        }) = from
            && !has
        {
            let frame_to = d.get_temp::<RevealMemo>(to_id).map_or(frame, |m| m.frame);
            d.insert_temp(
                to_id,
                RevealMemo {
                    height: Some(h),
                    frame: frame_to,
                },
            );
        }
    });
}

/// Settle the slot `id` in state `open` now, with no animation (a jump that must find the
/// content laid out in full this frame, e.g. scrolling to a note's card).
pub fn reveal_snap(ctx: &egui::Context, id: egui::Id, open: bool) {
    ctx.animate_bool_with_time(id, open, 0.0);
}

/// A wrapping row inset `left` and `right` points from `ui`'s edges: widgets flow left to
/// right and wrap onto the next line at the same `left` inset (a hanging indent, unlike a
/// bare `horizontal_wrapped` with a leading `add_space`, whose second line starts at the
/// edge). A wrapping `Label` inside starts after the widget before it and continues on the
/// line below; a widget that doesn't fit after it (a chip) moves to the next line. Never
/// wider than `ui`.
pub fn inset_wrapped<R>(
    ui: &mut Ui,
    left: f32,
    right: f32,
    add: impl FnOnce(&mut Ui) -> R,
) -> egui::InnerResponse<R> {
    let max = ui.max_rect();
    let top = ui.cursor().top();
    let rect = Rect::from_min_max(
        pos2(max.left() + left, top),
        pos2((max.right() - right).max(max.left() + left + 1.0), top),
    );
    ui.scope_builder(
        UiBuilder::new()
            .max_rect(rect)
            .layout(egui::Layout::left_to_right(egui::Align::Center).with_main_wrap(true)),
        add,
    )
}

/// `text` in `font` on one line, cut with "…" to at most `max_w` points.
pub fn truncated_galley(
    ui: &Ui,
    text: impl Into<String>,
    font: FontId,
    color: Color32,
    max_w: f32,
) -> std::sync::Arc<egui::Galley> {
    let mut job = egui::text::LayoutJob::simple_singleline(text.into(), font, color);
    job.wrap = egui::text::TextWrapping::truncate_at_width(max_w.max(1.0));
    ui.painter().layout_job(job)
}

/// The width of a plain `egui::Button` with `text` at `size` points (its label plus the
/// button padding), for laying out a row of buttons before drawing it.
pub fn button_width(ui: &Ui, text: &str, size: f32) -> f32 {
    ui.painter()
        .layout_no_wrap(text.to_owned(), FontId::proportional(size), Color32::WHITE)
        .size()
        .x
        + 2.0 * ui.spacing().button_padding.x
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
                        let b = check_box(ui, &mut on, &font, 1);
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

    /// Run one frame at egui time `t` with motion on; `f` draws.
    fn frame(ctx: &egui::Context, t: f64, f: impl FnMut(&mut Ui)) -> egui::FullOutput {
        let input = egui::RawInput {
            time: Some(t),
            predicted_dt: 1.0 / 60.0,
            ..Default::default()
        };
        let mut f = f;
        let mut out = ctx.run_ui(input, |ui| f(ui));
        out.textures_delta.clear();
        out
    }

    /// The tick pops in over a few frames after the change, then nothing repaints; a box
    /// first drawn checked doesn't animate.
    #[test]
    fn the_tick_pops_then_settles() {
        let ctx = egui::Context::default();
        crate::theme::install(&ctx);
        let font = FontId::proportional(13.0);
        let mut on = true;
        for i in 0..3 {
            frame(&ctx, f64::from(i) / 60.0, |ui| {
                check_box(ui, &mut on, &font, 1);
            });
        }
        assert!(!ctx.has_requested_repaint(), "first sight: at rest");
        // a change to checked starts the pop (as the click would: the value changes between frames)
        // unchecked: the tick fades out, then rests
        on = false;
        let mut t = 0.1;
        for _ in 0..20 {
            frame(&ctx, t, |ui| {
                check_box(ui, &mut on, &font, 1);
            });
            t += 1.0 / 60.0;
        }
        assert!(!ctx.has_requested_repaint(), "faded: at rest");
        on = true;
        let mut moving = 0;
        for _ in 0..30 {
            let out = frame(&ctx, t, |ui| {
                check_box(ui, &mut on, &font, 1);
            });
            if out.viewport_output[&egui::ViewportId::ROOT].repaint_delay
                == std::time::Duration::ZERO
            {
                moving += 1;
            }
            t += 1.0 / 60.0;
        }
        assert!((2..=12).contains(&moving), "popped over {moving} frames");
        assert!(!ctx.has_requested_repaint(), "settled: idle");
    }

    /// A reveal: nothing when closed, part of the height while moving, all of it when open
    /// (exactly the content's own advance), and idle once settled.
    #[test]
    fn a_reveal_grows_from_nothing_to_the_contents_height() {
        let ctx = egui::Context::default();
        crate::theme::install(&ctx);
        let id = egui::Id::new("slot");
        let mut open = false;
        let mut heights = Vec::new();
        let mut t = 0.0;
        for i in 0..30 {
            if i == 2 {
                open = true;
            }
            frame(&ctx, t, |ui| {
                let top = ui.cursor().top();
                let shown = reveal(ui, id, open, |ui| {
                    for _ in 0..4 {
                        ui.label("row");
                    }
                });
                heights.push((shown.is_some(), ui.cursor().top() - top));
            });
            t += 1.0 / 60.0;
        }
        assert_eq!(heights[0], (false, 0.0), "closed: nothing");
        let full = heights.last().unwrap().1;
        assert!(full > 40.0);
        let partial = heights
            .iter()
            .filter(|(_, h)| *h > 0.0 && *h < full)
            .count();
        assert!(partial >= 3, "{heights:?}");
        assert!(
            heights.windows(2).all(|w| w[0].1 <= w[1].1),
            "grows monotonically: {heights:?}"
        );
        assert!(!ctx.has_requested_repaint(), "settled: idle");
    }
}
