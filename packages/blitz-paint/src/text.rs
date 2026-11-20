use anyrender::{AntiAliasing, PaintScene};
use blitz_dom::{BaseDocument, NodeId, node::TextBrush};
use kurbo::{Affine, Rect};
use parley::{GlyphRun, Layout, Line, PositionedLayoutItem};
use peniko::Fill;
use std::sync::Arc;

use crate::FONT_EMBOLDEN_ENABLED;
use crate::color::Color;
use crate::text_decorations::{
    DecorationRunGeometry, DecorationStackEntry, LineDecoration, ResolvedTextShadow,
    WinAscentCache, draw_line_text_shadows, flush_line_decorations, resolve_decoration_entry,
};

/// Per glyph run values recorded while walking a line, so the line can be painted in
/// passes (shadows beneath, then glyphs) without re-resolving styles.
pub(crate) struct RunRecord {
    pub(crate) fill_color: Color,
    pub(crate) shadows: Option<Arc<[ResolvedTextShadow]>>,
}

/// Reusable scratch storage for drawing text across all inline formatting contexts in a
/// document. The vectors are cleared between uses without releasing their allocations.
#[derive(Default)]
pub(crate) struct DrawTextContext {
    stack: Vec<DecorationStackEntry>,
    path_scratch: Vec<NodeId>,
    deco_boxes: Vec<LineDecoration>,
    run_records: Vec<RunRecord>,
    win_ascent_ratios: WinAscentCache,
}

#[derive(Clone, Copy)]
pub(crate) struct TextPaintOptions {
    pub scale: f64,
    pub brush_override: Option<Color>,
    pub runs: RunFilter,
}

/// Which glyph runs of a layout to paint.
///
/// Non-atomic inline elements with an `opacity` below 1 are painted as groups of their own,
/// each of which paints the runs in it but not those in the groups nested in it.
#[derive(Clone, Copy)]
pub(crate) enum RunFilter {
    All,
    /// The runs of the given group, or those in no group.
    Group(Option<NodeId>),
}

