//! html-to-pdf F215: keep-with-next (`break-before` / `break-after: avoid`, css-break-3 §3.1, §4.4 Rule 1).
//!
//! The walks of [`super::fragment_pagination_root`] and [`super::fragment_block_subtree`] are greedy and single pass: a
//! box is placed where the cursor is and moves to the next page when the part of it that cannot be split does not fit
//! the room. Keep-with-next extends that prediction across an avoided break: a box (the *head*) that has `avoid` between
//! it and the next in-flow sibling moves to the next page together with that sibling when the sibling does not start on
//! this one. No re-run and no rollback: the extent of the next box is asked of the same predicates the walk uses to place
//! that box ([`first_fragment_extent`]), so the head moves exactly when the next box would move on its own.
//!
//! Everything here is gated by [`super::SubtreeFacts::keeps_with_next`]: a document that declares no `avoid` value takes
//! the paths it took before F215, byte for byte. Plan: `docs/plans/2026-11-02-f215-keep-with-next.md` (html-to-pdf).
//!
//! Out of scope (filed): a breakable head (a long paragraph, a table: F215b), the propagation of a first / last child's
//! value to its parent's edge (F215c), a relaxed `break-inside: avoid` box after a head (F215d), flex / grid items and
//! multicol (F215e).

use super::{
    BaseDocument, avoid_inside_holds, avoid_measure, avoid_piece_size, block_size_constrained,
    collect_inline_line_metrics, has_allowed_break, has_avoid, is_table_box, leading_block_size,
    line_rules, line_split_candidate, may_start_in_place, multicol_container,
};
use crate::blitz_adapter::UsedPageNameTable;
use crate::column_css::ColumnStyleTable;
use crate::draw_primitives::{BreakAfter, BreakBefore};

/// What the keep needs to know about the walk that asks.
#[derive(Clone, Copy)]
pub(super) struct KeepEnv<'a> {
    pub styles: &'a ColumnStyleTable,
    pub names: Option<&'a UsedPageNameTable>,
    pub multicol: Option<&'a crate::multicol_layout::MulticolGeometryTable>,
    /// `true` for the walk over `<body>`'s children, `false` for the nested walk: the two start a paragraph that does not
    /// fit the room in place by different predicates.
    pub body_level: bool,
    /// Body-level only: `<body>` is a block flow container ([`super::is_block_flow_container`]).
    pub body_block_flow: bool,
}

/// The block-size the head needs below its own top so that the next box starts where the head is, and the paragraph
/// (if any) that has to be started in place with Rule 3 dropped.
#[derive(Clone, Copy, Debug, PartialEq)]
pub(super) struct KeepExtent {
    /// From the top of the head to the bottom of the part of the last chain member that has to be on the same page.
    pub size: f32,
    /// A multi-line paragraph without an allowed break that the chain cannot avoid relaxing (css-break-3 §4.4: Rule 3
    /// gives way before Rules 1, 2 and 4): it starts where it fits its first line, and its break takes the last
    /// fallback of [`super::choose_line_break_with`].
    pub relax_rule3: Option<usize>,
}

/// Whether a page break between `prev` and `next`, two in-flow siblings in block flow (a Class A break point,
/// css-break-3 §4.1), is forbidden by `break-after` of `prev` or `break-before` of `next` (§4.4 Rule 1). A forced value
/// on either side wins over `avoid` (§4.3), and so does an implicit break between boxes with different used page names
/// (CSS Page 3 §5.3): both make the break unavoidable, so the answer is `false`.
pub(super) fn avoid_page_break_between(
    table: &ColumnStyleTable,
    names: Option<&UsedPageNameTable>,
    prev: usize,
    next: usize,
) -> bool {
    let after = table.get(&prev).and_then(|p| p.break_after);
    let before = table.get(&next).and_then(|p| p.break_before);
    if matches!(after, Some(BreakAfter::Page)) || matches!(before, Some(BreakBefore::Page)) {
        return false;
    }
    if let Some(names) = names {
        let none: Option<String> = None;
        let end = names.get(&prev).map_or(&none, |e| &e.1);
        let start = names.get(&next).map_or(&none, |e| &e.0);
        if end != start {
            return false;
        }
    }
    after.is_some_and(BreakAfter::avoids_page_break)
        || before.is_some_and(BreakBefore::avoids_page_break)
}

