//! F30g: absolutely positioned boxes whose text crosses a page.
//!
//! The in-flow fragmenter never places a `position: absolute` box (out-of-flow children are skipped), and the
//! body-direct pass ([`super::append_position_absolute_body_direct_fragments`]) gives every node of such a subtree one
//! fragment per page its box intersects, each with the full height. The renderer can only cut those windows at the
//! page edge, so what falls below the page the box starts on is not printed; a box nobody places is not drawn.
//!
//! [`fragment_abspos_flows`] runs after those passes. For every absolutely positioned box, outer boxes first, it finds
//! the start of the box (CSS 2.1 §10.1 / §10.6.4: against the padding box of its containing block as if that were not
//! fragmented, css-position-3), decides whether today's geometry is complete ([`Disposition::Keep`]), whether the box
//! only needs to be put down ([`Disposition::Place`]), or whether it has to be fragmented from its start with the
//! fragmenters of the normal flow ([`Disposition::Flow`]: between line boxes, orphans and widows 2, css-break-3 §4.1,
//! §4.4). `position: fixed` boxes are repeated whole per page (CSS 2.1 §9.6.1) and are not touched.
//!
//! Coordinates are the fragmenter's own: `y` is body coordinates on page 0 and page coordinates after it ([`PageStrips`]),
//! `x` is html-relative.

use super::*;
use ::style::properties::longhands::position::computed_value::T as Pos;

/// What to do with one absolutely positioned box.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum Disposition {
    /// Today's geometry is complete (or the box is out of scope): leave it.
    Keep,
    /// The box has no geometry and fits below its start: one fragment there, plus its descendants.
    Place,
    /// Fragment the box from its start like content in the normal flow.
    Flow,
}

/// Where the top of an absolutely positioned box is, in the containing block's own coordinates.
#[derive(Clone, Copy, Debug, PartialEq)]
pub(super) enum Anchor {
    /// `top` (or the initial containing block): `offset` px below the top of the border box of `cb` as if it were
    /// stitched from its fragments, or below the top of the page area when `cb` is `None`.
    Top { cb: Option<usize>, offset: f32 },
    /// `bottom` with `top: auto`: the box's top is `from_end` px above the bottom of the border box of `cb`.
    Bottom { cb: usize, from_end: f32 },
    /// No inset on the vertical axis: after the previous in-flow sibling `after` (its last fragment plus the gap Taffy
    /// left, `offset`), or `offset` px below the first fragment of `parent` when there is none.
    Static {
        after: Option<usize>,
        parent: usize,
        offset: f32,
    },
}

/// Facts about one box that the caller collects from the tree.
#[derive(Clone, Copy, Debug)]
pub(super) struct Facts {
    /// The box sits under a clipping, transformed or multicol ancestor up to its containing block, is clipping,
    /// transformed or multicol itself, or is inside a body-direct root that stays on the window model.
    pub guarded: bool,
    /// A child of `<body>`: its geometry comes from the window pass.
    pub body_direct: bool,
    /// The geometry in the table predates a box around this one being fragmented again: treat it as absent.
    pub stale: bool,
    /// The y of the box in the initial containing block, as the window pass resolves it (body-direct boxes only).
    pub root_y_icb: f32,
}

/// The `position` of `node`.
fn position_of(node: &blitz_dom::Node) -> Option<Pos> {
    node.primary_styles().map(|s| s.get_box().clone_position())
}

/// The children Taffy laid out: `layout_children` (it includes the anonymous block wrappers) or else the DOM children.
fn laid_out_children(node: &blitz_dom::Node) -> Vec<usize> {
    let borrowed = node.layout_children.borrow();
    borrowed
        .as_deref()
        .filter(|v| !v.is_empty())
        .map(|v| v.to_vec())
        .unwrap_or_else(|| node.children.clone())
}

fn has_transform(node: &blitz_dom::Node) -> bool {
    node.primary_styles()
        .is_some_and(|s| !s.get_box().transform.0.is_empty())
}

/// Every `position: absolute` box of the document in tree order (pre-order: `::before`, children, `::after`), outer
/// boxes before the boxes inside them, because the start of a nested box reads the fragments of its containing block.
///
/// Subtrees of `position: fixed` boxes, running elements and multi-column containers are skipped: they are repeated
/// whole, belong to the margin boxes, or are fragmented by the multicol pass.
fn abspos_boxes_in_tree_order(
    doc: &BaseDocument,
    running_store: Option<&crate::gcpm::running::RunningElementStore>,
) -> Vec<usize> {
    fn walk(
        doc: &BaseDocument,
        id: usize,
        running_store: Option<&crate::gcpm::running::RunningElementStore>,
        out: &mut Vec<usize>,
        seen: &mut BTreeSet<usize>,
        depth: usize,
    ) {
        if depth >= crate::MAX_DOM_DEPTH || !seen.insert(id) {
            return;
        }
        let Some(node) = doc.get_node(id) else {
            return;
        };
        if running_store.is_some_and(|s| s.instance_for_node(id).is_some()) {
            return;
        }
        if matches!(position_of(node), Some(Pos::Fixed))
            || crate::blitz_adapter::is_multicol_container(node)
        {
            return;
        }
        if matches!(position_of(node), Some(Pos::Absolute)) {
            out.push(id);
        }
        let mut kids: Vec<usize> = Vec::new();
        kids.extend(node.before);
        kids.extend(laid_out_children(node));
        kids.extend(node.after);
        for kid in kids {
            walk(doc, kid, running_store, out, seen, depth + 1);
        }
    }
    let mut out = Vec::new();
    walk(
        doc,
        doc.root_element().id,
        running_store,
        &mut out,
        &mut BTreeSet::new(),
        0,
    );
    out
}

/// The containing block of the absolutely positioned box `id` (CSS 2.1 §10.1): the nearest positioned ancestor below
/// `<body>`, or `None` for the initial containing block. The second value is whether a clipping, transformed or
/// multicol box lies between the box and its containing block, the containing block included.
fn containing_block(doc: &BaseDocument, id: usize, body_id: usize) -> (Option<usize>, bool) {
    let mut guarded = false;
    let mut cur = doc.get_node(id).and_then(|n| n.parent);
    let mut depth = 0;
    while let Some(pid) = cur {
        if pid == body_id || depth >= crate::MAX_DOM_DEPTH {
            break;
        }
        let Some(p) = doc.get_node(pid) else {
            break;
        };
        if p.element_data().is_none() {
            break;
        }
        if clips_overflow(p) || has_transform(p) || crate::blitz_adapter::is_multicol_container(p) {
            guarded = true;
        }
        if !matches!(position_of(p), None | Some(Pos::Static)) {
            return (Some(pid), guarded);
        }
        cur = p.parent;
        depth += 1;
    }
    (None, guarded)
}

/// Whether a clipping, transformed or multicol box lies anywhere above `id`, up to but not including `<body>` (a clip
/// on `<html>` / `<body>` propagates to the viewport and clips nothing on the page). The window geometry of such a box is
/// clipped, transformed or columned by the renderer, so a flow built from the start would only add blank pages.
fn has_guarded_ancestor(doc: &BaseDocument, id: usize, body_id: usize) -> bool {
    let mut cur = doc.get_node(id).and_then(|n| n.parent);
    let mut depth = 0;
    while let Some(pid) = cur {
        if pid == body_id || depth >= crate::MAX_DOM_DEPTH {
            return false;
        }
        let Some(p) = doc.get_node(pid) else {
            return false;
        };
        if p.element_data().is_some()
            && (clips_overflow(p)
                || has_transform(p)
                || crate::blitz_adapter::is_multicol_container(p))
        {
            return true;
        }
        cur = p.parent;
        depth += 1;
    }
    false
}

/// Whether some ancestor of `id` is in `set`.
fn has_ancestor_in(doc: &BaseDocument, id: usize, set: &BTreeSet<usize>) -> bool {
    if set.is_empty() {
        return false;
    }
    let mut cur = doc.get_node(id).and_then(|n| n.parent);
    let mut depth = 0;
    while let Some(pid) = cur {
        if set.contains(&pid) {
            return true;
        }
        depth += 1;
        if depth >= crate::MAX_DOM_DEPTH {
            return false;
        }
        cur = doc.get_node(pid).and_then(|n| n.parent);
    }
    false
}

/// The page and y where block-size `offset` falls in a box made of `frags`, laid end to end: fragment `k` covers
/// `[Σ h_<k, Σ h_≤k)` of the box. Past the last fragment, or before the first, the box continues with full page
/// strips.
fn locate_forward(frags: &[Fragment], offset: f32, strips: PageStrips) -> (u32, f32) {
    let Some(first) = frags.first() else {
        return (0, 0.0);
    };
    if offset < 0.0 {
        return (first.page_index, first.y.to_f32() + offset);
    }
    let mut before = 0.0_f32;
    for f in frags {
        let h = f.height.to_f32().max(0.0);
        if offset < before + h {
            return (f.page_index, f.y.to_f32() + (offset - before));
        }
        before += h;
    }
    let last = frags.last().unwrap_or(first);
    carry_over(
        last.page_index,
        last.y.to_f32() + last.height.to_f32().max(0.0) + (offset - before),
        strips,
    )
}

