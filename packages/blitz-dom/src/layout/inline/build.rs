use app_units::Au;
use blitz_traits::node_id::NodeId;
use markup5ever::local_name;
use parley::{
    CHROMIUM_LINE_BREAK_OVERRIDE, FontContext, InlineBox, InlineBoxKind, LayoutContext,
    LineBreakContext, LineBreakOverrideFn, TextWrapMode, TreeBuilder, VerticalAlign,
    WhiteSpaceCollapse,
};
use style::{
    Zero as _,
    computed_values::position::T as PositionProperty,
    computed_values::white_space_collapse::T as StyloWhiteSpaceCollapse,
    computed_values::{line_break::T as LineBreak, word_break::T as WordBreak},
    properties::ComputedValues,
    values::{
        computed::{Display, Float},
        generics::length::GenericMargin,
        specified::box_::{DisplayInside, DisplayOutside},
    },
};
use stylo_taffy::CopyStyleGetters;

use crate::{
    NodeData,
    layout::list::BULLET_FONT_FAMILY,
    layout::replaced::is_inline_box_element,
    layout::text_transform::{CaseTransform, TextTransformer, is_collapsible},
    node::{
        EDGE_JOINER, InlineBoxRef, InlineEdge, ListItemLayout, ListItemLayoutPosition, Marker,
        TextBrush, TextLayout, WORD_BREAK_CHAR_STR,
    },
    stylo_to_parley,
};