/// The room-independent shape a body-level paragraph needs to start where it is when it does not fit the room, minus the
/// length rule ([`has_allowed_break`]): in block flow, no `break-inside: avoid`, no float, no insets, no clip, no multicol,
/// no block size of its own, and its box is its lines (`child_h` is not more than `para_total_h` plus the line-box slack).
/// The conjunction `fragment_pagination_root` used inline for `start_here`; it is extracted so that the walk and the keep
/// share it.
#[allow(clippy::too_many_arguments)]
pub(super) fn body_paragraph_shape_ok(
    child: &blitz_dom::Node,
    avoid_inside: bool,
    is_float: bool,
    body_block_flow: bool,
    insets: f32,
    no_clip: bool,
    child_h: f32,
    para_total_h: f32,
) -> bool {
    !avoid_inside
        && !is_float
        && insets == 0.0
        && no_clip
        && body_block_flow
        && !crate::blitz_adapter::is_multicol_container(child)
        && !block_size_constrained(child)
        && child_h <= para_total_h + super::LINE_BOX_SLACK
}

/// [`body_paragraph_shape_ok`] for a paragraph the keep looks at, from the node alone, and the length rule: the
/// paragraph starts in place when it does not fit the room (`start_here` without its room test).
/// `lines` are its line boxes ([`collect_inline_line_metrics`]).
pub(super) fn body_paragraph_may_start(
    doc: &BaseDocument,
    child: &blitz_dom::Node,
    avoid_inside: bool,
    is_float: bool,
    body_block_flow: bool,
    lines: &[(f32, f32)],
    fresh: f32,
) -> bool {
    let (Some(first), Some(last)) = (lines.first(), lines.last()) else {
        return false;
    };
    let rules = line_rules(doc, child);
    body_paragraph_shape(doc, child, avoid_inside, is_float, body_block_flow, lines)
        && has_allowed_break(lines.len(), last.1 - first.0 > fresh + 1.0, rules)
}

/// [`body_paragraph_shape_ok`] with its inputs read from the node.
fn body_paragraph_shape(
    doc: &BaseDocument,
    child: &blitz_dom::Node,
    avoid_inside: bool,
    is_float: bool,
    body_block_flow: bool,
    lines: &[(f32, f32)],
) -> bool {
    let layout = child.final_layout;
    let child_h = if layout.size.height.is_finite() {
        layout.size.height
    } else {
        0.0
    };
    let insets =
        layout.border.top + layout.padding.top + layout.border.bottom + layout.padding.bottom;
    let para_total_h = lines.last().map_or(child_h, |l| l.1 - lines[0].0);
    let no_clip = !(body_block_flow
        && !is_float
        && !avoid_inside
        && !crate::blitz_adapter::is_multicol_container(child))
        || super::clip_block_limit(doc, child).is_none();
    body_paragraph_shape_ok(
        child,
        avoid_inside,
        is_float,
        body_block_flow,
        insets,
        no_clip,
        child_h,
        para_total_h,
    )
}

fn height_of(node: &blitz_dom::Node) -> f32 {
    let h = node.final_layout.size.height;
    if h.is_finite() { h.max(0.0) } else { 0.0 }
}

/// How the walks see a sibling for the purpose of the chain.
enum Sibling {
    /// Not in flow (whitespace text, `position: absolute | fixed`): not a neighbour.
    Skip,
    /// A box with no height: it takes part in no break decision, unless it forces one.
    Zero(usize),
    /// An in-flow box with a height.
    Flow,
}

fn classify(doc: &BaseDocument, id: usize) -> Sibling {
    let Some(node) = doc.get_node(id) else {
        return Sibling::Skip;
    };
    if let Some(text) = node.text_data()
        && text.content.chars().all(char::is_whitespace)
    {
        return Sibling::Skip;
    }
    {
        use ::style::properties::longhands::position::computed_value::T as Pos;
        let out_of_flow = node
            .primary_styles()
            .is_some_and(|s| matches!(s.get_box().clone_position(), Pos::Absolute | Pos::Fixed));
        if out_of_flow {
            return Sibling::Skip;
        }
    }
    if height_of(node) <= 0.0 {
        Sibling::Zero(id)
    } else {
        Sibling::Flow
    }
}

/// The in-flow neighbour of `siblings[index]` (forward or backward): its index, or `None` at the end of the parent or at a
/// zero-height box that forces a break (the chain does not reach across it).
fn neighbour(
    doc: &BaseDocument,
    siblings: &[usize],
    index: usize,
    forward: bool,
    styles: &ColumnStyleTable,
) -> Option<usize> {
    let mut i = index;
    loop {
        i = if forward {
            i.checked_add(1).filter(|&i| i < siblings.len())?
        } else {
            i.checked_sub(1)?
        };
        match classify(doc, siblings[i]) {
            Sibling::Skip => {}
            Sibling::Zero(id) => {
                let forced = styles.get(&id).is_some_and(|p| {
                    matches!(p.break_before, Some(BreakBefore::Page))
                        || matches!(p.break_after, Some(BreakAfter::Page))
                });
                if forced {
                    return None;
                }
            }
            Sibling::Flow => return Some(i),
        }
    }
}