/// The page and y of the point `from_end` above the end of a box made of `frags`. The box is `min_total` tall at least:
/// fragments that stop short of it (the block fragmenter closes the last one at the last child) leave the rest below
/// the last fragment.
fn locate_backward(
    frags: &[Fragment],
    from_end: f32,
    min_total: f32,
    strips: PageStrips,
) -> (u32, f32) {
    let stacked: f32 = frags.iter().map(|f| f.height.to_f32().max(0.0)).sum();
    locate_forward(frags, stacked.max(min_total) - from_end, strips)
}

/// `(page, y)` with a `y` at or below the bottom of the page carried to the pages after it.
fn carry_over(page: u32, y: f32, strips: PageStrips) -> (u32, f32) {
    if !y.is_finite() {
        return (page, 0.0);
    }
    let mut page = page;
    let mut y = y;
    if y >= strips.bottom(page) {
        y -= strips.bottom(page);
        page = page.saturating_add(1);
        let fresh = strips.fresh();
        if fresh > 0.0 && y >= fresh {
            let k = (y / fresh).floor().min(crate::MAX_PAGES as f32);
            y -= k * fresh;
            page = page.saturating_add(k as u32);
        }
    }
    (page.min(crate::MAX_PAGES), y)
}

/// The page and fragmenter y of a point `y_icb` below the top of the page area of page 0 (the initial containing
/// block, stitched over the pages). A point within the tolerance of the window pass of a page edge lands on it, so
/// `top: N * 100vh` stays where it is today. Page 0 is shifted by the body's offset (the renderer adds it there).
fn locate_icb(y_icb: f32, strips: PageStrips) -> (u32, f32) {
    let page_h = strips.fresh();
    let first_offset = page_h - strips.bottom(0);
    if !y_icb.is_finite() || page_h <= 0.0 {
        return (0, 0.0);
    }
    let (page, local) = if y_icb < 0.0 {
        (0u32, y_icb)
    } else {
        let row = page_row(y_icb, page_h).clamp(0, i64::from(crate::MAX_PAGES)) as u32;
        let snapped = (y_icb / page_h - row as f32).abs() < snap_tolerance(row as f32);
        (
            row,
            if snapped {
                0.0
            } else {
                y_icb - row as f32 * page_h
            },
        )
    };
    (
        page,
        if page == 0 {
            local - first_offset
        } else {
            local
        },
    )
}

/// The tolerance the window pass snaps a start to a page edge with: the sub-pixel residual of `Nvh` scales with `N`.
fn snap_tolerance(page: f32) -> f32 {
    (1e-3 * page.abs().max(1.0)).min(0.01)
}

/// The page row (0-based) of a point `y` px below the top of the page area, snapped to a page edge like the window
/// pass does.
fn page_row(y: f32, page_h: f32) -> i64 {
    let ratio = y / page_h;
    let near = ratio.round();
    if (ratio - near).abs() < snap_tolerance(near) {
        near as i64
    } else {
        ratio.floor() as i64
    }
}

/// Whether, were the box at `root_y_icb` in the page area with the window pass's geometry, some inline root of it, or
/// of an absolutely positioned box nested in it, would have its first line top and its last line bottom in different
/// page rows: the window model cuts such a paragraph at the page edge and prints only the part on the first row.
///
/// The y of a nested box follows the window pass (`record_subtree_fragments_at_offset`): an explicit inset is resolved
/// against the padding box of the nearest positioned ancestor, an `auto` one keeps the static position.
fn window_cuts_lines(doc: &BaseDocument, root_id: usize, root_y_icb: f32, page_h: f32) -> bool {
    /// `cb_anchor_y` and `cb_size` are those of the nearest positioned ancestor of `id` (its padding box).
    fn walk(
        doc: &BaseDocument,
        id: usize,
        y: f32,
        cb_anchor_y: f32,
        cb_size: (f32, f32),
        page_h: f32,
        depth: usize,
    ) -> bool {
        if depth >= crate::MAX_DOM_DEPTH {
            return false;
        }
        let Some(node) = doc.get_node(id) else {
            return false;
        };
        if clips_overflow(node) {
            // Stops at a clipping box, and so keeps the window model for an abspos box that holds, say, an
            // `overflow-hidden` card around a long paragraph: that paragraph is lossy there (F95 e). The in-flow
            // fragmenter splits such a card since F30f, but flowing the abspos root would give its nested
            // out-of-flow boxes window geometry against a placement that no longer exists.
            return false;
        }
        let layout = node.final_layout;
        let lines = collect_inline_line_metrics(node);
        if let (Some(first), Some(last)) = (lines.first(), lines.last()) {
            let inset = layout.border.top + layout.padding.top;
            let top = y + inset + first.0;
            let bottom = y + inset + last.1;
            if page_row(top, page_h) != page_row(bottom - 0.5, page_h) {
                return true;
            }
        }
        let (child_anchor_y, child_cb) = if matches!(position_of(node), None | Some(Pos::Static)) {
            (cb_anchor_y, cb_size)
        } else {
            (
                y + layout.border.top,
                (
                    layout.size.width - layout.border.left - layout.border.right,
                    layout.size.height - layout.border.top - layout.border.bottom,
                ),
            )
        };
        laid_out_children(node).into_iter().any(|kid| {
            let Some(child) = doc.get_node(kid) else {
                return false;
            };
            match position_of(child) {
                Some(Pos::Fixed) => false,
                Some(Pos::Absolute) => {
                    let l = child.final_layout;
                    let (_, rel_y) = resolve_viewport_cb_location(
                        child,
                        l.size.width,
                        l.size.height,
                        child_cb.0,
                        child_cb.1,
                    )
                    .unwrap_or((l.location.x, l.location.y));
                    let (_, explicit_y) = explicit_inset_axes(child);
                    let base_y = if explicit_y { child_anchor_y } else { y };
                    walk(
                        doc,
                        kid,
                        base_y + rel_y,
                        child_anchor_y,
                        child_cb,
                        page_h,
                        depth + 1,
                    )
                }
                _ => {
                    !child
                        .text_data()
                        .is_some_and(|t| t.content.chars().all(char::is_whitespace))
                        && walk(
                            doc,
                            kid,
                            y + child.final_layout.location.y,
                            child_anchor_y,
                            child_cb,
                            page_h,
                            depth + 1,
                        )
                }
            }
        })
    }
    page_h > 0.0 && walk(doc, root_id, root_y_icb, root_y_icb, (0.0, 0.0), page_h, 0)
}

/// Whether some inline root in the in-flow subtree of `root_id` that has geometry, and is not split between lines,
/// ends below the bottom of the page its fragment is on: text the renderer cannot print.
///
/// Like [`window_cuts_lines`] it stops at clipping boxes (F95 e): text inside a clipping card of an abspos root that
/// stays on the window model is not looked at, although the in-flow fragmenter splits such a card since F30f.
fn lines_end_below_page(
    doc: &BaseDocument,
    geometry: &PaginationGeometryTable,
    root_id: usize,
    strips: PageStrips,
) -> bool {
    fn walk(
        doc: &BaseDocument,
        geometry: &PaginationGeometryTable,
        id: usize,
        strips: PageStrips,
        depth: usize,
    ) -> bool {
        if depth >= crate::MAX_DOM_DEPTH {
            return false;
        }
        let Some(node) = doc.get_node(id) else {
            return false;
        };
        if clips_overflow(node) {
            return false;
        }
        let lines = collect_inline_line_metrics(node);
        if let (Some(first), Some(last), Some(g)) = (lines.first(), lines.last(), geometry.get(&id))
            && g.line_slices.is_empty()
        {
            let inset = node.final_layout.border.top + node.final_layout.padding.top;
            let span = last.1 - first.0;
            if g.fragments
                .iter()
                .any(|f| f.y.to_f32() + inset + span > strips.bottom(f.page_index) + 1.0)
            {
                return true;
            }
        }
        laid_out_children(node).into_iter().any(|kid| {
            let Some(child) = doc.get_node(kid) else {
                return false;
            };
            !is_out_of_flow_positioned(child) && walk(doc, geometry, kid, strips, depth + 1)
        })
    }
    walk(doc, geometry, root_id, strips, 0)
}