pub(crate) fn build_inline_layout_into(
    nodes: &crate::NodeTree,
    layout_ctx: &mut LayoutContext<TextBrush>,
    font_ctx: &mut FontContext,
    text_layout: &mut TextLayout,
    scale: f32,
    inline_context_root_node_id: NodeId,
) {
    // Get the inline context's root node's text styles
    let root_node = &nodes[inline_context_root_node_id];
    let root_node_style = root_node.primary_styles().or_else(|| {
        root_node
            .parent
            .and_then(|parent_id| nodes[parent_id].primary_styles())
    });

    let parley_style = root_node_style
        .as_ref()
        .map(|s| stylo_to_parley::style(inline_context_root_node_id, s))
        .unwrap_or_else(|| parley::TextStyle {
            white_space_collapse: WhiteSpaceCollapse::Collapse,
            ..Default::default()
        });

    // Create a parley tree builder
    let mut builder = layout_ctx.tree_builder(font_ctx, scale, true, &parley_style);
    if let Some(style) = root_node_style.as_deref() {
        builder.set_base_direction(stylo_to_parley::base_direction(
            style.clone_direction(),
            style.clone_unicode_bidi(),
        ));
    }

    let text_transform = root_node_style
        .as_deref()
        .map(|s| CaseTransform::from_style(s))
        .unwrap_or(CaseTransform::NONE);

    // Render position-inside list items
    if let Some(ListItemLayout {
        marker,
        position: ListItemLayoutPosition::Inside,
        ..
    }) = root_node
        .element_data()
        .and_then(|el| el.list_item_data.as_deref())
    {
        match marker {
            // Bullet glyphs live in the bundled bullet font. The position-outside
            // path already asks for it; without the same span here a marker like
            // disclosure-closed (U+25B8) falls back to the element's own font and
            // renders as a missing glyph.
            Marker::Char(char) => {
                let mut marker_style = parley_style.clone();
                marker_style.font_family = BULLET_FONT_FAMILY.into();
                builder.push_style_span(marker_style);
                builder.push_text(&format!("{char} "));
                builder.pop_style_span();
            }
            Marker::String(str) => builder.push_text(str),
        }
    };
    // The marker is a separate box, so words in the content don't continue from it.
    let mut text_transformer = TextTransformer::default();
    text_transformer.word_break(&builder);
    let mut edges = EdgeBoxes::default();
    let mut breaks = Breaks::default();

    if let Some(before_id) = root_node.before() {
        build_inline_layout_recursive(
            &mut builder,
            &mut text_transformer,
            &mut edges,
            &mut breaks,
            nodes,
            before_id,
            &text_transform,
        );
    }
    for child_id in root_node.children.iter().copied() {
        build_inline_layout_recursive(
            &mut builder,
            &mut text_transformer,
            &mut edges,
            &mut breaks,
            nodes,
            child_id,
            &text_transform,
        );
    }
    if let Some(after_id) = root_node.after() {
        build_inline_layout_recursive(
            &mut builder,
            &mut text_transformer,
            &mut edges,
            &mut breaks,
            nodes,
            after_id,
            &text_transform,
        );
    }

    builder.set_line_break_override(match (edges.any, breaks.anywhere) {
        (false, false) => Some(CHROMIUM_LINE_BREAK_OVERRIDE),
        (true, false) => Some(CHROMIUM_BREAKS_AND_EDGE_JOINER),
        (true, true) => Some(NO_BREAK_AT_EDGE_JOINER),
        (false, true) => None,
    });
    text_layout.text = builder.build_into(&mut text_layout.layout);
    text_layout.word_breaks = breaks.wbr;
    return;

    fn build_inline_layout_recursive(
        builder: &mut TreeBuilder<TextBrush>,
        text_transformer: &mut TextTransformer,
        edges: &mut EdgeBoxes,
        breaks: &mut Breaks,
        nodes: &crate::NodeTree,
        node_id: NodeId,
        parent_text_transform: &CaseTransform,
    ) {
        let node = &nodes[node_id];

        let style = node.primary_styles();
        let style = style.as_ref();

        let text_transform = style
            .map(|s| CaseTransform::from_style(s))
            .unwrap_or(CaseTransform::NONE);

        match &node.data {
            NodeData::Element(element_data) | NodeData::AnonymousBlock(element_data) => {
                // if the input type is hidden, hide it
                if *element_data.name.local == *"input" {
                    if let Some("hidden") = element_data.attr(local_name!("type")) {
                        return;
                    }
                }

                let display = node.display_style().unwrap_or(Display::inline());
                let position = style
                    .map(|s| s.clone_position())
                    .unwrap_or(PositionProperty::Static);
                let float = style.map(|s| s.clone_float()).unwrap_or(Float::None);
                let box_kind = if position.is_absolutely_positioned() {
                    InlineBoxKind::OutOfFlow
                } else if float.is_floating() {
                    InlineBoxKind::CustomOutOfFlow
                } else {
                    InlineBoxKind::InFlow
                };

                match (display.outside(), display.inside()) {
                    (DisplayOutside::None, DisplayInside::None) => {
                        // node.remove_damage(CONSTRUCT_DESCENDENT | CONSTRUCT_FC | CONSTRUCT_BOX);
                    }
                    (DisplayOutside::None, DisplayInside::Contents) => {
                        let whitespace = style.map(|s| {
                            [
                                parley::StyleProperty::WhiteSpaceCollapse(
                                    stylo_to_parley::white_space_collapse(
                                        s.clone_white_space_collapse(),
                                    ),
                                ),
                                parley::StyleProperty::TextWrapMode(
                                    stylo_to_parley::text_wrap_mode(s.clone_text_wrap_mode()),
                                ),
                                // No box, so no `vertical-align` of its own, nor its parent's.
                                parley::StyleProperty::VerticalAlign(VerticalAlign::BASELINE),
                            ]
                        });
                        edges.enter_span(builder);
                        builder.push_style_modification_span(
                            whitespace
                                .as_ref()
                                .map_or(&[][..], |styles| styles.as_slice()),
                        );
                        for child_id in node.children.iter().copied() {
                            // node.remove_damage(CONSTRUCT_DESCENDENT | CONSTRUCT_FC | CONSTRUCT_BOX);
                            build_inline_layout_recursive(
                                builder,
                                text_transformer,
                                edges,
                                breaks,
                                nodes,
                                child_id,
                                &text_transform,
                            );
                        }
                        builder.pop_style_span();
                    }
                    (DisplayOutside::Inline, DisplayInside::Flow) => {
                        let tag_name = &element_data.name.local;

                        if is_inline_box_element(tag_name) {
                            if box_kind == InlineBoxKind::InFlow {
                                edges.before_content(builder);
                                text_transformer.word_break(builder);
                            }
                            builder.push_inline_box(InlineBox {
                                id: node_id.as_u64(),
                                kind: box_kind,
                                // Overridden by push_inline_box method
                                index: 0,
                                // Width and height are set during layout
                                width: 0.0,
                                height: 0.0,
                                baseline: None,
                                vertical_align: node
                                    .primary_styles()
                                    .map(|s| stylo_to_parley::vertical_align(&s))
                                    .unwrap_or_default(),
                            });
                        } else if *tag_name == local_name!("br") {
                            // node.remove_damage(CONSTRUCT_DESCENDENT | CONSTRUCT_FC | CONSTRUCT_BOX);
                            // TODO: update span id for br spans
                            edges.before_content(builder);
                            // The line break belongs to the box it's in: it doesn't take that
                            // box's `vertical-align` again (a table cell's `middle` lifted every
                            // line ending in a `<br>` by a pixel).
                            builder.push_style_modification_span(&[
                                parley::StyleProperty::WhiteSpaceCollapse(
                                    WhiteSpaceCollapse::Preserve,
                                ),
                                parley::StyleProperty::VerticalAlign(VerticalAlign::BASELINE),
                            ]);
                            builder.push_text("\n");
                            builder.pop_style_span();
                            text_transformer.word_break(builder);
                        } else if *tag_name == local_name!("wbr") {
                            // A line break opportunity, which browsers lay out as a U+200B
                            // ZERO WIDTH SPACE. Chromium breaks there even where the text
                            // doesn't wrap (`white-space: nowrap`): Parley takes the opportunity
                            // after a character from that character's style.
                            edges.before_content(builder);
                            builder.push_style_modification_span(&[
                                parley::StyleProperty::TextWrapMode(TextWrapMode::Wrap),
                            ]);
                            builder.push_text(WORD_BREAK_CHAR_STR);
                            builder.pop_style_span();
                            breaks
                                .wbr
                                .push(builder.text().len() - WORD_BREAK_CHAR_STR.len());
                        } else {
                            // node.remove_damage(CONSTRUCT_DESCENDENT | CONSTRUCT_FC | CONSTRUCT_BOX);
                            let style = node
                                .primary_styles()
                                .map(|s| stylo_to_parley::style(node.id, &s))
                                .unwrap_or_else(|| parley::TextStyle {
                                    white_space_collapse: WhiteSpaceCollapse::Collapse,
                                    ..Default::default()
                                });

                            let has_edges = node
                                .primary_styles()
                                .is_some_and(|style| has_inline_edge_spacing(&style));
                            edges.enter_span(builder);
                            builder.push_style_span(style);
                            if has_edges {
                                edges.push_edge(builder, node_id, InlineEdge::Start);
                            }

                            if let Some(before_id) = node.before() {
                                build_inline_layout_recursive(
                                    builder,
                                    text_transformer,
                                    edges,
                                    breaks,
                                    nodes,
                                    before_id,
                                    &text_transform,
                                );
                            }

                            for child_id in node.children.iter().copied() {
                                build_inline_layout_recursive(
                                    builder,
                                    text_transformer,
                                    edges,
                                    breaks,
                                    nodes,
                                    child_id,
                                    &text_transform,
                                );
                            }
                            if let Some(after_id) = node.after() {
                                build_inline_layout_recursive(
                                    builder,
                                    text_transformer,
                                    edges,
                                    breaks,
                                    nodes,
                                    after_id,
                                    &text_transform,
                                );
                            }

                            if has_edges {
                                edges.push_edge(builder, node_id, InlineEdge::End);
                            }
                            builder.pop_style_span();
                        }
                    }
                    // Inline box
                    (_, _) => {
                        if box_kind == InlineBoxKind::InFlow {
                            edges.before_content(builder);
                            text_transformer.word_break(builder);
                        }
                        builder.push_inline_box(InlineBox {
                            id: node_id.as_u64(),
                            kind: box_kind,
                            // Overridden by push_inline_box method
                            index: 0,
                            // Width and height are set during layout
                            width: 0.0,
                            height: 0.0,
                            baseline: None,
                            vertical_align: node
                                .primary_styles()
                                .map(|s| stylo_to_parley::vertical_align(&s))
                                .unwrap_or_default(),
                        });
                    }
                };
            }
            NodeData::Text(data) => {
                // node.remove_damage(CONSTRUCT_DESCENDENT | CONSTRUCT_FC | CONSTRUCT_BOX);
                // dbg!(&data.content);

                let parent_style = node
                    .parent
                    .and_then(|parent_id| nodes[parent_id].primary_styles());
                if let Some(style) = &parent_style {
                    let text_style = style.get_inherited_text();
                    breaks.anywhere |= text_style.word_break == WordBreak::BreakAll
                        || text_style.line_break == LineBreak::Anywhere;
                }
                let text =
                    text_transformer.transform(&data.content, parent_text_transform, builder);
                edges.push_text(builder, text, || {
                    parent_style
                        .as_ref()
                        .map_or(StyloWhiteSpaceCollapse::Collapse, |s| {
                            s.clone_white_space_collapse()
                        })
                });
            }
            NodeData::Comment { .. } => {
                // node.remove_damage(CONSTRUCT_DESCENDENT | CONSTRUCT_FC | CONSTRUCT_BOX);
            }
            NodeData::Document(_) => unreachable!(),
        }
    }
}

