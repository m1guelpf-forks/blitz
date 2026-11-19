//! Text selection state and logic for non-input elements.
//!
//! This module handles text selection across inline roots in the document,
//! including support for anonymous blocks which have unstable IDs across
//! layout reconstruction.

use std::ops::Range;

use blitz_traits::node_id::NodeId;
use markup5ever::local_name;
use style::values::specified::box_::{DisplayInside, DisplayOutside};

use crate::util::Color;
use crate::{BaseDocument, CopyStyleGetters as _, Node};

/// Text painted highlighted, as a browser highlights find-in-page matches: `background` behind
/// the text's content area, and the text itself in `color` (its own colour when `None`).
#[derive(Clone, Debug, PartialEq)]
pub struct TextHighlight {
    /// The inline root whose text this is.
    pub node: NodeId,
    /// Byte offsets into the inline root's text.
    pub range: Range<usize>,
    pub background: Color,
    pub color: Option<Color>,
}

/// Represents one endpoint (anchor or focus) of a text selection.
///
/// For regular nodes, `node_or_parent` contains the node ID directly.
/// For anonymous blocks, `node_or_parent` contains the parent ID and
/// `sibling_index` contains the index among anonymous siblings.
#[derive(Clone, Debug, Default)]
pub struct SelectionEndpoint {
    /// For regular nodes: the node ID directly.
    /// For anonymous blocks: the parent ID (requires lookup via sibling_index).
    pub(crate) node_or_parent: Option<NodeId>,
    /// Byte offset within the inline root's text
    pub offset: usize,
    /// For anonymous blocks only: index among anonymous siblings.
    /// When Some, node_or_parent is a parent ID requiring lookup.
    pub(crate) sibling_index: Option<usize>,
}

impl SelectionEndpoint {
    /// Create a new endpoint at the given node and offset (for non-anonymous nodes)
    fn new(node: NodeId, offset: usize) -> Self {
        Self {
            node_or_parent: Some(node),
            offset,
            sibling_index: None,
        }
    }

    /// Check if this endpoint is set
    pub fn is_some(&self) -> bool {
        self.node_or_parent.is_some()
    }

    /// Clear this endpoint
    pub fn clear(&mut self) {
        self.node_or_parent = None;
        self.offset = 0;
        self.sibling_index = None;
    }

    /// Resolve the node ID, using a lookup function for anonymous blocks.
    pub fn resolve_node_id(
        &self,
        lookup_fn: impl FnOnce(NodeId, usize) -> Option<NodeId>,
    ) -> Option<NodeId> {
        match (self.node_or_parent, self.sibling_index) {
            (Some(node), None) => Some(node),
            (Some(parent), Some(idx)) => lookup_fn(parent, idx),
            _ => None,
        }
    }

    /// Set as a direct node reference (for regular nodes)
    pub fn set_node(&mut self, node: NodeId, offset: usize) {
        self.node_or_parent = Some(node);
        self.offset = offset;
        self.sibling_index = None;
    }

    /// Set as an anonymous block reference
    pub fn set_anonymous(&mut self, parent: NodeId, sibling_index: usize, offset: usize) {
        self.node_or_parent = Some(parent);
        self.offset = offset;
        self.sibling_index = Some(sibling_index);
    }
}

/// Text selection state for non-input elements.
///
/// Tracks both the anchor (where selection started) and focus (where it currently ends).
/// For anonymous blocks, we store stable parent references since anonymous block IDs
/// can change during layout reconstruction.
#[derive(Clone, Debug, Default)]
pub struct TextSelection {
    /// The anchor point (where selection started via mousedown)
    pub anchor: SelectionEndpoint,
    /// The focus point (where selection currently ends, updated during drag)
    pub focus: SelectionEndpoint,
}

impl TextSelection {
    /// Create a selection spanning from anchor to focus
    pub fn new(
        anchor_node: NodeId,
        anchor_offset: usize,
        focus_node: NodeId,
        focus_offset: usize,
    ) -> Self {
        Self {
            anchor: SelectionEndpoint::new(anchor_node, anchor_offset),
            focus: SelectionEndpoint::new(focus_node, focus_offset),
        }
    }