/// How far the in-flow content of `id` reaches below its top: its own height, or the bottom of its last line or of its
/// last in-flow child when that is more. Taffy lays an absolutely positioned box out against its parent, so a box
/// stretched by `top` and `bottom` inside an empty body gets height 0 while its content overflows it.
fn content_extent(doc: &BaseDocument, id: usize) -> f32 {
    fn walk(doc: &BaseDocument, id: usize, depth: usize) -> f32 {
        if depth >= crate::MAX_DOM_DEPTH {
            return 0.0;
        }
        let Some(node) = doc.get_node(id) else {
            return 0.0;
        };
        let layout = node.final_layout;
        let mut extent = if layout.size.height.is_finite() {
            layout.size.height.max(0.0)
        } else {
            0.0
        };
        let lines = collect_inline_line_metrics(node);
        if let (Some(first), Some(last)) = (lines.first(), lines.last()) {
            extent = extent.max(layout.border.top + layout.padding.top + (last.1 - first.0));
        }
        for kid in laid_out_children(node) {
            if let Some(child) = doc.get_node(kid)
                && !is_out_of_flow_positioned(child)
            {
                extent = extent.max(child.final_layout.location.y + walk(doc, kid, depth + 1));
            }
        }
        extent
    }
    walk(doc, id, 0)
}

/// What to do with the box `id` that starts at `start` (page, y): see [`Disposition`].
pub(super) fn abspos_disposition(
    doc: &BaseDocument,
    geometry: &PaginationGeometryTable,
    id: usize,
    start: (u32, f32),
    strips: PageStrips,
    facts: &Facts,
) -> Disposition {
    let Some(node) = doc.get_node(id) else {
        return Disposition::Keep;
    };
    let w = node.final_layout.size.width;
    if facts.guarded || w.is_nan() || w < 0.0 || start.0 >= crate::MAX_PAGES {
        return Disposition::Keep;
    }
    let has_geometry = !facts.stale && geometry.get(&id).is_some_and(|g| !g.fragments.is_empty());
    if facts.body_direct {
        let cuts = window_cuts_lines(doc, id, facts.root_y_icb, strips.fresh());
        if has_geometry {
            // The window pass drew the box: only a box whose text it cuts is fragmented again.
            return if cuts {
                Disposition::Flow
            } else {
                Disposition::Keep
            };
        }
        // The window pass drops a body-direct box that no page can show (`bottom: -100vh`): that stays dropped. It also
        // drops one whose height Taffy made 0 (stretched by `top` and `bottom` inside an empty body, or a wrapper of
        // nothing but boxes of its own), and those have content to show.
        if node.final_layout.size.height > 0.0 {
            return Disposition::Keep;
        }
        if cuts {
            return Disposition::Flow;
        }
    }
    let extent = content_extent(doc, id);
    if extent.is_nan() || extent <= 0.0 {
        return Disposition::Keep;
    }
    if !facts.body_direct && has_geometry {
        return if lines_end_below_page(doc, geometry, id, strips) {
            Disposition::Flow
        } else {
            Disposition::Keep
        };
    }
    if start.1 + extent <= strips.bottom(start.0) + 1.0 {
        Disposition::Place
    } else {
        Disposition::Flow
    }
}

/// The geometry of `id` and of its in-flow descendants, removed. Nested out-of-flow boxes are left: they are handled
/// in their own turn.
fn clear_subtree_geometry(geometry: &mut PaginationGeometryTable, doc: &BaseDocument, id: usize) {
    fn walk(geometry: &mut PaginationGeometryTable, doc: &BaseDocument, id: usize, depth: usize) {
        if depth >= crate::MAX_DOM_DEPTH {
            return;
        }
        geometry.remove(&id);
        let Some(node) = doc.get_node(id) else {
            return;
        };
        for kid in laid_out_children(node) {
            if doc
                .get_node(kid)
                .is_some_and(|c| !is_out_of_flow_positioned(c))
            {
                walk(geometry, doc, kid, depth + 1);
            }
        }
    }
    walk(geometry, doc, id, 0);
}

/// The end of the last fragment of `g`, with the frame of a paragraph split between lines (its top inset is on every
/// fragment, its bottom inset on the last).
fn bottom_of_last_fragment(g: &PaginationGeometry) -> Option<(u32, f32)> {
    let f = g.fragments.last()?;
    let (top, bottom) = g
        .line_split_insets
        .map_or((0.0, 0.0), |(t, b)| (t.to_f32(), b.to_f32()));
    Some((
        f.page_index,
        f.y.to_f32() + top + f.height.to_f32() + bottom,
    ))
}

/// Steps the search for a previous sibling may take in all, so that a parent with thousands of boxes without insets
/// cannot make the pass quadratic. Past it such a box keeps the geometry it has.
const SIBLING_SCAN_BUDGET: usize = 2_000_000;

/// The previous in-flow sibling of `id` that has geometry.
fn previous_flow_sibling(
    doc: &BaseDocument,
    geometry: &PaginationGeometryTable,
    parent_id: usize,
    id: usize,
    budget: &mut usize,
) -> Option<usize> {
    let parent = doc.get_node(parent_id)?;
    let borrowed = parent.layout_children.borrow();
    let kids: &[usize] = borrowed
        .as_deref()
        .filter(|v| !v.is_empty())
        .unwrap_or(&parent.children);
    let at = kids.iter().position(|&k| k == id)?;
    *budget = budget.checked_sub(at + 1)?;
    for &k in kids[..at].iter().rev() {
        *budget = budget.checked_sub(1)?;
        let in_flow = doc.get_node(k).is_some_and(|c| {
            !is_out_of_flow_positioned(c)
                && !crate::blitz_adapter::node_is_floating(c)
                && !c
                    .text_data()
                    .is_some_and(|t| t.content.chars().all(char::is_whitespace))
        });
        if in_flow && geometry.get(&k).is_some_and(|g| !g.fragments.is_empty()) {
            return Some(k);
        }
    }
    None
}

/// A containing block (or a parent) that the in-flow pass recorded no geometry for because an ancestor of it is a
/// zero-height element, which the pass records alone and does not enter: give the chain of zero-height elements between
/// the nearest ancestor that has geometry and `id` a zero-height fragment each, at the place their parent's fragment
/// puts them. Returns whether `id` has geometry afterwards.
fn ensure_zero_height_chain(
    geometry: &mut PaginationGeometryTable,
    doc: &BaseDocument,
    id: usize,
    depth: usize,
) -> bool {
    if geometry.get(&id).is_some_and(|g| !g.fragments.is_empty()) {
        return true;
    }
    if depth >= crate::MAX_DOM_DEPTH {
        return false;
    }
    let Some(node) = doc.get_node(id) else {
        return false;
    };
    let Some(parent_id) = node.parent else {
        return false;
    };
    let layout = node.final_layout;
    let block_level = node.primary_styles().is_some_and(|s| {
        s.clone_display().outside() == ::style::values::specified::box_::DisplayOutside::Block
    });
    if node.element_data().is_none()
        || !block_level
        || layout.size.height > 0.0
        || !ensure_zero_height_chain(geometry, doc, parent_id, depth + 1)
    {
        return false;
    }
    let Some(pf) = geometry
        .get(&parent_id)
        .and_then(|g| g.fragments.first())
        .cloned()
    else {
        return false;
    };
    geometry.entry(id).or_default().fragments.push(Fragment {
        page_index: pf.page_index,
        x: (pf.x.to_f32() + layout.location.x).as_px(),
        y: (pf.y.to_f32() + layout.location.y).as_px(),
        width: layout.size.width.as_px(),
        height: 0.0_f32.as_px(),
    });
    true
}

/// How the top of the box `id` is anchored (see [`Anchor`]), against the containing block `cb` (`None`: the initial
/// containing block of `viewport_w_px` x `page_h_px`).
fn anchor_of(
    doc: &BaseDocument,
    geometry: &PaginationGeometryTable,
    id: usize,
    cb: Option<usize>,
    viewport_w_px: f32,
    page_h_px: f32,
    scan_budget: &mut usize,
) -> Option<Anchor> {
    let node = doc.get_node(id)?;
    let layout = node.final_layout;
    let (_, explicit_y) = explicit_inset_axes(node);
    if !explicit_y {
        let parent_id = node.parent?;
        return Some(
            match previous_flow_sibling(doc, geometry, parent_id, id, scan_budget) {
                Some(sib_id) => {
                    let sib = doc.get_node(sib_id)?;
                    let sib_bottom = sib.final_layout.location.y + sib.final_layout.size.height;
                    Anchor::Static {
                        after: Some(sib_id),
                        parent: parent_id,
                        offset: (layout.location.y - sib_bottom).max(0.0),
                    }
                }
                None => Anchor::Static {
                    after: None,
                    parent: parent_id,
                    offset: layout.location.y,
                },
            },
        );
    }
    let (cb_pad_w, cb_pad_h, cb_border_top, cb_total_h) = match cb {
        Some(c) => {
            let cbn = doc.get_node(c)?;
            let l = cbn.final_layout;
            (
                (l.size.width - l.border.left - l.border.right).max(0.0),
                (l.size.height - l.border.top - l.border.bottom).max(0.0),
                l.border.top,
                l.size.height,
            )
        }
        None => (viewport_w_px, page_h_px, 0.0, page_h_px),
    };
    let (_, rel_y) = resolve_viewport_cb_location(
        node,
        layout.size.width,
        layout.size.height,
        cb_pad_w,
        cb_pad_h,
    )?;
    if uses_bottom_without_top(node) {
        Some(match cb {
            Some(c) => Anchor::Bottom {
                cb: c,
                from_end: cb_total_h - (cb_border_top + rel_y),
            },
            None => Anchor::Top {
                cb: None,
                offset: rel_y,
            },
        })
    } else {
        Some(Anchor::Top {
            cb,
            offset: cb_border_top + rel_y + layout.margin.top,
        })
    }
}

