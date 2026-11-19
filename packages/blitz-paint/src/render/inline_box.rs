//! Box decorations (backgrounds, borders and box shadows) of non-atomic inline elements.
//!
//! A non-atomic inline element is a style span of its inline root's text layout, which generates
//! one fragment per line box it is on. Its decorations are painted as if the element were never
//! broken and then sliced into its fragments (`box-decoration-break: slice`): each fragment
//! paints its part of a "strip", the element's box with its fragments laid end to end, so that
//! its inline-start edge is on its first fragment, its inline-end edge on its last, and
//! backgrounds continue from one fragment to the next.
//!
//! An inline element with an `opacity` below 1 is painted as a group of its own, after the rest
//! of its inline formatting context: its fragments, its text and the atomic inlines in it are
//! composited together.

use std::collections::HashMap;

use anyrender::{AntiAliasing, PaintScene};
use blitz_dom::CopyStyleGetters as _;
use blitz_dom::node::{InlineBoxRef, InlineEdge, TextBrush, TextLayout};
use blitz_dom::{BaseDocument, Node, NodeId};
use kurbo::{Affine, Rect, Shape as _};
use parley::{InlineBoxKind, Line, PositionedLayoutItem};
use peniko::{BlendMode, Fill};
use style::computed_values::direction::T as Direction;
use style::computed_values::visibility::T as Visibility;
use style::computed_values::white_space_collapse::T as WhiteSpaceCollapse;
use style::properties::ComputedValues;
use style::values::generics::image::GenericImage;

use super::BlitzDomPainter;
use crate::SELECTION_COLOR;
use crate::color::{Color, ToColorColor as _};
use crate::pixel_snap::PixelSnap;
use crate::text::HighlightRect;
use crate::text::{RunFilter, TextPaintOptions};

/// The line box fragment of a non-atomic inline element with box decorations.
///
/// Coordinates are in the layout units of the text layout (device pixels).
pub(crate) struct InlineFragment {
    node_id: NodeId,
    line_index: usize,
    /// The opacity group the fragment is painted in, if any.
    group: Option<NodeId>,
    is_ltr: bool,
    /// The left and right edges of the fragment's border box.
    x0: f64,
    x1: f64,
    /// The fragment's content area, from the element's own text if it has some on the line.
    content_area: Option<ContentArea>,
    has_own_content_area: bool,
    /// The offset of the fragment in the strip of its element's fragments, and the strip's width.
    strip_offset: f64,
    strip_width: f64,
    /// Whether the strip ends at the fragment's left and right edges.
    is_strip_left: bool,
    is_strip_right: bool,
}

/// The baseline of an inline box, and the ascent and descent of its first available font, which
/// bound its content area (CSS 2.1 §10.6.1).
#[derive(Clone, Copy)]
struct ContentArea {
    baseline: f64,
    ascent: f64,
    descent: f64,
}

/// The inline content of an inline root, as painted by [`BlitzDomPainter::draw_inline_content`].
struct InlineLines<'a> {
    inline_root: &'a Node,
    text_layout: &'a TextLayout,
    /// Maps the text layout's coordinates to the scene.
    transform: Affine,
    /// The inline size of the inline root's content box, in CSS pixels.
    containing_block_width: f32,
    boxes: &'a InlineBoxes,
}

/// A non-atomic inline element with an `opacity` below 1, which is painted as a group.
pub(crate) struct OpacityGroup {
    node_id: NodeId,
    /// The group it's nested in, if any.
    parent: Option<NodeId>,
    opacity: f32,
}

/// The fragments and opacity groups of the inline elements of a text layout.
pub(crate) struct InlineBoxes {
    fragments: Vec<InlineFragment>,
    groups: Vec<OpacityGroup>,
}