/// `Some(height)` when the walk places `node_id` whole on a fresh page: an inline root without an allowed break (a
/// one-line heading, a short paragraph), a leaf or replaced box, a `break-inside: avoid` box that holds, a box whose
/// first row is all of it. `None` for anything with an allowed break inside it (a long paragraph, a table of two or more
/// rows, a wrapper): such a head keeps only its first fragment with the next box, which is F215b.
pub(super) fn unbreakable_extent(
    doc: &BaseDocument,
    node_id: usize,
    styles: &ColumnStyleTable,
    fresh: f32,
) -> Option<f32> {
    let node = doc.get_node(node_id)?;
    let h = height_of(node);
    if crate::blitz_adapter::node_is_floating(node) {
        return None;
    }
    if node.flags.is_inline_root() {
        let lines = collect_inline_line_metrics(node);
        let (Some(first), Some(last)) = (lines.first(), lines.last()) else {
            return Some(h);
        };
        if lines.len() < 2 {
            return Some(h);
        }
        let extent = last.1 - first.0;
        if avoid_inside_holds(has_avoid(Some(styles), node_id), extent, fresh) {
            return Some(h);
        }
        let rules = line_rules(doc, node);
        return (!has_allowed_break(lines.len(), extent > fresh + 1.0, rules)).then_some(h);
    }
    if crate::blitz_adapter::is_multicol_container(node) || is_table_box(node) {
        return None;
    }
    if has_avoid(Some(styles), node_id)
        && avoid_inside_holds(true, avoid_measure(doc, node_id, node, Some(styles)), fresh)
    {
        return Some(h);
    }
    (leading_block_size(doc, node_id, Some(styles), 0.0, 0, fresh) >= h - 0.5).then_some(h)
}

/// The block-size the walk needs below the top of `node_id` so that it starts where it is: the same predicates the walk
/// uses to move the box (see the module docs).
///
/// - a body-level paragraph that starts in place ([`body_paragraph_may_start`]): its first `orphans` lines, the operands
///   of `starts_in_place`; one that does not moves whole: all its lines;
/// - a nested paragraph that starts in place ([`may_start_in_place`]) or is taller than a page: [`super::LineSplit::
///   leading_within`]; another one: its height;
/// - a held `break-inside: avoid` box: [`avoid_piece_size`];
/// - a multicol container: its top inset and first unit;
/// - anything else: [`leading_block_size`], what the F28 cut counts for a box it enters.
pub(super) fn first_fragment_extent(
    doc: &BaseDocument,
    parent: &blitz_dom::Node,
    node_id: usize,
    env: KeepEnv<'_>,
    fresh: f32,
) -> f32 {
    let Some(node) = doc.get_node(node_id) else {
        return 0.0;
    };
    let h = height_of(node);
    let avoid = has_avoid(Some(env.styles), node_id);
    if let Some(mc) = multicol_container(doc, env.multicol, Some(env.styles), node_id, fresh) {
        return if avoid {
            h
        } else {
            (mc.top_inset() + mc.first_unit()).min(h)
        };
    }
    if node.flags.is_inline_root() {
        let lines = collect_inline_line_metrics(node);
        let (Some(first), Some(last)) = (lines.first(), lines.last()) else {
            return h;
        };
        if lines.len() < 2 {
            return h;
        }
        let extent = last.1 - first.0;
        if avoid_inside_holds(avoid, extent, fresh) {
            return h;
        }
        let is_float = crate::blitz_adapter::node_is_floating(node);
        if env.body_level {
            let rules = line_rules(doc, node);
            if body_paragraph_may_start(
                doc,
                node,
                avoid,
                is_float,
                env.body_block_flow,
                &lines,
                fresh,
            ) && let Some(keep) = lines.get(rules.orphans.saturating_sub(1))
            {
                return keep.1 - first.0;
            }
            return extent;
        }
        if let Some(split) = line_split_candidate(doc, parent, node) {
            let tall = split.inset_top + split.span() > fresh + 1.0;
            if tall || may_start_in_place(doc, parent, node, &split, avoid, fresh) {
                return split.leading_within(fresh).min(h);
            }
        }
        return h;
    }
    if avoid
        && avoid_inside_holds(
            true,
            avoid_measure(doc, node_id, node, Some(env.styles)),
            fresh,
        )
    {
        return avoid_piece_size(doc, node_id, Some(env.styles), 0).min(h);
    }
    leading_block_size(doc, node_id, Some(env.styles), 0.0, 0, fresh)
}