pub(crate) fn stroke_text<'a>(
    scene: &mut impl PaintScene,
    lines: impl Iterator<Item = Line<'a, TextBrush>>,
    doc: &BaseDocument,
    transform: Affine,
    options: TextPaintOptions,
    inline_root_id: NodeId,
    context: &mut DrawTextContext,
) {
    let TextPaintOptions {
        scale,
        brush_override,
        runs,
    } = options;
    let DrawTextContext {
        stack,
        path_scratch,
        deco_boxes,
        run_records,
        win_ascent_ratios,
    } = context;
    stack.clear();
    path_scratch.clear();
    deco_boxes.clear();
    run_records.clear();

    // Persistent stack mirroring the ancestor path (inline root -> current run's
    // node) as we walk the runs. The `text-decoration-*` properties are *not*
    // inherited; instead a decoration set on an ancestor is propagated to the
    // descendant text it wraps. Rather than re-resolving styles for the whole
    // ancestor chain on every run, we cache each node's resolved values here and,
    // for each run, only resolve styles for the nodes newly descended into (popping
    // as we ascend). `path_scratch` is a reusable buffer for the run's node path.
    for line in lines {
        // Decorations accumulated for this line, keyed by decorating box, so each box is
        // painted once (spanning all its runs) using its own font — matching Firefox, which
        // draws one decoration per box rather than one stepped segment per differently-sized
        // run. Clearing preserves the allocation for the next line and inline context.
        deco_boxes.clear();
        run_records.clear();
        let mut line_has_shadows = false;
        let decorated_end = decorated_end(&line);

        // Pass 1: resolve styles for every glyph run on the line (recording the values
        // needed to paint it) and accumulate decorations. Painting is deferred so text
        // shadows can be drawn beneath *all* of the line's text and decorations.
        for item in line.items() {
            let PositionedLayoutItem::GlyphRun(glyph_run) = item else {
                continue;
            };
            let run_index = run_records.len();
            let run = glyph_run.run();
            let font = run.font();
            let font_size = run.font_size();
            let metrics = run.font_metrics();
            let style = glyph_run.style();

            let css_font_size = font_size as f64 / scale;

            // Reconcile the stack with this run's ancestor path. Build the path
            // (inline root -> run node), keep the shared prefix already on the stack,
            // pop the rest, and resolve styles only for the newly-descended nodes.
            path_scratch.clear();
            let mut walk_id = Some(style.brush.id);
            while let Some(node_id) = walk_id {
                path_scratch.push(node_id);
                if node_id == inline_root_id {
                    break;
                }
                walk_id = doc.get_node(node_id).and_then(|node| node.parent);
            }
            path_scratch.reverse();

            let shared = stack
                .iter()
                .zip(path_scratch.iter())
                .take_while(|(entry, node_id)| entry.node_id == **node_id)
                .count();
            stack.truncate(shared);
            for &node_id in &path_scratch[shared..] {
                let entry = resolve_decoration_entry(doc, node_id, stack.last(), scale);
                stack.push(entry);
            }

            // A run that isn't painted records no colour and no shadows, and contributes no
            // decorations.
            let is_painted = match runs {
                RunFilter::All => true,
                RunFilter::Group(group) => {
                    let run_group = stack.iter().rev().find(|e| e.is_opacity_group);
                    run_group.map(|e| e.node_id) == group
                }
            };
            if !is_painted {
                run_records.push(RunRecord {
                    fill_color: Color::TRANSPARENT,
                    shadows: None,
                });
                continue;
            }

            // The glyph colour and `text-shadow` come from the run's own node (the stack
            // top): both inherit, so the innermost inline element already carries the
            // right values. Shadows are not part of a `background-clip: text` mask, so
            // they are dropped when the brush is overridden.
            let (fill_color, shadows) = stack
                .last()
                .map(|e| (e.fill_color, e.text_shadows.clone()))
                .unwrap_or((Color::BLACK, None));
            let fill_color = brush_override.unwrap_or(fill_color);
            let shadows = shadows.filter(|_| brush_override.is_none());
            line_has_shadows |= shadows.is_some();
            run_records.push(RunRecord {
                fill_color,
                shadows,
            });

            // Accumulate this run's contribution to each decorating box on its ancestor
            // path. The decoration is drawn once per box after the whole line has been
            // walked (see `flush_line_decorations`), so mixed font sizes within a box
            // produce a single straight line rather than one stepped segment per run.
            let geometry = DecorationRunGeometry {
                baseline: glyph_run.baseline(),
                ascent: metrics.ascent,
                descent: metrics.descent,
                underline_offset: metrics.underline_offset,
                underline_size: metrics.underline_size,
                strikethrough_size: metrics.strikethrough_size,
                font: font.clone(),
                font_size,
                css_font_size,
            };
            let run_node_id = style.brush.id;
            let run_x0 = glyph_run.offset() as f64;
            let run_x1 = (run_x0 + glyph_run.advance() as f64).min(decorated_end);

            // (A run of only hanging whitespace decorates nothing.)
            for entry in stack.iter().filter(|_| run_x1 > run_x0) {
                if entry.decoration.is_none() {
                    continue;
                }
                let idx = match deco_boxes.iter().position(|d| d.node_id == entry.node_id) {
                    Some(idx) => idx,
                    None => {
                        deco_boxes.push(LineDecoration {
                            node_id: entry.node_id,
                            deco: entry.decoration.clone().unwrap(),
                            min_x: f64::INFINITY,
                            max_x: f64::NEG_INFINITY,
                            runs: run_index..run_index,
                            own: None,
                            first: None,
                        });
                        deco_boxes.len() - 1
                    }
                };
                let acc = &mut deco_boxes[idx];
                acc.min_x = acc.min_x.min(run_x0);
                acc.max_x = acc.max_x.max(run_x1);
                acc.runs.end = run_index + 1;
                if acc.first.is_none() {
                    acc.first = Some(geometry.clone());
                }
                // A run whose innermost node is the box itself is the box's own text, so
                // its font is the one Firefox positions and sizes the decoration with.
                if entry.node_id == run_node_id {
                    acc.own = Some(geometry.clone());
                }
            }
        }

        // Pass 2: text shadows, beneath all of the line's text and decorations.
        if line_has_shadows {
            draw_line_text_shadows(
                scene,
                &line,
                run_records,
                deco_boxes,
                win_ascent_ratios,
                transform,
                options,
                inline_root_id,
            );
        }

        // Pass 3: the glyphs themselves.
        for (glyph_run, record) in glyph_runs(&line).zip(run_records.iter()) {
            draw_glyph_run(scene, &glyph_run, record.fill_color, transform);
        }

        // Pass 4: decorations, on top of the glyphs.
        flush_line_decorations(
            scene,
            transform,
            options,
            deco_boxes,
            win_ascent_ratios,
            inline_root_id,
            &line,
            None,
        );
    }
}