/// An inline element on the path from the inline root to the current item of a line.
struct PathEntry {
    node_id: NodeId,
    /// The innermost opacity group the element is in (itself included).
    group: Option<NodeId>,
    /// The element's left and right margins (which its edge boxes include) and direction, if it
    /// has box decorations.
    decorated: Option<(f64, f64, bool)>,
    /// The index of the element's fragment on the current line, once it has one.
    fragment: Option<usize>,
}

/// Collects the fragments of the inline elements with box decorations in a text layout, in the
/// order they paint in: line by line, and in tree order within a line. Also collects the
/// opacity groups, outermost first.
///
/// `containing_block_width` (in CSS pixels) is the inline size of the inline root's content box,
/// which percentage margins resolve against.
fn collect_inline_fragments(
    doc: &BaseDocument,
    text_layout: &TextLayout,
    inline_root: &Node,
    containing_block_width: f32,
    scale: f64,
) -> InlineBoxes {
    // The inline elements of an anonymous inline root are descendants of its parent
    let root_parent = inline_root
        .is_anonymous()
        .then_some(inline_root.parent)
        .flatten();
    let mut fragments = Vec::new();
    let mut groups: Vec<OpacityGroup> = Vec::new();
    let mut stack: Vec<PathEntry> = Vec::new();
    let mut path = Vec::new();

    let layout = &text_layout.layout;
    for (line_index, line) in layout.lines().enumerate() {
        for entry in &mut stack {
            entry.fragment = None;
        }
        // Collapsible white space at the end of the line is removed rather than painted (CSS Text
        // 3 §4.1.3), so it's outside of any fragment.
        let (mut line_start, mut line_end) = (f64::NEG_INFINITY, f64::INFINITY);
        if line.metrics().hanging_advance > 0.0 {
            let removed = removed_trailing_space(doc, &text_layout.text, &line) as f64;
            // The line's metrics don't include justification, so take its extent from its items
            let (min_x, max_x) = line.items().fold(
                (f64::INFINITY, f64::NEG_INFINITY),
                |(min_x, max_x), item| {
                    let (x0, x1) = match item {
                        PositionedLayoutItem::GlyphRun(run) => {
                            (run.offset(), run.offset() + run.advance())
                        }
                        PositionedLayoutItem::InlineBox(ibox) => (ibox.x, ibox.x + ibox.width),
                    };
                    (min_x.min(x0 as f64), max_x.max(x1 as f64))
                },
            );
            if layout.is_rtl() {
                line_start = min_x + removed;
            } else {
                line_end = max_x - removed;
            }
        }

        for item in line.items() {
            let (leaf_id, x0, x1, edge, content_area) = match &item {
                PositionedLayoutItem::GlyphRun(run) => {
                    let node_id = run.style().brush.id;
                    let clamp = |x: f32| (x as f64).max(line_start).min(line_end);
                    let (x0, x1) = (clamp(run.offset()), clamp(run.offset() + run.advance()));
                    let span = run.span_metrics();
                    let content_area = ContentArea {
                        baseline: run.baseline() as f64,
                        ascent: span.ascent as f64,
                        descent: span.descent as f64,
                    };
                    (node_id, x0, x1, None, Some(content_area))
                }
                PositionedLayoutItem::InlineBox(ibox) if ibox.kind == InlineBoxKind::InFlow => {
                    let (node_id, edge) = match InlineBoxRef::from_id(ibox.id) {
                        InlineBoxRef::Node(node_id) => (node_id, None),
                        InlineBoxRef::Edge(node_id, edge) => (node_id, Some(edge)),
                    };
                    let x0 = ibox.x as f64;
                    (node_id, x0, x0 + ibox.width as f64, edge, None)
                }
                PositionedLayoutItem::InlineBox(_) => continue,
            };

            // The inline elements the item is in, outermost first
            path.clear();
            let mut current = doc.get_node(leaf_id);
            while let Some(node) = current
                && node.id != inline_root.id
                && Some(node.id) != root_parent
            {
                if node.is_non_atomic_inline() {
                    path.push(node);
                }
                current = node.parent.and_then(|id| doc.get_node(id));
            }
            path.reverse();

            let shared = stack
                .iter()
                .zip(&path)
                .take_while(|(entry, node)| entry.node_id == node.id)
                .count();
            stack.truncate(shared);
            for node in &path[shared..] {
                let style = node.primary_styles();
                let parent_group = stack.last().and_then(|entry| entry.group);
                let opacity = style
                    .as_ref()
                    .map_or(1.0, |style| style.get_effects().opacity);
                let group = if opacity < 1.0 {
                    if !groups.iter().any(|group| group.node_id == node.id) {
                        groups.push(OpacityGroup {
                            node_id: node.id,
                            parent: parent_group,
                            opacity,
                        });
                    }
                    Some(node.id)
                } else {
                    parent_group
                };
                let decorated = style
                    .filter(|style| has_box_decorations(style))
                    .map(|style| {
                        let margin = node.inline_box_edges(containing_block_width).margin;
                        let is_ltr = style.clone_direction() == Direction::Ltr;
                        (
                            margin.left as f64 * scale,
                            margin.right as f64 * scale,
                            is_ltr,
                        )
                    });
                stack.push(PathEntry {
                    node_id: node.id,
                    group,
                    decorated,
                    fragment: None,
                });
            }

            for entry in &mut stack {
                let Some((margin_left, margin_right, is_ltr)) = entry.decorated else {
                    continue;
                };
                let index = *entry.fragment.get_or_insert_with(|| {
                    fragments.push(InlineFragment {
                        node_id: entry.node_id,
                        line_index,
                        group: entry.group,
                        is_ltr,
                        x0: f64::INFINITY,
                        x1: f64::NEG_INFINITY,
                        content_area: None,
                        has_own_content_area: false,
                        strip_offset: 0.0,
                        strip_width: 0.0,
                        is_strip_left: false,
                        is_strip_right: false,
                    });
                    fragments.len() - 1
                });
                let fragment = &mut fragments[index];

                // The element's own edge boxes include its margins, which are outside of its
                // border box. The inline-start edge is on the left in LTR, and on the right in RTL.
                let is_own = entry.node_id == leaf_id;
                let (mut x0, mut x1) = (x0, x1);
                if let Some(edge) = edge.filter(|_| is_own) {
                    if (edge == InlineEdge::Start) == is_ltr {
                        x0 += margin_left;
                    } else {
                        x1 -= margin_right;
                    }
                }
                fragment.x0 = fragment.x0.min(x0);
                fragment.x1 = fragment.x1.max(x1);

                if content_area.is_some()
                    && (fragment.content_area.is_none() || is_own && !fragment.has_own_content_area)
                {
                    fragment.content_area = content_area;
                    fragment.has_own_content_area = is_own;
                }
            }
        }
    }

    lay_out_strips(&mut fragments);
    InlineBoxes { fragments, groups }
}