    /// Check if there is an active (non-empty) selection.
    /// Note: For anonymous blocks this compares parent+sibling_index, not resolved node IDs.
    pub fn is_active(&self) -> bool {
        self.anchor.is_some()
            && self.focus.is_some()
            && (self.anchor.node_or_parent != self.focus.node_or_parent
                || self.anchor.sibling_index != self.focus.sibling_index
                || self.anchor.offset != self.focus.offset)
    }

    /// Clear the selection
    pub fn clear(&mut self) {
        self.anchor.clear();
        self.focus.clear();
    }

    /// Update the focus endpoint (for non-anonymous nodes)
    pub fn set_focus(&mut self, node: NodeId, offset: usize) {
        self.focus.set_node(node, offset);
    }
}

/// Selected text as browsers serialize it, built an inline root at a time. What goes between two
/// inline roots follows Chromium's `TextIterator`, walking out of the first one's elements and
/// into the second's.
#[derive(Default)]
pub(crate) struct PlainText {
    text: String,
}

impl PlainText {
    pub(crate) fn push(&mut self, text: impl Iterator<Item = char>) {
        self.text.extend(text);
    }

    pub(crate) fn into_string(self) -> Option<String> {
        (!self.text.is_empty()).then_some(self.text)
    }

    /// What goes between inline root `from`'s text and the next one's, `to`: a newline where the
    /// text leaves or enters a block, unless it already ends in one (and as it leaves a block
    /// only once there's text); a second after a paragraph; and a tab before a table cell that
    /// doesn't start a line.
    pub(crate) fn separate(&mut self, doc: &BaseDocument, from: NodeId, to: NodeId) {
        let from_chain = doc.ancestor_chain_from_root(from);
        let to_chain = doc.ancestor_chain_from_root(to);
        let common = from_chain
            .iter()
            .zip(&to_chain)
            .take_while(|(a, b)| a == b)
            .count();
        // Anonymous blocks are layout's, not the document's.
        let elements = |chain: &[NodeId]| -> Vec<&Node> {
            chain
                .iter()
                .map(|&id| &doc.nodes[id])
                .filter(|node| node.is_element())
                .collect()
        };
        let mut extra_newline = false;
        for node in elements(&from_chain[common..]).into_iter().rev() {
            if self.text.is_empty() || !separates_lines(node) {
                continue;
            }
            let extra = is_paragraph(node);
            if self.text.ends_with('\n') {
                if extra {
                    self.text.push('\n');
                }
            } else {
                self.text.push('\n');
                extra_newline |= extra;
            }
        }
        if extra_newline {
            self.text.push('\n');
        }
        for node in elements(&to_chain[common..]) {
            if self.text.ends_with('\n') {
                break;
            }
            if is_table_cell(node) {
                // A cell at the selection's start is on its line.
                if !self.text.is_empty() {
                    self.text.push('\t');
                }
            } else if separates_lines(node) {
                // Even before any text: the selection starts on another line.
                self.text.push('\n');
            }
        }
    }
}

/// Whether text leaving or entering `node` starts a new line (`ShouldEmitNewlinesBeforeAndAfterNode`):
/// block-level boxes in flow and table rows, but not table cells, which tabs separate.
fn separates_lines(node: &Node) -> bool {
    let Some(style) = node.primary_styles() else {
        return false;
    };
    if style.clone_position().is_absolutely_positioned() || style.clone_float().is_floating() {
        return false;
    }
    let Some(display) = node.display_style() else {
        return false;
    };
    match display.inside() {
        DisplayInside::TableCell => false,
        DisplayInside::TableRow
        | DisplayInside::TableRowGroup
        | DisplayInside::TableHeaderGroup
        | DisplayInside::TableFooterGroup => true,
        _ => display.outside() == DisplayOutside::Block,
    }
}

fn is_table_cell(node: &Node) -> bool {
    node.display_style()
        .is_some_and(|display| display.inside() == DisplayInside::TableCell)
}

/// Whether `node` is a paragraph with height, which a blank line follows (Chromium's
/// `ShouldEmitExtraNewlineForNode`, after the `innerText` algorithm's two line breaks for `p`;
/// headings get one, whatever their margins).
fn is_paragraph(node: &Node) -> bool {
    node.data.is_element_with_tag_name(&local_name!("p")) && node.final_layout().size.height > 0.0
}