/// Whether a non-atomic inline element with `style` has margin, border or padding on its
/// inline-axis edges, which [edge boxes](InlineBoxRef::Edge) then reserve space in the line for.
pub(crate) fn has_inline_edge_spacing(style: &ComputedValues) -> bool {
    let (margin, padding, border) = (style.get_margin(), style.get_padding(), style.get_border());
    let (margins, paddings, borders) = if style.writing_mode.is_vertical() {
        (
            [&margin.margin_top, &margin.margin_bottom],
            [&padding.padding_top, &padding.padding_bottom],
            [
                (&border.border_top_width, border.border_top_style),
                (&border.border_bottom_width, border.border_bottom_style),
            ],
        )
    } else {
        (
            [&margin.margin_left, &margin.margin_right],
            [&padding.padding_left, &padding.padding_right],
            [
                (&border.border_left_width, border.border_left_style),
                (&border.border_right_width, border.border_right_style),
            ],
        )
    };
    // `auto` margins are zero on non-atomic inlines
    margins.into_iter().any(|margin| match margin {
        GenericMargin::LengthPercentage(lp) => !lp.is_zero(),
        GenericMargin::Auto => false,
        _ => true,
    }) || paddings.into_iter().any(|padding| !padding.0.is_zero())
        || borders
            .into_iter()
            .any(|(width, style)| !style.none_or_hidden() && width.0 != Au(0))
}