/// The combined opacity of the non-atomic inline elements that `node` is in, up to its inline
/// root. Lacking boxes of their own, they don't otherwise apply it to the atomic inlines in them.
pub(crate) fn inline_ancestor_opacity(doc: &BaseDocument, node: &Node, inline_root: &Node) -> f32 {
    let root_parent = inline_root
        .is_anonymous()
        .then_some(inline_root.parent)
        .flatten();
    let mut opacity = 1.0;
    let mut current = node.parent.and_then(|id| doc.get_node(id));
    while let Some(ancestor) = current
        && ancestor.id != inline_root.id
        && Some(ancestor.id) != root_parent
    {
        if ancestor.is_non_atomic_inline() {
            opacity *= ancestor
                .primary_styles()
                .map_or(1.0, |style| style.get_effects().opacity);
        }
        current = ancestor.parent.and_then(|id| doc.get_node(id));
    }
    opacity
}

/// The advance of the collapsible white space at the end of `line`.
fn removed_trailing_space(doc: &BaseDocument, text: &str, line: &Line<'_, TextBrush>) -> f32 {
    let mut runs: Vec<_> = line.runs().collect();
    runs.sort_by_key(|run| std::cmp::Reverse(run.text_range().start));
    let mut advance = 0.0;
    for run in runs {
        let clusters: Vec<_> = run.clusters().collect();
        for cluster in clusters.iter().rev() {
            let is_space = matches!(text.get(cluster.text_range()), Some(" " | "\t"));
            if !is_space || !collapses_spaces(doc, cluster.style().brush.id) {
                return advance;
            }
            advance += cluster.advance();
        }
    }
    advance
}