/// Where a line's decorations end: before the whitespace that hangs past its end, which
/// browsers don't paint. Only left-to-right lines are trimmed, where that whitespace is at
/// the right end.
fn decorated_end(line: &Line<'_, TextBrush>) -> f64 {
    let hanging = line.metrics().hanging_advance as f64;
    let mut end = f64::NEG_INFINITY;
    for glyph_run in glyph_runs(line) {
        if glyph_run.run().is_rtl() {
            return f64::INFINITY;
        }
        end = end.max((glyph_run.offset() + glyph_run.advance()) as f64);
    }
    if hanging > 0.0 {
        end - hanging
    } else {
        f64::INFINITY
    }
}

/// The glyph runs of a line, in order (matching the order of the recorded `RunRecord`s).
pub(crate) fn glyph_runs<'a, 'b>(
    line: &'b Line<'a, TextBrush>,
) -> impl Iterator<Item = GlyphRun<'a, TextBrush>> + 'b {
    line.items().filter_map(|item| match item {
        PositionedLayoutItem::GlyphRun(glyph_run) => Some(glyph_run),
        PositionedLayoutItem::InlineBox(_) => None,
    })
}

/// Draw a glyph run's glyphs in a single colour.
pub(crate) fn draw_glyph_run(
    scene: &mut impl PaintScene,
    glyph_run: &GlyphRun<'_, TextBrush>,
    color: Color,
    transform: Affine,
) {
    let run = glyph_run.run();
    let font_size = run.font_size();
    // Zero-size text is invisible, but hinting can give its glyphs ink. Fully
    // transparent text is skipped too: backends draw color (emoji) glyphs opaque.
    if font_size <= 0.0 || color.components[3] <= 0.0 {
        return;
    }
    let glyph_xform = run
        .synthesis()
        .skew()
        .map(|angle| Affine::skew(angle.to_radians().tan() as f64, 0.0));

    let mut embolden = if FONT_EMBOLDEN_ENABLED {
        font_embolden(font_size as f64, color, transform)
    } else {
        kurbo::Vec2::default()
    };
    if run.synthesis().embolden() {
        embolden += synthetic_bold(font_size as f64);
    }

    let normalized_coords: &[i16] = bytemuck::cast_slice(run.normalized_coords());
    scene.draw_glyphs(
        run.font(),
        font_size,
        !FONT_EMBOLDEN_ENABLED,
        normalized_coords,
        embolden,
        Fill::NonZero,
        &anyrender::Paint::from(color),
        1.0,
        transform,
        glyph_xform,
        glyph_run.positioned_glyphs().map(|glyph| anyrender::Glyph {
            id: glyph.id as _,
            x: glyph.x,
            y: glyph.y,
        }),
        AntiAliasing::Enabled,
    );
}

/// The layout-space box covered by a glyph run: its advance horizontally and the font's
/// ascent/descent vertically.
pub(crate) fn glyph_run_rect(glyph_run: &GlyphRun<'_, TextBrush>) -> Rect {
    let metrics = glyph_run.run().font_metrics();
    let x0 = glyph_run.offset() as f64;
    let x1 = x0 + glyph_run.advance() as f64;
    let baseline = glyph_run.baseline() as f64;
    let y0 = baseline - metrics.ascent as f64;
    let y1 = baseline + metrics.descent as f64;
    Rect::new(x0.min(x1), y0.min(y1), x0.max(x1), y0.max(y1))
}

/// Which line breaks a range of text's highlight paints.
#[derive(Clone, Copy)]
pub(crate) enum LineBreaks {
    /// None: a find-in-page match.
    None,
    /// Those of a selection, which runs on past the layout's text when `continues` is set.
    Selected { continues: bool },
}