/// Overrides the line break opportunities that Parley allows on either side of every in-flow
/// inline box, so that an [`EDGE_JOINER`] glues edge boxes to their element's content.
static NO_BREAK_AT_EDGE_JOINER: &LineBreakOverrideFn =
    &|cx: LineBreakContext| (cx.before == EDGE_JOINER || cx.after == EDGE_JOINER).then_some(false);

/// [`NO_BREAK_AT_EDGE_JOINER`], then Chromium's rules for the rest.
static CHROMIUM_BREAKS_AND_EDGE_JOINER: &LineBreakOverrideFn = &|cx: LineBreakContext| {
    NO_BREAK_AT_EDGE_JOINER(cx).or_else(|| CHROMIUM_LINE_BREAK_OVERRIDE(cx))
};

/// The line break opportunities a paragraph's content adds to the ones Parley finds.
///
/// Between two ASCII characters, browsers break only where their own tables say: Chromium (as
/// WebKit) never breaks after a `/` (a path, a URL, a date), where UAX #14 allows it. Parley
/// has Chromium's table, which applies unless some of the text breaks anywhere
/// (`word-break: break-all`, `line-break: anywhere`), whose breaks the table would suppress.
#[derive(Default)]
struct Breaks {
    /// Where each `<wbr>` is, in the text.
    wbr: Vec<usize>,
    /// Some of the text may break between any two characters.
    anywhere: bool,
}

