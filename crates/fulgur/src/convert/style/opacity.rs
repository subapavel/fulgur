//! opacity + visibility extraction.
//!
//! Free function — called directly by block / list_item / inline_root /
//! pseudo, not from extract_block_style.

use crate::blitz_adapter::{BaseDocument, Node};

/// Extract CSS opacity and visibility from computed styles.
/// Returns `(opacity, visible)` with defaults `(1.0, true)`.
///
/// `pub(in crate::convert)` rather than `pub(super)`: callers in
/// `convert/{block,list_item,inline_root,pseudo}.rs` resolve this symbol via
/// `style/mod.rs`'s `pub(super) use` re-export. A `pub(super)` definition
/// (visible only to `style/mod.rs`) makes the re-export non-transparent and
/// trips E0603 at the `convert/mod.rs` import site. Restricting to the
/// `convert` subtree keeps unrelated modules (`paragraph`, `render`, ...) from
/// reaching the helper directly.
pub(in crate::convert) fn extract_opacity_visible(node: &Node) -> (f32, bool) {
    use style::properties::longhands::visibility::computed_value::T as Visibility;
    node.primary_styles()
        .map(|s| {
            let opacity = s.clone_opacity();
            let v = s.clone_visibility();
            let visible = v != Visibility::Hidden && v != Visibility::Collapse;
            (opacity, visible)
        })
        .unwrap_or((1.0, true))
}

/// Whether the box of `node_id` is visible: its computed `visibility` is neither `hidden` nor `collapse` (CSS 2.1 §11.2).
/// `visibility` is inherited, so the computed value of a span or a pseudo-element is already the effective one, and a
/// `visibility: visible` descendant of a hidden box is visible again. A node without styles (text, anonymous) counts as visible.
///
/// html-to-pdf F209: read per glyph run through the node its brush names, so hidden inline text keeps its advance and is
/// not drawn.
pub(in crate::convert) fn node_is_visible(doc: &BaseDocument, node_id: usize) -> bool {
    use style::properties::longhands::visibility::computed_value::T as Visibility;
    doc.get_node(node_id)
        .and_then(|node| node.primary_styles())
        .map(|s| {
            !matches!(
                s.clone_visibility(),
                Visibility::Hidden | Visibility::Collapse
            )
        })
        .unwrap_or(true)
}