/// Whether the white space of `node_id`'s text is collapsible.
fn collapses_spaces(doc: &BaseDocument, node_id: NodeId) -> bool {
    doc.get_node(node_id)
        .and_then(|node| node.primary_styles())
        .is_none_or(|style| {
            matches!(
                style.clone_white_space_collapse(),
                WhiteSpaceCollapse::Collapse | WhiteSpaceCollapse::PreserveBreaks
            )
        })
}

/// Places each fragment in the strip of its element's fragments, which are laid end to end in
/// line order: from left to right in LTR, and from right to left in RTL.
fn lay_out_strips(fragments: &mut [InlineFragment]) {
    // The width and fragment count of each element's strip
    let mut strips: HashMap<NodeId, (f64, usize)> = HashMap::new();
    for fragment in fragments.iter() {
        let strip = strips.entry(fragment.node_id).or_default();
        strip.0 += fragment.x1 - fragment.x0;
        strip.1 += 1;
    }
    // The width and number of the fragments placed so far
    let mut placed: HashMap<NodeId, (f64, usize)> = HashMap::new();
    for fragment in fragments.iter_mut() {
        let (strip_width, count) = strips[&fragment.node_id];
        let (before, index) = placed.entry(fragment.node_id).or_default();
        let width = fragment.x1 - fragment.x0;
        let (is_first, is_last) = (*index == 0, *index + 1 == count);
        fragment.strip_width = strip_width;
        if fragment.is_ltr {
            fragment.strip_offset = *before;
            fragment.is_strip_left = is_first;
            fragment.is_strip_right = is_last;
        } else {
            fragment.strip_offset = strip_width - *before - width;
            fragment.is_strip_left = is_last;
            fragment.is_strip_right = is_first;
        }
        *before += width;
        *index += 1;
    }
}

/// Whether an element with `style` paints a background, border or box shadow.
fn has_box_decorations(style: &ComputedValues) -> bool {
    if style.get_inherited_box().visibility != Visibility::Visible {
        return false;
    }
    let background = style.get_background();
    let background_color = background
        .background_color
        .resolve_to_absolute(&style.clone_color())
        .as_srgb_color();
    let border = style.get_border();
    background_color != Color::TRANSPARENT
        || background
            .background_image
            .0
            .iter()
            .any(|image| !matches!(image, GenericImage::None))
        || [
            (border.border_top_style, &border.border_top_width),
            (border.border_right_style, &border.border_right_width),
            (border.border_bottom_style, &border.border_bottom_width),
            (border.border_left_style, &border.border_left_width),
        ]
        .iter()
        .any(|(style, width)| !style.none_or_hidden() && width.0.0 > 0)
        || !style.get_effects().box_shadow.0.is_empty()
}