/// How far the bottom border box of `cb` reaches below its last fragment `last`: the block fragmenter closes the last
/// fragment of a box it split at the bottom of its last in-flow child, so the bottom padding and border are not in it.
/// Measured from that child's last fragment when it is on the page of `last`; 0 when the fragment already holds them.
fn closing_deficit(
    doc: &BaseDocument,
    geometry: &PaginationGeometryTable,
    cb: usize,
    last: &Fragment,
) -> f32 {
    let Some(node) = doc.get_node(cb) else {
        return 0.0;
    };
    let inset = node.final_layout.border.bottom + node.final_layout.padding.bottom;
    if inset <= 0.0 {
        return 0.0;
    }
    let child_bottom = laid_out_children(node).into_iter().rev().find_map(|k| {
        let c = doc.get_node(k)?;
        if is_out_of_flow_positioned(c) {
            return None;
        }
        bottom_of_last_fragment(geometry.get(&k)?)
    });
    match child_bottom {
        Some((page, y)) if page == last.page_index => {
            (y + inset - (last.y.to_f32() + last.height.to_f32())).max(0.0)
        }
        _ => 0.0,
    }
}

/// The page and y of the top of a box anchored by `anchor`, or `None` when what it hangs on has no geometry.
fn locate_anchor(
    doc: &BaseDocument,
    geometry: &PaginationGeometryTable,
    anchor: &Anchor,
    strips: PageStrips,
) -> Option<(u32, f32)> {
    let frags = |id: usize| -> Option<&[Fragment]> {
        geometry
            .get(&id)
            .map(|g| g.fragments.as_slice())
            .filter(|f| !f.is_empty())
    };
    Some(match *anchor {
        Anchor::Top { cb: None, offset } => locate_icb(offset, strips),
        Anchor::Top {
            cb: Some(c),
            offset,
        } => locate_forward(frags(c)?, offset, strips),
        Anchor::Bottom { cb, from_end } => {
            let f = frags(cb)?;
            let stacked: f32 = f.iter().map(|x| x.height.to_f32().max(0.0)).sum();
            let extra = closing_deficit(doc, geometry, cb, f.last()?);
            locate_backward(f, from_end, stacked + extra, strips)
        }
        Anchor::Static {
            after: Some(sib),
            offset,
            ..
        } => {
            let (page, y) = bottom_of_last_fragment(geometry.get(&sib)?)?;
            carry_over(page, y + offset, strips)
        }
        Anchor::Static {
            after: None,
            parent,
            offset,
        } => {
            let f = frags(parent)?.first()?;
            carry_over(f.page_index, f.y.to_f32() + offset, strips)
        }
    })
}

/// The html-relative x of the border box of `id`: an explicit inset is resolved against the padding box of the
/// containing block, otherwise the box keeps the static position (its parent's x plus Taffy's offset).
fn abspos_x(
    doc: &BaseDocument,
    geometry: &PaginationGeometryTable,
    id: usize,
    cb: Option<usize>,
    viewport_w_px: f32,
    page_h_px: f32,
) -> Option<f32> {
    let node = doc.get_node(id)?;
    let layout = node.final_layout;
    let (explicit_x, _) = explicit_inset_axes(node);
    if !explicit_x {
        let parent_id = node.parent?;
        let pf = geometry.get(&parent_id)?.fragments.first()?;
        return Some(pf.x.to_f32() + layout.location.x);
    }
    let (cb_x, cb_pad_w, cb_pad_h) = match cb {
        Some(c) => {
            let cbn = doc.get_node(c)?;
            let l = cbn.final_layout;
            let cf = geometry.get(&c)?.fragments.first()?;
            (
                cf.x.to_f32() + l.border.left,
                (l.size.width - l.border.left - l.border.right).max(0.0),
                (l.size.height - l.border.top - l.border.bottom).max(0.0),
            )
        }
        None => (0.0, viewport_w_px, page_h_px),
    };
    let (rel_x, _) = resolve_viewport_cb_location(
        node,
        layout.size.width,
        layout.size.height,
        cb_pad_w,
        cb_pad_h,
    )?;
    let left_explicit = node.primary_styles().is_some_and(|s| {
        matches!(
            s.get_position().left,
            ::style::values::generics::position::GenericInset::LengthPercentage(_)
        )
    });
    Some(
        cb_x + rel_x
            + if left_explicit {
                layout.margin.left
            } else {
                0.0
            },
    )
}

/// Fragment the box `id` from `start` into `out`: a paragraph between its line boxes, any other box with the block
/// fragmenter. Returns `false` when the box cannot be fragmented (the caller keeps the geometry it had).
#[allow(clippy::too_many_arguments)]
fn flow_box(
    out: &mut PaginationGeometryTable,
    doc: &BaseDocument,
    column_styles: Option<&crate::column_css::ColumnStyleTable>,
    multicol: Option<&crate::multicol_layout::MulticolGeometryTable>,
    id: usize,
    x: f32,
    start: (u32, f32),
    strips: PageStrips,
) -> bool {
    let Some(node) = doc.get_node(id) else {
        return false;
    };
    let w = node.final_layout.size.width;
    let (mut page, mut y) = start;
    if let Some(split) = abspos_line_split(node) {
        // A first line that does not fit below the start goes to the next page whole (a line box is monolithic).
        let first = split.lines[0].1 - split.lines[0].0;
        if y + split.inset_top + first > strips.bottom(page) + 0.5 {
            page = page.saturating_add(1);
            y = 0.0;
        }
        if page >= crate::MAX_PAGES {
            return false;
        }
        fragment_nested_inline_root(out, doc, id, x, w, y, page, strips, 0.0, 0.0, &split, 0);
        return true;
    }
    if !collect_inline_line_metrics(node).is_empty() {
        // One line, or a paragraph that must stay one unit: one fragment, on the next page when it does not fit.
        let h = node.final_layout.size.height;
        if y + h > strips.bottom(page) + 1.0 && y > 0.0 {
            page = page.saturating_add(1);
            y = 0.0;
        }
        if page >= crate::MAX_PAGES {
            return false;
        }
        out.entry(id).or_default().fragments.push(Fragment {
            page_index: page,
            x: x.as_px(),
            y: y.as_px(),
            width: w.as_px(),
            height: h.as_px(),
        });
        record_subtree_descendants(out, doc, id, page, y, x, 0);
        return true;
    }
    if page >= crate::MAX_PAGES {
        return false;
    }
    fragment_block_subtree(
        out,
        doc,
        column_styles,
        None,
        multicol,
        id,
        w,
        x,
        page,
        y,
        strips,
        0,
        0.0,
        0.0,
        false,
    );
    true
}