/// Builds the [edge boxes](InlineBoxRef::Edge) of non-atomic inline elements, which reserve the
/// space of their inline-start and inline-end margin, border and padding.
///
/// An edge box is an in-flow inline box (whose width is set during layout) glued to the element's
/// content by an [`EDGE_JOINER`], so that it stays on the line of the content next to it. Edge boxes
/// are not content as far as white space processing is concerned (CSS Text 3 §4.1.1): white space
/// on either side of one collapses together, as does white space between one and the start or
/// end of the line. This is done here rather than by Parley's builder, which sees an edge box as
/// content:
///  - Collapsible white space that starts after an edge box placed after a collapsible space (or
///    at the start of the paragraph) is skipped.
///  - Trailing collapsible white space in an element with edge boxes is held back until more
///    content follows, so that it collapses into white space after the element's end edge (and is
///    removed with it at the end of the paragraph) instead of ending up inside it.
#[derive(Default)]
struct EdgeBoxes {
    /// Whether any edge box has been pushed.
    any: bool,
    /// Whether collapsible white space at the start of the next text must be skipped.
    skip_leading_space: bool,
    /// Whether a collapsible space is being held back from the builder.
    held_space: bool,
    /// The number of elements with edge boxes that are being built.
    open_elements: usize,
}

impl EdgeBoxes {
    /// Pushes the edge box for `edge` of `node_id`, whose span must be the current one.
    fn push_edge(
        &mut self,
        builder: &mut TreeBuilder<TextBrush>,
        node_id: NodeId,
        edge: InlineEdge,
    ) {
        let text = builder.text();
        self.skip_leading_space |= builder.has_pending_whitespace()
            || text.is_empty()
            || text.ends_with(['\n', '\r', '\u{2028}', '\u{2029}']);
        self.any = true;

        let edge_box = InlineBox {
            id: InlineBoxRef::Edge(node_id, edge).id(),
            kind: InlineBoxKind::InFlow,
            // Overridden by push_inline_box method
            index: 0,
            // The width is set during layout, and the box sits on the element's baseline
            width: 0.0,
            height: 0.0,
            baseline: Some(0.0),
            vertical_align: VerticalAlign::BASELINE,
        };
        let mut joiner = [0; 4];
        let joiner = EDGE_JOINER.encode_utf8(&mut joiner);
        match edge {
            InlineEdge::Start => {
                self.open_elements += 1;
                builder.push_inline_box(edge_box);
                builder.push_text(joiner);
            }
            InlineEdge::End => {
                self.open_elements -= 1;
                builder.push_text(joiner);
                builder.push_inline_box(edge_box);
            }
        }
    }

    /// Prepares for starting a span, whose content the held back space (if any) comes before.
    fn enter_span(&mut self, builder: &mut TreeBuilder<TextBrush>) {
        if std::mem::take(&mut self.held_space) {
            builder.push_text(" ");
        }
    }

    /// Prepares for pushing content other than text, such as an atomic inline or a line break.
    fn before_content(&mut self, builder: &mut TreeBuilder<TextBrush>) {
        self.enter_span(builder);
        self.skip_leading_space = false;
    }

    /// Pushes `text`, applying the white space collapsing around edge boxes.
    fn push_text(
        &mut self,
        builder: &mut TreeBuilder<TextBrush>,
        text: &str,
        white_space_collapse: impl FnOnce() -> StyloWhiteSpaceCollapse,
    ) {
        if !self.skip_leading_space && !self.held_space && self.open_elements == 0 {
            builder.push_text(text);
            return;
        }

        let white_space_collapse = white_space_collapse();
        let is_space = |c| is_collapsible(white_space_collapse, c);
        let Some(content_start) = text.find(|c| !is_space(c)) else {
            // Only white space, which collapses into the space before it if there is one
            if !text.is_empty() && !self.skip_leading_space {
                self.held_space = true;
            }
            return;
        };
        let content_end = text.trim_end_matches(is_space).len();

        let leading_space = &text[..content_start];
        if !std::mem::take(&mut self.skip_leading_space) {
            if !leading_space.is_empty() {
                builder.push_text(leading_space);
            } else if self.held_space {
                builder.push_text(" ");
            }
        }
        self.held_space = false;
        builder.push_text(&text[content_start..content_end]);

        let trailing_space = &text[content_end..];
        if !trailing_space.is_empty() {
            if self.open_elements > 0 {
                self.held_space = true;
            } else {
                builder.push_text(trailing_space);
            }
        }
    }
}