impl BlitzDomPainter<'_, '_> {
    /// Paints the inline content of `inline_root`, whose text layout is `text_layout`: its inline
    /// elements' box decorations and its text, with the selection highlight `selection_rects`
    /// and the text highlights `highlights`.
    ///
    /// Each line paints the box decorations of its inline elements, then the text highlights'
    /// backgrounds, the selection highlight and the text. A line's decorations thus cover the
    /// text of the lines before it where they overlap, as in browsers. Opacity groups are painted
    /// after the rest.
    pub(crate) fn draw_inline_content(
        &self,
        scene: &mut impl PaintScene,
        inline_root: &Node,
        text_layout: &TextLayout,
        transform: Affine,
        selection_rects: &[(Rect, usize)],
        highlights: &[HighlightRect],
    ) {
        let layout = inline_root.final_layout();
        let containing_block_width = layout.size.width
            - (layout.padding.left + layout.padding.right)
            - (layout.border.left + layout.border.right);
        let boxes = collect_inline_fragments(
            self.dom,
            text_layout,
            inline_root,
            containing_block_width,
            self.scale,
        );
        let lines = InlineLines {
            inline_root,
            text_layout,
            transform,
            containing_block_width,
            boxes: &boxes,
        };
        self.draw_inline_lines(scene, &lines, None, selection_rects, highlights);
        for group in boxes.groups.iter().filter(|group| group.parent.is_none()) {
            self.draw_opacity_group(scene, &lines, group, highlights);
        }
    }

    /// Paints an opacity group, and the groups nested in it.
    fn draw_opacity_group(
        &self,
        scene: &mut impl PaintScene,
        lines: &InlineLines<'_>,
        group: &OpacityGroup,
        highlights: &[HighlightRect],
    ) {
        self.layer_manager.maybe_with_layer(
            scene,
            true,
            BlendMode::default(),
            group.opacity,
            Affine::IDENTITY,
            &self.viewport_rect(),
            None,
            None,
            |scene| {
                self.draw_inline_lines(scene, lines, Some(group.node_id), &[], highlights);
                let nested = lines.boxes.groups.iter();
                for nested in nested.filter(|nested| nested.parent == Some(group.node_id)) {
                    self.draw_opacity_group(scene, lines, nested, highlights);
                }
            },
        );
    }

    /// Paints the lines' fragments and text in the given opacity group (or in none). The text
    /// highlights' backgrounds are painted with the rest, outside any group; their text colours
    /// in every group.
    fn draw_inline_lines(
        &self,
        scene: &mut impl PaintScene,
        lines: &InlineLines<'_>,
        group: Option<NodeId>,
        selection_rects: &[(Rect, usize)],
        highlights: &[HighlightRect],
    ) {
        let mut fragments = (lines.boxes.fragments.iter())
            .filter(|fragment| fragment.group == group)
            .peekable();
        for (line_index, line) in lines.text_layout.layout.lines().enumerate() {
            while let Some(fragment) = fragments.next_if(|f| f.line_index == line_index) {
                self.draw_inline_fragment(
                    scene,
                    fragment,
                    lines.transform,
                    lines.containing_block_width,
                );
            }
            let on_line = || highlights.iter().filter(|h| h.line == line_index);
            if group.is_none() {
                for highlight in on_line() {
                    scene.fill(
                        Fill::NonZero,
                        lines.transform,
                        highlight.background,
                        None,
                        &highlight.rect,
                        AntiAliasing::Enabled,
                    );
                }
            }
            for (rect, _) in selection_rects.iter().filter(|(_, i)| *i == line_index) {
                scene.fill(
                    Fill::NonZero,
                    lines.transform,
                    SELECTION_COLOR,
                    None,
                    rect,
                    AntiAliasing::Enabled,
                );
            }
            let stroke = |scene: &mut _, brush_override| {
                crate::text::stroke_text(
                    scene,
                    std::iter::once(line),
                    self.dom,
                    lines.transform,
                    TextPaintOptions {
                        scale: self.scale,
                        brush_override,
                        runs: RunFilter::Group(group),
                    },
                    lines.inline_root.id,
                    &mut self.draw_text_context.borrow_mut(),
                );
            };
            let recolored: Vec<&HighlightRect> = on_line().filter(|h| h.color.is_some()).collect();
            if recolored.is_empty() {
                stroke(scene, None);
                continue;
            }
            // Like Blink, the text outside the highlights is painted clipped to outside them,
            // and the text in each in its colour, clipped to it.
            let metrics = line.metrics();
            let reach = f64::from(metrics.line_height.max(1.0));
            let mut outside = Rect::new(
                f64::from(metrics.inline_min_coord) - reach,
                f64::from(metrics.block_min_coord) - reach,
                f64::from(metrics.inline_max_coord) + reach,
                f64::from(metrics.block_max_coord) + reach,
            )
            .to_path(0.1);
            for highlight in &recolored {
                outside.extend(highlight.rect.path_elements(0.1));
            }
            scene.push_clip_layer(Fill::EvenOdd, lines.transform, &outside);
            stroke(scene, None);
            scene.pop_layer();
            for highlight in recolored {
                scene.push_clip_layer(Fill::NonZero, lines.transform, &highlight.rect);
                stroke(scene, highlight.color);
                scene.pop_layer();
            }
        }
    }