/// F30g: fragment the absolutely positioned boxes whose text the earlier passes lose.
///
/// Called after the body-direct pass ([`append_position_absolute_body_direct_fragments`]) and before the second
/// `position: fixed` pass, so that fixed boxes repeat on any pages the flows add. See the module documentation.
/// `column_styles`, `multicol` and `running_store` are the ones the in-flow pass got.
pub fn fragment_abspos_flows(
    geometry: &mut PaginationGeometryTable,
    doc: &BaseDocument,
    page_height_px: f32,
    viewport_w_px: f32,
    column_styles: Option<&crate::column_css::ColumnStyleTable>,
    multicol: Option<&crate::multicol_layout::MulticolGeometryTable>,
    running_store: Option<&crate::gcpm::running::RunningElementStore>,
) {
    if page_height_px.is_nan() || page_height_px <= 0.0 {
        return;
    }
    let Some(body_id) = find_body_id(doc) else {
        return;
    };
    let boxes = abspos_boxes_in_tree_order(doc, running_store);
    if boxes.is_empty() {
        return;
    }
    let strips = PageStrips::new(page_height_px, body_origin_in_px(doc).1);
    // Body-direct roots left to the window model, and boxes fragmented here: what is inside them is judged by them.
    let mut kept_roots: BTreeSet<usize> = BTreeSet::new();
    let mut flowed: BTreeSet<usize> = BTreeSet::new();
    let mut emitted = 0usize;
    let mut scan_budget = SIBLING_SCAN_BUDGET;
    for id in boxes {
        let Some(node) = doc.get_node(id) else {
            continue;
        };
        let body_direct = node.parent == Some(body_id);
        let (cb, ancestors_guard) = containing_block(doc, id, body_id);
        let guarded = ancestors_guard
            || has_guarded_ancestor(doc, id, body_id)
            || clips_overflow(node)
            || has_transform(node)
            || has_ancestor_in(doc, id, &kept_roots);
        let keep = |kept_roots: &mut BTreeSet<usize>| {
            if body_direct {
                kept_roots.insert(id);
            }
        };
        if guarded {
            // Whatever is nested in a box left as it was is left too.
            kept_roots.insert(id);
            continue;
        }
        if let Some(c) = cb {
            ensure_zero_height_chain(geometry, doc, c, 0);
        }
        if let Some(parent_id) = node.parent {
            ensure_zero_height_chain(geometry, doc, parent_id, 0);
        }
        let Some(anchor) = anchor_of(
            doc,
            geometry,
            id,
            cb,
            viewport_w_px,
            page_height_px,
            &mut scan_budget,
        ) else {
            keep(&mut kept_roots);
            continue;
        };
        let (Some(start), Some(x)) = (
            locate_anchor(doc, geometry, &anchor, strips),
            abspos_x(doc, geometry, id, cb, viewport_w_px, page_height_px),
        ) else {
            keep(&mut kept_roots);
            continue;
        };
        let root_y_icb = if body_direct {
            let l = node.final_layout;
            resolve_viewport_cb_location(
                node,
                l.size.width,
                l.size.height,
                viewport_w_px,
                page_height_px,
            )
            .map_or(l.location.y, |(_, y)| y)
        } else {
            0.0
        };
        let facts = Facts {
            guarded: false,
            body_direct,
            stale: has_ancestor_in(doc, id, &flowed),
            root_y_icb,
        };
        let disposition = abspos_disposition(doc, geometry, id, start, strips, &facts);
        let (w, h) = (node.final_layout.size.width, node.final_layout.size.height);
        let mut tmp = PaginationGeometryTable::new();
        let done = match disposition {
            Disposition::Keep => false,
            Disposition::Place => {
                tmp.entry(id).or_default().fragments.push(Fragment {
                    page_index: start.0,
                    x: x.as_px(),
                    y: start.1.as_px(),
                    width: w.as_px(),
                    height: h.as_px(),
                });
                record_subtree_descendants(&mut tmp, doc, id, start.0, start.1, x, 0);
                true
            }
            Disposition::Flow => {
                flow_box(&mut tmp, doc, column_styles, multicol, id, x, start, strips)
            }
        };
        let added: usize = tmp.values().map(|g| g.fragments.len()).sum();
        if !done || added == 0 || emitted.saturating_add(added) > crate::MAX_SUBTREE_PAGE_FRAGMENTS
        {
            keep(&mut kept_roots);
            continue;
        }
        emitted += added;
        clear_subtree_geometry(geometry, doc, id);
        geometry.extend(tmp);
        if disposition == Disposition::Flow {
            flowed.insert(id);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::blitz_adapter;
    use std::ops::{Deref, DerefMut};

    /// The page content height of the fixtures below.
    const PAGE_H: f32 = 200.0;
    const VIEWPORT_W: f32 = 600.0;

    /// `n` lines of 20 px: `line0<br>line1<br>…`.
    fn lines_20px(n: usize) -> String {
        (0..n)
            .map(|i| format!("line{i}"))
            .collect::<Vec<_>>()
            .join("<br>")
    }

    fn parse(html: &str) -> blitz_html::HtmlDocument {
        let fonts: Vec<std::sync::Arc<Vec<u8>>> = Vec::new();
        let mut doc = blitz_adapter::parse(html, VIEWPORT_W, &fonts);
        blitz_adapter::resolve(&mut doc);
        doc
    }

    fn page_html(body: &str) -> String {
        format!(
            "<!doctype html><html><head><style>body{{margin:0;font-size:16px;line-height:20px}}\
             p{{margin:0}}</style></head><body>{body}</body></html>"
        )
    }

    /// The passes `engine.rs` runs on 200 px pages, up to and including the one under test when `flows` is set.
    fn fragments(
        body: &str,
        flows: bool,
        body_style: &str,
    ) -> (blitz_html::HtmlDocument, PaginationGeometryTable) {
        let html = page_html(body).replace("<body>", &format!("<body style=\"{body_style}\">"));
        let mut doc = parse(&html);
        let table = blitz_adapter::extract_column_style_table(&doc);
        let mut geom = run_pass_with_break_styles(doc.deref_mut(), PAGE_H.as_px(), &table);
        let pages = implied_page_count(&geom).max(1);
        append_position_fixed_fragments(&mut geom, doc.deref_mut(), pages, VIEWPORT_W, PAGE_H);
        append_position_absolute_body_direct_fragments(
            &mut geom,
            doc.deref_mut(),
            pages,
            VIEWPORT_W,
            PAGE_H,
            None,
        );
        if flows {
            fragment_abspos_flows(
                &mut geom,
                doc.deref(),
                PAGE_H,
                VIEWPORT_W,
                Some(&table),
                None,
                None,
            );
        }
        (doc, geom)
    }

    fn by_class(doc: &BaseDocument, class: &str) -> usize {
        fn walk(doc: &BaseDocument, id: usize, class: &str) -> Option<usize> {
            let n = doc.get_node(id)?;
            if n.element_data()
                .and_then(|e| blitz_adapter::get_attr(e, "class"))
                .is_some_and(|c| c.split_whitespace().any(|x| x == class))
            {
                return Some(id);
            }
            n.children.iter().find_map(|&c| walk(doc, c, class))
        }
        walk(doc, doc.root_element().id, class).unwrap_or_else(|| panic!("no .{class}"))
    }

    /// `(page, y, height)` of every fragment of `id`.
    fn shape(geom: &PaginationGeometryTable, id: usize) -> Vec<(u32, i32, i32)> {
        geom.get(&id)
            .map(|g| {
                g.fragments
                    .iter()
                    .map(|f| {
                        (
                            f.page_index,
                            f.y.to_f32().round() as i32,
                            f.height.to_f32().round() as i32,
                        )
                    })
                    .collect()
            })
            .unwrap_or_default()
    }

    fn slices(geom: &PaginationGeometryTable, id: usize) -> Vec<(u32, u32)> {
        geom.get(&id)
            .map(|g| g.line_slices.iter().map(|s| (s.first, s.end)).collect())
            .unwrap_or_default()
    }

    fn pages(geom: &PaginationGeometryTable, id: usize) -> Vec<u32> {
        geom.get(&id)
            .map(|g| g.fragments.iter().map(|f| f.page_index).collect())
            .unwrap_or_default()
    }

    /// The geometry of every node and the page count, with and without the F30g pass: for a box the pass must leave
    /// alone the two are equal.
    fn assert_pass_changes_nothing(body: &str) {
        type Shapes = Vec<(usize, Vec<(u32, i32, i32)>)>;
        let all = |geom: &PaginationGeometryTable| -> Shapes {
            geom.keys().map(|&id| (id, shape(geom, id))).collect()
        };
        let (_, off) = fragments(body, false, "");
        let (_, on) = fragments(body, true, "");
        assert_eq!(all(&off), all(&on));
        assert_eq!(implied_page_count(&off), implied_page_count(&on));
        // And none of the pages the geometry reaches is empty of fragments.
        let reached = implied_page_count(&on);
        for page in 0..reached {
            assert!(
                on.values()
                    .any(|g| g.fragments.iter().any(|f| f.page_index == page)),
                "page {page} of {reached} has no fragment"
            );
        }
    }

    /// F30g validator, `clip_above_cb`: a clipping ancestor above the containing block. The renderer clips the content,
    /// so the pages a flow added came out blank.
    #[test]
    fn abspos_under_a_clip_above_its_cb_is_kept() {
        assert_pass_changes_nothing(&format!(
            "<div style=\"overflow:hidden;height:80px\"><div style=\"position:relative\">\
             <div style=\"position:absolute;top:10px;left:0\">{}</div></div></div>",
            lines_20px(40)
        ));
    }

    /// `clip_above_cb_short`: the same with an auto-height clipping ancestor and a box that only just crosses a page.
    #[test]
    fn abspos_under_an_auto_height_clip_above_its_cb_is_kept() {
        assert_pass_changes_nothing(&format!(
            "<div style=\"overflow:hidden\"><div style=\"position:relative\"><p>head</p>\
             <p style=\"position:absolute;top:150px;left:0\">{}</p></div></div>",
            lines_20px(6)
        ));
    }

    /// An abspos wrapper holding a nested abspos box, both under a clipping ancestor.
    #[test]
    fn nested_abspos_wrapper_under_a_clip_is_kept() {
        assert_pass_changes_nothing(&format!(
            "<div style=\"overflow:hidden;height:80px\"><div style=\"position:relative\">\
             <div style=\"position:absolute;top:0;left:0\"><p>{}</p>\
             <div style=\"position:absolute;top:30px;left:50px;width:200px\">{}</div></div></div></div>",
            lines_20px(10),
            lines_20px(30)
        ));
    }

    /// `xform_above_cb`: a transformed ancestor above the containing block (a transform makes the renderer draw the
    /// geometry it has under the matrix: the added pages were empty).
    #[test]
    fn abspos_under_a_transform_above_its_cb_is_kept() {
        assert_pass_changes_nothing(&format!(
            "<div style=\"transform:translateX(3px)\"><div style=\"position:relative\">\
             <div style=\"position:absolute;top:0;left:0\">{}</div></div></div>",
            lines_20px(40)
        ));
    }

    /// A clip on `<body>` propagates to the viewport and clips nothing on the page: the box is still flowed.
    #[test]
    fn a_clip_on_body_does_not_guard_an_abspos_box() {
        let (doc, geom) = fragments(
            &format!(
                "<div style=\"position:relative\"><p class=\"a\" style=\"position:absolute;top:0;left:0\">{}</p></div>",
                lines_20px(25)
            ),
            true,
            "overflow:hidden",
        );
        assert_eq!(pages(&geom, by_class(&doc, "a")), vec![0, 1, 2]);
    }

    // ── red before F30g ─────────────────────────────────────────────────────────────────────────────────────────

    /// A paragraph of 25 lines (500 px) on 200 px pages: the window gave it three full-height windows and no line
    /// ranges, the renderer printed the first page's lines only. Now the lines are cut between line boxes.
    #[test]
    fn abspos_paragraph_taller_than_a_page_is_split_between_lines() {
        let (doc, geom) = fragments(
            &format!(
                "<p class=\"a\" style=\"position:absolute;top:0;left:0\">{}</p>",
                lines_20px(25)
            ),
            true,
            "",
        );
        let a = by_class(&doc, "a");
        assert_eq!(pages(&geom, a), vec![0, 1, 2], "{:?}", shape(&geom, a));
        assert!(
            line_slices_tile(&geom[&a].line_slices, 25),
            "{:?}",
            slices(&geom, a)
        );
        // Chrome: 10 lines per page; the window of the last page holds the 5 lines left.
        assert_eq!(slices(&geom, a), vec![(0, 10), (10, 20), (20, 25)]);
    }

    /// The same through the engine, so that the wiring in `engine.rs` is covered (A4 pages, 971 px content).
    #[test]
    fn abspos_paragraph_is_split_through_the_engine() {
        let html = page_html(&format!(
            "<p style=\"position:absolute;top:0;left:0\">{}</p>",
            lines_20px(120)
        ));
        let (_, geom) = crate::Engine::builder()
            .build()
            .build_drawables_and_geometry_for_testing_no_gcpm(&html);
        let split = geom
            .values()
            .find(|g| !g.line_slices.is_empty())
            .expect("a paragraph split between lines");
        assert!(line_slices_tile(&split.line_slices, 120));
        assert_eq!(
            split
                .fragments
                .iter()
                .map(|f| f.page_index)
                .collect::<Vec<_>>(),
            vec![0, 1, 2]
        );
    }

    /// A relative `div` without height recorded alone: the paragraph inside it had no geometry and was not drawn.
    #[test]
    fn abspos_paragraph_in_a_zero_height_relative_parent_gets_geometry() {
        let body = |top: &str| {
            format!(
                "<div style=\"position:relative\"><p class=\"a\" style=\"position:absolute;top:{top};left:0\">{}</p></div>",
                lines_20px(5)
            )
        };
        let (doc, geom) = fragments(&body("0"), true, "");
        let a = by_class(&doc, "a");
        assert_eq!(shape(&geom, a), vec![(0, 0, 100)]);
        // Crossing the page: 150 px down leaves room for two lines, the other three go to the next page.
        let (doc, geom) = fragments(&body("150px"), true, "");
        let a = by_class(&doc, "a");
        assert_eq!(slices(&geom, a), vec![(0, 2), (2, 5)]);
        assert_eq!(pages(&geom, a), vec![0, 1]);
        assert_eq!(shape(&geom, a)[0].1, 150);
    }

    /// The parent is entered by the fragmenter (60 px per paragraph, 300 px on 200 px pages), which skips its
    /// out-of-flow children: the stamp had no geometry.
    #[test]
    fn abspos_child_of_an_entered_parent_is_placed_at_its_cb() {
        let rows: String = (0..15).map(|i| format!("<p>row{i}</p>")).collect();
        let (doc, geom) = fragments(
            &format!(
                "<div class=\"rel\" style=\"position:relative\">{rows}\
                 <div class=\"stamp\" style=\"position:absolute;top:0;right:0\">ZAPLACENO</div></div>"
            ),
            true,
            "",
        );
        let rel = by_class(&doc, "rel");
        assert_eq!(pages(&geom, rel), vec![0, 1], "{:?}", shape(&geom, rel));
        let stamp = by_class(&doc, "stamp");
        assert_eq!(pages(&geom, stamp), vec![0]);
        assert_eq!(shape(&geom, stamp)[0].1, 0);
        // `right: 0`: its right edge is the right edge of the containing block.
        let f = &geom[&stamp].fragments[0];
        let rel_f = &geom[&rel].fragments[0];
        assert!(
            ((f.x + f.width).to_f32() - (rel_f.x + rel_f.width).to_f32()).abs() < 1.0,
            "{f:?} in {rel_f:?}"
        );
    }

    /// A wrapper of two paragraphs, 300 px each: fragmented from its start with a frame per page, each paragraph
    /// split between its lines.
    #[test]
    fn abspos_wrapper_is_fragmented_from_its_start() {
        let (doc, geom) = fragments(
            &format!(
                "<div class=\"w\" style=\"position:absolute;top:0;left:0\">\
                 <p class=\"p1\">{}</p><p class=\"p2\">{}</p></div>",
                lines_20px(15),
                lines_20px(15)
            ),
            true,
            "",
        );
        let w = by_class(&doc, "w");
        assert_eq!(pages(&geom, w), vec![0, 1, 2], "{:?}", shape(&geom, w));
        for class in ["p1", "p2"] {
            let p = by_class(&doc, class);
            assert!(
                line_slices_tile(&geom[&p].line_slices, 15),
                "{class}: {:?}",
                slices(&geom, p)
            );
        }
        // Page 0 holds the first ten lines of p1, page 1 the rest of p1 and the first lines of p2.
        assert_eq!(shape(&geom, by_class(&doc, "p1"))[0].0, 0);
        assert_eq!(shape(&geom, by_class(&doc, "p2")).last().unwrap().0, 2);
    }

    /// `bottom: 0` is the bottom of the containing block's last fragment (page 1, 100 px tall), not of the first.
    #[test]
    fn abspos_bottom_anchored_in_a_split_cb_lands_on_its_last_page() {
        let rows: String = (0..15).map(|i| format!("<p>row{i}</p>")).collect();
        let (doc, geom) = fragments(
            &format!(
                "<div class=\"rel\" style=\"position:relative\">{rows}\
                 <div class=\"stamp\" style=\"position:absolute;bottom:0;right:0\">ZAPLACENO</div></div>"
            ),
            true,
            "",
        );
        let stamp = by_class(&doc, "stamp");
        assert_eq!(shape(&geom, stamp), vec![(1, 80, 20)]);
    }

    /// The block fragmenter closes the last fragment of a split box at its last child, so the bottom padding is not in
    /// the fragments: `bottom: 0` still lands at the bottom of the padding box (40 px below the last row).
    #[test]
    fn abspos_bottom_anchored_counts_the_bottom_padding_of_a_split_cb() {
        let rows: String = (0..15).map(|i| format!("<p>row{i}</p>")).collect();
        let (doc, geom) = fragments(
            &format!(
                "<div class=\"rel\" style=\"position:relative;padding-bottom:40px\">{rows}\
                 <div class=\"stamp\" style=\"position:absolute;bottom:0;right:0\">ZAPLACENO</div></div>"
            ),
            true,
            "",
        );
        assert_eq!(shape(&geom, by_class(&doc, "stamp")), vec![(1, 120, 20)]);
    }

    /// `top: auto`: after the previous sibling. That sibling does not fit the 50 px left and moves whole to page 1,
    /// so the box follows it there (Taffy's offset, 250 px, would put it 50 px down page 1, inside the sibling). A box
    /// whose text fits the window of its page keeps the window's position (`abspos_that_fits_keeps_its_geometry`).
    #[test]
    fn abspos_static_position_follows_a_moved_sibling() {
        let (doc, geom) = fragments(
            &format!(
                "<div style=\"height:150px\"></div><div class=\"sib\" style=\"height:100px\"></div>\
                 <div class=\"a\" style=\"position:absolute\"><p class=\"p\">{}</p></div>",
                lines_20px(25)
            ),
            true,
            "",
        );
        assert_eq!(shape(&geom, by_class(&doc, "sib")), vec![(1, 0, 100)]);
        let a = shape(&geom, by_class(&doc, "a"));
        assert_eq!((a[0].0, a[0].1), (1, 100), "{a:?}");
        // 100 px left on page 1 hold five lines, then two pages of ten.
        assert_eq!(
            slices(&geom, by_class(&doc, "p")),
            vec![(0, 5), (5, 15), (15, 25)]
        );
    }

    /// The containing block sits in a chain of zero-height wrappers, which the in-flow pass records alone: the chain
    /// gets zero-height fragments so that the box can hang on its containing block.
    #[test]
    fn abspos_in_a_chain_of_zero_height_wrappers_gets_geometry() {
        let (doc, geom) = fragments(
            &format!(
                "<p>before</p><div><div class=\"rel\" style=\"position:relative\">\
                 <p class=\"a\" style=\"position:absolute;top:0;left:0\">{}</p></div></div>",
                lines_20px(5)
            ),
            true,
            "",
        );
        let rel = by_class(&doc, "rel");
        assert_eq!(shape(&geom, rel), vec![(0, 20, 0)]);
        assert_eq!(shape(&geom, by_class(&doc, "a")), vec![(0, 20, 100)]);
    }

    /// A paragraph in an absolutely positioned wrapper that has no text of its own: the window cuts the paragraph, so
    /// the wrapper is fragmented again and the paragraph with it.
    #[test]
    fn abspos_paragraph_nested_in_an_abspos_wrapper_is_split() {
        let (doc, geom) = fragments(
            &format!(
                "<div class=\"w\" style=\"position:absolute;top:20px;left:0\">\
                 <p class=\"a\" style=\"position:absolute;top:0;left:0\">{}</p></div>",
                lines_20px(25)
            ),
            true,
            "",
        );
        let a = by_class(&doc, "a");
        assert!(
            line_slices_tile(&geom[&a].line_slices, 25),
            "{:?} {:?}",
            shape(&geom, a),
            slices(&geom, a)
        );
        assert_eq!(shape(&geom, a)[0].1, 20);
    }

    /// `top: 0; bottom: 0` in a body with nothing else in flow: Taffy stretches the box against its parent, which is
    /// empty, so the box is 0 px tall and the window pass drops it. Its text overflows it (visible overflow) and
    /// goes on over the pages after it (Chrome: 3 pages for 1000 words).
    #[test]
    fn abspos_stretched_by_top_and_bottom_in_an_empty_body_flows() {
        let (doc, geom) = fragments(
            &format!(
                "<div class=\"a\" style=\"position:absolute;top:0;bottom:0\">{}</div>",
                lines_20px(25)
            ),
            true,
            "",
        );
        let a = by_class(&doc, "a");
        assert_eq!(pages(&geom, a), vec![0, 1, 2], "{:?}", shape(&geom, a));
        assert_eq!(slices(&geom, a), vec![(0, 10), (10, 20), (20, 25)]);
    }

    /// The pages a flow adds are known to the second fixed pass: a fixed box repeats on them (CSS 2.1 §9.6.1).
    #[test]
    fn fixed_box_repeats_on_the_pages_an_abspos_flow_adds() {
        let html = page_html(&format!(
            "<div class=\"fx\" style=\"position:fixed;top:0;width:10px;height:10px\"></div>\
             <p style=\"position:absolute;top:0;left:0\">{}</p>",
            lines_20px(120)
        ));
        let (_, geom) = crate::Engine::builder()
            .build()
            .build_drawables_and_geometry_for_testing_no_gcpm(&html);
        let mut fixed_pages: Vec<u32> = geom
            .values()
            .filter(|g| g.is_repeat)
            .flat_map(|g| g.fragments.iter().map(|f| f.page_index))
            .collect();
        fixed_pages.sort_unstable();
        fixed_pages.dedup();
        assert_eq!(fixed_pages, vec![0, 1, 2]);
    }

    // ── guards ──────────────────────────────────────────────────────────────────────────────────────────────────

    /// `(node, fragments, line ranges)`.
    type TableRow = (usize, Vec<(u32, i32, i32)>, Vec<(u32, u32)>);

    /// What the geometry of every node comes to, for comparing two runs.
    fn table_shape(geom: &PaginationGeometryTable) -> Vec<TableRow> {
        geom.keys()
            .map(|&id| (id, shape(geom, id), slices(geom, id)))
            .collect()
    }

    /// A box that fits keeps exactly the geometry the earlier passes gave it: the table is the same before and after.
    #[test]
    fn abspos_that_fits_keeps_its_geometry() {
        let body = format!(
            "<div style=\"position:relative;height:100px\">\
             <div class=\"n\" style=\"position:absolute;top:20px;left:20px\">Poznámka</div></div>\
             <div class=\"b\" style=\"position:absolute;top:30px;left:10px;height:40px\">{}</div>\
             <div class=\"c\" style=\"position:absolute;bottom:10px;right:0\">{}</div>",
            lines_20px(2),
            lines_20px(2)
        );
        let (_, before) = fragments(&body, false, "");
        let (_, after) = fragments(&body, true, "");
        assert_eq!(table_shape(&before), table_shape(&after));
    }

    /// A body margin shifts page 0 only; positions are kept in the page area, and no fragment sits above its page.
    #[test]
    fn abspos_paragraph_with_a_body_margin_starts_at_the_page_top() {
        let (doc, geom) = fragments(
            &format!(
                "<p class=\"a\" style=\"position:absolute;top:0;left:0\">{}</p>",
                lines_20px(25)
            ),
            true,
            "margin:40px",
        );
        let a = by_class(&doc, "a");
        let s = shape(&geom, a);
        // Page 0 is shorter by the margin (160 px: 8 lines), the flow starts at the top of the page area.
        assert_eq!(s[0].0, 0);
        assert_eq!(s[0].1, -40, "body coordinates: 40 px above the body top");
        assert!(s.iter().skip(1).all(|&(_, y, _)| y == 0), "{s:?}");
        assert!(line_slices_tile(&geom[&a].line_slices, 25));
        let page0_lines = geom[&a].line_slices[0].end;
        assert_eq!(page0_lines, 10, "the 200 px of page 0 hold 10 lines");
    }

    // ── units ───────────────────────────────────────────────────────────────────────────────────────────────────

    fn frag(page: u32, y: f32, h: f32) -> Fragment {
        Fragment {
            page_index: page,
            x: 0.0.as_px(),
            y: y.as_px(),
            width: 10.0.as_px(),
            height: h.as_px(),
        }
    }

    #[test]
    fn locate_forward_maps_an_offset_through_the_fragments() {
        let strips = PageStrips::new(200.0, 0.0);
        let frags = [
            frag(0, 120.0, 80.0),
            frag(1, 0.0, 200.0),
            frag(2, 0.0, 50.0),
        ];
        assert_eq!(locate_forward(&frags, 0.0, strips), (0, 120.0));
        assert_eq!(locate_forward(&frags, 79.0, strips), (0, 199.0));
        assert_eq!(locate_forward(&frags, 80.0, strips), (1, 0.0));
        assert_eq!(locate_forward(&frags, 300.0, strips), (2, 20.0));
        // Past the end continues with full strips; before the start stays above the first fragment.
        assert_eq!(locate_forward(&frags, 330.0, strips), (2, 50.0));
        assert_eq!(locate_forward(&frags, 400.0, strips), (2, 120.0));
        assert_eq!(locate_forward(&frags, 500.0, strips), (3, 20.0));
        assert_eq!(locate_forward(&frags, -10.0, strips), (0, 110.0));
        assert_eq!(locate_forward(&[], 5.0, strips), (0, 0.0));
    }

    #[test]
    fn locate_backward_counts_from_the_end() {
        let strips = PageStrips::new(200.0, 0.0);
        let frags = [frag(0, 120.0, 80.0), frag(1, 0.0, 100.0)];
        assert_eq!(locate_backward(&frags, 20.0, 0.0, strips), (1, 80.0));
        assert_eq!(locate_backward(&frags, 100.0, 0.0, strips), (1, 0.0));
        assert_eq!(locate_backward(&frags, 120.0, 0.0, strips), (0, 180.0));
        assert_eq!(locate_backward(&frags, 0.0, 0.0, strips), (1, 100.0));
        // Further back than the box: above its first fragment.
        assert_eq!(locate_backward(&frags, 200.0, 0.0, strips), (0, 100.0));
    }

    #[test]
    fn carry_over_moves_what_is_below_the_page_to_the_next() {
        let strips = PageStrips::new(200.0, 40.0);
        assert_eq!(carry_over(0, 100.0, strips), (0, 100.0));
        assert_eq!(carry_over(0, 160.0, strips), (1, 0.0), "page 0 ends at 160");
        assert_eq!(carry_over(0, 170.0, strips), (1, 10.0));
        assert_eq!(carry_over(1, 450.0, strips), (3, 50.0));
        assert_eq!(carry_over(1, f32::NAN, strips), (1, 0.0));
        assert_eq!(carry_over(0, 1e12, strips).0, crate::MAX_PAGES);
    }

    #[test]
    fn locate_icb_takes_the_body_offset_from_page_zero_only() {
        let strips = PageStrips::new(200.0, 40.0);
        assert_eq!(locate_icb(0.0, strips), (0, -40.0));
        assert_eq!(locate_icb(100.0, strips), (0, 60.0));
        assert_eq!(locate_icb(250.0, strips), (1, 50.0));
        // A start within the vh tolerance of a page edge lands on it (`top: 100vh` is 199.99 px here).
        assert_eq!(locate_icb(199.995, strips), (1, 0.0));
        assert_eq!(locate_icb(-5.0, strips), (0, -45.0));
    }

    #[test]
    fn window_cuts_lines_only_for_paragraphs_that_cross_a_page_row() {
        let cut = |body: &str, top: f32| {
            let doc = parse(&page_html(body));
            let a = by_class(&doc, "a");
            window_cuts_lines(&doc, a, top, PAGE_H)
        };
        let p = |n| {
            format!(
                "<p class=\"a\" style=\"position:absolute\">{}</p>",
                lines_20px(n)
            )
        };
        assert!(!cut(&p(5), 0.0), "five lines fit the first row");
        assert!(cut(&p(15), 0.0), "fifteen lines cross it");
        assert!(!cut(&p(1), 200.0), "one line that starts on the page edge");
        assert!(cut(&p(1), 190.0), "one line straddling the edge");
        assert!(!cut(&p(5), 200.0), "five lines on the second row");
        assert!(cut(&p(5), 160.0), "five lines from 160 px");
        let wrapped = format!(
            "<div class=\"a\" style=\"position:absolute\"><p>{}</p><p>{}</p></div>",
            lines_20px(2),
            lines_20px(2)
        );
        assert!(!cut(&wrapped, 0.0));
        assert!(cut(&wrapped, 170.0), "the first paragraph crosses");
        let empty = "<div class=\"a\" style=\"position:absolute;height:900px\"></div>";
        assert!(!cut(empty, 0.0), "no text, nothing to cut");
    }

    #[test]
    fn disposition_arms() {
        let strips = PageStrips::new(PAGE_H, 0.0);
        let facts = |body_direct: bool| Facts {
            guarded: false,
            body_direct,
            stale: false,
            root_y_icb: 0.0,
        };
        // No geometry, fits below its start: Place; does not fit: Flow.
        let doc = parse(&page_html(&format!(
            "<div style=\"position:relative\"><p class=\"a\" style=\"position:absolute\">{}</p></div>",
            lines_20px(5)
        )));
        let a = by_class(&doc, "a");
        let empty = PaginationGeometryTable::new();
        assert_eq!(
            abspos_disposition(&doc, &empty, a, (0, 0.0), strips, &facts(false)),
            Disposition::Place
        );
        assert_eq!(
            abspos_disposition(&doc, &empty, a, (0, 150.0), strips, &facts(false)),
            Disposition::Flow
        );
        // Guarded boxes are kept whatever they hold.
        let guarded = Facts {
            guarded: true,
            ..facts(false)
        };
        assert_eq!(
            abspos_disposition(&doc, &empty, a, (0, 150.0), strips, &guarded),
            Disposition::Keep
        );
        // Geometry that holds its lines: Keep; lines that end below the page: Flow; stale geometry is no geometry.
        let mut geom = PaginationGeometryTable::new();
        geom.entry(a)
            .or_default()
            .fragments
            .push(frag(0, 0.0, 100.0));
        assert_eq!(
            abspos_disposition(&doc, &geom, a, (0, 0.0), strips, &facts(false)),
            Disposition::Keep
        );
        geom.get_mut(&a).unwrap().fragments[0] = frag(0, 150.0, 100.0);
        assert_eq!(
            abspos_disposition(&doc, &geom, a, (0, 150.0), strips, &facts(false)),
            Disposition::Flow
        );
        let stale = Facts {
            stale: true,
            ..facts(false)
        };
        assert_eq!(
            abspos_disposition(&doc, &geom, a, (0, 0.0), strips, &stale),
            Disposition::Place
        );
        // A body-direct root stays on the window model unless the window cuts its lines; without geometry (the window
        // pass dropped it) it is never drawn here.
        let doc = parse(&page_html(&format!(
            "<p class=\"a\" style=\"position:absolute\">{}</p>",
            lines_20px(15)
        )));
        let a = by_class(&doc, "a");
        let mut geom = PaginationGeometryTable::new();
        geom.entry(a)
            .or_default()
            .fragments
            .push(frag(0, 0.0, 300.0));
        assert_eq!(
            abspos_disposition(&doc, &geom, a, (0, 0.0), strips, &facts(true)),
            Disposition::Flow
        );
        assert_eq!(
            abspos_disposition(&doc, &empty, a, (0, 0.0), strips, &facts(true)),
            Disposition::Keep
        );
    }

    #[test]
    fn abspos_boxes_come_outer_first_and_skip_fixed_and_multicol() {
        let doc = parse(&page_html(
            "<div class=\"o\" style=\"position:absolute\"><div class=\"i\" style=\"position:absolute\">x</div></div>\
             <div style=\"position:fixed\"><div class=\"f\" style=\"position:absolute\">y</div></div>\
             <div style=\"columns:2\"><div class=\"m\" style=\"position:absolute\">z</div></div>\
             <div class=\"s\" style=\"position:absolute\">w</div>",
        ));
        let order = abspos_boxes_in_tree_order(&doc, None);
        let ids: Vec<usize> = ["o", "i", "s"].iter().map(|c| by_class(&doc, c)).collect();
        assert_eq!(order, ids);
    }

    #[test]
    fn clear_subtree_geometry_spares_nested_out_of_flow_boxes() {
        let doc = parse(&page_html(
            "<div class=\"o\" style=\"position:absolute\"><p class=\"p\">x</p>\
             <div class=\"n\" style=\"position:absolute\">y</div></div>",
        ));
        let mut geom = PaginationGeometryTable::new();
        for class in ["o", "p", "n"] {
            geom.entry(by_class(&doc, class))
                .or_default()
                .fragments
                .push(frag(0, 0.0, 10.0));
        }
        clear_subtree_geometry(&mut geom, &doc, by_class(&doc, "o"));
        assert!(!geom.contains_key(&by_class(&doc, "o")));
        assert!(!geom.contains_key(&by_class(&doc, "p")));
        assert!(geom.contains_key(&by_class(&doc, "n")));
    }

    #[test]
    fn abspos_line_split_needs_two_lines_and_a_plain_box() {
        let doc = parse(&page_html(&format!(
            "<p class=\"two\">{}</p><p class=\"one\">x</p><p class=\"fl\" style=\"float:left\">{}</p>\
             <p class=\"ov\" style=\"overflow:hidden\">{}</p>",
            lines_20px(2),
            lines_20px(3),
            lines_20px(3)
        )));
        let node = |c: &str| doc.get_node(by_class(&doc, c)).unwrap();
        assert!(abspos_line_split(node("two")).is_some());
        assert!(abspos_line_split(node("one")).is_none());
        assert!(abspos_line_split(node("fl")).is_none());
        assert!(abspos_line_split(node("ov")).is_none());
    }

    /// Thousands of boxes without insets must not make the sibling search quadratic: 15 000 of them are searched
    /// within the scan budget (a quadratic search is 10^8 steps and takes minutes in a debug build).
    #[test]
    fn many_static_abspos_siblings_are_bounded() {
        let boxes = "<div style=\"position:absolute\">x</div>".repeat(15_000);
        let started = std::time::Instant::now();
        let (_, geom) = fragments(&boxes, true, "");
        assert!(geom.len() > 15_000);
        assert!(started.elapsed().as_secs() < 60, "{:?}", started.elapsed());
    }
}