/// The highlight rectangles for the given byte range of a layout, with the index of the line
/// each is on. As Blink paints a selection (`InlineCursor::CurrentLocalSelectionRectForText`),
/// a line break the range covers is a box as wide as a space of the line's last font: after a
/// line's text when the range runs on past it (unless the line ends in a space, which the
/// range covers instead), and for a `<br>` or a preserved newline.
pub(crate) fn text_selection_rects(
    layout: &Layout<TextBrush>,
    selection_start: usize,
    selection_end: usize,
    line_breaks: LineBreaks,
) -> Vec<(Rect, usize)> {
    let range = selection_start..selection_end;
    let last_line = layout.len().saturating_sub(1);
    let mut rects = Vec::new();
    for (line_index, line) in layout.lines().enumerate() {
        let text = line.text_range();
        if text.end <= range.start || text.start >= range.end {
            continue;
        }
        let metrics = line.metrics();
        let top = f32::min(metrics.block_min_coord, metrics.content_block_min_coord) as f64;
        let bottom = f32::max(metrics.block_max_coord, metrics.content_block_max_coord) as f64;
        let mut x = metrics.offset as f64 + metrics.inline_min_coord as f64;
        let mut start_x = None;
        // The line's logically last cluster: whether it is a space or a newline, and its run.
        let mut last: Option<(usize, bool, parley::Run<'_, TextBrush>)> = None;
        for run in line.runs() {
            for cluster in run.visual_clusters() {
                let cluster_text = cluster.text_range();
                if range.contains(&cluster_text.start) {
                    start_x.get_or_insert(x);
                } else if let Some(x0) = start_x.take() {
                    rects.push((Rect::new(x0, top, x, bottom), line_index));
                }
                x += cluster.advance() as f64;
                if last
                    .as_ref()
                    .is_none_or(|(end, ..)| cluster_text.end > *end)
                {
                    let ends_line = cluster.is_hard_line_break() || cluster.is_space_or_nbsp();
                    last = Some((cluster_text.end, ends_line, run));
                }
            }
        }
        let line_break = match (line_breaks, &last) {
            (LineBreaks::Selected { continues }, Some((_, false, run)))
                if !run.is_rtl()
                    && !matches!(
                        line.items().last(),
                        Some(PositionedLayoutItem::InlineBox(_))
                    )
                    && (range.end > text.end || continues && line_index == last_line) =>
            {
                Some(run)
            }
            (LineBreaks::Selected { .. }, Some((end, true, run)))
                if line.break_reason() == parley::BreakReason::Explicit
                    && !run.is_rtl()
                    && range.contains(&(end - 1)) =>
            {
                Some(run)
            }
            _ => None,
        };
        let x1 = x + line_break.map_or(0.0, |run| space_width(run) as f64);
        if let Some(x0) = start_x.or((x1 > x).then_some(x)) {
            rects.push((Rect::new(x0, top, x1, bottom), line_index));
        }
    }
    rects
}

/// The advance of a space in a run's font, which Blink paints a selected line break as wide
/// as (`Font::SpaceWidth`).
fn space_width(run: &parley::Run<'_, TextBrush>) -> f32 {
    use skrifa::MetadataProvider as _;
    use skrifa::instance::{LocationRef, Size};
    let font = run.font();
    let Ok(font_ref) = skrifa::FontRef::from_index(font.data.as_ref(), font.index) else {
        return 0.0;
    };
    let Some(space) = font_ref.charmap().map(' ') else {
        return 0.0;
    };
    let coords: &[skrifa::instance::NormalizedCoord] =
        bytemuck::cast_slice(run.normalized_coords());
    font_ref
        .glyph_metrics(Size::new(run.font_size()), LocationRef::new(coords))
        .advance_width(space)
        .unwrap_or(0.0)
}

/// A highlighted run of text's part on one line of its layout.
pub(crate) struct HighlightRect {
    pub(crate) rect: Rect,
    pub(crate) line: usize,
    pub(crate) background: Color,
    pub(crate) color: Option<Color>,
}

/// The boxes of `highlight` in `layout`: on each line, the selection's extent across its text,
/// over the content area of the text's font (its ascent above the baseline and its descent below,
/// rounded to CSS pixels), as Blink bounds a highlighted text fragment, snapped to device pixels.
pub(crate) fn text_highlight_rects(
    layout: &Layout<TextBrush>,
    highlight: &blitz_dom::TextHighlight,
    scale: f64,
    snap: crate::pixel_snap::PixelSnap,
) -> Vec<HighlightRect> {
    let round_to_css_pixel = |length: f64| (length / scale).round() * scale;
    let range = &highlight.range;
    text_selection_rects(layout, range.start, range.end, LineBreaks::None)
        .into_iter()
        .filter_map(|(rect, line_index)| {
            let line = layout.get(line_index)?;
            let metrics = line.metrics();
            let font = line
                .runs()
                .find(|run| {
                    let text = run.text_range();
                    text.start < range.end && text.end > range.start
                })
                .map(|run| *run.font_metrics());
            let baseline = metrics.baseline as f64;
            let (top, bottom) = match font {
                Some(font) => (
                    baseline - round_to_css_pixel(font.ascent as f64),
                    baseline + round_to_css_pixel(font.descent as f64),
                ),
                None => (
                    metrics.content_block_min_coord as f64,
                    metrics.content_block_max_coord as f64,
                ),
            };
            let rect = Rect::new(
                snap.x(rect.x0),
                snap.y(top),
                snap.x(rect.x1),
                snap.y(bottom),
            );
            Some(HighlightRect {
                rect,
                line: line_index,
                background: highlight.background,
                color: highlight.color,
            })
        })
        .collect()
}

/// How much a face without a bold is emboldened when bold is asked of it, per side, in the run's
/// units: Skia's fake bold, which strokes the outline `font_size / 24` wide at 9px, narrowing to
/// `font_size / 32` at 36px (`kStdFakeBoldInterpKeys`), and leaves advances alone.
fn synthetic_bold(font_size: f64) -> kurbo::Vec2 {
    let t = ((font_size - 9.0) / (36.0 - 9.0)).clamp(0.0, 1.0);
    let ratio = 1.0 / 24.0 + t * (1.0 / 32.0 - 1.0 / 24.0);
    let per_side = font_size * ratio / 2.0;
    kurbo::Vec2::new(per_side, per_side)
}

/// Approximates macOS font smoothing (stem dilation). CoreGraphics dilates each side by an amount
/// in device pixels that grows with the device ppem up to 0.3px (fitted to Safari's ink at 9-72px
/// and 1x-3x scale), and keeps dilating at large sizes, so there is no ppem cutoff. glifo applies
/// the embolden in user units, so the device amounts are divided by the transform's scale.
fn font_embolden(font_size: f64, color: Color, transform: Affine) -> kurbo::Vec2 {
    const MAX_DEVICE_EMBOLDEN: f64 = 0.3;
    let [a, b, c, d, _, _] = transform.as_coeffs();
    let (scale_x, scale_y) = (a.hypot(b), c.hypot(d));
    let device_ppem = font_size * (a * d - b * c).abs().sqrt();
    if scale_x == 0.0 || scale_y == 0.0 || !device_ppem.is_finite() {
        return kurbo::Vec2::default();
    }
    let fs = device_ppem * embolden_scale(color);
    kurbo::Vec2::new(
        (0.0121 * fs).min(MAX_DEVICE_EMBOLDEN) / scale_x,
        (0.015125 * fs).min(MAX_DEVICE_EMBOLDEN) / scale_y,
    )
}

/// Scales Pathfinder's stem-darkening factors by the text's relative luminance. CoreText
/// dilates light text more than dark text; the points are fitted to Safari's ink for text
/// colours from black to white on black and white backgrounds.
fn embolden_scale(color: Color) -> f64 {
    const POINTS: [(f64, f64); 4] = [(0.1, 0.5), (0.125, 0.72), (0.35, 0.72), (1.0, 1.3)];
    let lum = color.discard_alpha().relative_luminance() as f64;
    let mut prev = POINTS[0];
    if lum <= prev.0 {
        return prev.1;
    }
    for point in &POINTS[1..] {
        if lum <= point.0 {
            return prev.1 + (point.1 - prev.1) * (lum - prev.0) / (point.0 - prev.0);
        }
        prev = *point;
    }
    prev.1
}