    /// Paints the box decorations of an inline fragment.
    ///
    /// `transform` maps the text layout's coordinates to the scene, and `containing_block_width`
    /// (in CSS pixels) is the inline size of the inline root's content box.
    fn draw_inline_fragment(
        &self,
        scene: &mut impl PaintScene,
        fragment: &InlineFragment,
        transform: Affine,
        containing_block_width: f32,
    ) {
        let Some(content_area) = fragment.content_area else {
            return;
        };
        let node = &self.dom.as_ref().tree()[fragment.node_id];
        let edges = node.inline_box_edges(containing_block_width);
        let scale = self.scale;

        // The border box is the content area plus the vertical padding and border, whatever the
        // line height (CSS 2.1 §10.6.1). Like Blink, the content area uses the font's ascent and
        // descent rounded to whole CSS pixels, and edges are snapped to pixels.
        let snap = PixelSnap::new(transform, scale);
        let round_to_css_pixel = |length: f64| (length / scale).round() * scale;
        let top = content_area.baseline
            - round_to_css_pixel(content_area.ascent)
            - (edges.padding.top + edges.border.top) as f64 * scale;
        let bottom = content_area.baseline
            + round_to_css_pixel(content_area.descent)
            + (edges.padding.bottom + edges.border.bottom) as f64 * scale;
        let (top, bottom) = (snap.y(top), snap.y(bottom));
        let strip_x = snap.x(fragment.x0 - fragment.strip_offset);
        let strip_width =
            snap.x(fragment.x0 - fragment.strip_offset + fragment.strip_width) - strip_x;

        let layout = taffy::Layout {
            size: taffy::Size {
                width: (strip_width / scale) as f32,
                height: ((bottom - top) / scale) as f32,
            },
            border: edges.border,
            padding: edges.padding,
            ..Default::default()
        };
        let cx = self.element_cx(
            node,
            layout,
            transform * Affine::translate((strip_x, top)),
            None,
        );

        // Clip the strip to the fragment, except where the strip ends so that shadows reach out
        // past the element's edges there.
        const UNCLIPPED: f64 = 1e7;
        let clip = Rect::new(
            if fragment.is_strip_left {
                -UNCLIPPED
            } else {
                snap.x(fragment.x0) - strip_x
            },
            -UNCLIPPED,
            if fragment.is_strip_right {
                UNCLIPPED
            } else {
                snap.x(fragment.x1) - strip_x
            },
            UNCLIPPED,
        );
        self.layer_manager.maybe_with_layer(
            scene,
            !(fragment.is_strip_left && fragment.is_strip_right),
            BlendMode::default(),
            1.0,
            cx.transform,
            &clip,
            None,
            None,
            |scene| {
                cx.draw_outset_box_shadow(scene);
                cx.draw_background(scene);
                cx.draw_inset_box_shadow(scene);
                cx.draw_border(scene);
            },
        );
    }
}