/// The first line of a multi-line paragraph that has no allowed break and that the chain cannot avoid relaxing: the part
/// of it that has to fit where it starts when Rule 3 is dropped (css-break-3 §4.4). `None` for any other box.
fn relaxable_first_line(
    doc: &BaseDocument,
    parent: &blitz_dom::Node,
    node_id: usize,
    env: KeepEnv<'_>,
    fresh: f32,
) -> Option<f32> {
    let node = doc.get_node(node_id)?;
    if !node.flags.is_inline_root() {
        return None;
    }
    let lines = collect_inline_line_metrics(node);
    let (first, last) = (lines.first()?, lines.last()?);
    if lines.len() < 2 {
        return None;
    }
    let avoid = has_avoid(Some(env.styles), node_id);
    let extent = last.1 - first.0;
    if avoid_inside_holds(avoid, extent, fresh) {
        return None;
    }
    let rules = line_rules(doc, node);
    if has_allowed_break(lines.len(), extent > fresh + 1.0, rules) {
        return None;
    }
    let is_float = crate::blitz_adapter::node_is_floating(node);
    let shape = if env.body_level {
        body_paragraph_shape(doc, node, avoid, is_float, env.body_block_flow, &lines)
    } else {
        line_split_candidate(doc, parent, node)
            .is_some_and(|s| super::may_start_in_place_shape(doc, parent, node, &s, avoid, fresh))
    };
    shape.then_some(first.1 - first.0)
}

/// What `siblings[index]` has to take along when it moves to the next page, or `None` when it keeps nothing with it.
///
/// The head must be unbreakable ([`unbreakable_extent`]) and the break in front of it must be allowed (else it is a member
/// of a chain that started earlier, which decided for all of it). The chain is the run of next in-flow siblings joined by
/// an avoided break ([`avoid_page_break_between`]); a float, a forced break, a side-by-side sibling or the end of the
/// parent ends it, and so does a member that is not unbreakable (its first fragment is the last thing that has to stay).
/// The extent is `top(last) - top(head) + first_fragment_extent(last)`.
///
/// When the extent would exceed a fresh page no page break keeps the chain together and css-break-3 §4.4 relaxes the
/// rules, Rule 3 first: when the member that overflows is a multi-line paragraph without an allowed break, the extent ends
/// at its first line and `relax_rule3` names it; otherwise the chain is cut at the last member that fits (Rules 1 and 2
/// give way at the latest link). A cut that leaves only the head keeps nothing.
pub(super) fn keep_with_next_extent(
    doc: &BaseDocument,
    parent: &blitz_dom::Node,
    siblings: &[usize],
    index: usize,
    env: KeepEnv<'_>,
    fresh: f32,
) -> Option<KeepExtent> {
    let head_id = *siblings.get(index)?;
    let head = doc.get_node(head_id)?;
    // Cheap exit: nothing is avoided after the head.
    let next_index = neighbour(doc, siblings, index, true, env.styles)?;
    if !avoid_page_break_between(env.styles, env.names, head_id, siblings[next_index]) {
        return None;
    }
    // The break in front of the head must be allowed: otherwise the head belongs to a chain that began above it.
    if let Some(prev_index) = neighbour(doc, siblings, index, false, env.styles)
        && avoid_page_break_between(env.styles, env.names, siblings[prev_index], head_id)
    {
        return None;
    }
    unbreakable_extent(doc, head_id, env.styles, fresh)?;
    let head_top = head.final_layout.location.y;
    let mut cur_id = head_id;
    let mut cur_index = index;
    let mut size: Option<f32> = None;
    let mut relax: Option<usize> = None;
    let mut bounded = 0usize;
    while let Some(next_index) = neighbour(doc, siblings, cur_index, true, env.styles) {
        bounded += 1;
        if bounded > crate::MAX_DOM_DEPTH * 64 {
            break;
        }
        let next_id = siblings[next_index];
        let (Some(cur), Some(next)) = (doc.get_node(cur_id), doc.get_node(next_id)) else {
            break;
        };
        if crate::blitz_adapter::node_is_floating(cur)
            || crate::blitz_adapter::node_is_floating(next)
        {
            break;
        }
        let next_top = next.final_layout.location.y;
        // Level with the box before it: laid out beside it, not below.
        if next_top < cur.final_layout.location.y + height_of(cur) - 0.5 {
            break;
        }
        if !avoid_page_break_between(env.styles, env.names, cur_id, next_id) {
            break;
        }
        let delta = next_top - head_top;
        let candidate = delta + first_fragment_extent(doc, parent, next_id, env, fresh);
        if candidate > fresh {
            if let Some(line) = relaxable_first_line(doc, parent, next_id, env, fresh)
                && delta + line <= fresh
            {
                size = Some(delta + line);
                relax = Some(next_id);
            }
            break;
        }
        size = Some(candidate);
        if unbreakable_extent(doc, next_id, env.styles, fresh).is_none() {
            break;
        }
        cur_id = next_id;
        cur_index = next_index;
    }
    size.map(|size| KeepExtent {
        size,
        relax_rule3: relax,
    })
}
