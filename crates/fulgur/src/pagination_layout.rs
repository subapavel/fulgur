//! Taffy-hooked block-level paginator (fulgur-4cbc).
//!
//! Sibling of [`crate::multicol_layout`]. The multicol module proves the
//! `LayoutPartialTree` wrapper pattern works for routing one CSS feature
//! through fulgur-owned layout while leaving the rest to `BaseDocument`;
//! this module applies the same idiom to page fragmentation.
//!
//! # Status: production-wired, observational consumer
//!
//! [`run_pass_with_break_styles`] is invoked once per render from
//! `engine.rs` after `multicol_layout::run_pass`. The production path
//! skips `taffy::compute_root_layout` and calls
//! [`fragment_pagination_root`] directly: it walks the body's direct
//! block children's existing `final_layout` — descending into Parley
//! line metrics for inline roots — and records the would-be page
//! geometry in a `PaginationGeometryTable`. Re-driving Taffy on body
//! re-stores every descendant's layout fields and introduces sub-pixel
//! floating-point drift that breaks `examples_determinism`'s byte-wise
//! PDF comparison; see [`PaginationLayoutTree::drive_taffy_root_layout`]
//! for the full root cause.
//!
//! The wrapper's `LayoutPartialTree` / `RoundTree` / `CacheTree` /
//! `TraversePartialTree` impls (which dispatch body's layout into
//! [`compute_pagination_layout`] via `taffy::compute_root_layout`) are
//! kept compile-time live as scaffolding for a future per-strip
//! constrained variant; the `taffy_driven_dispatch_matches_direct_walk`
//! test exercises them at runtime and asserts geometry parity with the
//! production direct walk.
//!
//! Today the engine drops the returned table (`let _pagination_geometry
//! = …`). Follow-up work will capture the table on `ConvertContext`
//! and wire downstream consumers (counter / string-set replacement,
//! per-page repetition redesign, …).
//!
//! # Coverage
//!
//! The wrapper is currently exercised against the body subtree only.
//! Anything nested inside body's direct children continues to use
//! Blitz's normal layout dispatch, and the fragmenter post-walks
//! `final_layout` rather than re-issuing per-strip
//! `compute_child_layout` calls. The fulgur-ik6o probe established
//! that constraining `available_space.height` does not change Taffy's
//! block-layout output — see
//! `docs/plans/2026-04-28-pagination-layout-spike.md`.
//!
//! # Features wired today
//!
//! - Block-level fragmentation against `page_height_px`
//!   ([`fragment_pagination_root`]).
//! - Inline-aware split at Parley line boundaries
//!   ([`fragment_inline_root`], reads `inline_layout_data` populated by
//!   `resolve()`).
//! - `break-before` / `break-after` / `break-inside: avoid` from the
//!   shared [`crate::column_css::ColumnStyleTable`] side-table.
//!   `break-inside: avoid` holds while the box fits a page and is relaxed
//!   past that ([`avoid_inside_holds`]). A kept-whole avoid box in block
//!   flow (a body child, or a child of a block) moves whole to the next
//!   page when it does not fit and content precedes it; one taller than a
//!   page starts the next page when content precedes it and then breaks
//!   like an `auto` one ([`avoid_keeps_whole`], [`starts_with_relaxed_avoid`],
//!   F83b).
//! - Absolutely positioned boxes whose text crosses a page ([`fragment_abspos_flows`],
//!   `abspos_flow.rs`, F30g): after the body-direct pass, a box that has no geometry or
//!   whose lines the full-height windows of that pass would cut is fragmented from its
//!   own start with the fragmenters above.
//!
//! # Production extension points
//!
//! [`collect_string_set_states`] and [`implied_page_count`] are `pub`
//! for use by `render_v2` and friends. [`append_position_fixed_fragments`]
//! is wired into `engine.rs` so v2's geometry-driven dispatch can
//! repeat `position: fixed` elements on every page (`is_repeat=true`
//! on the resulting `PaginationGeometry`).

mod abspos_flow;
pub use abspos_flow::fragment_abspos_flows;
mod subtree_facts;
use subtree_facts::SubtreeFacts;

use crate::units::F32Units;
use blitz_dom::BaseDocument;
use std::collections::{BTreeMap, BTreeSet};
use taffy::{
    AvailableSpace, CacheTree, LayoutPartialTree, NodeId, RoundTree, Size, TraversePartialTree,
    TraverseTree,
};

/// One placement slot recorded per (source node × page).
///
/// `x`, `y`, `width`, `height` are type-enforced CSS pixels
/// ([`crate::units::Px`]) — Taffy's native unit — and `y` is measured from
/// the page's content-box top. The convert / draw layer is responsible for
/// px→pt conversion ([`crate::units::Px::in_pt`]) before reaching Krilla.
#[derive(Clone, Debug, PartialEq)]
pub struct Fragment {
    pub page_index: u32,
    pub x: crate::units::Px,
    pub y: crate::units::Px,
    pub width: crate::units::Px,
    pub height: crate::units::Px,
}

/// F78: the lines `[first, end)` of an inline root that one [`Fragment`] holds.
///
/// A fragmenter that splits a paragraph between line boxes decides the line
/// ranges once, here, and the renderer paints exactly these lines: deriving the
/// range a second time from float heights drifts (Parley advances `y` by the
/// unrounded `line-height` while a quantised line box is taller or shorter, so
/// consecutive boxes overlap or leave gaps) and loses or repeats a line.
///
/// `offset` is the distance from the top of the paragraph's first line box to
/// the top of line `first`, in the fragmenter's own coordinate: the renderer
/// moves the lines of the fragment up by it, which puts line `first` where line 0
/// sits on the first page. It is the base
/// [`record_split_inline_root_descendants`] uses for inline boxes, so text and
/// images agree on every page.
///
/// F30f: the *carrier* of an `inline-block` that spans pages (the box that holds the
/// block in its one line) has slices of its own kind: `0..1` on the fragment where
/// the block starts, `0..0` before it and `1..1` after it ([`carrier_slices_ok`],
/// [`record_carrier_slices`]). The line is painted once, on that page, and the later
/// pages of the block are painted as continuations.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct LineSlice {
    pub first: u32,
    pub end: u32,
    pub offset: crate::units::Px,
}

/// Whether `slices` are contiguous, start at line 0 and cover exactly `0..n`.
fn line_slices_tile(slices: &[LineSlice], n: usize) -> bool {
    let mut next = 0u32;
    for s in slices {
        if s.first != next || s.end <= s.first {
            return false;
        }
        next = s.end;
    }
    next as usize == n
}

/// F30d: what one column of a multicol container holds on one page.
///
/// `y` is the distance from the top of the column on that page (the top of the container's content box, below
/// its top padding and border on the first page) to the top of the piece.
#[derive(Clone, Debug, PartialEq)]
pub enum McPiece {
    /// A child box placed whole: its subtree is recorded at the column's x and this y.
    Block { node: usize, y: crate::units::Px },
    /// Lines `[first, end)` of the Parley layout of `source`: the renderer paints exactly these lines, whatever
    /// their heights add up to (the F78 rule).
    Lines {
        source: usize,
        first: u32,
        end: u32,
        y: crate::units::Px,
    },
    /// F174: the row bands `[first, end)` of the table `table` ([`McTable::bands`]): the renderer paints one table
    /// fragment for them at the column's x and this y, and each cell of the bands below it. `head` is set when the
    /// piece opens a column the table continues in and the table's repeated header ([`McHead`]) stands at its
    /// top: the bands then start `McHead::height` below `y`.
    Rows {
        table: usize,
        first: u32,
        end: u32,
        y: crate::units::Px,
        head: bool,
    },
}

/// F30d: one row of columns of a fragmented multicol container, on one page: the pieces of every column
/// (`columns.len()` is the used column count; a column without content is empty) and the height of the
/// tallest, the block-size of the content on that page.
///
/// F161: a container with `column-span: all` children has several rows, one per column group and page, and a
/// page can hold more than one (the rows above and below a spanner). The rows of a container are therefore
/// not parallel to its fragments (those are one per page).
#[derive(Clone, Debug, Default, PartialEq)]
pub struct McPage {
    pub height: crate::units::Px,
    pub columns: Vec<Vec<McPiece>>,
    /// Index of the column group (`MulticolGeometry::groups`) this row belongs to.
    pub group: u32,
    /// The distance from the top of the container's fragment on this page to the top of the columns of this
    /// row: its top padding and border on the first page (above the first row), the heights of the rows and
    /// spanners above it on the page.
    pub y: crate::units::Px,
}

/// F174: one run of cells of a table whose rows are joined by `rowspan`: the unit a table is cut at (css-break-3
/// §4.1: the boundary between two rows is a class A break point; a row is not cut, §4.4).
///
/// `cells` are indices into the table's `layout_children` (its cells, in visual order). `top` is where the band
/// starts in the table, from `final_layout` (the first band starts at 0: whatever is above its cells, the table's
/// top border and padding and the spacing, belongs to it), and `height` runs to the next band's `top` (the last to
/// the table's height), so the bands of a table add up to its height.
#[derive(Clone, Debug, PartialEq)]
struct RowBand {
    cells: std::ops::Range<u32>,
    top: f32,
    height: f32,
}

/// F174: the header rows a table repeats at the top of every column piece but its first (F12's policy, see
/// [`repeating_table_header`]).
#[derive(Clone, Debug, PartialEq)]
pub(crate) struct McHead {
    /// The `thead` node: its fragments are the source and the repeats.
    group: usize,
    /// The first `bands` bands of the table are the header's.
    bands: u32,
    /// The block-size reserved for the repeat at the top of a continuation column (F12's band: from the table's
    /// top edge to the bottom of the lowest header cell).
    height: f32,
}

/// F174: a table the multicol fragmenter cuts between rows ([`mc_table`]).
#[derive(Clone, Debug, PartialEq)]
pub(crate) struct McTable {
    bands: Vec<RowBand>,
    head: Option<McHead>,
}

impl McTable {
    /// The block-size of the bands `[first, end)`.
    fn height_of(&self, first: usize, end: usize) -> f32 {
        self.bands[first..end].iter().map(|b| b.height).sum()
    }

    /// The bands of the first unit of the table: its header and the first body band (a header is never left
    /// alone at the bottom of a column), or the first band.
    fn first_unit_end(&self) -> usize {
        self.head
            .as_ref()
            .map_or(1, |h| h.bands as usize + 1)
            .min(self.bands.len())
    }
}

/// An indivisible unit of a multicol flow: a box, one line of a paragraph, or the bands `[first, end)` of a table
/// (F174: one band, except the first unit of a table that repeats its header, which holds the header bands and the
/// first body band).
enum McAtom {
    Block {
        node: usize,
        h: f32,
    },
    Line {
        source: usize,
        index: u32,
        h: f32,
    },
    Row {
        table: usize,
        first: u32,
        end: u32,
        h: f32,
    },
}

impl McAtom {
    fn height(&self) -> f32 {
        match self {
            Self::Block { h, .. } | Self::Line { h, .. } | Self::Row { h, .. } => *h,
        }
    }
}

/// One page of columns filled greedily by [`fill_multicol_columns`].
struct McFill {
    columns: Vec<Vec<McPiece>>,
    height: f32,
    /// Index of the first atom that did not fit.
    next: usize,
}

/// Slack for the sum of line heights against a column budget (accumulated f32 error).
const MC_EPS: f32 = 1e-3;

/// Fill `n` columns of `budget` block-size from `atoms[start..]` in order: an atom goes below the previous
/// one in the same column unless it does not fit, then it opens the next column; the first atom of a column
/// goes in even when it is taller than the column (a box or line taller than the column overflows it rather
/// than loop for ever) when the columns are those of a fresh page (`fresh_page`) or it is the first atom of the
/// page. On a page with less room (below a spanner or other content) such an atom ends the page instead: it
/// may fit the next one (html-to-pdf F161: a `<div>` shorter than a page but taller than the room under a
/// spanner was put in the second column and ran past the page bottom). Stops when the columns are full or the
/// atoms are used up. A line is never split (css-break-3 §4.1, CSS 2.1 §13.3.3).
///
/// F174: a row of a table that opens a column the table continues in (the atom before it is a row of the same
/// table) starts below the table's repeated header, `McHead::height` down, when the header and the row fit the
/// column together; a row that fits the column only without the header gets the column without it (F12's rule).
/// "Fit the column" is `head_limit`, the block-size of a column of the page, not a trial `budget` that balancing
/// tries: a row gets its header whatever the balanced columns turn out to be (Chrome repeats the header over the
/// last two rows of a table in both columns), and the columns grow to hold it.
fn fill_multicol_columns(
    atoms: &[McAtom],
    tables: &BTreeMap<usize, McTable>,
    start: usize,
    n: usize,
    budget: f32,
    head_limit: f32,
    fresh_page: bool,
) -> McFill {
    let mut columns: Vec<Vec<McPiece>> = vec![Vec::new(); n];
    let mut col = 0usize;
    let mut used = 0.0f32;
    let mut height = 0.0f32;
    let mut at = start;
    while at < atoms.len() {
        let h = atoms[at].height();
        if used > 0.0 && used + h > budget + MC_EPS {
            if col + 1 >= n {
                break;
            }
            col += 1;
            used = 0.0;
        }
        if !fresh_page && at > start && h > budget + MC_EPS {
            break;
        }
        let y = used.as_px();
        let mut band = 0.0f32;
        if let McAtom::Row { table, .. } = &atoms[at]
            && columns[col].is_empty()
            && at > 0
            && matches!(&atoms[at - 1], McAtom::Row { table: t, .. } if t == table)
            && let Some(head) = tables.get(table).and_then(|t| t.head.as_ref())
            && head.height + h <= head_limit + MC_EPS
        {
            band = head.height;
        }
        match &atoms[at] {
            McAtom::Block { node, .. } => columns[col].push(McPiece::Block { node: *node, y }),
            McAtom::Line { source, index, .. } => match columns[col].last_mut() {
                Some(McPiece::Lines {
                    source: s, end: e, ..
                }) if *s == *source && *e == *index => *e += 1,
                _ => columns[col].push(McPiece::Lines {
                    source: *source,
                    first: *index,
                    end: *index + 1,
                    y,
                }),
            },
            McAtom::Row {
                table, first, end, ..
            } => match columns[col].last_mut() {
                Some(McPiece::Rows {
                    table: t, end: e, ..
                }) if *t == *table && *e == *first => *e = *end,
                _ => columns[col].push(McPiece::Rows {
                    table: *table,
                    first: *first,
                    end: *end,
                    y,
                    head: band > 0.0,
                }),
            },
        }
        used += band + h;
        height = height.max(used);
        at += 1;
    }
    McFill {
        columns,
        height,
        next: at,
    }
}

/// F30d: distribute the flow of a multicol container over pages (css-multicol-1 §8.2, css-break-3 §2: a
/// column box that does not fit the page continues in the columns of the next one).
///
/// `first_room` is the block-size of the columns on the first page, `page_room` on every later page.
/// `last_inset` (the container's bottom padding and border) is kept free on the page that holds the end of the
/// flow. Every page but the last is filled column after column to its room; the last one is filled the same
/// way for `column-fill: auto` and balanced (the smallest budget in which the rest fits `n` columns) for
/// `balance`, the only fragment that is balanced in a fragmented context (css-multicol-1 §7.1). Lines are never
/// split, every column gets at least one unit, so a box taller than a page is placed alone and overflows.
///
/// F174: a box of `tables` (keyed by node) is not one atom but one per row band, cut between rows; its header
/// repeats at the top of the columns it continues in (see [`fill_multicol_columns`]). The header bands and the
/// first body band are one atom: a header is never left alone at the bottom of a column.
pub(crate) fn plan_multicol_pages(
    flow: &[crate::multicol_layout::FlowItem],
    n: u32,
    first_room: f32,
    page_room: f32,
    fill: crate::column_css::ColumnFill,
    last_inset: f32,
    tables: &BTreeMap<usize, McTable>,
) -> Vec<McPage> {
    use crate::multicol_layout::FlowItem;
    let n = n.max(1) as usize;
    let mut atoms: Vec<McAtom> = Vec::new();
    for item in flow {
        match item {
            FlowItem::Block { node, height } => match tables.get(node) {
                Some(table) => {
                    let lead = table.first_unit_end();
                    atoms.push(McAtom::Row {
                        table: *node,
                        first: 0,
                        end: lead as u32,
                        h: table.height_of(0, lead).max(0.0),
                    });
                    for (b, band) in table.bands.iter().enumerate().skip(lead) {
                        atoms.push(McAtom::Row {
                            table: *node,
                            first: b as u32,
                            end: b as u32 + 1,
                            h: band.height.max(0.0),
                        });
                    }
                }
                None => atoms.push(McAtom::Block {
                    node: *node,
                    h: height.to_f32().max(0.0),
                }),
            },
            FlowItem::Lines { source, advances } => {
                for (index, h) in advances.iter().enumerate() {
                    atoms.push(McAtom::Line {
                        source: *source,
                        index: index as u32,
                        h: h.to_f32().max(0.0),
                    });
                }
            }
        }
    }
    let mut pages: Vec<McPage> = Vec::new();
    let mut at = 0usize;
    while at < atoms.len() {
        let fresh_page = !pages.is_empty() || first_room + MC_EPS >= page_room;
        if pages.len() >= crate::MAX_PAGES as usize {
            log::warn!(
                "pagination: a multi-column container needs more than {} pages; the rest of its content is dropped",
                crate::MAX_PAGES
            );
            break;
        }
        let room = if pages.is_empty() {
            first_room
        } else {
            page_room
        }
        .max(0.0);
        let room_for_end = (room - last_inset).max(0.0);
        let trial = fill_multicol_columns(
            &atoms,
            tables,
            at,
            n,
            room_for_end,
            room_for_end,
            fresh_page,
        );
        let filled = if trial.next >= atoms.len() {
            // The rest fits on this page: it is the last one.
            match fill {
                crate::column_css::ColumnFill::Auto => trial,
                crate::column_css::ColumnFill::Balance => {
                    balance_multicol_columns(&atoms, tables, at, n, room_for_end, fresh_page)
                }
            }
        } else {
            let full = fill_multicol_columns(&atoms, tables, at, n, room, room, fresh_page);
            if full.next >= atoms.len() && fill == crate::column_css::ColumnFill::Balance {
                // The rest fits only when the bottom padding is allowed to hang below the page: balance it
                // within the room anyway.
                balance_multicol_columns(&atoms, tables, at, n, room, fresh_page)
            } else {
                full
            }
        };
        at = filled.next;
        pages.push(McPage {
            height: filled.height.as_px(),
            columns: filled.columns,
            ..McPage::default()
        });
    }
    pages
}

/// The fill of `atoms[start..]` in `n` columns with the smallest budget (at most `room`) in which all of
/// them fit. Next-fit never needs more columns for a larger budget, so a bisection finds it.
fn balance_multicol_columns(
    atoms: &[McAtom],
    tables: &BTreeMap<usize, McTable>,
    start: usize,
    n: usize,
    room: f32,
    fresh_page: bool,
) -> McFill {
    let rest = &atoms[start..];
    let total: f32 = rest.iter().map(McAtom::height).sum();
    let tallest = rest.iter().map(McAtom::height).fold(0.0f32, f32::max);
    let fits = |budget: f32| {
        fill_multicol_columns(atoms, tables, start, n, budget, room, fresh_page).next >= atoms.len()
    };
    let mut lo = tallest.max(total / n as f32);
    let mut hi = room.max(lo);
    if fits(lo) {
        return fill_multicol_columns(atoms, tables, start, n, lo, room, fresh_page);
    }
    // `lo` does not fit, `hi` is the room the rest was already known to fit in (or more, when `room` is below
    // the tallest atom: then nothing smaller helps either).
    for _ in 0..32 {
        let mid = (lo + hi) / 2.0;
        if fits(mid) {
            hi = mid;
        } else {
            lo = mid;
        }
    }
    fill_multicol_columns(atoms, tables, start, n, hi, room, fresh_page)
}

/// F30d: a multi-column container the fragmenter can cut over pages, and what it needs to know about it.
struct McContainer<'a> {
    /// The container's column groups, in source order (F161: more than one when `column-span: all` children
    /// separate them).
    groups: &'a [crate::multicol_layout::ColumnGroupGeometry],
    /// The segments in source order: a group, or a spanner between two groups.
    segments: &'a [crate::multicol_layout::SegmentGeom],
    fill: crate::column_css::ColumnFill,
    /// The container's top padding and border, px: it sits above the columns of the first page only.
    top_inset: f32,
    /// The container's bottom padding and border, px.
    bottom_inset: f32,
    /// The block-size of the first segment's first unit (see [`McContainer::first_unit`]), px.
    first_unit: f32,
    /// F174: the tables of the container's flows (boxes of the groups) the fragmenter cuts between rows, by node.
    tables: BTreeMap<usize, McTable>,
}

impl McContainer<'_> {
    /// The container's top padding and border, px: it sits above the columns of the first page only.
    fn top_inset(&self) -> f32 {
        self.top_inset
    }

    /// Block-size of the first thing in the flow (a box, a spanner, or the first line of a paragraph), px.
    fn first_unit(&self) -> f32 {
        self.first_unit
    }
}

/// F30d: the block-size of the first unit of a flow (a box, or the first line of a paragraph), px.
///
/// F174: of a table in `tables` the first unit is its header and first body row, not the whole table (F28 would
/// otherwise move a container that starts with a long table to the next page).
fn flow_first_unit(
    flow: &[crate::multicol_layout::FlowItem],
    tables: &BTreeMap<usize, McTable>,
) -> f32 {
    use crate::multicol_layout::FlowItem;
    flow.iter()
        .find_map(|item| match item {
            FlowItem::Block { node, height } => Some(match tables.get(node) {
                Some(table) => table.height_of(0, table.first_unit_end()),
                None => height.to_f32(),
            }),
            FlowItem::Lines { advances, .. } => advances.first().map(|h| h.to_f32()),
        })
        .unwrap_or(0.0)
}

/// F30d: the container `node_id`, when it is a multi-column container with a flow the fragmenter can place:
/// the multicol pass recorded its column groups, and every group has something in it.
///
/// F161: `column-span: all` children are segments between the groups; a container with spanners is placed
/// like any other. A group whose flow is empty (a text node among its children, say) cannot be described, and
/// the container keeps the old paths. So does a container with spanners whose flow holds a box taller than a
/// page (`fresh`), or with a spanner taller than a quarter of a page: the fragmenter places a box whole (F30d-b)
/// and moves a spanner that does not fit the room to the next page whole, while the block recursion of the old
/// path, which every container with a spanner took before F161, cuts them (`<div><p>…a page and a half…</p></div>`
/// under a spanner lost its last lines; a spanner of a page left most of the page above it blank, one taller than
/// a page was not cut; F173). Spanners are headings in practice.
fn multicol_container<'a>(
    doc: &BaseDocument,
    multicol: Option<&'a crate::multicol_layout::MulticolGeometryTable>,
    column_styles: Option<&crate::column_css::ColumnStyleTable>,
    node_id: usize,
    fresh: f32,
) -> Option<McContainer<'a>> {
    use crate::multicol_layout::{FlowItem, SegmentGeom};
    let node = doc.get_node(node_id)?;
    if !crate::blitz_adapter::is_multicol_container(node) {
        return None;
    }
    let geometry = multicol?.get(&node_id)?;
    if geometry.groups.is_empty() || geometry.segments.is_empty() {
        return None;
    }
    for group in &geometry.groups {
        let has_content = group.flow.iter().any(|item| match item {
            FlowItem::Block { .. } => true,
            FlowItem::Lines { advances, .. } => !advances.is_empty(),
        });
        if !has_content || group.n == 0 {
            return None;
        }
    }
    // F174: the tables among the boxes of the flows, which are cut between rows.
    let mut tables: BTreeMap<usize, McTable> = BTreeMap::new();
    for group in &geometry.groups {
        for item in &group.flow {
            if let FlowItem::Block { node, .. } = item
                && let Some(table) = mc_table(doc, *node, column_styles, fresh)
            {
                tables.insert(*node, table);
            }
        }
    }
    let has_spanner = geometry
        .segments
        .iter()
        .any(|s| matches!(s, SegmentGeom::SpanAll { .. }));
    if has_spanner
        && geometry.segments.iter().any(|s| match s {
            SegmentGeom::SpanAll { node } => doc
                .get_node(*node)
                .is_none_or(|n| n.final_layout.size.height > fresh * 0.25),
            SegmentGeom::Group(_) => false,
        })
    {
        return None;
    }
    if has_spanner
        && geometry.groups.iter().any(|g| {
            g.flow.iter().any(
            |item| matches!(item, FlowItem::Block { node, height } if height.to_f32() > fresh + 1.0
                // F174: a table is cut between its rows, which each fit a page.
                && !tables.get(node).is_some_and(|t| t.bands.iter().all(|b| b.height <= fresh + 1.0))),
        )
        })
    {
        return None;
    }
    let layout = node.final_layout;
    let (top_inset, first_unit) = match geometry.segments.first()? {
        // The group sits below the container's top padding and border.
        SegmentGeom::Group(g) => {
            let group = geometry.groups.get(*g)?;
            (
                group.y_offset.to_f32(),
                flow_first_unit(&group.flow, &BTreeMap::new()),
            )
        }
        SegmentGeom::SpanAll { node: span } => (
            layout.border.top + layout.padding.top,
            doc.get_node(*span)?.final_layout.size.height,
        ),
    };
    let fill = column_styles
        .and_then(|t| t.get(&node_id))
        .and_then(|p| p.fill)
        .unwrap_or_default();
    Some(McContainer {
        groups: &geometry.groups,
        segments: &geometry.segments,
        fill,
        top_inset,
        bottom_inset: layout.border.bottom + layout.padding.bottom,
        first_unit,
        tables,
    })
}

/// F174: the row bands of a table from the `(y, height)` of its cells in visual order, and the table's `height`.
/// A row starts a band unless a cell of the band above reaches into it (`rowspan`: a cell spans the rows it
/// covers, so no cut between them may separate it from its row). `None` when the cells are not in non-decreasing
/// `y`, a position is not finite, or there are fewer than two bands.
fn row_bands(cells: &[(f32, f32)], height: f32) -> Option<Vec<RowBand>> {
    const EPS: f32 = 0.01;
    let mut starts: Vec<(usize, f32)> = Vec::new();
    let mut band_bottom = f32::NEG_INFINITY;
    let mut prev_y = f32::NEG_INFINITY;
    for (i, &(y, h)) in cells.iter().enumerate() {
        if !y.is_finite() || !h.is_finite() || y < prev_y - EPS {
            return None;
        }
        if i == 0 || (y > prev_y + EPS && y >= band_bottom - EPS) {
            starts.push((i, if i == 0 { 0.0 } else { y }));
            band_bottom = y + h;
        } else {
            band_bottom = band_bottom.max(y + h);
        }
        prev_y = prev_y.max(y);
    }
    if starts.len() < 2 || cells.len() > u32::MAX as usize {
        return None;
    }
    Some(
        starts
            .iter()
            .enumerate()
            .map(|(b, &(first, top))| {
                let (end, next_top) = starts
                    .get(b + 1)
                    .map_or((cells.len(), height), |&(i, t)| (i, t));
                RowBand {
                    cells: first as u32..end as u32,
                    top,
                    height: (next_top - top).max(0.0),
                }
            })
            .collect(),
    )
}

/// F174: whether `node_id`, a child box of a multi-column container, is a table the fragmenter cuts between rows,
/// and where. `None` (the box stays one atom, today's behaviour) unless all hold:
///
/// - its `display` is a table (`inside` is `table`) and it has at least two row bands ([`RowBand`]);
/// - its cells come in non-decreasing `y`, as Blitz's visual order gives them (F36, F163c): a table the order of
///   which the bands cannot follow is not cut;
/// - neither it nor an ancestor below `<body>` clips its overflow, is transformed or translucent
///   ([`table_subtree_is_wrapped`]: those subtrees are painted through wrappers the pieces would not pass through);
/// - it is not `break-inside: avoid` while it fits a fresh page (`fresh`), as everywhere else
///   ([`avoid_inside_holds`]).
///
/// The header repeats for a `<table>` element only, by F12's rules ([`repeating_table_header`]: at most a quarter
/// of a page, a prefix of the cells) and only when the header's bands end where a body band starts.
fn mc_table(
    doc: &BaseDocument,
    node_id: usize,
    column_styles: Option<&crate::column_css::ColumnStyleTable>,
    fresh: f32,
) -> Option<McTable> {
    use ::style::values::specified::box_::DisplayInside;
    let node = doc.get_node(node_id)?;
    if node.primary_styles()?.clone_display().inside() != DisplayInside::Table {
        return None;
    }
    let height = node.final_layout.size.height;
    if !height.is_finite() || height <= 0.0 {
        return None;
    }
    if avoid_inside_holds(has_avoid(column_styles, node_id), height, fresh) {
        return None;
    }
    if table_subtree_is_wrapped(doc, node_id) {
        return None;
    }
    let cells = node.layout_children.borrow();
    let cells: &[usize] = cells.as_deref()?;
    let extents: Vec<(f32, f32)> = cells
        .iter()
        .map(|&cell| {
            let layout = doc.get_node(cell)?.final_layout;
            Some((layout.location.y, layout.size.height))
        })
        .collect::<Option<_>>()?;
    let bands = row_bands(&extents, height)?;
    let head = repeating_table_header(doc, node_id, fresh).and_then(|repeat| {
        let n = repeat.cells.len() as u32;
        let header_bands = bands.iter().position(|b| b.cells.start == n)?;
        (header_bands > 0).then_some(McHead {
            group: repeat.group_id,
            bands: header_bands as u32,
            height: repeat.height,
        })
    });
    Some(McTable { bands, head })
}

/// F30d: whether a multi-column container of block-size `height` is cut over pages when `room` is what is
/// left of the page below it: it is when it does not fit (the paths that keep it whole either move it to a
/// page of its own, which is what a container that fits the next page may still do, or cut it into strips
/// with the text on the first). `break-inside: avoid` is honoured as long as the container fits a page.
fn multicol_wants_split(height: f32, room: f32, fresh: f32, avoid_inside: bool) -> bool {
    height.is_finite() && height > room && (!avoid_inside || height > fresh)
}

/// F174: whether the fragmenter places the multi-column container `mc` of block-size `height` (`room` is what is
/// left of the page below it): when [`multicol_wants_split`] says so, and also when it holds a table in two columns or
/// more, which it cuts between rows and balances over the columns even where it fits the page: Blitz lays a table
/// out whole in one column, the first of them. A container `break-inside: avoid` that does not fit the room but fits
/// a page keeps moving whole, as before.
fn multicol_cuts(
    mc: &McContainer<'_>,
    height: f32,
    room: f32,
    fresh: f32,
    avoid_inside: bool,
) -> bool {
    multicol_wants_split(height, room, fresh, avoid_inside)
        || (!mc.tables.is_empty()
            && mc.groups.iter().any(|g| g.n >= 2)
            && !(avoid_inside && height > room))
}

/// F83: whether `break-inside: avoid` on an inline root whose lines span `height` (bottom of the last line box minus
/// top of the first) still holds, `fresh` being the room of a fresh page. It holds while the lines fit a page; past
/// that it is relaxed and the box breaks like an `auto` one
/// (css-break-3 §4.4: when the avoid rules leave no way to keep content from overflowing the fragmentainer they are
/// dropped before orphans and widows; CSS 2.1 §13.3.3 has the same order). One px of slack, as the strip slicer
/// has (`child_h > fresh + 1.0`), so a box it would not cut is not relaxed either.
fn avoid_inside_holds(avoid_inside: bool, height: f32, fresh: f32) -> bool {
    avoid_inside && height <= fresh + 1.0
}

/// F83b: the block-size the `break-inside: avoid` rules measure for `node`: the line extent of an inline root with
/// two or more lines (the F83 / F83c rule: a box taller than a page only through `height` or padding keeps its lines
/// on one page), the border-box height of any other box (css-break-3 §4.4 counts the whole box, its bottom padding
/// and border included).
fn avoid_extent(node: &blitz_dom::Node) -> f32 {
    let lines = collect_inline_line_metrics(node);
    if let (Some(first), Some(last)) = (lines.first(), lines.last())
        && lines.len() >= 2
    {
        return last.1 - first.0;
    }
    if node.final_layout.size.height.is_finite() {
        node.final_layout.size.height.max(0.0)
    } else {
        0.0
    }
}

/// F83b: whether an avoid box of block-size `extent` is kept whole where it is placed: `avoid` holds
/// ([`avoid_inside_holds`], it fits a fresh page of `fresh`) and the box either fits the `room` left below its
/// placement or can move to the next page (`content_above`: in-flow content precedes it on this one, so the class A
/// break point in front of it exists and is perfect, css-break-3 §4.5). A box that does not fit, with nothing
/// above it, is entered as before: Chrome does not break at the start of a fragmentainer either, and a page of
/// nothing is not an improvement. `tail` is what the boxes around it need below it (their bottom padding and border,
/// only when it is their last child): it has to fit the room too.
fn avoid_keeps_whole(
    avoid_inside: bool,
    extent: f32,
    room: f32,
    fresh: f32,
    content_above: bool,
    tail: f32,
) -> bool {
    avoid_inside_holds(avoid_inside, extent, fresh)
        && (extent + tail <= room + 1.0 || content_above)
}

/// F83b: whether `node`'s children are laid out in block flow, i.e. the places where a child's `break-inside: avoid`
/// is honoured by moving the child whole: not a flex / grid container (its items are not class A break points, and
/// moving one would desynchronise the row state), not an atomic inline, not an orthogonal flow, not a table (its
/// rows and cells have their own handling, F83d), not a multicol container.
fn is_block_flow_container(doc: &BaseDocument, node: &blitz_dom::Node) -> bool {
    use ::style::values::specified::box_::DisplayInside;
    !crate::blitz_adapter::is_flex_or_grid_container_node(node)
        && !crate::blitz_adapter::is_atomic_inline_container_node(node)
        && !crate::blitz_adapter::is_multicol_container(node)
        && !node
            .parent
            .and_then(|gp| doc.get_node(gp))
            .is_some_and(|gp| crate::blitz_adapter::is_orthogonal_to_parent(gp, node))
        && !node
            .primary_styles()
            .is_some_and(|s| s.clone_display().inside() == DisplayInside::Table)
}

/// F83b: whether `node_id` or one of its ancestors has `break-inside: avoid`.
fn in_avoid_box(
    doc: &BaseDocument,
    node_id: usize,
    column_styles: Option<&crate::column_css::ColumnStyleTable>,
) -> bool {
    let mut id = Some(node_id);
    let mut hops = 0;
    while let Some(n) = id {
        if has_avoid(column_styles, n) {
            return true;
        }
        hops += 1;
        if hops > crate::MAX_DOM_DEPTH {
            return false;
        }
        id = doc.get_node(n).and_then(|node| node.parent);
    }
    false
}

/// F83b: whether `node_id` has `break-inside: avoid` in `column_styles`.
fn has_avoid(column_styles: Option<&crate::column_css::ColumnStyleTable>, node_id: usize) -> bool {
    column_styles
        .and_then(|t| t.get(&node_id))
        .is_some_and(|p| {
            matches!(
                p.break_inside,
                Some(crate::draw_primitives::BreakInside::Avoid)
            )
        })
}

/// F83b: block-size from the top border edge of `node_id` to the first forced break inside it (the top of a
/// descendant with `break-before: page`, the bottom of one with `break-after: page`), or the node's own height when
/// there is none. This is the piece of an avoid box that has to be kept together: a forced break wins over `avoid`
/// (css-break-3 §4.3), and what follows it starts a page of its own.
///
/// A forced break at the very start of the box (its first child has `break-before: page`) is a break before the box
/// in effect: it leaves nothing on the page it is reached on, so it is skipped and the piece is the one after it, up
/// to the next forced break (the validator's H2: counting 0 left the first line at the bottom of page 0). Walks the
/// DOM children like [`has_forced_break_below`].
fn avoid_piece_size(
    doc: &BaseDocument,
    node_id: usize,
    column_styles: Option<&crate::column_css::ColumnStyleTable>,
    depth: usize,
) -> f32 {
    let Some(node) = doc.get_node(node_id) else {
        return 0.0;
    };
    let own_h = if node.final_layout.size.height.is_finite() {
        node.final_layout.size.height.max(0.0)
    } else {
        0.0
    };
    let Some(table) = column_styles else {
        return own_h;
    };
    let mut offsets = Vec::new();
    forced_break_offsets(doc, node_id, table, 0.0, depth, &mut offsets);
    offsets
        .into_iter()
        .filter(|y| *y > 0.5)
        .fold(own_h, f32::min)
}

/// The block-size an avoid box is measured by for [`avoid_inside_holds`] / [`avoid_keeps_whole`]: its
/// [`avoid_extent`], or only the piece before its first forced break ([`avoid_piece_size`]) when that is shorter. A
/// box that is taller than a page only because of a forced break inside it is not relaxed (M1): the pieces are what
/// `avoid` keeps together.
fn avoid_measure(
    doc: &BaseDocument,
    node_id: usize,
    node: &blitz_dom::Node,
    column_styles: Option<&crate::column_css::ColumnStyleTable>,
) -> f32 {
    avoid_extent(node).min(avoid_piece_size(doc, node_id, column_styles, 0))
}

/// Offsets (from the top of the node `avoid_piece_size` started at; `base` is the top of `node_id` in that frame) of
/// every forced break below `node_id`, in no particular order.
fn forced_break_offsets(
    doc: &BaseDocument,
    node_id: usize,
    table: &crate::column_css::ColumnStyleTable,
    base: f32,
    depth: usize,
    out: &mut Vec<f32>,
) {
    if depth >= crate::MAX_DOM_DEPTH {
        return;
    }
    let Some(node) = doc.get_node(node_id) else {
        return;
    };
    for &child_id in &node.children {
        let Some(child) = doc.get_node(child_id) else {
            continue;
        };
        let top = base + child.final_layout.location.y;
        let props = table.get(&child_id);
        if props.is_some_and(|p| {
            matches!(
                p.break_before,
                Some(crate::draw_primitives::BreakBefore::Page)
            )
        }) {
            out.push(top);
        }
        forced_break_offsets(doc, child_id, table, top, depth + 1, out);
        if props.is_some_and(|p| {
            matches!(
                p.break_after,
                Some(crate::draw_primitives::BreakAfter::Page)
            )
        }) {
            out.push(top + child.final_layout.size.height.max(0.0));
        }
    }
}

/// F83b: whether `node_id`, or its first in-flow descendant reached with no gap (the walk of
/// [`leading_block_size`]), is an avoid box that does not hold because it is taller than a page. Such a box breaks
/// like an `auto` one, but the break before it is still preferred to one inside it (css-break-3 §4.5): when content
/// precedes it, it starts the next page.
fn starts_with_relaxed_avoid(
    doc: &BaseDocument,
    node_id: usize,
    column_styles: Option<&crate::column_css::ColumnStyleTable>,
    fresh: f32,
    depth: usize,
) -> bool {
    if depth >= crate::MAX_DOM_DEPTH {
        return false;
    }
    let Some(node) = doc.get_node(node_id) else {
        return false;
    };
    if crate::blitz_adapter::is_multicol_container(node) {
        return false;
    }
    // A forced break before the box (or before an ancestor on the way) is the break before it already: moving first
    // would make the forced break fire a second time on the fresh page, which has the box's parents' padding above
    // it, and leave a blank page (fuzz `Z1_198`).
    if column_styles
        .and_then(|t| t.get(&node_id))
        .is_some_and(|p| {
            matches!(
                p.break_before,
                Some(crate::draw_primitives::BreakBefore::Page)
            )
        })
    {
        return false;
    }
    if has_avoid(column_styles, node_id)
        && !avoid_inside_holds(
            true,
            avoid_measure(doc, node_id, node, column_styles),
            fresh,
        )
    {
        return true;
    }
    if !is_block_flow_container(doc, node) {
        return false;
    }
    let layout_children_borrow = node.layout_children.borrow();
    let walk_children: &[usize] = layout_children_borrow
        .as_deref()
        .filter(|v| !v.is_empty())
        .unwrap_or(&node.children);
    for &child_id in walk_children {
        let Some(child) = doc.get_node(child_id) else {
            continue;
        };
        if let Some(text) = child.text_data()
            && text.content.chars().all(char::is_whitespace)
        {
            continue;
        }
        {
            use ::style::properties::longhands::position::computed_value::T as Pos;
            let is_oof = child.primary_styles().is_some_and(|s| {
                matches!(s.get_box().clone_position(), Pos::Absolute | Pos::Fixed)
            });
            if is_oof {
                continue;
            }
        }
        let layout = child.final_layout;
        if !layout.size.height.is_finite() || layout.size.height <= 0.0 {
            continue;
        }
        // A gap between the content edge and the first child is a class C break point: the box is entered, not
        // moved. Border and padding are not a gap.
        let content_top = node.final_layout.border.top + node.final_layout.padding.top;
        if layout.location.y - content_top > 0.5 {
            return false;
        }
        return starts_with_relaxed_avoid(doc, child_id, column_styles, fresh, depth + 1);
    }
    false
}

/// What [`fragment_multicol_container`] needs to cut a `column-span: all` child that is taller than a page: the
/// tables [`fragment_block_subtree`] reads.
#[derive(Clone, Copy)]
struct McEnv<'a> {
    column_styles: Option<&'a crate::column_css::ColumnStyleTable>,
    used_page_names: Option<&'a crate::blitz_adapter::UsedPageNameTable>,
    facts: Option<&'a SubtreeFacts>,
    multicol: Option<&'a crate::multicol_layout::MulticolGeometryTable>,
}

/// The container's fragment on one page while [`fragment_multicol_container`] walks the segments: it starts at
/// `top` (page-local y) and holds `used` px of content so far (the top padding and border included on the first
/// page).
struct McFragment {
    page: u32,
    top: f32,
    used: f32,
    /// Whether a row or a spanner has been placed on it (the top padding and border are not content).
    has_content: bool,
}

impl McFragment {
    /// Page-local y of the next row.
    fn cursor(&self) -> f32 {
        self.top + self.used
    }
}

/// F30d: cut the multi-column container `container_id` over pages from `(page_in, cursor_in)` and record
/// what each page holds.
///
/// The flow of every column group the multicol pass recorded is distributed by [`plan_multicol_pages`]; this
/// records the result: one fragment of the container per page (the first below `cursor_in` with the
/// container's top padding and border, the last with its bottom ones), [`PaginationGeometry::multicol_pages`] on
/// the container (one entry per row of columns), a fragment (and its subtree) of every box at the column and
/// page y the plan gave it, and one fragment per page of every paragraph that has lines on it, which convert and
/// the renderer paint as given. Returns the page and the page-local y below the last fragment.
///
/// F161: the segments are walked in source order. A column group is planned from the room under the previous
/// segment; one followed by a `column-span: all` child is balanced on its last page (css-multicol-1 §6.1: the
/// columns above a spanner end together), the last group follows the container's `column-fill`. A spanner takes
/// the full width below the row above it, on the next page when it does not fit the room and fits a page, and
/// is cut like any other block when it is taller than a page.
///
/// The caller has already moved the container to the next page when its first unit does not fit below the
/// cursor (F28).
#[allow(clippy::too_many_arguments)]
fn fragment_multicol_container(
    geometry: &mut PaginationGeometryTable,
    doc: &BaseDocument,
    env: McEnv<'_>,
    c: &McContainer<'_>,
    container_id: usize,
    x_in_body: f32,
    width: f32,
    page_in: u32,
    cursor_in: f32,
    strips: PageStrips,
) -> (u32, f32) {
    use crate::multicol_layout::SegmentGeom;
    let mut frag = McFragment {
        page: page_in,
        top: cursor_in,
        used: c.top_inset(),
        has_content: false,
    };
    let close = |geometry: &mut PaginationGeometryTable, frag: &McFragment, height: f32| {
        geometry
            .entry(container_id)
            .or_default()
            .fragments
            .push(Fragment {
                page_index: frag.page,
                x: x_in_body.as_px(),
                y: frag.top.as_px(),
                width: width.as_px(),
                height: height.as_px(),
            });
    };
    // Start the next page: close the fragment on this one at its content and open a fresh one.
    let next_page = |geometry: &mut PaginationGeometryTable, frag: &mut McFragment| {
        close(geometry, frag, frag.used);
        *frag = McFragment {
            page: frag.page + 1,
            top: 0.0,
            used: 0.0,
            has_content: false,
        };
    };
    let last_segment = c.segments.len().saturating_sub(1);
    for (i, segment) in c.segments.iter().enumerate() {
        let last = i == last_segment;
        match *segment {
            SegmentGeom::Group(g) => {
                let Some(group) = c.groups.get(g) else {
                    continue;
                };
                // F28 for a group below a spanner: when its first unit does not fit the room under it, the
                // group starts on the next page.
                if i > 0 && frag.has_content {
                    let unit = flow_first_unit(&group.flow, &c.tables);
                    if unit > strips.bottom(frag.page) - frag.cursor() && unit <= strips.fresh() {
                        next_page(geometry, &mut frag);
                    }
                }
                let fill = if last {
                    c.fill
                } else {
                    crate::column_css::ColumnFill::Balance
                };
                let plan = plan_multicol_pages(
                    &group.flow,
                    group.n,
                    (strips.bottom(frag.page) - frag.cursor()).max(0.0),
                    strips.fresh(),
                    fill,
                    if last { c.bottom_inset } else { 0.0 },
                    &c.tables,
                );
                let plan_len = plan.len();
                for (k, mut page) in plan.into_iter().enumerate() {
                    if k > 0 {
                        next_page(geometry, &mut frag);
                    }
                    page.group = g as u32;
                    page.y = frag.used.as_px();
                    let row_top = frag.cursor();
                    record_multicol_row(
                        geometry,
                        doc,
                        group,
                        &page,
                        frag.page,
                        row_top,
                        x_in_body,
                        container_id,
                        strips,
                        &c.tables,
                    );
                    frag.used += page.height.to_f32();
                    frag.has_content = true;
                    if last && k + 1 == plan_len {
                        frag.used += c.bottom_inset;
                    }
                    geometry
                        .entry(container_id)
                        .or_default()
                        .multicol_pages
                        .push((frag.page, page));
                }
            }
            SegmentGeom::SpanAll { node: span } => {
                let Some(n) = doc.get_node(span) else {
                    continue;
                };
                let h = n.final_layout.size.height.max(0.0);
                let span_x = x_in_body + n.final_layout.location.x;
                let span_w = n.final_layout.size.width;
                let fits = |frag: &McFragment| frag.cursor() + h <= strips.bottom(frag.page) + 0.5;
                if !fits(&frag) && frag.has_content && h <= strips.fresh() + 1.0 {
                    next_page(geometry, &mut frag);
                }
                if fits(&frag) || h <= strips.fresh() + 1.0 {
                    // Whole on this page.
                    record_descendant(
                        geometry,
                        doc,
                        span,
                        frag.page,
                        frag.cursor() - n.final_layout.location.y,
                        x_in_body,
                        0,
                    );
                    frag.used += h;
                    frag.has_content = true;
                } else {
                    // Taller than a page: cut like a block of the page flow, from where it starts.
                    let parent = doc.get_node(container_id);
                    let split = parent.and_then(|parent| {
                        nested_line_split(
                            doc,
                            parent,
                            n,
                            frag.cursor(),
                            frag.cursor(),
                            strips,
                            frag.page,
                        )
                    });
                    let (end_page, end_y) = match split {
                        Some(split) => fragment_nested_inline_root(
                            geometry,
                            doc,
                            span,
                            span_x,
                            span_w,
                            frag.cursor(),
                            frag.page,
                            strips,
                            0.0,
                            0.0,
                            &split,
                            1,
                        ),
                        None => {
                            let (np, nc, _, _) = fragment_block_subtree(
                                geometry,
                                doc,
                                env.column_styles,
                                env.used_page_names,
                                env.facts,
                                env.multicol,
                                span,
                                span_w,
                                span_x,
                                frag.page,
                                frag.cursor(),
                                strips,
                                1,
                                0.0,
                                0.0,
                                frag.has_content,
                            );
                            (np, nc)
                        }
                    };
                    // The container's fragments on the pages the spanner covers: to the bottom of each, and
                    // to where the spanner ends on the last.
                    while frag.page < end_page {
                        let bottom = strips.bottom(frag.page).max(frag.cursor());
                        close(geometry, &frag, bottom - frag.top);
                        frag = McFragment {
                            page: frag.page + 1,
                            top: 0.0,
                            used: 0.0,
                            has_content: true,
                        };
                    }
                    frag.used = (end_y - frag.top).max(0.0);
                    frag.has_content = true;
                }
                if last {
                    frag.used += c.bottom_inset;
                }
            }
        }
    }
    close(geometry, &frag, frag.used);
    (frag.page, frag.cursor())
}

/// F30d: record what one row of columns of a cut multicol container holds on `page`, whose top is at the
/// page-local `row_top`: a fragment (and its subtree) for every box piece at its column x and y, and one
/// fragment per paragraph that has lines in the row.
#[allow(clippy::too_many_arguments)]
fn record_multicol_row(
    geometry: &mut PaginationGeometryTable,
    doc: &BaseDocument,
    group: &crate::multicol_layout::ColumnGroupGeometry,
    row: &McPage,
    page: u32,
    row_top: f32,
    x_in_body: f32,
    container_id: usize,
    strips: PageStrips,
    tables: &BTreeMap<usize, McTable>,
) {
    use crate::multicol_layout::FlowItem;
    let advances: BTreeMap<usize, &[crate::units::Px]> = group
        .flow
        .iter()
        .filter_map(|item| match item {
            FlowItem::Lines { source, advances } => Some((*source, advances.as_slice())),
            FlowItem::Block { .. } => None,
        })
        .collect();
    let col_w = group.col_w.to_f32();
    let stride = col_w + group.gap.to_f32();
    let mut paragraphs_on_page: BTreeSet<usize> = BTreeSet::new();
    for (col, pieces) in row.columns.iter().enumerate() {
        let col_x = x_in_body + group.x_offset.to_f32() + col as f32 * stride;
        for piece in pieces {
            match piece {
                McPiece::Block { node, y: piece_y } => {
                    let piece_y = row_top + piece_y.to_f32();
                    let Some(n) = doc.get_node(*node) else {
                        continue;
                    };
                    if n.final_layout.size.height > strips.fresh() + 1.0 {
                        log::warn!(
                            "pagination: a <{}> {}px tall inside a multi-column container is taller than a \
                             page; it is placed whole in its column and overflows the page",
                            n.element_data().map_or("box", |e| e.name.local.as_ref()),
                            n.final_layout.size.height
                        );
                    }
                    // `record_descendant` adds the box's own offset in its parent: take it off, the plan
                    // placed the box.
                    record_descendant(
                        geometry,
                        doc,
                        *node,
                        page,
                        piece_y - n.final_layout.location.y,
                        col_x - n.final_layout.location.x,
                        0,
                    );
                }
                McPiece::Lines {
                    source,
                    first,
                    end: end_line,
                    y: piece_y,
                } => {
                    // A container that is itself the paragraph is painted from its own fragments.
                    if *source == container_id || !paragraphs_on_page.insert(*source) {
                        continue;
                    }
                    let h: f32 = advances
                        .get(source)
                        .map(|a| {
                            a[*first as usize..*end_line as usize]
                                .iter()
                                .map(|h| h.to_f32())
                                .sum()
                        })
                        .unwrap_or(0.0);
                    geometry
                        .entry(*source)
                        .or_default()
                        .fragments
                        .push(Fragment {
                            page_index: page,
                            x: col_x.as_px(),
                            y: (row_top + piece_y.to_f32()).as_px(),
                            width: col_w.as_px(),
                            height: h.as_px(),
                        });
                }
                McPiece::Rows {
                    table,
                    first,
                    end: end_band,
                    y: piece_y,
                    head,
                } => {
                    let (Some(t), Some(n)) = (tables.get(table), doc.get_node(*table)) else {
                        continue;
                    };
                    let (first, end_band) = (*first as usize, *end_band as usize);
                    let cells = n.layout_children.borrow();
                    let Some(cells) = cells.as_deref() else {
                        continue;
                    };
                    if first >= end_band || end_band > t.bands.len() {
                        debug_assert!(
                            false,
                            "table piece {first}..{end_band} of {} bands",
                            t.bands.len()
                        );
                        continue;
                    }
                    let bands = &t.bands[first..end_band];
                    if bands.iter().any(|b| b.height > strips.fresh() + 1.0) {
                        log::warn!(
                            "pagination: a table row inside a multi-column container is taller than a page; \
                             it is placed alone in its column and overflows the page"
                        );
                    }
                    // The piece is one table fragment: a closed box at the column's x and the piece's y, as
                    // tall as the bands in it (and the repeated header above them).
                    let top = row_top + piece_y.to_f32();
                    let head_h = match (*head, &t.head) {
                        (true, Some(h)) => h.height,
                        _ => 0.0,
                    };
                    let table_geometry = geometry.entry(*table).or_default();
                    table_geometry.table_pieces = true;
                    table_geometry.fragments.push(Fragment {
                        page_index: page,
                        x: col_x.as_px(),
                        y: top.as_px(),
                        width: n.final_layout.size.width.as_px(),
                        height: (head_h + t.height_of(first, end_band)).as_px(),
                    });
                    // F12: the header stands at the top of a piece that continues the table, the first
                    // repeat records where the header was first placed (the table's first fragment).
                    if let (true, Some(h)) = (*head, &t.head) {
                        let source = table_geometry.fragments.first().map(|f| Fragment {
                            height: h.height.as_px(),
                            ..f.clone()
                        });
                        if let Some(source) = source {
                            let repeat = Fragment {
                                page_index: page,
                                y: top.as_px(),
                                x: col_x.as_px(),
                                ..source.clone()
                            };
                            let group = geometry.entry(h.group).or_default();
                            if group.fragments.is_empty() {
                                group.fragments.push(source);
                                group.is_repeat = true;
                            }
                            group.fragments.push(repeat);
                        }
                    }
                    for band in bands {
                        let band_y = top + head_h + (band.top - bands[0].top);
                        for &cell in cells
                            .iter()
                            .skip(band.cells.start as usize)
                            .take(band.cells.len())
                        {
                            record_descendant(
                                geometry,
                                doc,
                                cell,
                                page,
                                band_y - band.top,
                                col_x,
                                0,
                            );
                        }
                    }
                }
            }
        }
    }
}

/// Per-source-node geometry: every page on which the node has a placement.
///
/// For the block-only fragmenter the vector is normally length 1 (the node
/// fits on one page). A node taller than the page produces multiple
/// fragments — but in the current measurement-only implementation we
/// emit it as a single oversized fragment on the page where its top
/// edge lands, because we have no inline / break point information yet.
///
/// # Repeat vs. split semantics
///
/// `is_repeat = false` (default): the vector represents a *split* —
/// each fragment is one slice of the same content, so consumers
/// accumulate `frag.height` to recover where to slice paragraph lines
/// or block content.
///
/// `is_repeat = true`: the vector represents *per-page repetition* —
/// every fragment carries the full content (`width` / `height` ==
/// the full element size). Consumers must NOT slice; each fragment
/// is a complete redraw at the same coordinates. Used by
/// [`append_position_fixed_fragments`] for `position: fixed` elements
/// that repeat on every page.
#[derive(Clone, Debug, Default)]
pub struct PaginationGeometry {
    pub fragments: Vec<Fragment>,
    pub is_repeat: bool,
    /// F30: set for a paragraph nested in a block that was split between line
    /// boxes by `fragment_nested_inline_root`. Each fragment's `height` is then
    /// the span of its lines only; the box's own frame (border + padding) is
    /// `(top, bottom)` px bigger, the top on every fragment and the bottom on the
    /// last. `None` for everything else.
    pub line_split_insets: Option<(crate::units::Px, crate::units::Px)>,
    /// html-to-pdf F30f-d: with `line_split_insets`, the box is sliced at its breaks (`box-decoration-break: slice`,
    /// css-break-3 §5.4): the top inset is on the first fragment only, the bottom one on the last, and the renderer
    /// opens the edges at a break (no border, padding or radius there). `false` keeps the frame closed on every page.
    pub line_split_slice: bool,
    /// F78: for an inline root split between line boxes by
    /// [`fragment_inline_root`] or [`fragment_nested_inline_root`], the lines each
    /// fragment holds, parallel to `fragments`. Empty for everything else (the
    /// renderer then measures the lines itself, with a warning).
    pub line_slices: Vec<LineSlice>,
    /// F30d: for a multicol container cut by [`fragment_multicol_container`], what each column holds on every
    /// page (absolute page index), parallel to the container's `fragments`. Empty for everything else.
    pub multicol_pages: Vec<(u32, McPage)>,
    /// F174: set for a table that a multicol container cut between rows: each fragment is a closed piece of the
    /// table (its frame is as tall as the fragment), not a slice of one table box, and the header group repeats
    /// at the top of the pieces that continue it. `false` for everything else.
    pub table_pieces: bool,
    /// F37: for `<body>` only, its border box on each page it spans, in strip coordinates (y 0 is the top of the
    /// page's body area; on page 0 that is body's own top). Page 0 runs to `bottom(0)`, a middle page to
    /// `fresh()`, the last page to where the box ends ([`body_box_slices`]). The renderer paints body's own
    /// background, border and shadow from these instead of at a page-high box on every page. Empty for everything else
    /// (and for a body that was not fragmented, which has no body fragment).
    pub box_slices: Vec<Fragment>,
}

impl PaginationGeometry {
    /// Whether this node's content was *split* across multiple pages —
    /// i.e. each fragment is a slice of the same content. Returns
    /// `false` when the geometry represents per-page repetition
    /// (`is_repeat == true`), because in that case every fragment
    /// carries the full content and slicers must NOT subdivide it.
    pub fn is_split(&self) -> bool {
        !self.is_repeat && self.fragments.len() > 1
    }
}

/// Side-table mapping DOM `usize` NodeIds to their pagination geometry.
///
/// `BTreeMap` for the same determinism reason as
/// [`crate::multicol_layout::MulticolGeometryTable`]: PDF byte order
/// downstream depends on iteration order.
pub type PaginationGeometryTable = BTreeMap<usize, PaginationGeometry>;

/// F29: the block-size of the page strips the fragmenter fills, in CSS px.
///
/// The fragmenter works in body coordinates (`y = 0` is the top of the body's
/// content), but the renderer paints page 0 lower by the body's own offset
/// (`body` margin, a margin collapsed through `<body>`, `html` padding and
/// border: `body.final_layout.location.y`). Only page 0 is shifted — the offset
/// is not repeated on continuation pages — so page 0 ends that much higher in
/// the fragmenter's coordinates than the others.
///
/// Every place that compares a position with the end of the page asks for
/// [`PageStrips::bottom`] of the page it is on; places that only ask whether
/// something is taller than a page (it will not fit anywhere, so the offset is
/// irrelevant) use [`PageStrips::fresh`]. Passing this type instead of a bare
/// height makes each site choose.
#[derive(Clone, Copy, Debug, PartialEq)]
pub(crate) struct PageStrips {
    page_h: f32,
    first_offset: f32,
}

impl PageStrips {
    /// `page_h` is the content height of a page; `first_offset` the shift of
    /// page 0. An offset that is negative, not finite, or leaves no room on page 0
    /// is ignored (an offset that large is warned about: the content of page 0
    /// would be painted outside the page).
    pub(crate) fn new(page_h: f32, first_offset: f32) -> Self {
        let mut first_offset = if first_offset.is_finite() && first_offset > 0.0 {
            first_offset
        } else {
            0.0
        };
        if page_h > 0.0 && first_offset >= page_h {
            log::warn!(
                "pagination: the body starts {first_offset}px down the page, no less than the \
                 {page_h}px content height; page 0 is paginated as if it started at the top",
            );
            first_offset = 0.0;
        }
        Self {
            page_h,
            first_offset,
        }
    }

    /// The y (body coordinates) below which nothing may be placed on `page`.
    pub(crate) fn bottom(self, page: u32) -> f32 {
        if page == 0 {
            self.page_h - self.first_offset
        } else {
            self.page_h
        }
    }

    /// The block-size of a page that starts at its top: every page but the first.
    pub(crate) fn fresh(self) -> f32 {
        self.page_h
    }

    /// html-to-pdf F30f-b: [`bottom`](Self::bottom) as Chrome has it, rounded up to a whole px. Chrome's fragmentainer
    /// for an A4 page with 15 mm margins is 1010 px, not 1009.13: a 20 px line fits under a 990 px spacer and
    /// not under a 990.5 px one (measured, m141). The clip boxes that are fragmented by their own block size count the
    /// room on a page that way, so the box ends where Chrome's does and the content after it follows.
    pub(crate) fn chrome_bottom(self, page: u32) -> f32 {
        self.bottom(page).ceil()
    }
}

/// F37: the border box of `<body>` on each page it spans (css-break-3 section 5.4, `box-decoration-break: slice`): a
/// box that breaks fills the fragmentainer on every page but its last, so page 0 runs to `bottom(0)` and a middle page
/// to `fresh()`; the last page runs to `end.1`, the y where the box ends on it (its last in-flow child plus what body
/// has below it: padding, border, an inner margin), and never past the page area.
///
/// `end` is `(page, y)` of that end. A body that ends on page 0 has one slice of `body_h`, clamped to the page area
/// (a body taller than its content's pages is not extended with pages of its own, F37c). `x` and `w` are body's, copied
/// into every slice. Pure.
pub(crate) fn body_box_slices(
    strips: PageStrips,
    x: f32,
    w: f32,
    body_h: f32,
    end: (u32, f32),
) -> Vec<Fragment> {
    let (last, end_y) = end;
    let slice = |page: u32, height: f32| Fragment {
        page_index: page,
        x: x.as_px(),
        y: 0.0_f32.as_px(),
        width: w.as_px(),
        height: height.max(0.0).as_px(),
    };
    if last == 0 {
        return vec![slice(0, body_h.min(strips.bottom(0)))];
    }
    let mut slices = Vec::with_capacity(last as usize + 1);
    slices.push(slice(0, strips.bottom(0)));
    for page in 1..last {
        slices.push(slice(page, strips.fresh()));
    }
    slices.push(slice(last, end_y.min(strips.bottom(last))));
    slices
}

/// Taffy tree wrapper that intercepts the pagination root through
/// `compute_child_layout` and routes it through fulgur's own
/// page-stripping logic.
///
/// `page_height_px` is the height of the page content area (after the
/// engine has subtracted page-margin / `@page` margins). The wrapper
/// borrows the `BaseDocument` for one pass and is discarded; the
/// `geometry` it accumulates is drained via [`Self::take_geometry`] so
/// callers can either thread it into `ConvertContext` or drop it for
/// observational use.
pub struct PaginationLayoutTree<'a> {
    pub(crate) doc: &'a mut BaseDocument,
    pub(crate) strips: PageStrips,
    pub(crate) geometry: PaginationGeometryTable,
    /// Cached id of the `<body>` element, if any. Used as the
    /// fragmentation root for the block-only fragmenter. `None` means the
    /// document had no body and the pass becomes a no-op.
    pub(crate) body_id: Option<usize>,
    /// fulgur-k0g0: `break-before` / `break-after` / `break-inside`
    /// per node, harvested by
    /// [`crate::blitz_adapter::extract_column_style_table`]. The table
    /// is shared with `multicol_layout`, so the pagination fragmenter
    /// does not maintain its own break-style extraction. `None` means
    /// "no break properties set anywhere", which the fragmenter treats
    /// as all-`Auto`.
    pub(crate) column_styles: Option<&'a crate::column_css::ColumnStyleTable>,
    /// fulgur-s67g Phase 2.2: `position: running()` element instances
    /// registered by [`crate::blitz_adapter::RunningElementPass`].
    /// `fragment_pagination_root` consults this store to skip running-
    /// named children — they are placed into `@page` margin boxes per
    /// page, not into the body's flow, so they must not contribute to
    /// the body cursor or page-fragment geometry. `None` (the default
    /// for unit-test entry points) means "no running mappings"; the
    /// fragmenter treats every body child as in-flow.
    pub(crate) running_store: Option<&'a crate::gcpm::running::RunningElementStore>,
    /// F30d: the geometry of the multicol pass, which says what flows in each multi-column container. `None`
    /// (unit tests) leaves every multi-column container to the paths that treat it as one box.
    pub(crate) multicol: Option<&'a crate::multicol_layout::MulticolGeometryTable>,
    /// fulgur-uebl: per-element used page-name (CSS Page 3 §5.3),
    /// resolved from the same author-facing `page` declarations the
    /// `column_styles` table carries. The fragmenter consults this when
    /// a child is iterated: if its used page-name differs from the
    /// previously-placed sibling's, an implicit forced page break is
    /// induced before the child. `None` means the document has no
    /// `page` declarations and the fragmenter skips the comparison
    /// entirely.
    pub(crate) used_page_names: Option<crate::blitz_adapter::UsedPageNameTable>,
    /// F144: the answers to "is there a forced break below" and "is there a page-name change below" for every node,
    /// built from `column_styles` and `used_page_names` at the start of the pass. `None` (unit-test entry points
    /// that build the tree by hand) makes the gates walk the subtree instead.
    pub(crate) subtree_facts: Option<SubtreeFacts>,
}

/// One-shot entry: run the block-level fragmenter for `doc` against a
/// `page_height_px` page strip and return the resulting geometry table.
///
/// Intended to be called **after** `blitz_adapter::resolve()` (and after
/// `multicol_layout::run_pass` when multicol is in play) so that
/// `final_layout` reflects the post-layout positions the fragmenter
/// walks.
///
/// Calls [`fragment_pagination_root`] directly to walk body's
/// children's existing `final_layout` (populated by
/// `blitz_adapter::resolve` and `multicol_layout::run_pass`) and
/// record per-node fragments. Same direct-walk model as the production
/// entry point — see the module docs for why we skip
/// `taffy::compute_root_layout` here. The Taffy-dispatch path is
/// preserved as test-only via
/// [`PaginationLayoutTree::drive_taffy_root_layout`].
///
/// Test-only convenience for fixtures that don't need break-style
/// awareness. Production callers use [`run_pass_with_break_styles`]
/// so `break-before` / `break-after` / `break-inside` from the shared
/// `ColumnStyleTable` are honoured.
#[cfg(test)]
pub fn run_pass(doc: &mut BaseDocument, page_height_px: f32) -> PaginationGeometryTable {
    run_pass_inner(doc, page_height_px, None, None, None)
}

/// fulgur-k0g0 variant: thread the document's `break-before` /
/// `break-after` / `break-inside` side-table (harvested by
/// [`crate::blitz_adapter::extract_column_style_table`]) into the
/// fragmenter. `break-before: page` and `break-after: page` force
/// page boundaries; `break-inside: avoid` defers a child that does not
/// fit the remaining strip rather than splitting it, as long as the
/// child fits a page ([`avoid_inside_holds`]); an avoid block wrapper in block
/// flow moves whole instead of being entered and split between its children
/// ([`avoid_keeps_whole`], F83b).
pub fn run_pass_with_break_styles<'a>(
    doc: &'a mut BaseDocument,
    page_height_px: crate::units::Px,
    column_styles: &'a crate::column_css::ColumnStyleTable,
) -> PaginationGeometryTable {
    run_pass_inner(
        doc,
        page_height_px.to_f32(),
        Some(column_styles),
        None,
        None,
    )
}

/// fulgur-s67g Phase 2.2 variant: extends
/// [`run_pass_with_break_styles`] with awareness of `position:
/// running()` element instances. Running children are skipped during
/// the body walk so they do not contribute to body cursor or page
/// fragments — per-page placement is handled by
/// [`collect_running_element_states`].
pub fn run_pass_with_break_and_running<'a>(
    doc: &'a mut BaseDocument,
    page_height_px: f32,
    column_styles: &'a crate::column_css::ColumnStyleTable,
    running_store: &'a crate::gcpm::running::RunningElementStore,
) -> PaginationGeometryTable {
    run_pass_inner(
        doc,
        page_height_px,
        Some(column_styles),
        Some(running_store),
        None,
    )
}

/// F30d variant: the production entry point. Like [`run_pass_with_break_and_running`] (`running_store` is
/// optional), and `multicol` (the geometry the multicol pass recorded, see
/// [`crate::multicol_layout::run_pass`]) lets the fragmenter cut a multi-column container that is taller than
/// the room it has over the pages instead of treating it as one box ([`fragment_multicol_container`]).
pub fn run_pass_with_multicol<'a>(
    doc: &'a mut BaseDocument,
    page_height_px: f32,
    column_styles: &'a crate::column_css::ColumnStyleTable,
    running_store: Option<&'a crate::gcpm::running::RunningElementStore>,
    multicol: &'a crate::multicol_layout::MulticolGeometryTable,
) -> PaginationGeometryTable {
    run_pass_inner(
        doc,
        page_height_px,
        Some(column_styles),
        running_store,
        Some(multicol),
    )
}

fn run_pass_inner<'a>(
    doc: &'a mut BaseDocument,
    page_height_px: f32,
    column_styles: Option<&'a crate::column_css::ColumnStyleTable>,
    running_store: Option<&'a crate::gcpm::running::RunningElementStore>,
    multicol: Option<&'a crate::multicol_layout::MulticolGeometryTable>,
) -> PaginationGeometryTable {
    // fulgur-uebl: pre-compute the used page-name table when column
    // styles are available. The walk takes one DOM pass and produces a
    // `BTreeMap` keyed by node id, matching the determinism convention
    // used by the rest of the side-tables.
    //
    // F144: only when some node names a page. Without one every entry would be `(None, None)`, which is also what
    // every reader makes of a missing table.
    let used_page_names = column_styles
        .filter(|cs| crate::column_css::has_named_page(cs))
        .map(|cs| crate::blitz_adapter::compute_used_page_names(doc, cs));
    let subtree_facts = SubtreeFacts::build(doc, column_styles, used_page_names.as_ref());
    let mut tree = PaginationLayoutTree::new(doc, page_height_px);
    tree.column_styles = column_styles;
    tree.running_store = running_store;
    tree.multicol = multicol;
    tree.used_page_names = used_page_names;
    tree.subtree_facts = Some(subtree_facts);
    if tree.body_id.is_some() && page_height_px > 0.0 {
        // Read body's children's existing `final_layout` (populated by
        // Blitz's `resolve()` and `multicol_layout::run_pass`) and
        // produce the page-fragment geometry without re-driving Taffy.
        //
        // We deliberately *skip* `drive_taffy_root_layout` (which runs
        // `taffy::compute_root_layout` through the wrapper) on the
        // production path: re-issuing layout for body forces every
        // descendant's `compute_child_layout` to re-execute, and even
        // with cache hits the round-trip introduces sub-pixel
        // floating-point drift that breaks
        // `examples_determinism`'s byte-wise comparison against
        // committed PDFs. The wrapper's `LayoutPartialTree` /
        // `RoundTree` / `CacheTree` / `TraversePartialTree` impls
        // remain in place for tests that *do* exercise the full Taffy
        // dispatch (`drive_taffy_root_layout`) and as scaffolding for
        // a future per-strip-constrained variant where re-driving
        // layout is what actually does the pagination work.
        tree.fragment_pagination_root();
    }
    tree.take_geometry()
}

impl<'a> PaginationLayoutTree<'a> {
    pub fn new(doc: &'a mut BaseDocument, page_height_px: f32) -> Self {
        let body_id = find_body_id(doc);
        let strips = PageStrips::new(page_height_px, body_origin_in_px(doc).1);
        Self {
            doc,
            strips,
            geometry: BTreeMap::new(),
            body_id,
            column_styles: None,
            running_store: None,
            multicol: None,
            used_page_names: None,
            subtree_facts: None,
        }
    }

    /// fulgur-uebl: lookup helper for the per-element start / end used
    /// page-names (CSS Page 3 §5.3). Returns `(start, end)` where each
    /// is `None` for the unnamed/auto page or `Some(name)` for a named
    /// page. When the document has no `page` declarations at all the
    /// table is absent; we return `(None, None)` so the comparison `==`
    /// always succeeds and no implicit breaks fire.
    fn used_page_endpoints_of(&self, node_id: usize) -> (Option<String>, Option<String>) {
        self.used_page_names
            .as_ref()
            .and_then(|t| t.get(&node_id).cloned())
            .unwrap_or((None, None))
    }

    /// Drain the accumulated per-node geometry table.
    ///
    /// Mirrors [`crate::multicol_layout::FulgurLayoutTree::take_geometry`]:
    /// uses `mem::take` so a second call returns an empty table rather than
    /// double-counting.
    pub fn take_geometry(&mut self) -> PaginationGeometryTable {
        std::mem::take(&mut self.geometry)
    }

    /// Drive `taffy::compute_root_layout(&mut self, body_id, ...)` so the
    /// wrapper's `compute_child_layout` fires on body and dispatches into
    /// [`compute_pagination_layout`].
    ///
    /// **Test-only.** Production callers (`run_pass_with_break_styles`)
    /// reach geometry via `fragment_pagination_root` directly because
    /// re-driving Taffy on body re-stores every descendant's layout
    /// fields (even on cache hits) and introduces sub-pixel
    /// floating-point drift that breaks `examples_determinism`'s
    /// byte-wise PDF comparison against committed goldens. This entry
    /// is preserved so the wrapper's `LayoutPartialTree` / `RoundTree`
    /// / `CacheTree` / `TraversePartialTree` impls keep one runtime
    /// exerciser and a future per-strip-constrained variant has a
    /// drop-in seam.
    #[cfg(test)]
    ///
    /// The available space we hand Taffy is the body's *existing* layout
    /// width and an unbounded height (`AvailableSpace::MaxContent`). We
    /// pass MaxContent rather than `page_height_px` because the
    /// fragmenter relies on the children's natural `final_layout`
    /// heights — restricting `available_space.height` here would let
    /// Taffy clip or shrink children, breaking the measurement walk.
    /// (The fulgur-ik6o spike experimented with `Definite` and
    /// established that Taffy's block layout does not consult
    /// `available_space.height` for mid-element splitting; see
    /// `docs/plans/2026-04-28-pagination-layout-spike.md`.)
    ///
    /// `compute_root_layout` resets the layout's `location` to `(0, 0)`
    /// because it treats the node as a Taffy root. Body is *not* a real
    /// root in the document tree (html is its parent), so we save and
    /// restore body's location across the call — same approach as
    /// [`crate::multicol_layout::FulgurLayoutTree::layout_multicol_subtrees`].
    fn drive_taffy_root_layout(&mut self) {
        // fulgur-uebl: production `run_pass_inner` populates
        // `used_page_names` once `column_styles` is available; the
        // test-only Taffy parity path still needs the same table or
        // any future fixture using `page:` would silently skip the
        // implicit-break logic. Lazy-fill so call sites that want the
        // baseline behaviour can still leave both `None`.
        if self.used_page_names.is_none() {
            self.used_page_names = self
                .column_styles
                .filter(|cs| crate::column_css::has_named_page(cs))
                .map(|cs| crate::blitz_adapter::compute_used_page_names(self.doc, cs));
        }
        let Some(body_id) = self.body_id else {
            return;
        };
        let nid = NodeId::from(body_id);
        let prior_unrounded = self.doc.get_unrounded_layout(nid);
        let prior_final = self
            .doc
            .get_node(body_id)
            .map(|n| n.final_layout)
            .unwrap_or_default();

        let avail = Size {
            width: AvailableSpace::Definite(prior_unrounded.size.width.max(1.0)),
            height: AvailableSpace::MaxContent,
        };
        taffy::compute_root_layout(self, nid, avail);

        // Restore body's full layout so downstream readers (convert,
        // paginate) see byte-identical state to Blitz's first pass —
        // examples_determinism would otherwise pick up sub-pixel
        // float-rep differences when `compute_root_layout` re-stores
        // the same logical values via `set_unrounded_layout` /
        // `set_final_layout`.
        if let Some(node) = self.doc.get_node_mut(body_id) {
            node.unrounded_layout = prior_unrounded;
            node.final_layout = prior_final;
        }
    }

    /// Walk the body's direct block children and record fragments.
    ///
    /// F111: split `<body>` between its line boxes when it is the inline root and taller than a page
    /// ([`body_line_split`]). Returns the number of fragments emitted.
    ///
    /// The fragments are the ones [`fragment_nested_inline_root`] makes for a paragraph nested in a block: one per
    /// page, the first on page 0 (so body's own `counter-reset` / `string-set` fire there, see
    /// [`PaginationLayoutTree::fragment_pagination_root`]), each with the lines of its page; body's padding and
    /// border are reserved on every page (`line_split_insets`). Inline boxes and images in the lines get their
    /// fragments on the page of their line.
    ///
    /// The walk over body's children does not run: they are text nodes and inline elements, and an inline box
    /// would be placed a second time at its unsplit position. A running element among them (GCPM 3 §2.2) still
    /// gets the zero-height fragment on page 0 that the margin boxes read its source page from.
    fn fragment_body_inline_root(
        &mut self,
        body_id: usize,
        body_x: f32,
        body_w: f32,
        split: &LineSplit,
    ) -> usize {
        let end = fragment_nested_inline_root(
            &mut self.geometry,
            self.doc,
            body_id,
            body_x,
            body_w,
            0.0,
            0,
            self.strips,
            0.0,
            0.0,
            split,
            0,
        );
        // F37: body's own box ends where its last line (and its padding and border below) ends.
        let body_h = self
            .doc
            .get_node(body_id)
            .map_or(0.0, |n| n.final_layout.size.height);
        let slices = body_box_slices(self.strips, body_x, body_w, body_h, end);
        self.geometry.entry(body_id).or_default().box_slices = slices;
        let mut emitted = self.geometry.get(&body_id).map_or(0, |g| g.fragments.len());
        if let Some(store) = self.running_store {
            let children = self
                .doc
                .get_node(body_id)
                .map(|body| body.children.clone())
                .unwrap_or_default();
            for child_id in children {
                if store.instance_for_node(child_id).is_none() {
                    continue;
                }
                let Some(child) = self.doc.get_node(child_id) else {
                    continue;
                };
                if child.element_data().is_none() {
                    continue;
                }
                self.geometry
                    .entry(child_id)
                    .or_default()
                    .fragments
                    .push(Fragment {
                        page_index: 0,
                        x: (body_x + child.final_layout.location.x).as_px(),
                        y: 0.0_f32.as_px(),
                        width: 0.0_f32.as_px(),
                        height: 0.0_f32.as_px(),
                    });
                emitted += 1;
            }
        }
        emitted
    }

    /// Called from `compute_pagination_layout` after Taffy dispatches
    /// body's layout through the wrapper. Returns the number of
    /// fragments emitted. `0` means either the document has no body or
    /// the body has no children — both are expected for empty
    /// documents (a single empty page is still produced downstream).
    ///
    /// Algorithm (block-only, measurement-only):
    ///
    /// 1. Look up body's `final_layout` to fix the available width and
    ///    the body-relative y origin.
    /// 2. For each direct child whose `final_layout` is non-zero:
    ///    a. Compute the child's bottom edge relative to body content.
    ///    b. If `cursor_y + child_h <= page_height_px` the child fits on
    ///    the current page; emit one fragment with `page_index` set.
    ///    c. Otherwise advance `page_index`, reset `cursor_y` to 0, then
    ///    place the child on the new page. A child taller than the
    ///    page is emitted whole (oversized fragment) — true split
    ///    requires inline / break point support that is out of scope.
    /// 3. Record `Vec<Fragment>` per source node id.
    pub fn fragment_pagination_root(&mut self) -> usize {
        let Some(body_id) = self.body_id else {
            return 0;
        };
        if self.strips.fresh() <= 0.0 {
            return 0;
        }

        let body_layout = self
            .doc
            .get_node(body_id)
            .map(|n| n.final_layout)
            .unwrap_or_default();
        let body_w = body_layout.size.width;
        let body_x = body_layout.location.x;

        // F111: body is itself the inline root when all its in-flow children are inline-level (CSS 2.1 §9.4.2): its
        // lines are its own and no child carries them, so the walk below would see text nodes and `<br>`s of zero
        // height and place every line on page 0. Split it between its line boxes like a paragraph nested in a block.
        if let Some(body) = self.doc.get_node(body_id)
            && body.flags.is_inline_root()
        {
            match body_line_split(self.doc, body, self.strips) {
                Some(split) => {
                    return self.fragment_body_inline_root(body_id, body_x, body_w, &split);
                }
                None => {
                    // A barrier (clip, multicol, transform, ...) keeps body whole: say so, as for any paragraph
                    // that is placed whole and ends below the page. Not when body is kept for another reason (it
                    // fits, or it holds a box taller than a page that the walk below fragments): that is not a
                    // loss, and the walk warns where it is one.
                    if let Some(html) = body.parent.and_then(|id| self.doc.get_node(id))
                        && split_barrier(self.doc, html, body).is_some()
                    {
                        warn_unprinted_inline_lines(
                            self.doc,
                            html,
                            body,
                            0.0,
                            self.strips.bottom(0),
                        );
                    }
                }
            }
        }

        // fulgur-s67g Phase 2.3 (counter parity follow-up): record
        // body itself as a fragment on page 0. body's own
        // counter-reset / string-set / bookmark declarations must fire
        // only on page 0 (GCPM string-set / counters §3), so they need
        // to be recorded on the first page and only there.
        //
        // Without this entry the fragmenter's geometry table excludes body
        // entirely; `collect_counter_states` /
        // `collect_string_set_states` / `collect_bookmark_entries`
        // miss body's ops (e.g. `tests/gcpm_integration::test_counter_set`,
        // where body carries `counter-reset: chapter`). The body
        // fragment sits ahead of every body-direct-child entry in
        // NodeId order (Blitz allocates ids depth-first during parse,
        // so `body` is smaller than its descendants), so per-page
        // walks pick up body's ops before descendants — matching the
        // document tree-walk order the collectors expect.
        self.geometry
            .entry(body_id)
            .or_default()
            .fragments
            .push(Fragment {
                page_index: 0,
                x: body_x.as_px(),
                y: 0.0_f32.as_px(),
                width: body_w.as_px(),
                height: body_layout.size.height.as_px(),
            });

        // Prefer body's `layout_children` — same rationale as
        // `record_subtree_descendants`. When a block container has
        // mixed block-level and inline-level children, Stylo
        // synthesizes anonymous block wrappers around the inline-
        // level siblings (CSS 2.1 §9.2.1.1). Those wrappers carry
        // their own `node_id` and Taffy layout, but they live ONLY
        // in `layout_children` — `children` still points at the
        // underlying inline elements (e.g. a body containing
        // `<label>` followed by `<fieldset>` followed by
        // `<select><option>...</option></select>` produces an
        // anonymous block wrapping the `<select>` siblings, visible
        // only in `layout_children`).
        //
        // Without this preference v2 silently drops the inline-level
        // group's paint: extract assigns the inner paragraph's
        // `node_id` to the synthesized wrapper, but the body iteration
        // walks raw `children` and never visits the wrapper, so
        // geometry has no fragment for that node_id and
        // `dispatch_fragment` skips the paragraph entirely
        // (fulgur-bq6i: examples/wasm-demo lost label / legend / option
        // text content for this exact reason).
        let children = self
            .doc
            .get_node(body_id)
            .map(|n| {
                let layout_borrow = n.layout_children.borrow();
                if let Some(lc) = layout_borrow.as_deref()
                    && !lc.is_empty()
                {
                    lc.to_vec()
                } else {
                    n.children.clone()
                }
            })
            .unwrap_or_default();

        let mut page_index: u32 = 0;
        let mut cursor_y: f32 = 0.0;
        let mut emitted = 0usize;
        // F83b: `break-inside: avoid` on a child is honoured by moving it whole
        // only where its siblings are laid out in block flow.
        let body_block_flow = self
            .doc
            .get_node(body_id)
            .is_some_and(|b| is_block_flow_container(self.doc, b));
        // Tracks the bottom edge of the previously emitted in-flow child
        // in body-content-box coordinates. Used to pick up inter-child
        // gaps (collapsed margins, padding) that Blitz baked into each
        // child's `final_layout.location.y` but the cursor-only walk
        // would otherwise miss — the child margin gaps must be included
        // in body's normal-flow height (CSS 2.1 §10.6.3) so per-page
        // walks match the recorded fragment tops.
        let mut prev_bottom_y_in_body: f32 = 0.0;
        // The previous child when it was placed by the plain path: its top in
        // body coordinates, its page, the page-local y it was placed at, and
        // the cursor after it. A following child that starts level with it is
        // laid out beside it (the side-by-side run Blitz makes of boxes it
        // treats as inline-level, e.g. tables under `display: contents`) and
        // must neither move the cursor on again nor be counted as a row of
        // its own.
        let mut prev_plain: Option<(f32, u32, f32, f32)> = None;
        // F37: the page and the y (page-local) below the last child that was placed, for the end of body's box. Not
        // `page_index`: a trailing `break-after: page` moves that on without placing anything.
        let mut last_end: Option<(u32, f32)> = None;
        // fulgur-uebl: tracks the used page-name of the previously
        // placed in-flow sibling (`Some(Some(name))` named, `Some(None)`
        // auto/unnamed, `None` no previous). When the next sibling's
        // used page-name differs, we induce a forced break before it
        // (CSS Page 3 §5.3, "Using Named Pages").
        let mut prev_used_page: Option<Option<String>> = None;
        // html-to-pdf F30f-b: a clip box that was fragmented by its own block size ended where Chrome's does, in a
        // fragmentainer that is a whole px taller than ours (`PageStrips::chrome_bottom`): the child that follows it
        // may reach that px below the content box without moving to the next page.
        let mut after_clip_box = false;

        for child_id in children {
            let Some(child) = self.doc.get_node(child_id) else {
                continue;
            };
            // Skip pure-whitespace text nodes — same convention as
            // multicol_layout's `partition_children_into_segments`.
            if let Some(text) = child.text_data()
                && text.content.chars().all(char::is_whitespace)
            {
                continue;
            }
            // CSS 2.1 §10.6.4 / §9.6: out-of-flow elements
            // (`position: absolute` / `position: fixed`) do not
            // contribute to their containing block's normal-flow
            // height, so the fragmenter must not advance cursors for
            // them. Abs/fixed children are handled by separate passes
            // (`append_position_fixed_fragments` and the abs positioning
            // pipeline in `render`).
            {
                use ::style::properties::longhands::position::computed_value::T as Pos;
                let is_out_of_flow = child.primary_styles().is_some_and(|s| {
                    matches!(s.get_box().clone_position(), Pos::Absolute | Pos::Fixed)
                });
                if is_out_of_flow {
                    continue;
                }
            }
            // fulgur-s67g Phase 2.2: skip `position: running()` named
            // children from the body cursor. They are removed from
            // body flow and placed into `@page` margin boxes per page;
            // including them in the cursor would over-count height.
            //
            // Phase 3.4 follow-up (PR #296 Devin): record a zero-height
            // fragment at the cursor position before skipping so the
            // running node enters `geometry` keyed by its NodeId. The
            // fragment carries `height = 0` (cursor does not advance)
            // but `page_index` is the page on which the running
            // element's source position lands — exactly what
            // `collect_running_element_states` needs to map running
            // instances to their per-page state.
            if self
                .running_store
                .is_some_and(|s| s.instance_for_node(child_id).is_some())
            {
                if child.element_data().is_some() {
                    let layout = child.final_layout;
                    self.geometry
                        .entry(child_id)
                        .or_default()
                        .fragments
                        .push(Fragment {
                            page_index,
                            x: (body_x + layout.location.x).as_px(),
                            y: cursor_y.as_px(),
                            width: 0.0_f32.as_px(),
                            height: 0.0_f32.as_px(),
                        });
                    emitted += 1;
                }
                continue;
            }
            let layout = child.final_layout;
            // fulgur-2m6w: defend against a non-finite Taffy height
            // (`+inf` / `NaN`). A `NaN` height slips past the
            // `child_h > page_height_px + 1.0` slice gate (NaN compares
            // false) into the normal path where `cursor_y += child_h`
            // would poison every later page advance; `+inf` would corrupt
            // `prev_bottom_y_in_body`. Treat either as zero height — the
            // node still enters geometry via the `child_h <= 0.0` branch
            // so its counter / bookmark markers survive.
            let child_h = if layout.size.height.is_finite() {
                layout.size.height
            } else {
                0.0
            };
            let child_w = if layout.size.width > 0.0 {
                layout.size.width
            } else {
                body_w
            };
            if child_h <= 0.0 {
                // Phase 2.3 fix: zero-height **element** nodes still
                // need to enter geometry so their counter /
                // string-set / bookmark markers participate in the
                // per-page metadata walks (e.g.
                // `<div class="reset" style="..."></div>` carrying a
                // `counter-set` declaration). The fragment carries
                // height 0 so it does not advance the cursor — only
                // the NodeId matters for the per-page metadata walks.
                // Whitespace-only text nodes are already filtered
                // above; running and abs/fixed elements are filtered
                // before this branch.
                //
                // fulgur-p3uf (Phase 3.1.5a): honour `break-before`
                // / `break-after` on zero-height element nodes too.
                // Bare `<img>` with no explicit dimensions and
                // pseudo-only `<div>` (rendering only `::before`
                // content) both arrive here with `child_h == 0` after
                // Blitz's intrinsic-size collapse, but their
                // `break-before: page` directive must still take
                // effect (CSS Fragmentation §3, forced breaks apply
                // regardless of the element's own height — see
                // `tests/pseudo_only_break_before.rs`). An earlier
                // fragmenter `continue`'d before reading break
                // properties at all, silently dropping the directive.
                let zero_break_props = self
                    .column_styles
                    .and_then(|t| t.get(&child_id))
                    .cloned()
                    .unwrap_or_default();
                // fulgur-uebl: floats are out of normal flow for the
                // sibling page-name comparison (CSS Page 3 / CSS 2.1
                // §9.5). Skip the comparison and the `prev_used_page`
                // update entirely for floated zero-height children;
                // they do not establish class A break points and should
                // not influence break decisions on adjacent in-flow
                // boxes.
                let zero_is_float = crate::blitz_adapter::node_is_floating(child);
                let (zero_used_start, zero_used_end) = self.used_page_endpoints_of(child_id);
                let zero_page_name_changed = !zero_is_float
                    && prev_used_page
                        .as_ref()
                        .is_some_and(|p| *p != zero_used_start);
                let zero_force_break = matches!(
                    zero_break_props.break_before,
                    Some(crate::draw_primitives::BreakBefore::Page)
                ) || zero_page_name_changed;
                if zero_force_break && cursor_y > 0.0 {
                    page_index += 1;
                    cursor_y = 0.0;
                }
                if child.element_data().is_some() {
                    self.geometry
                        .entry(child_id)
                        .or_default()
                        .fragments
                        .push(Fragment {
                            page_index,
                            x: (body_x + layout.location.x).as_px(),
                            y: cursor_y.as_px(),
                            width: child_w.as_px(),
                            height: 0.0_f32.as_px(),
                        });
                    emitted += 1;
                }
                if !zero_is_float {
                    prev_used_page = Some(zero_used_end);
                }
                if matches!(
                    zero_break_props.break_after,
                    Some(crate::draw_primitives::BreakAfter::Page)
                ) {
                    page_index += 1;
                    cursor_y = 0.0;
                }
                continue;
            }

            let clip_end_slack = if std::mem::take(&mut after_clip_box) {
                1.0
            } else {
                0.0
            };
            // Pick up the inter-child gap in body coordinates (collapsed
            // top/bottom margins, body padding before the first child)
            // before any break / overflow logic so the cursor reflects
            // Blitz's flow positions. `max(0.0)` guards against negative
            // gaps from sibling overlap (rare with default UA styles).
            let this_top_in_body = layout.location.y;
            let gap = (this_top_in_body - prev_bottom_y_in_body).max(0.0);
            cursor_y += gap;
            let parallel_row = prev_plain
                .filter(|&(top, page, _, _)| {
                    page == page_index
                        && (this_top_in_body - top).abs() < 0.5
                        && this_top_in_body < prev_bottom_y_in_body - 0.5
                })
                .map(|(_, page, placed_y, end)| {
                    cursor_y = placed_y;
                    (page, end, prev_bottom_y_in_body)
                });
            prev_plain = None;

            // fulgur-k0g0: read break-before / break-after / break-inside
            // for this child from the column-style side-table (shared with
            // multicol). Default `Auto` for nodes the table does not cover.
            let break_props = self
                .column_styles
                .and_then(|t| t.get(&child_id))
                .cloned()
                .unwrap_or_default();

            // fulgur-uebl: page-name change between adjacent siblings
            // induces an implicit forced break (CSS Page 3 §5.3).
            // Treated identically to an authored `break-before: page`
            // so the existing leading-break-on-fresh-page collapse
            // applies (CSS 3 Fragmentation §3). Compare the previous
            // sibling's `end` against this child's `start` — that's how
            // a page-name change buried inside a subtree (e.g.
            // `propagated-008`) surfaces to the body-level walk.
            //
            // Floats are out of normal flow (CSS 2.1 §9.5) and do not
            // establish class A break points, so they're skipped from
            // both the comparison and the `prev_used_page` update.
            let is_float = crate::blitz_adapter::node_is_floating(child);
            let (used_start, used_end) = self.used_page_endpoints_of(child_id);
            let page_name_changed =
                !is_float && prev_used_page.as_ref().is_some_and(|p| *p != used_start);

            // `break-before: page` forces a page boundary before the
            // child whenever there is in-flow content already placed on
            // the current page. A leading break-before on a fresh page
            // is a no-op (CSS 3 Fragmentation §3 collapses it).
            let explicit_break_before = matches!(
                break_props.break_before,
                Some(crate::draw_primitives::BreakBefore::Page)
            );
            let page_filling_break_child = gap > 0.0
                && child_h >= self.strips.fresh() * 0.9
                && gap + child_h <= self.strips.fresh() + 0.5;
            if (explicit_break_before || page_name_changed) && emitted > 0 && cursor_y > 0.0 {
                page_index += 1;
                cursor_y = if explicit_break_before && page_filling_break_child {
                    gap
                } else {
                    0.0
                };
            }

            let avoid_inside = matches!(
                break_props.break_inside,
                Some(crate::draw_primitives::BreakInside::Avoid)
            );

            // F30d: a multi-column container that does not fit the room below the cursor is
            // distributed over the pages column by column, wherever it is itself an inline
            // root (the line-split path below would cut the one-column layout Blitz measured, not
            // the columns). One that fits keeps the paths below, byte for byte.
            if let Some(mc) = multicol_container(
                self.doc,
                self.multicol,
                self.column_styles,
                child_id,
                self.strips.fresh(),
            ) {
                let strips = self.strips;
                let wants = |page: u32, cursor: f32| {
                    multicol_cuts(
                        &mc,
                        child_h,
                        strips.bottom(page) - cursor,
                        strips.fresh(),
                        avoid_inside,
                    )
                };
                if wants(page_index, cursor_y) {
                    // F28: when its first unit does not fit below the cursor the container starts on the
                    // next page, where it may fit whole after all.
                    let leading = mc.top_inset() + mc.first_unit();
                    if cursor_y > 0.0
                        && leading > strips.bottom(page_index) - cursor_y
                        && leading <= strips.fresh()
                    {
                        page_index += 1;
                        cursor_y = 0.0;
                    }
                    if wants(page_index, cursor_y) {
                        let (new_page_index, new_cursor_y) = fragment_multicol_container(
                            &mut self.geometry,
                            self.doc,
                            McEnv {
                                column_styles: self.column_styles,
                                used_page_names: self.used_page_names.as_ref(),
                                facts: self.subtree_facts.as_ref(),
                                multicol: self.multicol,
                            },
                            &mc,
                            child_id,
                            body_x + layout.location.x,
                            child_w,
                            page_index,
                            cursor_y,
                            strips,
                        );
                        page_index = new_page_index;
                        cursor_y = new_cursor_y;
                        emitted += 1;
                        prev_bottom_y_in_body = this_top_in_body + child_h;
                        if let Some((row_page, row_end, row_bottom)) = parallel_row {
                            if page_index == row_page {
                                cursor_y = cursor_y.max(row_end);
                            }
                            prev_bottom_y_in_body = prev_bottom_y_in_body.max(row_bottom);
                        }
                        last_end = Some((page_index, cursor_y));
                        if !is_float {
                            prev_used_page = Some(used_end.clone());
                        }
                        if matches!(
                            break_props.break_after,
                            Some(crate::draw_primitives::BreakAfter::Page)
                        ) {
                            page_index += 1;
                            cursor_y = 0.0;
                        }
                        continue;
                    }
                }
            }

            // fulgur-p55h: if the child carries a Parley inline layout,
            // probe its line metrics and split at line boundaries —
            // mirrors the v1 paragraph-pageable split path (removed in
            // PR 8j-1; see git history) but inside the Taffy hook rather
            // than post-conversion.
            //
            // fulgur-k0g0: when `break-inside: avoid` is set, fall
            // through to the block path below so the paragraph emits
            // whole instead of splitting between lines.
            //
            // F83: only while it fits a page. A paragraph taller than a
            // page cannot be kept together, so `avoid` is relaxed
            // (css-break-3 §4.4) and it takes this line path like an
            // `auto` one: the strip slicer below cuts at fixed heights,
            // records no line ranges, and the renderer then measures the
            // lines from those heights and drops the one that straddles
            // each cut.
            //
            // "Fits a page" is about the lines, not the box: a box taller
            // than a page only through `height`, padding or `min-height`
            // keeps its lines on one page, so `avoid` holds and it takes
            // the path it always had (F83c: the strip slicer continues the
            // box frame over the pages like Chrome).
            let all_line_metrics = collect_inline_line_metrics(child);
            let lines_extent = all_line_metrics
                .last()
                .map_or(0.0, |l| l.1 - all_line_metrics[0].0);
            let line_metrics =
                if avoid_inside_holds(avoid_inside, lines_extent, self.strips.fresh()) {
                    Vec::new()
                } else {
                    all_line_metrics
                };
            // html-to-pdf F30f-b: a clip box that hides some of its content (`overflow-y` not `visible`, a block size
            // below its content) is placed whole while it fits the room below the cursor: the lines it hides are not
            // written, and the box is as tall as its block size. When it does not fit, it is fragmented by its own
            // block size, in place: its content is cut at the clip, its frames add up to its height, nothing at or
            // below the clip gets a page.
            let clip_limit_here = (body_block_flow
                && !is_float
                && !avoid_inside
                && !crate::blitz_adapter::is_multicol_container(child))
            .then(|| clip_block_limit(self.doc, child))
            .flatten();
            let clip_whole_now = clip_limit_here.is_some()
                && cursor_y + child_h <= self.strips.bottom(page_index) + 0.5;
            // html-to-pdf F30f-d: a paragraph with a border or padding that does not fit its page is cut by the splitter
            // that knows its insets, and sliced at its breaks (`box-decoration-break: slice`, css-break-3 §5.4). The line
            // fit of `fragment_inline_root` below counts the lines from the top of the border box, while the renderer
            // paints them `inset_top` lower: the last line of every page stood past the content box (in the margin, or
            // under the clip of an `overflow` box), and the frame closed `inset_top + inset_bottom` too early on the last
            // page. A box without insets, a clip box (above), `break-inside: avoid`, a float and a box that is not in
            // block flow keep the path they had (F30c for the unpadded ones).
            let insets = layout.border.top
                + layout.padding.top
                + layout.border.bottom
                + layout.padding.bottom;
            // The lines and their insets do not fit the room. A box whose `height`, `max-height` or `min-height` makes it
            // taller than that keeps the path it had (F83c: its frame continues over the pages, its lines stay on the
            // page that has room for them: a 120px box with four lines at the page bottom).
            let inset_split_here = clip_limit_here.is_none()
                && insets > 0.0
                && body_block_flow
                && !is_float
                && !avoid_inside
                && !crate::blitz_adapter::is_multicol_container(child)
                && !block_size_constrained(child)
                && cursor_y + insets + lines_extent > self.strips.bottom(page_index) + 0.5;
            if line_metrics.len() > 1
                && (clip_limit_here.is_some() && !clip_whole_now || inset_split_here)
                && let Some(body_node) = self.doc.get_node(body_id)
                && let Some(mut split) = line_split_candidate(self.doc, body_node, child)
            {
                split.slice = inset_split_here;
                // A paragraph that is itself the clip box: the lines at or below the clip are dropped.
                let leading = split.leading();
                if cursor_y > 0.0
                    && leading > self.strips.bottom(page_index) - cursor_y
                    && leading <= self.strips.fresh()
                {
                    page_index += 1;
                    cursor_y = 0.0;
                }
                let (new_page_index, new_cursor_y) = fragment_nested_inline_root(
                    &mut self.geometry,
                    self.doc,
                    child_id,
                    body_x + layout.location.x,
                    child_w,
                    cursor_y,
                    page_index,
                    self.strips,
                    0.0,
                    0.0,
                    &split,
                    0,
                );
                page_index = new_page_index;
                cursor_y = new_cursor_y;
                emitted += 1;
                after_clip_box = clip_limit_here.is_some();
                prev_bottom_y_in_body = this_top_in_body + child_h;
                if let Some((row_page, row_end, row_bottom)) = parallel_row {
                    if page_index == row_page {
                        cursor_y = cursor_y.max(row_end);
                    }
                    prev_bottom_y_in_body = prev_bottom_y_in_body.max(row_bottom);
                }
                last_end = Some((page_index, cursor_y));
                if !is_float {
                    prev_used_page = Some(used_end.clone());
                }
                if matches!(
                    break_props.break_after,
                    Some(crate::draw_primitives::BreakAfter::Page)
                ) {
                    page_index += 1;
                    cursor_y = 0.0;
                }
                continue;
            }
            if line_metrics.len() > 1 && !clip_whole_now {
                // fulgur-s67g Phase 2.2: if the paragraph cannot fit
                // the remaining space on the current page strip but
                // would fit (or at least start fresh) on a new page,
                // advance the page boundary before calling
                // `fragment_inline_root`. This is the "split before
                // the child" fallback (CSS Fragmentation §4.2, class A
                // break point) taken when the inline split path can't
                // honour widow / orphan constraints within the strip.
                let para_total_h = line_metrics
                    .last()
                    .map(|l| l.1 - line_metrics[0].0)
                    .unwrap_or(child_h);
                if cursor_y > 0.0
                    && cursor_y + para_total_h > self.strips.bottom(page_index) + clip_end_slack
                {
                    page_index += 1;
                    cursor_y = 0.0;
                }
                let para_x = layout.location.x;
                let (new_page_index, new_cursor_y, frag_count, placements) = fragment_inline_root(
                    &mut self.geometry,
                    child_id,
                    body_x + para_x,
                    child_w,
                    cursor_y,
                    page_index,
                    self.strips,
                    &line_metrics,
                );
                // Atomic inlines and images in the paragraph get their fragments on the page that
                // holds their line. Without them the paragraph's `InlineBox` items find no geometry
                // and the content is never drawn (F32a): an `<img>` or `inline-block` in any
                // paragraph of two or more lines vanished.
                record_split_inline_root_descendants(
                    &mut self.geometry,
                    self.doc,
                    child_id,
                    body_x + para_x,
                    0.0,
                    &placements,
                    layout.border.top + layout.padding.top,
                    0,
                );
                page_index = new_page_index;
                cursor_y = new_cursor_y;
                emitted += frag_count;
                prev_bottom_y_in_body = this_top_in_body + child_h;
                if let Some((row_page, row_end, row_bottom)) = parallel_row {
                    if page_index == row_page {
                        cursor_y = cursor_y.max(row_end);
                    }
                    prev_bottom_y_in_body = prev_bottom_y_in_body.max(row_bottom);
                }
                last_end = Some((page_index, cursor_y));
                if !is_float {
                    prev_used_page = Some(used_end.clone());
                }
                if matches!(
                    break_props.break_after,
                    Some(crate::draw_primitives::BreakAfter::Page)
                ) {
                    page_index += 1;
                    cursor_y = 0.0;
                }
                continue;
            }

            // fulgur-g9e3.1 + fulgur-a36m + fulgur-7hf5: unified
            // recursion gate covering all three break cases —
            //   - truly oversized (`child_h > page_h_px`) caught
            //     here when `would_split_block_subtree` finds the
            //     overflowing grandchild,
            //   - in-place mid-element split (`cursor_y + child_h >
            //     page_h_px` with `child_h <= page_h_px`),
            //   - forced break declared anywhere in the subtree.
            //
            // The recursion enters from the **current** cursor (not
            // a pre-advanced 0), so an in-place split with `cursor_y
            // > 0` produces a within-child fragment on the current
            // page and a tail on the next (CSS Fragmentation §3,
            // splits at class B/C break points inside the box).
            //
            // `would_split_block_subtree` is a cheap simulator that
            // walks the DOM children once with the same gap / OOF /
            // whitespace skips `fragment_block_subtree` uses — it
            // returns `false` when the children all fit in the
            // available strip, so the in-place case where the
            // parent's CSS height exceeds children's sum (e.g. a
            // 600px div with one 30px h2) falls through to the
            // existing whole-emit path and avoids the children-sum
            // parent-height bug.
            //
            // F83b: an `avoid` box that is kept whole (`avoid_keeps_whole`: it
            // fits a page, and either fits the room below the cursor or can
            // move to the next page) drops the `would_split_block_subtree`
            // term, so it is not entered and split between its children: it
            // takes the "move whole" advance below like any child the gate
            // skips. A forced break inside it still wins (css-break-3 §4.3).
            // A block that does not hold (taller than a page) is entered as
            // an `auto` one, but when content precedes it, it starts the next
            // page first (the rule above the gate). An inline root with text
            // children only never passes the gate (text nodes have no height).
            let child_node = self.doc.get_node(child_id);
            let has_splittable_children = child_node.is_some_and(|n| !n.children.is_empty());
            let is_inline_root = child_node.is_some_and(|n| n.flags.is_inline_root());
            // fulgur-7hf5: multicol containers (`column-count > 1` /
            // `column-width: <len>`) distribute children across
            // columns; their DOM children's flow does not match the
            // visual flow `would_split_block_subtree` simulates. Skip
            // recursion for them — multicol handles its own
            // fragmentation via `multicol_layout::run_pass`, and the
            // outer fragmenter treats the multicol container as a
            // single unit for break decisions.
            //
            // fulgur-916y: multicol containers with a `column-span:
            // all` direct child get an exception — the span subtree
            // is laid out by Taffy as a full-width block flowing
            // between the column groups, so block-flow recursion via
            // `fragment_block_subtree` can split it across pages
            // when it overflows. Containers without span:all stay
            // atomic.
            let is_multicol = child_node.is_some_and(crate::blitz_adapter::is_multicol_container);
            let multicol_has_span_all = is_multicol
                && crate::blitz_adapter::multicol_has_span_all_child(self.doc, child_id);
            let strips = self.strips;
            let page_height_px = strips.fresh();
            // F83b: in-flow content precedes this child on the page: the
            // cursor before the child's own margin gap. After a page advance
            // it is 0.
            let mut content_above = cursor_y - gap > 0.5;
            if content_above
                && body_block_flow
                && starts_with_relaxed_avoid(
                    self.doc,
                    child_id,
                    self.column_styles,
                    page_height_px,
                    0,
                )
            {
                // An avoid box taller than a page breaks like `auto`, but the
                // break before it (a perfect class A point) is preferred to
                // one inside it (css-break-3 §4.5): it starts the next page,
                // where it is entered from the top. The margin above it
                // truncates at the unforced break.
                page_index += 1;
                cursor_y = 0.0;
                content_above = false;
            }
            let avoid_extent_h = if avoid_inside && body_block_flow && !is_multicol {
                child_node.map_or(0.0, |n| {
                    avoid_measure(self.doc, child_id, n, self.column_styles)
                })
            } else {
                0.0
            };
            let keeps_whole_at = |page: u32, cursor: f32, above: bool| {
                avoid_inside
                    && body_block_flow
                    && !is_multicol
                    && avoid_keeps_whole(
                        true,
                        avoid_extent_h,
                        strips.bottom(page) - cursor,
                        page_height_px,
                        above,
                        0.0,
                    )
            };
            // F83b review (fuzz `Z2_34`): a relaxed avoid box with an `auto` height whose children fit the strip but
            // whose frame does not (bottom padding and border) is entered, as nested ones are, so that its last child
            // moves on and the frame closes with it; the strip slicer used to leave the frame's end on a page of
            // its own, which the next relaxed box then left blank.
            let relaxed_frame = avoid_inside
                && body_block_flow
                && !is_multicol
                && !avoid_inside_holds(true, avoid_extent_h, page_height_px)
                && child_node.is_some_and(|n| frame_is_content_sized(self.doc, n));
            let gate = |doc: &BaseDocument, available_strip: f32, keeps_whole: bool| {
                has_splittable_children
                    && (!is_multicol || multicol_has_span_all)
                    && (relaxed_frame
                        || forced_break_below(
                            self.subtree_facts.as_ref(),
                            doc,
                            self.column_styles,
                            child_id,
                        )
                        || page_name_change_below(
                            self.subtree_facts.as_ref(),
                            doc,
                            self.used_page_names.as_ref(),
                            child_id,
                        )
                        || (!keeps_whole
                            && would_split_block_subtree(
                                doc,
                                child_id,
                                available_strip,
                                page_height_px,
                                0,
                            )))
            };
            let mut keeps_whole = keeps_whole_at(page_index, cursor_y, content_above);
            // F30f-b: a clip box that fits the room is not entered (its content past the clip is hidden); one that does
            // not fit is entered, wherever its children are.
            let clip_enters = |page: u32, cursor: f32| -> Option<bool> {
                clip_limit_here?;
                let whole = cursor + child_h <= strips.bottom(page) + 0.5;
                Some(!whole && has_splittable_children && !is_multicol && !is_inline_root)
            };
            let mut needs_recursion = match clip_enters(page_index, cursor_y) {
                Some(enter) => enter,
                None => gate(
                    self.doc,
                    (strips.bottom(page_index) - cursor_y).max(0.0),
                    keeps_whole,
                ),
            };
            // F28: entering a box means placing its first row at the current
            // cursor. When even that row does not fit below the cursor, the
            // only break point before it is the class A point in front of the
            // box (CSS Fragmentation 3 §4.1), so move the whole box to the next
            // page first. Entering it anyway hangs the row into the bottom
            // margin, leaves the cursor past the page end, and every following
            // sibling is then entered past the bottom as well (the page count
            // stays at one and the rest is clipped). The margin above the box
            // truncates at the unforced break, as for the non-recursive advance
            // below.
            //
            // A first row taller than a page gains nothing from a fresh page
            // (it overflows there too), so such a box is entered where it is.
            //
            // F83b: a kept-whole avoid box that a forced break inside makes the
            // gate enter counts the piece before the first forced break; the
            // break itself wins, what follows it starts a page of its own.
            if needs_recursion && cursor_y > 0.0 {
                let leading = if keeps_whole {
                    avoid_piece_size(self.doc, child_id, self.column_styles, 0).min(child_h)
                } else {
                    // Counting an avoid box below as a whole only helps when something precedes this child and it can
                    // move; first on its page it would only be moved away from its own top (a blank page).
                    leading_block_size(
                        self.doc,
                        child_id,
                        self.column_styles.filter(|_| content_above),
                        0.0,
                        0,
                        page_height_px,
                    )
                };
                if leading <= page_height_px && cursor_y + leading > strips.bottom(page_index) {
                    page_index += 1;
                    cursor_y = 0.0;
                    content_above = false;
                    keeps_whole = keeps_whole_at(page_index, cursor_y, content_above);
                    needs_recursion = match clip_enters(page_index, cursor_y) {
                        Some(enter) => enter,
                        None => gate(self.doc, strips.bottom(page_index), keeps_whole),
                    };
                }
            }
            if needs_recursion {
                let child_x_in_body = body_x + layout.location.x;
                let (new_page, new_cursor, _, _) = fragment_block_subtree(
                    &mut self.geometry,
                    self.doc,
                    self.column_styles,
                    self.used_page_names.as_ref(),
                    self.subtree_facts.as_ref(),
                    self.multicol,
                    child_id,
                    child_w,
                    child_x_in_body,
                    page_index,
                    cursor_y,
                    strips,
                    0,
                    0.0,
                    0.0,
                    content_above,
                );
                page_index = new_page;
                cursor_y = new_cursor;
                emitted += 1;
                after_clip_box = clip_limit_here.is_some();
                prev_bottom_y_in_body = this_top_in_body + child_h;
                if let Some((row_page, row_end, row_bottom)) = parallel_row {
                    if page_index == row_page {
                        cursor_y = cursor_y.max(row_end);
                    }
                    prev_bottom_y_in_body = prev_bottom_y_in_body.max(row_bottom);
                }
                last_end = Some((page_index, cursor_y));
                if !is_float {
                    prev_used_page = Some(used_end.clone());
                }
                if matches!(
                    break_props.break_after,
                    Some(crate::draw_primitives::BreakAfter::Page)
                ) {
                    page_index += 1;
                    cursor_y = 0.0;
                }
                continue;
            }

            // No recursion needed — apply the existing strip-overflow
            // page advance for non-splittable / fits-fine children.
            // `break-inside: avoid` collapses to this path via
            // `avoid_inside_holds` above (it just suppresses the inline
            // split branch; remaining-strip overflow handling is
            // identical), and only for a box that fits a page. F83b: a
            // block wrapper that is kept whole (`avoid_keeps_whole`) takes
            // this path too, because the gate no longer enters it. A taller
            // inline root has gone through the line path above (F83); a
            // taller box without lines is cut into strips below.
            if cursor_y > 0.0 && cursor_y + child_h > strips.bottom(page_index) + clip_end_slack {
                page_index += 1;
                cursor_y = 0.0;
            }

            // Phase 4 PR 5 fix: include `layout.location.x` so the
            // child's left margin / padding offset within body is
            // captured. Pre-Phase-4 the fragmenter only fed
            // `slice_for_page` which doesn't read `frag.x`, so
            // `body_x` alone happened to be enough; the new
            // geometry-driven render path now consults `frag.x` for
            // every Block / Image / Paragraph and reverts to v2
            // drawing at x=0 without this. Matches the descendant
            // fragment shape on the line below.
            let frag_x = body_x + layout.location.x;

            // fulgur-sbw2: a child whose CSS-resolved height alone
            // exceeds `page_height_px` (e.g. `<div height:300vh>`)
            // must be sliced across pages. The recursion gate above
            // returns false for this shape because the child's *own*
            // children fit the strip (the gate measures descendant
            // overflow, not the parent's intrinsic height), so we
            // would otherwise emit a single oversized fragment on the
            // current page and stop. Emit one fragment per page strip
            // with page-local y — `PaginationGeometry::is_split()`
            // flips to `true` automatically once `fragments.len() > 1`,
            // and render.rs picks the per-slice height accordingly.
            // Descendants are recorded against the *first* slice only;
            // exact mid-element pagination of nested content is still
            // future work (see `record_subtree_descendants` notes).
            //
            // Tolerance: Stylo / Taffy round CSS-resolved `<length>`
            // values to integer CSS pixels in some cases — a 220pt
            // spacer (= 293.333… px) is reported back as `child_h =
            // 294` while `page_height_px` is computed without that
            // round-trip (= 293.33334). The literal `>` then trips
            // by ~0.67 px and the spacer is wrongly sliced into two
            // pages (gcpm_snapshot tests regress: spacer + page-break
            // pair becomes 2 + 1 instead of 1 + 1 per section). One
            // CSS pixel of slack absorbs the quantization without
            // letting truly oversized content (`300vh ≈ 880 px on a
            // 293-px strip`) slip through.
            //
            // Atomic children opt out: a transformed subtree paints
            // as a single atomic box (CSS Transforms §6.1) — it must
            // never split across pages because the rotation /
            // skew / matrix is applied to a single shape, not
            // per-slice. `contain: size` is NOT excluded: a
            // `<div contain:size height:350vh>` still spans four
            // pages visually (WPT `monolithic-overflow-022-print`),
            // it's only the descendant content that's atomic.
            let has_transform = child
                .primary_styles()
                .is_some_and(|s| !s.get_box().transform.0.is_empty());
            if !has_transform && child_h > self.strips.fresh() + 1.0 {
                let first_slice_h = (strips.bottom(page_index) - cursor_y).min(child_h);
                self.geometry
                    .entry(child_id)
                    .or_default()
                    .fragments
                    .push(Fragment {
                        page_index,
                        x: frag_x.as_px(),
                        y: cursor_y.as_px(),
                        width: child_w.as_px(),
                        height: first_slice_h.as_px(),
                    });
                record_subtree_descendants(
                    &mut self.geometry,
                    self.doc,
                    child_id,
                    page_index,
                    cursor_y,
                    frag_x,
                    0,
                );
                let mut remaining = child_h - first_slice_h;
                let mut last_slice_h = first_slice_h;
                // fulgur-ezst: a CHILDLESS block whose slicing would exceed
                // the cap is a pathological amplifier — `<div
                // style="height:99999999px">` is a web-only spacer/overflow
                // idiom that prints nothing but blank pages. Collapse it to
                // its single first slice: emit only that slice AND bound the
                // space it occupies (resume following content on the next
                // page) so the document does not balloon. Background / border
                // presence does NOT gate this — nobody authors a
                // >MAX_PAGES-tall filled band on purpose. A content-bearing
                // block, or a childless band that fits within the cap, is not
                // collapsed and takes the truncate-and-warn path below
                // unchanged.
                //
                // fulgur-c8re (security): a replaced element (`<img>` /
                // `<svg>`) is NOT special-cased out of this collapse —
                // painting or not. It only reaches this branch when it is
                // taller than `MAX_PAGES` pages (~10M px), a range no
                // legitimate single image occupies, so clipping it to one page
                // loses nothing real — even a *resolved* image, because a 1×1
                // bitmap stretched with `height:99999999px` is the same
                // amplifier as an unresolved `src`. Gating on tag name alone
                // (the removed `is_replaced_content`) let such a node amplify a
                // few bytes of HTML into ~`MAX_PAGES` blank pages (a validated
                // high-severity DoS).
                let collapse_childless = self.strips.fresh() > 0.0
                    && (remaining / self.strips.fresh()).ceil() > crate::MAX_PAGES as f32
                    && !subtree_has_rendered_content(self.doc, child_id, 0);
                if collapse_childless {
                    // fulgur-c8re (Codex P1 on PR #575): bound the OCCUPIED
                    // SPACE, not just the fragment pushes. `implied_page_count`
                    // reads the max fragment index across ALL nodes, so if the
                    // slice loop advanced `page_index` to `MAX_PAGES` a trailing
                    // in-flow sibling (`<div huge></div><p>after</p>`) would be
                    // stranded on a deep page and re-inflate the PDF to
                    // ~`MAX_PAGES` blank pages. The first slice already emitted
                    // above fills the current strip, so resume following content
                    // on the next page.
                    log::warn!(
                        "pagination: collapsed a childless block of height \
                         {child_h}px (slicing would exceed the {}-page limit) \
                         to a single page (fulgur-ezst)",
                        crate::MAX_PAGES,
                    );
                    page_index += 1;
                    cursor_y = 0.0;
                } else {
                    // fulgur-2m6w: cap the per-page-strip slicing at
                    // `MAX_PAGES`. `child_h` is attacker-controlled CSS
                    // (`height` / `vh`), so without this bound a few bytes of
                    // HTML (`<div style="height:99999999px">`) generate ~10^5
                    // fragments — and a non-finite height would never reduce
                    // `remaining`, looping forever. The `page_index` ceiling is
                    // load-bearing on its own (it stops the loop even when
                    // `remaining` is `+inf`); content past the cap is truncated.
                    while remaining > 0.0 && page_index < crate::MAX_PAGES {
                        page_index += 1;
                        last_slice_h = remaining.min(self.strips.fresh());
                        self.geometry
                            .entry(child_id)
                            .or_default()
                            .fragments
                            .push(Fragment {
                                page_index,
                                x: frag_x.as_px(),
                                y: 0.0_f32.as_px(),
                                width: child_w.as_px(),
                                height: last_slice_h.as_px(),
                            });
                        remaining -= last_slice_h;
                    }
                    if remaining > 0.0 {
                        log::warn!(
                            "pagination: block height {child_h}px exceeds the \
                             {}-page limit; truncating remaining content to \
                             bound rendering (fulgur-2m6w)",
                            crate::MAX_PAGES,
                        );
                    }
                    cursor_y = if child_h - first_slice_h > 0.0 {
                        last_slice_h
                    } else {
                        cursor_y + first_slice_h
                    };
                }
                emitted += 1;
                prev_bottom_y_in_body = this_top_in_body + child_h;
                if let Some((row_page, row_end, row_bottom)) = parallel_row {
                    if page_index == row_page {
                        cursor_y = cursor_y.max(row_end);
                    }
                    prev_bottom_y_in_body = prev_bottom_y_in_body.max(row_bottom);
                }
                last_end = Some((page_index, cursor_y));
                if !is_float {
                    prev_used_page = Some(used_end.clone());
                }
                if matches!(
                    break_props.break_after,
                    Some(crate::draw_primitives::BreakAfter::Page)
                ) {
                    page_index += 1;
                    cursor_y = 0.0;
                }
                continue;
            }

            let frag = Fragment {
                page_index,
                x: frag_x.as_px(),
                y: cursor_y.as_px(),
                width: child_w.as_px(),
                height: child_h.as_px(),
            };
            self.geometry
                .entry(child_id)
                .or_default()
                .fragments
                .push(frag);

            // fulgur-s67g Phase 2.5: descend into the child's subtree
            // and record per-node fragments for every visible
            // descendant. The collect_*_states walks expect coverage of
            // nested DOM elements so bookmark / counter / string-set
            // markers attached e.g. to an `h2` inside a wrapper `<div>`
            // appear in geometry too.
            //
            // The descendant fragments live on the same page as
            // their ancestor — exact mid-element split inside a
            // body child is still future work. Y / width / height
            // come from the descendant's `final_layout` and are
            // mainly informational; the collectors that consume
            // this geometry today read only `page_index`.
            record_subtree_descendants(
                &mut self.geometry,
                self.doc,
                child_id,
                page_index,
                cursor_y,
                frag_x,
                0,
            );

            let placed_y = cursor_y;
            cursor_y += child_h;
            emitted += 1;
            prev_bottom_y_in_body = this_top_in_body + child_h;
            if let Some((row_page, row_end, row_bottom)) = parallel_row {
                if page_index == row_page {
                    cursor_y = cursor_y.max(row_end);
                }
                prev_bottom_y_in_body = prev_bottom_y_in_body.max(row_bottom);
            }
            last_end = Some((page_index, cursor_y));
            prev_plain = Some((this_top_in_body, page_index, placed_y, cursor_y));
            if !is_float {
                prev_used_page = Some(used_end.clone());
            }

            // `break-after: page` forces a page boundary after the
            // child. A trailing break on the last in-flow child does
            // emit an empty trailing page in CSS, but the fragmenter's
            // observable signal (page_count) treats this as "advance
            // cursor"; the next iteration's emit-or-skip handles
            // whether the page is materialised.
            if matches!(
                break_props.break_after,
                Some(crate::draw_primitives::BreakAfter::Page)
            ) {
                page_index += 1;
                cursor_y = 0.0;
            }
        }

        // F37: body's own box on each page. It ends below its last child by what body has under it (padding,
        // border, an inner margin: `body_h - prev_bottom_y_in_body`, as the cursor includes the gaps between
        // children); a body without placed children is its own height.
        let end = match last_end {
            Some((page, y)) => (
                page,
                y + (body_layout.size.height - prev_bottom_y_in_body).max(0.0),
            ),
            None => (0, body_layout.size.height),
        };
        let slices = body_box_slices(self.strips, body_x, body_w, body_layout.size.height, end);
        self.geometry.entry(body_id).or_default().box_slices = slices;

        emitted
    }
}

/// fulgur-ezst: true if `parent_id`'s subtree renders any VISIBLE box (a
/// descendant with positive area — both width and height > 0). Used to
/// classify a pathologically tall block as "childless" (collapsible): the
/// collapse is safe only when nothing visible would be lost.
///
/// Walks children like `record_subtree_descendants` (the `layout_children`
/// preference, short-circuiting on the first hit), but the emptiness test
/// differs deliberately: a descendant with zero AREA (EITHER dimension
/// <= 0) paints nothing, so it does not count as content — instead we
/// recurse into it, because with `overflow: visible` a zero-height /
/// zero-width box can still host visible descendants that overflow it.
/// (This is why the test is `||`, not `&&`: an empty block child lays out
/// width>0 / height==0, and `&&` would wrongly count it as content and
/// disable the collapse — a trivial DoS bypass,
/// `<div style="height:99999999px"><div></div></div>`; Codex P2 on PR #553.)
/// `record_subtree_descendants` still *records* such zero-area boxes (their
/// node ids may carry counters / ids), which is a separate question from
/// "does this paint anything". Conservative on depth: a subtree deeper than
/// `MAX_DOM_DEPTH` reads as no content, matching `record_subtree_descendants`
/// (which also stops recording there, so such content renders blank anyway).
fn subtree_has_rendered_content(doc: &BaseDocument, parent_id: usize, depth: usize) -> bool {
    if depth >= crate::MAX_DOM_DEPTH {
        return false;
    }
    let Some(parent) = doc.get_node(parent_id) else {
        return false;
    };
    let layout_children_borrow = parent.layout_children.borrow();
    let walk_children: &[usize] = layout_children_borrow
        .as_deref()
        .filter(|v| !v.is_empty())
        .unwrap_or(&parent.children);
    for &child_id in walk_children {
        let Some(child) = doc.get_node(child_id) else {
            continue;
        };
        let layout = child.final_layout;
        // `visibility: hidden` / `collapse` occupies layout space but paints
        // nothing (conversion skips its paint), so it does not count as
        // content — a `<div style="visibility:hidden">` must not defeat the
        // collapse (Codex P2 on PR #553).
        let invisible = {
            use style::properties::longhands::visibility::computed_value::T as Visibility;
            child
                .primary_styles()
                .is_some_and(|s| s.clone_visibility() != Visibility::Visible)
        };
        if invisible || layout.size.height <= 0.0 || layout.size.width <= 0.0 {
            // An invisible or zero-area box paints nothing itself, but may
            // host visible descendants — overflowing it (overflow: visible)
            // or flipping `visibility` back to `visible` — so recurse.
            if subtree_has_rendered_content(doc, child_id, depth + 1) {
                return true;
            }
            continue;
        }
        return true;
    }
    false
}

/// fulgur-s67g Phase 2.5: recursively record fragments for every
/// visible descendant of a body-direct child, attaching them to the
/// same `page_index` as the ancestor.
///
/// `parent_page_y` is the parent's body-relative y position on the
/// current page strip; `parent_x_in_body` is the parent's x position
/// (already pre-resolved against `body_x`). For each descendant, the
/// recorded fragment uses absolute body-relative coordinates
/// computed by adding the descendant's `final_layout.location` to
/// the parent's frame.
///
/// Skips zero-size descendants and bails at
/// [`crate::MAX_DOM_DEPTH`] to keep recursion bounded against
/// adversarial input.
///
/// Mid-element split inside a body child (a deeply nested element
/// crossing the page boundary that the parent itself did not split
/// at) is **not** modelled here — descendants land on the same page
/// as their ancestor. Closing this "block-level only" gap requires the
/// full per-strip layout pass that future fragmenter work will
/// introduce.
fn record_subtree_descendants(
    geometry: &mut PaginationGeometryTable,
    doc: &BaseDocument,
    parent_id: usize,
    page_index: u32,
    parent_page_y: f32,
    parent_x_in_body: f32,
    depth: usize,
) {
    if depth >= crate::MAX_DOM_DEPTH {
        return;
    }
    let Some(parent) = doc.get_node(parent_id) else {
        return;
    };
    // Prefer Blitz's `layout_children` over the raw DOM `children` when
    // it's been computed: when a block container has mixed
    // block-level and inline-level children, Stylo synthesizes
    // anonymous block wrappers around inline-level siblings (CSS 2.1
    // §9.2.1.1). Those wrappers are real `Node` instances with their
    // own `node_id` and Taffy layout, but they live ONLY in
    // `layout_children` — the original `children` list still points
    // at the underlying inline elements (e.g. a `<span
    // display:inline-block>`).
    //
    // Without this preference v2 silently drops the inline-level
    // span: extract assigns the inner paragraph's `node_id` to the
    // anonymous wrapper (because Blitz's `is_inline_root()` flag sits
    // on the wrapper), but the fragmenter — walking `children` —
    // never visits the wrapper, so geometry has no fragment for that
    // node_id and `dispatch_fragment` skips the paragraph entirely.
    // (fulgur-bq6i: review_card_inline_block.html lost its
    // "OK Approved" rounded badge for this exact reason.)
    let layout_children_borrow = parent.layout_children.borrow();
    let walk_children: &[usize] = layout_children_borrow
        .as_deref()
        .filter(|v| !v.is_empty())
        .unwrap_or(&parent.children);
    for &child_id in walk_children {
        record_descendant(
            geometry,
            doc,
            child_id,
            page_index,
            parent_page_y,
            parent_x_in_body,
            depth,
        );
    }
}

/// One step of [`record_subtree_descendants`]: record `child_id` (unless it is a
/// zero-size container, which only passes through) and everything below it on
/// `page_index`. `depth` is the depth of the parent.
fn record_descendant(
    geometry: &mut PaginationGeometryTable,
    doc: &BaseDocument,
    child_id: usize,
    page_index: u32,
    parent_page_y: f32,
    parent_x_in_body: f32,
    depth: usize,
) {
    let Some(child) = doc.get_node(child_id) else {
        return;
    };
    let layout = child.final_layout;
    let h = layout.size.height;
    let w = layout.size.width;
    // Phase 4 PR 5: zero-size containers (`<tbody>`, `<tr>`,
    // anonymous boxes) carry no paint payload but DO host visible
    // descendants (e.g. table cells) that v2 needs in geometry.
    // Skipping them entirely leaves cells out of `geometry`; the
    // dispatcher then never finds the cell node_ids and v2 emits
    // a blank table. Recurse without recording when h/w are
    // both zero so the descendant cells still register.
    if h <= 0.0 && w <= 0.0 {
        record_subtree_descendants(
            geometry,
            doc,
            child_id,
            page_index,
            parent_page_y + layout.location.y,
            parent_x_in_body + layout.location.x,
            depth + 1,
        );
        return;
    }
    let child_x = parent_x_in_body + layout.location.x;
    let child_y = parent_page_y + layout.location.y;
    geometry
        .entry(child_id)
        .or_default()
        .fragments
        .push(Fragment {
            page_index,
            x: child_x.as_px(),
            y: child_y.as_px(),
            width: w.as_px(),
            height: h.as_px(),
        });
    record_subtree_descendants(
        geometry,
        doc,
        child_id,
        page_index,
        child_y,
        child_x,
        depth + 1,
    );
}

/// fulgur-7hf5 (Phase 3.1.5c): pre-flight check for the recursion
/// gate — true if walking `parent_id`'s direct children would cross
/// a page boundary at `available_h`.
///
/// Cheaper-than-real `fragment_block_subtree` simulator: same gap /
/// OOF / whitespace skips, but no fragment emission. Returns `true`
/// on the first overflow detected. Lets the caller decide "should I
/// recurse here?" without paying the cost of recursion when recursion
/// would not actually split — distinguishing "recurse and split"
/// (within-child) from "push child whole / emit whole" (at-index or
/// no split).
///
/// `available_h` is the strip height left below the parent's entry
/// cursor on the current page. `page_h_px` lets the simulator detect
/// grandchildren taller than a full page (which would themselves
/// recurse and therefore split, regardless of `available_h`).
fn would_split_block_subtree(
    doc: &BaseDocument,
    parent_id: usize,
    available_h: f32,
    page_h_px: f32,
    depth: usize,
) -> bool {
    if depth >= crate::MAX_DOM_DEPTH {
        return false;
    }
    let Some(parent) = doc.get_node(parent_id) else {
        return false;
    };
    let mut cursor: f32 = 0.0;
    let mut prev_bottom: f32 = 0.0;
    // fulgur-yb27: walk `layout_children` so anonymous block wrappers
    // Stylo synthesizes around inline-level siblings (CSS 2.1
    // §9.2.1.1) participate in the cumulative-overflow simulation.
    // Mirrors the `fragment_block_subtree` walker switch above —
    // without this, a block whose tail anon block wrapper would
    // overflow is missed by the preflight, the recursion gate
    // returns false, and the parent falls back to a single
    // oversize fragment.
    let layout_children_borrow = parent.layout_children.borrow();
    let walk_children: Vec<usize> = layout_children_borrow
        .as_deref()
        .filter(|v| !v.is_empty())
        .map(|v| v.to_vec())
        .unwrap_or_else(|| parent.children.clone());
    drop(layout_children_borrow);
    for &child_id in &walk_children {
        let Some(child) = doc.get_node(child_id) else {
            continue;
        };
        if let Some(text) = child.text_data()
            && text.content.chars().all(char::is_whitespace)
        {
            continue;
        }
        {
            use ::style::properties::longhands::position::computed_value::T as Pos;
            let is_oof = child.primary_styles().is_some_and(|s| {
                matches!(s.get_box().clone_position(), Pos::Absolute | Pos::Fixed)
            });
            if is_oof {
                continue;
            }
        }
        let layout = child.final_layout;
        let h = layout.size.height;
        if h <= 0.0 {
            continue;
        }
        let this_top = layout.location.y;
        let gap = (this_top - prev_bottom).max(0.0);
        cursor += gap;
        if cursor + h > available_h {
            return true;
        }
        if h > page_h_px {
            // Grandchild itself oversized → would recurse → would
            // split, regardless of `available_h` budget.
            return true;
        }
        cursor += h;
        prev_bottom = this_top + h;
    }
    false
}

/// F28: block-size from the top border edge of `node_id` to the first place
/// the fragmenter can break inside it, i.e. how much of the box has to fit on
/// a page strip for entering it there to make progress (CSS Fragmentation 3
/// §4.1, §4.4).
///
/// - A gap between the content edge and the first in-flow child (a margin
///   that did not collapse through) is a class C break point (§4.1), so the
///   answer is the first child's offset. The node's own border and padding are
///   not that gap.
/// - Without a gap the only break point before the first row is the class A
///   point in front of the box itself, so the box needs its first *row* to fit:
///   the maximum over the children that share the first child's `y` (cells of a
///   table row, items of a flex / grid row) of `y + leading_block_size(item)`.
/// - A box without in-flow children with a height (a leaf, an inline root, a
///   multicol container) is not entered by the fragmenter and counts whole.
///
/// - A paragraph taller than a page (`page_height_px`) in plain block flow is
///   split between its line boxes (F30), so it counts its top inset and its first
///   [`ORPHANS`] lines, not its height.
///
/// - F83b: a first-row child in block flow whose `break-inside: avoid` holds
///   ([`avoid_inside_holds`]) is kept whole by the fragmenter, so it counts the piece of it before its first forced
///   break ([`avoid_piece_size`]), not its own first row. Without this a parent whose first row is such a box
///   would be entered at a page top it cannot use, and the box would overflow it. The node's own `avoid` is for the
///   caller to decide: it knows whether anything precedes the node. `column_styles` is `None` where
///   `break-inside` does not apply (table cells, F83d). `above` is the offset of this node's top below the top of the
///   box the caller asks about: an avoid box that would not fit a page together with that inset above it is not
///   counted whole (the chain around it cannot be kept together, and it would only report a first row taller than a
///   page, so F28 would leave its top inset behind on the old page).
///
/// The result never exceeds the node's own height. It walks the same children
/// (`layout_children` first, whitespace / out-of-flow / empty skipped) as
/// [`would_split_block_subtree`] and follows only the first row, so it is cheap.
fn leading_block_size(
    doc: &BaseDocument,
    node_id: usize,
    column_styles: Option<&crate::column_css::ColumnStyleTable>,
    above: f32,
    depth: usize,
    page_height_px: f32,
) -> f32 {
    let Some(node) = doc.get_node(node_id) else {
        return 0.0;
    };
    let own_h = if node.final_layout.size.height.is_finite() {
        node.final_layout.size.height.max(0.0)
    } else {
        0.0
    };
    if depth >= crate::MAX_DOM_DEPTH || crate::blitz_adapter::is_multicol_container(node) {
        return own_h;
    }
    let layout_children_borrow = node.layout_children.borrow();
    let walk_children: &[usize] = layout_children_borrow
        .as_deref()
        .filter(|v| !v.is_empty())
        .unwrap_or(&node.children);
    let avoid_applies = column_styles.is_some() && is_block_flow_container(doc, node);
    // F83d: a row of a table that `avoid` keeps together counts whole, like an avoid box in block flow.
    let kept_rows = if column_styles.is_some() && is_table_box(node) {
        avoid_rows(doc, node, walk_children, column_styles, page_height_px)
    } else {
        BTreeMap::new()
    };
    let mut first_row_top: Option<f32> = None;
    let mut leading: f32 = 0.0;
    for &child_id in walk_children {
        let Some(child) = doc.get_node(child_id) else {
            continue;
        };
        if let Some(text) = child.text_data()
            && text.content.chars().all(char::is_whitespace)
        {
            continue;
        }
        {
            use ::style::properties::longhands::position::computed_value::T as Pos;
            let is_oof = child.primary_styles().is_some_and(|s| {
                matches!(s.get_box().clone_position(), Pos::Absolute | Pos::Fixed)
            });
            if is_oof {
                continue;
            }
        }
        let layout = child.final_layout;
        if !layout.size.height.is_finite() || layout.size.height <= 0.0 {
            continue;
        }
        let top = layout.location.y;
        match first_row_top {
            None => {
                // An inline root that holds a box (an `<img>`) and has two lines or more: the first thing the
                // fragmenter can keep apart from the rest is its first lines, as for a cut paragraph (`LineSplit::
                // leading`), not the bottom of the first box, which may sit on a later line (a text line, then the
                // image) or on a line shorter than its strut. It was `top + the box`: a cell of `text<br><img>` near
                // the bottom of a page asked for the text and the image, did not fit, and moved whole where Chrome
                // keeps its first two lines (html-to-pdf F183, plan 7.7).
                if node.flags.is_inline_root()
                    && let Some(split) = abspos_line_split(node)
                {
                    return split.leading().min(own_h);
                }
                // The gap is measured from the content edge: the node's own
                // border and padding are not a gap, they belong to its box.
                let content_top = node.final_layout.border.top + node.final_layout.padding.top;
                // Not for the children of an inline root: they are inline-level boxes (an `<img>`, an `inline-block`)
                // placed by line layout, and where the line puts one says nothing about a margin. A `border-collapse:
                // collapse` cell with a 1 px border lays its first line 1 px below the `border + padding` its layout
                // reports (the cell is 0 + 8 px at the top, the image at 9), and a box shorter than the strut's ascent
                // sits below the top of its line: both were read as a gap and the box was entered to break in front of
                // its first child at the bottom of a page, instead of moving whole (html-to-pdf F183, plan 7.4).
                if !node.flags.is_inline_root() && top - content_top > 0.5 {
                    // Class C break point: the gap in front of the first child.
                    return top.min(own_h);
                }
                first_row_top = Some(top);
            }
            // Later children that are not level with the first one start a
            // later row.
            Some(row_top) if top > row_top + 0.5 => break,
            Some(_) => {}
        }
        // F30: a paragraph taller than a page is split between its line boxes,
        // so only its inset and ORPHANS lines have to fit, not all of it.
        // F30f-b: so is a paragraph that a clip box cuts (`hidden_tail`): the lines it hides are not placed, and the
        // clip box that holds it is fragmented in place.
        let child_leading = match line_split_candidate(doc, node, child) {
            Some(split)
                if split.inset_top + split.span() > page_height_px + 1.0
                    || split.hidden_tail > 0 =>
            {
                split.leading().min(layout.size.height)
            }
            _ if kept_rows.contains_key(&row_key(top))
                && above + top + layout.size.height <= page_height_px + 1.0 =>
            {
                layout.size.height
            }
            _ if avoid_applies
                && has_avoid(column_styles, child_id)
                && avoid_inside_holds(
                    true,
                    avoid_measure(doc, child_id, child, column_styles),
                    page_height_px,
                )
                && above + top + avoid_piece_size(doc, child_id, column_styles, depth + 1)
                    <= page_height_px + 1.0 =>
            {
                avoid_piece_size(doc, child_id, column_styles, depth + 1).min(layout.size.height)
            }
            _ => leading_block_size(
                doc,
                child_id,
                column_styles,
                above + top,
                depth + 1,
                page_height_px,
            ),
        };
        leading = leading.max(top + child_leading);
    }
    if first_row_top.is_none() {
        return own_h;
    }
    leading.min(own_h)
}

/// F144: [`has_forced_break_below`] at depth 0, answered from `facts` when the pass built them. Under
/// `debug_assertions` the table is checked against the walk on every call, so the whole test suite runs the
/// oracle; with `None` (the abspos flow, hand-built trees in tests) it is the walk.
fn forced_break_below(
    facts: Option<&SubtreeFacts>,
    doc: &BaseDocument,
    column_styles: Option<&crate::column_css::ColumnStyleTable>,
    node_id: usize,
) -> bool {
    let Some(facts) = facts else {
        return has_forced_break_below(doc, node_id, column_styles, 0);
    };
    let answer = facts.forced_break_below(node_id);
    debug_assert_eq!(
        answer,
        has_forced_break_below(doc, node_id, column_styles, 0),
        "SubtreeFacts: forced break below node {node_id}"
    );
    answer
}

/// F144: [`has_page_name_change_below`] at depth 0, answered from `facts`; see [`forced_break_below`].
fn page_name_change_below(
    facts: Option<&SubtreeFacts>,
    doc: &BaseDocument,
    used_page_names: Option<&crate::blitz_adapter::UsedPageNameTable>,
    node_id: usize,
) -> bool {
    let Some(facts) = facts else {
        return has_page_name_change_below(doc, node_id, used_page_names, 0);
    };
    let answer = facts.page_name_change_below(node_id);
    debug_assert_eq!(
        answer,
        has_page_name_change_below(doc, node_id, used_page_names, 0),
        "SubtreeFacts: page-name change below node {node_id}"
    );
    answer
}

/// fulgur-a36m (Phase 3.1.5b): true if any descendant of `node_id`
/// declares `break-before: page` or `break-after: page` in
/// `column_styles`. Walks the entire DOM subtree, bails at
/// [`crate::MAX_DOM_DEPTH`].
///
/// Detects forced page breaks anywhere below `node_id`, working on
/// Blitz nodes via the column-style side-table. Used by
/// `fragment_pagination_root` and `fragment_block_subtree` to decide
/// whether a body-direct (or nested) child needs to be entered for
/// break recursion even when it fits the current page strip whole.
fn has_forced_break_below(
    doc: &BaseDocument,
    node_id: usize,
    column_styles: Option<&crate::column_css::ColumnStyleTable>,
    depth: usize,
) -> bool {
    if depth >= crate::MAX_DOM_DEPTH {
        return false;
    }
    let Some(node) = doc.get_node(node_id) else {
        return false;
    };
    for &child_id in &node.children {
        if let Some(props) = column_styles.and_then(|t| t.get(&child_id))
            && (matches!(
                props.break_before,
                Some(crate::draw_primitives::BreakBefore::Page)
            ) || matches!(
                props.break_after,
                Some(crate::draw_primitives::BreakAfter::Page)
            ))
        {
            return true;
        }
        if has_forced_break_below(doc, child_id, column_styles, depth + 1) {
            return true;
        }
    }
    false
}

/// fulgur-uebl: true if [`has_page_name_change_below`] does not look inside `node` at all. An atomic inline container
/// (`inline-block`, `inline-flex`, ...) is fully opaque: its internal block flow does not paginate independently from
/// the parent line box. A node in an orthogonal flow to its own parent is atomic from the outer flow's perspective
/// (CSS Writing Modes 4 §9), so a call made directly with it as the target treats its subtree as opaque too: the
/// recursion gate must not enter it, which would interact with Taffy's orthogonal-flow sizing and produce layout
/// drift not present in the whole-emit baseline.
///
/// Shared with [`subtree_facts`], so that the table and the walk cannot drift apart on these predicates.
fn page_name_walk_is_opaque(doc: &BaseDocument, node: &blitz_dom::Node) -> bool {
    if crate::blitz_adapter::is_atomic_inline_container_node(node) {
        return true;
    }
    if let Some(gp_id) = node.parent
        && let Some(gp) = doc.get_node(gp_id)
        && crate::blitz_adapter::is_orthogonal_to_parent(gp, node)
    {
        return true;
    }
    false
}

/// fulgur-uebl: the child of `node` that [`has_page_name_change_below`] compares and recurses into, or `None` for one it
/// skips: a missing node, whitespace-only text, a non-element, a child orthogonal to `node` (fully atomic from this
/// node's perspective, CSS Writing Modes 4 §9), an absolutely positioned or fixed child, and a float (CSS 2.1 §9.5) —
/// the same filters as the fragmenter loop, so the predicate matches exactly what `fragment_block_subtree` would
/// compare. Shared with [`subtree_facts`].
fn page_name_walk_child<'a>(
    doc: &'a BaseDocument,
    node: &blitz_dom::Node,
    child_id: usize,
) -> Option<&'a blitz_dom::Node> {
    let child = doc.get_node(child_id)?;
    if let Some(text) = child.text_data()
        && text.content.chars().all(char::is_whitespace)
    {
        return None;
    }
    child.element_data()?;
    if crate::blitz_adapter::is_orthogonal_to_parent(node, child) {
        return None;
    }
    {
        use ::style::properties::longhands::position::computed_value::T as Pos;
        let is_out_of_flow = child
            .primary_styles()
            .is_some_and(|s| matches!(s.get_box().clone_position(), Pos::Absolute | Pos::Fixed));
        if is_out_of_flow {
            return None;
        }
    }
    if crate::blitz_adapter::node_is_floating(child) {
        return None;
    }
    Some(child)
}

/// fulgur-uebl: true if any sibling pair inside `node_id`'s subtree
/// has different used page-names. Used as a recursion gate so that
/// `fragment_block_subtree` is entered for subtrees that fit the page
/// strip but contain implicit page-name forced breaks (CSS Page 3
/// §5.3). Walking the whole subtree is acceptable here — the
/// `column_styles` / `used_page_names` tables are sparse, and the bail
/// at [`crate::MAX_DOM_DEPTH`] matches `has_forced_break_below`.
fn has_page_name_change_below(
    doc: &BaseDocument,
    node_id: usize,
    used_page_names: Option<&crate::blitz_adapter::UsedPageNameTable>,
    depth: usize,
) -> bool {
    if depth >= crate::MAX_DOM_DEPTH {
        return false;
    }
    let Some(table) = used_page_names else {
        return false;
    };
    let Some(node) = doc.get_node(node_id) else {
        return false;
    };
    if page_name_walk_is_opaque(doc, node) {
        return false;
    }
    // Flex / grid containers suppress sibling comparison among their
    // direct items (CSS Page 3 / CSS Fragmentation 3 — flex / grid
    // items are not class A break points). But page-name forced breaks
    // inside an item's own BFC still apply, so we must keep recursing
    // into each item — only the direct-children comparison is gated.
    let suppress_direct_compare = crate::blitz_adapter::is_flex_or_grid_container_node(node);
    let mut prev_used: Option<Option<String>> = None;
    for &child_id in &node.children {
        if page_name_walk_child(doc, node, child_id).is_none() {
            continue;
        }
        let (child_start, child_end) = table.get(&child_id).cloned().unwrap_or((None, None));
        if !suppress_direct_compare && prev_used.as_ref().is_some_and(|p| *p != child_start) {
            return true;
        }
        if !suppress_direct_compare {
            prev_used = Some(child_end);
        }
        // Always recurse: even when direct sibling comparison is
        // suppressed (flex / grid container), descendants in their
        // own BFC may still trigger an internal page-name break.
        if has_page_name_change_below(doc, child_id, used_page_names, depth + 1) {
            return true;
        }
    }
    false
}

/// fulgur-g9e3.1: split a block element across pages by walking its DOM
/// children and emitting per-page fragments for both the block itself
/// and its children.
///
/// For each in-flow child, if it does not fit in the remaining strip,
/// advance the page boundary and continue placing on a fresh strip.
/// Children with their own DOM children that are taller than a full page
/// recurse so the split walks all the way down to where overflow actually
/// resolves.
///
/// Per-page parent fragments capture the height consumed by children on
/// each page (`cursor - page_start_y`). The downstream collectors
/// (`collect_string_set_states` / `collect_counter_states` /
/// `collect_bookmark_entries`) consume the per-page snapshots produced
/// here.
///
/// fulgur-a36m (Phase 3.1.5b): also honours `break-before: page` /
/// `break-after: page` on direct children, and recurses into children
/// whose subtrees declare a forced break (`has_forced_break_below`)
/// so deeper nested breaks land on the right page.
///
/// In-place mid-element split (`cursor_y + child_h > page_h` with
/// `child_h <= page_h` and a CSS-set parent height that diverges from
/// children sum) still falls back to the pre-3.1 push-to-next-page
/// behaviour — that's `fulgur-7hf5` (Phase 3.1.5c).
///
/// Skips OOF / whitespace-text children, same convention as
/// `fragment_pagination_root`. Bails at [`crate::MAX_DOM_DEPTH`] —
/// any nodes below that depth go unrecorded (matches
/// `record_subtree_descendants`).
///
/// ## Known gaps deferred to `fulgur-a9qf` (Phase 3.1.5)
///
/// `fragment_block_subtree` does **not** mirror `fragment_pagination_root`
/// in three respects. None of these surface in the current test corpus
/// (`cargo test -p fulgur` 1111 / 0); each is tracked as a regression
/// scope-add on `fulgur-a9qf` (notes §5a / §5b / §5c) so they close
/// alongside in-place mid-element split:
///
/// - **Nested `position: running()` markers are not skipped here.** The
///   helper has no access to `running_store`, so a running marker that
///   sits inside an oversized subtree is treated as in-flow and
///   over-advances `cursor_y`. Body-level filtering is intact; only the
///   recursion path is affected.
/// - **Nested inline roots are not split at line edges.** When a tall
///   `<p>` (multi-line inline root) lives inside an oversized ancestor,
///   the recursion falls back to DOM-child block split rather than
///   calling `collect_inline_line_metrics` / `fragment_inline_root` like
///   the body-level walker does.
/// - **Multi-page recursive traversal does not emit per-page parent
///   fragments for intermediate pages.** When the recursive call
///   advances more than one page, only the first and last page get a
///   parent-`parent_id` fragment via the pre-recursion overflow close
///   and the trailing close at the end of this function. Counter /
///   string-set / bookmark ops attached to `parent_id` itself would
///   then miss the intermediate pages — the existing tests attach ops
///   to leaf children, so this stays masked until 3.1.5.
///
/// `top_floor` is the block-size reserved at the top of every continuation page
/// this call starts (F30 / F12: the repeated header of the table whose cell
/// `parent_id` is, so a cell that spans pages leaves room for it).
///
/// `tail_below` is the block-size the boxes around `parent_id` need below
/// it (F30: so that a paragraph split between lines leaves room for their
/// bottom padding and border on its last page).
///
/// Returns `(final_page_index, final_cursor_y, lines_split_below)`: the page
/// and y where the parent's last child finished, and whether a paragraph
/// below was split between lines (then the cursor is below the parent's
/// frame). The caller resumes its outer cursor from these values.
/// Row-level state for grid/flex parallel-sibling co-split (fulgur-ysms).
///
/// Saved once at the first cell of each row; subsequent cells in the same
/// row restore from `start_*` so their recursion begins at the same
/// (page, cursor) as the first cell did. After all cells in the row are
/// processed the outer cursor advances to `max_end_*`.
struct RowState {
    start_page: u32,
    start_cursor_y: f32,
    start_page_start_y: f32,
    start_page_taffy_origin: f32,
    max_end_page: u32,
    max_end_cursor_y: f32,
    /// Taffy `location.y` of the first cell in this row (reserved for future use).
    _row_top: f32,
    /// Max `location.y + height` seen across cells in this row.
    row_bottom: f32,
    /// Pages for which a parent fragment has already been emitted this row,
    /// to avoid duplicate entries when multiple cells cross the same boundary.
    emitted_parent_pages: BTreeSet<u32>,
    /// True when at least one cell in this row has crossed a page boundary via
    /// recursion. Only in this case do subsequent same-row cells need the full
    /// state-restore (co-split); non-recursion page advances are already handled
    /// by the existing `origin_pending_same_row` mechanism.
    crossed_by_recursion: bool,
}

/// The placement state of `fragment_block_subtree` at one point of a table row
/// (see [`SplitRow`]).
#[derive(Clone)]
struct RowCursor {
    page_index: u32,
    cursor_y: f32,
    page_start_y: f32,
    page_taffy_origin: f32,
    band: f32,
    pending_target_y: Option<f32>,
    pending_prev_bottom: Option<f32>,
    pending_same_row: Option<(f32, f32, f32)>,
}

/// F30: a table row one of whose cells continued on later pages. The cells after
/// it in the row are placed from where the row started (`start`) — they sit
/// beside it, on the row's first page — and what follows the row continues from
/// where the split cell ended (`end`).
struct SplitRow {
    top: f32,
    bottom: f32,
    start: RowCursor,
    end: RowCursor,
}

/// An in-flow child that takes part in the block-size of its parent: not
/// whitespace-only text, not absolutely positioned, with a finite height above 0.
fn is_flow_child_with_height(c: &blitz_dom::Node) -> bool {
    use ::style::properties::longhands::position::computed_value::T as Pos;
    let h = c.final_layout.size.height;
    let is_blank_text = c
        .text_data()
        .is_some_and(|t| t.content.chars().all(char::is_whitespace));
    let is_oof = c
        .primary_styles()
        .is_some_and(|s| matches!(s.get_box().clone_position(), Pos::Absolute | Pos::Fixed));
    !is_blank_text && !is_oof && h.is_finite() && h > 0.0
}

/// F30b: the margin below node `node_id` that ends up below the last thing in its
/// parent's content: its own bottom margin, and the margins of its last descendants
/// that collapse through its bottom edge (no bottom padding or border on the way).
/// Taffy reports each box's own margin only.
fn trailing_margin(doc: &BaseDocument, node_id: usize, depth: usize) -> f32 {
    let Some(node) = doc.get_node(node_id) else {
        return 0.0;
    };
    let own = node.final_layout.margin.bottom.max(0.0);
    if depth >= crate::MAX_DOM_DEPTH
        || node.final_layout.border.bottom > 0.0
        || node.final_layout.padding.bottom > 0.0
    {
        return own;
    }
    let layout_children = node.layout_children.borrow();
    let children: &[usize] = layout_children
        .as_deref()
        .filter(|v| !v.is_empty())
        .unwrap_or(&node.children);
    let below = children
        .iter()
        .rev()
        .copied()
        .find(|&id| doc.get_node(id).is_some_and(is_flow_child_with_height))
        .map_or(0.0, |id| trailing_margin(doc, id, depth + 1));
    own.max(below)
}

/// F83b: whether the height of `node` is made by its in-flow children and its own bottom padding and border (and the
/// bottom margin of its last child that ends up inside it): an `auto` height box, not one stretched by `height` /
/// `min-height`. The recursion gate does not enter a box whose children fit, so that a CSS height larger than the
/// children does not shrink its frame; a box like this one has no such height, so entering it is safe.
fn frame_is_content_sized(doc: &BaseDocument, node: &blitz_dom::Node) -> bool {
    let layout_children_borrow = node.layout_children.borrow();
    let walk_children: &[usize] = layout_children_borrow
        .as_deref()
        .filter(|v| !v.is_empty())
        .unwrap_or(&node.children);
    let mut bottom: f32 = 0.0;
    let mut last = None;
    for &id in walk_children {
        if let Some(c) = doc.get_node(id)
            && is_flow_child_with_height(c)
        {
            bottom = bottom.max(c.final_layout.location.y + c.final_layout.size.height);
            last = Some(id);
        }
    }
    let Some(last) = last else {
        return false;
    };
    let inset = node.final_layout.border.bottom + node.final_layout.padding.bottom;
    node.final_layout.size.height - bottom <= inset + trailing_margin(doc, last, 0) + 0.5
}

/// F30b: whether no two in-flow children of a flex / grid container overlap in
/// y, i.e. each one starts at or below the bottom of the one before: a column
/// flex container, a one-column grid, a single item. Such a container
/// fragments like block flow (css-flexbox-1 §12.1, css-grid-2 "Fragmenting
/// Grid Layout"); with two items side by side it does not.
fn items_stack_vertically(doc: &BaseDocument, children: &[usize]) -> bool {
    let mut spans: Vec<(f32, f32)> = children
        .iter()
        .filter_map(|&id| doc.get_node(id))
        .filter(|c| is_flow_child_with_height(c))
        .map(|c| {
            let l = &c.final_layout;
            (l.location.y, l.location.y + l.size.height)
        })
        .collect();
    spans.sort_by(|a, b| a.0.total_cmp(&b.0));
    spans.windows(2).all(|w| w[1].0 >= w[0].1 - 0.5)
}

#[allow(clippy::too_many_arguments)]
fn fragment_block_subtree(
    geometry: &mut PaginationGeometryTable,
    doc: &BaseDocument,
    column_styles: Option<&crate::column_css::ColumnStyleTable>,
    used_page_names: Option<&crate::blitz_adapter::UsedPageNameTable>,
    facts: Option<&SubtreeFacts>,
    multicol: Option<&crate::multicol_layout::MulticolGeometryTable>,
    parent_id: usize,
    parent_w: f32,
    parent_x_in_body: f32,
    page_in: u32,
    cursor_in: f32,
    strips: PageStrips,
    depth: usize,
    tail_below: f32,
    top_floor: f32,
    content_above: bool,
) -> (u32, f32, bool, f32) {
    if depth >= crate::MAX_DOM_DEPTH {
        // Bailed: emit a single whole-fragment for the parent at its
        // entry coordinates so geometry still has an entry for it.
        let h = doc
            .get_node(parent_id)
            .map(|n| n.final_layout.size.height)
            .unwrap_or(0.0);
        geometry
            .entry(parent_id)
            .or_default()
            .fragments
            .push(Fragment {
                page_index: page_in,
                x: parent_x_in_body.as_px(),
                y: cursor_in.as_px(),
                width: parent_w.as_px(),
                height: h.as_px(),
            });
        return (page_in, cursor_in + h, false, 0.0);
    }
    let Some(parent) = doc.get_node(parent_id) else {
        return (page_in, cursor_in, false, 0.0);
    };

    let mut page_index = page_in;
    let mut cursor_y = cursor_in;
    // Y on `page_index` where the parent's current-page fragment
    // starts. We close one parent fragment and start a new one each
    // time we cross a page boundary.
    let mut page_start_y = cursor_in;
    // fulgur-kv0r: parent-relative y of the first in-flow child on
    // the current page strip. Taffy's `layout.location.y` is in the
    // parent's full coordinate system (same value across page
    // splits); each child's page-local y becomes
    // `page_start_y + (this_top_in_parent - page_taffy_origin)`,
    // which gives:
    // - block siblings: sequential placement (same as cursor-advance)
    // - grid / flex parallel siblings: same y (Taffy reports same
    //   `location.y` for cards in the same row, so the offset
    //   collapses to the row's first y).
    let mut page_taffy_origin: f32 = 0.0;
    let mut origin_pending_target_y: Option<f32> = None;
    let mut origin_pending_same_row: Option<(f32, f32, f32)> = None;
    // F30: bottom (in parent coordinates) of a paragraph that was just split
    // between line boxes. The next sibling keeps the gap Taffy left below it
    // (its margin) instead of starting flush against the last line.
    let mut origin_pending_prev_bottom: Option<f32> = None;
    // F30: a paragraph below (this box's child or deeper) was split between
    // line boxes across pages. The box's frame then has to be closed below its
    // last child (its bottom padding and border), which the plain block split
    // leaves out, and the caller gets the cursor below the frame.
    let mut lines_split_below = false;
    // F30: the table row a cell of which just continued on later pages.
    let mut split_row: Option<SplitRow> = None;
    let parent_is_table = {
        use ::style::values::specified::box_::DisplayInside;
        parent
            .primary_styles()
            .is_some_and(|st| st.clone_display().inside() == DisplayInside::Table)
    };
    // Lowest bottom (parent coordinates) of an in-flow child with a height.
    let mut children_bottom: f32 = 0.0;
    // fulgur-uebl: tracks the previous in-flow sibling's used page-name
    // for implicit forced-break detection; see `fragment_pagination_root`
    // for the rationale and the outer-Option semantics.
    let mut prev_used_page: Option<Option<String>> = None;
    let mut row_state: Option<RowState> = None;
    // F12: a table whose first `table-header-group` can be repeated on
    // continuation pages. `band` is the block-size reserved for the repeated
    // header at the top of the current page strip (0 on the first strip and
    // on any strip without a repeat).
    let mut repeat: Option<HeaderRepeat> = repeating_table_header(doc, parent_id, strips.fresh());
    let mut band: f32 = 0.0;
    // fulgur-uebl: flex / grid containers establish a flex/grid
    // formatting context where children are not class A break points
    // (CSS Fragmentation 3 §3.2). The `page` property doesn't apply to
    // flex / grid items, so we suppress the implicit-forced-break
    // comparison among them. Atomic inline containers (`inline-block`,
    // `inline-flex`, `inline-grid`) are similarly opaque from a
    // pagination perspective — their internal block flow does not
    // paginate independently, so sibling comparison among their
    // children would just produce spurious breaks. Orthogonal-flow
    // containers (writing-mode different from their own parent) are
    // also treated atomically per CSS Writing Modes 4 §9. Inner
    // block-level descendants in their own BFC still get the
    // comparison via deeper recursion.
    let parent_is_orthogonal = parent
        .parent
        .and_then(|gp_id| doc.get_node(gp_id))
        .is_some_and(|gp| crate::blitz_adapter::is_orthogonal_to_parent(gp, parent));
    let allow_same_row_rebase = crate::blitz_adapter::is_flex_or_grid_container_node(parent);
    let suppress_page_check = allow_same_row_rebase
        || crate::blitz_adapter::is_atomic_inline_container_node(parent)
        || parent_is_orthogonal;
    // html-to-pdf F30f-b: in block flow a clip box (this one or an ancestor) hides what lies below its padding box, and
    // that gets no place on any page. `clip_limit` is the tightest such clip in this box's coordinates; `own_clip_height`
    // is this box's block size when this box is the clip box that is fragmented by it.
    let clip_flow =
        !suppress_page_check && !parent_is_table && is_block_flow_container(doc, parent);
    let clip_limit = clip_flow
        .then(|| tightest_clip_limit(doc, parent))
        .flatten();
    let own_clip_height = clip_flow
        .then(|| clip_block_limit(doc, parent))
        .flatten()
        .map(|_| parent.final_layout.size.height);

    // fulgur-yb27: prefer `layout_children` over raw `children` —
    // same rationale as `record_subtree_descendants` and
    // `fragment_pagination_root`. When a block container has mixed
    // block-level and inline-level children, Stylo synthesizes
    // anonymous block wrappers around the inline-level siblings (CSS
    // 2.1 §9.2.1.1). Those wrappers carry their own Taffy layout and
    // `node_id`, but they live ONLY in `layout_children`. Walking
    // raw `children` would re-visit the underlying inline runs
    // without their Taffy layout (so they'd land at the parent's
    // origin instead of after their predecessor block) and skip the
    // per-anon-block break decision the body-level walker honours
    // since fulgur-bq6i.
    //
    // Cross-page recursion correctness depends on fulgur-oc51's
    // parent-fragment push above — flipping this walk to
    // `layout_children` without that fix would lose the parent's
    // pre-recursion-page fragment in mo-006/008 (flex/grid + tall
    // monolithic + trailing inline text).
    let layout_children_borrow = parent.layout_children.borrow();
    let walk_children: Vec<usize> = layout_children_borrow
        .as_deref()
        .filter(|v| !v.is_empty())
        .map(|v| v.to_vec())
        .unwrap_or_else(|| parent.children.clone());
    drop(layout_children_borrow);
    // F83d: the rows of this table that `break-inside: avoid` keeps on one page.
    let kept_rows = if parent_is_table {
        avoid_rows(doc, parent, &walk_children, column_styles, strips.fresh())
    } else {
        BTreeMap::new()
    };
    // F30: the last in-flow child with a height: what is below it in this box
    // (padding, border) is `tail` for a paragraph in it.
    let last_flow_child = walk_children
        .iter()
        .rev()
        .copied()
        .find(|&id| doc.get_node(id).is_some_and(is_flow_child_with_height));
    // F30b: a single-column flex / grid box lays its items out like block flow,
    // so it is closed below a split item like a block (css-flexbox-1 §12.1).
    // Items of a row are level with each other and keep today's handling.
    let closes_like_block = !allow_same_row_rebase || items_stack_vertically(doc, &walk_children);
    for &child_id in &walk_children {
        let Some(child) = doc.get_node(child_id) else {
            continue;
        };
        // Whitespace-only text — same skip as `fragment_pagination_root`.
        if let Some(text) = child.text_data()
            && text.content.chars().all(char::is_whitespace)
        {
            continue;
        }
        // CSS 2.1 §10.6.4: out-of-flow children do not contribute to
        // their containing block's normal-flow height.
        {
            use ::style::properties::longhands::position::computed_value::T as Pos;
            let is_out_of_flow = child.primary_styles().is_some_and(|s| {
                matches!(s.get_box().clone_position(), Pos::Absolute | Pos::Fixed)
            });
            if is_out_of_flow {
                continue;
            }
        }
        let layout = child.final_layout;
        // fulgur-2m6w: same non-finite guard as `fragment_pagination_root`.
        // A nested child with a non-finite Taffy height (`+inf` / `NaN`, or
        // an `f32::MAX` height that overflows to `+inf` after one
        // `cursor_y += child_h`) would otherwise poison `cursor_y` /
        // `page_start_y` for the parent and every following sibling. Treat
        // it as zero height so it falls into the `child_h <= 0.0` branch.
        let child_h = if layout.size.height.is_finite() {
            layout.size.height
        } else {
            0.0
        };
        let child_w = if layout.size.width > 0.0 {
            layout.size.width
        } else {
            parent_w
        };

        // fulgur-a36m: read break-* props for this child once. Both
        // the zero-height and non-zero paths honour them.
        let break_props = column_styles
            .and_then(|t| t.get(&child_id))
            .cloned()
            .unwrap_or_default();
        // fulgur-uebl: detect page-name change against the previous
        // in-flow sibling and treat it as an implicit forced break.
        // Compare prev's `end` against this child's `start`. Floats are
        // out of normal flow (CSS 2.1 §9.5) and skipped here too.
        let is_float = crate::blitz_adapter::node_is_floating(child);
        let (used_start, used_end) = used_page_names
            .and_then(|t| t.get(&child_id).cloned())
            .unwrap_or((None, None));
        let page_name_changed = !suppress_page_check
            && !is_float
            && prev_used_page.as_ref().is_some_and(|p| *p != used_start);
        let break_before_page = matches!(
            break_props.break_before,
            Some(crate::draw_primitives::BreakBefore::Page)
        ) || page_name_changed;
        let break_after_page = matches!(
            break_props.break_after,
            Some(crate::draw_primitives::BreakAfter::Page)
        );

        // Compute Taffy parent-relative top early — both the zero-
        // height path below and the non-zero path further down use
        // it (and break-before / break-after rebases the
        // `page_taffy_origin` against it on page advance).
        let this_top_in_parent = layout.location.y;
        let is_header_cell = repeat.as_ref().is_some_and(|r| r.cells.contains(&child_id));
        // F28: only the first item of a flex / grid row may start a new page
        // on its own account; the others are level with it.
        let mut first_in_row = true;

        // fulgur-ysms: row-level co-split for flex/grid containers.
        // Must run BEFORE origin_pending_target_y is consumed so that
        // restoring page_taffy_origin is consistent.
        if allow_same_row_rebase {
            let same_row = row_state
                .as_ref()
                .map(|rs| this_top_in_parent < rs.row_bottom - 0.5);
            match same_row {
                Some(true) => {
                    first_in_row = false;
                    let rs = row_state.as_ref().unwrap();
                    if rs.crossed_by_recursion {
                        // Co-split: a previous cell in this row crossed a page
                        // boundary via recursion. Restore to the row-start state
                        // so this cell's recursion begins at the same (page,
                        // cursor) — both cells will independently fragment and
                        // their results are merged via max_end_*.
                        //
                        // Also clear origin_pending_* set by the previous cell's
                        // recursion: the restored page_taffy_origin is the one
                        // that puts THIS cell at the correct row-start y, and the
                        // previous cell's pending origin would incorrectly shift
                        // page_taffy_origin again.
                        page_index = rs.start_page;
                        cursor_y = rs.start_cursor_y;
                        page_start_y = rs.start_page_start_y;
                        page_taffy_origin = rs.start_page_taffy_origin;
                        origin_pending_target_y = None;
                        origin_pending_same_row = None;
                    }
                    // If crossed_by_recursion is false (e.g., a previous cell
                    // crossed via the non-recursion strip-overflow path), the
                    // existing origin_pending_same_row mechanism already handles
                    // correct placement — no additional restore is needed.
                }
                Some(false) => {
                    // New row: advance outer cursor to the max end reached
                    // across all cells in the previous row.
                    let rs = row_state.take().unwrap();
                    page_index = rs.max_end_page;
                    cursor_y = rs.max_end_cursor_y;
                    if rs.max_end_page > rs.start_page {
                        page_start_y = 0.0;
                        origin_pending_target_y = Some(rs.max_end_cursor_y);
                        origin_pending_same_row = None;
                    } else {
                        page_start_y = rs.start_page_start_y;
                    }
                }
                None => {}
            }
            if row_state.is_none() {
                // First cell in a new row: snapshot current state.
                row_state = Some(RowState {
                    start_page: page_index,
                    start_cursor_y: cursor_y,
                    start_page_start_y: page_start_y,
                    start_page_taffy_origin: page_taffy_origin,
                    max_end_page: page_index,
                    max_end_cursor_y: cursor_y,
                    _row_top: this_top_in_parent,
                    row_bottom: this_top_in_parent + child_h,
                    emitted_parent_pages: BTreeSet::new(),
                    crossed_by_recursion: false,
                });
            } else if let Some(ref mut rs) = row_state {
                rs.row_bottom = rs.row_bottom.max(this_top_in_parent + child_h);
            }
        }

        // F30: a cell in the row of a cell that continued on later pages is
        // placed from the row's start; the first thing past the row continues
        // from where that cell ended.
        if let Some(row) = split_row.as_ref() {
            let in_row =
                this_top_in_parent >= row.top - 0.5 && this_top_in_parent < row.bottom - 0.5;
            let at = if in_row {
                row.start.clone()
            } else {
                row.end.clone()
            };
            if !in_row {
                split_row = None;
            }
            page_index = at.page_index;
            cursor_y = at.cursor_y;
            page_start_y = at.page_start_y;
            page_taffy_origin = at.page_taffy_origin;
            band = at.band;
            origin_pending_target_y = at.pending_target_y;
            origin_pending_prev_bottom = at.pending_prev_bottom;
            origin_pending_same_row = at.pending_same_row;
        }
        let pending_prev_bottom = origin_pending_prev_bottom.take();
        if let Some(mut target_y) = origin_pending_target_y.take() {
            if let Some((row_top, row_bottom, same_row_y)) = origin_pending_same_row.take()
                && this_top_in_parent < row_bottom - 0.5
            {
                target_y = same_row_y + (this_top_in_parent - row_top);
            }
            if let Some(prev_bottom) = pending_prev_bottom {
                target_y += (this_top_in_parent - prev_bottom).max(0.0);
            }
            page_taffy_origin = this_top_in_parent - (target_y - page_start_y);
        }

        // F30f-b: a child that starts at or below the clip is hidden. It gets no place and no page: its geometry sits
        // below the page's content box (outside every clip band), on the page the box has reached, so that the counters,
        // string-set and bookmarks in it keep their page.
        if let Some(limit) = clip_limit
            && child.element_data().is_some()
            && this_top_in_parent >= limit - CLIP_LINE_SLACK
        {
            let hidden_y = strips.bottom(page_index) + (this_top_in_parent - limit).max(0.0);
            let hidden_x = parent_x_in_body + layout.location.x;
            geometry
                .entry(child_id)
                .or_default()
                .fragments
                .push(Fragment {
                    page_index,
                    x: hidden_x.as_px(),
                    y: hidden_y.as_px(),
                    width: child_w.as_px(),
                    height: child_h.as_px(),
                });
            record_subtree_descendants(
                geometry,
                doc,
                child_id,
                page_index,
                hidden_y,
                hidden_x,
                depth + 1,
            );
            if !is_float {
                prev_used_page = Some(used_end.clone());
            }
            continue;
        }

        if child_h <= 0.0 {
            // Phase 2.3 fix: zero-height **element** nodes still
            // need to enter geometry so their counter / string-set
            // / bookmark markers participate in the parity walks.
            //
            // Zero-height children skip the inter-child gap (matching
            // `fragment_pagination_root`'s zero-height branch where
            // `continue` happens before the gap calc), so break-before
            // can fire here without first folding gap into cursor_y.
            if break_before_page && cursor_y > page_start_y + band {
                geometry
                    .entry(parent_id)
                    .or_default()
                    .fragments
                    .push(Fragment {
                        page_index,
                        x: parent_x_in_body.as_px(),
                        y: page_start_y.as_px(),
                        width: parent_w.as_px(),
                        height: (cursor_y - page_start_y).as_px(),
                    });
                page_index += 1;
                band = reserve_header_band(&mut repeat, is_header_cell, geometry, page_index, 0.0);
                band = band.max(top_floor);
                cursor_y = band;
                page_start_y = 0.0;
                // Zero-height break-before: this child IS the first
                // on the new page — apply origin rebase eagerly.
                page_taffy_origin = this_top_in_parent - band;
                (origin_pending_target_y, origin_pending_same_row) = (None, None);
            }
            if child.element_data().is_some() {
                geometry
                    .entry(child_id)
                    .or_default()
                    .fragments
                    .push(Fragment {
                        page_index,
                        x: (parent_x_in_body + layout.location.x).as_px(),
                        y: cursor_y.as_px(),
                        width: child_w.as_px(),
                        height: 0.0_f32.as_px(),
                    });
            }
            // Honour `break-after: page` for zero-height elements
            // too — same fulgur-p3uf (Phase 3.1.5a) fix as
            // `fragment_pagination_root`'s zero-height branch.
            if break_after_page {
                geometry
                    .entry(parent_id)
                    .or_default()
                    .fragments
                    .push(Fragment {
                        page_index,
                        x: parent_x_in_body.as_px(),
                        y: page_start_y.as_px(),
                        width: parent_w.as_px(),
                        height: (cursor_y - page_start_y).as_px(),
                    });
                page_index += 1;
                band = reserve_header_band(&mut repeat, is_header_cell, geometry, page_index, 0.0);
                band = band.max(top_floor);
                cursor_y = band;
                page_start_y = 0.0;
                // Zero-height break-after: NEXT child is the first
                // on the new page — defer origin rebase.
                (origin_pending_target_y, origin_pending_same_row) =
                    (Some(page_start_y + band), None);
            }
            if !is_float {
                prev_used_page = Some(used_end.clone());
            }
            continue;
        }

        children_bottom = children_bottom.max(this_top_in_parent + child_h);

        // fulgur-kv0r: place the child at its Taffy-reported parent-
        // relative y, offset by the parent's start on the current
        // page (`page_start_y`) and rebased against
        // `page_taffy_origin` so the first child on each page strip
        // lands at `page_start_y` regardless of its absolute parent
        // y. For grid / flex parallel siblings (same `location.y`),
        // this places them at the same page-local y; for sequential
        // block flow, it matches Taffy's stacked positions exactly.
        let mut child_page_y = page_start_y + (this_top_in_parent - page_taffy_origin);
        // F83b: in-flow content precedes this child on its strip: the cursor
        // before the child, past the top of the strip (and the repeated
        // header band), or, on the page the parent was entered on, content
        // above the parent. The parent's own border, padding and margin
        // never count as content.
        let cursor_before = cursor_y;
        let mut content_above_child =
            (page_index == page_in && content_above) || cursor_before > page_start_y + band + 0.5;
        // Update the cursor only when the child's bottom advances
        // past it. For block flow this matches cursor advancing by
        // `gap + child_h`; for grid parallel siblings the cursor
        // tracks the row's max bottom (so break-before / overflow
        // checks see the full row height).
        cursor_y = cursor_y.max(child_page_y);

        // Honour `break-before: page`. Leading collapse: only fires
        // when some content has already been placed on this page —
        // gated by `cursor_y > page_start_y` (mirrors body-level's
        // `cursor_y > 0.0` since body's implicit page_start is 0).
        // F83b review (fuzz `Z1_234`): in block flow a forced break before the first child of a box only counts when
        // something precedes it (`content_above_child`). The box's own padding above it is not content: when the box
        // was moved to a fresh page by F28 or by an avoid rule, the break is the page boundary already, and firing it
        // again left a blank page.
        if break_before_page
            && cursor_y > page_start_y + band
            && (content_above_child || suppress_page_check || parent_is_table)
        {
            geometry
                .entry(parent_id)
                .or_default()
                .fragments
                .push(Fragment {
                    page_index,
                    x: parent_x_in_body.as_px(),
                    y: page_start_y.as_px(),
                    width: parent_w.as_px(),
                    height: (cursor_y - page_start_y).as_px(),
                });
            page_index += 1;
            band = reserve_header_band(&mut repeat, is_header_cell, geometry, page_index, child_h);
            band = band.max(top_floor);
            cursor_y = band;
            page_start_y = 0.0;
            content_above_child = false;
            // The breaking child is the first in-flow child on the
            // new page strip. Rebase the Taffy origin to its
            // `this_top_in_parent` so it lands at `page_start_y` (= 0,
            // or just below the repeated table header band) — discarding
            // the inter-child gap, matching CSS 3 Fragmentation §3
            // (margins at forced breaks truncate).
            page_taffy_origin = this_top_in_parent - band;
            child_page_y = band;
        }

        // (Strip-overflow page cut moved below the recursion gate as
        // part of fulgur-7hf5 — see the `if cursor_y > page_start_y
        // && cursor_y + child_h > page_height_px` block after the
        // gate. The gate must run from the **current** cursor so an
        // in-place split produces a `WithinChild`-shaped result on
        // the current strip, not a pre-advanced fresh page.)

        let child_x_in_body = parent_x_in_body + layout.location.x;

        // fulgur-7hf5 (Phase 3.1.5c): unified recursion gate matching
        // `fragment_pagination_root`'s body-direct branch — recurse
        // whenever the child's subtree would split (in-place,
        // truly-oversized, or forced-break-below). The recursion
        // enters from the current cursor so an in-place split
        // produces a `WithinChild`-shaped result on the current page
        // strip and a tail on the next.
        //
        // `would_split_block_subtree` returns `false` when all the
        // child's grandchildren fit the available strip — protects
        // against the "parent CSS height > children sum" case where
        // recursion would emit a parent fragment shorter than
        // expected.
        // fulgur-7hf5: see body-direct branch — multicol containers
        // are atomic from the fragmenter's perspective.
        let is_multicol = crate::blitz_adapter::is_multicol_container(child);
        // F83b: an avoid box in block flow that is kept whole is not entered
        // for want of room (`avoid_keeps_whole`); the cut below moves it, as
        // it moves any unsplittable child. A forced break inside it still
        // enters it.
        let block_parent = !suppress_page_check && !parent_is_table;
        let avoid_inside_child = matches!(
            break_props.break_inside,
            Some(crate::draw_primitives::BreakInside::Avoid)
        );
        let avoid_extent_c = if avoid_inside_child && block_parent && !is_multicol {
            avoid_measure(doc, child_id, child, column_styles)
        } else {
            0.0
        };
        let parent_avoids = in_avoid_box(doc, parent_id, column_styles);
        let tail_raw = if block_parent && Some(child_id) == last_flow_child && closes_like_block {
            let parent_h = parent.final_layout.size.height;
            let below = if parent_h.is_finite() {
                (parent_h - (this_top_in_parent + child_h)).max(0.0)
            } else {
                0.0
            };
            tail_below + below
        } else {
            0.0
        };
        // The room the child keeps for it when it is moved whole: only when both fit a page together (a bottom inset
        // taller than that overflows, as for a split paragraph, F30).
        let leaf_tail = if child_h + tail_raw <= strips.fresh() {
            tail_raw
        } else {
            0.0
        };
        let keeps_whole_at = |page: u32, y: f32, above: bool| {
            avoid_inside_child
                && block_parent
                && !is_multicol
                && avoid_keeps_whole(
                    true,
                    avoid_extent_c,
                    strips.bottom(page) - y,
                    strips.fresh(),
                    above,
                    tail_raw,
                )
        };
        // A box that fits a page and the room, but not together with what its parents need below it (their bottom
        // padding and border, it being their last child), at a place where it cannot move (first on its strip): it is
        // entered, so that its last child moves on and the frames close inside the page (gen1 `A_k49_*_d2_p1`).
        // Only for an `auto` height box, like `relaxed_frame`; it used to be placed whole and the frames crossed the
        // content bottom.
        let tail_over_at = |page: u32, y: f32, above: bool| {
            block_parent
                && !is_multicol
                && !above
                && tail_raw > 0.0
                && tail_raw <= strips.fresh() * 0.5
                && y + child_h + tail_raw > strips.bottom(page) + 1.0
                && frame_is_content_sized(doc, child)
        };
        // F83b review (H3): a relaxed avoid box (taller than a page) whose height is its content and its bottom inset
        // is entered even when its children fit the strip: its frame does not, and it used to cross the content
        // bottom by the bottom padding and border when it started a page.
        let relaxed_frame = avoid_inside_child
            && block_parent
            && !is_multicol
            && !avoid_inside_holds(true, avoid_extent_c, strips.fresh())
            && frame_is_content_sized(doc, child);
        let gate = |available_strip: f32, keeps_whole: bool, tail_over: bool| {
            !child.children.is_empty()
                && !is_multicol
                && (relaxed_frame
                    || (!keeps_whole && tail_over)
                    || forced_break_below(facts, doc, column_styles, child_id)
                    || page_name_change_below(facts, doc, used_page_names, child_id)
                    || (!keeps_whole
                        && would_split_block_subtree(
                            doc,
                            child_id,
                            available_strip,
                            strips.fresh(),
                            0,
                        )))
        };
        // F30: a paragraph that does not fit any page whole is split between
        // its line boxes instead of being placed as one oversized leaf (the
        // gate sees no overflowing child in it: its children are text).
        //
        // F83d: not a cell of a row that `avoid` keeps together (`td` or `tr`), which fits a page: the row is the unit,
        // it is not entered, and the cut below moves it to the next page when it does not fit the room. A row that sits
        // at the top of its strip and does not fit the room has nowhere to move to and is split as before. A forced
        // break inside the cell wins over `avoid` (css-break-3 section 4.3) and enters it as before.
        let row_whole = kept_rows
            .get(&row_key(this_top_in_parent))
            .is_some_and(|&row_h| {
                is_cell_of_table(parent, child)
                    && (child_page_y > page_start_y + band
                        || child_page_y + row_h <= strips.bottom(page_index) + 1.0)
                    && !forced_break_below(facts, doc, column_styles, child_id)
                    && !page_name_change_below(facts, doc, used_page_names, child_id)
            });
        // F30f-b: a child that is a clip box hiding some of its content (`overflow-y` not `visible`, a block size below
        // its content) is placed whole while it fits the room below it (the lines it hides are not written, the box is
        // as tall as its block size); when it does not, it is fragmented by its own block size: its content is cut at
        // the clip, the frames add up to its height, nothing at or below the clip gets a page.
        let child_clips = clip_flow
            && !is_float
            && !is_multicol
            && !avoid_inside_child
            && clip_block_limit(doc, child).is_some();
        let clip_whole_at =
            |page: u32, y: f32| child_clips && y + child_h <= strips.bottom(page) + 0.5;
        // A clip box with block children that does not fit is entered even though the gate sees no overflowing child
        // (its content overflows its box, not the strip below the cursor).
        let clip_enter = |page: u32, y: f32| {
            child_clips
                && !clip_whole_at(page, y)
                && !child.flags.is_inline_root()
                && !child.children.is_empty()
        };
        let mut line_split = if row_whole || clip_whole_at(page_index, child_page_y) {
            None
        } else {
            nested_line_split(
                doc,
                parent,
                child,
                child_page_y,
                page_start_y + band,
                strips,
                page_index,
            )
        };
        // F30d: a multi-column container that does not fit the room below it is distributed over the
        // pages (plain block flow only: in a flex / grid box, a table or an atomic inline it stays one
        // box, F30d-b).
        let mc_for = |page: u32, y: f32| -> Option<McContainer<'_>> {
            if !block_parent {
                return None;
            }
            multicol_container(doc, multicol, column_styles, child_id, strips.fresh()).filter(
                |mc| {
                    multicol_cuts(
                        mc,
                        child_h,
                        strips.bottom(page) - y,
                        strips.fresh(),
                        avoid_inside_child,
                    )
                },
            )
        };
        let mut mc_split = if line_split.is_none() {
            mc_for(page_index, child_page_y)
        } else {
            None
        };
        // The parent's own padding / border and the child's margin are not content above it (H1): only a previous
        // sibling, or content above the parent on the page it was entered on, is.
        // A child that sits at the top edge of its parent cannot be moved by the cut below (the parent's caller moves a
        // parent whose first row does not fit): it is "movable" only when it is below the strip top as well. Counting
        // outer content above a flush first child kept it whole in place, and a box a hair taller than the page lost
        // its last lines (fuzz `Z1_59`).
        let movable = content_above_child && child_page_y > page_start_y + band;
        let mut keeps_whole = keeps_whole_at(page_index, child_page_y, movable);
        let mut needs_recursion = line_split.is_none()
            && !row_whole
            && !clip_whole_at(page_index, child_page_y)
            && (clip_enter(page_index, child_page_y)
                || gate(
                    (strips.bottom(page_index) - cursor_y).max(0.0),
                    keeps_whole,
                    tail_over_at(page_index, child_page_y, content_above_child),
                ));
        // html-to-pdf F183 (plan 7.7): the gate enters an inline root because one of its inline boxes (an `<img>`, the
        // second line's) overflows the strip. The children of an inline root are not block children: the lines put
        // them where they are, and the block walk cut the root between two boxes, in fragments as tall as the boxes,
        // without line ranges (a 64 px line of a 60 px image no longer fitted its fragment and was dropped, with the
        // text under it). Where an inline root has to be cut it is cut between its line boxes, as one that is first
        // on its strip or taller than a page already is. A forced break inside it (an inline-block holding one) still
        // enters it, and a root the line splitter must keep whole (`split_barrier`) is entered as before.
        if needs_recursion
            && mc_split.is_none()
            && !forced_break_below(facts, doc, column_styles, child_id)
            && !page_name_change_below(facts, doc, used_page_names, child_id)
            && let Some(split) = line_split_candidate(doc, parent, child)
        {
            line_split = Some(split);
            needs_recursion = false;
        }
        // F83b: an avoid box taller than a page breaks like `auto`, but when
        // content precedes it (or something that starts with it) the break
        // before it is preferred to one inside it (css-break-3 §4.5): it
        // starts the next page, where it is split as before.
        let relaxed_start = block_parent
            && content_above_child
            && starts_with_relaxed_avoid(doc, child_id, column_styles, strips.fresh(), 0);

        // Strip-overflow page cut (fulgur-7hf5 moved it below the gate; F28
        // moves it back above): the child starts a new page when the part of
        // it that cannot be split — the whole child, or for a child the gate
        // will enter, its first row — does not fit below its placement. Use
        // `child_page_y` (the actual placement) rather than `cursor_y` so a
        // parallel sibling returning to a smaller page-local y is checked
        // correctly.
        //
        // `child_page_y > page_start_y + band` keeps the first content of a
        // strip from being cut again (a row taller than `page - header` would
        // otherwise advance a page per attempt). `band` is the repeated table
        // header (F12) reserved at the top of this strip. Entering a child
        // that sits at the parent's top edge is safe because the parent's own
        // caller already checked that this first row fits (`leading_block_size`
        // follows it down), or the parent starts a page.
        //
        // Before F28 a child the gate would enter was never cut, so a first
        // row of a nested box that sat mid-page hung into the bottom margin.
        // Of a flex / grid row only the first item decides.
        //
        // A first row taller than a page is not cut: a fresh page would not
        // hold it either.
        //
        // F83b review: inside an avoid box in block flow the guard is `content_above_child` as well: a first child
        // that only the padding, border or margin of the box (or of a parent inside it) precede is not cut to a page
        // of its own (that left a page holding nothing but the frame, the validator's H3 and fuzz `Z1_549`); like an
        // atomic leaf it overflows where it is. Other parents keep the cut (main's behaviour, which the harness
        // showed Chrome's words agree with).
        // And a child kept whole that is the last in its box needs room for what the boxes around it need below it
        // (bottom padding and border), as a split paragraph does (F30): without it the frame crossed the content
        // bottom by the padding.
        let first_break_h = if relaxed_start {
            strips.fresh()
        } else if let Some(split) = &line_split {
            // The inset above the first line and ORPHANS lines must fit; a
            // paragraph that cannot even start here moves to the next page.
            let leading = split.leading();
            // Of a flex / grid row only the first item decides, as for an
            // entered item below (F28).
            if first_in_row && leading <= strips.fresh() {
                leading
            } else {
                0.0
            }
        } else if let Some(mc) = &mc_split {
            // The container's top padding and its first line (or box) must fit; otherwise it starts on
            // the next page.
            let leading = mc.top_inset() + mc.first_unit();
            if first_in_row && leading <= strips.fresh() {
                leading
            } else {
                0.0
            }
        } else if needs_recursion {
            let leading = if !first_in_row {
                0.0
            } else if keeps_whole {
                // A kept-whole avoid box a forced break inside makes the gate enter: the piece before the break.
                avoid_piece_size(doc, child_id, column_styles, 0).min(child_h)
            } else {
                // As at body level: an avoid box below counts whole only for a child that can move.
                leading_block_size(
                    doc,
                    child_id,
                    column_styles.filter(|_| movable),
                    0.0,
                    0,
                    strips.fresh(),
                )
            };
            if leading <= strips.fresh() {
                leading
            } else {
                0.0
            }
        } else if child_h > strips.fresh()
            && row_mate_can_split(doc, parent, &walk_children, child_id, strips.fresh())
        {
            // A table cell stretched to the height of a row that spans pages
            // (F30): another cell of the row splits, so the row starts here
            // instead of moving whole to the next page, which would not hold it.
            0.0
        } else if keeps_whole {
            // Kept whole because it fits within the 1 px of slack `avoid_keeps_whole` has: the cut must not move
            // what the fit test let stay (gen6 `M_k50_*_bordered`: a box 1 px over the room was moved away from the
            // top of its page, leaving it blank).
            (child_h + leaf_tail - 1.0).max(0.0)
        } else {
            child_h + leaf_tail
        };
        if child_page_y > page_start_y + band
            && (content_above_child || !block_parent || !parent_avoids)
            && child_page_y + first_break_h > strips.bottom(page_index)
        {
            // The child itself fits: it moves on for the room its box's bottom padding and border need, so that
            // box is closed below its last child on the page it ends on.
            if leaf_tail > 0.0 && child_page_y + child_h <= strips.bottom(page_index) {
                lines_split_below = true;
            }
            let should_emit = row_state
                .as_mut()
                .map(|rs| rs.emitted_parent_pages.insert(page_index))
                .unwrap_or(true);
            if should_emit {
                geometry
                    .entry(parent_id)
                    .or_default()
                    .fragments
                    .push(Fragment {
                        page_index,
                        x: parent_x_in_body.as_px(),
                        y: page_start_y.as_px(),
                        width: parent_w.as_px(),
                        height: (cursor_y - page_start_y).as_px(),
                    });
            }
            page_index += 1;
            band = reserve_header_band(&mut repeat, is_header_cell, geometry, page_index, child_h);
            band = band.max(top_floor);
            cursor_y = band;
            page_start_y = 0.0;
            // Forced to a fresh page: rebase the Taffy origin so the
            // current child lands at page_start_y (= 0, or just below the
            // repeated table header band) on the new page. Sequential
            // siblings then continue from this point.
            page_taffy_origin = this_top_in_parent - band;
            child_page_y = band;
            let was_entered = line_split.is_some() || needs_recursion || mc_split.is_some();
            if mc_split.is_some() {
                // On a fresh page the container may fit whole after all.
                mc_split = mc_for(page_index, child_page_y);
            }
            if line_split.is_some() {
                // On a fresh page the paragraph may fit whole after all.
                line_split = if clip_whole_at(page_index, child_page_y) {
                    None
                } else {
                    nested_line_split(
                        doc,
                        parent,
                        child,
                        child_page_y,
                        page_start_y + band,
                        strips,
                        page_index,
                    )
                };
            } else if needs_recursion {
                // The child may fit the new page whole.
                keeps_whole = keeps_whole_at(page_index, child_page_y, false);
                needs_recursion = !clip_whole_at(page_index, child_page_y)
                    && (clip_enter(page_index, child_page_y)
                        || gate(
                            (strips.bottom(page_index) - cursor_y).max(0.0),
                            keeps_whole,
                            tail_over_at(page_index, child_page_y, false),
                        ));
            }
            content_above_child = false;
            if was_entered {
                // A later cell of this flex / grid row restores the row's
                // start state when the cell recurses across pages; that
                // start is now the new page.
                if let Some(rs) = row_state.as_mut() {
                    rs.start_page = page_index;
                    rs.start_cursor_y = cursor_y;
                    rs.start_page_start_y = page_start_y;
                    rs.start_page_taffy_origin = page_taffy_origin;
                    rs.max_end_page = page_index;
                    rs.max_end_cursor_y = cursor_y;
                }
            }
        }
        if needs_recursion || line_split.is_some() || mc_split.is_some() {
            let pre_recursion_page = page_index;
            let pre_recursion_cursor_y = cursor_y;
            let row_start = RowCursor {
                page_index,
                cursor_y,
                page_start_y,
                page_taffy_origin,
                band,
                pending_target_y: None,
                pending_prev_bottom: None,
                pending_same_row: None,
            };
            // fulgur-u0p0: enter recursion from `child_page_y` (the rebased
            // page-local y of THIS child) instead of the parent's running
            // `cursor_y`. For block flow, `child_page_y == cursor_y` after
            // the cursor_y update above, so this is a no-op. For grid / flex
            // parallel siblings (same Taffy `location.y` as a previous
            // sibling on this page), `cursor_y` still holds the previous
            // sibling's bottom — passing it would stack the parallel cell
            // below the previous one instead of beside it. Using
            // `child_page_y` keeps each cell's recursion strip aligned to
            // the row's y on the current page.
            // What the boxes up to here need below this child, when it is the
            // last thing in flow in this box.
            let child_tail = if Some(child_id) == last_flow_child && closes_like_block {
                let parent_h = parent.final_layout.size.height;
                let below = if parent_h.is_finite() {
                    (parent_h - (this_top_in_parent + child_h)).max(0.0)
                } else {
                    0.0
                };
                tail_below + below
            } else {
                0.0
            };
            // F12: pages this child's own content spans below the first get the
            // table's repeated header, so they are reserved room for it.
            if !is_header_cell && let Some(r) = repeat.as_mut() {
                r.note_body_cell(page_index);
            }
            let child_floor = match repeat.as_ref() {
                Some(r) if is_header_cell || r.source.is_none() => 0.0,
                Some(r) => r.height,
                None => top_floor,
            };
            let mc_used = mc_split.is_some();
            let (np, nc, split_below, stretch) = match &line_split {
                None if mc_split.is_some() => {
                    let mc = mc_split.as_ref().expect("checked above");
                    let (np, nc) = fragment_multicol_container(
                        geometry,
                        doc,
                        McEnv {
                            column_styles,
                            used_page_names,
                            facts,
                            multicol,
                        },
                        mc,
                        child_id,
                        child_x_in_body,
                        child_w,
                        page_index,
                        child_page_y,
                        strips,
                    );
                    (np, nc, np > pre_recursion_page, 0.0)
                }
                Some(split) => {
                    let (np, nc) = fragment_nested_inline_root(
                        geometry,
                        doc,
                        child_id,
                        child_x_in_body,
                        child_w,
                        child_page_y,
                        page_index,
                        strips,
                        child_tail,
                        child_floor,
                        split,
                        depth + 1,
                    );
                    (np, nc, np > pre_recursion_page, 0.0)
                }
                None => fragment_block_subtree(
                    geometry,
                    doc,
                    column_styles,
                    used_page_names,
                    facts,
                    multicol,
                    child_id,
                    child_w,
                    child_x_in_body,
                    page_index,
                    child_page_y,
                    strips,
                    depth + 1,
                    child_tail,
                    child_floor,
                    content_above_child,
                ),
            };
            page_index = np;
            lines_split_below |= split_below;
            if is_header_cell {
                // F12: a header cell whose own content split across pages
                // cannot be repeated; the header must sit on one page.
                let keep = np == pre_recursion_page
                    && repeat.as_mut().is_none_or(|r| {
                        r.note_header_cell(
                            pre_recursion_page,
                            parent_x_in_body,
                            parent_w,
                            child_page_y - this_top_in_parent,
                        )
                    });
                if !keep {
                    repeat = None;
                }
            } else if let Some(r) = repeat.as_mut() {
                r.note_body_cell(pre_recursion_page);
            }
            // fulgur-u0p0: when the recursion stayed on the same page,
            // keep the larger of the parent's existing `cursor_y` (row max
            // bottom from a previous parallel sibling) and the recursion's
            // returned `nc`. When the recursion crossed pages, the previous
            // page's `cursor_y` is stale — adopt `nc` directly.
            cursor_y = if np == pre_recursion_page {
                cursor_y.max(nc)
            } else {
                nc
            };
            // If the recursion crossed a boundary, the parent's
            // current-page fragment must restart at y=0 on the new
            // page. Defensive `nc < page_start_y` guards against
            // backward cursor returns (impossible in normal flow).
            if page_index != pre_recursion_page || nc < page_start_y {
                // fulgur-oc51: emit parent fragments for every
                // page span the recursion crossed. Without this,
                // only the trailing close at the end of this
                // function emits a parent fragment (on the *last*
                // page), so the parent's background / borders
                // disappear from the previous and intermediate
                // pages. Pre-recursion overflow close (no-
                // recursion branch, line ~1713) covers the
                // analogous case for non-recursive children, but
                // there is no equivalent push when the page
                // advance happens *inside* the recursion.
                //
                // The parent's content extended to the page
                // bottom on every previous page (otherwise the
                // recursion would not have advanced past that
                // page), so the previous-page fragment spans
                // `[page_start_y, page_height_px]` and any
                // intermediate page is a full strip.
                if page_index > pre_recursion_page {
                    // Match the no-recursion strip-overflow shape
                    // (line ~1713): the parent fragment's height is
                    // its accumulated logical content on the
                    // previous page, treated as if the splitting
                    // child had been laid out in full (without
                    // fragmentation). The renderer clips the
                    // overflow visually. Using the visible page
                    // strip (`page_height_px - page_start_y`)
                    // instead would stop short of the page's
                    // margin-area paint that adjacent passing tests
                    // (mo-006/008) expect.
                    // F30: a paragraph split below (in this child or deeper)
                    // is taller than the page, and the frame ends at the
                    // page's content bottom, not at the unsplit height of
                    // the child. Page 0 is shifted by the body's own offset.
                    let logical_height = if split_below {
                        0.0
                    } else {
                        (pre_recursion_cursor_y + child_h - page_start_y).max(0.0)
                    };
                    let strip_end = if split_below {
                        strips.bottom(pre_recursion_page)
                    } else {
                        strips.fresh()
                    };
                    let prev_height = logical_height.max((strip_end - page_start_y).max(0.0));
                    if prev_height > 0.0 {
                        let should_emit = row_state
                            .as_mut()
                            .map(|rs| rs.emitted_parent_pages.insert(pre_recursion_page))
                            .unwrap_or(true);
                        if should_emit {
                            geometry
                                .entry(parent_id)
                                .or_default()
                                .fragments
                                .push(Fragment {
                                    page_index: pre_recursion_page,
                                    x: parent_x_in_body.as_px(),
                                    y: page_start_y.as_px(),
                                    width: parent_w.as_px(),
                                    height: prev_height.as_px(),
                                });
                        }
                    }
                    for p in (pre_recursion_page + 1)..page_index {
                        let should_emit = row_state
                            .as_mut()
                            .map(|rs| rs.emitted_parent_pages.insert(p))
                            .unwrap_or(true);
                        if should_emit {
                            geometry
                                .entry(parent_id)
                                .or_default()
                                .fragments
                                .push(Fragment {
                                    page_index: p,
                                    x: parent_x_in_body.as_px(),
                                    y: 0.0_f32.as_px(),
                                    width: parent_w.as_px(),
                                    height: strips.fresh().as_px(),
                                });
                        }
                    }
                }
                page_start_y = 0.0;
                // F12: every page this cell's content continued on gets the
                // repeated header; the content below it was placed with room
                // reserved (`child_floor`), so nothing overlaps. Without a
                // repeat there is no band.
                band = top_floor;
                if child_floor > 0.0 {
                    for p in (pre_recursion_page + 1)..=page_index {
                        band = reserve_header_band(&mut repeat, is_header_cell, geometry, p, 0.0)
                            .max(top_floor);
                    }
                }
                origin_pending_target_y = Some(cursor_y);
                origin_pending_prev_bottom = split_below.then_some(this_top_in_parent + child_h);
                let row_top = this_top_in_parent;
                let row_bottom = row_top + child_h;
                origin_pending_same_row =
                    allow_same_row_rebase.then_some((row_top, row_bottom, 0.0));
                // fulgur-ysms: mark that this row had a recursion-driven page
                // cross so subsequent same-row cells know to co-split.
                if let Some(ref mut rs) = row_state {
                    rs.crossed_by_recursion = true;
                }
                if parent_is_table && !is_header_cell {
                    // The row ends where the content of its cells ends: the
                    // stretch below a shorter cell's last line is not content.
                    let this_row = SplitRow {
                        top: this_top_in_parent,
                        bottom: this_top_in_parent + child_h,
                        start: row_start,
                        end: RowCursor {
                            page_index,
                            cursor_y: cursor_y - stretch,
                            page_start_y,
                            page_taffy_origin,
                            band,
                            pending_target_y: origin_pending_target_y.map(|y| y - stretch),
                            pending_prev_bottom: origin_pending_prev_bottom,
                            pending_same_row: origin_pending_same_row,
                        },
                    };
                    // A row ends where its last cell ends, not where the cell
                    // processed last does: an earlier cell of the same row that
                    // continued further keeps the end (F30b: cells of direct
                    // text end at their last line, not at the stretched frame).
                    split_row = Some(match split_row.take() {
                        Some(prev)
                            if (prev.top - this_row.top).abs() < 0.5
                                && (prev.end.page_index, prev.end.cursor_y)
                                    > (this_row.end.page_index, this_row.end.cursor_y) =>
                        {
                            SplitRow {
                                end: prev.end,
                                ..this_row
                            }
                        }
                        _ => this_row,
                    });
                }
            } else if mc_used {
                // F30d: the container ended on the page it started on, shorter than the one-column height
                // Taffy gave it: what follows it starts below the columns, not where Taffy put it.
                origin_pending_target_y = Some(cursor_y);
                origin_pending_prev_bottom = Some(this_top_in_parent + child_h);
            }

            // Honour `break-after: page` after recursion.
            if break_after_page {
                geometry
                    .entry(parent_id)
                    .or_default()
                    .fragments
                    .push(Fragment {
                        page_index,
                        x: parent_x_in_body.as_px(),
                        y: page_start_y.as_px(),
                        width: parent_w.as_px(),
                        height: (cursor_y - page_start_y).as_px(),
                    });
                page_index += 1;
                band =
                    reserve_header_band(&mut repeat, is_header_cell, geometry, page_index, child_h);
                band = band.max(top_floor);
                cursor_y = band;
                page_start_y = 0.0;
                (origin_pending_target_y, origin_pending_same_row) =
                    (Some(page_start_y + band), None);
                origin_pending_prev_bottom = None;
            }
            if !is_float {
                prev_used_page = Some(used_end.clone());
            }
            if let Some(ref mut rs) = row_state {
                if page_index > rs.max_end_page
                    || (page_index == rs.max_end_page && cursor_y > rs.max_end_cursor_y)
                {
                    rs.max_end_page = page_index;
                    rs.max_end_cursor_y = cursor_y;
                }
            }
            continue;
        }

        // Child fits the strip (or is an atomic oversized leaf that
        // simply overflows below the page bottom — with a single
        // in-flow child there is no valid interior split point, so
        // the leaf emits whole). Emit its fragment and recurse into
        // descendants on the same page.
        warn_unprinted_inline_lines(doc, parent, child, child_page_y, strips.bottom(page_index));
        geometry
            .entry(child_id)
            .or_default()
            .fragments
            .push(Fragment {
                page_index,
                x: child_x_in_body.as_px(),
                y: child_page_y.as_px(),
                width: child_w.as_px(),
                height: child_h.as_px(),
            });
        record_subtree_descendants(
            geometry,
            doc,
            child_id,
            page_index,
            child_page_y,
            child_x_in_body,
            depth + 1,
        );
        if is_header_cell {
            let keep = repeat.as_mut().is_none_or(|r| {
                r.note_header_cell(
                    page_index,
                    parent_x_in_body,
                    parent_w,
                    child_page_y - this_top_in_parent,
                )
            });
            if !keep {
                repeat = None;
            }
        } else if let Some(r) = repeat.as_mut() {
            r.note_body_cell(page_index);
        }
        // Track the lowest point reached on this page so the
        // overflow / break-before checks above see the full row's
        // bottom for grid / flex parents (parallel siblings update
        // `cursor_y` to `max(cursor_y, child_page_y + child_h)` —
        // the per-row max bottom).
        cursor_y = cursor_y.max(child_page_y + child_h);

        // Honour `break-after: page` after the child fragment lands
        // (and the descendant walk records same-page entries).
        if break_after_page {
            geometry
                .entry(parent_id)
                .or_default()
                .fragments
                .push(Fragment {
                    page_index,
                    x: parent_x_in_body.as_px(),
                    y: page_start_y.as_px(),
                    width: parent_w.as_px(),
                    height: (cursor_y - page_start_y).as_px(),
                });
            page_index += 1;
            band = reserve_header_band(&mut repeat, is_header_cell, geometry, page_index, child_h);
            band = band.max(top_floor);
            cursor_y = band;
            page_start_y = 0.0;
            (origin_pending_target_y, origin_pending_same_row) = (Some(page_start_y + band), None);
        }
        if !is_float {
            prev_used_page = Some(used_end.clone());
        }
        if let Some(ref mut rs) = row_state {
            if page_index > rs.max_end_page
                || (page_index == rs.max_end_page && cursor_y > rs.max_end_cursor_y)
            {
                rs.max_end_page = page_index;
                rs.max_end_cursor_y = cursor_y;
            }
        }
    }

    // F30: the table ended with the row of a split cell: continue from its end.
    if let Some(row) = split_row.take() {
        page_index = row.end.page_index;
        cursor_y = row.end.cursor_y;
        page_start_y = row.end.page_start_y;
    }

    // fulgur-ysms: finalize any open row — advance to the max end state
    // reached across all parallel sibling cells.
    if let Some(rs) = row_state.take() {
        page_index = rs.max_end_page;
        cursor_y = rs.max_end_cursor_y;
        if rs.max_end_page > rs.start_page {
            page_start_y = 0.0;
        }
    }

    // F30: below the last child of a box with a paragraph split across pages
    // there is its bottom padding and border (and any space Taffy left under
    // the child): the last fragment of the frame includes them, and the cursor
    // handed back is below the frame so the next sibling does not start inside
    // it. Other splits keep closing at the last child, as before.
    let closing = if lines_split_below && closes_like_block {
        let parent_h = parent.final_layout.size.height;
        if parent_h.is_finite() {
            (parent_h - children_bottom).max(0.0)
        } else {
            0.0
        }
    } else {
        0.0
    };

    // F30f-b: a clip box that is fragmented by its own block size is as tall as that over the pages: the frames before
    // the last one ran to the page bottom, and the last one is what is left of the box's height. What follows the box
    // starts at its bottom, not below its last visible line.
    let clip_frame = own_clip_height.map(|box_height| {
        // Every frame so far ended at the bottom of its page.
        let used: f32 = geometry.get(&parent_id).map_or(0.0, |g| {
            g.fragments
                .iter()
                .map(|f| (strips.chrome_bottom(f.page_index) - f.y.to_f32()).max(0.0))
                .sum()
        });
        (box_height - used)
            .max(0.0)
            .min((strips.chrome_bottom(page_index) - page_start_y).max(0.0))
    });
    if let Some(frame) = clip_frame {
        cursor_y = page_start_y + frame;
    }

    // Close the parent's fragment for the final page span. Always
    // emit at least one fragment so the parent is represented in
    // geometry — `collect_counter_states` and friends look up nodes
    // by id, and a missing entry would silently bypass the parity
    // gate via the early `counter_ops_by_node ⊄ geometry` check in
    // `render.rs`. Height may be 0 when every child was skipped
    // (whitespace / OOF / running) — that's intentional and
    // matches `fragment_pagination_root`'s zero-height-element path.
    geometry
        .entry(parent_id)
        .or_default()
        .fragments
        .push(Fragment {
            page_index,
            x: parent_x_in_body.as_px(),
            y: page_start_y.as_px(),
            width: parent_w.as_px(),
            // A frame never extends below the content box: padding taller
            // than the room left overflows (the next sibling moves on).
            height: match clip_frame {
                Some(frame) => frame,
                None => {
                    cursor_y - page_start_y
                        + closing.min((strips.bottom(page_index) - cursor_y).max(0.0))
                }
            }
            .as_px(),
        });

    // F30f: an `inline-block` alone on this box's line now spans several pages: the box's one line is painted where
    // the inline-block starts.
    record_carrier_slices_after(geometry, doc, parent, &walk_children);

    // What `closing` holds beyond the box's own bottom padding and border: the
    // room a stretched box (a table cell as tall as its row) has below its last
    // child. It is part of the frame but not of the content.
    // The margin below the last child is inside a box that contains it (a table
    // cell), so it is not stretch either.
    let trailing = last_flow_child.map_or(0.0, |id| trailing_margin(doc, id, 0));
    let stretch = (closing
        - (parent.final_layout.border.bottom + parent.final_layout.padding.bottom)
        - trailing)
        .max(0.0);
    if clip_frame.is_some() {
        return (page_index, cursor_y, lines_split_below, 0.0);
    }
    (page_index, cursor_y + closing, lines_split_below, stretch)
}

/// F30f: whether `geom` is the geometry of the carrier of a split `inline-block` that [`record_carrier_slices`] gave
/// its slices: one per fragment, exactly one of them the carrier's one line (`0..1`), the fragments before it empty
/// (`0..0`) and the ones after it empty too (`1..1`). Stricter than [`line_slices_tile`], which an inline root's own
/// slices must satisfy.
fn carrier_slices_ok(geom: &PaginationGeometry) -> bool {
    let slices = &geom.line_slices;
    if slices.is_empty() || slices.len() != geom.fragments.len() {
        return false;
    }
    let Some(line) = slices.iter().position(|s| (s.first, s.end) == (0, 1)) else {
        return false;
    };
    slices.iter().enumerate().all(|(k, s)| match k.cmp(&line) {
        std::cmp::Ordering::Less => (s.first, s.end) == (0, 0),
        std::cmp::Ordering::Equal => true,
        std::cmp::Ordering::Greater => (s.first, s.end) == (1, 1),
    })
}

/// F30f: give the carrier `carrier_id` of an `inline-block` that spans several pages line slices that put its one
/// line (the line that holds the box) on the first fragment on `line_page`, the page where the box starts, and none on
/// the other fragments.
///
/// A carrier that is split over several fragments and has no slices is painted by measuring its line from the fragment
/// heights ([`crate::render`]'s `paragraph_lines_for_page`): its one line is taller than every fragment, so it is
/// painted on no page, and the box with it. With these slices the line (and so the box's first fragment, which the
/// carrier paints under the baseline shift of CSS 2.1 §10.8.1) is drawn once, on the page where it always was.
fn record_carrier_slices(
    geometry: &mut PaginationGeometryTable,
    carrier_id: usize,
    line_page: u32,
) {
    let Some(entry) = geometry.get_mut(&carrier_id) else {
        return;
    };
    let Some(line) = entry
        .fragments
        .iter()
        .position(|f| f.page_index == line_page)
    else {
        return;
    };
    entry.line_slices = (0..entry.fragments.len())
        .map(|k| {
            let (first, end) = match k.cmp(&line) {
                std::cmp::Ordering::Less => (0, 0),
                std::cmp::Ordering::Equal => (0, 1),
                std::cmp::Ordering::Greater => (1, 1),
            };
            LineSlice {
                first,
                end,
                offset: 0.0_f32.as_px(),
            }
        })
        .collect();
    debug_assert!(
        carrier_slices_ok(entry),
        "record_carrier_slices: {:?}",
        entry.line_slices
    );
}

/// F30f: after a box `parent` has been fragmented: when it is the carrier of an `inline-block` alone on its line, and
/// the box now has fragments on several pages, record the carrier's slices ([`record_carrier_slices`]).
fn record_carrier_slices_after(
    geometry: &mut PaginationGeometryTable,
    doc: &BaseDocument,
    parent: &blitz_dom::Node,
    walk_children: &[usize],
) {
    if geometry
        .get(&parent.id)
        .is_none_or(|g| g.fragments.len() < 2)
        || collect_inline_line_metrics(parent).len() != 1
    {
        return;
    }
    for &id in walk_children {
        let Some(ib) = doc.get_node(id) else {
            continue;
        };
        if !alone_on_its_line(doc, ib) {
            continue;
        }
        let pages = geometry.get(&id).and_then(|g| {
            let (first, last) = (g.fragments.first()?, g.fragments.last()?);
            Some((first.page_index, last.page_index))
        });
        if let Some((first, last)) = pages
            && last > first
        {
            record_carrier_slices(geometry, parent.id, first);
            return;
        }
    }
}

/// F12: a table's first `table-header-group`, repeated at the top of every
/// continuation page of the table (CSS 2.1 §17.2: "Print user agents may
/// repeat header rows on each page spanned by a table").
///
/// Blitz lays a table out as a grid whose `layout_children` are the cells
/// only, in visual order (`thead` / `tbody` / `tr` get zero size; since F36
/// the first header group comes first and the first footer group last,
/// wherever they are written), so the header group is represented by the
/// cells that are a prefix of that list.
/// The fragmenter reserves `height` px at the top of each continuation page
/// and records one extra fragment of `group_id` there; `render` paints the
/// header cells again at that spot.
struct HeaderRepeat {
    /// The `thead` (or any `display: table-header-group`) node.
    group_id: usize,
    /// Cells of the header group; a prefix of the table's `layout_children`.
    cells: BTreeSet<usize>,
    /// Block-size of the band, measured from the table's top border edge to
    /// the bottom of the lowest header cell.
    height: f32,
    /// Where the header was first placed (the table's top edge on that page).
    /// Written to the geometry table lazily, together with the first repeat,
    /// so tables that never need one leave the table untouched.
    source: Option<Fragment>,
    /// Page height the band competes with.
    page_height: f32,
    /// A body cell has been placed on the page holding the header. Without
    /// one the header is an orphan at the bottom of its page and is not
    /// repeated (the table is left as it was before F12).
    source_has_body: bool,
}

impl HeaderRepeat {
    /// Record that a body (non-header) cell was placed on `page_index`.
    fn note_body_cell(&mut self, page_index: u32) {
        if self
            .source
            .as_ref()
            .is_some_and(|s| s.page_index == page_index)
        {
            self.source_has_body = true;
        }
    }

    /// Record where a header cell landed. `table_top_y` is the page-local y of
    /// the table's top edge implied by that cell. Returns `false` when the
    /// header ended up on more than one page, which disables the repeat.
    fn note_header_cell(
        &mut self,
        page_index: u32,
        table_x: f32,
        table_w: f32,
        table_top_y: f32,
    ) -> bool {
        match &self.source {
            None => {
                self.source = Some(Fragment {
                    page_index,
                    x: table_x.as_px(),
                    y: table_top_y.as_px(),
                    width: table_w.as_px(),
                    height: self.height.as_px(),
                });
                true
            }
            Some(src) => src.page_index == page_index,
        }
    }
}

/// F12: reserve the repeated-header band at the top of `page_index`, a page
/// the table has just advanced to while placing a body cell. Returns the
/// reserved block-size (0 when there is nothing to repeat, or when the cell
/// being placed is itself a header cell).
///
/// Pushes the repeat fragment into the header group's geometry. The first
/// push also records the source fragment, so `fragments[0]` is always where
/// the header was originally laid out; `is_repeat` keeps `is_split()` false.
///
/// `child_h` is the height of the row being placed. A row that fits a page but
/// not the page minus the band gets the whole page and no header there.
/// If no body cell followed the header on its own page (it is an orphan at the
/// bottom of that page), the repeat is dropped for the whole table.
fn reserve_header_band(
    repeat: &mut Option<HeaderRepeat>,
    is_header_cell: bool,
    geometry: &mut PaginationGeometryTable,
    page_index: u32,
    child_h: f32,
) -> f32 {
    if is_header_cell {
        return 0.0;
    }
    let Some(r) = repeat.as_mut() else {
        return 0.0;
    };
    let Some(src) = r.source.clone() else {
        return 0.0;
    };
    if src.page_index == page_index {
        return 0.0;
    }
    if !r.source_has_body {
        *repeat = None;
        return 0.0;
    }
    if r.height + child_h > r.page_height {
        return 0.0;
    }
    let entry = geometry.entry(r.group_id).or_default();
    if entry.fragments.is_empty() {
        entry.fragments.push(src.clone());
        entry.is_repeat = true;
    }
    entry.fragments.push(Fragment {
        page_index,
        x: src.x,
        y: 0.0_f32.as_px(),
        width: src.width,
        height: r.height.as_px(),
    });
    r.height
}

/// F12: decide whether `table_id` repeats its header group, and gather what
/// the fragmenter needs. `None` (no repeat, today's behaviour) unless all hold:
///
/// - `table_id` is a table with a `table-header-group` child;
/// - the group's cells are a prefix of the table's `layout_children` (Blitz
///   orders them visually, so this holds for a `thead` written after the
///   `tbody` as well; it fails for a header inside a `display: contents`
///   wrapper, which Blitz flattens and `first_table_header_group` does not);
/// - `0 < height <= page_height_px / 4` (Chrome's policy: a header larger
///   than a quarter page is not repeated; it also rules out pages holding
///   nothing but a header);
/// - neither the table nor an ancestor (below `<body>`) clips its overflow,
///   is transformed, or is translucent: those subtrees are painted through
///   wrappers that the repeated copy would not pass through.
fn repeating_table_header(
    doc: &BaseDocument,
    table_id: usize,
    page_height_px: f32,
) -> Option<HeaderRepeat> {
    // `convert::table` records the header's drawables only for `<table>`
    // elements; a `display: table` element would get a band and no header.
    let is_table_element = doc
        .get_node(table_id)
        .and_then(|n| n.element_data())
        .is_some_and(|e| e.name.local.as_ref() == "table");
    if !is_table_element {
        return None;
    }
    let group_id = crate::blitz_adapter::first_table_header_group(doc, table_id)?;
    let table = doc.get_node(table_id)?;
    let layout_children = table.layout_children.borrow();
    let layout_children = layout_children.as_deref()?;

    let in_group = |mut id: usize| {
        while let Some(parent) = doc.get_node(id).and_then(|n| n.parent) {
            if parent == group_id {
                return true;
            }
            if parent == table_id {
                return false;
            }
            id = parent;
        }
        false
    };
    let n = layout_children.iter().take_while(|&&c| in_group(c)).count();
    if n == 0 || layout_children[n..].iter().any(|&c| in_group(c)) {
        return None;
    }
    let cells: BTreeSet<usize> = layout_children[..n].iter().copied().collect();

    let height = layout_children[..n]
        .iter()
        .filter_map(|&c| doc.get_node(c))
        .map(|c| c.final_layout.location.y + c.final_layout.size.height)
        .filter(|h| h.is_finite())
        .fold(0.0_f32, f32::max);
    if height <= 0.0 || height > page_height_px / 4.0 {
        return None;
    }
    if table_subtree_is_wrapped(doc, table_id) {
        return None;
    }
    Some(HeaderRepeat {
        group_id,
        cells,
        height,
        source: None,
        page_height: page_height_px,
        source_has_body: false,
    })
}

/// F12: true if `table_id` or one of its ancestors below `<body>` has
/// `overflow` other than `visible`, a `transform`, or `opacity < 1`.
fn table_subtree_is_wrapped(doc: &BaseDocument, table_id: usize) -> bool {
    use ::style::values::computed::Overflow as Ov;
    let mut cur = Some(table_id);
    while let Some(id) = cur {
        let Some(node) = doc.get_node(id) else {
            break;
        };
        if let Some(elem) = node.element_data() {
            let tag = elem.name.local.as_ref();
            if tag == "body" || tag == "html" {
                break;
            }
        }
        if let Some(s) = node.primary_styles() {
            if s.clone_overflow_x() != Ov::Visible
                || s.clone_overflow_y() != Ov::Visible
                || !s.get_box().transform.0.is_empty()
                || s.clone_opacity() < 1.0
            {
                return true;
            }
        }
        cur = node.parent;
    }
    false
}

/// fulgur-p55h: read per-line `(min_coord, max_coord)` pairs from a
/// node's Parley `inline_layout_data`, if any.
///
/// `min_coord` is the line's top-most Y in the paragraph's local
/// coordinate system; `max_coord` is its bottom-most Y. Both are in
/// CSS pixels and accumulate top-to-bottom across the line vector.
/// Returns an empty vec for non-inline-root nodes (block / text /
/// element with no inline children) so callers can branch on
/// `metrics.len() > 1` to decide between line-aware and block paths.
fn collect_inline_line_metrics(node: &blitz_dom::Node) -> Vec<(f32, f32)> {
    let Some(elem) = node.element_data() else {
        return Vec::new();
    };
    let Some(text_layout) = elem.inline_layout_data.as_deref() else {
        return Vec::new();
    };
    text_layout
        .layout
        .lines()
        .map(|line| {
            let m = line.metrics();
            (m.min_coord, m.max_coord)
        })
        .collect()
}

/// CSS 3 Fragmentation default for `orphans`: lines kept at the bottom of a
/// page before a break between line boxes (css-break-3 §4.4). The properties
/// themselves are not read.
const ORPHANS: usize = 2;
/// CSS 3 Fragmentation default for `widows`: lines carried to the top of the
/// next page after a break between line boxes.
const WIDOWS: usize = 2;

/// F30: where to break the lines of an inline root that spans pages, as
/// half-open line index ranges, one per page strip.
///
/// `lines` are the `(top, bottom)` coordinates from
/// [`collect_inline_line_metrics`]; a range's height is the bottom of its last
/// line minus the top of its first. `first_room` is the block-size available
/// for lines on the first strip, `page_room` on every later one.
///
/// Greedy, like the body-level splitter: each strip takes as many lines as fit.
/// A strip always takes at least one line (a line box is monolithic, css-break-3
/// §4.1), even when it overflows. When the greedy break would leave fewer than
/// [`WIDOWS`] lines for the next strip, lines are pulled back to the next strip so
/// it gets [`WIDOWS`], unless that would leave this strip with fewer than
/// [`ORPHANS`] — then the constraint is relaxed (css-break-3 §4.4 allows it when
/// no break avoids a violation).
///
/// `last_extra` is block-size that has to fit below the last line of the last
/// range (the paragraph's bottom padding and border, and what the boxes around it
/// need below it).
///
/// `max_strips` bounds the number of ranges (the last one takes all the rest and
/// overflows), so that a huge paragraph cannot create unbounded pages.
fn plan_line_breaks(
    lines: &[(f32, f32)],
    first_room: f32,
    page_room: f32,
    last_extra: f32,
    max_strips: usize,
) -> Vec<std::ops::Range<usize>> {
    /// Rounding slack when a run of lines is compared with a room.
    const EPS: f32 = 0.01;
    let n = lines.len();
    let mut ranges = Vec::new();
    let mut start = 0usize;
    let mut room = first_room;
    while start < n {
        if ranges.len() + 1 >= max_strips.max(1) {
            ranges.push(start..n);
            break;
        }
        let mut end = start + 1;
        while end < n && lines[end].1 - lines[start].0 <= room + EPS {
            end += 1;
        }
        // What has to follow the last line (the paragraph's bottom inset and the
        // closing of the boxes around it) must fit as well: when it does not,
        // the last line moves on to the next strip.
        if end == n && n - start > 1 && lines[n - 1].1 - lines[start].0 + last_extra > room + EPS {
            end = n - 1;
        }
        if end < n && n - end < WIDOWS {
            let pulled = n - WIDOWS;
            if pulled >= start + ORPHANS {
                end = pulled;
            }
        }
        ranges.push(start..end);
        start = end;
        room = page_room;
    }
    ranges
}

/// F30: an inline root (a paragraph: `<p>`, `<li>`, `<pre>`, a `div` with text)
/// nested in a block, with the line geometry needed to split it between line
/// boxes.
struct LineSplit {
    /// `(top, bottom)` of each line box, paragraph-local (content box).
    lines: Vec<(f32, f32)>,
    /// Border + padding above the first line: reserved on every strip, because
    /// the renderer paints the lines at `fragment.y + inset_top`.
    inset_top: f32,
    /// Border + padding below the last line.
    inset_bottom: f32,
    /// html-to-pdf F30f-b: how many lines after `lines` a clip box hides ([`LineSplit::clip_to`]); they take no place
    /// on any page. The fragments get a slice for them (`LineSlice` after the last fragment's) so that the slices
    /// still tile all the lines of the paragraph.
    hidden_tail: usize,
    /// F30f-b: the border-box height of the paragraph when the paragraph is itself the clip box that hid the lines:
    /// its frames add up to this height over the pages, however far its last visible line reaches.
    own_box_height: Option<f32>,
    /// html-to-pdf F30f-d: the box is sliced at its breaks (`box-decoration-break: slice`, css-break-3 §5.4, the
    /// initial value): its top border and padding are on the first fragment only, the bottom ones on the last, and a
    /// fragment that is not the last has its frame run to the page bottom. A continuation page then starts its lines at
    /// the top of the page, without `inset_top`. `false` keeps the geometry of the nested splitter (the insets of
    /// the frame reserved on every strip, F30).
    slice: bool,
}

/// F30f-b: a line whose top is less than this far (px) inside the clip does not show. The writer's slack is 0.5 pt
/// (`paragraph::CLIP_BAND_SLACK_PT`) of the line's height: a hair of difference that only decides whether a sliver of a
/// line is written.
const CLIP_LINE_SLACK: f32 = 0.5;

impl LineSplit {
    /// F30f-b: the lines that show above `limit` px below the top of the paragraph's border box (the bottom of the
    /// padding box of the clip box, in the paragraph's own coordinates); the rest is hidden by the clip and gets no
    /// place on any page. The first line is always kept (a split has at least one line); a line that starts within
    /// [`CLIP_LINE_SLACK`] of the limit is hidden.
    fn clip_to(mut self, limit: f32) -> LineSplit {
        let shows = |line: &(f32, f32)| self.inset_top + line.0 < limit - CLIP_LINE_SLACK;
        let keep = self.lines.iter().take_while(|l| shows(l)).count().max(1);
        self.hidden_tail += self.lines.len() - keep;
        self.lines.truncate(keep);
        self
    }

    /// Block-size of the lines alone.
    fn span(&self) -> f32 {
        match (self.lines.first(), self.lines.last()) {
            (Some(first), Some(last)) => last.1 - first.0,
            _ => 0.0,
        }
    }

    /// How much of the paragraph has to fit below its placement for starting it
    /// there to make progress: its inset and [`ORPHANS`] lines.
    fn leading(&self) -> f32 {
        let keep = ORPHANS.min(self.lines.len());
        match keep.checked_sub(1).and_then(|i| self.lines.get(i)) {
            Some(last) => self.inset_top + last.1 - self.lines[0].0,
            None => 0.0,
        }
    }
}

/// F30f: why the line splitter does not look through a box (`None` from [`split_barrier`]: it does).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum LineBarrier {
    /// An `inline-block` or `inline-table` that is not alone on its line: the carrier's line holds more than the box,
    /// and one fragment of it would have to hold the rest (F30f-b).
    SharesItsLine,
    /// An `inline-flex` or `inline-grid` box, or an `inline-block` or `inline-table` with no carrier line to find.
    AtomicNotFlowRoot,
    /// A clipping box that hides some of its content in the block axis: fragmenting it would give it one frame per
    /// page, and the per-fragment clip would show what the clip hides today (and in Chrome).
    ClipsItsContent,
    Multicol,
    Transformed,
    Floating,
    Orthogonal,
    /// A child of a table box that is neither a cell nor an anonymous cell of it: its `::before` / `::after` box.
    TablePart,
}

impl LineBarrier {
    /// What the warning calls the box that keeps the paragraph whole.
    fn describe(self) -> &'static str {
        match self {
            Self::SharesItsLine => "an inline-block or inline-table that shares its line",
            Self::AtomicNotFlowRoot => "an inline-flex or inline-grid box",
            Self::ClipsItsContent => "a clipping box shorter than its content",
            Self::Multicol => "a multi-column container",
            Self::Transformed => "a transformed box",
            Self::Floating => "a float",
            Self::Orthogonal => "an orthogonal writing mode",
            Self::TablePart => "a `display: table` box",
        }
    }
}

/// F30f: how far the last line box of a paragraph may reach past the box that holds it without the clip hiding
/// anything: Blitz quantises line boxes, so with a font whose ascent and descent exceed the line height they are taller
/// than their advance, start a pixel above the content top and the last one ends a pixel or two below the box (Noto at
/// 20 px: lines `(-1, 21)`, `(19, 41)`, ..). A line that is really hidden is hidden by a line, 10 px or more.
const LINE_BOX_SLACK: f32 = 2.0;

/// F30f: whether the box `node` is a clipping box whose content ends inside its padding box in the block axis, so
/// that the clip hides nothing there and the box may be fragmented like any other (css-break-3 §4.1: only an
/// `overflow: hidden` box with a definite block size is one a UA may treat as monolithic; Chrome fragments `hidden`,
/// `clip`, `auto` and `scroll` boxes in print).
///
/// An inline root: its inset, lines and bottom inset fit the border box. A block: the lowest in-flow child, and the
/// margin below it (inside a clipping box, a BFC root, the last child's margin does not collapse out), end within the
/// padding box. Floats count. 0.5 px of slack, as in [`frame_is_content_sized`]; content that overflows (a negative
/// margin, a float that sticks out) makes this false and the box stays whole, as before.
fn clip_hides_nothing(doc: &BaseDocument, node: &blitz_dom::Node) -> bool {
    let layout = &node.final_layout;
    let height = layout.size.height;
    if !height.is_finite() {
        return false;
    }
    let lines = collect_inline_line_metrics(node);
    if let Some(last) = lines.last() {
        let inset_top = layout.border.top + layout.padding.top;
        let inset_bottom = layout.border.bottom + layout.padding.bottom;
        // The lines' coordinates are measured from the content top; the first line box of a quantised paragraph
        // (`line-height: normal`) starts a pixel above it, so the end of the last one is what counts.
        return inset_top + last.1 + inset_bottom <= height + LINE_BOX_SLACK;
    }
    let layout_children = node.layout_children.borrow();
    let children: &[usize] = layout_children
        .as_deref()
        .filter(|v| !v.is_empty())
        .unwrap_or(&node.children);
    let mut bottom: f32 = 0.0;
    let mut last = None;
    for &id in children {
        if let Some(c) = doc.get_node(id)
            && is_flow_child_with_height(c)
        {
            let l = &c.final_layout;
            bottom = bottom.max(l.location.y + l.size.height);
            // A child whose lines overflow its own box (a `height` on the paragraph, a stretched flex item) reaches
            // further than its box does. The slack of the line boxes is taken off here, so that the comparison below
            // keeps its own half pixel for the boxes.
            let lines = collect_inline_line_metrics(c);
            if let Some(last_line) = lines.last() {
                bottom = bottom.max(
                    l.location.y
                        + l.border.top
                        + l.padding.top
                        + last_line.1
                        + l.border.bottom
                        + l.padding.bottom
                        - LINE_BOX_SLACK,
                );
            }
            last = Some(id);
        }
    }
    let Some(last) = last else {
        return true;
    };
    bottom + trailing_margin(doc, last, 0)
        <= height - layout.border.bottom - layout.padding.bottom + 0.5
}

/// html-to-pdf F30f-b: whether `node` is `<html>` or `<body>`, whose `overflow` is the viewport's (CSS Overflow 3 §3.5),
/// not a clip of the box.
fn is_root_or_body(node: &blitz_dom::Node) -> bool {
    node.element_data()
        .is_some_and(|e| matches!(e.name.local.as_ref(), "html" | "body"))
}

/// F30f-b: whether `node` clips its block axis: its computed `overflow-y` is not `visible` (css-overflow-3 §3: a
/// `visible` axis next to a `hidden`, `auto` or `scroll` one computes to `auto`, so `overflow-x: hidden` alone clips
/// the block axis too, and `overflow-x: clip` alone does not). Not a root or body box.
fn clips_block_axis(node: &blitz_dom::Node) -> bool {
    use ::style::values::computed::Overflow as Ov;
    !is_root_or_body(node)
        && clips_overflow(node)
        && node
            .primary_styles()
            .is_some_and(|s| s.clone_overflow_y() != Ov::Visible)
}

/// F30f-b: whether the author constrains the block size of `node`: its computed `height` is a length (or a percentage
/// of a basis that is definite) or its `max-height` is not `none` (a length, or a percentage of a definite basis). Only
/// such a box can be shorter than its content, and only for such a box is a line that lies below the clip "hidden": an
/// auto-height box (no `height`, no `max-height`; `min-height`, `fit-content` and `stretch` do not count) is as tall as
/// its content, whatever the line boxes measure, and is fragmented like any other box (F30f, css-break-3 §4.1).
///
/// The line boxes cannot decide this: Blitz makes them as tall as the font's ascent and descent when the line height is
/// below that (16 px Inter in 12 px lines: 20 px boxes 12 px apart), so the last one ends further below the box than
/// [`LINE_BOX_SLACK`] and a content-sized box looked like one with hidden content. `<html>` and `<body>` are never
/// constrained: their overflow is the viewport's. Neither is a table box, a row, a row group or a cell: their `height`
/// is a minimum (CSS 2.1 §17.5.3).
pub(crate) fn block_size_constrained(node: &blitz_dom::Node) -> bool {
    use ::style::values::generics::length::{GenericMaxSize, GenericSize};
    use ::style::values::specified::box_::DisplayInside;
    if is_root_or_body(node) {
        return false;
    }
    let Some(styles) = node.primary_styles() else {
        return false;
    };
    // CSS 2.1 §17.5.3: `height` on a table, a row, a row group or a cell is a minimum; the box grows with its content,
    // so it is not a box the author constrains (Chrome grows a `td{height:300px;overflow:hidden}` to its content).
    if matches!(
        styles.clone_display().inside(),
        DisplayInside::Table
            | DisplayInside::TableRowGroup
            | DisplayInside::TableHeaderGroup
            | DisplayInside::TableFooterGroup
            | DisplayInside::TableRow
            | DisplayInside::TableCell
    ) {
        return false;
    }
    let definite = |percentage: bool| !percentage || percentage_basis_is_definite(node);
    match styles.clone_height() {
        GenericSize::LengthPercentage(lp) if definite(lp.0.has_percentage()) => return true,
        _ => {}
    }
    matches!(
        styles.clone_max_height(),
        GenericMaxSize::LengthPercentage(lp) if definite(lp.0.has_percentage())
    )
}

/// F30f-b: whether a percentage block size of `node` has a definite basis (css-sizing-3 §5.2.1: it computes to `auto`
/// when the containing block's height depends on its content): the nearest ancestor with a non-percentage `height`
/// is reached before one with an `auto` (or keyword) `height`. The root's containing block is the viewport.
fn percentage_basis_is_definite(node: &blitz_dom::Node) -> bool {
    use ::style::values::generics::length::GenericSize;
    let mut current = node;
    for _ in 0..crate::MAX_DOM_DEPTH {
        let Some(parent_id) = current.layout_parent.get().or(current.parent) else {
            return true;
        };
        let parent = current.with(parent_id);
        if parent.element_data().is_none() {
            return true;
        }
        let Some(styles) = parent.primary_styles() else {
            return false;
        };
        match styles.clone_height() {
            GenericSize::LengthPercentage(lp) if !lp.0.has_percentage() => return true,
            GenericSize::LengthPercentage(_) => {}
            _ => return false,
        }
        current = parent;
    }
    false
}

thread_local! {
    /// html-to-pdf F30f-b: the nodes with `break-inside: avoid` (or `page-break-inside: avoid`) of the document being
    /// rendered, for [`clip_shape_supported`]. The declaration is read from the stylesheets by `column_css` (Stylo
    /// does not compute it here), so the free functions of the fragmenter and the style extraction of the converter,
    /// which see only the DOM, learn it from the scope the engine opens ([`BreakAvoidScope`]). Without a scope (unit
    /// tests that call the fragmenter directly) no node avoids.
    static AVOID_NODES: std::cell::RefCell<std::rc::Rc<BTreeSet<usize>>> =
        std::cell::RefCell::new(std::rc::Rc::new(BTreeSet::new()));
}

/// html-to-pdf F30f-b: the engine opens this once per render, after the side table of `column_css` is built and before
/// the fragmenter and the converter run, so that both ask [`clip_shape_supported`] with the same answer. Dropping it
/// restores the previous set.
pub(crate) struct BreakAvoidScope {
    previous: std::rc::Rc<BTreeSet<usize>>,
}

impl BreakAvoidScope {
    pub(crate) fn enter(column_styles: &crate::column_css::ColumnStyleTable) -> Self {
        let avoiding: BTreeSet<usize> = column_styles
            .iter()
            .filter(|(_, p)| {
                matches!(
                    p.break_inside,
                    Some(crate::draw_primitives::BreakInside::Avoid)
                )
            })
            .map(|(&id, _)| id)
            .collect();
        let previous = AVOID_NODES.with(|a| a.replace(std::rc::Rc::new(avoiding)));
        Self { previous }
    }
}

impl Drop for BreakAvoidScope {
    fn drop(&mut self) {
        AVOID_NODES.with(|a| {
            a.replace(std::rc::Rc::clone(&self.previous));
        });
    }
}

fn avoids_break_inside(node_id: usize) -> bool {
    AVOID_NODES.with(|a| a.borrow().contains(&node_id))
}

/// F30f-b: whether the clip semantics of this change (a clip box is fragmented by its own block size, its lines are cut
/// at the clip, the writer drops what lies below it) apply to `node`: its block size is constrained
/// ([`block_size_constrained`]) and what is in it is a shape the fragmenter handles ([`clip_shape_supported`]).
/// Everything else is laid out, fragmented and drawn as it was before F30f-b, as if the box had no `height`.
pub(crate) fn clip_semantics_apply(node: &blitz_dom::Node) -> bool {
    block_size_constrained(node) && clip_shape_supported(node)
}

/// F30f-b (plan 7, "Supported shapes"): whether the content of the clip box `node` is a shape whose lines the fragmenter
/// can place where Chrome does: direct text (the box is itself the inline root), or exactly one in-flow child that
/// starts at the content top of the box and is such a shape in turn: a paragraph, or a block-level wrapper around one.
///
/// The fragmenter splits only the first thing on a strip (F30c). With anything above the paragraph (a heading, a
/// margin, a float, an image, an earlier paragraph) the paragraph moved whole to the next page and was clipped there,
/// and lines Chrome shows on the page the box starts on were not written; a second paragraph is not placed at all. Such
/// a box keeps main's output until F30c (finding F30f-f).
///
/// Positions are the layout's: a child is "at the top" when its offset equals the resolved border and padding of its
/// parent (so margins count), not what the style says. A percentage `padding-top` or `padding-bottom` is the exception
/// the layout cannot answer: Taffy's block layout resolves it against the parent's height, which is not definite, so
/// the box is reported with no vertical padding and a height without it, while its content is placed below the padding
/// (plan 7). Such a box is not a supported shape. A node with `break-inside: avoid` is not either: the line splitter
/// does not honour it for the paragraph that is first in a clip box.
pub(crate) fn clip_shape_supported(node: &blitz_dom::Node) -> bool {
    in_block_flow(node) && shape_supported(node, 0)
}

/// F30f-b: whether the box `node` is placed by block flow: its parent is not a flex, grid, multicol, table or atomic
/// inline box, which place their children by rules of their own. The fragmenter lifts the clip only where the parent is
/// a block container (`line_split_candidate`: `through_clips`), so for a flex or grid item the clip semantics would be
/// half on (the limit, `frame_above` and the writer's band) and half off, and a grid row with one such item and one
/// unsupported item came out worse than main (3 pages and twice the ink, where main and Chrome have 2: the validator's
/// `tw_clip_grid_card.s880`). Such an item keeps main's output.
fn in_block_flow(node: &blitz_dom::Node) -> bool {
    use ::style::values::specified::box_::DisplayInside;
    let Some(parent) = node.layout_parent.get().or(node.parent) else {
        return false;
    };
    let parent = node.with(parent);
    !crate::blitz_adapter::is_flex_or_grid_container_node(parent)
        && !crate::blitz_adapter::is_atomic_inline_container_node(parent)
        && !crate::blitz_adapter::is_multicol_container(parent)
        && !parent
            .primary_styles()
            .is_some_and(|s| s.clone_display().inside() == DisplayInside::Table)
}

fn shape_supported(node: &blitz_dom::Node, depth: usize) -> bool {
    use ::style::values::specified::box_::{DisplayInside, DisplayOutside};
    if depth >= crate::MAX_DOM_DEPTH
        || avoids_break_inside(node.id)
        || has_percentage_vertical_padding(node)
    {
        return false;
    }
    if node.flags.is_inline_root() {
        return true;
    }
    // A wrapper (not the clip box itself) is a block container of normal flow: a flex, grid or table box, a multicol
    // container or a float lays its child out by rules of its own.
    if depth > 0 {
        let Some(styles) = node.primary_styles() else {
            return false;
        };
        let display = styles.clone_display();
        if display.outside() != DisplayOutside::Block
            || !matches!(
                display.inside(),
                DisplayInside::Flow | DisplayInside::FlowRoot
            )
            || crate::blitz_adapter::is_multicol_container(node)
            || crate::blitz_adapter::node_is_floating(node)
        {
            return false;
        }
    }
    let layout_children = node.layout_children.borrow();
    let children: &[usize] = layout_children
        .as_deref()
        .filter(|v| !v.is_empty())
        .unwrap_or(&node.children);
    let mut only: Option<&blitz_dom::Node> = None;
    for &id in children {
        let child = node.with(id);
        if let Some(text) = child.text_data() {
            if text.content.chars().all(char::is_whitespace) {
                continue;
            }
        } else if child.element_data().is_none() {
            continue;
        }
        // The second child that takes part in the layout: more than one thing in the box.
        if only.replace(child).is_some() {
            return false;
        }
    }
    let Some(child) = only else {
        return false;
    };
    let layout = &node.final_layout;
    child.element_data().is_some()
        && !crate::blitz_adapter::node_is_floating(child)
        && !child.primary_styles().is_some_and(|s| {
            use ::style::properties::longhands::position::computed_value::T as Pos;
            matches!(s.get_box().clone_position(), Pos::Absolute | Pos::Fixed)
        })
        && (child.final_layout.location.y - (layout.border.top + layout.padding.top)).abs() <= 0.5
        && shape_supported(child, depth + 1)
}

/// F30f-b: whether `padding-top` or `padding-bottom` of `node` is a percentage (or a `calc()` with one).
fn has_percentage_vertical_padding(node: &blitz_dom::Node) -> bool {
    node.primary_styles().is_some_and(|s| {
        s.clone_padding_top().0.has_percentage() || s.clone_padding_bottom().0.has_percentage()
    })
}

/// F30f-b: where the clip of `node` ends, when it hides some of its content: the bottom of its padding box, in px from
/// the top of its border box. `None` for a box that does not clip its block axis, for one whose block size the author
/// does not constrain ([`block_size_constrained`]: a content-sized box hides nothing, however tall its line boxes are),
/// and for one whose content ends within its padding box ([`clip_hides_nothing`]): content-sized clip boxes are
/// fragmented like any other (F30f).
fn clip_block_limit(doc: &BaseDocument, node: &blitz_dom::Node) -> Option<f32> {
    if !clips_block_axis(node) || !clip_semantics_apply(node) || clip_hides_nothing(doc, node) {
        return None;
    }
    let layout = &node.final_layout;
    layout
        .size
        .height
        .is_finite()
        .then_some(layout.size.height - layout.border.bottom)
}

/// F30f-b: the tightest clip that an ancestor of `node` puts on its block axis, as px below the top of `node`'s border
/// box: the smallest [`clip_block_limit`] of the clipping boxes above it (the `layout_parent` chain, below `<body>`),
/// moved by the offsets of the boxes between. `None` when no ancestor hides anything.
fn ancestor_clip_limit(doc: &BaseDocument, node: &blitz_dom::Node) -> Option<f32> {
    let mut best: Option<f32> = None;
    let mut offset = 0.0_f32;
    let mut current = node;
    for _ in 0..crate::MAX_DOM_DEPTH {
        offset += current.final_layout.location.y;
        let Some(parent) = current
            .layout_parent
            .get()
            .or(current.parent)
            .and_then(|id| doc.get_node(id))
        else {
            break;
        };
        if is_root_or_body(parent) {
            break;
        }
        if let Some(limit) = clip_block_limit(doc, parent) {
            let here = limit - offset;
            best = Some(best.map_or(here, |b| b.min(here)));
        }
        current = parent;
    }
    best
}

/// F30f-b: the smaller of the clip `node` puts on itself and the one its ancestors put on it, in px below the top of its
/// border box.
fn tightest_clip_limit(doc: &BaseDocument, node: &blitz_dom::Node) -> Option<f32> {
    match (clip_block_limit(doc, node), ancestor_clip_limit(doc, node)) {
        (Some(a), Some(b)) => Some(a.min(b)),
        (a, b) => a.or(b),
    }
}

/// F30e: whether `node` is an `inline-block` or an `inline-table`: an atomic inline whose inside is a block formatting
/// context (a flow-root, or a table box whose cells are block containers) that [`alone_on_its_line`] may split.
fn is_inline_block_or_table(node: &blitz_dom::Node) -> bool {
    use ::style::values::specified::box_::{DisplayInside, DisplayOutside};
    node.primary_styles().is_some_and(|s| {
        let display = s.clone_display();
        display.outside() == DisplayOutside::Inline
            && matches!(
                display.inside(),
                DisplayInside::FlowRoot | DisplayInside::Table
            )
    })
}

/// F30f: whether the `inline-block` (F30e: or `inline-table`) `node` is the only thing on its carrier's single line:
/// the box that holds it in the inline formatting context (its `layout_parent`) has exactly one Parley line, and that
/// line holds this box and glyph runs of whitespace only.
///
/// Then the carrier's line *is* the box: the carrier is never a line-split candidate itself (one line), and
/// nothing else on the line needs the room a page break takes from it. css-break-3 §4.1 lets a UA treat such a box as
/// monolithic; Chrome slices it, we split it between its line boxes (ADR-0004).
fn alone_on_its_line(doc: &BaseDocument, node: &blitz_dom::Node) -> bool {
    if !is_inline_block_or_table(node) {
        return false;
    }
    let Some(carrier) = node.layout_parent.get().and_then(|id| doc.get_node(id)) else {
        return false;
    };
    let Some(text_layout) = carrier
        .element_data()
        .and_then(|e| e.inline_layout_data.as_deref())
    else {
        return false;
    };
    let mut lines = text_layout.layout.lines();
    let (Some(line), None) = (lines.next(), lines.next()) else {
        return false;
    };
    let mut found = false;
    for item in line.items() {
        match item {
            parley::PositionedLayoutItem::InlineBox(b) => {
                if b.id as usize != node.id || found {
                    return false;
                }
                found = true;
            }
            parley::PositionedLayoutItem::GlyphRun(run) => {
                let blank = text_layout
                    .text
                    .get(run.run().text_range())
                    .is_some_and(|t| t.chars().all(char::is_whitespace));
                if !blank {
                    return false;
                }
            }
        }
    }
    found
}

/// F30f: the barrier a box puts up on its own, whether it is the parent or the child of the paragraph: an atomic
/// inline that is not an `inline-block` alone on its line ([`alone_on_its_line`]), or a clipping box that hides
/// content ([`clip_hides_nothing`]).
fn box_barrier(
    doc: &BaseDocument,
    node: &blitz_dom::Node,
    through_clips: bool,
) -> Option<LineBarrier> {
    if crate::blitz_adapter::is_atomic_inline_container_node(node) && !alone_on_its_line(doc, node)
    {
        return Some(if is_inline_block_or_table(node) {
            LineBarrier::SharesItsLine
        } else {
            LineBarrier::AtomicNotFlowRoot
        });
    }
    // F30f-b: a box that clips its block axis and hides content keeps its paragraph whole, unless the caller cuts the
    // paragraph's lines at the clip (`through_clips`: the box is fragmented by its own block size). An
    // `overflow-x: clip` box hides nothing in the block axis.
    if !clips_overflow(node) || clip_hides_nothing(doc, node) {
        return None;
    }
    // A box whose block size the author does not constrain keeps its paragraph whole as it always did: whatever its line
    // boxes measure, it hides nothing the fragmenter should cut ([`block_size_constrained`]). The same goes for one
    // whose content is not a shape the fragmenter handles ([`clip_shape_supported`]).
    if !clip_semantics_apply(node) {
        return Some(LineBarrier::ClipsItsContent);
    }
    (!through_clips && clips_block_axis(node)).then_some(LineBarrier::ClipsItsContent)
}

/// F30f: why the line splitter does not look through `parent` and `child` to split the paragraph `child`, or `None`
/// when it does. The first barrier in the order the checks are made.
///
/// The parent side is about the formatting context the child takes part in (atomic and clipping parents, multicol,
/// orthogonal flows, table parts); the child side about the child itself (atomic, clipping, multicol, floating,
/// transformed). An absolutely positioned box takes part in no such context and asks only the child side
/// ([`abspos_line_split`]).
fn split_barrier(
    doc: &BaseDocument,
    parent: &blitz_dom::Node,
    child: &blitz_dom::Node,
) -> Option<LineBarrier> {
    split_barrier_with(doc, parent, child, false)
}

/// [`split_barrier`], optionally looking through the clip boxes that hide content (F30f-b: `through_clips`).
fn split_barrier_with(
    doc: &BaseDocument,
    parent: &blitz_dom::Node,
    child: &blitz_dom::Node,
    through_clips: bool,
) -> Option<LineBarrier> {
    use ::style::values::specified::box_::DisplayInside;
    let parent_orthogonal = parent
        .parent
        .and_then(|gp| doc.get_node(gp))
        .is_some_and(|gp| crate::blitz_adapter::is_orthogonal_to_parent(gp, parent));
    let inside_of = |n: &blitz_dom::Node| n.primary_styles().map(|s| s.clone_display().inside());
    // A cell of a table box is a block container (CSS 2.1 §17.5), and so is the anonymous cell Blitz makes for text
    // and boxes that are not table parts (CSS 2.1 §17.2.1, F50): its lines are break points. A `table-cell` box that
    // is not in a table box is laid out as a block container too (CSS 2.1 §9.4.1), so it is no barrier (F30e).
    let table_parent = inside_of(parent) == Some(DisplayInside::Table);
    let cell_of_table = table_parent
        && (inside_of(child) == Some(DisplayInside::TableCell)
            || crate::blitz_adapter::is_anonymous_table_cell(parent, child));
    if let Some(barrier) = box_barrier(doc, parent, through_clips) {
        return Some(barrier);
    }
    if crate::blitz_adapter::is_multicol_container(parent) {
        return Some(LineBarrier::Multicol);
    }
    if parent_orthogonal || crate::blitz_adapter::is_orthogonal_to_parent(parent, child) {
        return Some(LineBarrier::Orthogonal);
    }
    if table_parent && !cell_of_table {
        return Some(LineBarrier::TablePart);
    }
    if let Some(barrier) = box_barrier(doc, child, through_clips) {
        return Some(barrier);
    }
    if crate::blitz_adapter::is_multicol_container(child) {
        return Some(LineBarrier::Multicol);
    }
    if crate::blitz_adapter::node_is_floating(child) {
        return Some(LineBarrier::Floating);
    }
    if child
        .primary_styles()
        .is_some_and(|s| !s.get_box().transform.0.is_empty())
    {
        return Some(LineBarrier::Transformed);
    }
    None
}

/// Whether `child` is a cell of the table box `parent`, or an anonymous cell Blitz made for what is not a table part
/// (CSS 2.1 section 17.2.1).
fn is_cell_of_table(parent: &blitz_dom::Node, child: &blitz_dom::Node) -> bool {
    use ::style::values::specified::box_::DisplayInside;
    let inside_of = |n: &blitz_dom::Node| n.primary_styles().map(|s| s.clone_display().inside());
    inside_of(parent) == Some(DisplayInside::Table)
        && (inside_of(child) == Some(DisplayInside::TableCell)
            || crate::blitz_adapter::is_anonymous_table_cell(parent, child))
}

/// Whether `node` is a table box (`display: table`, also the box an `inline-table` makes).
fn is_table_box(node: &blitz_dom::Node) -> bool {
    use ::style::values::specified::box_::DisplayInside;
    node.primary_styles()
        .is_some_and(|s| s.clone_display().inside() == DisplayInside::Table)
}

/// F83d: the key of a table row in [`avoid_rows`]: the row's top, in quarter pixels. The cells of a row share their top
/// (Taffy places them at the row's `y`); the key survives f32 noise between them.
fn row_key(top: f32) -> i64 {
    (top * 4.0).round() as i64
}

/// F83d: the rows of the table box `table` that `break-inside: avoid` keeps on one page, keyed by [`row_key`], with the
/// block-size of the row. A row is kept when one of its cells has `avoid`, or the `tr` that holds the cell does (`tr` has
/// no box of its own in the fragmenter, Blitz gives `tbody` and `tr` no size, so the rule is read from the cell's
/// parent), and when the row fits a fresh page: css-break-3 section 4.4 relaxes `avoid` only when the content cannot be
/// kept from overflowing the fragmentainer, and then the row breaks like an `auto` one. `children` are the table's
/// `layout_children`, the cells in visual order. Empty when `column_styles` is `None` (the caller has no use for the rule)
/// or no row asks for it, which is the common case and costs one pass over the cells.
fn avoid_rows(
    doc: &BaseDocument,
    table: &blitz_dom::Node,
    children: &[usize],
    column_styles: Option<&crate::column_css::ColumnStyleTable>,
    fresh: f32,
) -> BTreeMap<i64, f32> {
    use ::style::values::specified::box_::DisplayInside;
    let mut rows: BTreeMap<i64, (bool, f32)> = BTreeMap::new();
    if column_styles.is_none() {
        return BTreeMap::new();
    }
    for &id in children {
        let Some(cell) = doc.get_node(id) else {
            continue;
        };
        if !is_cell_of_table(table, cell) || !is_flow_child_with_height(cell) {
            continue;
        }
        let in_tr = cell
            .parent
            .filter(|&p| p != table.id)
            .and_then(|p| doc.get_node(p))
            .filter(|tr| {
                tr.primary_styles()
                    .is_some_and(|s| s.clone_display().inside() == DisplayInside::TableRow)
            });
        let avoid =
            has_avoid(column_styles, id) || in_tr.is_some_and(|tr| has_avoid(column_styles, tr.id));
        let row = rows
            .entry(row_key(cell.final_layout.location.y))
            .or_insert((false, 0.0));
        row.0 |= avoid;
        row.1 = row.1.max(cell.final_layout.size.height);
    }
    rows.into_iter()
        .filter(|(_, (avoid, h))| *avoid && *h <= fresh + 1.0)
        .map(|(k, (_, h))| (k, h))
        .collect()
}

/// F30: whether a child of `parent` is an inline root that the fragmenter may
/// split between its line boxes, and if so its lines — whatever its height.
///
/// Paragraphs in block flow qualify, and so do inline roots that are flex items,
/// grid items (css-flexbox-1 §4, css-grid-2 §6: each is a block container whose
/// lines are class B break points), cells of a table box and the anonymous cells
/// Blitz makes for text directly in one (F30b, F30e), and table cells outside a
/// table (F30e). Anything the fragmenter treats as one unit stays one
/// ([`split_barrier`]): atomic inlines that share their line and their contents,
/// orthogonal flows, multicol containers, transformed boxes, clipping boxes that
/// hide content, floats, the pseudo-element boxes of a table box.
fn line_split_candidate(
    doc: &BaseDocument,
    parent: &blitz_dom::Node,
    child: &blitz_dom::Node,
) -> Option<LineSplit> {
    // Cheap exit first: a paragraph of one line has nothing to split.
    let split = inline_root_lines(child)?;
    // F30f-b: in block flow a clip box that hides content is fragmented by its own block size, and the lines it hides
    // get no place on any page: the barrier of the clip boxes is lifted and the lines are cut at the tightest clip.
    let through_clips = is_block_flow_container(doc, parent);
    if split_barrier_with(doc, parent, child, through_clips).is_some() {
        return None;
    }
    let own = clip_block_limit(doc, child);
    let limit = match (own, ancestor_clip_limit(doc, child)) {
        (Some(a), Some(b)) => Some(a.min(b)),
        (a, b) => a.or(b),
    };
    Some(match limit {
        Some(limit) => {
            let mut split = split.clip_to(limit);
            split.own_box_height = own.map(|_| child.final_layout.size.height);
            split
        }
        None => split,
    })
}

/// F30g: the child-side part of [`line_split_candidate`] for an absolutely positioned box: whether `child` is an inline
/// root of at least two lines that the fragmenter may split between its line boxes, and if so its lines. Atomic
/// inlines, multicol containers, floats, clipping and transformed boxes stay one unit, whatever they hold: F30g keeps
/// every clipping or transformed abspos box `Keep`, and an abspos box is blockified (CSS 2.1 §9.7).
///
/// The parent-side checks of [`line_split_candidate`] are about the formatting context the child takes part in (flex /
/// grid / table parents, orthogonal flows); an absolutely positioned box takes part in none, so it asks only this.
fn abspos_line_split(child: &blitz_dom::Node) -> Option<LineSplit> {
    let split = inline_root_lines(child)?;
    (!crate::blitz_adapter::is_atomic_inline_container_node(child) && !clips_overflow(child))
        .then_some(split)
}

/// F30f: `child` as an inline root of at least two lines that is not multicol, floating or transformed, with its
/// lines. What [`abspos_line_split`] and [`line_split_candidate`] share; the atomic and clipping checks differ.
fn inline_root_lines(child: &blitz_dom::Node) -> Option<LineSplit> {
    let lines = collect_inline_line_metrics(child);
    if lines.len() < 2
        || crate::blitz_adapter::is_multicol_container(child)
        || crate::blitz_adapter::node_is_floating(child)
        || child
            .primary_styles()
            .is_some_and(|s| !s.get_box().transform.0.is_empty())
    {
        return None;
    }
    let layout = &child.final_layout;
    Some(LineSplit {
        lines,
        inset_top: layout.border.top + layout.padding.top,
        inset_bottom: layout.border.bottom + layout.padding.bottom,
        hidden_tail: 0,
        own_box_height: None,
        slice: false,
    })
}

/// F30: the split to apply to `child` when it is placed at `child_page_y` on
/// `page`, on a strip that starts at `strip_top`, or `None` to place it as before.
///
/// A paragraph is split when it cannot fit any page whole (taller than a page),
/// or when it is the first thing on its strip and overflows it. A paragraph that
/// fits a page keeps moving whole to the next one when it does not fit the rest
/// of this one (splitting those, as body level does, is F30c).
fn nested_line_split(
    doc: &BaseDocument,
    parent: &blitz_dom::Node,
    child: &blitz_dom::Node,
    child_page_y: f32,
    strip_top: f32,
    strips: PageStrips,
    page: u32,
) -> Option<LineSplit> {
    let split = line_split_candidate(doc, parent, child)?;
    let total = split.inset_top + split.span();
    let taller_than_a_page = total > strips.fresh() + 1.0;
    // The first thing in the box: nothing precedes it but the box's own border and padding. A cell of a table with a
    // border is placed below the table's border (a collapsed table holds half of its outer borders, Blitz rounds that
    // to the pixel above the cell), which is no content above the cell, and measuring from the border edge left such
    // a cell "not first": it was not split between its lines, the block path cut it between its boxes instead
    // (html-to-pdf F183, plan 7.5). Only table cells and boxes inside a clip box that hides content (the box itself or an
    // ancestor, `tightest_clip_limit`): a block child of another padded box keeps moving whole (F30c). A clip box with a
    // block size is fragmented where it is (F30f-b), so the lines of its first descendant paragraph stay on the page the
    // box starts on, below the padding and border of the boxes above them, as the lines of a clip box with direct text
    // do; the paragraph, cut by the clip, would otherwise fit a page and move whole, and leave its box's frame empty
    // (plan 7, "Padded and bordered clip boxes").
    let frame_above =
        if is_cell_of_table(parent, child) || tightest_clip_limit(doc, parent).is_some() {
            parent.final_layout.border.top + parent.final_layout.padding.top
        } else {
            0.0
        };
    let first_and_overflowing = child_page_y <= strip_top + frame_above + 0.5
        && child_page_y + total > strips.bottom(page) + 1.0;
    // F30f-b: a paragraph that is itself a clip box taller than the room is fragmented where it is, by its own block
    // size, like Chrome does (css-break-3 §2: `overflow: hidden` with a `max-height` must fragment, and with a `height`
    // it may); one that fits the room is placed whole.
    let clip_box_overflows = split
        .own_box_height
        .is_some_and(|h| child_page_y + h > strips.bottom(page) + 1.0);
    (taller_than_a_page || first_and_overflowing || clip_box_overflows).then_some(split)
}

/// F111: the split to apply to `<body>` when it is the inline root of the document: its text and inline elements are
/// its own lines, and it is taller than the first page (or than any page). The same rule and the same barriers as
/// for a paragraph nested in a block ([`nested_line_split`], [`split_barrier`]), with `<html>` as the parent and
/// body placed at the top of page 0 (the body offset is already in `strips`). `None` when body is not an inline root,
/// fits, a barrier keeps it whole, or one of its lines is taller than a page.
fn body_line_split(
    doc: &BaseDocument,
    body: &blitz_dom::Node,
    strips: PageStrips,
) -> Option<LineSplit> {
    if !body.flags.is_inline_root() {
        return None;
    }
    let html = doc.get_node(body.parent?)?;
    let split = nested_line_split(doc, html, body, 0.0, 0.0, strips, 0)?;
    // A line taller than a page holds a box (an `inline-block` alone on its line, F30f) that the walk over body's
    // children fragments on its own; one line of it cannot be split here, and splitting around it would place the
    // box whole. Leave such a body to the walk, as before.
    let fresh = strips.fresh();
    (!split
        .lines
        .iter()
        .any(|&(top, bottom)| bottom - top > fresh))
    .then_some(split)
}

/// F30: whether `cell_id`, a cell of table `parent`, shares its row with a cell
/// taller than a page that the fragmenter can enter and split (it holds a
/// paragraph that splits between lines, or is one itself: direct text, F30b).
/// Such a row starts where it is.
fn row_mate_can_split(
    doc: &BaseDocument,
    parent: &blitz_dom::Node,
    siblings: &[usize],
    cell_id: usize,
    page_height_px: f32,
) -> bool {
    use ::style::values::specified::box_::DisplayInside;
    if !parent
        .primary_styles()
        .is_some_and(|s| s.clone_display().inside() == DisplayInside::Table)
    {
        return false;
    }
    let Some(cell) = doc.get_node(cell_id) else {
        return false;
    };
    let top = cell.final_layout.location.y;
    siblings.iter().any(|&id| {
        id != cell_id
            && doc.get_node(id).is_some_and(|m| {
                m.element_data().is_some()
                    && (m.final_layout.location.y - top).abs() < 0.5
                    && m.final_layout.size.height > page_height_px
                    && (leading_block_size(doc, id, None, 0.0, 0, page_height_px) <= page_height_px
                        || line_split_candidate(doc, parent, m)
                            .is_some_and(|split| split.leading() <= page_height_px))
            })
    })
}

/// F30: split a paragraph nested in a block across pages at line boxes.
///
/// One fragment per range from [`plan_line_breaks`]: the first at `y_in`, the
/// others at the top of their page. Each fragment's height is the span of its
/// lines; the renderer paints the lines at `fragment.y + inset_top`, so that
/// inset is reserved on every strip, and the box's own frame (when it has one)
/// is painted from `geometry[child].line_split_insets`.
///
/// Atomic inlines and images in the paragraph get their fragments on the page
/// that holds their line.
///
/// `top_floor` is reserved at the top of every continuation page (a repeated
/// table header, F12): the lines there start below it.
///
/// `strips.bottom(page_in)` is the block-size of the first page strip below which
/// nothing may be placed (less on page 0: the renderer shifts it by the body's own
/// offset). `tail` is what the boxes around the paragraph need below it (their
/// bottom padding and border), so that the last strip leaves room for it.
///
/// Returns `(last_page, cursor_y)`: the page of the last fragment and the y below
/// the paragraph (including its bottom inset) on it.
#[allow(clippy::too_many_arguments)]
fn fragment_nested_inline_root(
    geometry: &mut PaginationGeometryTable,
    doc: &BaseDocument,
    child_id: usize,
    x_in_body: f32,
    width: f32,
    y_in: f32,
    page_in: u32,
    strips: PageStrips,
    tail: f32,
    top_floor: f32,
    split: &LineSplit,
    depth: usize,
) -> (u32, f32) {
    let max_strips = crate::MAX_PAGES.saturating_sub(page_in).max(1) as usize;
    // Room for what follows the last line is only kept when it fits a page next
    // to a line; a bottom padding taller than that overflows, as in browsers,
    // instead of pushing lines onto pages of their own.
    let first_line = split.lines[0].1 - split.lines[0].0;
    // F30f-d: a sliced box has no top border or padding on a continuation page (css-break-3 §5.4), so its lines start at
    // the top there; otherwise the frame's top inset is reserved on every strip.
    let continuation_inset = if split.slice { 0.0 } else { split.inset_top };
    let usable = strips.fresh() - top_floor - continuation_inset - first_line;
    let below = split.inset_bottom + tail;
    let last_extra = if below <= usable { below } else { 0.0 };
    let ranges = plan_line_breaks(
        &split.lines,
        strips.bottom(page_in) - y_in - split.inset_top,
        strips.fresh() - top_floor - continuation_inset,
        last_extra,
        max_strips,
    );
    let mut placements: Vec<(u32, f32, f32)> = Vec::with_capacity(ranges.len());
    let mut cursor_y = y_in;
    for (k, range) in ranges.iter().enumerate() {
        let top = split.lines[range.start].0;
        let bottom = split.lines[range.end - 1].1;
        let (page, y) = if k == 0 {
            (page_in, y_in)
        } else {
            (page_in + k as u32, top_floor)
        };
        let entry = geometry.entry(child_id).or_default();
        entry.fragments.push(Fragment {
            page_index: page,
            x: x_in_body.as_px(),
            y: y.as_px(),
            width: width.as_px(),
            height: (bottom - top).as_px(),
        });
        entry.line_slices.push(LineSlice {
            first: range.start as u32,
            end: range.end as u32,
            offset: (top - split.lines[0].0).as_px(),
        });
        // Where the lines of this strip start, so that a descendant can be
        // placed relative to the strip: `y + (local_y - top)`. A descendant's
        // paragraph-local y counts the first strip's top inset, which a sliced
        // continuation strip does not have (F30f-d).
        let strip_inset = if k == 0 {
            split.inset_top
        } else {
            continuation_inset
        };
        let base_y = if strip_inset == split.inset_top {
            y - top
        } else {
            y - top - (split.inset_top - strip_inset)
        };
        placements.push((page, base_y, top));
        cursor_y = y + strip_inset + (bottom - top);
    }
    cursor_y += split.inset_bottom;
    let entry = geometry.entry(child_id).or_default();
    entry.line_split_insets = Some((split.inset_top.as_px(), split.inset_bottom.as_px()));
    entry.line_split_slice = split.slice;
    debug_assert!(
        line_slices_tile(&entry.line_slices, split.lines.len()),
        "fragment_nested_inline_root: line slices do not tile 0..{}: {:?}",
        split.lines.len(),
        entry.line_slices
    );
    // F30f-b: the lines a clip box hides are the slice after the last fragment's.
    if split.hidden_tail > 0 {
        entry.line_slices.push(LineSlice {
            first: split.lines.len() as u32,
            end: (split.lines.len() + split.hidden_tail) as u32,
            offset: 0.0_f32.as_px(),
        });
    }
    // F30f-b: a paragraph that is itself the clip box that hid lines is as tall as its box over the pages: every frame
    // but the last runs to the page bottom (the break between two line boxes leaves the room under the last line to
    // the box, like Chrome's), and the last one is what is left of the box's height.
    //
    // F30f-d: a sliced box is the same without the clip: it fills the fragmentainer on every page but its last
    // (css-break-3 §5.4; Chrome's side borders run to the content box bottom). The frame's top inset is only on the
    // first fragment of a sliced box.
    let n = ranges.len();
    let first = entry.fragments.len() - n;
    let frame_inset = |k: usize| {
        if k == 0 {
            split.inset_top
        } else {
            continuation_inset
        }
    };
    if let Some(box_height) = split.own_box_height {
        let mut used = 0.0_f32;
        for (k, fragment) in entry.fragments[first..first + n - 1].iter_mut().enumerate() {
            let room = strips.chrome_bottom(fragment.page_index) - fragment.y.to_f32();
            used += room;
            fragment.height = (room - frame_inset(k))
                .max(fragment.height.to_f32())
                .as_px();
        }
        let last = &mut entry.fragments[first + n - 1];
        let room = strips.chrome_bottom(last.page_index) - last.y.to_f32();
        let frame = (box_height - used).clamp(0.0, room.max(0.0));
        last.height = (frame - frame_inset(n - 1) - split.inset_bottom)
            .max(0.0)
            .as_px();
        cursor_y = last.y.to_f32() + frame;
    } else if split.slice {
        for (k, fragment) in entry.fragments[first..first + n - 1].iter_mut().enumerate() {
            let room = strips.chrome_bottom(fragment.page_index) - fragment.y.to_f32();
            fragment.height = (room - frame_inset(k))
                .max(fragment.height.to_f32())
                .as_px();
        }
    }
    record_split_inline_root_descendants(
        geometry,
        doc,
        child_id,
        x_in_body,
        0.0,
        &placements,
        split.inset_top,
        depth,
    );
    (page_in + ranges.len() as u32 - 1, cursor_y)
}

/// F30: record the descendants of a split inline root on the page holding
/// their line (see [`fragment_nested_inline_root`]).
///
/// `placements[k]` is `(page, base_y, line_top)` of strip `k`; a descendant at
/// paragraph-local y lands at `base_y + local_y` on `page` of the last strip whose
/// first line starts at or above it. Zero-size wrappers (inline elements, text
/// nodes) are not recorded themselves, as in [`record_subtree_descendants`];
/// their offsets carry down to what they contain.
#[allow(clippy::too_many_arguments)]
fn record_split_inline_root_descendants(
    geometry: &mut PaginationGeometryTable,
    doc: &BaseDocument,
    parent_id: usize,
    parent_x_in_body: f32,
    parent_y_in_root: f32,
    placements: &[(u32, f32, f32)],
    inset_top: f32,
    depth: usize,
) {
    if depth >= crate::MAX_DOM_DEPTH {
        return;
    }
    let Some(parent) = doc.get_node(parent_id) else {
        return;
    };
    let layout_children_borrow = parent.layout_children.borrow();
    let walk_children: &[usize] = layout_children_borrow
        .as_deref()
        .filter(|v| !v.is_empty())
        .unwrap_or(&parent.children);
    for &child_id in walk_children {
        let Some(child) = doc.get_node(child_id) else {
            continue;
        };
        let layout = child.final_layout;
        let x = parent_x_in_body + layout.location.x;
        let y_in_root = parent_y_in_root + layout.location.y;
        if layout.size.height <= 0.0 && layout.size.width <= 0.0 {
            record_split_inline_root_descendants(
                geometry,
                doc,
                child_id,
                x,
                y_in_root,
                placements,
                inset_top,
                depth + 1,
            );
            continue;
        }
        let local_y = y_in_root - inset_top;
        let k = placements
            .iter()
            .rposition(|&(_, _, top)| top <= local_y + 0.5)
            .unwrap_or(0);
        let (page, base_y, _) = placements[k];
        record_descendant(
            geometry,
            doc,
            child_id,
            page,
            base_y + parent_y_in_root,
            parent_x_in_body,
            depth + 1,
        );
    }
}

/// F30b guard: how far below the page the lines of an inline root end when the
/// fragmenter places it whole at `child_page_y` (a table cell with direct text,
/// a flex / grid item, a paragraph in a clipping box), with its line count.
/// `None` when it fits or has no lines.
fn inline_lines_below_page(
    child: &blitz_dom::Node,
    child_page_y: f32,
    page_height_px: f32,
) -> Option<(usize, f32)> {
    let lines = collect_inline_line_metrics(child);
    let (first, last) = (lines.first()?, lines.last()?);
    let layout = &child.final_layout;
    let bottom = child_page_y + layout.border.top + layout.padding.top + (last.1 - first.0);
    (bottom > page_height_px + 1.0).then_some((lines.len(), bottom - page_height_px))
}

/// F30f: the lines of the paragraph `child` that the fragmenter places whole at `child_page_y` and that end below the
/// content box, as `(lines in the paragraph, how far below the page the last of them ends, the barrier that kept it
/// whole)`; `None` when none do.
///
/// A clipping box that hides content hides the lines below it in Chrome too: only the lines that are inside the clip
/// box and below the page count, and there may be none. Blaming pagination for a clip would be wrong.
fn unprinted_inline_lines(
    doc: &BaseDocument,
    parent: &blitz_dom::Node,
    child: &blitz_dom::Node,
    child_page_y: f32,
    page_height_px: f32,
) -> Option<(usize, f32, Option<LineBarrier>)> {
    let (lines, mut below) = inline_lines_below_page(child, child_page_y, page_height_px)?;
    let barrier = split_barrier(doc, parent, child);
    if barrier == Some(LineBarrier::ClipsItsContent) {
        // The bottom of the clip box (its padding box) in page coordinates.
        let clip_bottom = if clips_overflow(parent) {
            child_page_y - child.final_layout.location.y + parent.final_layout.size.height
                - parent.final_layout.border.bottom
        } else {
            child_page_y + child.final_layout.size.height - child.final_layout.border.bottom
        };
        let metrics = collect_inline_line_metrics(child);
        let first = metrics.first()?;
        let top = child_page_y + child.final_layout.border.top + child.final_layout.padding.top;
        let last = metrics.iter().rfind(|line| {
            top + (line.0 - first.0) < clip_bottom
                && top + (line.1 - first.0) > page_height_px + 1.0
        })?;
        below = (top + (last.1 - first.0)).min(clip_bottom) - page_height_px;
    }
    Some((lines, below, barrier))
}

/// Say so when [`unprinted_inline_lines`] finds lines that end below the content box, rather than leave them
/// unremarked, and name the box that kept the paragraph whole ([`split_barrier`]; F30f). Whether the lines are lost
/// depends on the page margin: what falls outside the page box is not printed.
fn warn_unprinted_inline_lines(
    doc: &BaseDocument,
    parent: &blitz_dom::Node,
    child: &blitz_dom::Node,
    child_page_y: f32,
    page_height_px: f32,
) {
    if let Some((lines, below, barrier)) =
        unprinted_inline_lines(doc, parent, child, child_page_y, page_height_px)
    {
        let what = barrier.map_or_else(String::new, |b| format!(" in {}", b.describe()));
        log::warn!(
            "pagination: a paragraph of {lines} lines{what} is placed whole and ends {below:.0}px below the \
             content box; lines outside the page box are not printed",
        );
    }
}

/// fulgur-p55h: split a multi-line inline root across page boundaries
/// at line edges, append one Fragment per page span to the geometry
/// table, and return the updated `(page_index, cursor_y, fragments_emitted)`.
///
/// Walks lines, tracks the first line of the current fragment in
/// `fragment_start_idx`, and splits when the cumulative height in
/// paragraph-local coords would push the bottom past
/// `strips.bottom(page) - paragraph_top_in_body`.
///
/// fulgur-s67g Phase 2.1 (widow / orphan): each candidate split point
/// must leave the **first** fragment with `>= ORPHANS_MIN` lines and
/// the **remainder** of the paragraph with `>= WIDOWS_MIN` lines
/// (CSS Fragmentation §4.4). When neither holds at the natural
/// overflow point, the split is skipped — subsequent lines accumulate
/// into the current fragment (overflow-tolerant) until a valid split
/// is found or the paragraph ends, in which case the paragraph emits
/// whole (oversized or pushed to a fresh page by sibling-driven
/// flow).
///
/// CSS `orphans` / `widows` properties are not parsed today, so the
/// fragmenter uses the CSS 3 Fragmentation defaults (`2` for both).
///
/// Output:
///
/// - On a single-page paragraph (no overflow), one Fragment is appended
///   covering all lines. `cursor_y` advances by the paragraph's natural
///   height.
/// - On a multi-page paragraph, one Fragment per page is appended. The
///   final `cursor_y` is the height consumed on the last page (lines
///   ending on a partial page leave room for a following sibling).
/// - On a paragraph with too few lines to honour orphans+widows
///   simultaneously (`< ORPHANS_MIN + WIDOWS_MIN` lines total), no
///   split is taken — the paragraph emits as one fragment, possibly
///   oversized.
///
/// Edge case: if the very first line on a fresh page is taller than
/// the page strip, the line is emitted as an oversized fragment (no
/// further mid-line split) — same fallback as the block branch.
///
/// The fourth element is one `(page, y - top, top)` per emitted fragment: where
/// its first line starts, for placing the paragraph's descendants (F32a).
#[allow(clippy::too_many_arguments)]
pub(crate) fn fragment_inline_root(
    geometry: &mut PaginationGeometryTable,
    child_id: usize,
    paragraph_x: f32,
    width: f32,
    initial_cursor_y: f32,
    initial_page_index: u32,
    strips: PageStrips,
    line_metrics: &[(f32, f32)],
) -> (u32, f32, usize, Vec<(u32, f32, f32)>) {
    /// CSS 3 Fragmentation default for `orphans`.
    const ORPHANS_MIN: usize = 2;
    /// CSS 3 Fragmentation default for `widows`.
    const WIDOWS_MIN: usize = 2;

    if line_metrics.is_empty() {
        return (initial_page_index, initial_cursor_y, 0, Vec::new());
    }

    let mut page_index = initial_page_index;
    let mut paragraph_top_in_body = initial_cursor_y;
    let mut fragment_start_idx: usize = 0;
    let mut emitted = 0usize;
    let total_lines = line_metrics.len();
    // `(page, y - top, top)` per fragment, as `record_split_inline_root_descendants` takes them.
    let mut placements: Vec<(u32, f32, f32)> = Vec::new();

    for (i, &(_line_top_local, line_bottom_local)) in line_metrics.iter().enumerate() {
        let frag_top_local = line_metrics[fragment_start_idx].0;
        let projected_bottom_in_body = paragraph_top_in_body + (line_bottom_local - frag_top_local);

        if projected_bottom_in_body > strips.bottom(page_index) && i > fragment_start_idx {
            // fulgur-s67g Phase 2.1: honour widow / orphan minimums.
            let first_size = i - fragment_start_idx;
            let remaining_size = total_lines - i;
            if first_size < ORPHANS_MIN || remaining_size < WIDOWS_MIN {
                // Cannot split here without violating widow/orphan.
                // Skip — keep accumulating into the current fragment.
                // If no future split point honours both constraints,
                // the loop falls through to "emit final fragment"
                // below and the paragraph emits as a single oversized
                // fragment (CSS Fragmentation §4.4: widows/orphans
                // constraints are treated as preferences that can be
                // relaxed to avoid unfulfillable breaks).
                continue;
            }

            // Lines [fragment_start_idx, i) fit on the current page.
            // Emit them as one fragment, advance to the next page, and
            // start the next fragment at line i.
            let prev_line_bottom = line_metrics[i - 1].1;
            let frag_h = prev_line_bottom - frag_top_local;
            let frag = Fragment {
                page_index,
                x: paragraph_x.as_px(),
                y: paragraph_top_in_body.as_px(),
                width: width.as_px(),
                height: frag_h.as_px(),
            };
            let entry = geometry.entry(child_id).or_default();
            entry.fragments.push(frag);
            entry.line_slices.push(LineSlice {
                first: fragment_start_idx as u32,
                end: i as u32,
                offset: (frag_top_local - line_metrics[0].0).as_px(),
            });
            placements.push((
                page_index,
                paragraph_top_in_body - frag_top_local,
                frag_top_local,
            ));
            emitted += 1;

            page_index += 1;
            paragraph_top_in_body = 0.0;
            fragment_start_idx = i;
        }
    }

    // Final fragment covers lines [fragment_start_idx, end).
    let frag_top_local = line_metrics[fragment_start_idx].0;
    let last_bottom_local = line_metrics.last().expect("non-empty checked above").1;
    let frag_h = last_bottom_local - frag_top_local;
    let frag = Fragment {
        page_index,
        x: paragraph_x.as_px(),
        y: paragraph_top_in_body.as_px(),
        width: width.as_px(),
        height: frag_h.as_px(),
    };
    let entry = geometry.entry(child_id).or_default();
    entry.fragments.push(frag);
    entry.line_slices.push(LineSlice {
        first: fragment_start_idx as u32,
        end: total_lines as u32,
        offset: (frag_top_local - line_metrics[0].0).as_px(),
    });
    debug_assert!(
        line_slices_tile(&entry.line_slices, total_lines),
        "fragment_inline_root: line slices do not tile 0..{total_lines}: {:?}",
        entry.line_slices
    );
    placements.push((
        page_index,
        paragraph_top_in_body - frag_top_local,
        frag_top_local,
    ));
    emitted += 1;

    let cursor_y = paragraph_top_in_body + frag_h;
    (page_index, cursor_y, emitted, placements)
}

/// Per-page state for a named string emitted by `string-set:`.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct StringSetPageState {
    /// Value at start of page (carried from previous page's `last`).
    pub start: Option<String>,
    /// First value set on this page.
    pub first: Option<String>,
    /// Last value set on this page.
    pub last: Option<String>,
}

/// Per-page state for running element instances of a given name.
#[derive(Debug, Clone, Default)]
pub struct PageRunningState {
    /// Instance IDs of running elements whose source position falls on
    /// this page, in source order.
    pub instance_ids: Vec<usize>,
}

/// fulgur-6tco: walk the geometry table page-by-page to thread
/// `string-set` state across pages.
///
/// For each page index 0..max_page:
///
/// 1. Initialise per-name `start` from the previous page's `last`
///    (the carry).
/// 2. For each node id whose **first** fragment lands on this page,
///    apply its `(name, value)` markers in NodeId order — records
///    `first` (only set once per page per name) and updates `last`
///    plus the carry for subsequent pages.
///
/// Markers fire only on a node's first appearance: when an inline
/// root spans two pages, its second-page fragment does **not** re-
/// emit the marker.
///
/// Source-order assumption: `geometry` is a `BTreeMap<usize, ..>` so
/// iteration is by ascending NodeId. For body's direct children that
/// matches DOM source order, since Blitz allocates ids sequentially
/// during parse. Nested string-set declarations (markers attached to
/// a `<span>` inside a `<p>`) are not in the fragmenter's geometry
/// table today and so are silently dropped — same scope limitation as
/// `fragment_pagination_root` itself.
pub fn collect_string_set_states(
    geometry: &PaginationGeometryTable,
    string_set_by_node: &BTreeMap<usize, Vec<(String, String)>>,
) -> Vec<BTreeMap<String, StringSetPageState>> {
    let max_page = geometry
        .values()
        .flat_map(|g| g.fragments.iter())
        .map(|f| f.page_index)
        .max()
        .map(|m| m + 1)
        .unwrap_or(1);

    // For each page, the list of nodes whose first fragment lands
    // there, in NodeId (≈ source) order.
    let mut nodes_per_page: Vec<Vec<usize>> = vec![Vec::new(); max_page as usize];
    for (&node_id, geom) in geometry {
        if let Some(first_frag) = geom.fragments.first()
            && (first_frag.page_index as usize) < nodes_per_page.len()
        {
            nodes_per_page[first_frag.page_index as usize].push(node_id);
        }
    }

    let mut result: Vec<BTreeMap<String, StringSetPageState>> =
        Vec::with_capacity(nodes_per_page.len());
    let mut carry: BTreeMap<String, String> = BTreeMap::new();

    for nodes in &nodes_per_page {
        let mut page_state: BTreeMap<String, StringSetPageState> = BTreeMap::new();
        for (name, value) in &carry {
            page_state.entry(name.clone()).or_default().start = Some(value.clone());
        }
        for node_id in nodes {
            let Some(entries) = string_set_by_node.get(node_id) else {
                continue;
            };
            for (name, value) in entries {
                let state = page_state.entry(name.clone()).or_default();
                if state.first.is_none() {
                    state.first = Some(value.clone());
                }
                state.last = Some(value.clone());
                carry.insert(name.clone(), value.clone());
            }
        }
        result.push(page_state);
    }

    result
}

/// F102: gives every registered running element that has no geometry a zero-height fragment at its source
/// position.
///
/// The engine hides a running element (`display: none`, CSS GCPM 3 §2.2) so it is not painted where it stands.
/// One directly under `<body>` still gets its zero-height fragment from the body loop, but a node below a
/// wrapper is zero-sized and `record_descendant` never records it, so
/// [`collect_running_element_states`] would not see it and its margin box would stay empty. Which page an
/// instance counts for follows its source position (GCPM §2.2.1, `first` / `last` / `first-except`), so such a
/// node gets the position the body loop's cursor would have had there: the bottom of the nearest previous
/// sibling that has geometry, or else the top of the nearest ancestor that has. The height is zero, so the
/// page count cannot grow. A registered node that already has geometry is left alone.
///
/// Nodes are taken in id (source) order and each placeholder is visible to the ones after it, so two running
/// elements in a row land together.
pub fn append_running_placeholders(
    geometry: &mut PaginationGeometryTable,
    doc: &BaseDocument,
    running_store: &crate::gcpm::running::RunningElementStore,
) {
    for node_id in running_store.node_ids() {
        if geometry
            .get(&node_id)
            .is_some_and(|g| !g.fragments.is_empty())
        {
            continue;
        }
        if let Some(fragment) = running_placeholder_fragment(geometry, doc, node_id) {
            geometry
                .entry(node_id)
                .or_default()
                .fragments
                .push(fragment);
        }
    }
}

/// The zero-height fragment [`append_running_placeholders`] gives `node_id`, or `None` when neither a previous
/// sibling nor an ancestor has geometry (a detached or fully hidden subtree).
fn running_placeholder_fragment(
    geometry: &PaginationGeometryTable,
    doc: &BaseDocument,
    node_id: usize,
) -> Option<Fragment> {
    use ::style::properties::longhands::position::computed_value::T as Pos;

    let zero = |page_index: u32, x: crate::units::Px, y: crate::units::Px| Fragment {
        page_index,
        x,
        y,
        width: 0.0_f32.as_px(),
        height: 0.0_f32.as_px(),
    };
    let mut current = node_id;
    for _ in 0..crate::MAX_DOM_DEPTH {
        let parent_id = doc.get_node(current)?.parent?;
        let parent = doc.get_node(parent_id)?;
        let at = parent.children.iter().position(|&c| c == current)?;
        for &sibling_id in parent.children[..at].iter().rev() {
            let Some(last) = geometry.get(&sibling_id).and_then(|g| g.fragments.last()) else {
                continue;
            };
            // An absolutely positioned or fixed sibling is out of flow: the cursor never moved for it.
            let out_of_flow = doc.get_node(sibling_id).is_some_and(|n| {
                n.primary_styles().is_some_and(|s| {
                    matches!(s.get_box().clone_position(), Pos::Absolute | Pos::Fixed)
                })
            });
            if out_of_flow {
                continue;
            }
            return Some(zero(last.page_index, last.x, last.y + last.height));
        }
        if let Some(first) = geometry.get(&parent_id).and_then(|g| g.fragments.first()) {
            return Some(zero(first.page_index, first.x, first.y));
        }
        current = parent_id;
    }
    None
}

/// Walk the geometry table page-by-page and emit the running element
/// instances whose first fragment lands on each page.
///
/// Each `instance_id` is adopted only once — on the page where its
/// node's first fragment lands. This matches the source-order policy
/// the margin-box renderer uses with `resolve_element_policy` to pick
/// the right instance for `first` / `last` / `first-except`.
pub fn collect_running_element_states(
    geometry: &PaginationGeometryTable,
    running_store: &crate::gcpm::running::RunningElementStore,
) -> Vec<BTreeMap<String, PageRunningState>> {
    let max_page = geometry
        .values()
        .flat_map(|g| g.fragments.iter())
        .map(|f| f.page_index)
        .max()
        .map(|m| m + 1)
        .unwrap_or(1);

    let mut result: Vec<BTreeMap<String, PageRunningState>> =
        vec![BTreeMap::new(); max_page as usize];

    for (&node_id, geom) in geometry {
        let Some(first_frag) = geom.fragments.first() else {
            continue;
        };
        let page_idx = first_frag.page_index as usize;
        if page_idx >= result.len() {
            continue;
        }
        let Some(instance_id) = running_store.instance_for_node(node_id) else {
            continue;
        };
        let Some(name) = running_store.name_of(instance_id) else {
            continue;
        };
        result[page_idx]
            .entry(name.to_string())
            .or_default()
            .instance_ids
            .push(instance_id);
    }

    result
}

/// fulgur-s67g Phase 2.3: walk the geometry table page-by-page and
/// replay counter operations in document order, returning the
/// cumulative counter snapshot at the end of each page.
///
/// Same source-order assumption as
/// [`collect_string_set_states`]: the per-node counter ops are
/// applied in the order they appear in the body's children list,
/// approximated by `BTreeMap<NodeId, _>` iteration. Nested counter
/// declarations on descendants of body's direct children are not in
/// the fragmenter's geometry today and are silently dropped — same
/// scope limitation as `fragment_pagination_root` itself.
pub fn collect_counter_states(
    geometry: &PaginationGeometryTable,
    counter_ops_by_node: &BTreeMap<usize, Vec<crate::gcpm::CounterOp>>,
) -> Vec<BTreeMap<String, i32>> {
    use crate::gcpm::CounterOp;
    use crate::gcpm::counter::CounterState;

    let max_page = geometry
        .values()
        .flat_map(|g| g.fragments.iter())
        .map(|f| f.page_index)
        .max()
        .map(|m| m + 1)
        .unwrap_or(1);

    // For each page, the list of nodes whose first fragment lands
    // there, in NodeId (≈ source) order.
    let mut nodes_per_page: Vec<Vec<usize>> = vec![Vec::new(); max_page as usize];
    for (&node_id, geom) in geometry {
        if let Some(first_frag) = geom.fragments.first()
            && (first_frag.page_index as usize) < nodes_per_page.len()
        {
            nodes_per_page[first_frag.page_index as usize].push(node_id);
        }
    }

    let mut state = CounterState::new();
    let mut result: Vec<BTreeMap<String, i32>> = Vec::with_capacity(nodes_per_page.len());

    for nodes in &nodes_per_page {
        for node_id in nodes {
            let Some(ops) = counter_ops_by_node.get(node_id) else {
                continue;
            };
            for op in ops {
                match op {
                    CounterOp::Reset { name, value } => state.reset(name, *value),
                    CounterOp::Increment { name, value } => state.increment(name, *value),
                    CounterOp::Set { name, value } => state.set(name, *value),
                }
            }
        }
        result.push(state.snapshot());
    }

    result
}

/// fulgur-jkl5: enumerate `position: fixed` elements and emit one
/// fragment per page so downstream rendering can repeat them on every
/// page (Chrome-compatible behaviour for paged media — see WPT
/// fixedpos-* family).
///
/// fulgur-rpvu: wired into the v2 production path. v2's
/// `dispatch_fragment` loop iterates `Fragment`s per (node_id, page),
/// so emitting one Fragment per page for each `position: fixed`
/// element produces the expected per-page repetition naturally. The
/// resulting `PaginationGeometry.is_repeat` is set to `true` so
/// consumers know each fragment carries the *full* content rather
/// than a slice (paragraph-line / block-height slicing must be
/// suppressed for repeat fragments). Both fixed-element paths
/// (v1 and v2) produce equivalent output.
///
/// `total_pages` is the document's resolved page count, typically
/// computed from `PaginationGeometryTable`'s max `page_index + 1` after
/// `run_pass*` has run. `0` is normalised to `1` so even an empty
/// document gets a valid fragment for any fixed element on it.
///
/// The fragment's `(x, y, width, height)` come from each fixed
/// element's existing `final_layout` — same coordinate frame the
/// non-paginated convert path already uses. **This function relies on
/// `blitz_adapter::relayout_position_fixed` (added in fulgur-tbxs,
/// branch `feat/fixedpos-viewport-cb`) having run beforehand** so
/// that `final_layout` reflects viewport-CB resolution rather than
/// the inherited (often wrong) abs-position layout. The fragmenter branch
/// does not yet include `relayout_position_fixed`; once both land on
/// `main` this function picks up the corrected positions automatically.
///
/// The emitted fragments are appended to `geometry` (typically the
/// table returned by `run_pass`) so a single side-table carries both
/// the body-fragmentation geometry and the fixed-element repetition.
/// Convert-side consumers (`convert::positioned.rs`) iterate the
/// node's `Vec<Fragment>` to place one copy of the element per page.
pub fn append_position_fixed_fragments(
    geometry: &mut PaginationGeometryTable,
    doc: &BaseDocument,
    total_pages: u32,
    viewport_w_px: f32,
    viewport_h_px: f32,
) {
    use ::style::properties::longhands::position::computed_value::T as Pos;

    let pages = total_pages.max(1);
    let body_offset_xy = body_origin_in_px(doc);
    let mut fixed_ids: Vec<usize> = Vec::new();
    let root_id = doc.root_element().id;
    walk_for_position_fixed(doc, root_id, &mut fixed_ids, 0);

    // Aggregate budget across all fixed roots and their descendants: each
    // node emits one fragment per page, so the retained total is
    // O((roots + descendants) × pages). `pages` is bounded by MAX_PAGES, but
    // the node axis is not — cap the total to keep the pass bounded on
    // untrusted input (`crate::MAX_SUBTREE_PAGE_FRAGMENTS`).
    let mut emitted: usize = 0;

    for id in fixed_ids {
        let Some(node) = doc.get_node(id) else {
            continue;
        };
        // Re-check style here even though `walk_for_position_fixed`
        // already filtered — guards against nodes whose style was
        // mutated between the walk and this read (defensive only,
        // single-threaded code path).
        let is_fixed = node
            .primary_styles()
            .is_some_and(|s| matches!(s.get_box().clone_position(), Pos::Fixed));
        if !is_fixed {
            continue;
        }
        let layout = node.final_layout;
        let (w, h) = (layout.size.width, layout.size.height);
        // fulgur-a8m5: Taffy's `compute_root_layout` (used by
        // `relayout_position_fixed`) does not resolve `bottom` / `right`
        // insets when the absolute element is the root of a layout
        // subtree — it places the element at (0, 0) regardless. The
        // CSS 2.1 §9.4 viewport CB needs explicit inset resolution
        // here, otherwise WPT fixedpos-001/002/008 render `bottom: 0`
        // fixed elements at the top of every page.
        let (resolved_x, resolved_y) =
            resolve_viewport_cb_location(node, w, h, viewport_w_px, viewport_h_px)
                .unwrap_or((layout.location.x, layout.location.y));
        // Render adds `body_offset_pt.y` to every fragment's y on page 0
        // (and only there) to account for the html→body offset (collapsed
        // margins from in-flow body-direct children, etc.). Fixed elements
        // are viewport-anchored, not body-anchored, so each page's fragment
        // goes through `icb_to_fragment`, which takes that offset off on
        // page 0 and leaves x alone (in-flow fragments already hold body's
        // x). Without this compensation, documents that mix in-flow content
        // with `position: fixed` (WPT fixedpos-008) shift the fixed text by
        // the in-flow div's margin-top; subtracting it on every page and
        // from x (html-to-pdf F40 / F187) moved the box on pages 2+ and
        // left of where its insets put it.
        let (x, y) = (resolved_x, resolved_y);

        let entry = geometry.entry(id).or_default();
        // Replace any prior placements (e.g. if the fixed element was
        // also walked by `fragment_pagination_root` and emitted as a
        // single fragment). Per-page repetition is the canonical
        // representation for fixed content.
        entry.fragments.clear();
        entry.line_slices.clear();
        entry.is_repeat = true;
        for page_index in 0..pages {
            if emitted >= crate::MAX_SUBTREE_PAGE_FRAGMENTS {
                break;
            }
            let (frag_x, frag_y) = icb_to_fragment(page_index, x, y, body_offset_xy.1);
            entry.fragments.push(Fragment {
                page_index,
                x: frag_x.as_px(),
                y: frag_y.as_px(),
                width: w.as_px(),
                height: h.as_px(),
            });
            emitted += 1;
        }

        // fulgur-4m16: emit per-page repeated fragments for every
        // in-flow descendant of the fixed root. v2 dispatch is
        // geometry-driven and reads fragments per `node_id`, so a
        // fixed root with a sized block-element child (e.g. WPT
        // fixedpos-009 `<div style="position:fixed; bottom:0; right:0">
        // <div class="pencil" style="width:36px; height:36px;
        // background:black; mask-image:..."></div></div>`) needs an
        // entry for the pencil child or v2 never reaches it. The
        // existing root-only fragment carries inline text rendering
        // (fixedpos-001 / 008 ref pattern) but not block descendants.
        // Always walk the subtree (even once the budget is spent): the walk
        // clears every descendant's entry, which is required because this
        // fixed pass runs a second time after the absolute pass and must not
        // leave stale fragments from the first pass behind (Codex review). The
        // per-node push is itself bounded by the `emitted` budget.
        record_fixed_subtree_descendants(
            geometry,
            doc,
            id,
            (x, y),
            body_offset_xy.1,
            pages,
            &mut emitted,
        );
    }

    // Don't allocate empty entries for nodes without fragments.
    geometry.retain(|_, geom| !geom.fragments.is_empty());
}

/// fulgur-4m16: walk every in-flow descendant of a `position: fixed`
/// root and emit one fragment per page (`is_repeat = true`) at the
/// descendant's offset within the fixed subtree, anchored to the
/// root's already-resolved viewport-CB position.
///
/// Mirrors [`record_subtree_fragments_at_offset`] (used by
/// `append_position_absolute_body_direct_fragments`) except:
///   - fragments repeat on every page instead of landing on a single
///     y-derived page,
///   - the caller passes the root's resolved (x, y) in the initial
///     containing block so we don't re-resolve viewport-CB here (the
///     root's `final_layout` can lack end-side inset resolution — see
///     fulgur-a8m5),
///   - each page's fragment goes through [`icb_to_fragment`] with body's
///     y offset (taken off on page 0 only), as the root's own does.
///
/// Skips out-of-flow descendants (handled by their own pass) and
/// whitespace-only text nodes (matches fragmenter behavior).
fn record_fixed_subtree_descendants(
    geometry: &mut PaginationGeometryTable,
    doc: &BaseDocument,
    fixed_root_id: usize,
    root_stored_xy: (f32, f32),
    body_offset_y: f32,
    pages: u32,
    emitted: &mut usize,
) {
    use ::style::properties::longhands::position::computed_value::T as Pos;

    #[allow(clippy::too_many_arguments)]
    fn walk(
        geometry: &mut PaginationGeometryTable,
        doc: &BaseDocument,
        node_id: usize,
        offset_in_subtree: (f32, f32),
        root_stored_xy: (f32, f32),
        body_offset_y: f32,
        pages: u32,
        depth: usize,
        emitted: &mut usize,
    ) {
        if depth >= crate::MAX_DOM_DEPTH {
            return;
        }
        let Some(node) = doc.get_node(node_id) else {
            return;
        };
        let stored_x = root_stored_xy.0 + offset_in_subtree.0;
        let stored_y = root_stored_xy.1 + offset_in_subtree.1;
        let w = node.final_layout.size.width;
        let h = node.final_layout.size.height;

        let entry = geometry.entry(node_id).or_default();
        entry.fragments.clear();
        entry.line_slices.clear();
        entry.is_repeat = true;
        // Per-page repeat fragments, bounded by the aggregate `emitted` budget
        // (the hard cap on O(descendants × pages)). Use `break`, not `return`,
        // and do not skip zero-area nodes: the walk must visit and clear every
        // descendant (the fixed pass runs twice — before and after the absolute
        // pass — so a later node can hold stale fragments from the first pass),
        // and a zero-size wrapper's fragment can still scope a visible child via
        // transform / opacity dispatch, which reads the wrapper's geometry
        // (Codex review).
        for page_index in 0..pages.max(1) {
            if *emitted >= crate::MAX_SUBTREE_PAGE_FRAGMENTS {
                break;
            }
            let (frag_x, frag_y) = icb_to_fragment(page_index, stored_x, stored_y, body_offset_y);
            entry.fragments.push(Fragment {
                page_index,
                x: frag_x.as_px(),
                y: frag_y.as_px(),
                width: w.as_px(),
                height: h.as_px(),
            });
            *emitted += 1;
        }

        let children: Vec<usize> = {
            let layout_borrow = node.layout_children.borrow();
            if let Some(lc) = layout_borrow.as_deref()
                && !lc.is_empty()
            {
                lc.to_vec()
            } else {
                node.children.clone()
            }
        };
        for child_id in children {
            let Some(child) = doc.get_node(child_id) else {
                continue;
            };
            let is_oof = child.primary_styles().is_some_and(|s| {
                matches!(s.get_box().clone_position(), Pos::Absolute | Pos::Fixed)
            });
            if is_oof {
                continue;
            }
            if let Some(text) = child.text_data()
                && text.content.chars().all(char::is_whitespace)
            {
                continue;
            }
            let child_offset = (
                offset_in_subtree.0 + child.final_layout.location.x,
                offset_in_subtree.1 + child.final_layout.location.y,
            );
            walk(
                geometry,
                doc,
                child_id,
                child_offset,
                root_stored_xy,
                body_offset_y,
                pages,
                depth + 1,
                emitted,
            );
        }
    }

    let Some(root) = doc.get_node(fixed_root_id) else {
        return;
    };
    let children: Vec<usize> = {
        let layout_borrow = root.layout_children.borrow();
        if let Some(lc) = layout_borrow.as_deref()
            && !lc.is_empty()
        {
            lc.to_vec()
        } else {
            root.children.clone()
        }
    };
    for child_id in children {
        let Some(child) = doc.get_node(child_id) else {
            continue;
        };
        let is_oof = child
            .primary_styles()
            .is_some_and(|s| matches!(s.get_box().clone_position(), Pos::Absolute | Pos::Fixed));
        if is_oof {
            continue;
        }
        if let Some(text) = child.text_data()
            && text.content.chars().all(char::is_whitespace)
        {
            continue;
        }
        let child_offset = (child.final_layout.location.x, child.final_layout.location.y);
        walk(
            geometry,
            doc,
            child_id,
            child_offset,
            root_stored_xy,
            body_offset_y,
            pages,
            1,
            emitted,
        );
    }
}

/// fulgur-a8m5: emit a Fragment for every body-direct
/// `position: absolute` element whose effective containing block falls
/// back to the viewport (when body's box collapses to zero because all
/// of its children are out-of-flow — see CSS 2.1 §10.1.5 and the
/// matching `viewport_size_px` body-zero fallback in
/// `convert::positioned::resolve_cb_for_absolute`).
///
/// `fragment_pagination_root` skips out-of-flow children unconditionally,
/// so without this pass `<body><div style="position:absolute; bottom:0">…</div></body>`
/// never reaches `pagination_geometry` and the v2 dispatch loop drops
/// the element entirely (WPT `fixedpos-00{1,2,8}` ref-side breakage).
///
/// Each visited in-flow node emits fragments for every page intersected
/// by its resolved y range; off-page elements (e.g. `bottom: -100vh` in
/// a single-page document) are dropped because no page can paint them.
pub fn append_position_absolute_body_direct_fragments(
    geometry: &mut PaginationGeometryTable,
    doc: &BaseDocument,
    total_pages: u32,
    viewport_w_px: f32,
    viewport_h_px: f32,
    running_store: Option<&crate::gcpm::running::RunningElementStore>,
) {
    use ::style::properties::longhands::position::computed_value::T as Pos;

    let pages = total_pages.max(1);
    let body_id = match find_body_id(doc) {
        Some(id) => id,
        None => return,
    };
    let body = match doc.get_node(body_id) {
        Some(n) => n,
        None => return,
    };
    // Per CSS 2.1 §10.1.5, the containing block for `position: absolute`
    // children of `<body>` (a static-position element) falls through to
    // the initial containing block (the viewport) regardless of body's
    // own size. The fragmenter unconditionally skips out-of-flow
    // children (`fragment_pagination_root` `continue` for `Pos::Absolute`),
    // so this pass runs for every body-direct abs — not just when body
    // collapses to zero.
    let body_offset_xy = body_origin_in_px(doc);

    // Aggregate budget across all body-direct absolute subtrees: each node is
    // recorded once per intersected page, so many page-spanning absolutes are
    // O(nodes × pages). Cap the retained total (shared with the fixed pass via
    // `crate::MAX_SUBTREE_PAGE_FRAGMENTS`).
    let mut emitted: usize = 0;

    let body_children = body.children.clone();
    let body_has_in_flow_content = body_children.iter().any(|&child_id| {
        let Some(child) = doc.get_node(child_id) else {
            return false;
        };
        if let Some(text) = child.text_data()
            && text.content.chars().all(char::is_whitespace)
        {
            return false;
        }
        if running_store.is_some_and(|s| s.instance_for_node(child_id).is_some()) {
            return false;
        }
        !is_out_of_flow_positioned(child) && !crate::blitz_adapter::node_is_floating(child)
    });
    for child_id in body_children {
        let Some(child) = doc.get_node(child_id) else {
            continue;
        };
        let is_abs_only = child
            .primary_styles()
            .is_some_and(|s| matches!(s.get_box().clone_position(), Pos::Absolute));
        if !is_abs_only {
            continue;
        }
        let (resolved_x, resolved_y) =
            body_direct_root_icb(child, body, viewport_w_px, viewport_h_px);
        let page_stride_px = if uses_bottom_without_top(child) {
            viewport_h_px.round()
        } else {
            viewport_h_px
        };

        // Walk the subtree and emit fragments for every page each
        // in-flow node intersects (block, paragraph, anonymous wrapper).
        // Without this the dispatch loop drops descendants whose node_id
        // is not the root abs id (anonymous block wrappers around mixed
        // text/element content — fixedpos-002 / fixedpos-008 ref-side
        // pattern). Out-of-flow descendants are skipped because they
        // are handled by their own pass. `(resolved_x, resolved_y)` is
        // the box in the initial containing block; the walker stores it
        // in the fragmenter's frame ([`icb_to_fragment`]: the render path
        // adds body's y offset on page 0 only) while keeping page
        // assignment based on the ICB y.
        record_subtree_fragments_at_offset(
            geometry,
            doc,
            child_id,
            (resolved_x, resolved_y),
            body_offset_xy.1,
            viewport_h_px,
            page_stride_px,
            pages,
            !body_has_in_flow_content,
            &mut emitted,
        );
    }

    geometry.retain(|_, geom| !geom.fragments.is_empty());
}

/// Walk a body-direct out-of-flow subtree and emit Fragments on every
/// intersected page for each in-flow node (the subtree root + every
/// block / paragraph / anonymous wrapper inside it). Each fragment's
/// body-relative location = the subtree root's resolved viewport-CB
/// location plus the node's accumulated `final_layout.location` offset
/// from that root.
///
/// Skips out-of-flow descendants (their own pass handles them) and
/// whitespace-only text nodes (mirrors `fragment_pagination_root`).
#[allow(clippy::too_many_arguments)]
fn record_subtree_fragments_at_offset(
    geometry: &mut PaginationGeometryTable,
    doc: &BaseDocument,
    subtree_root_id: usize,
    root_xy_for_paging: (f32, f32),
    body_offset_y: f32,
    page_h_px: f32,
    page_stride_px: f32,
    total_pages: u32,
    may_extend_pages: bool,
    emitted: &mut usize,
) {
    #[allow(clippy::too_many_arguments)]
    fn walk(
        geometry: &mut PaginationGeometryTable,
        doc: &BaseDocument,
        node_id: usize,
        offset_in_subtree: (f32, f32),
        root_xy_for_paging: (f32, f32),
        body_offset_y: f32,
        page_h_px: f32,
        page_stride_px: f32,
        total_pages: u32,
        may_extend_pages: bool,
        // fulgur-xa9q: true once we are at or below an overflow-clipping
        // ancestor (`clips_overflow`: any non-`visible` overflow). Inside such
        // a subtree the "START beyond budget extends" exception is suppressed
        // for descendants, so clipped overflow cannot generate pages
        // (page-background-003 cat box, which is `overflow:clip`). `contain:
        // size` alone is NOT a clip and does not set this boundary.
        containment_boundary: bool,
        // Subtree-offset and border-box size of the nearest positioned
        // ancestor of `node_id` — the containing block that `node_id`'s
        // out-of-flow children resolve their explicit insets against (CSS
        // 2.1 §10.1.4). The body-direct abs at the subtree root is itself
        // positioned, so it overrides these on the first frame.
        cb_anchor: (f32, f32),
        cb_size: (f32, f32),
        depth: usize,
        emitted: &mut usize,
    ) {
        if depth >= crate::MAX_DOM_DEPTH {
            return;
        }
        let Some(node) = doc.get_node(node_id) else {
            return;
        };
        // Prefer `layout_children` so anonymous block wrappers Stylo
        // synthesizes around mixed inline/block content are visited
        // (CSS 2.1 §9.2.1.1 — see `fragment_pagination_root` for the
        // same idiom).
        let children: Vec<usize> = {
            let layout_borrow = node.layout_children.borrow();
            if let Some(lc) = layout_borrow.as_deref()
                && !lc.is_empty()
            {
                lc.to_vec()
            } else {
                node.children.clone()
            }
        };
        // The containing block for THIS node's out-of-flow children is
        // `node` itself when `node` is positioned (non-static), otherwise the
        // inherited nearest-positioned ancestor (`cb_anchor`/`cb_size`). A
        // `position: static` element does not establish a CB, so an abs child
        // beneath it anchors to the same ancestor its static parent does.
        //
        // The CB is the positioned ancestor's *padding box* (CSS 2.1
        // §10.1.4): the anchor backs in by its top/left border and the size
        // drops both borders. `offset_in_subtree` is `node`'s border-box
        // origin, so add the border to reach the padding edge.
        let node_positioned = {
            use ::style::properties::longhands::position::computed_value::T as Pos;
            node.primary_styles()
                .is_some_and(|s| !matches!(s.get_box().clone_position(), Pos::Static))
        };
        let (child_cb_anchor, child_cb_size) = if node_positioned {
            let border = node.final_layout.border;
            (
                (
                    offset_in_subtree.0 + border.left,
                    offset_in_subtree.1 + border.top,
                ),
                (
                    node.final_layout.size.width - border.left - border.right,
                    node.final_layout.size.height - border.top - border.bottom,
                ),
            )
        } else {
            (cb_anchor, cb_size)
        };
        // Page assignment is based on the viewport-CB resolved position in
        // the initial containing block (the actual paint location). Storage
        // is in the fragmenter's frame ([`icb_to_fragment`]): x is
        // page-area-relative as is, y has body's offset taken off on page 0
        // because the dispatch path adds `body_offset_pt` back there only.
        let final_y_for_paging = root_xy_for_paging.1 + offset_in_subtree.1;
        let stored_x = root_xy_for_paging.0 + offset_in_subtree.0;
        let w = node.final_layout.size.width;
        let h = node.final_layout.size.height;
        let is_size_contained = has_contain_size(node);
        let monolithic_adjust: f32 = children
            .iter()
            .filter_map(|child_id| doc.get_node(*child_id))
            .filter(|child| !is_out_of_flow_positioned(child) && has_contain_size(child))
            .map(|child| (child.final_layout.size.height - page_h_px).max(0.0))
            .sum();
        let h_for_paging = (h - monolithic_adjust).max(0.0);
        let mut descendant_total_pages = total_pages;

        if final_y_for_paging.is_finite()
            && h.is_finite()
            && h_for_paging.is_finite()
            && page_h_px > 0.0
            && h_for_paging > 0.0
        {
            // Stylo computes `Nvh` against the viewport snapshot taken
            // at parse time, which can differ from `page_h_px` by a
            // sub-px amount (the @page resolution uses the resolved
            // content area; Stylo's computed `100vh` rounds elsewhere).
            // Without tolerance, `top: 100vh` on a 1-page render
            // becomes `final_y = 971.0` against `page_h_px = 971.34`,
            // which floor()s to page 0 and renders the off-page text
            // on page 1 (WPT fixedpos-008 ref-side). Snap final_y
            // toward integer multiples of page_h before paging.
            let start_ratio = final_y_for_paging / page_h_px;
            let start_round = start_ratio.round();
            // fulgur-xa9q: the Stylo `Nvh` sub-px residual vs `page_h_px` scales
            // with the `vh` MULTIPLE summed into `final_y_for_paging` — Stylo's
            // per-`vh`-unit rounding error is multiplied by the vh count, so a
            // `top:400vh` lands ~1.35px short of `4*page_h` and a `top:500vh`
            // further, regardless of nesting depth. A flat `1e-3` tolerance
            // misses this and mis-pages the element onto the page boundary so
            // its line splits and clips (fixedpos-005 "fifth", fixedpos-008
            // page 6). Scale the tolerance by the rounded page (≈ the vh
            // multiple), capped at 1% of the page (~half a line) so extreme
            // page counts cannot grow the window unbounded. NOTE: depth-scaling
            // was tried and regresses fixedpos-008 — the residual tracks the vh
            // multiple, not nesting depth (Codex review on PR #498).
            let snap_tol = (1e-3 * start_round.abs().max(1.0)).min(0.01);
            let start_is_snapped = (start_ratio - start_round).abs() < snap_tol;
            let snapped_start_ratio = if start_is_snapped {
                start_round
            } else {
                start_ratio
            };
            let bottom_y_for_paging = final_y_for_paging + h_for_paging;
            let bottom_ratio = bottom_y_for_paging / page_h_px;
            let mut last_page_f =
                if bottom_y_for_paging.is_infinite() && bottom_y_for_paging.is_sign_positive() {
                    total_pages.saturating_sub(1) as f32
                } else if (bottom_ratio - bottom_ratio.round()).abs() < 1e-6 {
                    bottom_ratio.round() - 1.0
                } else {
                    bottom_ratio.floor()
                };
            let first_page_f = snapped_start_ratio.floor().max(0.0);
            if is_size_contained {
                last_page_f = first_page_f;
            }
            // fulgur-xa9q: the start snap can advance `first_page_f` onto the
            // next page while `last_page_f` still floors from the UNSNAPPED
            // bottom; for a box shorter than the corrected residual that would
            // make `first_page_f > last_page_f` and drop the fragment entirely
            // (Codex review). A box must appear on at least its (snapped) start
            // page, so never let the last page precede the first.
            last_page_f = last_page_f.max(first_page_f);
            // fulgur-xa9q: an abs whose START is at/beyond the existing in-flow
            // page budget extends the page count even with in-flow content
            // present (Chrome-compatible) — UNLESS it sits inside a containment
            // /clip boundary, where overflow is invisible and must not paginate.
            // A tall abs anchored WITHIN the budget stays clamped (it has an
            // in-budget page to clip onto) so short-flow layouts do not grow.
            let node_may_extend =
                may_extend_pages || (first_page_f >= total_pages as f32 && !containment_boundary);
            if first_page_f.is_finite()
                && last_page_f.is_finite()
                && first_page_f <= last_page_f
                && (node_may_extend || first_page_f < total_pages as f32)
            {
                // fulgur-2m6w: clamp the emitted page index to `MAX_PAGES`,
                // but ONLY on the page-EXTENSION path. `first_page_f` /
                // `last_page_f` derive from the abs element's viewport-CB
                // resolved Y, which is attacker-controlled CSS
                // (`position:absolute; top:99999999px`). When the element
                // extends the page count (`node_may_extend`) an unclamped
                // value lands at page ~10^5, `descendant_total_pages`
                // propagates that to `implied_page_count`, and `render_v2`
                // allocates + renders every intervening page — the same
                // small-input DoS the body-direct slice cap blocks. This
                // `walk` runs for the abs root AND nested abs descendants,
                // so one clamp bounds both.
                //
                // The NON-extending branch is already bounded by
                // `total_pages` (the in-flow page count, itself capped /
                // input-proportional), so it must stay UNCLAMPED: clamping
                // `first_page` while `last_page` keeps `min(.., total_pages-1)`
                // would emit a spurious fragment run from `MAX_PAGES` through
                // the real (in-budget) page when `total_pages > MAX_PAGES`
                // from many ordinary in-flow pages (Codex review on PR #501).
                let (first_page, last_page) = if node_may_extend {
                    (
                        (first_page_f as u32).min(crate::MAX_PAGES),
                        (last_page_f as u32).min(crate::MAX_PAGES),
                    )
                } else {
                    (
                        first_page_f as u32,
                        (last_page_f as u32).min(total_pages.saturating_sub(1)),
                    )
                };
                // fulgur-xa9q: page ASSIGNMENT snaps the start ratio to the page
                // grid (`first_page_f` via `snapped_start_ratio`), but the stored
                // Y must use the SAME snapped origin. Otherwise a `top: 100vh`
                // abs lands ~1px off the in-flow fragmenter's exact page-top
                // placement (the Stylo `100vh` vs `page_h_px` sub-px residual),
                // producing the fixedpos-005/006/008 page-2 pixel diff once the
                // page count matches. Snap only the paging origin, and only for
                // top-anchored subtrees (`page_stride_px == page_h_px`); the
                // bottom-only repeat idiom uses a rounded stride and must keep
                // its raw origin.
                let paging_origin_y = if page_stride_px == page_h_px && start_is_snapped {
                    snapped_start_ratio * page_h_px
                } else {
                    final_y_for_paging
                };
                let entry = geometry.entry(node_id).or_default();
                entry.fragments.clear();
                entry.line_slices.clear();
                entry.is_repeat = false;
                for page_index in first_page..=last_page {
                    // Aggregate budget: each node emits one fragment per
                    // intersected page, so a subtree of many page-spanning
                    // absolutes is O(nodes × pages). Stop past the cap
                    // (`crate::MAX_SUBTREE_PAGE_FRAGMENTS`) — shared with the
                    // fixed pass. Use `break`, not `return`: the walk must keep
                    // visiting (and clearing) the rest of the subtree so no
                    // stale fragments survive from an earlier pass (Codex
                    // review).
                    if *emitted >= crate::MAX_SUBTREE_PAGE_FRAGMENTS {
                        break;
                    }
                    let is_monolithic_continuation =
                        monolithic_adjust > 0.0 && page_index > first_page;
                    let y_on_page = if is_monolithic_continuation {
                        0.0
                    } else {
                        paging_origin_y - (page_index as f32) * page_stride_px
                    };
                    let (_, stored_y) =
                        icb_to_fragment(page_index, stored_x, y_on_page, body_offset_y);
                    let stored_h = if is_monolithic_continuation {
                        let consumed = (page_index - first_page) as f32 * page_h_px;
                        (h_for_paging - consumed).clamp(0.0, page_h_px)
                    } else {
                        h
                    };
                    entry.fragments.push(Fragment {
                        page_index,
                        x: stored_x.as_px(),
                        y: stored_y.as_px(),
                        width: w.as_px(),
                        height: stored_h.as_px(),
                    });
                    *emitted += 1;
                }
                descendant_total_pages = descendant_total_pages.max(last_page.saturating_add(1));
            }
        }

        let mut monolithic_y_adjust = 0.0;
        for child_id in children {
            let Some(child) = doc.get_node(child_id) else {
                continue;
            };
            // Out-of-flow descendants:
            //   - `position: fixed` is a repeat element handled by
            //     `append_position_fixed_fragments`; skip it here so it
            //     does not contribute to extent (it would otherwise be
            //     double-counted).
            //   - `position: absolute` (fulgur-puml #1): a nested abs's
            //     height/offset must still drive the page count. Resolve
            //     its location against ITS containing block (the nearest
            //     positioned ancestor = the current `node`'s box) via
            //     `resolve_viewport_cb_location`, fold that CB-relative
            //     (x, y) into the accumulated `offset_in_subtree`, and
            //     recurse. `root_xy_for_paging` stays the body-direct
            //     abs root anchor; viewport-relative (`vh`) insets are
            //     CB-independent, so the CB dims only affect percentage
            //     insets.
            {
                use ::style::properties::longhands::position::computed_value::T as Pos;
                match child.primary_styles().map(|s| s.get_box().clone_position()) {
                    // Fixed is handled by its own pass; skip here.
                    Some(Pos::Fixed) => continue,
                    Some(Pos::Absolute) => {
                        let (child_w, child_h) = (
                            child.final_layout.size.width,
                            child.final_layout.size.height,
                        );
                        // Resolve against the nearest positioned ancestor's
                        // padding box (`child_cb_*`), NOT the immediate DOM
                        // parent — a `position: static` parent does not
                        // establish a CB (CSS 2.1 §10.1.4).
                        let (rel_x, rel_y) = resolve_viewport_cb_location(
                            child,
                            child_w,
                            child_h,
                            child_cb_size.0,
                            child_cb_size.1,
                        )
                        .unwrap_or((child.final_layout.location.x, child.final_layout.location.y));
                        // Per-axis anchor: an explicit inset is relative to the
                        // CB origin (`child_cb_anchor`); an `auto` inset keeps
                        // the static position, i.e. the flow offset relative to
                        // the immediate parent (`offset_in_subtree`). For a
                        // directly-nested abs the immediate parent IS the
                        // positioned ancestor, so both bases coincide and this
                        // is a no-op; they diverge only across a static
                        // intermediate.
                        let (explicit_x, explicit_y) = explicit_inset_axes(child);
                        let base_x = if explicit_x {
                            child_cb_anchor.0
                        } else {
                            offset_in_subtree.0
                        };
                        let base_y = if explicit_y {
                            child_cb_anchor.1
                        } else {
                            offset_in_subtree.1
                        };
                        let nested_offset = (base_x + rel_x, base_y + rel_y);
                        walk(
                            geometry,
                            doc,
                            child_id,
                            nested_offset,
                            root_xy_for_paging,
                            body_offset_y,
                            page_h_px,
                            page_stride_px,
                            descendant_total_pages,
                            may_extend_pages,
                            containment_boundary || clips_overflow(node),
                            child_cb_anchor,
                            child_cb_size,
                            depth + 1,
                            emitted,
                        );
                        continue;
                    }
                    // In-flow (or unstyled): fall through to the normal path below.
                    _ => {}
                }
            }
            // Skip whitespace-only text (matches fragmenter).
            if let Some(text) = child.text_data()
                && text.content.chars().all(char::is_whitespace)
            {
                continue;
            }
            let child_offset = (
                offset_in_subtree.0 + child.final_layout.location.x,
                offset_in_subtree.1 + child.final_layout.location.y - monolithic_y_adjust,
            );
            walk(
                geometry,
                doc,
                child_id,
                child_offset,
                root_xy_for_paging,
                body_offset_y,
                page_h_px,
                page_stride_px,
                descendant_total_pages,
                may_extend_pages,
                containment_boundary || clips_overflow(node),
                child_cb_anchor,
                child_cb_size,
                depth + 1,
                emitted,
            );
            if has_contain_size(child) {
                monolithic_y_adjust += (child.final_layout.size.height - page_h_px).max(0.0);
            }
        }
    }

    walk(
        geometry,
        doc,
        subtree_root_id,
        (0.0, 0.0),
        root_xy_for_paging,
        body_offset_y,
        page_h_px,
        page_stride_px,
        total_pages,
        may_extend_pages,
        // fulgur-xa9q: the subtree root starts outside any containment boundary.
        false,
        // Initial CB: the subtree root is the body-direct abs (positioned),
        // so it overrides these on the first frame — seed with the root's own
        // anchor/size for correctness if that ever changes.
        (0.0, 0.0),
        (0.0, 0.0),
        0,
        emitted,
    );
}

/// Which axes of an out-of-flow element carry an explicit (length /
/// percentage) inset, as `(x, y)`. An explicit inset is resolved against the
/// containing block; an `auto` inset falls back to the static (flow) position.
/// Mirrors the per-axis idiom in [`resolve_viewport_cb_location`].
fn explicit_inset_axes(node: &blitz_dom::Node) -> (bool, bool) {
    use ::style::values::generics::position::GenericInset;

    fn is_length_percentage(inset: &::style::values::computed::position::Inset) -> bool {
        matches!(inset, GenericInset::LengthPercentage(_))
    }

    let Some(styles) = node.primary_styles() else {
        return (false, false);
    };
    let pos = styles.get_position();
    (
        is_length_percentage(&pos.left) || is_length_percentage(&pos.right),
        is_length_percentage(&pos.top) || is_length_percentage(&pos.bottom),
    )
}

fn is_out_of_flow_positioned(node: &blitz_dom::Node) -> bool {
    use ::style::properties::longhands::position::computed_value::T as Pos;

    node.primary_styles()
        .is_some_and(|s| matches!(s.get_box().clone_position(), Pos::Absolute | Pos::Fixed))
}

fn has_contain_size(node: &blitz_dom::Node) -> bool {
    node.primary_styles().is_some_and(|s| {
        s.get_box()
            .clone_contain()
            .contains(::style::values::computed::box_::Contain::SIZE)
    })
}

/// Whether `node` establishes a clip context that hides its overflowing
/// descendants from painting — used as the page-extension boundary
/// (fulgur-xa9q): a descendant whose paint overflow is clipped here cannot
/// generate pages even when it lands past the in-flow budget.
///
/// Two conditions, both required:
///   1. `overflow` is not `visible` on some axis (`hidden`/`clip`/`scroll`/
///      `auto` all clip; mirrors `convert::style::overflow`).
///   2. the box actually establishes a clip context — a block-level box or an
///      atomic inline / BFC root. A non-replaced `display: inline` box ignores
///      `overflow` and the renderer pushes no clip scope for it, so an inline
///      wrapper like `<span style="overflow:hidden">` must NOT act as a
///      boundary (Codex review on PR #498).
///
/// NOTE: `contain: size` is deliberately NOT treated as a clip — it sizes the
/// box without its contents but leaves overflow visible, so a
/// `contain:size; overflow:visible` box must still let descendants extend.
fn clips_overflow(node: &blitz_dom::Node) -> bool {
    use ::style::values::computed::Overflow as Ov;
    use ::style::values::specified::box_::{DisplayInside, DisplayOutside};
    node.primary_styles().is_some_and(|s| {
        let clips = s.clone_overflow_x() != Ov::Visible || s.clone_overflow_y() != Ov::Visible;
        if !clips {
            return false;
        }
        let display = s.clone_display();
        display.outside() == DisplayOutside::Block || display.inside() != DisplayInside::Flow
    })
}

/// CSS-px (x, y) of `<body>`'s top-left in its containing block (html).
/// Mirrors `convert::extract_body_offset_pt` but stays in CSS px so
/// pagination_layout doesn't need to round-trip through pt. The render
/// path adds `drawables.body_offset_pt.1` to a page-0 fragment's y (only
/// there) and nothing to x when dispatching, so viewport-anchored
/// fragments go through [`icb_to_fragment`] with this offset to keep the
/// dispatched position page-area-relative.
fn body_origin_in_px(doc: &BaseDocument) -> (f32, f32) {
    let Some(body_id) = find_body_id(doc) else {
        return (0.0, 0.0);
    };
    let Some(body) = doc.get_node(body_id) else {
        return (0.0, 0.0);
    };
    (body.final_layout.location.x, body.final_layout.location.y)
}

/// The frame the in-flow fragmenter writes and `render_v2` reads (html-to-pdf F187, the F29 rule), for a point that lies
/// `x_icb` px right of the page area's left edge and `y_on_page` px below its top on page `page_index`: `x` is
/// page-area-relative (in-flow fragments already hold body's x offset), `y` is page-area-relative less body's y offset
/// on page 0 only, because `render_v2` adds `body_offset` there and nowhere else. Every pass that writes geometry for a
/// box placed in the initial containing block (the body-direct absolute pass, the fixed pass) goes through this one
/// function. CSS px in, CSS px out.
pub(crate) fn icb_to_fragment(
    page_index: u32,
    x_icb: f32,
    y_on_page: f32,
    body_offset_y: f32,
) -> (f32, f32) {
    (
        x_icb,
        y_on_page - if page_index == 0 { body_offset_y } else { 0.0 },
    )
}

/// Whether `body`'s `position` is not `static`: it is then the containing block of its absolutely positioned children
/// (CSS 2.1 section 10.1 (4)) instead of the initial containing block.
fn is_positioned_body(body: &blitz_dom::Node) -> bool {
    use ::style::properties::longhands::position::computed_value::T as Pos;
    body.primary_styles()
        .is_some_and(|s| !matches!(s.get_box().clone_position(), Pos::Static))
}

/// The top-left of the border box of the absolutely positioned child `child` of `body` in the initial containing
/// block (page-area-relative CSS px; the page-0 stitched frame). CSS 2.1 section 10.3.7 / 10.6.4:
///
/// - static body: the containing block is the initial containing block of `viewport_w_px` x `viewport_h_px`; an axis
///   with an inset is resolved against it ([`resolve_viewport_cb_location`]); an axis with both insets `auto` keeps the
///   static position, which Taffy reports relative to body's border box, so body's own origin is added;
/// - positioned body: Taffy has already resolved both axes against body's padding box (insets and `%` included), so the
///   box is at body's origin plus `final_layout.location` on both axes.
fn body_direct_root_icb(
    child: &blitz_dom::Node,
    body: &blitz_dom::Node,
    viewport_w_px: f32,
    viewport_h_px: f32,
) -> (f32, f32) {
    let layout = child.final_layout;
    let origin = (body.final_layout.location.x, body.final_layout.location.y);
    let static_xy = (origin.0 + layout.location.x, origin.1 + layout.location.y);
    if is_positioned_body(body) {
        return static_xy;
    }
    let Some((x, y)) = resolve_viewport_cb_location(
        child,
        layout.size.width,
        layout.size.height,
        viewport_w_px,
        viewport_h_px,
    ) else {
        return static_xy;
    };
    let (explicit_x, explicit_y) = explicit_inset_axes(child);
    (
        if explicit_x { x } else { static_xy.0 },
        if explicit_y { y } else { static_xy.1 },
    )
}

/// Resolve a viewport-CB-anchored absolute/fixed element's CSS px
/// (x, y) using its computed `top` / `left` / `right` / `bottom`
/// insets. Mirrors CSS 2.1 §10.3.7 / §10.6.4 over-constrained
/// resolution: start-side (top / left) wins when both sides are set,
/// end-side (bottom / right) only fires when the start-side is `auto`.
///
/// Returns `None` when no inset is set on either axis (caller should
/// keep Taffy's `final_layout.location` as the static-position
/// fallback).
fn resolve_viewport_cb_location(
    node: &blitz_dom::Node,
    el_w_px: f32,
    el_h_px: f32,
    cb_w_px: f32,
    cb_h_px: f32,
) -> Option<(f32, f32)> {
    use ::style::values::computed::Length;
    use ::style::values::generics::position::GenericInset;

    fn resolve(inset: &::style::values::computed::position::Inset, basis_px: f32) -> Option<f32> {
        match inset {
            GenericInset::LengthPercentage(lp) => Some(lp.resolve(Length::new(basis_px)).px()),
            _ => None,
        }
    }

    let styles = node.primary_styles()?;
    let pos = styles.get_position();
    let left = resolve(&pos.left, cb_w_px);
    let top = resolve(&pos.top, cb_h_px);
    let right = resolve(&pos.right, cb_w_px);
    let bottom = resolve(&pos.bottom, cb_h_px);
    if left.is_none() && top.is_none() && right.is_none() && bottom.is_none() {
        return None;
    }
    // Per-axis resolution: when both insets on an axis are `auto`, fall
    // back to Taffy's static position (`final_layout.location`). Caller
    // unwraps the returned tuple against that fallback, so we surface
    // only the axes that have an explicit inset.
    // End-side (right / bottom) anchoring positions the element's *margin
    // box* edge against the CB edge, so the border-box origin must back off
    // by the used end-side margin (CSS 2.1 §10.3.7 / §10.6.4). Taffy resolves
    // these into `final_layout.margin`; without subtracting them an abs with
    // `bottom:0; margin-bottom:2em` collapses onto `bottom:0`. This is a
    // distinct end-side-margin bug from nested-abs pagination — see the
    // `abs_bottom_margin_offsets_above_sibling` regression test. NOTE: this
    // helper resolves `position: fixed` elements too (all three callers), so
    // the margin term corrects fixed end-anchoring as well as abs. The margin
    // is subtracted unrounded between the two rounded terms; fixedpos-004's
    // pixel-exact reftest is the validator for this rounding choice.
    let margin = node.final_layout.margin;
    let x = if let Some(l) = left {
        l
    } else if let Some(r) = right {
        // To mirror Taffy's internal end-side layout, collapse viewport
        // edge before subtracting element width.
        (cb_w_px - r).round() - margin.right - el_w_px.round()
    } else {
        node.final_layout.location.x
    };
    let y = if let Some(t) = top {
        t
    } else if let Some(b) = bottom {
        // Same order as the in-tree flow for `bottom: 0` style anchors:
        // first round viewport location, then subtract rounded element
        // height. This keeps fixed/abs reference paths aligned to one
        // px boundary when cb height / element size are sub-pixel.
        (cb_h_px - b).round() - margin.bottom - el_h_px.round()
    } else {
        node.final_layout.location.y
    };
    Some((x, y))
}

fn uses_bottom_without_top(node: &blitz_dom::Node) -> bool {
    use ::style::values::generics::position::GenericInset;

    fn is_length_percentage(inset: &::style::values::computed::position::Inset) -> bool {
        matches!(inset, GenericInset::LengthPercentage(_))
    }

    let Some(styles) = node.primary_styles() else {
        return false;
    };
    let pos = styles.get_position();
    !is_length_percentage(&pos.top) && is_length_percentage(&pos.bottom)
}

/// Recursive walker that collects every node id whose computed
/// `position` is `fixed`. Mirrors the helper of the same shape in
/// `blitz_adapter::relayout_position_fixed`. Visits raw `node.children`
/// rather than `layout_children` because the latter may be invalidated
/// by the time this runs, and pseudo-elements (`::before` / `::after`)
/// live in `node.before` / `node.after` outside the children vec.
///
/// Used by [`append_position_fixed_fragments`].
fn walk_for_position_fixed(doc: &BaseDocument, node_id: usize, out: &mut Vec<usize>, depth: usize) {
    use ::style::properties::longhands::position::computed_value::T as Pos;

    if depth >= crate::MAX_DOM_DEPTH {
        return;
    }
    let Some(node) = doc.get_node(node_id) else {
        return;
    };
    let is_fixed = node
        .primary_styles()
        .is_some_and(|s| matches!(s.get_box().clone_position(), Pos::Fixed));
    if is_fixed {
        out.push(node_id);
    }
    for &child_id in &node.children {
        walk_for_position_fixed(doc, child_id, out, depth + 1);
    }
    // Pseudo-elements: a `::before { position: fixed }` would
    // otherwise be missed by the children-only walk. The `before` /
    // `after` slots live directly on `Node`, not on `ElementData`.
    if let Some(pseudo_id) = node.before {
        walk_for_position_fixed(doc, pseudo_id, out, depth + 1);
    }
    if let Some(pseudo_id) = node.after {
        walk_for_position_fixed(doc, pseudo_id, out, depth + 1);
    }
}

/// fulgur-jkl5: total page count implied by a geometry table.
///
/// Returns `max(page_index) + 1` if the table has any fragments, else
/// `1` (at least one page is always implied).
pub fn implied_page_count(geometry: &PaginationGeometryTable) -> u32 {
    geometry
        .values()
        .flat_map(|g| g.fragments.iter())
        .map(|f| f.page_index)
        .max()
        .map(|m| m + 1)
        .unwrap_or(1)
}

/// Locate the `<body>` element id by walking the html root's children.
///
/// Prefers the first child whose tag name is `body`. Falls back to
/// `None` when the document has no `<body>` (e.g. fragments parsed
/// outside a normal document context). Spec-pure HTML5 parsing always
/// synthesizes a `<body>`, but tests and library callers can pass
/// arbitrary fragments so we do not rely on its presence.
fn find_body_id(doc: &BaseDocument) -> Option<usize> {
    let root_id = doc.root_element().id;
    let root = doc.get_node(root_id)?;
    for child_id in &root.children {
        let Some(child) = doc.get_node(*child_id) else {
            continue;
        };
        if let Some(elem) = child.element_data()
            && elem.name.local.as_ref() == "body"
        {
            return Some(*child_id);
        }
    }
    None
}

// ── Trait delegation to BaseDocument (mirrors multicol_layout) ────────────
//
// These trait impls are not exercised by the current measurement-only
// `fragment_pagination_root` walk — they are scaffolding for the next
// iteration that will call `taffy::compute_root_layout(self, body_id, ...)`
// to drive the fragmenter through Taffy's normal dispatch. Keeping the
// shapes here so the upgrade is a localized change.

impl TraversePartialTree for PaginationLayoutTree<'_> {
    type ChildIter<'a>
        = <BaseDocument as TraversePartialTree>::ChildIter<'a>
    where
        Self: 'a;

    fn child_ids(&self, node_id: NodeId) -> Self::ChildIter<'_> {
        self.doc.child_ids(node_id)
    }

    fn child_count(&self, node_id: NodeId) -> usize {
        self.doc.child_count(node_id)
    }

    fn get_child_id(&self, node_id: NodeId, index: usize) -> NodeId {
        self.doc.get_child_id(node_id, index)
    }
}

impl TraverseTree for PaginationLayoutTree<'_> {}

impl CacheTree for PaginationLayoutTree<'_> {
    fn cache_get(
        &self,
        node_id: NodeId,
        known_dimensions: Size<Option<f32>>,
        available_space: Size<AvailableSpace>,
        run_mode: taffy::RunMode,
    ) -> Option<taffy::LayoutOutput> {
        self.doc
            .cache_get(node_id, known_dimensions, available_space, run_mode)
    }

    fn cache_store(
        &mut self,
        node_id: NodeId,
        known_dimensions: Size<Option<f32>>,
        available_space: Size<AvailableSpace>,
        run_mode: taffy::RunMode,
        layout_output: taffy::LayoutOutput,
    ) {
        self.doc.cache_store(
            node_id,
            known_dimensions,
            available_space,
            run_mode,
            layout_output,
        );
    }

    fn cache_clear(&mut self, node_id: NodeId) {
        self.doc.cache_clear(node_id);
    }
}

impl LayoutPartialTree for PaginationLayoutTree<'_> {
    type CoreContainerStyle<'a>
        = &'a taffy::Style<style::Atom>
    where
        Self: 'a;

    type CustomIdent = style::Atom;

    fn get_core_container_style(&self, node_id: NodeId) -> Self::CoreContainerStyle<'_> {
        self.doc.get_core_container_style(node_id)
    }

    fn set_unrounded_layout(&mut self, node_id: NodeId, layout: &taffy::Layout) {
        self.doc.set_unrounded_layout(node_id, layout);
    }

    fn resolve_calc_value(&self, calc_ptr: *const (), parent_size: f32) -> f32 {
        self.doc.resolve_calc_value(calc_ptr, parent_size)
    }

    fn compute_child_layout(
        &mut self,
        node_id: NodeId,
        inputs: taffy::tree::LayoutInput,
    ) -> taffy::LayoutOutput {
        if Some(usize::from(node_id)) == self.body_id {
            return compute_pagination_layout(self, node_id, inputs);
        }
        // Everything else delegates to BaseDocument's normal dispatch.
        self.doc.compute_child_layout(node_id, inputs)
    }
}

impl RoundTree for PaginationLayoutTree<'_> {
    fn get_unrounded_layout(&self, node_id: NodeId) -> taffy::Layout {
        self.doc.get_unrounded_layout(node_id)
    }

    fn set_final_layout(&mut self, node_id: NodeId, layout: &taffy::Layout) {
        self.doc.set_final_layout(node_id, layout);
    }
}

/// Custom layout dispatch for the body (the fragmenter's fragmentation root).
///
/// Mirrors the structure of [`crate::multicol_layout::compute_multicol_layout`]:
/// the wrapper's `compute_child_layout` fires for body, delegates the
/// real layout to `BaseDocument` (so children's `final_layout` is
/// populated correctly), then post-walks body's direct children and
/// records fragments in the geometry side-table.
///
/// In the next iteration this is where per-strip available_space
/// constraint and child-by-child re-layout will live. For the current
/// fragmenter it's a thin shim that proves the dispatch path works.
fn compute_pagination_layout(
    tree: &mut PaginationLayoutTree<'_>,
    body_id: NodeId,
    inputs: taffy::tree::LayoutInput,
) -> taffy::LayoutOutput {
    // Delegate the actual layout work to BaseDocument so children get
    // their normal natural sizes. The output is body's full natural
    // height — that height is what `convert::dom_to_drawables` already
    // expects to read from `final_layout`.
    let output = tree.doc.compute_child_layout(body_id, inputs);

    // Now post-walk to populate the geometry side-table. We can't reuse
    // `fragment_pagination_root` directly because it returns a fragment
    // count; the dispatch path doesn't need that, so we inline the same
    // walk and discard the count.
    let _emitted = tree.fragment_pagination_root();

    output
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::blitz_adapter;
    use std::ops::{Deref, DerefMut};
    use std::sync::Arc;

    /// Parse helper for the fragmenter's tests.
    ///
    /// We deliberately don't accept a viewport height: `blitz_adapter::parse`
    /// uses a hardcoded viewport_h internally, and the fragmenter's strip slicing
    /// is driven by the `page_height_px` argument to `run_pass` rather than
    /// by the viewport. The fixtures pass viewport_w only.
    fn parse(html: &str, viewport_w: f32) -> blitz_html::HtmlDocument {
        let fonts: Vec<Arc<Vec<u8>>> = Vec::new();
        let mut doc = blitz_adapter::parse(html, viewport_w, &fonts);
        blitz_adapter::resolve(&mut doc);
        doc
    }

    /// F29: page 0 is shorter by the body's offset, every other page is full.
    #[test]
    fn page_strips_shorten_only_page_zero() {
        let strips = PageStrips::new(1000.0, 40.0);
        assert_eq!(strips.bottom(0), 960.0);
        assert_eq!(strips.bottom(1), 1000.0);
        assert_eq!(strips.bottom(7), 1000.0);
        assert_eq!(strips.fresh(), 1000.0);
    }

    /// F29: an offset that is absent, negative, not finite, or leaves no room
    /// on page 0 changes nothing (the last one is warned about).
    #[test]
    fn page_strips_ignore_an_unusable_offset() {
        for offset in [0.0, -12.0, f32::NAN, f32::INFINITY, 1000.0, 5000.0] {
            let strips = PageStrips::new(1000.0, offset);
            assert_eq!(strips.bottom(0), 1000.0, "offset {offset}");
        }
        assert_eq!(PageStrips::new(1000.0, 999.0).bottom(0), 1.0);
    }

    /// F29: the offset is read from the laid-out body: a margin collapsed
    /// through `<body>` from its first child counts like the body's own.
    #[test]
    fn the_tree_takes_the_body_offset_from_the_layout() {
        for (html, offset) in [
            (
                r#"<html><body style="margin:0"><p style="margin:0">x</p></body></html>"#,
                0.0,
            ),
            (
                r#"<html><body style="margin:40px"><p style="margin:0">x</p></body></html>"#,
                40.0,
            ),
            (
                r#"<html><body style="margin:0"><p style="margin:60px 0 0">x</p></body></html>"#,
                60.0,
            ),
            (
                r#"<html style="padding-top:25px"><body style="margin:0"><p style="margin:0">x</p></body></html>"#,
                25.0,
            ),
            (
                r#"<html><body style="margin:0;padding:30px"><p style="margin:0">x</p></body></html>"#,
                0.0,
            ),
        ] {
            let mut doc = parse(html, 600.0);
            let tree = PaginationLayoutTree::new(&mut doc, 800.0);
            assert!(
                (tree.strips.bottom(0) - (800.0 - offset)).abs() < 0.01,
                "{html}: page 0 ends at {}",
                tree.strips.bottom(0)
            );
            assert_eq!(tree.strips.bottom(1), 800.0);
        }
    }

    #[test]
    fn empty_document_emits_only_body_fragment() {
        let mut doc = parse("<html><body></body></html>", 600.0);
        let table = run_pass(&mut doc, 800.0);
        // Phase 2.3 fix: body itself is now recorded so its own
        // counter / string-set / bookmark ops are visible to the
        // parity walks. Empty body → just the body fragment.
        assert_eq!(table.len(), 1, "expected only body fragment, got {table:?}");
    }

    #[test]
    fn html_only_input_still_paginates_synthesized_body() {
        // html5ever synthesizes `<body>` for any HTML input, so
        // `find_body_id` always succeeds in the parse pipeline. The
        // synthesized body has no children — the geometry table
        // still contains the body fragment itself (Phase 2.3 fix)
        // but no child entries.
        let mut doc = parse("<html></html>", 600.0);
        let tree = PaginationLayoutTree::new(&mut doc, 800.0);
        assert!(tree.body_id.is_some(), "html5ever should synthesize a body");
        let table = run_pass(&mut doc, 800.0);
        assert_eq!(table.len(), 1, "expected only body fragment, got {table:?}");
    }

    /// fulgur-s67g Phase 2.5: nested descendants must be recorded
    /// in the geometry table on the same page as their ancestor, so
    /// bookmark / counter / string-set markers attached to deeply
    /// nested DOM elements participate in parity assertions.
    #[test]
    fn nested_descendants_inherit_parent_page() {
        let html = r#"
            <html><body>
              <div style="height: 600px">
                <h2 style="height: 30px">Section 1</h2>
              </div>
              <div style="height: 600px">
                <h2 style="height: 30px">Section 2</h2>
              </div>
            </body></html>
        "#;
        let mut doc = parse(html, 600.0);
        let table = run_pass(&mut doc, 800.0);

        // Two outer divs split across two pages (600 + 600 > 800),
        // each carrying a nested h2. Geometry should contain both
        // outer divs AND both inner h2s — four entries total — with
        // the h2 sharing its parent's page_index.
        assert!(
            table.len() >= 4,
            "expected at least 4 entries (2 divs + 2 h2s), got {}",
            table.len(),
        );
        let h2_pages: Vec<u32> = table
            .values()
            .filter(|g| {
                g.fragments
                    .iter()
                    .any(|f| (f.height.to_f32() - 30.0).abs() < 0.5)
            })
            .map(|g| g.fragments[0].page_index)
            .collect();
        assert_eq!(h2_pages.len(), 2, "expected 2 h2 entries, got {h2_pages:?}");
        // Pages of the h2s should match those of their containing divs:
        // first div on page 0 → first h2 on page 0; second div on page
        // 1 → second h2 on page 1.
        assert_eq!(h2_pages, vec![0, 1]);
    }

    /// fulgur-2map.5: directly exercise `fragment_block_subtree`'s
    /// `depth >= MAX_DOM_DEPTH` guard (pagination_layout.rs ~1539-1557).
    ///
    /// The guard is the FIRST statement of the function, so calling it
    /// with `depth = crate::MAX_DOM_DEPTH` trips the bail immediately —
    /// no deep HTML and no big-stack thread required (the prior
    /// render_smoke coverage needed a 600-deep `<div>` chain plus a
    /// 256 MB stack just to recurse far enough to reach this arm). This
    /// also tracks the constant itself: if `MAX_DOM_DEPTH` ever changes,
    /// the test still enters the guard at exactly the limit.
    #[test]
    fn fragment_block_subtree_at_depth_limit_bails_with_whole_fragment() {
        let doc = parse(
            r#"<html><body style="margin:0"><div id="d" style="height:50px"></div></body></html>"#,
            600.0,
        );
        let parent_id = find_by_id(&doc, "d").expect("div#d should exist");
        // The bail copies this node's laid-out height into the fragment.
        let node_h = doc
            .get_node(parent_id)
            .expect("div node")
            .final_layout
            .size
            .height;
        assert!(
            (node_h - 50.0).abs() < 1.0,
            "div should lay out ~50px tall, got {node_h}"
        );

        let mut geom = PaginationGeometryTable::new();
        let (page_out, cursor_out, _, _) = fragment_block_subtree(
            &mut geom,
            &doc, // &BaseDocument via deref coercion
            None, // column_styles
            None, // used_page_names
            None, // facts
            None, // multicol
            parent_id,
            600.0, // parent_w
            0.0,   // parent_x_in_body
            0,     // page_in
            0.0,   // cursor_in
            PageStrips::new(800.0, 0.0),
            crate::MAX_DOM_DEPTH, // depth → trips the guard immediately
            0.0,                  // tail_below
            0.0,                  // top_floor
            false,                // content_above
        );

        // The bail pushes exactly ONE whole fragment for parent_id at its
        // entry coordinates, then returns (page_in, cursor_in + height).
        let entry = geom
            .get(&parent_id)
            .expect("bail must emit a geometry entry for parent_id");
        // Hot-path-bind `.len()` so the assert message doesn't introduce a
        // failure-only region that codecov marks uncovered (see the P1c
        // assert-arg artifact note in CLAUDE.md / units migration memory).
        let n_frags = entry.fragments.len();
        assert_eq!(
            n_frags, 1,
            "bail emits a single whole fragment, got {n_frags}"
        );
        let frag = &entry.fragments[0];
        assert_eq!(frag.page_index, 0, "fragment stays on the entry page");
        let fx = frag.x.to_f32();
        assert_eq!(fx, 0.0, "fragment x == parent_x_in_body, got {fx}");
        let fy = frag.y.to_f32();
        assert_eq!(fy, 0.0, "fragment y == cursor_in, got {fy}");
        let fw = frag.width.to_f32();
        assert_eq!(fw, 600.0, "fragment width == parent_w, got {fw}");
        let fh = frag.height.to_f32();
        assert!(
            (fh - node_h).abs() < 1.0,
            "fragment height == node layout height: {fh} vs {node_h}"
        );

        assert_eq!(page_out, 0, "bail returns the entry page unchanged");
        assert!(
            (cursor_out - node_h).abs() < 1.0,
            "cursor advances by the node height: {cursor_out} vs {node_h}"
        );
    }

    #[test]
    fn three_short_blocks_fit_one_page() {
        // Each block is 200px tall; page is 800px → all three fit on
        // page 0.
        let html = r#"
            <html><body>
              <div style="height: 200px"></div>
              <div style="height: 200px"></div>
              <div style="height: 200px"></div>
            </body></html>
        "#;
        let mut doc = parse(html, 600.0);
        let table = run_pass(&mut doc, 800.0);
        // Phase 2.3 fix: body itself is recorded too, so total = 4
        // (body + 3 child divs). All on page 0.
        assert_eq!(table.len(), 4, "expected 4 entries, got {}", table.len());
        for (id, geom) in &table {
            assert_eq!(
                geom.fragments.len(),
                1,
                "node {id} should have a single fragment"
            );
            assert_eq!(geom.fragments[0].page_index, 0);
        }
    }

    #[test]
    fn oversize_block_run_breaks_to_next_page() {
        // Block 1 is 600px, block 2 is 400px. Page strip is 800px.
        // Block 1 fits on page 0 (cursor 0 → 600). Block 2 starts at
        // y=600, would end at y=1000 > 800 → break to page 1.
        let html = r#"
            <html><body>
              <div style="height: 600px"></div>
              <div style="height: 400px"></div>
            </body></html>
        "#;
        let mut doc = parse(html, 600.0);
        let table = run_pass(&mut doc, 800.0);
        // Phase 2.3 fix: body + 2 children = 3 entries.
        // Body fragment is page 0; children are 0, 1.
        assert_eq!(table.len(), 3);
        let pages: Vec<u32> = table.values().map(|g| g.fragments[0].page_index).collect();
        assert_eq!(
            pages,
            vec![0, 0, 1],
            "body page 0, first child page 0, second child page 1, got {pages:?}"
        );
    }

    /// Find the first element node carrying `id="<id>"`.
    fn find_by_id(doc: &blitz_dom::BaseDocument, id: &str) -> Option<usize> {
        fn walk(doc: &blitz_dom::BaseDocument, node_id: usize, target: &str) -> Option<usize> {
            let node = doc.get_node(node_id)?;
            if let Some(ed) = node.element_data()
                && let Some(attr_id) = ed.attrs().iter().find(|a| a.name.local.as_ref() == "id")
                && attr_id.value.as_str() == target
            {
                return Some(node_id);
            }
            for &child in &node.children {
                if let Some(found) = walk(doc, child, target) {
                    return Some(found);
                }
            }
            None
        }
        walk(doc, doc.root_element().id, id)
    }

    /// fulgur-ezst: a tiny input with a pathologically tall CSS height on a
    /// CHILDLESS block prints only blank pages, so instead of slicing it to
    /// the `MAX_PAGES` cap (~10k blank pages) the fragmenter collapses it to
    /// a single page. `height: 99999999px` would otherwise slice into
    /// ~125 000 fragments — the small-input DoS.
    #[test]
    fn pathological_childless_tall_block_collapses() {
        let html = r#"<html><body><div style="height: 99999999px"></div></body></html>"#;
        let mut doc = parse(html, 600.0);
        let table = run_pass(&mut doc, 800.0);
        assert_eq!(
            implied_page_count(&table),
            1,
            "childless pathological height must collapse to a single page",
        );
    }

    /// fulgur-ezst: Stylo/Taffy clamps an absurd `<length>` to `f32::MAX`
    /// (≈3.4e38), the worst-case finite input. A childless block that tall
    /// also collapses to one page (the ceiling-bounded slice loop still runs
    /// its counter math but pushes no fragment).
    #[test]
    fn f32_max_childless_height_collapses() {
        let html = r#"<html><body><div style="height: 1e39px"></div></body></html>"#;
        let mut doc = parse(html, 600.0);
        let table = run_pass(&mut doc, 800.0);
        assert_eq!(
            implied_page_count(&table),
            1,
            "childless f32::MAX height must collapse to a single page",
        );
    }

    /// fulgur-ezst: background presence does not gate the collapse — a
    /// childless filled band this tall is still a pathological amplifier.
    #[test]
    fn childless_tall_block_with_background_collapses() {
        let html =
            r#"<html><body><div style="height: 99999999px; background: red"></div></body></html>"#;
        let mut doc = parse(html, 600.0);
        let table = run_pass(&mut doc, 800.0);
        assert_eq!(implied_page_count(&table), 1);
    }

    /// fulgur-ezst: a tall block WITH rendered content is not childless, so
    /// it is NOT collapsed — it still clamps to ~MAX_PAGES (truncate-and-
    /// warn). Also guards routing: a huge parent with a small child must
    /// reach the slice loop, not the recursion branch (the recursion gate
    /// measures descendant overflow, not the parent's intrinsic height).
    #[test]
    fn content_bearing_tall_block_is_page_capped() {
        let html = r#"<html><body><div style="height: 99999999px"><p>x</p></div></body></html>"#;
        let mut doc = parse(html, 600.0);
        let table = run_pass(&mut doc, 800.0);
        let pages = implied_page_count(&table);
        assert!(
            pages > 1_000,
            "content-bearing block must take the cap path, not collapse; got {pages}",
        );
        assert!(
            pages <= crate::MAX_PAGES + 1,
            "content-bearing block must stay clamped to ~MAX_PAGES ({}); got {pages}",
            crate::MAX_PAGES,
        );
    }

    /// fulgur-ezst: a childless band that fits WITHIN the cap renders its
    /// full page count — the collapse must not over-fire on ordinary
    /// multi-page spacers. `height: 4000px` on an 800px strip = 5 pages (the
    /// body margin is cleared: with the UA's 8px page 0 is 792px, F29, and the
    /// band needs a sixth page for its last 8px).
    #[test]
    fn childless_subcap_band_renders_full() {
        let html =
            r#"<html><body style="margin:0"><div style="height: 4000px"></div></body></html>"#;
        let mut doc = parse(html, 600.0);
        let table = run_pass(&mut doc, 800.0);
        assert_eq!(
            implied_page_count(&table),
            5,
            "a sub-cap childless band must render fully, not collapse",
        );
    }

    /// fulgur-ezst (Codex P2 on PR #553): an empty *block* child lays out
    /// with positive width but zero height, so a `height <= 0 && width <= 0`
    /// emptiness test would wrongly count it as content and disable the
    /// collapse — a trivial DoS bypass. `subtree_has_rendered_content` treats
    /// any zero-AREA descendant as blank (recursing for overflow), so a tall
    /// block whose only child is an empty block still collapses to one page.
    #[test]
    fn childless_tall_block_with_empty_block_child_collapses() {
        let html = r#"<html><body><div style="height: 99999999px"><div></div></div></body></html>"#;
        let mut doc = parse(html, 600.0);
        let table = run_pass(&mut doc, 800.0);
        assert_eq!(
            implied_page_count(&table),
            1,
            "an empty (zero-area) block child must not defeat the collapse",
        );
    }

    /// fulgur-ezst: the emptiness recursion must still find VISIBLE content
    /// nested under a zero-area wrapper — a zero-height (overflow:visible)
    /// wrapper holding a real `<p>` is content, so the tall block is NOT
    /// childless and stays on the cap path. Guards the `||` fix against
    /// over-collapsing (dropping visibly-overflowing descendants).
    #[test]
    fn tall_block_with_content_under_zero_height_wrapper_is_not_collapsed() {
        let html = r#"<html><body><div style="height: 99999999px"><div style="height: 0"><p>x</p></div></div></body></html>"#;
        let mut doc = parse(html, 600.0);
        let table = run_pass(&mut doc, 800.0);
        let pages = implied_page_count(&table);
        assert!(
            pages > 1_000,
            "visible content under a zero-height wrapper must block the collapse; got {pages}",
        );
        assert!(
            pages <= crate::MAX_PAGES + 1,
            "still clamped to ~MAX_PAGES ({}); got {pages}",
            crate::MAX_PAGES,
        );
    }

    /// fulgur-ezst: the defensive guards of `subtree_has_rendered_content`
    /// read as "no content" (false) without panicking — a nonexistent node
    /// id (the `get_node` None guard) and a call already at the recursion
    /// ceiling (the `MAX_DOM_DEPTH` guard).
    #[test]
    fn subtree_has_rendered_content_guards() {
        use std::ops::DerefMut;
        let html = r#"<html><body><div id="x">hi</div></body></html>"#;
        let mut doc = parse(html, 600.0);
        let x = find_by_id(doc.deref_mut(), "x").expect("div#x");
        assert!(!subtree_has_rendered_content(
            doc.deref_mut(),
            usize::MAX,
            0
        ));
        assert!(!subtree_has_rendered_content(
            doc.deref_mut(),
            x,
            crate::MAX_DOM_DEPTH
        ));
    }

    /// fulgur-c8re (security): a pathologically tall CHILDLESS replaced
    /// element (`<img>` / `<svg>`) that paints nothing must collapse like any
    /// blank spacer. "Paints nothing" covers the common offline-first case of
    /// an unresolved `src` (no matching `AssetBundle` entry), a
    /// `visibility:hidden` image, an undecodable format, and an empty `<svg>`.
    ///
    /// The predecessor `is_replaced_content` exception (Codex P2 on PR #553 /
    /// fulgur-ezst) gated the collapse on tag name alone, so such a
    /// non-painting node disabled the collapse and amplified a few bytes of
    /// HTML into ~`MAX_PAGES` blank pages (a validated high-severity DoS). It
    /// was removed: a replaced element only reaches this branch when it is
    /// taller than `MAX_PAGES` pages (~10M px), a range no legitimate single
    /// image occupies, so clipping it to one page loses no real content.
    #[test]
    fn replaced_tall_childless_block_collapses() {
        for tag in [
            // Unresolved `src`: no `AssetBundle` on this test path, so the
            // image paints nothing — yet it is still a pathological amplifier.
            r#"<img src="missing.png" style="display:block;width:10px;height:99999999px">"#,
            // `visibility:hidden`: occupies layout, paints nothing.
            r#"<img src="x.png" style="display:block;visibility:hidden;width:10px;height:99999999px">"#,
            // Empty `<svg>`: no drawable content.
            r#"<svg style="display:block;width:10px;height:99999999px"></svg>"#,
        ] {
            let html = format!(r#"<html><body>{tag}</body></html>"#);
            let mut doc = parse(&html, 600.0);
            let table = run_pass(&mut doc, 800.0);
            assert_eq!(
                implied_page_count(&table),
                1,
                "a non-painting tall replaced element must collapse, not \
                 amplify into blank pages: {tag}",
            );
        }
    }

    /// fulgur-ezst (Codex P2 on PR #553): a `visibility:hidden` descendant
    /// occupies layout space but paints nothing, so it must not defeat the
    /// collapse — a tall block whose only child is invisible still collapses.
    #[test]
    fn invisible_only_child_collapses() {
        let html = r#"<html><body><div style="height:99999999px"><div style="visibility:hidden;height:1px"></div></div></body></html>"#;
        let mut doc = parse(html, 600.0);
        let table = run_pass(&mut doc, 800.0);
        assert_eq!(
            implied_page_count(&table),
            1,
            "a visibility:hidden-only descendant must not defeat the collapse",
        );
    }

    /// fulgur-ezst: the visibility skip must still recurse — a
    /// `visibility:visible` descendant under a `visibility:hidden` wrapper is
    /// real content, so the tall block is not childless and stays capped.
    #[test]
    fn visible_child_under_hidden_wrapper_blocks_collapse() {
        let html = r#"<html><body><div style="height:99999999px"><div style="visibility:hidden"><p style="visibility:visible">x</p></div></div></body></html>"#;
        let mut doc = parse(html, 600.0);
        let table = run_pass(&mut doc, 800.0);
        let pages = implied_page_count(&table);
        assert!(
            pages > 1_000,
            "visible content under a hidden wrapper must block the collapse; got {pages}",
        );
    }

    /// fulgur-c8re (security, Codex P1 on PR #575): the childless collapse must
    /// bound the SPACE the pathological element occupies, not just skip its
    /// per-page fragment pushes. The predecessor behaviour left the slice loop
    /// advancing `page_index` all the way to `MAX_PAGES`, so a following in-flow
    /// sibling was stranded on a deep page and `implied_page_count` — which
    /// reads the max fragment index across ALL nodes — re-inflated the document
    /// to ~`MAX_PAGES` blank pages (`<div huge></div><p>after</p>`), defeating
    /// the collapse. The element now consumes only its single first slice and
    /// following content reflows onto the next page. Asserts: the tall div
    /// contributes exactly one fragment (collapse fired) AND the `<p>` sibling
    /// lands on a bounded page, so the whole document stays a handful of pages.
    #[test]
    fn childless_collapse_reflows_following_sibling() {
        use std::ops::DerefMut;
        let html = r#"<html><body><div id="tall" style="height: 99999999px"></div><p id="after">after</p></body></html>"#;
        let mut doc = parse(html, 600.0);
        let table = run_pass(&mut doc, 800.0);
        let tall_id = find_by_id(doc.deref_mut(), "tall").expect("div#tall");
        let after_id = find_by_id(doc.deref_mut(), "after").expect("p#after");
        let tall_frags = table
            .get(&tall_id)
            .expect("div#tall must appear in geometry")
            .fragments
            .len();
        assert_eq!(
            tall_frags, 1,
            "collapsed childless div must contribute exactly one fragment, got {tall_frags}",
        );
        let after_page = table
            .get(&after_id)
            .expect("p#after must appear in geometry")
            .fragments[0]
            .page_index;
        assert!(
            after_page <= 1,
            "collapsed spacer must reflow its trailing sibling onto a bounded page \
             (<= 1), not strand it ~MAX_PAGES deep; got {after_page}",
        );
        assert!(
            implied_page_count(&table) <= 2,
            "a childless spacer followed by content must stay a handful of pages; got {}",
            implied_page_count(&table),
        );
    }

    /// fulgur-c8re (security, Codex P1 on PR #575): the replaced-element
    /// collapse must bound trailing content too. A non-painting tall image
    /// (`<img src="missing.png" height:99999999px>`) followed by a `<p>` must
    /// not amplify into ~`MAX_PAGES` blank pages — the same trailing-sibling
    /// vector as `childless_collapse_reflows_following_sibling`, but exercising
    /// the replaced-element path that the `is_replaced_content` removal opened.
    #[test]
    fn replaced_tall_collapse_bounds_trailing_sibling() {
        let html = r#"<html><body><img src="missing.png" style="display:block;width:10px;height:99999999px"><p>after</p></body></html>"#;
        let mut doc = parse(html, 600.0);
        let table = run_pass(&mut doc, 800.0);
        assert!(
            implied_page_count(&table) <= 2,
            "a non-painting tall replaced element followed by content must \
             collapse without amplifying; got {} pages",
            implied_page_count(&table),
        );
    }

    /// fulgur-2m6w: a non-finite Taffy height (`+inf` / `NaN`) is treated
    /// as zero so it can never reach the slicing loop (where `+inf` would
    /// make `remaining -= last_slice_h` loop forever) nor poison the
    /// `cursor_y` advance. CSS clamps to `f32::MAX` and cannot produce a
    /// non-finite height, so inject one directly into the resolved layout.
    /// The node still enters geometry (via the `child_h <= 0.0` branch) and
    /// the document stays single-page.
    #[test]
    fn non_finite_height_treated_as_zero() {
        use std::ops::DerefMut;
        for bad in [f32::INFINITY, f32::NAN] {
            let html = r#"<html><body><div id="x">hi</div></body></html>"#;
            let mut doc = parse(html, 600.0);
            let id = find_by_id(doc.deref_mut(), "x").expect("div#x");
            doc.deref_mut()
                .get_node_mut(id)
                .expect("div#x")
                .final_layout
                .size
                .height = bad;
            let table = run_pass(&mut doc, 800.0);
            assert!(
                implied_page_count(&table) == 1,
                "non-finite height {bad} must not paginate; got {} pages",
                implied_page_count(&table),
            );
            assert!(
                table.contains_key(&id),
                "non-finite-height node must still be recorded in geometry",
            );
        }
    }

    /// fulgur-i5a: `overflow: hidden` is a pure visual effect that must
    /// not affect pagination. A 50px-tall parent containing a 1200px-tall
    /// child stays single-page even though the child would otherwise
    /// extend past the 800px page strip. The clip is applied at draw
    /// time (`render.rs` push/pop_clip_path), and pagination follows the
    /// parent's layout box, not the child's overflowing intrinsic height.
    #[test]
    fn overflow_hidden_does_not_split_oversize_child() {
        let html = r#"
            <html><body>
              <div style="height: 50px; overflow: hidden">
                <div style="height: 1200px"></div>
              </div>
            </body></html>
        "#;
        let mut doc = parse(html, 600.0);
        let table = run_pass(&mut doc, 800.0);

        // body + outer (50px clip) + inner (1200px overflowing child)
        // = 3 entries; every entry has a single fragment on page 0.
        assert_eq!(table.len(), 3, "expected body + outer + inner = 3 entries");
        for (id, geom) in &table {
            assert_eq!(
                geom.fragments.len(),
                1,
                "node {id} should not split under overflow:hidden parent"
            );
            assert_eq!(geom.fragments[0].page_index, 0);
        }
    }

    /// Phase 4 prerequisite repro: confirm `string-set` carry semantic
    /// across page break.
    #[test]
    fn string_set_carry_across_page_break() {
        use crate::blitz_adapter;
        use crate::gcpm::parser::parse_gcpm;
        use std::ops::DerefMut;
        use std::sync::Arc;

        let html = r#"<!DOCTYPE html>
<html>
<head><style>
@page { size: A4; margin: 60pt; }
h2 { string-set: chapter-title content(text); }
.box { padding: 60pt 0; }
</style></head>
<body>
<h2>Introduction</h2>
<div class="box">f1</div>
<div class="box">f2</div>
<div class="box">f3</div>
<div class="box">f4</div>
<div class="box">f5</div>
<div class="box">f6</div>
<div class="box">f7</div>
<div class="box">f8</div>
<h2 style="page-break-before:always">Background</h2>
</body></html>"#;
        let css = "h2 { string-set: chapter-title content(text); }";
        let gcpm = parse_gcpm(css);
        let fonts: Vec<Arc<Vec<u8>>> = Vec::new();
        let mut doc = blitz_adapter::parse(html, 600.0, &fonts);
        let pass = blitz_adapter::StringSetPass::new(gcpm.string_set_mappings.clone());
        let pass_ctx = blitz_adapter::PassContext { font_data: &fonts };
        blitz_adapter::apply_single_pass(&pass, &mut doc, &pass_ctx);
        let store = pass.into_store();
        blitz_adapter::resolve(&mut doc);
        let column_styles = blitz_adapter::extract_column_style_table(&doc);
        let geometry =
            run_pass_with_break_styles(doc.deref_mut(), 720.0_f32.as_pt().in_px(), &column_styles);

        let mut by_node: std::collections::BTreeMap<usize, Vec<(String, String)>> =
            std::collections::BTreeMap::new();
        for entry in store.entries() {
            by_node
                .entry(entry.node_id)
                .or_default()
                .push((entry.name.clone(), entry.value.clone()));
        }
        let states = collect_string_set_states(&geometry, &by_node);
        assert!(
            states.len() >= 2,
            "must span at least 2 pages, got {}",
            states.len()
        );
        let p0 = states[0]
            .get("chapter-title")
            .expect("page 0 must have chapter-title state");
        assert_eq!(p0.first.as_deref(), Some("Introduction"), "page 0 first");
        let p1 = states[1]
            .get("chapter-title")
            .expect("page 1 must have chapter-title state (carry)");
        assert_eq!(
            p1.start.as_deref(),
            Some("Introduction"),
            "page 1 start (carry from page 0 last)"
        );
    }

    /// Phase 3.4 follow-up (PR #296 Devin): regression for the
    /// fragmenter's running-element handling. `fragment_pagination_root`
    /// must record a zero-height fragment for every
    /// `position: running()` element so the running NodeId appears in
    /// geometry; without this, the downstream collect walk returns
    /// all-empty maps and `content: element(name)` in margin boxes
    /// silently produces nothing. Drive the engine pipeline through
    /// `Engine::render_html` and inspect the geometry table built by
    /// the same fragmenter pass.
    #[test]
    fn running_element_node_lands_in_geometry_with_zero_height() {
        use crate::blitz_adapter;
        use crate::gcpm::parser::parse_gcpm;
        use std::ops::DerefMut;
        use std::sync::Arc;

        let css = ".header { position: running(pageHeader); }";
        let html = r#"<!DOCTYPE html>
<html><head><style>.header { position: running(pageHeader); }</style></head>
<body>
<div class="header">Doc Header</div>
<p>Body.</p>
</body></html>"#;

        let gcpm = parse_gcpm(css);
        let fonts: Vec<Arc<Vec<u8>>> = Vec::new();
        let mut doc = blitz_adapter::parse(html, 600.0, &fonts);
        let pass = blitz_adapter::RunningElementPass::new(gcpm.running_mappings.clone());
        let pass_ctx = blitz_adapter::PassContext { font_data: &fonts };
        blitz_adapter::apply_single_pass(&pass, &mut doc, &pass_ctx);
        let store = pass.into_running_store();
        blitz_adapter::resolve(&mut doc);
        let column_styles = blitz_adapter::extract_column_style_table(&doc);

        let geometry = run_pass_with_break_and_running(
            doc.deref_mut(),
            800.0_f32.as_pt().in_px().to_f32(),
            &column_styles,
            &store,
        );

        // The running element's NodeId must exist in geometry on page 0
        // with a zero-height fragment.
        let mut found_running_node = None;
        for (&node_id, geom) in &geometry {
            if store.instance_for_node(node_id).is_some() {
                found_running_node = Some((node_id, geom.fragments.clone()));
                break;
            }
        }
        let (node_id, fragments) =
            found_running_node.expect("running element NodeId must appear in geometry table");
        assert_eq!(fragments.len(), 1, "single zero-height fragment");
        assert_eq!(fragments[0].page_index, 0);
        assert_eq!(
            fragments[0].height.to_f32(),
            0.0,
            "running fragment must not advance the cursor"
        );

        // collect_running_element_states must surface the instance.
        let states = collect_running_element_states(&geometry, &store);
        let entry = states[0]
            .get("pageHeader")
            .expect("pageHeader entry must appear in page 0 state");
        assert_eq!(
            entry.instance_ids,
            vec![store.instance_for_node(node_id).unwrap()]
        );
    }

    /// Parses `html`, runs the running-element pass for `.run { position: running(x) }` (the rule is read from
    /// the `<style>` of the document, which is the inline path: Stylo drops `position: running()`), resolves
    /// the styles and returns the geometry the fragmenter made together with the document and the store.
    fn running_geometry(
        html: &str,
    ) -> (
        blitz_html::HtmlDocument,
        crate::gcpm::running::RunningElementStore,
        PaginationGeometryTable,
    ) {
        use crate::blitz_adapter;
        use std::ops::DerefMut;
        use std::sync::Arc;

        let gcpm = crate::gcpm::parser::parse_gcpm(".run { position: running(x); }");
        let fonts: Vec<Arc<Vec<u8>>> = Vec::new();
        let mut doc = blitz_adapter::parse(html, 600.0, &fonts);
        let pass = blitz_adapter::RunningElementPass::new(gcpm.running_mappings.clone());
        let pass_ctx = blitz_adapter::PassContext { font_data: &fonts };
        blitz_adapter::apply_single_pass(&pass, &mut doc, &pass_ctx);
        let store = pass.into_running_store();
        blitz_adapter::resolve(&mut doc);
        let column_styles = blitz_adapter::extract_column_style_table(&doc);
        let geometry = run_pass_with_break_and_running(
            doc.deref_mut(),
            800.0_f32.as_pt().in_px().to_f32(),
            &column_styles,
            &store,
        );
        (doc, store, geometry)
    }

    /// F102: a running element nested in a wrapper is hidden (`display: none`, zero size), so the fragmenter
    /// never records it. [`append_running_placeholders`] gives it a zero-height fragment on the page its source
    /// position is on, which is what `collect_running_element_states` needs to place it in the margin boxes.
    #[test]
    fn running_placeholder_for_nested_element() {
        use std::ops::Deref;

        let html = r#"<!DOCTYPE html><html><head><style>.run { position: running(x); }</style></head>
<body><p>First page.</p>
<div style="break-before: page"><div class="run">Nested header</div><p>Second page.</p></div>
</body></html>"#;
        let (doc, store, mut geometry) = running_geometry(html);
        let run_id = doc.query_selector(".run").unwrap().expect(".run node");
        assert!(store.instance_for_node(run_id).is_some());
        assert!(
            !geometry.contains_key(&run_id),
            "a hidden nested node has no geometry before the placeholder pass"
        );

        append_running_placeholders(&mut geometry, doc.deref(), &store);

        let fragments = &geometry
            .get(&run_id)
            .expect("the placeholder pass records the nested running node")
            .fragments;
        assert_eq!(fragments.len(), 1, "one zero-height fragment");
        assert_eq!(fragments[0].page_index, 1, "the page the wrapper starts on");
        assert_eq!(fragments[0].height.to_f32(), 0.0);
        let states = collect_running_element_states(&geometry, &store);
        assert_eq!(
            states[1].get("x").map(|s| s.instance_ids.clone()),
            Some(vec![store.instance_for_node(run_id).unwrap()])
        );
    }

    /// A running element directly under `<body>` keeps the fragment the body loop gave it.
    #[test]
    fn running_placeholder_leaves_body_level_fragment_alone() {
        use std::ops::Deref;

        let html = r#"<!DOCTYPE html><html><head><style>.run { position: running(x); }</style></head>
<body><p>First page.</p><div class="run">Header</div><p>More.</p></body></html>"#;
        let (doc, store, mut geometry) = running_geometry(html);
        let run_id = doc.query_selector(".run").unwrap().expect(".run node");
        let before = geometry
            .get(&run_id)
            .expect("body-level node has geometry")
            .fragments
            .clone();
        assert_eq!(before.len(), 1);

        append_running_placeholders(&mut geometry, doc.deref(), &store);

        let after = &geometry[&run_id].fragments;
        assert_eq!(after.len(), 1);
        assert_eq!(
            (
                after[0].page_index,
                after[0].x,
                after[0].y,
                after[0].width,
                after[0].height
            ),
            (
                before[0].page_index,
                before[0].x,
                before[0].y,
                before[0].width,
                before[0].height
            )
        );
    }

    /// `Line001<br>Line002<br>…` with 20px lines, directly in `<body>` (no block around them).
    fn body_lines_html(n: usize, body_style: &str, wrap: Option<&str>) -> String {
        let text = (1..=n)
            .map(|i| format!("Line{i:03}"))
            .collect::<Vec<_>>()
            .join("<br>");
        let content = match wrap {
            Some(tag) => format!("<{tag}>{text}</{tag}>"),
            None => text,
        };
        format!(
            "<!DOCTYPE html><html><head><style>.run {{ position: running(x); }}</style></head>\
             <body style=\"margin:0;line-height:20px;{body_style}\">{content}</body></html>"
        )
    }

    fn body_node(doc: &blitz_html::HtmlDocument) -> usize {
        doc.query_selector("body").unwrap().expect("body")
    }

    /// F111: `<body>` is the inline root when all its in-flow children are inline-level (CSS 2.1 §9.4.2), so
    /// the lines are body's own and the fragmenter splits body between them like it splits a nested paragraph.
    /// Before, body had one fragment of its full height on page 0 and the lines below the page were lost.
    #[test]
    fn body_inline_root_taller_than_a_page_gets_one_fragment_per_page() {
        let mut doc = parse(&body_lines_html(200, "", None), 600.0);
        let body_id = body_node(&doc);
        assert!(doc.get_node(body_id).unwrap().flags.is_inline_root());
        let table = run_pass(&mut doc, 800.0);
        let body = &table[&body_id];
        // 200 lines of 20px on pages of 800px: 40 per page.
        let pages: Vec<u32> = body.fragments.iter().map(|f| f.page_index).collect();
        assert_eq!(pages, [0, 1, 2, 3, 4], "{:?}", body.fragments);
        assert_eq!(implied_page_count(&table), 5);
        assert!(
            line_slices_tile(&body.line_slices, 200),
            "{:?}",
            body.line_slices
        );
        for (k, f) in body.fragments.iter().enumerate() {
            assert_eq!(
                f.y.to_f32(),
                0.0,
                "fragment {k} starts at the top of its page"
            );
            assert!(
                (f.height.to_f32() - 800.0).abs() < 0.5,
                "fragment {k}: {:?}",
                f.height
            );
        }
        // The same lines in a <div> make as many pages: the body path is the nested one.
        let mut wrapped = parse(&body_lines_html(200, "", Some("div")), 600.0);
        assert_eq!(implied_page_count(&run_pass(&mut wrapped, 800.0)), 5);
    }

    /// F111: a body with padding reserves it on every page, like a padded box split by F30.
    #[test]
    fn body_inline_root_split_reserves_its_padding_on_every_page() {
        let mut doc = parse(&body_lines_html(200, "padding:20px", None), 600.0);
        let body_id = body_node(&doc);
        let table = run_pass(&mut doc, 800.0);
        let body = &table[&body_id];
        // 760px of lines per page: 38 lines; 200 lines need 6 pages.
        assert_eq!(body.fragments.len(), 6, "{:?}", body.fragments);
        assert_eq!(
            body.line_split_insets
                .map(|(t, b)| (t.to_f32(), b.to_f32())),
            Some((20.0, 20.0))
        );
        assert!(line_slices_tile(&body.line_slices, 200));
    }

    /// F111: a body that fits a page keeps the one fragment of its full height, as before.
    #[test]
    fn short_body_inline_root_keeps_its_single_fragment() {
        let mut doc = parse(&body_lines_html(10, "", None), 600.0);
        let body_id = body_node(&doc);
        let height = doc.get_node(body_id).unwrap().final_layout.size.height;
        let table = run_pass(&mut doc, 800.0);
        let body = &table[&body_id];
        assert_eq!(body.fragments.len(), 1);
        assert_eq!(body.fragments[0].page_index, 0);
        assert_eq!(body.fragments[0].height.to_f32(), height);
        assert!(body.line_slices.is_empty());
        assert_eq!(implied_page_count(&table), 1);
        let heights: Vec<f32> = body.box_slices.iter().map(|f| f.height.to_f32()).collect();
        assert_eq!(heights, [height], "F37: one slice of body's own height");
    }

    fn slice_heights(geom: &PaginationGeometry) -> Vec<(u32, f32)> {
        geom.box_slices
            .iter()
            .map(|f| (f.page_index, f.height.to_f32()))
            .collect()
    }

    /// F37: an inline-root body's box fills every page but the last and ends at its last line plus the padding and
    /// border below it.
    #[test]
    fn body_inline_root_box_slices_end_at_the_last_line() {
        let mut doc = parse(&body_lines_html(200, "", None), 600.0);
        let body_id = body_node(&doc);
        let table = run_pass(&mut doc, 800.0);
        // 40 lines per page: five full pages.
        let want: Vec<(u32, f32)> = (0..5).map(|p| (p, 800.0)).collect();
        assert_eq!(slice_heights(&table[&body_id]), want);

        let mut doc = parse(&body_lines_html(200, "padding:20px", None), 600.0);
        let body_id = body_node(&doc);
        let table = run_pass(&mut doc, 800.0);
        // 39 lines per page below the top inset (780); the last page holds 5 lines (100) and both insets.
        let mut want: Vec<(u32, f32)> = (0..5).map(|p| (p, 800.0)).collect();
        want.push((5, 140.0));
        assert_eq!(slice_heights(&table[&body_id]), want);
    }

    /// F37: the walk over body's children records where body's box ends on each page: page 0 runs to `bottom(0)`
    /// (the page strip less body's offset), a middle page to the strip, the last to the last child plus body's
    /// padding and border; a trailing `break-after: page` adds no slice.
    #[test]
    fn body_box_slices_follow_the_children_of_the_walk() {
        let divs = |n: usize, last_style: &str| {
            let mut s = "<div style=\"height:100px\"></div>".repeat(n - 1);
            s.push_str(&format!("<div style=\"height:100px;{last_style}\"></div>"));
            s
        };
        let html = |content: String| {
            format!(
                "<!DOCTYPE html><html><body style=\"margin:40px 0 0;padding-bottom:20px;border-bottom:5px solid\">\
                 {content}</body></html>"
            )
        };
        for (last_style, label) in [("", "plain"), ("break-after:page", "trailing break")] {
            let mut doc = parse(&html(divs(25, last_style)), 600.0);
            let body_id = body_node(&doc);
            let table = crate::blitz_adapter::extract_column_style_table(&doc);
            let geom = run_pass_with_break_styles(doc.deref_mut(), 1000.0_f32.as_px(), &table);
            // Strips: page 0 is 960 (the 40px offset), the others 1000. Nine divs fit page 0, ten page 1, six page 2
            // (600) with the 20px padding and the 5px border below them.
            assert_eq!(
                slice_heights(&geom[&body_id]),
                [(0, 960.0), (1, 1000.0), (2, 625.0)],
                "{label}"
            );
            for f in &geom[&body_id].box_slices {
                assert_eq!(f.y.to_f32(), 0.0);
            }
            // `fragments` is untouched: body keeps its one fragment on page 0.
            assert_eq!(geom[&body_id].fragments.len(), 1);
        }
    }

    /// F37: a body that fits one page has one slice of its own height; one taller than the page area is clamped to it.
    #[test]
    fn body_box_slices_of_a_short_and_of_a_tall_body() {
        let mut doc = parse(
            "<!DOCTYPE html><html><body style=\"margin:40px 0;padding:10px\">\
             <div style=\"height:300px\"></div></body></html>",
            600.0,
        );
        let body_id = body_node(&doc);
        let table = run_pass(&mut doc, 1000.0);
        assert_eq!(slice_heights(&table[&body_id]), [(0, 320.0)]);

        // Content on one page, but a `height` far beyond it (F37c): the box stops at the page area (1000 - 40).
        let mut doc = parse(
            "<!DOCTYPE html><html><body style=\"margin:40px 0;height:5000px\">\
             <div style=\"height:300px\"></div></body></html>",
            600.0,
        );
        let body_id = body_node(&doc);
        let table = run_pass(&mut doc, 1000.0);
        assert_eq!(slice_heights(&table[&body_id]), [(0, 960.0)]);
    }

    fn strips_1000_offset_40() -> PageStrips {
        PageStrips::new(1000.0, 40.0)
    }

    /// F37: pure slice arithmetic of `body_box_slices`.
    #[test]
    fn body_box_slices_arithmetic() {
        let strips = strips_1000_offset_40();
        let heights = |s: Vec<Fragment>| -> Vec<(u32, f32)> {
            s.iter()
                .map(|f| (f.page_index, f.height.to_f32()))
                .collect()
        };
        // one page: the body's height
        assert_eq!(
            heights(body_box_slices(strips, 7.0, 500.0, 340.0, (0, 340.0))),
            [(0, 340.0)]
        );
        // one page: clamped to the page area
        assert_eq!(
            heights(body_box_slices(strips, 7.0, 500.0, 4000.0, (0, 340.0))),
            [(0, 960.0)]
        );
        // three pages: bottom(0), fresh, the end
        assert_eq!(
            heights(body_box_slices(strips, 0.0, 500.0, 2500.0, (2, 620.0))),
            [(0, 960.0), (1, 1000.0), (2, 620.0)]
        );
        // the end past the page area is clamped; before it, trailing padding is in `end` already
        assert_eq!(
            heights(body_box_slices(strips, 0.0, 500.0, 2500.0, (1, 1500.0))),
            [(0, 960.0), (1, 1000.0)]
        );
        // x and w are copied into every slice; y is the top of the slice
        let s = body_box_slices(strips, 7.0, 500.0, 2500.0, (2, 620.0));
        assert!(
            s.iter()
                .all(|f| f.x.to_f32() == 7.0 && f.width.to_f32() == 500.0 && f.y.to_f32() == 0.0)
        );
        // a negative or NaN end is no height
        assert_eq!(
            heights(body_box_slices(strips, 0.0, 1.0, 1.0, (1, f32::NAN))),
            [(0, 960.0), (1, 1000.0)]
        );
        assert_eq!(
            heights(body_box_slices(strips, 0.0, 1.0, 1.0, (1, -5.0))),
            [(0, 960.0), (1, 0.0)]
        );
    }

    /// F111: a body with a line taller than a page (a box alone on its line) is left to the walk over its children,
    /// which fragments the box (F30f): the body is not split around a line it cannot split.
    #[test]
    fn body_inline_root_with_a_line_taller_than_a_page_is_left_to_the_walk() {
        let html = "<html><body style=\"margin:0;line-height:20px\">Intro<br>\
                    <div id=\"box\" style=\"display:inline-block;width:100%;height:1700px\">x</div></body></html>";
        let mut doc = parse(html, 600.0);
        let body_id = body_node(&doc);
        assert!(doc.get_node(body_id).unwrap().flags.is_inline_root());
        let table = run_pass(&mut doc, 800.0);
        assert_eq!(
            table[&body_id].fragments.len(),
            1,
            "body keeps its single fragment"
        );
        assert!(table[&body_id].line_slices.is_empty());
        let box_id = node_with_id(&doc, "box");
        assert!(
            table[&box_id].fragments.len() >= 2,
            "the box is fragmented on its own: {:?}",
            table[&box_id].fragments
        );
    }

    /// F111: a barrier keeps body whole (the pagination warning names it, see `warn_unprinted_inline_lines`).
    #[test]
    fn transformed_body_inline_root_is_not_split() {
        let mut doc = parse(
            &body_lines_html(200, "transform:translateX(0)", None),
            600.0,
        );
        let body_id = body_node(&doc);
        let table = run_pass(&mut doc, 800.0);
        assert_eq!(table[&body_id].fragments.len(), 1);
        assert_eq!(implied_page_count(&table), 1);
    }

    /// F111: the body walk no longer runs for a split body, and a running element in it still gets its
    /// placeholder on page 0 (it is the margin box's source).
    #[test]
    fn split_body_still_records_a_running_placeholder() {
        let text = (1..=200)
            .map(|i| format!("Line{i:03}"))
            .collect::<Vec<_>>()
            .join("<br>");
        let html = format!(
            "<!DOCTYPE html><html><body style=\"margin:0;line-height:20px\">\
             <div class=\"run\">Footer</div>{text}</body></html>"
        );
        let (doc, _store, geometry) = running_geometry(&html);
        let run_id = doc.query_selector(".run").unwrap().expect(".run node");
        let body_id = body_node(&doc);
        // `running_geometry` paginates 800pt pages (1066px): 4000px of lines make 4.
        assert_eq!(geometry[&body_id].fragments.len(), 4, "body is split");
        let run = &geometry[&run_id].fragments;
        assert_eq!(run.len(), 1, "{run:?}");
        assert_eq!(run[0].page_index, 0);
    }

    /// fulgur-6tco: synthesize a geometry table + string_set_by_node
    /// map and verify `collect_string_set_states` produces the expected
    /// per-page `(start, first, last)` shape.
    #[test]
    fn string_set_state_carries_across_pages() {
        use super::StringSetPageState;

        // Three nodes: A on page 0, B on page 0, C on page 1.
        // A sets header="a", B sets header="b" (so first/last on page 0
        // differ), C sets nothing — page 1 inherits "b" via carry.
        let mut geom = PaginationGeometryTable::new();
        geom.entry(10).or_default().fragments.push(Fragment {
            page_index: 0,
            x: 0.0_f32.as_px(),
            y: 0.0_f32.as_px(),
            width: 100.0_f32.as_px(),
            height: 50.0_f32.as_px(),
        });
        geom.entry(20).or_default().fragments.push(Fragment {
            page_index: 0,
            x: 0.0_f32.as_px(),
            y: 50.0_f32.as_px(),
            width: 100.0_f32.as_px(),
            height: 50.0_f32.as_px(),
        });
        geom.entry(30).or_default().fragments.push(Fragment {
            page_index: 1,
            x: 0.0_f32.as_px(),
            y: 0.0_f32.as_px(),
            width: 100.0_f32.as_px(),
            height: 50.0_f32.as_px(),
        });

        let mut markers: BTreeMap<usize, Vec<(String, String)>> = BTreeMap::new();
        markers.insert(10, vec![("header".into(), "a".into())]);
        markers.insert(20, vec![("header".into(), "b".into())]);

        let states = super::collect_string_set_states(&geom, &markers);
        assert_eq!(states.len(), 2);

        // Page 0: no carry (first page), first set by A, last updated by B.
        let p0 = &states[0]["header"];
        assert_eq!(
            *p0,
            StringSetPageState {
                start: None,
                first: Some("a".into()),
                last: Some("b".into()),
            }
        );
        // Page 1: carry from p0.last ("b"). C sets nothing → first/last stay None.
        let p1 = &states[1]["header"];
        assert_eq!(
            *p1,
            StringSetPageState {
                start: Some("b".into()),
                first: None,
                last: None,
            }
        );
    }

    #[test]
    fn string_set_first_appearance_only_for_split_paragraph() {
        // A node spans two pages (inline-aware split). Markers fire
        // only on the first appearance.
        use super::StringSetPageState;

        let mut geom = PaginationGeometryTable::new();
        geom.entry(42).or_default().fragments.push(Fragment {
            page_index: 0,
            x: 0.0_f32.as_px(),
            y: 0.0_f32.as_px(),
            width: 100.0_f32.as_px(),
            height: 800.0_f32.as_px(),
        });
        geom.entry(42).or_default().fragments.push(Fragment {
            page_index: 1,
            x: 0.0_f32.as_px(),
            y: 0.0_f32.as_px(),
            width: 100.0_f32.as_px(),
            height: 200.0_f32.as_px(),
        });

        let mut markers: BTreeMap<usize, Vec<(String, String)>> = BTreeMap::new();
        markers.insert(42, vec![("title".into(), "intro".into())]);

        let states = super::collect_string_set_states(&geom, &markers);
        assert_eq!(states.len(), 2);
        assert_eq!(
            states[0]["title"],
            StringSetPageState {
                start: None,
                first: Some("intro".into()),
                last: Some("intro".into()),
            }
        );
        assert_eq!(
            states[1]["title"],
            StringSetPageState {
                start: Some("intro".into()),
                first: None,
                last: None,
            }
        );
    }

    #[test]
    fn string_set_states_empty_geometry_returns_one_empty_page() {
        // "Always at least one page" convention: even an empty
        // geometry yields a single empty per-page state so downstream
        // consumers can index by page without special-casing zero.
        let geom = PaginationGeometryTable::new();
        let markers = BTreeMap::new();
        let states = super::collect_string_set_states(&geom, &markers);
        assert_eq!(states.len(), 1);
        assert!(states[0].is_empty());
    }

    /// fulgur-jkl5: `position: fixed` element should emit one
    /// fragment per page so downstream rendering can repeat it.
    #[test]
    fn position_fixed_repeats_per_page() {
        let html = r#"
            <html><body>
              <div style="height: 600px"></div>
              <div style="height: 600px"></div>
              <div style="position: fixed; top: 10px; left: 20px;
                          width: 100px; height: 50px"></div>
            </body></html>
        "#;
        let mut doc = parse(html, 600.0);

        let mut geom = super::run_pass(doc.deref_mut(), 800.0);
        let pages_before = super::implied_page_count(&geom);
        assert!(
            pages_before >= 2,
            "two 600px blocks on 800px page should split → {pages_before} pages",
        );

        super::append_position_fixed_fragments(
            &mut geom,
            doc.deref_mut(),
            pages_before,
            600.0,
            800.0,
        );

        // The fixed div should now appear in `geom` with one fragment
        // per page. We don't know its NodeId statically, so locate it
        // by the per-fragment width = 100.0.
        let fixed_entries: Vec<_> = geom
            .iter()
            .filter(|(_, g)| {
                g.fragments.iter().any(|f| {
                    (f.width.to_f32() - 100.0).abs() < 0.5 && (f.height.to_f32() - 50.0).abs() < 0.5
                })
            })
            .collect();
        assert_eq!(
            fixed_entries.len(),
            1,
            "exactly one fixed element entry expected, got {}",
            fixed_entries.len()
        );
        let (_, fixed_geom) = fixed_entries[0];
        assert_eq!(
            fixed_geom.fragments.len() as u32,
            pages_before,
            "fixed element should have one fragment per page",
        );
        let pages_seen: Vec<u32> = fixed_geom.fragments.iter().map(|f| f.page_index).collect();
        assert_eq!(pages_seen, (0..pages_before).collect::<Vec<_>>());
    }

    #[test]
    fn position_fixed_with_no_pages_normalises_to_one_page() {
        // append_position_fixed_fragments(geom, doc, 0) should still
        // emit exactly one fragment per fixed element (the "always
        // at least one page" convention applied to fixed
        // repetition).
        let html = r#"
            <html><body>
              <div style="position: fixed; top: 0; left: 0;
                          width: 50px; height: 30px"></div>
            </body></html>
        "#;
        let mut doc = parse(html, 600.0);
        let mut geom = PaginationGeometryTable::new();
        super::append_position_fixed_fragments(&mut geom, doc.deref_mut(), 0, 600.0, 800.0);
        assert_eq!(geom.len(), 1);
        let (_, g) = geom.iter().next().unwrap();
        assert_eq!(g.fragments.len(), 1);
        assert_eq!(g.fragments[0].page_index, 0);
    }

    /// fulgur-a8m5: `append_position_fixed_fragments` must resolve
    /// `bottom: 0` against the viewport CB. Taffy's `compute_root_layout`
    /// (used by `relayout_position_fixed`) does not honour end-side
    /// insets when the absolute element IS the layout-tree root, so
    /// `final_layout.location.y` stays at 0 even for `bottom: 0`. v2's
    /// dispatch reads `pagination_geometry` directly, so without inset
    /// resolution here, WPT fixedpos-001 / fixedpos-002 / fixedpos-008
    /// render their `bottom: 0` fixed text at the top of every page
    /// instead of the bottom.
    #[test]
    fn position_fixed_bottom_zero_resolves_against_viewport() {
        let html = r#"
            <html><body style="margin:0">
              <div style="position: fixed; bottom: 0; height: 30px"></div>
            </body></html>
        "#;
        let mut doc = parse(html, 600.0);
        crate::blitz_adapter::relayout_position_fixed(&mut doc, 600.0, 800.0);
        let mut geom = PaginationGeometryTable::new();
        super::append_position_fixed_fragments(&mut geom, doc.deref_mut(), 1, 600.0, 800.0);

        // Locate the fixed div fragment by its 30px height.
        let entries: Vec<_> = geom
            .iter()
            .filter(|(_, g)| {
                g.fragments
                    .iter()
                    .any(|f| (f.height.to_f32() - 30.0).abs() < 0.5)
            })
            .collect();
        assert_eq!(entries.len(), 1, "exactly one fixed entry");
        let (_, g) = entries[0];
        assert_eq!(g.fragments.len(), 1);
        let frag = &g.fragments[0];
        // viewport_h_px=800, height=30 → bottom edge sits at 800 → top at 770.
        // body has zero height (no in-flow content), so body_offset_xy=(0,0).
        let frag_y = frag.y.to_f32();
        assert!(
            (frag_y - 770.0).abs() < 1.0,
            "bottom:0 fixed should resolve to y=770 (viewport_h - height); got {frag_y}",
        );
    }

    /// fulgur-orcx: `position: fixed` uses `bottom` anchoring against
    /// viewport-anchored CB. When both values are sub-pixel, resolving
    /// via the exact computed formula and rounding order used by Taffy
    /// keeps fixed root and abs-in-relative reference paths aligned.
    /// Without this, fixed-path can land 0.5px lower and fail
    /// WPT fixedpos-009-print by halo edge.
    #[test]
    fn position_fixed_bottom_zero_rounds_like_taffy_with_fractional_viewport_height() {
        let html = r#"
            <html><body style="margin:0">
              <div style="position: fixed; right: 0; bottom: 0; width: 36px; height: 36px">
              </div>
            </body></html>
        "#;
        let mut doc = parse(html, 600.0);
        crate::blitz_adapter::relayout_position_fixed(&mut doc, 600.0, 800.6);
        let mut geom = PaginationGeometryTable::new();
        super::append_position_fixed_fragments(&mut geom, doc.deref_mut(), 1, 600.0, 800.6);

        let entries: Vec<_> = geom
            .iter()
            .filter(|(_, g)| {
                g.fragments
                    .iter()
                    .any(|f| (f.width.to_f32() - 36.0).abs() < 0.5)
            })
            .collect();
        assert_eq!(entries.len(), 1, "expected one fixed entry");
        let frag = entries[0].1.fragments.first().unwrap();
        assert_eq!(frag.page_index, 0, "expected fixed fragment only on page 0");
        // With Taffy-like rounding: round(cb_h - b) - round(h) => round(800.6)-36 = 765.
        let frag_y = frag.y.to_f32();
        assert!(
            (frag_y - 765.0).abs() < 0.25,
            "fractional fixed bottom anchor should resolve to y≈765; got {frag_y}",
        );
    }

    #[test]
    fn position_absolute_body_direct_bottom_viewport_page_stride_keeps_page_local_y_stable() {
        let html = r#"
            <html><body style="margin:0">
              <div style="position: absolute; bottom: -971px; height: 19px">x</div>
            </body></html>
        "#;
        let mut doc = parse(html, 600.0);
        let mut geom = PaginationGeometryTable::new();
        super::append_position_absolute_body_direct_fragments(
            &mut geom,
            doc.deref_mut(),
            3,
            600.0,
            971.338_87,
            None,
        );

        let frag = geom
            .values()
            .flat_map(|g| &g.fragments)
            .find(|f| f.page_index == 1 && (f.height.to_f32() - 19.0).abs() < 0.5)
            .expect("absolute bottom:-viewport fragment should land on page 1");
        let frag_y = frag.y.to_f32();
        assert!(
            (frag_y - 952.0).abs() < 0.01,
            "absolute ref fragment should keep the same page-local bottom anchor as fixed; got {frag_y}",
        );
    }

    /// fulgur-a8m5: body's collapsed-margin offset (e.g. an in-flow
    /// child with `margin-top:4em`) appears in
    /// `drawables.body_offset_pt`, which the v2 dispatch path adds to
    /// every fragment's y. Viewport-anchored fixed elements must
    /// subtract that offset at storage time so the dispatched y lands
    /// at the page-relative position the CSS asks for. Locks the math
    /// PDF y = margin_top_pt + body_offset_pt + (frag.y_pt) — for
    /// `top:0` we want PDF y = margin_top_pt, so frag.y_px must equal
    /// `-body_offset_y_px`.
    #[test]
    fn position_fixed_top_zero_compensates_for_body_offset() {
        // The in-flow div pushes body's content area down by ~4em.
        let html = r#"
            <html><body>
              <div style="margin-top:4em">x</div>
              <div style="position: fixed; top: 0; height: 30px"></div>
            </body></html>
        "#;
        let mut doc = parse(html, 600.0);
        crate::blitz_adapter::relayout_position_fixed(&mut doc, 600.0, 800.0);
        let body_y_px = super::body_origin_in_px(doc.deref_mut()).1;
        // Sanity: body_offset must be non-zero, otherwise the test is
        // not exercising compensation at all.
        assert!(
            body_y_px > 0.5,
            "test assumes body has a non-zero offset; got {body_y_px}"
        );
        let mut geom = PaginationGeometryTable::new();
        super::append_position_fixed_fragments(&mut geom, doc.deref_mut(), 1, 600.0, 800.0);

        let entries: Vec<_> = geom
            .iter()
            .filter(|(_, g)| {
                g.fragments
                    .iter()
                    .any(|f| (f.height.to_f32() - 30.0).abs() < 0.5)
            })
            .collect();
        assert_eq!(entries.len(), 1, "exactly one fixed entry");
        let frag = entries[0].1.fragments.first().unwrap();
        // top:0 → resolved_y=0 → stored_y = 0 - body_y_px = -body_y_px.
        let frag_y = frag.y.to_f32();
        assert!(
            (frag_y - (-body_y_px)).abs() < 0.5,
            "top:0 fixed frag.y must be -body_offset (={}); got {}",
            -body_y_px,
            frag_y
        );
    }

    /// html-to-pdf F40 / F187: on pages 2+ `render_v2` adds no body offset, so a fixed box's fragment holds the
    /// resolved y there, and x is never reduced by body's x offset (in-flow fragments already hold it).
    #[test]
    fn position_fixed_keeps_its_page_position_on_pages_after_the_first() {
        let html = r#"
            <html><body style="margin:30px 20px">
              <div style="height:100px">x</div>
              <div style="position: fixed; top: 50px; left: 70px; width: 40px; height: 30px"></div>
            </body></html>
        "#;
        let mut doc = parse(html, 600.0);
        crate::blitz_adapter::relayout_position_fixed(&mut doc, 600.0, 800.0);
        let (body_x, body_y) = super::body_origin_in_px(doc.deref_mut());
        assert!(body_x > 0.5 && body_y > 0.5, "test needs a body offset");
        let mut geom = PaginationGeometryTable::new();
        super::append_position_fixed_fragments(&mut geom, doc.deref_mut(), 2, 600.0, 800.0);
        let g = geom
            .values()
            .find(|g| {
                g.fragments
                    .iter()
                    .any(|f| (f.height.to_f32() - 30.0).abs() < 0.5)
            })
            .expect("the fixed box");
        assert_eq!(g.fragments.len(), 2);
        let (p0, p1) = (&g.fragments[0], &g.fragments[1]);
        assert_eq!((p0.page_index, p1.page_index), (0, 1));
        assert_eq!(p0.x.to_f32(), 70.0, "x is the ICB x on page 0");
        assert_eq!(p1.x.to_f32(), 70.0, "and on page 1");
        assert_eq!(
            p0.y.to_f32(),
            50.0 - body_y,
            "page 0: the body offset is added back by the renderer"
        );
        assert_eq!(
            p1.y.to_f32(),
            50.0,
            "page 1: no body offset is added, none is taken off"
        );
    }

    /// html-to-pdf F187: the frame rule of the geometry passes: x as is, body's y offset off page 0 only.
    #[test]
    fn icb_to_fragment_takes_the_body_offset_off_page_zero_only() {
        assert_eq!(super::icb_to_fragment(0, 50.0, 100.0, 40.0), (50.0, 60.0));
        assert_eq!(super::icb_to_fragment(2, 50.0, 100.0, 40.0), (50.0, 100.0));
        assert_eq!(super::icb_to_fragment(0, 7.0, -5.0, 40.0), (7.0, -45.0));
        assert_eq!(super::icb_to_fragment(1, 7.0, -5.0, 40.0), (7.0, -5.0));
        assert_eq!(super::icb_to_fragment(0, 3.0, 10.0, 0.0), (3.0, 10.0));
    }

    /// `body_direct_root_icb` of the box `#a` in `html` (viewport 600 x 800), against `<body>`.
    fn root_icb(html: &str) -> (f32, f32) {
        let mut doc = parse(html, 600.0);
        let a = find_by_id(doc.deref_mut(), "a").expect("#a");
        let body_id = super::find_body_id(doc.deref_mut()).expect("body");
        let (a, body) = (doc.get_node(a).unwrap(), doc.get_node(body_id).unwrap());
        super::body_direct_root_icb(a, body, 600.0, 800.0)
    }

    fn assert_near(got: (f32, f32), want: (f32, f32)) {
        assert!(
            (got.0 - want.0).abs() < 0.5 && (got.1 - want.1).abs() < 0.5,
            "got {got:?}, want {want:?}"
        );
    }

    const ROOT_BODY: &str = "margin:30px;border:4px solid #c00";
    const ROOT_BOX: &str = "position:absolute;width:100px;height:100px";

    /// html-to-pdf F187: explicit insets are resolved against the initial containing block, whatever body's box is.
    #[test]
    fn body_direct_root_with_explicit_insets_is_placed_in_the_icb() {
        let html = format!(
            "<html><body style=\"{ROOT_BODY}\"><div id=\"a\" style=\"{ROOT_BOX};top:50px;left:50px\"></div></body></html>"
        );
        assert_near(root_icb(&html), (50.0, 50.0));
        let html = format!(
            "<html><body style=\"{ROOT_BODY}\"><div id=\"a\" style=\"{ROOT_BOX};right:50px;bottom:50px\"></div></body></html>"
        );
        assert_near(root_icb(&html), (450.0, 650.0));
    }

    /// An axis with both insets `auto` keeps the static position, which includes body's origin (margin 30 + border 4
    /// to the content, below the 100 px block).
    #[test]
    fn body_direct_root_with_auto_insets_keeps_the_static_position() {
        let html = format!(
            "<html><body style=\"{ROOT_BODY}\"><div style=\"height:100px\"></div>\
             <div id=\"a\" style=\"{ROOT_BOX}\"></div></body></html>"
        );
        assert_near(root_icb(&html), (34.0, 134.0));
    }

    /// Mixed axes: `left` is explicit (ICB), `top` is `auto` (static position, body's origin included).
    #[test]
    fn body_direct_root_with_mixed_axes_resolves_each_axis_on_its_own() {
        let html = format!(
            "<html><body style=\"{ROOT_BODY}\"><div style=\"height:100px\"></div>\
             <div id=\"a\" style=\"{ROOT_BOX};left:50px\"></div></body></html>"
        );
        assert_near(root_icb(&html), (50.0, 134.0));
        let html = format!(
            "<html><body style=\"{ROOT_BODY}\"><div style=\"height:100px\"></div>\
             <div id=\"a\" style=\"{ROOT_BOX};top:50px\"></div></body></html>"
        );
        assert_near(root_icb(&html), (34.0, 50.0));
    }

    /// A positioned body is the containing block: Taffy has resolved both axes against its padding box.
    #[test]
    fn body_direct_root_in_a_positioned_body_is_placed_in_its_padding_box() {
        let body = format!("{ROOT_BODY};position:relative");
        let html = format!(
            "<html><body style=\"{body}\"><div style=\"height:300px\"></div>\
             <div id=\"a\" style=\"{ROOT_BOX};top:50px;left:50px\"></div></body></html>"
        );
        assert_near(root_icb(&html), (84.0, 84.0));
        // Body's border box is 540 x 308 at (30, 30): its padding box ends at x 566, y 334.
        let html = format!(
            "<html><body style=\"{body}\"><div style=\"height:300px\"></div>\
             <div id=\"a\" style=\"{ROOT_BOX};right:50px;bottom:50px\"></div></body></html>"
        );
        assert_near(root_icb(&html), (416.0, 184.0));
    }

    /// html-to-pdf F187: the body-direct pass stores the box in the fragmenter's frame: x is the ICB x, y has body's
    /// offset taken off on page 0 only (the renderer adds it there and nowhere else).
    #[test]
    fn body_direct_boxes_are_stored_in_the_fragmenter_frame() {
        let html = r#"
            <html><body style="margin:30px;border:4px solid #c00">
              <div id="p0" style="position:absolute;top:50px;left:50px;width:100px;height:100px"></div>
              <div id="p2" style="position:absolute;top:2150px;left:50px;width:100px;height:100px"></div>
            </body></html>
        "#;
        let mut doc = parse(html, 600.0);
        let (body_x, body_y) = super::body_origin_in_px(doc.deref_mut());
        assert_eq!((body_x, body_y), (30.0, 30.0));
        let p0 = find_by_id(doc.deref_mut(), "p0").unwrap();
        let p2 = find_by_id(doc.deref_mut(), "p2").unwrap();
        let mut geom = PaginationGeometryTable::new();
        super::append_position_absolute_body_direct_fragments(
            &mut geom,
            doc.deref_mut(),
            1,
            600.0,
            1000.0,
            None,
        );
        let f0 = &geom[&p0].fragments;
        assert_eq!(f0.len(), 1);
        assert_eq!(
            (f0[0].page_index, f0[0].x.to_f32(), f0[0].y.to_f32()),
            (0, 50.0, 20.0),
            "page 0: x as is, y 50 less body's 30"
        );
        let f2 = &geom[&p2].fragments;
        assert_eq!(f2.len(), 1);
        assert_eq!(
            (f2[0].page_index, f2[0].x.to_f32(), f2[0].y.to_f32()),
            (2, 50.0, 150.0),
            "page 2: no body offset at all"
        );
    }

    /// fulgur-4m16: when a `position: fixed` root has a sized
    /// block-element child, the child must also receive per-page
    /// repeated fragments. v2 dispatch reads
    /// `pagination_geometry[node_id]` and never recurses into a fixed
    /// root's subtree, so without per-descendant fragments the child
    /// is never drawn (WPT fixedpos-009: a `<div class="pencil"
    /// style="width:36px; height:36px">` inside the fixed root never
    /// renders, leaving every page blank where the pencil should be).
    #[test]
    fn position_fixed_emits_fragments_for_block_descendants() {
        // Use shrink-to-fit-friendly width / height on the fixed root
        // so Taffy gives it a definite size; otherwise an unsized fixed
        // root inherits available_space (600 wide), which obscures the
        // root-vs-child position relationship the test pins. The bug
        // being tested (descendants missing from geometry) is
        // independent of root sizing.
        let html = r#"
            <html><body style="margin:0">
              <div style="position: fixed; bottom: 0; width: 36px; height: 36px">
                <div style="width: 36px; height: 36px"></div>
              </div>
            </body></html>
        "#;
        let mut doc = parse(html, 600.0);
        crate::blitz_adapter::relayout_position_fixed(&mut doc, 600.0, 800.0);
        let mut geom = PaginationGeometryTable::new();
        super::append_position_fixed_fragments(&mut geom, doc.deref_mut(), 2, 600.0, 800.0);

        // Both root and its 36×36 child must appear in `geom`. The
        // root's fragments come from the existing inset-resolution
        // path; the child's fragments come from the new descendants
        // walker (fulgur-4m16). Without that walker, only one entry
        // (the root) shows up.
        let entries: Vec<_> = geom
            .iter()
            .filter(|(_, g)| {
                g.fragments.iter().any(|f| {
                    (f.width.to_f32() - 36.0).abs() < 0.5 && (f.height.to_f32() - 36.0).abs() < 0.5
                })
            })
            .collect();
        assert_eq!(
            entries.len(),
            2,
            "expected 2 entries (fixed root + child), got {} — fulgur-4m16: \
             without record_fixed_subtree_descendants the child entry is missing",
            entries.len(),
        );

        // Both must be `is_repeat = true` with one fragment per page.
        for (_, g) in &entries {
            assert!(g.is_repeat, "entry must be is_repeat=true");
            assert_eq!(g.fragments.len(), 2, "entry: one fragment per page");
            let pages_seen: Vec<u32> = g.fragments.iter().map(|f| f.page_index).collect();
            assert_eq!(pages_seen, vec![0u32, 1u32]);
        }

        // Both entries' first fragments must agree on (x, y) because
        // the child's `final_layout.location` inside the root is
        // (0, 0) (no padding/margin/border on the root). A divergence
        // here would mean the descendants walker is computing offsets
        // wrong.
        let f0 = &entries[0].1.fragments[0];
        let f1 = &entries[1].1.fragments[0];
        let f0x = f0.x.to_f32();
        let f0y = f0.y.to_f32();
        let f1x = f1.x.to_f32();
        let f1y = f1.y.to_f32();
        assert!(
            (f0x - f1x).abs() < 0.5 && (f0y - f1y).abs() < 0.5,
            "root and child must share (x, y); got root=({f0x},{f0y}) child=({f1x},{f1y})",
        );

        // Pin the y coordinate: bottom:0 with height=36 in an 800px
        // viewport places the box top at y=764. body is empty so
        // body_offset_xy=(0,0).
        assert!(
            (f0y - 764.0).abs() < 1.0,
            "bottom:0 fixed (h=36) must resolve to y=764 (viewport_h - h); got {f0y}",
        );
    }

    /// fulgur-a8m5: body-direct `position: absolute` with `bottom: 0`
    /// should land at the bottom of page 0 with its descendant Paragraph
    /// fragments at the same position. The fragmenter unconditionally
    /// skips out-of-flow children, so this pass is the only thing that
    /// puts these abs body-direct nodes into `pagination_geometry` for
    /// v2 dispatch (WPT fixedpos-001 ref, fixedpos-008 ref).
    #[test]
    fn position_absolute_body_direct_bottom_zero_lands_on_page_zero() {
        let html = r#"
            <html><body style="margin:0">
              <div style="position: absolute; bottom: 0; height: 30px">x</div>
            </body></html>
        "#;
        let mut doc = parse(html, 600.0);
        let mut geom = PaginationGeometryTable::new();
        super::append_position_absolute_body_direct_fragments(
            &mut geom,
            doc.deref_mut(),
            1,
            600.0,
            800.0,
            None,
        );

        // Locate the abs div by height=30.
        let mut found = None;
        for g in geom.values() {
            for f in &g.fragments {
                if (f.height.to_f32() - 30.0).abs() < 0.5 {
                    found = Some(f.clone());
                }
            }
        }
        let frag = found.expect("abs body-direct fragment for bottom:0 must be emitted");
        assert_eq!(frag.page_index, 0);
        // Same math as the fixed case: body has zero height, so
        // body_offset compensation is a no-op and viewport-CB
        // resolution gives y = 800 - 30 = 770.
        let frag_y = frag.y.to_f32();
        assert!(
            (frag_y - 770.0).abs() < 1.0,
            "abs body-direct bottom:0 should land at y=770; got {frag_y}",
        );
    }

    /// fulgur-z4zc: a body-direct `position:absolute` subtree whose
    /// viewport-CB y range crosses page boundaries must emit geometry on
    /// every intersected page. The main fragmenter skips absolute OOF
    /// children, so `append_position_absolute_body_direct_fragments` is
    /// responsible for recording these page-local placements.
    #[test]
    fn position_absolute_body_direct_overflow_emits_fragments_on_each_page() {
        let html = r#"
            <html><body style="margin:0">
              <div style="position: absolute; top: 0; width: 100px; height: 1800px">
                <div style="width: 50px; height: 1800px"></div>
              </div>
            </body></html>
        "#;
        let mut doc = parse(html, 600.0);
        let mut geom = PaginationGeometryTable::new();
        super::append_position_absolute_body_direct_fragments(
            &mut geom,
            doc.deref_mut(),
            1,
            600.0,
            800.0,
            None,
        );

        let mut tall_entries: Vec<Vec<u32>> = geom
            .values()
            .filter(|g| {
                g.fragments
                    .iter()
                    .any(|f| (f.height.to_f32() - 1800.0).abs() < 0.5)
            })
            .map(|g| {
                let mut pages: Vec<u32> = g.fragments.iter().map(|f| f.page_index).collect();
                pages.sort_unstable();
                pages
            })
            .collect();
        tall_entries.sort();

        assert_eq!(
            tall_entries,
            vec![vec![0, 1, 2], vec![0, 1, 2]],
            "expected absolute root and in-flow child to emit fragments on pages 0, 1, and 2; got {tall_entries:?}"
        );
    }

    /// fulgur-2m6w: a body-direct `position:absolute` node positioned far
    /// beyond the page budget (`top: 99999999px`) must not extend the page
    /// count without bound. The abs path emits one fragment per intersected
    /// page and `descendant_total_pages` feeds `implied_page_count`, so an
    /// unclamped page index from a tiny box makes `render_v2` allocate and
    /// render ~10^5 pages — the same small-input DoS the body-direct slice
    /// cap blocks, reached via the absolute-positioning path (Codex review
    /// on PR #501). Without the clamp this input lands at page ~125000;
    /// page indices are clamped to `MAX_PAGES`.
    #[test]
    fn position_absolute_far_offset_is_page_capped() {
        let html = r#"
            <html><body style="margin:0">
              <div style="position: absolute; top: 99999999px; width: 10px; height: 10px"></div>
            </body></html>
        "#;
        let mut doc = parse(html, 600.0);
        let mut geom = PaginationGeometryTable::new();
        super::append_position_absolute_body_direct_fragments(
            &mut geom,
            doc.deref_mut(),
            1,
            600.0,
            800.0,
            None,
        );
        // The fragment must still be emitted (not silently dropped) — just
        // pinned to the cap page rather than page ~125000.
        let max_page = geom
            .values()
            .flat_map(|g| g.fragments.iter())
            .map(|f| f.page_index)
            .max()
            .expect("abs fragment must be emitted");
        assert!(
            max_page <= crate::MAX_PAGES,
            "abs page index must be clamped to MAX_PAGES ({}), got {max_page}",
            crate::MAX_PAGES,
        );
    }

    /// fulgur-2m6w (Codex review on PR #501): the `MAX_PAGES` clamp must
    /// apply ONLY to the page-extension path. When `total_pages` legitimately
    /// exceeds `MAX_PAGES` (many ordinary in-flow pages — input-proportional,
    /// not amplified), a NON-extending absolute element starting at an
    /// in-budget page above the cap must emit a single fragment at its real
    /// page, not a spurious run from `MAX_PAGES` through the real page.
    #[test]
    fn position_absolute_in_budget_start_above_cap_not_clamped() {
        let page_h = 800.0_f32;
        let start_page = crate::MAX_PAGES + 20_000; // > MAX_PAGES
        let total_pages = start_page + 5; // in-budget
        let top = page_h * start_page as f32;
        // In-flow `<p>` makes `body_has_in_flow_content` true, so
        // `may_extend_pages` is false → the non-extending branch.
        let html = format!(
            r#"<html><body style="margin:0">
                 <p>in-flow</p>
                 <div style="position:absolute; top:{top}px; width:10px; height:10px"></div>
               </body></html>"#
        );
        let mut doc = parse(&html, 600.0);
        let mut geom = PaginationGeometryTable::new();
        super::append_position_absolute_body_direct_fragments(
            &mut geom,
            doc.deref_mut(),
            total_pages,
            600.0,
            page_h,
            None,
        );
        // Isolate the 10px-wide abs node's fragments.
        let abs_pages: Vec<u32> = geom
            .values()
            .filter(|g| {
                g.fragments
                    .iter()
                    .any(|f| (f.width.to_f32() - 10.0).abs() < 0.5)
            })
            .flat_map(|g| g.fragments.iter().map(|f| f.page_index))
            .collect();
        assert_eq!(
            abs_pages,
            vec![start_page],
            "non-extending in-budget abs must emit only at its real page \
             (no spurious clamp run), got {abs_pages:?}",
        );
    }

    #[test]
    fn position_absolute_body_direct_expanded_pages_reach_later_text() {
        let html = r#"
            <html><body style="margin:0">
              <div style="position: absolute; top: 0; width: 100%; background: yellow">
                <div style="contain: size; width: 50px; height: 1800px"></div>
                This text should land after the contained child.
              </div>
            </body></html>
        "#;
        let mut doc = parse(html, 600.0);
        let mut geom = PaginationGeometryTable::new();
        super::append_position_absolute_body_direct_fragments(
            &mut geom,
            doc.deref_mut(),
            1,
            600.0,
            800.0,
            None,
        );

        let has_later_text_fragment = geom.values().any(|g| {
            g.fragments.iter().any(|f| {
                f.page_index == 1
                    && f.height > crate::units::Px::ZERO
                    && f.height < 100.0_f32.as_px()
            })
        });

        assert!(
            has_later_text_fragment,
            "expected later text/anonymous fragment on page 1; got {geom:?}"
        );
    }

    #[test]
    fn position_fixed_repeats_on_pages_added_by_body_direct_absolute() {
        let html = r#"
            <html><body style="margin:0">
              <div style="position: fixed; top: 0; width: 10px; height: 20px"></div>
              <div style="position: absolute; top: 0; width: 100px; height: 1200px"></div>
            </body></html>
        "#;
        let engine = crate::Engine::builder().build();
        let (_, geom) = engine.build_drawables_and_geometry_for_testing_no_gcpm(html);

        let fixed_pages = geom
            .values()
            .find(|g| {
                g.is_repeat
                    && g.fragments.iter().any(|f| {
                        (f.width.to_f32() - 10.0).abs() < 0.5
                            && (f.height.to_f32() - 20.0).abs() < 0.5
                    })
            })
            .map(|g| {
                let mut pages: Vec<u32> = g.fragments.iter().map(|f| f.page_index).collect();
                pages.sort_unstable();
                pages
            });

        assert_eq!(fixed_pages, Some(vec![0, 1]));
    }

    #[test]
    fn fixed_subtree_fragment_total_is_bounded() {
        // Security regression: a `position: fixed` root and every in-flow
        // descendant each emit one fragment per page → O(nodes × pages).
        // The aggregate MAX_SUBTREE_PAGE_FRAGMENTS budget caps the retained
        // total regardless of how many descendants the subtree has.
        let mut kids = String::new();
        for _ in 0..20_000 {
            kids.push_str(r#"<div style="height:1px">x</div>"#);
        }
        let html = format!(
            r#"<html><body style="margin:0">
              <div style="position: fixed; top:0; width:50px; height:50px">{kids}</div>
              <div style="height:100000px"></div>
            </body></html>"#
        );
        let engine = crate::Engine::builder().build();
        let (_, geom) = engine.build_drawables_and_geometry_for_testing_no_gcpm(&html);
        let total: usize = geom
            .values()
            .filter(|g| g.is_repeat)
            .map(|g| g.fragments.len())
            .sum();
        assert!(
            total <= crate::MAX_SUBTREE_PAGE_FRAGMENTS,
            "fixed fragment total must be bounded, got {total}"
        );
    }

    #[test]
    fn absolute_subtree_fragment_total_is_bounded() {
        // Security regression (same class as fixed-subtree fragments): the
        // body-direct absolute pass records, for every node in each absolute
        // subtree, one fragment per page the node intersects
        // (`first_page..=last_page`, bounded per-node by MAX_PAGES). Many
        // page-spanning absolute elements amplify to O(nodes × pages). The
        // aggregate MAX_SUBTREE_PAGE_FRAGMENTS budget caps the retained total.
        let mut sibs = String::new();
        for _ in 0..15_000 {
            sibs.push_str(
                r#"<div style="position:absolute; top:0; width:10px; height:200000px"></div>"#,
            );
        }
        let html = format!(r#"<html><body style="margin:0">{sibs}</body></html>"#);
        let engine = crate::Engine::builder().build();
        let (_, geom) = engine.build_drawables_and_geometry_for_testing_no_gcpm(&html);
        let total: usize = geom.values().map(|g| g.fragments.len()).sum();
        // The absolute pass is capped at MAX_SUBTREE_PAGE_FRAGMENTS; the sum
        // also counts incidental non-absolute fragments (root/body/in-flow),
        // which are themselves bounded by MAX_PAGES (a single in-flow flow).
        // Uncapped this scenario emits ~3M fragments, so the budget clearly
        // fired.
        assert!(
            total <= crate::MAX_SUBTREE_PAGE_FRAGMENTS + crate::MAX_PAGES as usize,
            "absolute fragment total must be bounded, got {total}"
        );
    }

    #[test]
    fn position_absolute_body_direct_tiny_overflow_reaches_next_page() {
        let html = r#"
            <html><body style="margin:0">
              <div id="tiny" style="position: absolute; top: 0; width: 100px; height: 801px"></div>
              <div id="exact" style="position: absolute; top: 0; width: 100px; height: 800px"></div>
            </body></html>
        "#;
        let mut doc = parse(html, 600.0);
        // Reuses the module-level `find_by_id` helper (see the
        // pagination-cap tests) instead of redefining it here.
        let tiny_id = find_by_id(doc.deref_mut(), "tiny").expect("tiny abs node");
        doc.deref_mut()
            .get_node_mut(tiny_id)
            .expect("tiny abs node")
            .final_layout
            .size
            .height = 800.1;
        let mut geom = PaginationGeometryTable::new();
        super::append_position_absolute_body_direct_fragments(
            &mut geom,
            doc.deref_mut(),
            2,
            600.0,
            800.0,
            None,
        );

        let mut tiny_overflow_pages = None;
        let mut exact_boundary_pages = None;
        for g in geom.values() {
            if g.fragments
                .iter()
                .any(|f| (f.height.to_f32() - 800.1).abs() < 0.05)
            {
                let mut pages: Vec<u32> = g.fragments.iter().map(|f| f.page_index).collect();
                pages.sort_unstable();
                tiny_overflow_pages = Some(pages);
            }
            if g.fragments
                .iter()
                .any(|f| (f.height.to_f32() - 800.0).abs() < 0.05)
            {
                let mut pages: Vec<u32> = g.fragments.iter().map(|f| f.page_index).collect();
                pages.sort_unstable();
                exact_boundary_pages = Some(pages);
            }
        }

        assert_eq!(tiny_overflow_pages, Some(vec![0, 1]));
        assert_eq!(exact_boundary_pages, Some(vec![0]));
    }

    #[test]
    fn position_absolute_body_direct_height_overflow_extends_to_last_page() {
        let html = r#"
            <html><body style="margin:0">
              <div style="position: absolute; top: 0; width: 100px; height: 1px"></div>
            </body></html>
        "#;
        let mut doc = parse(html, 600.0);
        let abs_id = find_node_by_local_name(&doc, "div").expect("abs div");
        doc.deref_mut()
            .get_node_mut(abs_id)
            .expect("abs div")
            .final_layout
            .size
            .height = f32::MAX;
        let mut geom = PaginationGeometryTable::new();
        super::record_subtree_fragments_at_offset(
            &mut geom,
            doc.deref_mut(),
            abs_id,
            (0.0, f32::MAX),
            0.0,
            f32::MAX,
            f32::MAX,
            3,
            true,
            &mut 0,
        );

        let pages: Vec<u32> = geom
            .get(&abs_id)
            .expect("abs div geometry")
            .fragments
            .iter()
            .map(|f| f.page_index)
            .collect();
        assert_eq!(pages, vec![1, 2]);
    }

    /// Exercises `PaginationLayoutTree`'s `LayoutPartialTree` /
    /// `RoundTree` / `CacheTree` / `TraversePartialTree` impls
    /// at runtime by routing body's layout through
    /// `taffy::compute_root_layout`. Production reaches geometry via
    /// `fragment_pagination_root` directly (see the docstring on
    /// `drive_taffy_root_layout` for why), so this test is the only
    /// runtime user of those trait impls — without it, `cargo build`
    /// would still type-check the impls but no code path would actually
    /// invoke them. Asserts the geometry the Taffy-driven path produces
    /// matches the direct walk used in production.
    ///
    /// Both sides feed the same `ColumnStyleTable` so the parity check
    /// covers the break-style-aware code path that production wires
    /// through `run_pass_with_break_styles`. The fixture sets
    /// `break-before: page` on the middle child so the geometry differs
    /// from the style-unaware case (without the table all three blocks
    /// pack onto page 0; with it, the middle block opens page 1).
    #[test]
    fn taffy_driven_dispatch_matches_direct_walk() {
        let html = r#"
            <html><body>
              <div style="height: 200px"></div>
              <div style="break-before: page; height: 200px"></div>
              <div style="height: 200px"></div>
            </body></html>
        "#;

        let direct_geom = {
            let mut doc = parse(html, 600.0);
            let table = blitz_adapter::extract_column_style_table(&doc);
            super::run_pass_with_break_styles(doc.deref_mut(), 800.0_f32.as_px(), &table)
        };

        let taffy_geom = {
            let mut doc = parse(html, 600.0);
            let table = blitz_adapter::extract_column_style_table(&doc);
            let mut tree = PaginationLayoutTree::new(doc.deref_mut(), 800.0);
            tree.column_styles = Some(&table);
            tree.drive_taffy_root_layout();
            tree.take_geometry()
        };

        // Sanity: the break-* branch actually fired — page_index 1
        // appears at least once in the direct geometry.
        assert!(
            direct_geom
                .values()
                .flat_map(|g| g.fragments.iter())
                .any(|f| f.page_index == 1),
            "expected break-before: page to push a child onto page 1, got {direct_geom:?}"
        );

        assert_eq!(direct_geom.len(), taffy_geom.len());
        for (id, direct) in &direct_geom {
            let taffy = taffy_geom.get(id).expect("same node id in both passes");
            assert_eq!(direct.fragments, taffy.fragments, "node {id}");
        }
    }

    /// fulgur-kv0r: parallel siblings in a grid / flex parent should
    /// share the same page-local y when they share Taffy's
    /// `layout.location.y`. Pre-fix, `fragment_block_subtree`
    /// advanced `cursor_y` after each child via `cursor_y += child_h`,
    /// so card 2 (Taffy y=0) was recorded at y=200 (= card 1's
    /// height) in geometry. Post-fix the loop reads
    /// `child_page_y = page_start_y + (this_top_in_parent - page_taffy_origin)`
    /// directly from Taffy, and updates `cursor_y` only as a row's
    /// max bottom for break / overflow checks.
    #[test]
    fn fragment_block_subtree_grid_parallel_siblings_share_page_y() {
        // Two cells in a 2-column grid row, each 100px tall and
        // 100px wide so the grid container distinguishes them by x.
        // Pre-fix: card 2 placed at y=100 (cursor-advanced after
        // card 1). Post-fix: card 2 placed at y=0 (Taffy `location.y`).
        let html = r#"
            <html><body style="margin: 0; padding: 0">
              <div style="display: grid; grid-template-columns: 100px 100px; width: 200px;">
                <div style="height: 100px; width: 100px"></div>
                <div style="height: 100px; width: 100px"></div>
              </div>
            </body></html>
        "#;
        let mut doc = parse(html, 600.0);
        let table = blitz_adapter::extract_column_style_table(&doc);
        let geom = super::run_pass_with_break_styles(doc.deref_mut(), 800.0_f32.as_px(), &table);

        // Filter to fragments whose width is exactly the cell width
        // (100) — that's only the two cards. Grid container has
        // width 200 (two columns), html / body have viewport width.
        let card_y: Vec<f32> = geom
            .values()
            .flat_map(|g| g.fragments.iter())
            .filter(|f| {
                f.page_index == 0
                    && (f.height.to_f32() - 100.0).abs() < 0.5
                    && (f.width.to_f32() - 100.0).abs() < 0.5
            })
            .map(|f| f.y.to_f32())
            .collect();
        assert_eq!(
            card_y.len(),
            2,
            "expected two grid cells (100×100) on page 0, got {card_y:?}"
        );
        for y in &card_y {
            assert!(
                y.abs() < 0.5,
                "grid parallel siblings must share y=0, got {y} (pre-fix: card 2 at y=100 due \
                 to cursor-advance)",
            );
        }
    }

    #[test]
    fn fragment_block_subtree_following_block_continues_after_split_child_tail() {
        let html = r#"
            <html><body style="margin: 0; padding: 0">
              <section style="width: 220px;">
                <div style="height: 100px; width: 200px"></div>
                <div style="display: grid; grid-template-columns: 100px 100px; width: 200px;">
                  <div style="height: 100px; width: 100px"></div>
                  <div style="height: 100px; width: 100px"></div>
                  <div style="height: 100px; width: 100px"></div>
                  <div style="height: 100px; width: 100px"></div>
                </div>
                <h2 style="height: 30px; width: 200px; margin: 0">after grid</h2>
              </section>
            </body></html>
        "#;
        let mut doc = parse(html, 600.0);
        let table = blitz_adapter::extract_column_style_table(&doc);
        let geom = super::run_pass_with_break_styles(doc.deref_mut(), 250.0_f32.as_px(), &table);

        let mut candidates: Vec<_> = geom
            .values()
            .flat_map(|g| g.fragments.iter())
            .filter(|f| f.page_index == 1 && (f.height.to_f32() - 30.0).abs() < 0.5)
            .collect();
        candidates.sort_by(|a, b| a.y.partial_cmp(&b.y).unwrap());
        let h2 = candidates
            .first()
            .expect("expected the trailing h2 to land on page 1");
        let h2_y = h2.y.to_f32();
        assert!(
            h2_y >= 100.0,
            "block sibling after split grid must continue after the grid tail; got y={h2_y} (pre-fix: y=0 overlaps the tail)",
        );
    }

    /// fulgur-2m6w: a nested child with a non-finite Taffy height must not
    /// poison `cursor_y` inside the recursive splitter. The grid pattern
    /// routes the section through `fragment_block_subtree` (250px content vs
    /// 150px strip); a `+inf` height injected into one cell is sanitized to
    /// zero so the recursion neither panics nor produces a non-finite /
    /// unbounded page count, and the node is still recorded in geometry.
    #[test]
    fn fragment_block_subtree_non_finite_child_height_is_sanitized() {
        let html = r#"
            <html><body style="margin: 0; padding: 0">
              <section style="width: 220px;">
                <div style="display: grid; grid-template-columns: 100px 100px; width: 200px;">
                  <div id="bad" style="height: 100px; width: 100px"></div>
                  <div style="height: 100px; width: 100px"></div>
                  <div style="height: 100px; width: 100px"></div>
                  <div style="height: 100px; width: 100px"></div>
                </div>
              </section>
            </body></html>
        "#;
        let mut doc = parse(html, 600.0);
        let bad = find_by_id(doc.deref_mut(), "bad").expect("div#bad");
        doc.deref_mut()
            .get_node_mut(bad)
            .expect("div#bad")
            .final_layout
            .size
            .height = f32::INFINITY;
        let table = blitz_adapter::extract_column_style_table(&doc);
        let geom = super::run_pass_with_break_styles(doc.deref_mut(), 150.0_f32.as_px(), &table);
        // Load-bearing assertion: without the guard the injected `+inf`
        // reaches a Fragment height and poisons `cursor_y`, so emitted
        // fragments carry non-finite `y` / `height`. The guard zeroes it,
        // keeping every emitted coordinate finite.
        for (id, g) in geom.iter() {
            for f in &g.fragments {
                let fy = f.y.to_f32();
                let fh = f.height.to_f32();
                assert!(
                    fy.is_finite() && fh.is_finite(),
                    "node {id}: non-finite fragment y={fy} height={fh} leaked through \
                     fragment_block_subtree",
                );
            }
        }
        assert!(
            geom.contains_key(&bad),
            "nested non-finite-height node must still be recorded",
        );
    }

    #[test]
    fn fragment_block_subtree_following_block_continues_after_split_flex_tail() {
        let html = r#"
            <html><body style="margin: 0; padding: 0">
              <section style="width: 220px;">
                <div style="height: 100px; width: 200px"></div>
                <div style="display: flex; flex-wrap: wrap; width: 200px;">
                  <div style="height: 100px; width: 100px"></div>
                  <div style="height: 100px; width: 100px"></div>
                  <div style="height: 100px; width: 100px"></div>
                  <div style="height: 100px; width: 100px"></div>
                </div>
                <h2 style="height: 30px; width: 200px; margin: 0">after flex</h2>
              </section>
            </body></html>
        "#;
        let mut doc = parse(html, 600.0);
        let table = blitz_adapter::extract_column_style_table(&doc);
        let geom = super::run_pass_with_break_styles(doc.deref_mut(), 250.0_f32.as_px(), &table);

        let mut candidates: Vec<_> = geom
            .values()
            .flat_map(|g| g.fragments.iter())
            .filter(|f| f.page_index == 1 && (f.height.to_f32() - 30.0).abs() < 0.5)
            .collect();
        candidates.sort_by(|a, b| a.y.partial_cmp(&b.y).unwrap());
        let h2 = candidates
            .first()
            .expect("expected the trailing h2 to land on page 1");
        let h2_y = h2.y.to_f32();
        assert!(
            h2_y >= 100.0,
            "block sibling after split flex must continue after the flex tail; got y={h2_y} (pre-fix: y=0 overlaps the tail)",
        );
    }

    #[test]
    fn fragment_block_subtree_grid_later_row_parallel_siblings_share_page_y() {
        let html = r#"
            <html><body style="margin: 0; padding: 0">
              <section style="width: 220px;">
                <div style="height: 100px; width: 200px"></div>
                <div style="display: grid; grid-template-columns: 100px 100px; width: 200px;">
                  <div style="height: 100px; width: 100px"></div>
                  <div style="height: 100px; width: 100px"></div>
                  <div style="height: 100px; width: 100px"></div>
                  <div style="height: 100px; width: 100px"></div>
                </div>
              </section>
            </body></html>
        "#;
        let mut doc = parse(html, 600.0);
        let table = blitz_adapter::extract_column_style_table(&doc);
        let geom = super::run_pass_with_break_styles(doc.deref_mut(), 250.0_f32.as_px(), &table);

        let mut cells: Vec<_> = geom
            .values()
            .flat_map(|g| g.fragments.iter())
            .filter(|f| {
                (f.height.to_f32() - 100.0).abs() < 0.5 && (f.width.to_f32() - 100.0).abs() < 0.5
            })
            .map(|f| (f.page_index, f.x.to_f32(), f.y.to_f32()))
            .collect();
        cells.sort_by(|a, b| a.partial_cmp(b).unwrap());
        let page_one_cells: Vec<_> = cells.iter().filter(|(p, _, _)| *p == 1).collect();
        assert_eq!(
            page_one_cells.len(),
            2,
            "expected the second grid row's two cells on page 1, got {cells:?}"
        );
        assert!(
            (page_one_cells[0].2 - page_one_cells[1].2).abs() < 0.5,
            "parallel cells in the same later grid row must share y; got {cells:?}"
        );
    }

    #[test]
    fn fragment_block_subtree_flex_later_row_parallel_siblings_share_page_y() {
        let html = r#"
            <html><body style="margin: 0; padding: 0">
              <section style="width: 220px;">
                <div style="height: 100px; width: 200px"></div>
                <div style="display: flex; flex-wrap: wrap; width: 200px;">
                  <div style="height: 100px; width: 100px"></div>
                  <div style="height: 100px; width: 100px"></div>
                  <div style="height: 100px; width: 100px"></div>
                  <div style="height: 100px; width: 100px"></div>
                </div>
              </section>
            </body></html>
        "#;
        let mut doc = parse(html, 600.0);
        let table = blitz_adapter::extract_column_style_table(&doc);
        let geom = super::run_pass_with_break_styles(doc.deref_mut(), 250.0_f32.as_px(), &table);

        let mut cells: Vec<_> = geom
            .values()
            .flat_map(|g| g.fragments.iter())
            .filter(|f| {
                (f.height.to_f32() - 100.0).abs() < 0.5 && (f.width.to_f32() - 100.0).abs() < 0.5
            })
            .map(|f| (f.page_index, f.x.to_f32(), f.y.to_f32()))
            .collect();
        cells.sort_by(|a, b| a.partial_cmp(b).unwrap());
        let page_one_cells: Vec<_> = cells.iter().filter(|(p, _, _)| *p == 1).collect();
        assert_eq!(
            page_one_cells.len(),
            2,
            "expected the second flex row's two cells on page 1, got {cells:?}"
        );
        assert!(
            (page_one_cells[0].2 - page_one_cells[1].2).abs() < 0.5,
            "parallel cells in the same later flex row must share y; got {cells:?}"
        );
    }

    #[test]
    fn fragment_block_subtree_grid_later_row_preserves_parallel_sibling_offset() {
        let html = r#"
            <html><body style="margin: 0; padding: 0">
              <section style="width: 220px;">
                <div style="height: 100px; width: 200px"></div>
                <div style="display: grid; grid-template-columns: 100px 100px; align-items: start; width: 200px;">
                  <div style="height: 100px; width: 100px"></div>
                  <div style="height: 100px; width: 100px"></div>
                  <div style="height: 80px; width: 100px; margin-top: 20px"></div>
                  <div style="height: 100px; width: 100px"></div>
                </div>
              </section>
            </body></html>
        "#;
        let mut doc = parse(html, 600.0);
        let table = blitz_adapter::extract_column_style_table(&doc);
        let geom = super::run_pass_with_break_styles(doc.deref_mut(), 250.0_f32.as_px(), &table);

        let mut page_one_cells: Vec<_> = geom
            .values()
            .flat_map(|g| g.fragments.iter())
            .filter(|f| f.page_index == 1 && (f.width.to_f32() - 100.0).abs() < 0.5)
            .map(|f| (f.height.to_f32(), f.x.to_f32(), f.y.to_f32()))
            .collect();
        page_one_cells.sort_by(|a, b| a.partial_cmp(b).unwrap());
        let unshifted = page_one_cells
            .iter()
            .find(|(h, _, _)| (*h - 100.0).abs() < 0.5)
            .expect("expected unshifted second-row grid cell on page 1");
        assert!(
            (unshifted.2 + 20.0).abs() < 0.5,
            "same-row sibling must preserve its cross-axis offset relative to the split row; got {page_one_cells:?}"
        );
    }

    #[test]
    fn fragment_block_subtree_flex_later_row_preserves_parallel_sibling_offset() {
        let html = r#"
            <html><body style="margin: 0; padding: 0">
              <section style="width: 220px;">
                <div style="height: 100px; width: 200px"></div>
                <div style="display: flex; flex-wrap: wrap; align-items: flex-start; width: 200px;">
                  <div style="height: 100px; width: 100px"></div>
                  <div style="height: 100px; width: 100px"></div>
                  <div style="height: 80px; width: 100px; margin-top: 20px"></div>
                  <div style="height: 100px; width: 100px"></div>
                </div>
              </section>
            </body></html>
        "#;
        let mut doc = parse(html, 600.0);
        let table = blitz_adapter::extract_column_style_table(&doc);
        let geom = super::run_pass_with_break_styles(doc.deref_mut(), 250.0_f32.as_px(), &table);

        let mut page_one_cells: Vec<_> = geom
            .values()
            .flat_map(|g| g.fragments.iter())
            .filter(|f| f.page_index == 1 && (f.width.to_f32() - 100.0).abs() < 0.5)
            .map(|f| (f.height.to_f32(), f.x.to_f32(), f.y.to_f32()))
            .collect();
        page_one_cells.sort_by(|a, b| a.partial_cmp(b).unwrap());
        let unshifted = page_one_cells
            .iter()
            .find(|(h, _, _)| (*h - 100.0).abs() < 0.5)
            .expect("expected unshifted second-row flex item on page 1");
        assert!(
            (unshifted.2 + 20.0).abs() < 0.5,
            "same-row sibling must preserve its cross-axis offset relative to the split row; got {page_one_cells:?}"
        );
    }

    #[test]
    fn fragment_block_subtree_grid_row_recursive_cells_share_page_y() {
        // 2 cell の grid row で、両 cell が inner content を持ち
        // recursion 経路を通る (`needs_recursion=true`) case。両 cell
        // が page 内に収まるサイズなので page boundary は跨がない。
        //
        // pre-fix: 右 cell の recursion が parent の `cursor_y`
        // (= 左 cell の bottom = 200) を引きずり、右 column の inner
        // div が y=200 (block flow stacking) に積まれる。
        //
        // post-fix: 両 cell とも row top y=0 から開始し、各 inner div
        // が y=0..50, y=50..100 に並ぶ。
        let html = r#"
            <html><body style="margin: 0; padding: 0">
              <div style="display: grid; grid-template-columns: 100px 100px; width: 200px;">
                <div style="width: 100px">
                  <div style="height: 50px; width: 100px"></div>
                  <div style="height: 50px; width: 100px"></div>
                </div>
                <div style="width: 100px">
                  <div style="height: 50px; width: 100px"></div>
                  <div style="height: 50px; width: 100px"></div>
                </div>
              </div>
            </body></html>
        "#;
        let mut doc = parse(html, 600.0);
        let table = blitz_adapter::extract_column_style_table(&doc);
        let geom = super::run_pass_with_break_styles(doc.deref_mut(), 800.0_f32.as_px(), &table);

        // 100×50 の inner div fragment 4 個を集める。
        let mut inner: Vec<(u32, f32, f32)> = geom
            .values()
            .flat_map(|g| g.fragments.iter())
            .filter(|f| {
                (f.width.to_f32() - 100.0).abs() < 0.5 && (f.height.to_f32() - 50.0).abs() < 0.5
            })
            .map(|f| (f.page_index, f.x.to_f32(), f.y.to_f32()))
            .collect();
        inner.sort_by(|a, b| a.partial_cmp(b).unwrap());

        assert_eq!(
            inner.len(),
            4,
            "expected 4 inner divs (2 per column × 2 columns), got inner={inner:?}"
        );

        // すべて page 0 に収まる
        assert!(
            inner.iter().all(|(p, _, _)| *p == 0),
            "all 4 inner divs must be on page 0, inner={inner:?}"
        );

        // 左 column (x=0): y=0 と y=50 の 2 個
        let left_ys: Vec<f32> = inner
            .iter()
            .filter(|(_, x, _)| x.abs() < 0.5)
            .map(|(_, _, y)| *y)
            .collect();
        let approx_eq = |a: &[f32], b: &[f32]| -> bool {
            a.len() == b.len() && a.iter().zip(b).all(|(x, y)| (x - y).abs() < 0.5)
        };
        assert!(
            approx_eq(&left_ys, &[0.0, 50.0]),
            "left column y, inner={inner:?}"
        );

        // 右 column (x=100): y=0 と y=50 の 2 個 (block flow になっていれば
        // y=100, y=150 になる)
        let right_ys: Vec<f32> = inner
            .iter()
            .filter(|(_, x, _)| (x - 100.0).abs() < 0.5)
            .map(|(_, _, y)| *y)
            .collect();
        assert!(
            approx_eq(&right_ys, &[0.0, 50.0]),
            "right column y must match left column (parallel siblings, not stacked), inner={inner:?}"
        );
    }

    #[test]
    fn fragment_block_subtree_flex_row_recursive_cells_share_page_y() {
        let html = r#"
            <html><body style="margin: 0; padding: 0">
              <div style="display: flex; width: 200px;">
                <div style="width: 100px; flex: 0 0 100px">
                  <div style="height: 50px; width: 100px"></div>
                  <div style="height: 50px; width: 100px"></div>
                </div>
                <div style="width: 100px; flex: 0 0 100px">
                  <div style="height: 50px; width: 100px"></div>
                  <div style="height: 50px; width: 100px"></div>
                </div>
              </div>
            </body></html>
        "#;
        let mut doc = parse(html, 600.0);
        let table = blitz_adapter::extract_column_style_table(&doc);
        let geom = super::run_pass_with_break_styles(doc.deref_mut(), 800.0_f32.as_px(), &table);

        let mut inner: Vec<(u32, f32, f32)> = geom
            .values()
            .flat_map(|g| g.fragments.iter())
            .filter(|f| {
                (f.width.to_f32() - 100.0).abs() < 0.5 && (f.height.to_f32() - 50.0).abs() < 0.5
            })
            .map(|f| (f.page_index, f.x.to_f32(), f.y.to_f32()))
            .collect();
        inner.sort_by(|a, b| a.partial_cmp(b).unwrap());

        assert_eq!(
            inner.len(),
            4,
            "expected 4 inner divs (2 per column × 2 columns), got inner={inner:?}"
        );
        assert!(
            inner.iter().all(|(p, _, _)| *p == 0),
            "all 4 inner divs must be on page 0, inner={inner:?}"
        );

        let left_ys: Vec<f32> = inner
            .iter()
            .filter(|(_, x, _)| x.abs() < 0.5)
            .map(|(_, _, y)| *y)
            .collect();
        let right_ys: Vec<f32> = inner
            .iter()
            .filter(|(_, x, _)| (x - 100.0).abs() < 0.5)
            .map(|(_, _, y)| *y)
            .collect();
        let approx_eq = |a: &[f32], b: &[f32]| -> bool {
            a.len() == b.len() && a.iter().zip(b).all(|(x, y)| (x - y).abs() < 0.5)
        };
        assert!(
            approx_eq(&left_ys, &[0.0, 50.0]),
            "left column y, inner={inner:?}"
        );
        assert!(
            approx_eq(&right_ys, &[0.0, 50.0]),
            "right column y must match left column (parallel siblings, not stacked), inner={inner:?}"
        );
    }

    /// fulgur-916y: a multicol container with a `column-span: all`
    /// child whose subtree exceeds one page must split across pages
    /// in the partition path. Pre-fix, the multicol gate
    /// (`!is_multicol`) blocked recursion, so the whole multicol
    /// container ended up as a single fragment regardless of
    /// overflow — fragmenter reported 1 page. Post-fix, the gate
    /// admits multicol containers that have a span:all child, so
    /// `fragment_block_subtree` recurses into the span subtree and
    /// splits it across pages via the regular block-flow logic.
    ///
    /// Pins `implied_page_count(geometry) >= 2` for the
    /// `multicol_span_all` integration fixture's HTML rendered with
    /// the fragmenter's strip height set small enough that the
    /// span:all section overflows page 0.
    #[test]
    fn fragment_pagination_root_recurses_into_multicol_with_span_all() {
        let mut long = String::new();
        for _ in 0..40 {
            long.push_str(
                "<p>Lorem ipsum dolor sit amet, consectetur adipiscing elit. \
                 Sed do eiusmod tempor incididunt ut labore et dolore magna \
                 aliqua. Ut enim ad minim veniam, quis nostrud exercitation.</p>",
            );
        }
        let html = format!(
            r#"<!doctype html><html><head><style>
                body {{ margin: 10pt; font-size: 10pt; }}
                .mc {{ column-count: 2; column-gap: 10pt; }}
                .span {{ column-span: all; }}
            </style></head><body>
              <div class="mc">
                <p>before column content.</p>
                <section class="span">{long}</section>
                <p>after column content.</p>
              </div>
            </body></html>"#,
            long = long
        );

        // 600 viewport, 400 page strip (small enough to overflow).
        let mut doc = parse(&html, 600.0);
        let table = blitz_adapter::extract_column_style_table(&doc);
        let geom = super::run_pass_with_break_styles(doc.deref_mut(), 400.0_f32.as_px(), &table);
        let pages = super::implied_page_count(&geom);
        assert!(
            pages >= 2,
            "expected multicol with span:all overflow to split into >=2 pages, got {pages}",
        );
    }

    /// Devin Review on PR #285 (fulgur-a36m Phase 3.1.5b):
    /// `fragment_block_subtree` had `break-before: page` firing BEFORE
    /// the inter-child gap was folded into `cursor_y`, so the gap was
    /// re-applied AFTER the break-before reset — placing the child at
    /// `y=gap` on the new page instead of `y=0`. The body-level
    /// `fragment_pagination_root` had the correct ordering. This test
    /// pins B's y-coordinate on the new page and would catch the
    /// pre-fix value (gap≈20, was 26.6 in CSS px after Stylo's pt→px).
    ///
    /// Setup: outer wrapper triggers recursion via
    /// `has_forced_break_below`. Inside, A (h=100) at y=0 and B
    /// (h=100) at y=120 with `break-before: page`. The `margin-top:
    /// 20px` on B creates a 20px gap that the bug would leak through.
    #[test]
    fn fragment_block_subtree_break_before_after_gap_places_child_at_y_zero() {
        let html = r#"
            <html><body style="margin: 0; padding: 0">
              <div id="outer" style="margin: 0; padding: 0">
                <div id="a" style="height: 100px; margin: 0"></div>
                <div id="b" style="margin-top: 20px; break-before: page; height: 100px"></div>
              </div>
            </body></html>
        "#;
        let mut doc = parse(html, 600.0);
        let table = blitz_adapter::extract_column_style_table(&doc);
        let geom = super::run_pass_with_break_styles(doc.deref_mut(), 800.0_f32.as_px(), &table);

        // Find every fragment with height ≈ 100 on page 1; B is the
        // only such fragment (outer's page-1 fragment height is the
        // total parent strip, which equals 100 after the fix because
        // only B sits on page 1; outer's page-0 fragment carries
        // A + gap = 120; A is on page 0).
        let b_on_page1: Vec<&Fragment> = geom
            .values()
            .flat_map(|g| g.fragments.iter())
            .filter(|f| f.page_index == 1 && (f.height.to_f32() - 100.0).abs() < 0.5)
            .collect();
        assert!(
            !b_on_page1.is_empty(),
            "expected B fragment on page 1, geom={geom:?}"
        );
        for f in &b_on_page1 {
            let fy = f.y.to_f32();
            assert!(
                fy.abs() < 0.5,
                "B should land at y=0 on the new page (forced break discards \
                 the inter-child gap), but got y={fy} (gap leaked through \
                 break-before — see Devin Review on PR #285). frag={f:?}",
            );
        }
    }

    #[test]
    fn body_level_break_before_preserves_own_top_margin_on_new_page() {
        let html = r#"
            <html><body style="margin: 0; padding: 0">
              <div style="height: 40px; margin: 0"></div>
              <div style="height: 90px; margin-top: 10px; break-before: page"></div>
            </body></html>
        "#;
        let mut doc = parse(html, 600.0);
        let table = blitz_adapter::extract_column_style_table(&doc);
        let geom = super::run_pass_with_break_styles(doc.deref_mut(), 100.0_f32.as_px(), &table);

        let second_on_page1: Vec<&Fragment> = geom
            .values()
            .flat_map(|g| g.fragments.iter())
            .filter(|f| f.page_index == 1 && (f.height.to_f32() - 90.0).abs() < 0.5)
            .collect();
        assert_eq!(
            second_on_page1.len(),
            1,
            "expected only the second child on page 1, geom={geom:?}"
        );
        assert!(
            (second_on_page1[0].y.to_f32() - 10.0).abs() < 0.5,
            "body-level break-before should keep the element's own top margin on the new page; geom={geom:?}"
        );
    }

    #[test]
    fn implied_page_count_is_one_for_empty_geometry() {
        let geom = PaginationGeometryTable::new();
        assert_eq!(super::implied_page_count(&geom), 1);
    }

    #[test]
    fn implied_page_count_uses_max_index_plus_one() {
        let mut geom = PaginationGeometryTable::new();
        geom.entry(1).or_default().fragments.push(Fragment {
            page_index: 2,
            x: 0.0_f32.as_px(),
            y: 0.0_f32.as_px(),
            width: 1.0_f32.as_px(),
            height: 1.0_f32.as_px(),
        });
        assert_eq!(super::implied_page_count(&geom), 3);
    }

    /// fulgur-s67g Phase 2.1: a 3-line paragraph that overflows the
    /// page strip after line 2 cannot split between line 2 and the
    /// final line — the second fragment would have only 1 line, below
    /// the widows = 2 minimum. Spike emits the paragraph whole.
    #[test]
    fn widow_minimum_blocks_single_line_tail_fragment() {
        let mut geom = PaginationGeometryTable::new();
        // Each line 75px; cumulative bottoms at 75, 150, 225.
        // Page strip = 200, so naturally we'd split at line 2 (bottom
        // 225 > 200), leaving 1 line in the tail — widow violated.
        let lines = vec![(0.0, 75.0), (75.0, 150.0), (150.0, 225.0)];
        let (new_page, new_cursor, emitted, _) = super::fragment_inline_root(
            &mut geom,
            /*child_id=*/ 1,
            /*paragraph_x=*/ 0.0,
            /*width=*/ 100.0,
            /*initial_cursor_y=*/ 0.0,
            /*initial_page_index=*/ 0,
            /*strips=*/ PageStrips::new(200.0, 0.0),
            &lines,
        );
        assert_eq!(emitted, 1, "widow violation → single oversized fragment");
        assert_eq!(new_page, 0);
        assert!(
            (new_cursor - 225.0).abs() < 0.01,
            "cursor advances by full paragraph height, got {new_cursor}",
        );
        let frags = &geom.get(&1).unwrap().fragments;
        assert_eq!(frags.len(), 1);
        assert_eq!(frags[0].page_index, 0);
    }

    /// fulgur-s67g Phase 2.1: a 4-line paragraph splittable at line 2
    /// (first 2 lines on page 0, last 2 on page 1) honours both
    /// orphans = 2 and widows = 2.
    #[test]
    fn widow_orphan_minimum_allows_balanced_split() {
        let mut geom = PaginationGeometryTable::new();
        // Each line 75px; bottoms at 75, 150, 225, 300.
        // Page strip = 200 → natural split at line 2 (bottom 225 > 200).
        // first_size = 2 ≥ orphans, remaining_size = 2 ≥ widows. Split OK.
        let lines = vec![(0.0, 75.0), (75.0, 150.0), (150.0, 225.0), (225.0, 300.0)];
        let (new_page, new_cursor, emitted, _) = super::fragment_inline_root(
            &mut geom,
            1,
            0.0,
            100.0,
            0.0,
            0,
            PageStrips::new(200.0, 0.0),
            &lines,
        );
        assert_eq!(emitted, 2, "valid split → 2 fragments");
        assert_eq!(new_page, 1);
        let frags = &geom.get(&1).unwrap().fragments;
        assert_eq!(frags.len(), 2);
        assert_eq!(frags[0].page_index, 0);
        assert_eq!(frags[1].page_index, 1);
        // First fragment: lines 0-1 (height = 150).
        assert!((frags[0].height.to_f32() - 150.0).abs() < 0.01);
        // Second fragment: lines 2-3 (height = 150 in para-local).
        assert!((frags[1].height.to_f32() - 150.0).abs() < 0.01);
        // cursor_y on page 1 = paragraph_top_in_body (0.0) + 150 = 150.
        assert!((new_cursor - 150.0).abs() < 0.01);
    }

    /// fulgur-s67g Phase 2.1: orphan violation. A 3-line paragraph
    /// with overflow at line 1 (only line 0 fits) would put just 1
    /// line in the first fragment — below orphans = 2. No split; emit
    /// whole.
    #[test]
    fn orphan_minimum_blocks_single_line_head_fragment() {
        let mut geom = PaginationGeometryTable::new();
        // Lines 75px; bottoms at 75, 150, 225.
        // Page strip = 100 → natural split at line 1 (bottom 150 > 100).
        // first_size = 1 < orphans=2. Don't split.
        let lines = vec![(0.0, 75.0), (75.0, 150.0), (150.0, 225.0)];
        let (new_page, _new_cursor, emitted, _) = super::fragment_inline_root(
            &mut geom,
            1,
            0.0,
            100.0,
            0.0,
            0,
            PageStrips::new(100.0, 0.0),
            &lines,
        );
        assert_eq!(emitted, 1, "orphan violation → single oversized fragment");
        assert_eq!(new_page, 0);
        let frags = &geom.get(&1).unwrap().fragments;
        assert_eq!(frags.len(), 1);
        assert_eq!(frags[0].page_index, 0);
        assert!((frags[0].height.to_f32() - 225.0).abs() < 0.01);
    }

    /// Line boxes of 20px that advance by 19.2px (a 16px font at `line-height: normal` = 1.2, quantised by
    /// Blitz): `(top, bottom)` of line `i`, as `collect_inline_line_metrics` reports them.
    fn overlapping_boxes(n: usize) -> Vec<(f32, f32)> {
        (0..n)
            .map(|i| {
                let top = (i as f32 * 19.2).round() - 1.0;
                (top, top + 20.0)
            })
            .collect()
    }

    /// F78: a body-level paragraph records the line range and offset of every fragment.
    #[test]
    fn fragment_inline_root_records_one_tiling_slice_per_fragment() {
        let lines = overlapping_boxes(3000);
        let mut geom = PaginationGeometryTable::new();
        let (last_page, _, emitted, _) = super::fragment_inline_root(
            &mut geom,
            1,
            0.0,
            100.0,
            0.0,
            0,
            PageStrips::new(1000.0, 0.0),
            &lines,
        );
        assert!(emitted > 50, "about 60 pages, got {emitted}");
        assert_eq!(last_page as usize, emitted - 1);
        let entry = geom.get(&1).unwrap();
        assert_eq!(entry.line_slices.len(), entry.fragments.len());
        assert!(super::line_slices_tile(&entry.line_slices, lines.len()));
        for (s, f) in entry.line_slices.iter().zip(&entry.fragments) {
            let (first, end) = (s.first as usize, s.end as usize);
            assert_eq!(
                s.offset.to_f32(),
                lines[first].0 - lines[0].0,
                "offset is the fragmenter's own distance from the first line's top: {s:?}"
            );
            assert_eq!(
                f.height.to_f32(),
                lines[end - 1].1 - lines[first].0,
                "the fragment holds exactly the lines of its slice: {f:?} {s:?}"
            );
        }
        assert_eq!(entry.line_slices[0].offset.to_f32(), 0.0);
    }

    // ── F83: `break-inside: avoid` on an inline root taller than a page ─────

    /// `n` lines of 20 px, one per `<br>`.
    fn lines_20px(n: usize) -> String {
        (0..n)
            .map(|i| format!("line{i}"))
            .collect::<Vec<_>>()
            .join("<br>")
    }

    /// Fragment every node of `body` on 200 px pages (document margin 0, 20 px lines, `.av` is `break-inside:
    /// avoid`) and return the document with the geometry.
    fn avoid_fragments(body: &str) -> (blitz_html::HtmlDocument, PaginationGeometryTable) {
        let html = format!(
            "<!doctype html><html><head><style>body{{margin:0;font-size:16px;line-height:20px}}\
             p{{margin:0}}.av{{break-inside:avoid}}</style></head><body>{body}</body></html>"
        );
        let mut doc = parse(&html, 600.0);
        let table = blitz_adapter::extract_column_style_table(&doc);
        let geom = run_pass_with_break_styles(doc.deref_mut(), 200.0_f32.as_px(), &table);
        (doc, geom)
    }

    /// `(page, y, height)` of every fragment and `(first, end, offset)` of every slice.
    type Shape = (Vec<(u32, f32, f32)>, Vec<(u32, u32, f32)>);

    /// What one node's geometry holds, for comparing two runs: page, y, height of every fragment and the line range
    /// and offset of every slice.
    fn shape_of(g: &PaginationGeometry) -> Shape {
        (
            g.fragments
                .iter()
                .map(|f| (f.page_index, f.y.to_f32(), f.height.to_f32()))
                .collect(),
            g.line_slices
                .iter()
                .map(|s| (s.first, s.end, s.offset.to_f32()))
                .collect(),
        )
    }

    /// F83: a body-level `break-inside: avoid` paragraph of 800 px on 200 px pages is split between its line boxes
    /// exactly like the same paragraph without the property (css-break-3 §4.4: `avoid` is relaxed when the box cannot
    /// fit a fragmentainer). It used to be cut into strips by height, with no line ranges.
    #[test]
    fn avoid_paragraph_taller_than_a_page_splits_like_an_auto_one() {
        let text = lines_20px(40);
        let (avoid_doc, avoid) = avoid_fragments(&format!("<p class=\"av\">{text}</p>"));
        let (auto_doc, auto) = avoid_fragments(&format!("<p class=\"x\">{text}</p>"));
        let a = &avoid[&node_by_class(&avoid_doc, "av")];
        let b = &auto[&node_by_class(&auto_doc, "x")];
        assert!(a.fragments.len() >= 4, "{:?}", a.fragments);
        assert_eq!(
            a.line_slices.len(),
            a.fragments.len(),
            "one slice per fragment"
        );
        assert!(line_slices_tile(&a.line_slices, 40), "{:?}", a.line_slices);
        assert_eq!(shape_of(a), shape_of(b), "the same fragments and slices");
    }

    /// An inline root that is not a `<p>` (direct text of a `div`) takes the same path.
    #[test]
    fn avoid_div_with_direct_text_taller_than_a_page_splits_like_an_auto_one() {
        let text = lines_20px(40);
        let (avoid_doc, avoid) = avoid_fragments(&format!("<div class=\"av\">{text}</div>"));
        let (auto_doc, auto) = avoid_fragments(&format!("<div class=\"x\">{text}</div>"));
        let a = &avoid[&node_by_class(&avoid_doc, "av")];
        let b = &auto[&node_by_class(&auto_doc, "x")];
        assert_eq!(a.line_slices.len(), a.fragments.len());
        assert!(line_slices_tile(&a.line_slices, 40));
        assert_eq!(shape_of(a), shape_of(b));
    }

    /// F83c: an avoid box taller than a page only through `height` (its three lines fit a page) keeps the path it
    /// had: `avoid` holds, the strip slicer cuts the box frame over the pages and records no line slices.
    #[test]
    fn avoid_box_taller_than_a_page_only_by_height_keeps_the_strip_path() {
        let (doc, geom) = avoid_fragments(&format!(
            "<div class=\"av\" style=\"height:500px\">{}</div>",
            lines_20px(3)
        ));
        let g = &geom[&node_by_class(&doc, "av")];
        assert_eq!(g.fragments.len(), 3, "{:?}", g.fragments);
        assert!(g.line_slices.is_empty(), "{:?}", g.line_slices);
        let (doc, geom) = avoid_fragments(&format!(
            "<p>before</p><div class=\"av\" style=\"height:500px\">{}</div>",
            lines_20px(3)
        ));
        let g = &geom[&node_by_class(&doc, "av")];
        assert_eq!(
            g.fragments[0].page_index, 1,
            "moved to the next page: {:?}",
            g.fragments
        );
        assert!(g.line_slices.is_empty());
    }

    /// An avoid paragraph that fits a page is untouched: it does not fit the 50 px left under the spacer, so it
    /// moves whole to the next page (page index 1) as one fragment, and records no slices.
    #[test]
    fn avoid_paragraph_that_fits_a_page_moves_whole() {
        let (doc, geom) = avoid_fragments(&format!(
            "<div style=\"height:150px\"></div><p class=\"av\">{}</p>",
            lines_20px(5)
        ));
        let g = &geom[&node_by_class(&doc, "av")];
        assert_eq!(g.fragments.len(), 1, "{:?}", g.fragments);
        assert_eq!(g.fragments[0].page_index, 1);
        assert_eq!(g.fragments[0].y.to_f32(), 0.0);
        assert!(g.line_slices.is_empty(), "{:?}", g.line_slices);
    }

    /// The nested paths already recorded slices (`nested_line_split` never read `break-inside`): `div > p{avoid}`
    /// and `div{avoid} > p`. Pinned so the body-level change does not touch them.
    #[test]
    fn avoid_paragraphs_in_a_div_keep_their_tiling_slices() {
        let text = lines_20px(40);
        for body in [
            format!("<div><p class=\"av\">{text}</p></div>"),
            format!("<div class=\"av\"><p class=\"t\">{text}</p></div>"),
        ] {
            let (doc, geom) = avoid_fragments(&body);
            let p = &geom[&if body.contains("class=\"t\"") {
                node_by_class(&doc, "t")
            } else {
                node_by_class(&doc, "av")
            }];
            assert!(p.fragments.len() >= 4, "{body}: {:?}", p.fragments);
            assert_eq!(p.line_slices.len(), p.fragments.len(), "{body}");
            assert!(line_slices_tile(&p.line_slices, 40), "{body}");
        }
    }

    /// F83: `break-inside: avoid` holds while the box fits a page (1 px of slack, as the strip slicer has) and is
    /// relaxed past that.
    #[test]
    fn avoid_inside_holds_only_while_the_box_fits_a_page() {
        assert!(!avoid_inside_holds(false, 100.0, 1000.0), "auto");
        assert!(!avoid_inside_holds(false, 5000.0, 1000.0), "auto, tall");
        assert!(avoid_inside_holds(true, 100.0, 1000.0), "fits");
        assert!(avoid_inside_holds(true, 1000.0, 1000.0), "exactly a page");
        assert!(avoid_inside_holds(true, 1000.5, 1000.0), "within the slack");
        assert!(!avoid_inside_holds(true, 1001.5, 1000.0), "past the slack");
        assert!(!avoid_inside_holds(true, 7000.0, 1000.0), "taller");
    }

    // ── F83b: `break-inside: avoid` on a block wrapper ───────────────────────

    /// `count` paragraphs of `lines` lines of 20 px each.
    fn paragraphs_20px(count: usize, lines: usize) -> String {
        (0..count)
            .map(|_| format!("<p>{}</p>", lines_20px(lines)))
            .collect()
    }

    /// `(page, y)` of the first fragment of `class`.
    fn first_fragment(
        doc: &blitz_html::HtmlDocument,
        geom: &PaginationGeometryTable,
        class: &str,
    ) -> (u32, f32) {
        let f = &geom[&node_by_class(doc, class)].fragments[0];
        (f.page_index, f.y.to_f32())
    }

    /// A wrapper that does not fit the 50 px left under the spacer but fits a page moves whole to the next page
    /// (css-break-3 §4.5: the class A break point in front of it is preferred to one inside it). The gate used to
    /// enter it and split it between its paragraphs, because its first one fit.
    #[test]
    fn avoid_wrapper_that_fits_a_fresh_page_moves_whole() {
        let (doc, geom) = avoid_fragments(&format!(
            "<div style=\"height:150px\"></div><div class=\"av\">{}</div>",
            paragraphs_20px(3, 2)
        ));
        let g = &geom[&node_by_class(&doc, "av")];
        assert_eq!(g.fragments.len(), 1, "{:?}", g.fragments);
        assert_eq!(first_fragment(&doc, &geom, "av"), (1, 0.0));
    }

    /// A wrapper taller than a page is relaxed, but the break before it is still preferred: after content it starts the
    /// next page and breaks between its children from there, like an `auto` box.
    #[test]
    fn avoid_wrapper_taller_than_a_page_after_content_starts_on_the_next_page() {
        let (doc, geom) = avoid_fragments(&format!(
            "<p>before</p><div class=\"av\">{}</div>",
            paragraphs_20px(3, 4)
        ));
        assert_eq!(first_fragment(&doc, &geom, "av"), (1, 0.0));
    }

    /// The same inside a block (`fragment_block_subtree`).
    #[test]
    fn nested_avoid_wrapper_moves_whole() {
        let (doc, geom) = avoid_fragments(&format!(
            "<div class=\"o\"><div style=\"height:150px\"></div><div class=\"av\">{}</div></div>",
            paragraphs_20px(3, 2)
        ));
        let g = &geom[&node_by_class(&doc, "av")];
        assert_eq!(g.fragments.len(), 1, "{:?}", g.fragments);
        assert_eq!(first_fragment(&doc, &geom, "av"), (1, 0.0));
    }

    /// A relaxed avoid paragraph in a block starts a fresh page when something precedes it (Chrome: probe 2 of F83).
    #[test]
    fn nested_relaxed_avoid_paragraph_starts_a_page() {
        let (doc, geom) = avoid_fragments(&format!(
            "<div><p>before</p><p class=\"av\">{}</p></div>",
            lines_20px(40)
        ));
        assert_eq!(first_fragment(&doc, &geom, "av"), (1, 0.0));
        let g = &geom[&node_by_class(&doc, "av")];
        assert!(line_slices_tile(&g.line_slices, 40), "{:?}", g.line_slices);
    }

    /// A forced break inside an avoid box wins (css-break-3 §4.3); the piece before it is what has to be kept
    /// together, and it moves whole to the next page when it does not fit here.
    #[test]
    fn forced_break_piece_of_an_avoid_box_moves_whole() {
        let (doc, geom) = avoid_fragments(&format!(
            "<div style=\"height:150px\"></div><div class=\"av\">\
             <p class=\"a1\">{}</p><p>{}</p><p class=\"b\" style=\"break-before:page\">after</p></div>",
            lines_20px(2),
            lines_20px(2)
        ));
        assert_eq!(first_fragment(&doc, &geom, "a1"), (1, 0.0));
        assert_eq!(first_fragment(&doc, &geom, "b").0, 2);
    }

    /// Guard: first in the body there is no better break point, so a wrapper taller than a page is entered where it is.
    #[test]
    fn avoid_wrapper_first_on_its_page_is_entered() {
        let (doc, geom) = avoid_fragments(&format!(
            "<div class=\"av\">{}</div>",
            paragraphs_20px(3, 4)
        ));
        let g = &geom[&node_by_class(&doc, "av")];
        assert_eq!(first_fragment(&doc, &geom, "av"), (0, 0.0));
        assert!(g.fragments.len() >= 2, "{:?}", g.fragments);
    }

    /// Guard: page 0 starts 60 px down (the body's top margin), so its strip is 140 px. A wrapper of 160 px fits a fresh
    /// page and not that strip, but nothing precedes it, so it is entered where it is, not moved to leave page 0 blank
    /// (Chrome does not break at the start of a fragmentainer either).
    #[test]
    fn avoid_box_on_a_short_first_strip_is_split() {
        let html = format!(
            "<!doctype html><html><head><style>body{{margin:60px 0 0;font-size:16px;line-height:20px}}\
             p{{margin:0}}.av{{break-inside:avoid}}</style></head><body>\
             <div class=\"av\">{}</div></body></html>",
            paragraphs_20px(4, 2)
        );
        let mut doc = parse(&html, 600.0);
        let table = blitz_adapter::extract_column_style_table(&doc);
        let geom = run_pass_with_break_styles(doc.deref_mut(), 200.0_f32.as_px(), &table);
        let g = &geom[&node_by_class(&doc, "av")];
        assert_eq!(first_fragment(&doc, &geom, "av").0, 0);
        assert!(g.fragments.len() >= 2, "{:?}", g.fragments);
    }

    // ── F83b review (vrt-validator): H1, H2, H3, M1 ──────────────────────────

    /// H1: the padding, border and margin of the parent and the child's own margin are not content above the child.
    /// A kept-whole move would leave a page of nothing (a blank page 0) where Chrome splits the box in place: a box
    /// first on its page has no better break point (css-break-3 §4.1: a gap before the first child counts only
    /// between siblings; Chrome does not break at the start of a fragmentainer).
    #[test]
    fn padding_border_and_margin_above_an_avoid_child_are_not_content() {
        for parent_style in [
            "padding:8px",
            "border:4px solid red",
            "padding:8px;border:4px solid red",
        ] {
            // 10 lines of 20 px = 200 px: fits a page (200 px), not the 168 px left below the 32 px of margin and
            // padding.
            let (doc, geom) = avoid_fragments(&format!(
                "<div style=\"{parent_style}\"><div class=\"av\" style=\"margin-top:24px\">{}</div></div>",
                (0..5)
                    .map(|i| format!("<p class=\"p{i}\">{}</p>", lines_20px(2)))
                    .collect::<String>()
            ));
            let (page, y) = first_fragment(&doc, &geom, "p0");
            assert_eq!(
                page, 0,
                "{parent_style}: the first paragraph stays on page 0"
            );
            assert!(y > 20.0, "{parent_style}: below the inset, got {y}");
        }
    }

    /// H2: a forced break at the very start of an avoid box (its first child has `break-before: page`) is a break
    /// before the box: the piece after it is what has to be kept together, and it moves whole. The first line used to
    /// be left at the bottom of page 0, in the margin.
    #[test]
    fn forced_break_at_the_start_of_an_avoid_box_moves_the_box_whole() {
        let (doc, geom) = avoid_fragments(&format!(
            "<div style=\"height:190px\"></div><div class=\"av\">\
             <p class=\"a1\" style=\"break-before:page\">{}</p><p>{}</p></div>",
            lines_20px(2),
            lines_20px(2)
        ));
        assert_eq!(first_fragment(&doc, &geom, "a1"), (1, 0.0));
    }

    /// H3: a relaxed avoid box with padding that starts the next page has its first child placed there, not cut to
    /// a page of its own (which left page 1 holding only the box's top padding).
    #[test]
    fn relaxed_padded_avoid_box_moved_to_the_next_page_leaves_no_blank_page() {
        let (doc, geom) = avoid_fragments(
            "<div class=\"av\"><p>x</p></div>\
             <div class=\"av\" style=\"padding:8px\"><div class=\"sp\" style=\"height:195px\"></div></div>",
        );
        assert_eq!(
            first_fragment(&doc, &geom, "sp").0,
            1,
            "{:?}",
            geom[&node_by_class(&doc, "sp")].fragments
        );
    }

    /// H3: a nested relaxed box at the top of a page closes its frame inside the page: the last child moves on when
    /// the box's bottom padding and border would not fit below it (Chrome breaks before it). The frame used to cross
    /// the content bottom by the padding.
    #[test]
    fn nested_relaxed_box_at_a_page_top_closes_its_frame_inside_the_page() {
        let (doc, geom) = avoid_fragments(&format!(
            "<div><div style=\"height:20px\"></div><div class=\"av\" style=\"padding:15px;border:3px solid\">{}</div></div>",
            (0..9)
                .map(|i| format!("<div class=\"l\">line{i}</div>"))
                .collect::<String>()
        ));
        let lines = nodes_by_class(doc.deref(), "l");
        let last = &geom[&lines[8]].fragments[0];
        assert_eq!(
            last.page_index, 2,
            "the 9th line starts the next page: {last:?}"
        );
        for id in &lines[..8] {
            let f = &geom[id].fragments[0];
            assert!(
                f.page_index != 1 || f.y.to_f32() + f.height.to_f32() + 18.0 <= 200.01,
                "{f:?}"
            );
        }
    }

    /// M1: a forced break inside an avoid box that is taller than a page only because of that break: the first
    /// piece fits, it is kept together where it is (§4.3: the forced break wins, the pieces are what `avoid` measures).
    #[test]
    fn forced_break_inside_an_avoid_box_taller_than_a_page_keeps_the_first_piece_in_place() {
        let six = (0..6).map(|_| "<p>l</p>").collect::<String>();
        let (doc, geom) = avoid_fragments(&format!(
            "<p>before</p><div class=\"av\"><p class=\"a1\">first</p>{six}\
             <p class=\"b\" style=\"break-before:page\">second</p>{six}{six}</div>"
        ));
        // Pieces of 140 px and 260 px, 400 px together: taller than a page, but the first piece fits the 180 px
        // left below `before`, so it starts there.
        assert_eq!(first_fragment(&doc, &geom, "a1"), (0, 20.0));
        assert_eq!(first_fragment(&doc, &geom, "b").0, 1);
    }

    /// Harness (gen1 `A_k50_*_d2_p1`): a padded parent chain around an avoid box that fits a page only without the
    /// insets above it: the chain cannot be kept together, so it is not entered where it is. Counting the avoid box as
    /// a whole made the first-row estimate exceed a page, so F28 did not move the chain and its top inset stayed on
    /// page 0 as a stub (main moves it: the first row is a line).
    #[test]
    fn padded_parents_around_an_avoid_box_that_fills_a_page_leave_no_stub() {
        let (doc, geom) = avoid_fragments(&format!(
            "<div style=\"height:190px\"></div>\
             <div class=\"o\" style=\"padding:6px;border:2px solid red\">\
             <div style=\"padding:6px;border:2px solid red\"><div class=\"av\">{}</div></div></div>",
            (0..10).map(|_| "<div>l</div>").collect::<String>()
        ));
        assert_eq!(
            first_fragment(&doc, &geom, "o").0,
            1,
            "{:?}",
            geom[&node_by_class(&doc, "o")].fragments
        );
    }

    /// Harness (gen1 `A_k49_*_d2_p1`): an avoid box at a page top that fits the room but not with the bottom padding
    /// and border of its parents below it: the last line moves on (Chrome), the frames close inside the page.
    #[test]
    fn avoid_box_that_fits_without_its_parents_bottom_inset_is_split_at_a_page_top() {
        let (doc, geom) = avoid_fragments(&format!(
            "<div style=\"padding:6px;border:3px solid red\"><div style=\"padding:6px;border:3px solid red\">\
             <div class=\"av\">{}</div></div></div>",
            (0..9)
                .map(|i| format!("<div class=\"l\">l{i}</div>"))
                .collect::<String>()
        ));
        let lines = nodes_by_class(doc.deref(), "l");
        assert_eq!(geom[&lines[7]].fragments[0].page_index, 0);
        assert_eq!(
            geom[&lines[8]].fragments[0].page_index, 1,
            "{:?}",
            geom[&lines[8]].fragments
        );
    }

    /// Fuzz `Z1_59`: an avoid box a hair taller than a page (it holds, 1 px of slack) inside two plain wrappers after a
    /// spacer: every line keeps a fragment (words were lost) and none starts below the page bottom.
    #[test]
    fn avoid_box_a_hair_taller_than_a_page_in_wrappers_loses_no_line() {
        for spacer in [60, 140] {
            let (doc, geom) = avoid_fragments(&format!(
                "<div style=\"height:{spacer}px\"></div><div><div><div class=\"av\" style=\"padding:10.4px\">{}</div></div></div>",
                (0..9)
                    .map(|i| format!("<div class=\"l\">l{i}</div>"))
                    .collect::<String>()
            ));
            let lines = nodes_by_class(doc.deref(), "l");
            for (i, id) in lines.iter().enumerate() {
                let f = geom
                    .get(id)
                    .map(|g| g.fragments.clone())
                    .unwrap_or_default();
                assert!(!f.is_empty(), "spacer {spacer}: line {i} has no fragment");
                assert!(
                    f[0].y.to_f32() + f[0].height.to_f32() <= 200.5,
                    "spacer {spacer}: line {i} {f:?}"
                );
            }
        }
    }

    /// Fuzz `Z1_198`: a relaxed avoid box with its own `break-before: page` inside a padded parent after content: the
    /// forced break is the one break before it. Moving the parent first made the break fire again on the fresh page
    /// (the padding above the box counts as content there) and left a blank page.
    #[test]
    fn relaxed_avoid_box_with_a_forced_break_before_it_starts_one_page_only() {
        let (doc, geom) = avoid_fragments(&format!(
            "<p>x</p><div style=\"padding:12px\"><div class=\"av\" style=\"break-before:page\">{}</div></div>",
            (0..12)
                .map(|i| format!("<div class=\"l\">l{i}</div>"))
                .collect::<String>()
        ));
        let lines = nodes_by_class(doc.deref(), "l");
        assert_eq!(geom[&lines[0]].fragments[0].page_index, 1);
    }

    /// Fuzz `Z1_549`: a relaxed avoid box that starts the next page, whose parent (inside it) is padded and holds an
    /// atomic leaf that does not fit below the padding: the leaf stays where it is, it is not cut to another page that
    /// would hold only the frames (a blank page).
    #[test]
    fn leaf_in_a_padded_parent_inside_a_relaxed_avoid_box_is_not_cut_to_a_blank_page() {
        let (doc, geom) = avoid_fragments(
            "<p>x</p><div class=\"av\"><div style=\"padding:12px\"><div class=\"sp\" style=\"height:195px\"></div></div></div>",
        );
        assert_eq!(first_fragment(&doc, &geom, "sp").0, 1);
    }

    /// Fuzz `Z2_320`: padded parent > plain wrapper > avoid box that fits a page without the padding above it: nothing
    /// precedes the chain, so it starts on page 0 and the box breaks there like Chrome's (it used to be moved whole to
    /// page 1, leaving page 0 empty).
    #[test]
    fn avoid_box_in_a_plain_wrapper_in_a_padded_parent_stays_on_the_first_page() {
        let (doc, geom) = avoid_fragments(&format!(
            "<div style=\"padding:6px\"><div><div class=\"av\">{}</div></div></div>",
            (0..10)
                .map(|i| format!("<div class=\"l\">l{i}</div>"))
                .collect::<String>()
        ));
        let lines = nodes_by_class(doc.deref(), "l");
        assert_eq!(geom[&lines[0]].fragments[0].page_index, 0);
    }

    /// Fuzz `Z2_34`: a padded relaxed avoid box at the top of the document whose children fit the page but whose frame
    /// does not: the last line moves on and the frame closes with it (the strip slicer left the end of the frame on a
    /// page of its own, which stayed blank when the next relaxed box moved on).
    #[test]
    fn relaxed_padded_avoid_box_at_the_top_closes_its_frame_with_its_last_line() {
        let (doc, geom) = avoid_fragments(&format!(
            "<div class=\"av\" style=\"padding:12px\"><div>{}</div></div>",
            (0..9)
                .map(|i| format!("<div class=\"l\">l{i}</div>"))
                .collect::<String>()
        ));
        let lines = nodes_by_class(doc.deref(), "l");
        assert_eq!(geom[&lines[7]].fragments[0].page_index, 0);
        assert_eq!(geom[&lines[8]].fragments[0].page_index, 1);
    }

    /// Fuzz `Z1_234`: F28 moves a padded box to a fresh page because its first row does not fit; the forced break
    /// before its first child is the page boundary already and must not fire again (the padding above the child is not
    /// content), or a blank page is left.
    #[test]
    fn forced_break_before_the_first_child_of_a_moved_padded_box_does_not_fire_twice() {
        let (doc, geom) = avoid_fragments(
            "<div style=\"height:190px\"></div><div style=\"padding:12px\">\
             <div style=\"break-before:page\"><p class=\"a\">text</p></div></div>",
        );
        assert_eq!(first_fragment(&doc, &geom, "a").0, 1);
    }

    /// Harness gen6 `M_k50_*`: an avoid box that fits the room within the 1 px of slack, inside a padded plain parent
    /// at the top of the page, stays where it is (the strip cut used to move it by the sub-pixel it was over, leaving the
    /// page blank).
    #[test]
    fn avoid_box_within_the_slack_of_the_room_is_not_cut_away_from_the_page_top() {
        let (doc, geom) = avoid_fragments(&format!(
            "<div style=\"padding:0.8px\"><div class=\"av\">{}</div></div>",
            (0..10)
                .map(|i| format!("<div class=\"l\">l{i}</div>"))
                .collect::<String>()
        ));
        let lines = nodes_by_class(doc.deref(), "l");
        assert_eq!(geom[&lines[0]].fragments[0].page_index, 0);
    }

    /// Harness gen6 `M_k50_*pp10`: an avoid box that fits the room exactly but not together with its parent's bottom
    /// padding, at the top of the page: it is entered and its last line moves on (Chrome), instead of the box staying
    /// whole with the parent's padding crossing the page bottom.
    #[test]
    fn avoid_box_that_fills_the_room_but_not_its_parents_bottom_padding_is_split() {
        let (doc, geom) = avoid_fragments(&format!(
            "<div style=\"padding:10px\"><div class=\"av\">{}</div></div>",
            (0..10)
                .map(|i| format!("<div class=\"l\" style=\"height:19.1px\">l{i}</div>"))
                .collect::<String>()
        ));
        let lines = nodes_by_class(doc.deref(), "l");
        assert_eq!(geom[&lines[8]].fragments[0].page_index, 0);
        assert_eq!(geom[&lines[9]].fragments[0].page_index, 1);
    }

    /// Validator `r6` (plain document, no `avoid`): three nested parents with a bottom padding after a spacer, a one-line
    /// last child that fits but not with the padding below it. The chain is not entered for the padding (its last child
    /// cannot move: it is flush with the chain's top), so the cursor handed back is below the frames and the next
    /// sibling goes to the next page, as on main. Entering it left the cursor at the line and the sibling landed in
    /// the padding.
    #[test]
    fn plain_nested_bottom_padding_does_not_pull_the_next_sibling_up() {
        let (doc, geom) = avoid_fragments(
            "<div style=\"height:150px\"></div>\
             <div style=\"padding-bottom:20px\"><div style=\"padding-bottom:20px\"><div style=\"padding-bottom:20px\">\
             <p class=\"a\">x</p></div></div></div><p class=\"q\">y</p>",
        );
        assert_eq!(first_fragment(&doc, &geom, "a").0, 0);
        assert_eq!(first_fragment(&doc, &geom, "q").0, 1);
    }

    /// F83b: kept whole when `avoid` holds and the box fits the room or can move; the six combinations.
    #[test]
    fn avoid_keeps_whole_combinations() {
        // (avoid, extent, room, fresh, content_above)
        assert!(
            !avoid_keeps_whole(false, 100.0, 50.0, 200.0, true, 0.0),
            "auto"
        );
        assert!(
            avoid_keeps_whole(true, 100.0, 50.0, 200.0, true, 0.0),
            "does not fit, can move"
        );
        assert!(
            !avoid_keeps_whole(true, 100.0, 50.0, 200.0, false, 0.0),
            "does not fit, nothing above: entered as before"
        );
        assert!(
            avoid_keeps_whole(true, 40.0, 50.0, 200.0, false, 0.0),
            "fits here"
        );
        assert!(
            avoid_keeps_whole(true, 100.0, 99.5, 200.0, false, 0.0),
            "within the 1 px slack"
        );
        assert!(
            !avoid_keeps_whole(true, 300.0, 50.0, 200.0, true, 0.0),
            "taller than a page: relaxed"
        );
    }

    /// The document of `body` (the page style of [`avoid_fragments`]) with its break table.
    fn avoid_doc(
        body: &str,
    ) -> (
        blitz_html::HtmlDocument,
        crate::column_css::ColumnStyleTable,
    ) {
        let html = format!(
            "<!doctype html><html><head><style>body{{margin:0;font-size:16px;line-height:20px}}\
             p{{margin:0}}.av{{break-inside:avoid}}</style></head><body>{body}</body></html>"
        );
        let doc = parse(&html, 600.0);
        let table = blitz_adapter::extract_column_style_table(&doc);
        (doc, table)
    }

    /// F83b: the piece of an avoid box before its first forced break: its own height without one, the top of a
    /// descendant with `break-before: page`, the bottom of one with `break-after: page`, at any depth.
    #[test]
    fn avoid_piece_size_stops_at_the_first_forced_break() {
        let two = lines_20px(2);
        let piece = |inner: &str| {
            let (doc, table) = avoid_doc(&format!("<div class=\"av\">{inner}</div>"));
            avoid_piece_size(
                doc.deref(),
                node_by_class(doc.deref(), "av"),
                Some(&table),
                0,
            )
        };
        let p = |style: &str| format!("<p style=\"{style}\">{two}</p>");
        assert_eq!(piece(&format!("{}{}", p(""), p(""))), 80.0, "none");
        assert_eq!(
            piece(&format!("{}{}{}", p(""), p("break-before:page"), p(""))),
            40.0,
            "before the second paragraph"
        );
        assert_eq!(
            piece(&format!("{}{}{}", p(""), p("break-after:page"), p(""))),
            80.0,
            "after the second paragraph"
        );
        assert_eq!(
            piece(&format!(
                "{}<div>{}{}</div>{}",
                p(""),
                p(""),
                p("break-before:page"),
                p("")
            )),
            80.0,
            "nested: before the second paragraph of the inner div"
        );
        let (doc, _) = avoid_doc(&format!("<div class=\"av\">{}</div>", p("")));
        assert_eq!(
            avoid_piece_size(doc.deref(), node_by_class(doc.deref(), "av"), None, 0),
            40.0,
            "without a table"
        );
    }

    /// H2: a forced break at the very start of the box is skipped: the piece is the one after it.
    #[test]
    fn avoid_piece_size_skips_a_forced_break_at_the_start() {
        let two = lines_20px(2);
        let piece = |inner: &str| {
            let (doc, table) = avoid_doc(&format!("<div class=\"av\">{inner}</div>"));
            avoid_piece_size(
                doc.deref(),
                node_by_class(doc.deref(), "av"),
                Some(&table),
                0,
            )
        };
        let p = |style: &str| format!("<p style=\"{style}\">{two}</p>");
        assert_eq!(
            piece(&format!("{}{}", p("break-before:page"), p(""))),
            80.0,
            "only the leading break: the whole box"
        );
        assert_eq!(
            piece(&format!(
                "{}{}{}{}",
                p("break-before:page"),
                p(""),
                p("break-before:page"),
                p("")
            )),
            80.0,
            "the leading break and a second one"
        );
    }

    /// H3: only an `auto` height box has a content-sized frame.
    #[test]
    fn frame_is_content_sized_only_for_auto_height() {
        let sized = |style: &str| {
            let (doc, _) = avoid_doc(&format!(
                "<div class=\"b\" style=\"{style}\"><p>{}</p></div>",
                lines_20px(2)
            ));
            let n = node_by_class(doc.deref(), "b");
            frame_is_content_sized(doc.deref(), doc.get_node(n).unwrap())
        };
        assert!(sized(""));
        assert!(sized("padding:10px;border:3px solid"));
        assert!(!sized("height:300px"));
        assert!(!sized("min-height:300px"));
    }

    /// F83b: the walk of `starts_with_relaxed_avoid` follows the first in-flow child while no gap separates it from
    /// the content edge: padding does not stop it, a margin that did not collapse through does.
    #[test]
    fn starts_with_relaxed_avoid_follows_the_first_child_without_a_gap() {
        // 12 lines: 240 px, taller than the 200 px page.
        let tall = format!("<div class=\"av\"><p>{}</p></div>", lines_20px(12));
        let check = |outer_style: &str, inner: &str| {
            let (doc, table) = avoid_doc(&format!(
                "<div class=\"o\" style=\"{outer_style}\">{inner}</div>"
            ));
            starts_with_relaxed_avoid(
                doc.deref(),
                node_by_class(doc.deref(), "o"),
                Some(&table),
                200.0,
                0,
            )
        };
        assert!(check("", &tall), "first child");
        assert!(check("padding:10px", &tall), "padding is not a gap");
        assert!(
            check("border-top:2px solid red", &tall),
            "border is not a gap"
        );
        assert!(
            !check("", &format!("<p>x</p>{tall}")),
            "not the first child"
        );
        assert!(
            !check("display:flex", &tall),
            "flex items are not block flow"
        );
        let fits = format!("<div class=\"av\"><p>{}</p></div>", lines_20px(3));
        assert!(!check("", &fits), "avoid holds: not relaxed");
        // A gap between the content edge and the first child (a margin on the child inside a parent with padding
        // collapses to nothing, but a spacer is one) stops the walk.
        assert!(!check(
            "",
            &format!("<div style=\"height:5px\"></div>{tall}")
        ));
    }

    /// F83b: a parent whose first row is an avoid box that holds counts the whole box (to its first forced break), not
    /// the first row of it, so it is moved before it is entered; without the table it counts the first row as before.
    #[test]
    fn leading_block_size_counts_an_avoid_first_row_whole() {
        let (doc, table) = avoid_doc(&format!(
            "<div class=\"o\"><div class=\"av\">{}</div></div>",
            paragraphs_20px(3, 2)
        ));
        let o = node_by_class(doc.deref(), "o");
        assert_eq!(
            super::leading_block_size(doc.deref(), o, Some(&table), 0.0, 0, 200.0),
            120.0
        );
        assert_eq!(
            super::leading_block_size(doc.deref(), o, None, 0.0, 0, 200.0),
            40.0,
            "no table: the first row"
        );
    }

    /// F87: a `break-before` inside `@layer` (Tailwind v4's shape) reaches the fragmenter: the second paragraph
    /// starts page index 1. The same rule outside a layer gave the same result before; the layered one was dropped
    /// by the reader.
    #[test]
    fn break_before_inside_a_layer_starts_a_page() {
        for css in [
            "@layer u{.b{break-before:page}}",
            "@layer base,u;@layer u{.b{break-before:page}}@layer base{p{break-before:auto}}",
            ".b{break-before:page}",
        ] {
            let html = format!(
                "<!doctype html><html><head><style>body{{margin:0;font-size:16px;line-height:20px}}p{{margin:0}}{css}\
                 </style></head><body><p class=\"a\">one</p><p class=\"b\">two</p></body></html>"
            );
            let mut doc = parse(&html, 600.0);
            let table = blitz_adapter::extract_column_style_table(&doc);
            let geom = run_pass_with_break_styles(doc.deref_mut(), 200.0_f32.as_px(), &table);
            let page = |class: &str| geom[&node_by_class(&doc, class)].fragments[0].page_index;
            assert_eq!((page("a"), page("b")), (0, 1), "{css}");
        }
    }

    #[test]
    fn line_slices_tile_rejects_gaps_overlaps_and_short_cover() {
        let s = |first, end| super::LineSlice {
            first,
            end,
            offset: 0.0_f32.as_px(),
        };
        assert!(super::line_slices_tile(&[s(0, 3), s(3, 5)], 5));
        assert!(!super::line_slices_tile(&[s(0, 3), s(4, 5)], 5), "gap");
        assert!(!super::line_slices_tile(&[s(0, 3), s(2, 5)], 5), "overlap");
        assert!(!super::line_slices_tile(&[s(0, 3), s(3, 4)], 5), "short");
        assert!(!super::line_slices_tile(&[s(1, 5)], 5), "starts at 1");
        assert!(!super::line_slices_tile(&[s(0, 0), s(0, 5)], 5), "empty");
        assert!(!super::line_slices_tile(&[], 5), "nothing");
    }

    /// Node id of the element with `id="..."`.
    fn node_with_id(doc: &blitz_html::HtmlDocument, id: &str) -> usize {
        doc.tree()
            .iter()
            .find(|(_, n)| {
                n.element_data().is_some_and(|e| {
                    e.attrs()
                        .iter()
                        .any(|a| a.name.local.as_ref() == "id" && a.value == id)
                })
            })
            .map(|(i, _)| i)
            .unwrap_or_else(|| panic!("no #{id}"))
    }

    /// html-to-pdf F183 (plan 7.4): the first line of a `border-collapse: collapse` cell with a 1 px border sits 1 px
    /// below `border + padding`; that was read as a margin gap in front of the first child (a class C break point), so
    /// a row that did not fit the bottom of the page was entered there: a 9 px stub of the cell stayed on page 0, the
    /// image went to page 1 as a box of its own (60 px) in a cell fragment of 60 px, shorter than its 64 px line, and the
    /// paragraph painter dropped the line. Chrome moves the row whole.
    #[test]
    fn a_collapsed_cell_with_an_inline_image_moves_to_the_next_page_whole() {
        let font = Arc::new(
            std::fs::read(concat!(
                env!("CARGO_MANIFEST_DIR"),
                "/../../examples/.fonts/NotoSans-Regular.ttf"
            ))
            .expect("the bundled Noto Sans"),
        );
        for table_style in ["border-collapse:collapse", ""] {
            let html = format!(
                r#"<style>body{{margin:0;font:16px 'Noto Sans'}}td{{padding:8px;border:1px solid #000}}</style><body>
                <div style="height:1080px"></div><table style="{table_style}"><tr><td><img id="box" width="60" height="60"></td></tr></table></body>"#
            );
            let mut doc = blitz_adapter::parse(&html, 794.0, std::slice::from_ref(&font));
            blitz_adapter::resolve(&mut doc);
            let table = run_pass(&mut doc, 1122.52);
            let td = find_by_id(doc.deref(), "box")
                .and_then(|id| doc.get_node(id))
                .and_then(|n| n.parent)
                .expect("the cell");
            let pages: Vec<u32> = table[&td].fragments.iter().map(|f| f.page_index).collect();
            assert_eq!(pages, [1], "{table_style:?}: the cell is whole on page 1");
            let img = find_by_id(doc.deref(), "box").unwrap();
            assert_eq!(
                table[&img]
                    .fragments
                    .iter()
                    .map(|f| f.page_index)
                    .collect::<Vec<_>>(),
                [1],
                "{table_style:?}"
            );
        }
    }

    /// html-to-pdf F183 (plan 7.5): a table cell is a paragraph that may be split between its line boxes (F30b) whatever
    /// the border of its table is. The test "the paragraph is flush with the top of its box" (`nested_line_split`)
    /// measured from the border edge, so a cell of a table with a border (separated borders with `border: 3px`) or of a
    /// `border-collapse: collapse` table (the table holds half of its outer borders, Blitz puts the cells 1 px or more
    /// below its top) was never "first": it was not split between its lines but cut between its boxes by the block
    /// path, the second line landed in a fragment as tall as the image, and the paragraph painter dropped the 64 px
    /// line that did not fit it (the later lines vanished from page 2).
    ///
    /// Two lines (an image each) at the bottom of page 0: a cell on two pages has a line range for each fragment, and
    /// the images are where the last column says. The cell of a table with a frame is not flush with the top of the strip, so the cut
    /// that keeps the first two lines (orphans) together moves it to page 1 whole, as Chrome does; so does the plain
    /// table at the top of its strip since plan 7.7 (`leading_block_size` of an inline root counts its first two
    /// lines, not the bottom of its first image: it used to split 1 | 1).
    #[test]
    fn a_cell_of_a_bordered_table_is_split_between_its_lines() {
        let font = Arc::new(
            std::fs::read(concat!(
                env!("CARGO_MANIFEST_DIR"),
                "/../../examples/.fonts/NotoSans-Regular.ttf"
            ))
            .expect("the bundled Noto Sans"),
        );
        // (table style, td style, page of each image)
        let variants = [
            ("", "border:1px solid #000", [1, 1]),
            ("border-collapse:collapse", "border:1px solid #000", [1, 1]),
            ("border-collapse:collapse", "border:2px solid #000", [1, 1]),
            ("border-collapse:collapse", "border:3px solid #000", [1, 1]),
            (
                "border-collapse:collapse;border:4px solid red",
                "border:1px solid #000",
                [1, 1],
            ),
            ("border:3px solid red", "border:1px solid #000", [1, 1]),
            ("padding:4px", "border:1px solid #000", [1, 1]),
        ];
        for (table_style, td_style, image_pages) in variants {
            let html = format!(
                r#"<style>body{{margin:0;font:16px 'Noto Sans'}}td{{padding:5px;{td_style}}}</style><body>
                <div style="height:1000px"></div><table style="{table_style}"><tr><td id="td"><img id="a" width="60" height="60"><br><img id="b" width="60" height="60"></td></tr></table></body>"#
            );
            let mut doc = blitz_adapter::parse(&html, 794.0, std::slice::from_ref(&font));
            blitz_adapter::resolve(&mut doc);
            let table = run_pass(&mut doc, 1122.52);
            let label = format!("{table_style:?} / {td_style:?}");
            let td = &table[&find_by_id(doc.deref(), "td").unwrap()];
            assert!(
                td.fragments.len() < 2 || line_slices_tile(&td.line_slices, 2),
                "{label}: the cell is cut but not by line ranges: {td:?}"
            );
            for (id, page) in ["a", "b"].into_iter().zip(image_pages) {
                let img = &table[&find_by_id(doc.deref(), id).unwrap()];
                assert_eq!(
                    img.fragments
                        .iter()
                        .map(|f| f.page_index)
                        .collect::<Vec<_>>(),
                    [page],
                    "{label}: image {id}"
                );
            }
        }
    }

    /// html-to-pdf F183 (plan 7.7): an inline root that holds inline boxes (an `<img>` on each of its lines) and that
    /// the strip cannot hold is cut between its lines, never between its boxes. The block walk used to enter it because
    /// one of its children (an image on a later line) overflowed the strip, and cut it between the images in fragments
    /// as tall as the images: with no line ranges the renderer measured the lines from those heights, and a line taller
    /// than its fragment (the 64 px line of a 60 px image) was dropped, with the text on it. The cells below are not the
    /// first of their table (row 5), so `nested_line_split` leaves them alone. Every cell that is cut has line ranges
    /// that tile its lines, and every image has one fragment.
    #[test]
    fn an_inline_root_with_boxes_is_never_cut_between_its_boxes() {
        let font = Arc::new(
            std::fs::read(concat!(
                env!("CARGO_MANIFEST_DIR"),
                "/../../examples/.fonts/NotoSans-Regular.ttf"
            ))
            .expect("the bundled Noto Sans"),
        );
        let words = |k: usize| {
            (0..20)
                .map(|i| format!("w{:03}", k * 20 + i))
                .collect::<Vec<_>>()
                .join(" ")
        };
        let img = |id: &str, k: usize| format!(r#"<img id="{id}{k}" width="60" height="60">"#);
        // (label, cell content, ids of its images)
        type Cell = (
            &'static str,
            fn(usize, &dyn Fn(&str, usize) -> String, &str) -> String,
        );
        let shapes: [Cell; 4] = [
            ("img img words", |k, i, w| {
                format!("{}<br>{}<br>{w}", i("a", k), i("b", k))
            }),
            ("img img img words", |k, i, w| {
                format!("{}<br>{}<br>{}<br>{w}", i("a", k), i("b", k), i("c", k))
            }),
            ("words img img", |k, i, w| {
                format!("{w}<br>{}<br>{}", i("a", k), i("b", k))
            }),
            ("img words img", |k, i, w| {
                format!("{}<br>{w}<br>{}", i("a", k), i("b", k))
            }),
        ];
        // (label, table style, td style)
        let tables = [
            (
                "collapsed",
                "border-collapse:collapse;table-layout:fixed;width:100%",
                "padding:6px;border:1px solid #000",
            ),
            (
                "separated",
                "border-collapse:separate;border-spacing:0;table-layout:fixed;width:100%",
                "padding:6px;border:1px solid #000",
            ),
            ("no border", "width:100%", "padding:6px"),
        ];
        let mut cut_cells = 0;
        let mut failures = Vec::new();
        for (shape, content) in shapes {
            for (label, table_style, td_style) in tables {
                for filler in (0..260).step_by(6) {
                    let rows: String = (0..8)
                        .map(|k| {
                            format!(
                                r#"<tr><td id="td{k}">{}</td><td>x</td></tr>"#,
                                content(k, &img, &words(k))
                            )
                        })
                        .collect();
                    let html = format!(
                        r#"<style>body{{margin:0;font:16px 'Noto Sans'}}table{{{table_style}}}td{{{td_style};vertical-align:top}}</style><body><div style="height:{filler}px"></div><table>{rows}</table></body>"#
                    );
                    let mut doc = blitz_adapter::parse(&html, 794.0, std::slice::from_ref(&font));
                    blitz_adapter::resolve(&mut doc);
                    let table = run_pass(&mut doc, 1122.52);
                    for k in 0..8 {
                        let td_id = find_by_id(doc.deref(), &format!("td{k}")).unwrap();
                        let lines = collect_inline_line_metrics(doc.get_node(td_id).unwrap()).len();
                        let td = &table[&td_id];
                        let what = format!("{shape}, {label}, filler {filler}, td{k}");
                        if td.fragments.len() > 1 {
                            cut_cells += 1;
                            if !line_slices_tile(&td.line_slices, lines) {
                                failures.push(format!(
                                    "{what}: cut into {:?} with line ranges {:?} of {lines} lines",
                                    td.fragments
                                        .iter()
                                        .map(|f| (f.page_index, f.y.to_f32(), f.height.to_f32()))
                                        .collect::<Vec<_>>(),
                                    td.line_slices
                                        .iter()
                                        .map(|s| (s.first, s.end))
                                        .collect::<Vec<_>>()
                                ));
                            }
                        }
                        for pre in ["a", "b", "c"] {
                            let Some(id) = find_by_id(doc.deref(), &format!("{pre}{k}")) else {
                                continue;
                            };
                            let n = table[&id].fragments.len();
                            if n != 1 {
                                failures.push(format!("{what}: image {pre}{k} has {n} fragments"));
                            }
                        }
                    }
                }
            }
        }
        assert!(cut_cells > 0, "no cell was cut: the test does not test");
        assert!(
            failures.is_empty(),
            "{} failures in {cut_cells} cut cells: {:#?}",
            failures.len(),
            &failures[..failures.len().min(8)]
        );
    }

    /// html-to-pdf F183 (plan 7.7): the first thing an inline root with a box in it needs below its top is its first
    /// two lines (orphans), as for a cut paragraph, not the bottom of its first box. A cell of `text<br><img><br>text`
    /// whose two text lines fit the room but not the image under them is entered and cut after the text, as Chrome
    /// does; it used to ask for the text and the image, did not fit, and moved to the next page whole.
    #[test]
    fn an_inline_root_asks_for_its_first_lines_not_for_its_first_box() {
        let font = Arc::new(
            std::fs::read(concat!(
                env!("CARGO_MANIFEST_DIR"),
                "/../../examples/.fonts/NotoSans-Regular.ttf"
            ))
            .expect("the bundled Noto Sans"),
        );
        let words = |from: usize| {
            (from..from + 28)
                .map(|i| format!("w{i:03}"))
                .collect::<Vec<_>>()
                .join(" ")
        };
        let html = format!(
            r#"<style>body{{margin:0;font:16px 'Noto Sans'}}td{{padding:5px;vertical-align:top}}</style><body>
            <div style="height:1030px"></div><table style="width:100%"><tr><td id="td">{}<br><img id="im" width="60" height="60"><br>{}</td></tr></table></body>"#,
            words(1),
            words(29)
        );
        let mut doc = blitz_adapter::parse(&html, 794.0, std::slice::from_ref(&font));
        blitz_adapter::resolve(&mut doc);
        let table = run_pass(&mut doc, 1122.52);
        let td = &table[&find_by_id(doc.deref(), "td").unwrap()];
        let lines = collect_inline_line_metrics(
            doc.get_node(find_by_id(doc.deref(), "td").unwrap())
                .unwrap(),
        )
        .len();
        assert_eq!(lines, 5, "two text lines, the image line, two text lines");
        assert_eq!(
            td.fragments
                .iter()
                .map(|f| f.page_index)
                .collect::<Vec<_>>(),
            [0, 1],
            "the cell is cut: {td:?}"
        );
        assert_eq!(
            td.line_slices
                .iter()
                .map(|s| (s.first, s.end))
                .collect::<Vec<_>>(),
            [(0, 2), (2, 5)],
            "after the two text lines"
        );
        let im = &table[&find_by_id(doc.deref(), "im").unwrap()];
        assert_eq!(
            im.fragments
                .iter()
                .map(|f| f.page_index)
                .collect::<Vec<_>>(),
            [1]
        );
    }

    /// Cell content for the `avoid` tests: an image, then `n` words that a 60 px cell wraps one per line (20 px lines). The
    /// fragmenter counts a cell made of text alone as a leaf and moves its table whole; it enters the row at its first
    /// line only when an inline box is the first thing in the cell (the shape of an invoice cell with a logo or an icon).
    fn avoid_cell_lines(n: usize) -> String {
        format!(
            "<img width=\"20\" height=\"20\">{}",
            vec!["line"; n].join(" ")
        )
    }

    /// Pages of the fragments of the element `id`, and the top (page-local y) of the first one.
    fn pages_and_top(
        doc: &blitz_dom::BaseDocument,
        table: &PaginationGeometryTable,
        id: &str,
    ) -> (Vec<u32>, f32) {
        let g = &table[&find_by_id(doc, id).unwrap()];
        (
            g.fragments.iter().map(|f| f.page_index).collect(),
            g.fragments[0].y.to_f32(),
        )
    }

    /// html-to-pdf F83d (table rows and cells): `break-inside: avoid` on a `td`, or on its `tr` (which has no box of its own
    /// in the fragmenter: Blitz gives `tbody` / `tr` no size), keeps the row on one page when it fits one (css-break-3
    /// section 3.3: a class A break point between rows is preferred to one inside the row; section 4.4 relaxes `avoid`
    /// only when the row is taller than the fragmentainer). The cell used to be split between its lines whenever it was the
    /// first thing in its table, which is every cell of a table without a border and, since F183, every cell of a
    /// bordered table too. Chrome moves the row whole. Page 1000 px, 20 px lines.
    #[test]
    fn avoid_on_a_row_or_a_cell_moves_a_row_that_fits_a_page_whole() {
        let font = Arc::new(
            std::fs::read(concat!(
                env!("CARGO_MANIFEST_DIR"),
                "/../../examples/.fonts/NotoSans-Regular.ttf"
            ))
            .expect("the bundled Noto Sans"),
        );
        // (table style, rule, what the rule is for)
        let rules = [
            ("tr{break-inside:avoid}", "tr"),
            ("td{break-inside:avoid}", "td"),
            ("tr{page-break-inside:avoid}", "page-break-inside on tr"),
            ("td{page-break-inside:avoid}", "page-break-inside on td"),
        ];
        for table_style in ["border-collapse:collapse", "border-collapse:separate", ""] {
            for (rule, what) in rules {
                // 900 px of filler, eight lines of 20 px: the row does not fit the 100 px left, fits a page.
                let html = format!(
                    r#"<style>body{{margin:0;font:16px 'Noto Sans';line-height:20px}}td{{width:60px;padding:4px;border:1px solid #000}}{rule}</style><body>
                    <div style="height:900px"></div><table style="{table_style}"><tr><td id="a">{}</td><td id="b"><img width="20" height="20">short</td></tr></table></body>"#,
                    avoid_cell_lines(8)
                );
                let mut doc = blitz_adapter::parse(&html, 794.0, std::slice::from_ref(&font));
                blitz_adapter::resolve(&mut doc);
                let styles = blitz_adapter::extract_column_style_table(&doc);
                let table =
                    run_pass_with_break_styles(doc.deref_mut(), 1000.0_f32.as_px(), &styles);
                let label = format!("{table_style:?} / {what}");
                let (a_pages, a_top) = pages_and_top(&doc, &table, "a");
                let (b_pages, b_top) = pages_and_top(&doc, &table, "b");
                assert_eq!(a_pages, [1], "{label}: the cell is whole on page 1");
                assert_eq!(b_pages, [1], "{label}: its row mate follows it");
                assert!(
                    (a_top - b_top).abs() < 0.5 && a_top < 5.0,
                    "{label}: the row starts the page: {a_top} / {b_top}"
                );
                assert!(
                    table[&find_by_id(doc.deref(), "a").unwrap()]
                        .line_slices
                        .len()
                        < 2,
                    "{label}: not cut between lines"
                );
            }
        }
    }

    /// F83d: only the row that straddles the page moves; the rows before it stay, the row after it follows it. And a
    /// row that does not fit a page whole is split as before (`avoid` cannot be honoured, css-break-3 section 4.4).
    #[test]
    fn avoid_row_moves_alone_and_a_row_taller_than_a_page_still_splits() {
        let font = Arc::new(
            std::fs::read(concat!(
                env!("CARGO_MANIFEST_DIR"),
                "/../../examples/.fonts/NotoSans-Regular.ttf"
            ))
            .expect("the bundled Noto Sans"),
        );
        for table_style in ["border-collapse:collapse", ""] {
            let html = format!(
                r#"<style>body{{margin:0;font:16px 'Noto Sans';line-height:20px}}td{{width:60px;padding:4px;border:1px solid #000}}tr{{break-inside:avoid}}</style><body>
                <div style="height:760px"></div><table style="{table_style}">
                <tr><td id="r1">{}</td></tr><tr><td id="r2">{}</td></tr><tr><td id="r3">{}</td></tr>
                <tr><td id="r4">{}</td></tr></table></body>"#,
                avoid_cell_lines(6),
                avoid_cell_lines(16),
                avoid_cell_lines(6),
                avoid_cell_lines(120),
            );
            let mut doc = blitz_adapter::parse(&html, 794.0, std::slice::from_ref(&font));
            blitz_adapter::resolve(&mut doc);
            let styles = blitz_adapter::extract_column_style_table(&doc);
            let table = run_pass_with_break_styles(doc.deref_mut(), 1000.0_f32.as_px(), &styles);
            let pages = |id: &str| pages_and_top(&doc, &table, id).0;
            assert_eq!(pages("r1"), [0], "{table_style:?}: r1 fits");
            assert_eq!(
                pages("r2"),
                [1],
                "{table_style:?}: r2 straddles, moves whole"
            );
            assert_eq!(pages("r3"), [1], "{table_style:?}: r3 follows r2");
            // 1200 px of lines cannot be kept together: split as an `auto` row, from page 1 on.
            let r4 = pages("r4");
            assert!(
                r4.len() >= 2 && r4[0] >= 1,
                "{table_style:?}: r4 is taller than a page and splits: {r4:?}"
            );
        }
    }

    /// F83d: the row that moves goes below the repeated header of the next page (F12), and the table is not left with a
    /// stub on the page it came from.
    #[test]
    fn avoid_row_moved_to_the_next_page_sits_below_the_repeated_header() {
        let font = Arc::new(
            std::fs::read(concat!(
                env!("CARGO_MANIFEST_DIR"),
                "/../../examples/.fonts/NotoSans-Regular.ttf"
            ))
            .expect("the bundled Noto Sans"),
        );
        for table_style in ["border-collapse:collapse", ""] {
            let html = format!(
                r#"<style>body{{margin:0;font:16px 'Noto Sans';line-height:20px}}td,th{{width:60px;padding:4px;border:1px solid #000}}tr.k{{break-inside:avoid}}</style><body>
                <div style="height:830px"></div><table style="{table_style}"><thead id="h"><tr><th>head</th></tr></thead>
                <tbody><tr class="k"><td id="r1">{}</td></tr><tr class="k"><td id="r2">{}</td></tr></tbody></table></body>"#,
                avoid_cell_lines(6),
                avoid_cell_lines(16),
            );
            let mut doc = blitz_adapter::parse(&html, 794.0, std::slice::from_ref(&font));
            blitz_adapter::resolve(&mut doc);
            let styles = blitz_adapter::extract_column_style_table(&doc);
            let table = run_pass_with_break_styles(doc.deref_mut(), 1000.0_f32.as_px(), &styles);
            let (r1_pages, _) = pages_and_top(&doc, &table, "r1");
            let (r2_pages, r2_top) = pages_and_top(&doc, &table, "r2");
            assert_eq!(r1_pages, [0], "{table_style:?}: r1 fits below the header");
            assert_eq!(r2_pages, [1], "{table_style:?}: r2 moves whole");
            let h = &table[&find_by_id(doc.deref(), "h").unwrap()];
            assert!(
                h.fragments.iter().any(|f| f.page_index == 1),
                "{table_style:?}: the header is repeated on page 1: {h:?}"
            );
            assert!(
                r2_top >= 20.0,
                "{table_style:?}: r2 sits below the repeated header: {r2_top}"
            );
        }
    }

    /// F32a: a paragraph of two or more lines took the line-splitting path, which recorded the
    /// paragraph but none of its inline boxes. `paragraph.rs` draws an `InlineBox` item only when
    /// the box has a fragment on the page, so an `<img>` or `inline-block` in such a paragraph was
    /// never drawn.
    #[test]
    fn multi_line_paragraph_records_its_inline_boxes() {
        let html = r#"
            <html><body style="margin: 0">
              <p style="margin: 0">one<br>two <span id="box" style="display:inline-block;width:20px;height:10px"></span></p>
              <p style="margin: 0">three<br>four<br>five <span id="late" style="display:inline-block;width:20px;height:10px"></span></p>
            </body></html>
        "#;
        let mut doc = parse(html, 600.0);
        let table = run_pass(&mut doc, 800.0);
        for id in ["box", "late"] {
            let node = node_with_id(&doc, id);
            let frags = &table
                .get(&node)
                .unwrap_or_else(|| panic!("#{id} has no geometry"))
                .fragments;
            assert_eq!(frags.len(), 1, "#{id}");
            assert_eq!(frags[0].page_index, 0);
            assert_eq!(frags[0].width.to_f32(), 20.0);
        }
    }

    /// F32a: an inline box on the second page of a paragraph that splits between lines lands on
    /// that page.
    #[test]
    fn split_paragraph_places_inline_boxes_on_their_lines_page() {
        // Six 30 px lines (`<br>`); the page strip holds four, so lines 4-5 go to page 1 (widows
        // = 2 satisfied). The box is on the last line.
        let html = r#"
            <html><body style="margin: 0">
              <p style="margin: 0; line-height: 30px; font-size: 20px">a<br>b<br>c<br>d<br>e<br><span id="last" style="display:inline-block;width:20px;height:10px"></span></p>
            </body></html>
        "#;
        let mut doc = parse(html, 600.0);
        let table = run_pass(&mut doc, 130.0);
        let last = node_with_id(&doc, "last");
        let frags = &table
            .get(&last)
            .expect("no geometry for the inline box")
            .fragments;
        assert_eq!(
            frags[0].page_index, 1,
            "the box is on the last line: {frags:?}"
        );
    }

    #[test]
    fn taller_than_page_block_emits_single_oversize_fragment() {
        // 1000px block on a 800px page. Block-only fragmenter emits it whole
        // on the page where its top lands, with the full height — true
        // split is the next iteration's job.
        let html = r#"
            <html><body>
              <div style="height: 1000px"></div>
            </body></html>
        "#;
        let mut doc = parse(html, 600.0);
        let table = run_pass(&mut doc, 800.0);
        // Phase 2.3 fix: body + 1 oversized child = 2 entries.
        assert_eq!(table.len(), 2);
        // The oversized child is the entry whose height ≈ 1000.
        let oversize = table
            .values()
            .find(|g| (g.fragments[0].height.to_f32() - 1000.0).abs() < 1.0)
            .expect("oversized child fragment");
        assert_eq!(oversize.fragments.len(), 1);
        assert_eq!(oversize.fragments[0].page_index, 0);
    }

    /// fulgur-yb27: `fragment_block_subtree` must walk `layout_children`
    /// so anonymous block wrappers Stylo synthesizes around inline-
    /// level siblings (CSS 2.1 §9.2.1.1) are visited. Without this,
    /// a tail inline string after a tall block sibling never
    /// fragments to the next page — the block consumes page 1, the
    /// trailing inline run is treated as a zero-height text node
    /// (`final_layout` defaults), and pagination terminates with a
    /// single fragment.
    #[test]
    fn fragment_block_subtree_walks_layout_children_for_anon_block_synthesis() {
        // Outer wrapper > [tall block sibling, trailing inline text].
        // Stylo wraps the trailing text in an anon block whose Taffy
        // location.y is past the page boundary.
        let html = r#"
            <html><body style="margin: 0; padding: 0">
              <div style="width: 200px;">
                <div style="height: 300px; width: 50px; background:hotpink;"></div>
                trailing tail
              </div>
            </body></html>
        "#;
        let mut doc = parse(html, 600.0);
        let table = blitz_adapter::extract_column_style_table(&doc);
        let geom = super::run_pass_with_break_styles(doc.deref_mut(), 200.0_f32.as_px(), &table);
        // Page count must be ≥ 2 — without yb27 the trailing inline
        // text never reaches a new page (single fragment, page 0 only).
        let max_page = geom
            .values()
            .flat_map(|g| g.fragments.iter())
            .map(|f| f.page_index)
            .max()
            .unwrap_or(0);
        assert!(
            max_page >= 1,
            "yb27: anon block from mixed inline/block siblings must \
             paginate to a new page; max_page={max_page}",
        );
    }

    /// fulgur-oc51: when `fragment_block_subtree`'s recursion advances
    /// `page_index`, the parent's pre-recursion-page span must be
    /// recorded as a fragment. Without this, a tall nested subtree
    /// that crosses pages inside the recursion lifts the parent's
    /// only fragment to the *last* page (line 1799 close), leaving
    /// page 1 with no parent paint at all (background/borders gone).
    #[test]
    fn fragment_block_subtree_emits_parent_fragment_when_recursion_crosses_page() {
        // Two-deep nesting: outer (with background) > inner > [tall
        // child, trailing inline]. Inner's recursion will cross the
        // page boundary; outer (the <section>) must still get a
        // fragment on page 0.
        let html = r#"
            <html><body style="margin: 0; padding: 0">
              <section style="width: 200px;">
                <div style="width: 200px;">
                  <div style="height: 300px; width: 50px; background:hotpink;"></div>
                  tail
                </div>
              </section>
            </body></html>
        "#;
        let mut doc = parse(html, 600.0);
        let table = blitz_adapter::extract_column_style_table(&doc);
        // Locate the <section> node id explicitly so the assertion
        // targets the specific block that should keep a page-0
        // fragment — without this, a wide body fragment on page 0
        // would satisfy a generic "any wide page-0 fragment" check
        // even when the section itself has no page-0 entry.
        let section_id =
            find_node_by_local_name(&doc, "section").expect("fixture must contain a <section>");
        let geom = super::run_pass_with_break_styles(doc.deref_mut(), 200.0_f32.as_px(), &table);
        let section_geom = geom.get(&section_id).unwrap_or_else(|| {
            panic!("oc51: <section> (node_id={section_id}) missing from geometry; geom={geom:?}")
        });
        let has_page_0 = section_geom.fragments.iter().any(|f| f.page_index == 0);
        assert!(
            has_page_0,
            "oc51: <section> (node_id={section_id}) must keep a page-0 \
             fragment when its nested recursion crosses a page boundary; \
             section_fragments={:?}",
            section_geom.fragments,
        );
        // Must also reach page 1+ — otherwise the test isn't actually
        // exercising the recursion-cross-page path.
        let max_page = geom
            .values()
            .flat_map(|g| g.fragments.iter())
            .map(|f| f.page_index)
            .max()
            .unwrap_or(0);
        assert!(
            max_page >= 1,
            "oc51: fixture must paginate to ≥ 2 pages to exercise the \
             recursion-cross-page path; max_page={max_page}",
        );
    }

    /// Locate the first node in the document whose element local name
    /// matches `tag`. Used by tests that need a node_id reference for
    /// a specific HTML element without depending on Stylo's internal
    /// node numbering.
    fn find_node_by_local_name(doc: &blitz_html::HtmlDocument, tag: &str) -> Option<usize> {
        use std::ops::Deref;
        let base: &blitz_dom::BaseDocument = doc.deref();
        let root_id = base.root_element().id;
        fn walk(base: &blitz_dom::BaseDocument, id: usize, tag: &str) -> Option<usize> {
            let node = base.get_node(id)?;
            if let Some(elem) = node.element_data()
                && elem.name.local.as_ref() == tag
            {
                return Some(id);
            }
            for &child_id in &node.children {
                if let Some(found) = walk(base, child_id, tag) {
                    return Some(found);
                }
            }
            None
        }
        walk(base, root_id, tag)
    }

    /// fulgur-puml: `explicit_inset_axes` must classify each axis as
    /// explicit (length/percentage inset present) vs auto, since the nested-
    /// abs base selection depends on it. A silent regression here would
    /// mis-anchor abs descendants across a static intermediate.
    #[test]
    fn explicit_inset_axes_classifies_per_axis() {
        use std::ops::Deref;
        // (fragment, (x_explicit, y_explicit))
        let cases: &[(&str, (bool, bool))] = &[
            ("<div style=\"position:absolute\"></div>", (false, false)),
            (
                "<div style=\"position:absolute; top:10px\"></div>",
                (false, true),
            ),
            (
                "<div style=\"position:absolute; right:0\"></div>",
                (true, false),
            ),
            (
                "<div style=\"position:absolute; left:0; right:0\"></div>",
                (true, false),
            ),
            (
                "<div style=\"position:absolute; top:0; left:0\"></div>",
                (true, true),
            ),
        ];
        for (frag, expected) in cases {
            let html = format!("<html><body>{frag}</body></html>");
            let doc = parse(&html, 600.0);
            let id = find_node_by_local_name(&doc, "div").expect("div present");
            let node = doc.deref().get_node(id).expect("node present");
            assert_eq!(explicit_inset_axes(node), *expected, "fragment: {frag}");
        }
    }

    /// coderabbit: body containing only a `position: running()` element
    /// plus a tall `position: absolute` div must treat the body as
    /// having no in-flow content so `may_extend_pages = true`.
    /// Without the running_store guard, the running child increments
    /// `body_has_in_flow_content` and truncates the abs subtree.
    #[test]
    fn position_absolute_body_direct_running_only_body_extends_pages() {
        use crate::blitz_adapter;
        use crate::gcpm::parser::parse_gcpm;
        use std::sync::Arc;

        let css = ".header { position: running(pageHeader); }";
        let html = r#"<!DOCTYPE html>
<html><head></head>
<body style="margin:0">
<div class="header">Doc Header</div>
<div style="position: absolute; top: 0; width: 100px; height: 1800px">x</div>
</body></html>"#;

        let gcpm = parse_gcpm(css);
        let fonts: Vec<Arc<Vec<u8>>> = Vec::new();
        let mut doc = blitz_adapter::parse(html, 600.0, &fonts);
        let pass = blitz_adapter::RunningElementPass::new(gcpm.running_mappings.clone());
        let pass_ctx = blitz_adapter::PassContext { font_data: &fonts };
        blitz_adapter::apply_single_pass(&pass, &mut doc, &pass_ctx);
        let store = pass.into_running_store();
        blitz_adapter::resolve(&mut doc);

        let mut geom = PaginationGeometryTable::new();
        super::append_position_absolute_body_direct_fragments(
            &mut geom,
            doc.deref_mut(),
            1,
            600.0,
            800.0,
            Some(&store),
        );

        let max_page = geom
            .values()
            .flat_map(|g| g.fragments.iter())
            .filter(|f| (f.height.to_f32() - 1800.0).abs() < 0.5)
            .map(|f| f.page_index)
            .max();
        assert!(
            max_page.is_some_and(|p| p >= 2),
            "tall abs div in running-only body should extend to page 2; max_page={max_page:?}"
        );
    }

    /// coderabbit: abs subtree starting beyond the current page budget
    /// must still emit fragments when `may_extend_pages` is true.
    /// Regression for the condition `first_page_f < total_pages` that
    /// blocked fragment emission even when the absolute pass is
    /// responsible for extending the page count.
    #[test]
    fn position_absolute_body_direct_beyond_page_budget_extends_pages() {
        let html = r#"
            <html><body style="margin:0">
              <div style="position: absolute; top: 1600px; width: 100px; height: 100px">x</div>
            </body></html>
        "#;
        let mut doc = parse(html, 600.0);
        let mut geom = PaginationGeometryTable::new();
        super::append_position_absolute_body_direct_fragments(
            &mut geom,
            doc.deref_mut(),
            1,
            600.0,
            800.0,
            None,
        );
        let max_page = geom
            .values()
            .flat_map(|g| g.fragments.iter())
            .map(|f| f.page_index)
            .max();
        assert!(
            max_page.is_some_and(|p| p >= 2),
            "abs div at top:1600px with 800px pages should land on page 2; max_page={max_page:?}"
        );
    }

    #[test]
    fn grid_row_leaf_cells_move_together_when_the_row_does_not_fit() {
        // 2-col grid, each leaf cell 60px tall.
        // spacer 80px pushes grid to y=80 on a 100px page.
        // fulgur-ysms: cell 2 must not be pushed to page 1 on its own while
        // cell 1 stays on page 0; the cells of a row move together.
        let html = r#"
            <html><body style="margin: 0; padding: 0">
              <div style="height: 80px"></div>
              <div style="display: grid; grid-template-columns: 100px 100px; width: 200px;">
                <div style="height: 60px; width: 100px"></div>
                <div style="height: 60px; width: 100px"></div>
              </div>
            </body></html>
        "#;
        let mut doc = parse(html, 400.0);
        let table = blitz_adapter::extract_column_style_table(&doc);
        let geom = super::run_pass_with_break_styles(doc.deref_mut(), 100.0_f32.as_px(), &table);

        // collect all 60px-tall, 100px-wide fragments (the two leaf cells)
        let mut frags: Vec<(u32, f32, f32)> = geom
            .values()
            .flat_map(|g| g.fragments.iter())
            .filter(|f| {
                (f.height.to_f32() - 60.0).abs() < 0.5 && (f.width.to_f32() - 100.0).abs() < 0.5
            })
            .map(|f| (f.page_index, f.x.to_f32(), f.y.to_f32()))
            .collect();
        frags.sort_by(|a, b| a.partial_cmp(b).unwrap());

        // The row (60px) does not fit the 20px left under the spacer and its
        // cells cannot be split, so the whole row starts page 1 (F28). Before,
        // both cells were placed hanging below the bottom of page 0.
        assert_eq!(frags.len(), 2, "frags={frags:?}");
        assert!(
            frags.iter().all(|(p, _, y)| *p == 1 && y.abs() < 0.5),
            "both cells of the row must move to the top of page 1 together; frags={frags:?}"
        );
    }

    #[test]
    fn flex_row_leaf_cells_move_together_when_the_row_does_not_fit() {
        let html = r#"
            <html><body style="margin: 0; padding: 0">
              <div style="height: 80px"></div>
              <div style="display: flex; width: 200px;">
                <div style="height: 60px; width: 100px; flex: 0 0 100px"></div>
                <div style="height: 60px; width: 100px; flex: 0 0 100px"></div>
              </div>
            </body></html>
        "#;
        let mut doc = parse(html, 400.0);
        let table = blitz_adapter::extract_column_style_table(&doc);
        let geom = super::run_pass_with_break_styles(doc.deref_mut(), 100.0_f32.as_px(), &table);

        let mut frags: Vec<(u32, f32, f32)> = geom
            .values()
            .flat_map(|g| g.fragments.iter())
            .filter(|f| {
                (f.height.to_f32() - 60.0).abs() < 0.5 && (f.width.to_f32() - 100.0).abs() < 0.5
            })
            .map(|f| (f.page_index, f.x.to_f32(), f.y.to_f32()))
            .collect();
        frags.sort_by(|a, b| a.partial_cmp(b).unwrap());

        // The row (60px) does not fit the 20px left under the spacer and its
        // cells cannot be split, so the whole row starts page 1 (F28). Before,
        // both cells were placed hanging below the bottom of page 0.
        assert_eq!(frags.len(), 2, "frags={frags:?}");
        assert!(
            frags.iter().all(|(p, _, y)| *p == 1 && y.abs() < 0.5),
            "both cells of the row must move to the top of page 1 together; frags={frags:?}"
        );
    }

    #[test]
    fn grid_row_recursive_cells_cosplit_across_page_boundary() {
        // 2-col grid, each cell has 2 inner divs (40px each) = 80px total.
        // spacer 50px, page_height 100px → grid starts at y=50.
        // The first inner div fits (50..90), the second does not (90..130),
        // so the row crosses the page boundary between the inner divs.
        // (Before F28 the spacer was 70px and the first inner div hung below
        // the page; the fragmenter now moves a row whose first block does
        // not fit to the next page instead.)
        //
        // pre-fix: cell 2 recursion starts at page 1 cursor=0 (because
        //   cell 1's recursion advanced page_index to 1) → both inner
        //   divs of cell 2 land entirely on page 1.
        // post-fix: both cells start recursion at page 0 cursor=70 →
        //   first inner div (40px) lands on page 0 (y=50..90), the second
        //   one on page 1. Both columns' first inner divs must appear on page 0.
        let html = r#"
            <html><body style="margin: 0; padding: 0">
              <div style="height: 50px"></div>
              <div style="display: grid; grid-template-columns: 100px 100px; width: 200px;">
                <div style="width: 100px">
                  <div style="height: 40px; width: 100px"></div>
                  <div style="height: 40px; width: 100px"></div>
                </div>
                <div style="width: 100px">
                  <div style="height: 40px; width: 100px"></div>
                  <div style="height: 40px; width: 100px"></div>
                </div>
              </div>
            </body></html>
        "#;
        let mut doc = parse(html, 400.0);
        let table = blitz_adapter::extract_column_style_table(&doc);
        let geom = super::run_pass_with_break_styles(doc.deref_mut(), 100.0_f32.as_px(), &table);

        // inner divs: 40px tall, 100px wide
        let mut inner: Vec<(u32, f32, f32)> = geom
            .values()
            .flat_map(|g| g.fragments.iter())
            .filter(|f| {
                (f.height.to_f32() - 40.0).abs() < 0.5 && (f.width.to_f32() - 100.0).abs() < 0.5
            })
            .map(|f| (f.page_index, f.x.to_f32(), f.y.to_f32()))
            .collect();
        inner.sort_by(|a, b| a.partial_cmp(b).unwrap());

        // Each column has 2 inner divs (40px each).
        // After the fix, both columns' FIRST inner div must appear on page 0
        // (at y≈50, since the grid row starts there).
        // Pre-fix: only cell 1's first inner div is on page 0; cell 2's
        // recursion starts from page 1 so its inner divs are at y=0,40 on page 1.
        let _first_divs_page0: Vec<_> = inner
            .iter()
            .filter(|(p, _, y)| *p == 0 && *y > 40.0)
            .collect();
        // The critical assertion: the RIGHT column's first inner div must also
        // appear on page 0 (at x=100, y≈70). Pre-fix: cell 2's recursion starts
        // from page 1, so its inner divs land at x=100 only on page 1.
        let right_col_page0: Vec<_> = inner
            .iter()
            .filter(|(p, x, y)| *p == 0 && (x - 100.0).abs() < 0.5 && *y > 40.0)
            .collect();
        assert!(
            !right_col_page0.is_empty(),
            "right column's first inner div must appear on page 0 at x=100 (pre-fix: only on page 1); inner={inner:?}"
        );
    }

    #[test]
    fn flex_row_recursive_cells_cosplit_across_page_boundary() {
        let html = r#"
            <html><body style="margin: 0; padding: 0">
              <div style="height: 50px"></div>
              <div style="display: flex; width: 200px;">
                <div style="width: 100px; flex: 0 0 100px">
                  <div style="height: 40px; width: 100px"></div>
                  <div style="height: 40px; width: 100px"></div>
                </div>
                <div style="width: 100px; flex: 0 0 100px">
                  <div style="height: 40px; width: 100px"></div>
                  <div style="height: 40px; width: 100px"></div>
                </div>
              </div>
            </body></html>
        "#;
        let mut doc = parse(html, 400.0);
        let table = blitz_adapter::extract_column_style_table(&doc);
        let geom = super::run_pass_with_break_styles(doc.deref_mut(), 100.0_f32.as_px(), &table);

        let mut inner: Vec<(u32, f32, f32)> = geom
            .values()
            .flat_map(|g| g.fragments.iter())
            .filter(|f| {
                (f.height.to_f32() - 40.0).abs() < 0.5 && (f.width.to_f32() - 100.0).abs() < 0.5
            })
            .map(|f| (f.page_index, f.x.to_f32(), f.y.to_f32()))
            .collect();
        inner.sort_by(|a, b| a.partial_cmp(b).unwrap());

        let right_col_page0: Vec<_> = inner
            .iter()
            .filter(|(p, x, y)| *p == 0 && (x - 100.0).abs() < 0.5 && *y > 40.0)
            .collect();
        assert!(
            !right_col_page0.is_empty(),
            "right column's first inner div must appear on page 0 at x=100 (pre-fix: only on page 1); inner={inner:?}"
        );
    }

    // ── break-after: page in nested block subtree (lines 2263-2279) ─────────────

    /// `break-after: page` on a child inside a non-body container must close
    /// the parent's current-page fragment and advance page_index, so the
    /// sibling that follows lands on page 1.
    #[test]
    fn fragment_block_subtree_break_after_page_pushes_sibling_to_next_page() {
        let html = r#"
            <html><body style="margin: 0; padding: 0">
              <div>
                <div style="height: 100px; break-after: page"></div>
                <div style="height: 100px"></div>
              </div>
            </body></html>
        "#;
        let mut doc = parse(html, 600.0);
        let table = blitz_adapter::extract_column_style_table(&doc);
        let geom = super::run_pass_with_break_styles(doc.deref_mut(), 800.0_f32.as_px(), &table);
        assert!(
            geom.values()
                .flat_map(|g| g.fragments.iter())
                .any(|f| f.page_index == 1),
            "break-after: page must push the following sibling onto page 1; geom={geom:?}"
        );
    }

    // ── zero-height body-direct children with break (lines 639-668) ─────────────

    /// A zero-height body-direct element with `break-before: page` must fire
    /// a page break when cursor_y > 0 (lines 643-644).
    #[test]
    fn zero_height_body_direct_break_before_page_fires_page_break() {
        let html = r#"
            <html><body style="margin: 0; padding: 0">
              <div style="height: 100px"></div>
              <div style="height: 0px; break-before: page"></div>
              <div style="height: 100px"></div>
            </body></html>
        "#;
        let mut doc = parse(html, 600.0);
        let table = blitz_adapter::extract_column_style_table(&doc);
        let geom = super::run_pass_with_break_styles(doc.deref_mut(), 800.0_f32.as_px(), &table);
        assert!(
            geom.values()
                .flat_map(|g| g.fragments.iter())
                .any(|f| f.page_index == 1),
            "break-before: page on a zero-height body-direct child must advance to page 1; \
             geom={geom:?}"
        );
    }

    /// A zero-height body-direct element with `break-after: page` must fire
    /// a page break (lines 667-668).
    #[test]
    fn zero_height_body_direct_break_after_page_fires_page_break() {
        let html = r#"
            <html><body style="margin: 0; padding: 0">
              <div style="height: 100px"></div>
              <div style="height: 0px; break-after: page"></div>
              <div style="height: 100px"></div>
            </body></html>
        "#;
        let mut doc = parse(html, 600.0);
        let table = blitz_adapter::extract_column_style_table(&doc);
        let geom = super::run_pass_with_break_styles(doc.deref_mut(), 800.0_f32.as_px(), &table);
        assert!(
            geom.values()
                .flat_map(|g| g.fragments.iter())
                .any(|f| f.page_index == 1),
            "break-after: page on a zero-height body-direct child must advance to page 1; \
             geom={geom:?}"
        );
    }

    // ── zero-height nested children with break (lines 1900-1953) ────────────────

    /// A zero-height element inside a block container with `break-before: page`
    /// must fire a page break and flush a parent fragment (lines 1900-1918).
    #[test]
    fn zero_height_nested_break_before_page_fires_page_break() {
        let html = r#"
            <html><body style="margin: 0; padding: 0">
              <div>
                <div style="height: 100px"></div>
                <div style="height: 0px; break-before: page"></div>
              </div>
            </body></html>
        "#;
        let mut doc = parse(html, 600.0);
        let table = blitz_adapter::extract_column_style_table(&doc);
        let geom = super::run_pass_with_break_styles(doc.deref_mut(), 800.0_f32.as_px(), &table);
        assert!(
            geom.values()
                .flat_map(|g| g.fragments.iter())
                .any(|f| f.page_index == 1),
            "break-before: page on a zero-height nested child must advance to page 1; \
             geom={geom:?}"
        );
    }

    /// A zero-height element inside a block container with `break-after: page`
    /// must fire a page break and flush a parent fragment (lines 1936-1953).
    #[test]
    fn zero_height_nested_break_after_page_fires_page_break() {
        let html = r#"
            <html><body style="margin: 0; padding: 0">
              <div>
                <div style="height: 100px"></div>
                <div style="height: 0px; break-after: page"></div>
                <div style="height: 100px"></div>
              </div>
            </body></html>
        "#;
        let mut doc = parse(html, 600.0);
        let table = blitz_adapter::extract_column_style_table(&doc);
        let geom = super::run_pass_with_break_styles(doc.deref_mut(), 800.0_f32.as_px(), &table);
        assert!(
            geom.values()
                .flat_map(|g| g.fragments.iter())
                .any(|f| f.page_index == 1),
            "break-after: page on a zero-height nested child must advance to page 1; \
             geom={geom:?}"
        );
    }

    // ── record_subtree_walk: position:fixed skip (line 3386) ────────────────────

    /// A `position: fixed` child inside an abs subtree must be skipped by
    /// `record_subtree_fragments_at_offset`'s walk (line 3386 `continue`).
    /// The fixed pass handles those separately; the abs walk must not emit them.
    #[test]
    fn record_subtree_walk_skips_position_fixed_child() {
        let html = r#"
            <html><body style="margin: 0">
              <div id="abs_root" style="position: absolute; top: 0; left: 0; width: 200px; height: 200px">
                <div id="fixed_child" style="position: fixed; top: 10px; left: 10px; width: 50px; height: 50px"></div>
              </div>
            </body></html>
        "#;
        let mut doc = parse(html, 600.0);
        let abs_id = find_by_id(doc.deref_mut(), "abs_root").expect("abs_root");
        let fixed_id = find_by_id(doc.deref_mut(), "fixed_child").expect("fixed_child");
        let mut geom = PaginationGeometryTable::new();
        super::record_subtree_fragments_at_offset(
            &mut geom,
            doc.deref_mut(),
            abs_id,
            (0.0, 0.0),
            0.0,
            800.0,
            800.0,
            1,
            false,
            &mut 0,
        );
        assert!(
            !geom.contains_key(&fixed_id),
            "position:fixed child must not be emitted by the abs subtree walk; \
             geom keys={:?}",
            geom.keys().collect::<Vec<_>>()
        );
    }

    // ── record_subtree_walk: position:absolute recursion (lines 3387-3441) ──────

    /// A `position: absolute` child nested inside an abs subtree must be
    /// recursively walked and appear in geometry (lines 3387-3441).
    #[test]
    fn record_subtree_walk_recurses_into_nested_position_absolute_child() {
        let html = r#"
            <html><body style="margin: 0">
              <div id="abs_root" style="position: absolute; top: 0; left: 0; width: 200px; height: 200px">
                <div id="nested_abs" style="position: absolute; top: 10px; left: 10px; width: 80px; height: 80px"></div>
              </div>
            </body></html>
        "#;
        let mut doc = parse(html, 600.0);
        let abs_id = find_by_id(doc.deref_mut(), "abs_root").expect("abs_root");
        let nested_id = find_by_id(doc.deref_mut(), "nested_abs").expect("nested_abs");
        let mut geom = PaginationGeometryTable::new();
        super::record_subtree_fragments_at_offset(
            &mut geom,
            doc.deref_mut(),
            abs_id,
            (0.0, 0.0),
            0.0,
            800.0,
            800.0,
            1,
            false,
            &mut 0,
        );
        assert!(
            geom.contains_key(&nested_id),
            "position:absolute child must be recursively walked and appear in geometry; \
             geom keys={:?}",
            geom.keys().collect::<Vec<_>>()
        );
    }

    // ── F12: repeating the table header group on continuation pages ─────────

    /// A table of `rows` body rows of `row_h` px under a one-row header of
    /// `head_h` px, with no padding / spacing so geometry is exact. Header
    /// cells are `h0` / `h1`; body cells are `r{i}a` / `r{i}b`.
    fn thead_table_html(head_h: f32, rows: usize, row_h: f32) -> String {
        let body: String = (0..rows)
            .map(|i| {
                format!(
                    r#"<tr><td id="r{i}a" style="height:{row_h}px;padding:0">x</td>
                       <td id="r{i}b" style="padding:0">y</td></tr>"#
                )
            })
            .collect();
        format!(
            r#"<html><body style="margin:0"><table style="border-spacing:0;width:200px">
                 <thead><tr><th id="h0" style="height:{head_h}px;padding:0">a</th>
                            <th id="h1" style="padding:0">b</th></tr></thead>
                 <tbody>{body}</tbody></table></body></html>"#
        )
    }

    fn table_geometry(
        html: &str,
        page_h: f32,
    ) -> (blitz_html::HtmlDocument, PaginationGeometryTable) {
        let mut doc = parse(html, 600.0);
        let geom = super::run_pass(doc.deref_mut(), page_h);
        (doc, geom)
    }

    fn frag_on_page(geom: &PaginationGeometryTable, id: usize, page: u32) -> Option<&Fragment> {
        geom.get(&id)?
            .fragments
            .iter()
            .find(|f| f.page_index == page)
    }

    #[test]
    fn table_header_has_one_fragment_per_page() {
        // 10 rows x 40px under a 30px header on 200px pages: 4 rows fit the
        // first page (30 + 4*40 = 190), then 4 per continuation page
        // (30 + 4*40), so 3 pages.
        let (doc, geom) = table_geometry(&thead_table_html(30.0, 10, 40.0), 200.0);
        let thead = find_node_by_local_name(&doc, "thead").unwrap();
        let group = geom.get(&thead).expect("thead has geometry");
        let pages: Vec<u32> = group.fragments.iter().map(|f| f.page_index).collect();
        assert_eq!(pages, vec![0, 1, 2], "one header fragment per page");
        assert!(group.is_repeat, "repeat fragments must not read as a split");
        assert!(!group.is_split());
        for f in &group.fragments[1..] {
            assert_eq!(f.y.to_f32(), 0.0, "repeat sits at the page top");
            assert!(
                (f.height.to_f32() - 30.0).abs() < 0.5,
                "band = header height"
            );
        }
        // The first fragment is where the header was laid out.
        assert_eq!(group.fragments[0].y.to_f32(), 0.0);
    }

    #[test]
    fn table_first_body_cell_starts_below_the_repeated_header() {
        let (doc, geom) = table_geometry(&thead_table_html(30.0, 10, 40.0), 200.0);
        // Row 4 is the first row of page 1, row 8 of page 2.
        for (row, page) in [(4, 1), (8, 2)] {
            let id = find_by_id(doc.deref(), &format!("r{row}a")).unwrap();
            let f =
                frag_on_page(&geom, id, page).unwrap_or_else(|| panic!("r{row}a on page {page}"));
            assert!(
                (f.y.to_f32() - 30.0).abs() < 0.5,
                "row {row} must start at the header height, got y={}",
                f.y.to_f32()
            );
        }
        // Page 0 is unaffected: the first body row follows the header in place.
        let r0 = find_by_id(doc.deref(), "r0a").unwrap();
        assert!((frag_on_page(&geom, r0, 0).unwrap().y.to_f32() - 30.0).abs() < 0.5);
    }

    #[test]
    fn table_body_cells_never_overflow_a_page_with_a_repeated_header() {
        let page_h = 200.0;
        let (doc, geom) = table_geometry(&thead_table_html(30.0, 25, 37.0), page_h);
        let mut seen = 0;
        for i in 0..25 {
            let id = find_by_id(doc.deref(), &format!("r{i}a")).unwrap();
            let frags = &geom[&id].fragments;
            assert_eq!(frags.len(), 1, "row {i} is placed exactly once");
            let f = &frags[0];
            assert!(
                f.y.to_f32() + f.height.to_f32() <= page_h + 0.5,
                "row {i} overflows page {}: y={} h={}",
                f.page_index,
                f.y.to_f32(),
                f.height.to_f32()
            );
            if f.page_index > 0 {
                assert!(f.y.to_f32() >= 30.0 - 0.5, "row {i} sits under the band");
            }
            seen += 1;
        }
        assert_eq!(seen, 25);
        // Rows on one page never overlap.
        let mut by_page: BTreeMap<u32, Vec<(f32, f32)>> = BTreeMap::new();
        for i in 0..25 {
            let id = find_by_id(doc.deref(), &format!("r{i}a")).unwrap();
            let f = &geom[&id].fragments[0];
            by_page
                .entry(f.page_index)
                .or_default()
                .push((f.y.to_f32(), f.y.to_f32() + f.height.to_f32()));
        }
        for (page, rows) in by_page {
            for w in rows.windows(2) {
                assert!(w[0].1 <= w[1].0 + 0.5, "page {page}: rows overlap {w:?}");
            }
        }
    }

    #[test]
    fn table_header_taller_than_a_quarter_page_is_not_repeated() {
        // 60px > 200/4.
        let (doc, geom) = table_geometry(&thead_table_html(60.0, 10, 40.0), 200.0);
        let thead = find_node_by_local_name(&doc, "thead").unwrap();
        assert!(
            !geom.contains_key(&thead),
            "no geometry for the header group when it is not repeated"
        );
        // Continuation pages start with body rows at the top, as before:
        // 60 + 3*40 = 180 fits page 0, so row 3 opens page 1 at y = 0.
        let id = find_by_id(doc.deref(), "r3a").unwrap();
        let f = &geom[&id].fragments[0];
        assert_eq!(f.page_index, 1);
        assert!(f.y.to_f32().abs() < 0.5, "got y={}", f.y.to_f32());
    }

    #[test]
    fn table_header_exactly_a_quarter_page_is_repeated() {
        let (doc, geom) = table_geometry(&thead_table_html(50.0, 10, 40.0), 200.0);
        let thead = find_node_by_local_name(&doc, "thead").unwrap();
        assert!(geom.get(&thead).is_some_and(|g| g.is_repeat));
    }

    #[test]
    fn table_with_tbody_before_thead_repeats_its_header() {
        // F36: Blitz lays the header group out first wherever it is written
        // (CSS 2.1 §17.2), so its cells are a prefix of `layout_children`
        // and F12 repeats it like any other header.
        let rows: String = (0..10)
            .map(|i| format!(r#"<tr><td id="r{i}" style="height:40px;padding:0">x</td></tr>"#))
            .collect();
        let html = format!(
            r#"<html><body style="margin:0"><table style="border-spacing:0;width:200px">
                 <tbody>{rows}</tbody>
                 <thead><tr><th style="height:20px;padding:0">h</th></tr></thead>
               </table></body></html>"#
        );
        let (doc, geom) = table_geometry(&html, 200.0);
        let thead = find_node_by_local_name(&doc, "thead").unwrap();
        let group = geom.get(&thead).expect("thead has geometry");
        let pages: Vec<u32> = group.fragments.iter().map(|f| f.page_index).collect();
        assert_eq!(pages, vec![0, 1, 2], "one header fragment per page");
        assert!(group.is_repeat);
        // The header is on top of page 0, above the first row.
        let first = frag_on_page(&geom, find_by_id(doc.deref(), "r0").unwrap(), 0).unwrap();
        assert!(first.y.to_f32() >= 20.0 - 0.5, "r0 is below the header");
    }

    #[test]
    fn table_footer_written_first_is_last_in_layout_children_and_on_the_last_page() {
        // F36: `<thead>, <tfoot>, <tbody>` (HTML 4.01 order). The footer cells
        // are a suffix of the table's `layout_children` (the mirror of F12's
        // header prefix), and their fragments are on the last page, below
        // the last body cell.
        let body: String = (0..10)
            .map(|i| {
                format!(
                    r#"<tr><td id="r{i}a" style="height:40px;padding:0">x</td>
                       <td id="r{i}b" style="padding:0">y</td></tr>"#
                )
            })
            .collect();
        let html = format!(
            r#"<html><body style="margin:0"><table style="border-spacing:0;width:200px">
                 <thead><tr><th id="h0" style="height:20px;padding:0">a</th>
                            <th id="h1" style="padding:0">b</th></tr></thead>
                 <tfoot><tr><td id="f0" style="height:30px;padding:0">t</td>
                            <td id="f1" style="padding:0">u</td></tr></tfoot>
                 <tbody>{body}</tbody></table></body></html>"#
        );
        let (doc, geom) = table_geometry(&html, 200.0);
        let id = |name: &str| find_by_id(doc.deref(), name).unwrap();
        let table = find_node_by_local_name(&doc, "table").unwrap();
        let children = doc
            .get_node(table)
            .unwrap()
            .layout_children
            .borrow()
            .clone();
        let children = children.expect("a table has layout children");
        assert_eq!(&children[..2], &[id("h0"), id("h1")], "header cells first");
        assert_eq!(
            &children[children.len() - 2..],
            &[id("f0"), id("f1")],
            "footer cells last"
        );
        let last_body = geom.get(&id("r9a")).unwrap().fragments.last().unwrap();
        let footer = geom.get(&id("f0")).unwrap();
        assert_eq!(footer.fragments.len(), 1, "the footer is printed once");
        let footer = &footer.fragments[0];
        assert_eq!(footer.page_index, last_body.page_index, "last page");
        assert!(
            footer.y.to_f32() >= last_body.y.to_f32() + 40.0 - 0.5,
            "the footer is below the last body row"
        );
    }

    #[test]
    fn table_header_group_is_chosen_by_display_not_tag() {
        // `<thead>` demoted to a row group, a `<tbody>` promoted to header.
        let html = r#"<html><body style="margin:0"><table style="border-spacing:0;width:200px">
              <tbody id="promoted" style="display:table-header-group">
                <tr><td style="height:20px;padding:0">h</td></tr></tbody>
              <thead id="demoted" style="display:table-row-group">
                <tr><td style="height:20px;padding:0">x</td></tr></thead>
              <tbody><tr><td style="height:300px;padding:0">b</td></tr></tbody>
            </table></body></html>"#;
        let doc = parse(html, 600.0);
        let promoted = find_by_id(doc.deref(), "promoted").unwrap();
        assert_eq!(
            crate::blitz_adapter::first_table_header_group(
                doc.deref(),
                find_node_by_local_name(&doc, "table").unwrap()
            ),
            Some(promoted)
        );
    }

    #[test]
    fn table_row_between_page_minus_header_and_page_gets_the_whole_page() {
        // Header 30px, page 200px, a 190px row: it fits a page (190 <= 200)
        // but not below the band (30 + 190 > 200). It must get a page of its
        // own, whole and without a header, not be split or pushed on.
        let rows = r#"<tr><td id="big" style="height:190px;padding:0">x</td><td style="padding:0">y</td></tr>
                      <tr><td id="after" style="height:20px;padding:0">z</td><td style="padding:0">w</td></tr>"#;
        let html = format!(
            r#"<html><body style="margin:0"><table style="border-spacing:0;width:200px">
                 <thead><tr><th style="height:30px;padding:0">a</th><th style="padding:0">b</th></tr></thead>
                 <tbody><tr><td id="first" style="height:100px;padding:0">q</td><td style="padding:0">r</td></tr>{rows}</tbody>
               </table></body></html>"#
        );
        let (doc, geom) = table_geometry(&html, 200.0);
        let big = find_by_id(doc.deref(), "big").unwrap();
        let after = find_by_id(doc.deref(), "after").unwrap();
        let fb = &geom[&big].fragments;
        assert_eq!(fb.len(), 1, "the tall row is placed once");
        assert_eq!(fb[0].page_index, 1);
        assert!(
            fb[0].y.to_f32().abs() < 0.5,
            "no band on its page, got y={}",
            fb[0].y.to_f32()
        );
        // The next row follows on page 2 under a repeated header.
        let fa = &geom[&after].fragments;
        assert_eq!(fa[0].page_index, 2);
        assert!((fa[0].y.to_f32() - 30.0).abs() < 0.5);
        let thead = find_node_by_local_name(&doc, "thead").unwrap();
        let pages: Vec<u32> = geom[&thead]
            .fragments
            .iter()
            .map(|f| f.page_index)
            .collect();
        assert_eq!(
            pages,
            vec![0, 2],
            "no header fragment on the tall row's page"
        );
    }

    #[test]
    fn table_row_taller_than_a_page_is_placed_once_without_band() {
        let html = r#"<html><body style="margin:0"><table style="border-spacing:0;width:200px">
                 <thead><tr><th style="height:30px;padding:0">a</th></tr></thead>
                 <tbody><tr><td id="first" style="height:100px;padding:0">q</td></tr>
                 <tr><td id="huge" style="height:250px;padding:0">x</td></tr>
                 <tr><td id="after" style="height:20px;padding:0">z</td></tr></tbody>
               </table></body></html>"#;
        let (doc, geom) = table_geometry(html, 200.0);
        let huge = find_by_id(doc.deref(), "huge").unwrap();
        let f = &geom[&huge].fragments;
        assert_eq!(f.len(), 1);
        assert_eq!(f[0].page_index, 1);
        assert!(f[0].y.to_f32().abs() < 0.5);
    }

    #[test]
    fn display_table_on_a_non_table_element_is_not_repeated() {
        // convert records header drawables only for `<table>` elements, so a
        // band reserved for a `display: table` div would stay blank.
        let rows: String = (0..10)
            .map(|i| {
                format!(
                    r#"<div style="display:table-row"><div id="r{i}" style="display:table-cell;height:40px">x</div></div>"#
                )
            })
            .collect();
        let html = format!(
            r#"<html><body style="margin:0"><div style="display:table;width:200px">
                 <div id="hg" style="display:table-header-group"><div style="display:table-row">
                   <div style="display:table-cell;height:30px">h</div></div></div>
                 <div style="display:table-row-group">{rows}</div></div></body></html>"#
        );
        let (doc, geom) = table_geometry(&html, 200.0);
        let hg = find_by_id(doc.deref(), "hg").unwrap();
        assert!(
            !geom.contains_key(&hg),
            "no header repeat for a display:table div"
        );
    }

    #[test]
    fn table_header_orphaned_at_the_bottom_of_a_page_is_not_repeated() {
        // A 160px spacer, then a 30px header ending at 190: the first 40px row
        // does not fit page 0, so the header is the last thing on it. The
        // table keeps its pre-F12 layout: the next page starts with the row.
        let rows: String = (0..6)
            .map(|i| format!(r#"<tr><td id="r{i}" style="height:40px;padding:0">x</td></tr>"#))
            .collect();
        let html = format!(
            r#"<html><body style="margin:0"><div style="height:160px"></div>
               <table style="border-spacing:0;width:200px">
                 <thead><tr><th style="height:30px;padding:0">h</th></tr></thead>
                 <tbody>{rows}</tbody></table></body></html>"#
        );
        let (doc, geom) = table_geometry(&html, 200.0);
        let thead = find_node_by_local_name(&doc, "thead").unwrap();
        assert!(
            !geom.contains_key(&thead),
            "orphan header must not be repeated"
        );
        let r0 = find_by_id(doc.deref(), "r0").unwrap();
        let f = &geom[&r0].fragments[0];
        assert_eq!(f.page_index, 1);
        assert!(f.y.to_f32().abs() < 0.5, "got y={}", f.y.to_f32());
        // Later pages are not given a header either.
        let r5 = find_by_id(doc.deref(), "r5").unwrap();
        assert!(geom[&r5].fragments[0].page_index >= 1);
    }

    #[test]
    fn table_that_fits_one_page_leaves_header_group_out_of_geometry() {
        let (doc, geom) = table_geometry(&thead_table_html(30.0, 3, 40.0), 400.0);
        let thead = find_node_by_local_name(&doc, "thead").unwrap();
        assert!(!geom.contains_key(&thead));
    }

    #[test]
    fn table_with_overflow_hidden_is_not_repeated() {
        let html = thead_table_html(30.0, 10, 40.0)
            .replace("<table style=\"", "<table style=\"overflow:hidden;");
        let (doc, geom) = table_geometry(&html, 200.0);
        let thead = find_node_by_local_name(&doc, "thead").unwrap();
        assert!(!geom.contains_key(&thead));
    }

    #[test]
    fn table_header_repeat_is_ignored_for_non_tables() {
        let doc = parse(
            r#"<html><body><div id="d"><p>x</p></div></body></html>"#,
            600.0,
        );
        let d = find_by_id(doc.deref(), "d").unwrap();
        assert_eq!(
            crate::blitz_adapter::first_table_header_group(doc.deref(), d),
            None
        );
    }

    // ── F28: boxes whose first row sits at their top edge ───────────────────

    fn leading_of(html: &str, id: &str) -> f32 {
        let doc = parse(html, 600.0);
        let node = find_by_id(doc.deref(), id).unwrap();
        super::leading_block_size(doc.deref(), node, None, 0.0, 0, 10_000.0)
    }

    #[test]
    fn leading_block_size_of_a_leaf_is_its_height() {
        let html =
            r#"<html><body style="margin:0"><div id="a" style="height:37px">x</div></body></html>"#;
        assert!((leading_of(html, "a") - 37.0).abs() < 0.5);
    }

    #[test]
    fn leading_block_size_of_a_table_is_its_first_row() {
        let html = r#"<html><body style="margin:0"><table id="t" style="border-spacing:0;width:200px">
              <tr><td style="height:30px;padding:0">a</td><td style="height:50px;padding:0">b</td></tr>
              <tr><td style="height:40px;padding:0">c</td><td style="padding:0">d</td></tr></table></body></html>"#;
        let leading = leading_of(html, "t");
        assert!(
            (leading - 50.0).abs() < 0.5,
            "first row = tallest cell of row 1, got {leading}"
        );
    }

    #[test]
    fn leading_block_size_sees_through_gapless_wrappers() {
        let html = r#"<html><body style="margin:0"><div id="o"><div><div style="height:20px">a</div>
              <div style="height:90px">b</div></div><div style="height:100px">c</div></div></body></html>"#;
        let leading = leading_of(html, "o");
        assert!(
            (leading - 20.0).abs() < 0.5,
            "first leaf of the first chain, got {leading}"
        );
    }

    #[test]
    fn leading_block_size_stops_at_a_gap_before_the_first_child() {
        // Class C break point: the child's margin did not collapse through the
        // padded parent, so there is a gap between the content edge and it.
        let html = r#"<html><body style="margin:0"><div id="o" style="padding-top:6px">
              <div style="margin-top:10px;height:80px">a</div></div></body></html>"#;
        let leading = leading_of(html, "o");
        assert!((leading - 16.0).abs() < 0.5, "got {leading}");
    }

    #[test]
    fn leading_block_size_does_not_treat_border_or_padding_as_a_gap() {
        // Border and padding belong to the box; with no gap behind them the
        // first child cannot start a page on its own.
        let html = r#"<html><body style="margin:0"><div id="o" style="border-top:5px solid #000;padding-top:6px">
              <div style="height:80px">a</div></div></body></html>"#;
        let leading = leading_of(html, "o");
        assert!((leading - 91.0).abs() < 0.5, "got {leading}");
        let table = r#"<html><body style="margin:0"><table id="t" style="border-collapse:collapse;border:1px solid #333">
              <tr><td style="height:30px;padding:0">a</td></tr><tr><td style="height:40px;padding:0">b</td></tr></table></body></html>"#;
        let leading = leading_of(table, "t");
        assert!(
            (30.0..=32.0).contains(&leading),
            "first row plus the border, got {leading}"
        );
    }

    #[test]
    fn leading_block_size_of_an_inline_root_is_not_cut_short_by_where_its_line_puts_a_box() {
        // A 10 px image sits on the baseline, so it starts below the top of its line. That offset is not a margin: the
        // first line box is one unit, and the box needs the line (html-to-pdf F183).
        let html = r#"<html><body style="margin:0;font:16px sans-serif"><div id="o"><img width="10" height="10"></div></body></html>"#;
        let doc = parse(html, 600.0);
        let o = find_by_id(doc.deref(), "o").unwrap();
        let own = doc.get_node(o).unwrap().final_layout.size.height;
        let leading = leading_of(html, "o");
        assert!(own > 10.0, "the line is taller than the image: {own}");
        assert!(leading >= 10.0 + 0.5, "got {leading} of {own}");
    }

    #[test]
    fn leading_block_size_of_a_flex_row_is_its_tallest_item() {
        let html = r#"<html><body style="margin:0"><div id="f" style="display:flex">
              <div style="height:30px;width:50px">a</div><div style="height:45px;width:50px">b</div></div>
              <div style="height:100px">after</div></body></html>"#;
        let leading = leading_of(html, "f");
        assert!((leading - 45.0).abs() < 0.5, "got {leading}");
    }

    #[test]
    fn leading_block_size_never_exceeds_the_node_height() {
        let html = r#"<html><body style="margin:0"><div id="o" style="height:10px">
              <div style="height:90px">a</div></div></body></html>"#;
        assert!(leading_of(html, "o") <= 10.5);
    }

    /// Every `ids` entry has exactly one fragment, inside its page, and the
    /// pages never go backwards in document order.
    fn assert_placed_once_inside_pages(
        doc: &blitz_html::HtmlDocument,
        geom: &PaginationGeometryTable,
        ids: &[String],
        page_h: f32,
    ) -> Vec<u32> {
        let mut pages = Vec::new();
        for id in ids {
            let node = find_by_id(doc.deref(), id).unwrap_or_else(|| panic!("no #{id}"));
            let frags = &geom
                .get(&node)
                .unwrap_or_else(|| panic!("#{id} has no geometry"))
                .fragments;
            assert_eq!(frags.len(), 1, "#{id} is placed exactly once: {frags:?}");
            let f = &frags[0];
            assert!(
                f.y.to_f32() + f.height.to_f32() <= page_h + 0.5,
                "#{id} hangs below page {}: y={} h={}",
                f.page_index,
                f.y.to_f32(),
                f.height.to_f32()
            );
            pages.push(f.page_index);
        }
        assert!(pages.windows(2).all(|w| w[0] <= w[1]), "pages {pages:?}");
        pages
    }

    fn ids(prefix: &str, n: usize) -> Vec<String> {
        (0..n).map(|i| format!("{prefix}{i}")).collect()
    }

    #[test]
    fn consecutive_one_row_tables_each_land_on_one_page() {
        // 15 tables of one 30px row on 200px pages: 6 per page.
        let tables: String = (0..15)
            .map(|i| {
                format!(
                    r#"<table id="t{i}" style="border-spacing:0;width:100px">
                         <tr><td id="c{i}" style="height:30px;padding:0">x</td></tr></table>"#
                )
            })
            .collect();
        let html = format!(r#"<html><body style="margin:0">{tables}</body></html>"#);
        let (doc, geom) = table_geometry(&html, 200.0);
        let cells = assert_placed_once_inside_pages(&doc, &geom, &ids("c", 15), 200.0);
        assert_eq!(cells[5], 0);
        assert_eq!(cells[6], 1, "row 7 opens page 2");
        assert_eq!(*cells.last().unwrap(), 2);
        // A table that moved has no fragment left on the page it left.
        let tabs = assert_placed_once_inside_pages(&doc, &geom, &ids("t", 15), 200.0);
        assert_eq!(tabs, cells, "each table sits on its cell's page");
        assert_eq!(implied_page_count(&geom), 3);
    }

    #[test]
    fn consecutive_one_row_tables_inside_a_div_each_land_on_one_page() {
        let tables: String = (0..15)
            .map(|i| {
                format!(
                    r#"<table id="t{i}" style="border-spacing:0;width:100px">
                         <tr><td id="c{i}" style="height:30px;padding:0">x</td></tr></table>"#
                )
            })
            .collect();
        let html =
            format!(r#"<html><body style="margin:0"><div id="w">{tables}</div></body></html>"#);
        let (doc, geom) = table_geometry(&html, 200.0);
        let cells = assert_placed_once_inside_pages(&doc, &geom, &ids("c", 15), 200.0);
        assert_eq!(cells[6], 1);
        let tabs = assert_placed_once_inside_pages(&doc, &geom, &ids("t", 15), 200.0);
        assert_eq!(tabs, cells);
        // The wrapper has one fragment per page it spans.
        let w = find_by_id(doc.deref(), "w").unwrap();
        let wp: Vec<u32> = geom[&w].fragments.iter().map(|f| f.page_index).collect();
        assert_eq!(wp, vec![0, 1, 2]);
    }

    #[test]
    fn consecutive_nested_divs_each_land_on_one_page() {
        let rows: String = (0..15)
            .map(|i| format!(r#"<div><div><div id="c{i}" style="height:30px">x</div></div></div>"#))
            .collect();
        let html = format!(r#"<html><body style="margin:0">{rows}</body></html>"#);
        let (doc, geom) = table_geometry(&html, 200.0);
        let cells = assert_placed_once_inside_pages(&doc, &geom, &ids("c", 15), 200.0);
        assert_eq!((cells[5], cells[6], cells[14]), (0, 1, 2));
    }

    #[test]
    fn consecutive_flex_and_grid_rows_each_land_on_one_page() {
        for display in ["flex", "grid"] {
            let rows: String = (0..15)
                .map(|i| {
                    format!(
                        r#"<div style="display:{display};grid-template-columns:80px 80px"><div id="c{i}" style="height:30px;width:80px">x</div>
                             <div id="d{i}" style="height:20px;width:80px">y</div></div>"#
                    )
                })
                .collect();
            let html = format!(r#"<html><body style="margin:0"><div>{rows}</div></body></html>"#);
            let (doc, geom) = table_geometry(&html, 200.0);
            let cells = assert_placed_once_inside_pages(&doc, &geom, &ids("c", 15), 200.0);
            let mates = assert_placed_once_inside_pages(&doc, &geom, &ids("d", 15), 200.0);
            assert_eq!(cells, mates, "{display}: items of a row stay together");
            assert_eq!((cells[5], cells[6], cells[14]), (0, 1, 2), "{display}");
        }
    }

    #[test]
    fn table_with_room_for_one_of_two_rows_splits_between_them() {
        let html = r#"<html><body style="margin:0"><div style="height:130px"></div>
            <table id="t" style="border-spacing:0;width:100px">
              <tr><td id="a" style="height:40px;padding:0">x</td></tr>
              <tr><td id="b" style="height:40px;padding:0">y</td></tr></table></body></html>"#;
        let (doc, geom) = table_geometry(html, 200.0);
        let a = find_by_id(doc.deref(), "a").unwrap();
        let b = find_by_id(doc.deref(), "b").unwrap();
        assert_eq!(geom[&a].fragments[0].page_index, 0);
        assert_eq!(geom[&b].fragments[0].page_index, 1);
        assert!(geom[&b].fragments[0].y.to_f32().abs() < 0.5);
        let t = find_by_id(doc.deref(), "t").unwrap();
        let tp: Vec<u32> = geom[&t].fragments.iter().map(|f| f.page_index).collect();
        assert_eq!(tp, vec![0, 1], "the table frame continues on page 2");
    }

    #[test]
    fn table_with_no_room_for_its_first_row_moves_whole() {
        let html = r#"<html><body style="margin:0"><div style="height:170px"></div>
            <table id="t" style="border-spacing:0;width:100px">
              <tr><td id="a" style="height:40px;padding:0">x</td></tr>
              <tr><td id="b" style="height:40px;padding:0">y</td></tr></table></body></html>"#;
        let (doc, geom) = table_geometry(html, 200.0);
        let t = find_by_id(doc.deref(), "t").unwrap();
        assert_eq!(
            geom[&t].fragments.len(),
            1,
            "no empty table frame is left on page 1: {:?}",
            geom[&t].fragments
        );
        assert_eq!(geom[&t].fragments[0].page_index, 1);
        assert!(geom[&t].fragments[0].y.to_f32().abs() < 0.5);
        for id in ["a", "b"] {
            let n = find_by_id(doc.deref(), id).unwrap();
            assert_eq!(geom[&n].fragments[0].page_index, 1, "#{id}");
        }
    }

    #[test]
    fn table_taller_than_a_page_with_no_room_for_its_first_row_starts_a_page() {
        // 12 rows of 40px (480px) on 200px pages after a 170px spacer: the
        // first row cannot fit under the spacer, so the table starts page 2
        // and then paginates 5 rows per page.
        let rows: String = (0..12)
            .map(|i| format!(r#"<tr><td id="r{i}" style="height:40px;padding:0">x</td></tr>"#))
            .collect();
        let html = format!(
            r#"<html><body style="margin:0"><div style="height:170px"></div>
               <table style="border-spacing:0;width:100px">{rows}</table></body></html>"#
        );
        let (doc, geom) = table_geometry(&html, 200.0);
        let pages = assert_placed_once_inside_pages(&doc, &geom, &ids("r", 12), 200.0);
        assert_eq!(pages[0], 1);
        assert_eq!(pages[4], 1);
        assert_eq!(pages[5], 2);
        assert_eq!(pages[11], 3);
    }

    #[test]
    fn a_gap_before_the_first_row_still_breaks_inside_the_wrapper() {
        // Class C break point: the first child's 10px margin does not collapse
        // through the wrapper's 10px padding, so the gap (20px) fits under the
        // 170px spacer and the 40px child does not. The child moves alone and
        // the wrapper's frame continues on the next page.
        let html = r#"<html><body style="margin:0"><div style="height:170px"></div>
            <div id="w" style="padding-top:10px">
              <div id="a" style="height:40px;margin-top:10px">x</div><div id="b" style="height:40px">y</div></div>
            </body></html>"#;
        let (doc, geom) = table_geometry(html, 200.0);
        let w = find_by_id(doc.deref(), "w").unwrap();
        let wp: Vec<u32> = geom[&w].fragments.iter().map(|f| f.page_index).collect();
        assert_eq!(wp, vec![0, 1]);
        let a = find_by_id(doc.deref(), "a").unwrap();
        assert_eq!(geom[&a].fragments[0].page_index, 1);
        assert!(geom[&a].fragments[0].y.to_f32().abs() < 0.5);
    }

    #[test]
    fn flex_row_recursive_cells_with_a_fitting_first_line_still_cosplit() {
        // The first inner block fits under the spacer, the second does not:
        // both cells of the row split at the same place.
        let html = r#"<html><body style="margin:0"><div style="height:50px"></div>
            <div style="display:flex;width:200px">
              <div style="width:100px"><div id="a1" style="height:40px">a</div><div id="a2" style="height:40px">b</div></div>
              <div style="width:100px"><div id="b1" style="height:40px">c</div><div id="b2" style="height:40px">d</div></div>
            </div></body></html>"#;
        let (doc, geom) = table_geometry(html, 100.0);
        let page_of = |id: &str| {
            let n = find_by_id(doc.deref(), id).unwrap();
            geom[&n].fragments[0].page_index
        };
        assert_eq!((page_of("a1"), page_of("b1")), (0, 0));
        assert_eq!((page_of("a2"), page_of("b2")), (1, 1));
    }

    #[test]
    fn grid_row_that_moves_to_the_next_page_still_cosplits_when_it_is_taller_than_a_page() {
        // Row 1 is 70px; the first block of row 2 does not fit under it, so
        // the first cell of row 2 starts page 1 (F28). Each cell of row 2 is
        // 120px and splits between its blocks. The second cell must restart
        // from the row's new start on page 1, not from page 0.
        let cell = |p: &str| {
            format!(
                r#"<div><div id="{p}1" style="height:40px">a</div>
                   <div id="{p}2" style="height:40px">b</div><div id="{p}3" style="height:40px">c</div></div>"#
            )
        };
        let html = format!(
            r#"<html><body style="margin:0">
               <div style="display:grid;grid-template-columns:100px 100px;width:200px">
                 <div style="height:70px">x</div><div style="height:70px">y</div>{}{}</div></body></html>"#,
            cell("a"),
            cell("b")
        );
        let (doc, geom) = table_geometry(&html, 100.0);
        let place = |id: &str| {
            let n = find_by_id(doc.deref(), id).unwrap();
            let f = &geom[&n].fragments[0];
            (f.page_index, f.y.to_f32())
        };
        for p in ["a", "b"] {
            assert_eq!(place(&format!("{p}1")), (1, 0.0), "{p}1");
            assert_eq!(place(&format!("{p}2")), (1, 40.0), "{p}2");
            assert_eq!(place(&format!("{p}3")), (2, 0.0), "{p}3");
        }
    }

    #[test]
    fn first_row_taller_than_a_page_is_not_pushed_to_a_fresh_page() {
        // The 300px block cannot fit a 200px page anywhere, so moving its
        // wrapper to page 1 would only waste the rest of page 0 (F28 follow-up).
        let html = r#"<html><body style="margin:0"><div style="height:150px"></div>
            <div><div id="big" style="height:300px">x</div><p style="margin:0;height:20px">y</p></div>
            </body></html>"#;
        let (doc, geom) = table_geometry(html, 200.0);
        let big = find_by_id(doc.deref(), "big").unwrap();
        assert_eq!(geom[&big].fragments[0].page_index, 0);
        assert_eq!(implied_page_count(&geom), 2);
    }

    #[test]
    fn nested_first_row_taller_than_a_page_is_not_cut_to_a_fresh_page() {
        let html = r#"<html><body style="margin:0"><div><div style="height:150px"></div>
            <div><div id="big" style="height:300px">x</div><p style="margin:0;height:20px">y</p></div>
            </div></body></html>"#;
        let (doc, geom) = table_geometry(html, 200.0);
        let big = find_by_id(doc.deref(), "big").unwrap();
        assert_eq!(geom[&big].fragments[0].page_index, 0);
        assert_eq!(implied_page_count(&geom), 2);
    }

    #[test]
    fn bordered_table_that_does_not_fit_moves_whole_without_a_frame_left_behind() {
        // The table's own border is not a class C gap: the first row cannot
        // start a page on its own, so the table moves as a whole and leaves no
        // empty frame on the page it left.
        let html = r#"<html><body style="margin:0"><div style="height:170px"></div>
            <table id="t" style="border-collapse:collapse;border:1px solid #333;width:100px">
              <tr><td id="a" style="height:40px;padding:0">x</td></tr>
              <tr><td id="b" style="height:40px;padding:0">y</td></tr></table></body></html>"#;
        let (doc, geom) = table_geometry(html, 200.0);
        let t = find_by_id(doc.deref(), "t").unwrap();
        assert_eq!(
            geom[&t].fragments.len(),
            1,
            "frame left behind: {:?}",
            geom[&t].fragments
        );
        assert_eq!(geom[&t].fragments[0].page_index, 1);
        let a = find_by_id(doc.deref(), "a").unwrap();
        assert_eq!(geom[&a].fragments[0].page_index, 1);
    }

    #[test]
    fn padded_wrapper_that_does_not_fit_moves_whole() {
        let html = r#"<html><body style="margin:0"><div style="height:170px"></div>
            <div id="w" style="padding-top:10px;border:1px solid #000">
              <div id="a" style="height:40px">x</div></div></body></html>"#;
        let (doc, geom) = table_geometry(html, 200.0);
        let w = find_by_id(doc.deref(), "w").unwrap();
        assert_eq!(geom[&w].fragments.len(), 1, "{:?}", geom[&w].fragments);
        assert_eq!(geom[&w].fragments[0].page_index, 1);
    }

    #[test]
    fn tables_under_display_contents_are_stacked_like_tables_outside_it() {
        // Blitz places the children of a `display: contents` wrapper as if they
        // were children of the wrapper's parent (css-display-3 §2.5), so the
        // tables are block-level boxes stacked one below the other. Before that
        // fix a wrapper that was the only child of `body` made `body` an inline
        // root and the tables sat side by side (seven to a row), and the cursor
        // had to be told not to advance for each of them. 60 tables of 21px on
        // pages of 400px: 19 fit a page, so 4 pages, every table placed once,
        // and the same pages as the document without the wrapper.
        let tables: String = (0..60)
            .map(|i| {
                format!(
                    r#"<table id="t{i}" style="border-spacing:0"><tr><td id="c{i}" style="height:21px;padding:0;width:80px">x</td></tr></table>"#
                )
            })
            .collect();
        let wrapped = format!(
            r#"<html><body style="margin:0"><div style="display:contents">{tables}</div></body></html>"#
        );
        let (doc, geom) = table_geometry(&wrapped, 400.0);
        let cells = assert_placed_once_inside_pages(&doc, &geom, &ids("c", 60), 400.0);
        assert_eq!(implied_page_count(&geom), 4);
        assert_eq!(cells.iter().filter(|&&p| p == 0).count(), 19, "{cells:?}");

        let plain = format!(r#"<html><body style="margin:0">{tables}</body></html>"#);
        let (plain_doc, plain_geom) = table_geometry(&plain, 400.0);
        let plain_cells =
            assert_placed_once_inside_pages(&plain_doc, &plain_geom, &ids("c", 60), 400.0);
        assert_eq!(cells, plain_cells);
    }

    /// `n` lines of 20px: `(0, 20), (20, 40), ...`.
    fn uniform_lines(n: usize) -> Vec<(f32, f32)> {
        (0..n)
            .map(|i| (i as f32 * 20.0, (i + 1) as f32 * 20.0))
            .collect()
    }

    fn ranges_of(n: usize, first_room: f32, page_room: f32) -> Vec<(usize, usize)> {
        plan_line_breaks(&uniform_lines(n), first_room, page_room, 0.0, usize::MAX)
            .into_iter()
            .map(|r| (r.start, r.end))
            .collect()
    }

    #[test]
    fn plan_line_breaks_keeps_everything_on_one_strip_when_it_fits() {
        assert_eq!(ranges_of(10, 200.0, 200.0), vec![(0, 10)]);
        assert_eq!(ranges_of(1, 5.0, 5.0), vec![(0, 1)]);
        assert!(ranges_of(0, 100.0, 100.0).is_empty());
    }

    #[test]
    fn plan_line_breaks_fills_each_strip_greedily() {
        // 5 lines per strip.
        assert_eq!(ranges_of(10, 100.0, 100.0), vec![(0, 5), (5, 10)]);
        // The first strip has less room than the later ones.
        assert_eq!(ranges_of(12, 60.0, 100.0), vec![(0, 3), (3, 8), (8, 12)]);
    }

    #[test]
    fn plan_line_breaks_pulls_a_line_back_to_avoid_a_widow() {
        // Greedy would be 5, 5, 1: the last strip gets WIDOWS lines.
        assert_eq!(ranges_of(11, 100.0, 100.0), vec![(0, 5), (5, 9), (9, 11)]);
    }

    #[test]
    fn plan_line_breaks_relaxes_widows_when_orphans_would_break() {
        // 3 lines, room for 2: pulling back would leave 1 line on the first strip.
        assert_eq!(ranges_of(3, 40.0, 40.0), vec![(0, 2), (2, 3)]);
    }

    #[test]
    fn plan_line_breaks_takes_at_least_one_line_per_strip() {
        // Not even one line fits: it overflows rather than loop or vanish.
        assert_eq!(ranges_of(3, 5.0, 5.0), vec![(0, 1), (1, 2), (2, 3)]);
        assert_eq!(
            ranges_of(4, -50.0, 0.0),
            vec![(0, 1), (1, 2), (2, 3), (3, 4)]
        );
    }

    #[test]
    fn plan_line_breaks_uses_line_coordinates_not_counts() {
        // A tall line (60px) among 20px lines: strips are measured, not counted.
        let lines = vec![(0.0, 20.0), (20.0, 80.0), (80.0, 100.0), (100.0, 120.0)];
        let r: Vec<_> = plan_line_breaks(&lines, 90.0, 90.0, 0.0, usize::MAX)
            .into_iter()
            .map(|r| (r.start, r.end))
            .collect();
        assert_eq!(r, vec![(0, 2), (2, 4)]);
    }

    #[test]
    fn plan_line_breaks_stops_at_max_strips() {
        let r = plan_line_breaks(&uniform_lines(100), 20.0, 20.0, 0.0, 3);
        assert_eq!(r.len(), 3);
        assert_eq!(r.last().unwrap().end, 100, "the last strip takes the rest");
        let r = plan_line_breaks(&uniform_lines(5), 20.0, 20.0, 0.0, 0);
        assert_eq!(r, vec![0..5]);
    }

    // ── F30: a paragraph taller than a page inside a block ──────────────────

    /// `n` lines of 20px (`<br>`-separated, so the count does not depend on
    /// font metrics) as the body of a paragraph.
    fn br_lines(n: usize) -> String {
        (1..=n)
            .map(|i| format!("L{i}"))
            .collect::<Vec<_>>()
            .join("<br>")
    }

    const LINE_CSS: &str = "<style>body{margin:0} p{margin:0;line-height:20px} \
        div,section,article,li{line-height:20px}</style>";

    fn nested_geometry(
        body: &str,
        page_h: f32,
    ) -> (blitz_html::HtmlDocument, PaginationGeometryTable) {
        let mut doc = parse(
            &format!("<html><head>{LINE_CSS}</head><body>{body}</body></html>"),
            600.0,
        );
        let table = run_pass(&mut doc, page_h);
        (doc, table)
    }

    fn fragments_of<'t>(
        doc: &blitz_html::HtmlDocument,
        table: &'t PaginationGeometryTable,
        id: &str,
    ) -> &'t [Fragment] {
        let node = find_by_id(doc.deref(), id).unwrap();
        &table[&node].fragments
    }

    #[test]
    fn a_tall_paragraph_in_a_div_gets_one_fragment_per_page() {
        // 30 lines of 20px = 600px on 200px pages: 10 lines per page.
        let (doc, table) = nested_geometry(
            &format!("<div><p id=\"p\">{}</p></div>", br_lines(30)),
            200.0,
        );
        let frags = fragments_of(&doc, &table, "p");
        assert_eq!(frags.len(), 3, "{frags:?}");
        for (k, f) in frags.iter().enumerate() {
            assert_eq!(f.page_index, k as u32);
            assert!((f.height.to_f32() - 200.0).abs() < 0.5, "{f:?}");
            assert!(f.y.to_f32() + f.height.to_f32() <= 200.5, "{f:?}");
        }
        assert!(
            table[&find_by_id(doc.deref(), "p").unwrap()].is_split(),
            "the renderer slices lines by fragment"
        );
    }

    /// F78: the nested fragmenter records the same slices.
    #[test]
    fn a_tall_paragraph_in_a_div_records_its_line_slices() {
        let (doc, table) = nested_geometry(
            &format!("<div><p id=\"p\">{}</p></div>", br_lines(30)),
            200.0,
        );
        let geom = &table[&find_by_id(doc.deref(), "p").unwrap()];
        let got: Vec<(u32, u32, f32)> = geom
            .line_slices
            .iter()
            .map(|s| (s.first, s.end, s.offset.to_f32()))
            .collect();
        assert_eq!(
            got,
            vec![(0, 10, 0.0), (10, 20, 200.0), (20, 30, 400.0)],
            "{geom:?}"
        );
        assert_eq!(geom.line_slices.len(), geom.fragments.len());
    }

    #[test]
    fn a_tall_paragraph_fills_the_page_after_other_content() {
        // 50px of spacer, then 30 lines: 7 lines fit on page 0 (150px).
        let (doc, table) = nested_geometry(
            &format!(
                "<div><div style=\"height:50px\"></div><p id=\"p\">{}</p></div>",
                br_lines(30)
            ),
            200.0,
        );
        let frags = fragments_of(&doc, &table, "p");
        assert_eq!(frags.len(), 4, "{frags:?}");
        assert!((frags[0].y.to_f32() - 50.0).abs() < 0.5);
        assert!(
            (frags[0].height.to_f32() - 140.0).abs() < 0.5,
            "{:?}",
            frags[0]
        );
        assert_eq!(frags[1].y.to_f32(), 0.0);
    }

    #[test]
    fn a_paragraph_that_cannot_start_with_two_lines_moves_to_the_next_page() {
        // Room for one line below the spacer (orphans).
        let (doc, table) = nested_geometry(
            &format!(
                "<div><div style=\"height:170px\"></div><p id=\"p\">{}</p></div>",
                br_lines(30)
            ),
            200.0,
        );
        let frags = fragments_of(&doc, &table, "p");
        assert_eq!(frags[0].page_index, 1, "{frags:?}");
        assert_eq!(frags[0].y.to_f32(), 0.0);
    }

    #[test]
    fn padding_and_border_are_reserved_on_every_page_of_a_split_box() {
        // The box is its own paragraph: border 1 + padding 9 = 10px above the lines.
        let (doc, table) = nested_geometry(
            &format!(
                "<section><div id=\"b\" style=\"border:1px solid #000;padding:9px\">{}</div></section>",
                br_lines(30)
            ),
            200.0,
        );
        let node = find_by_id(doc.deref(), "b").unwrap();
        let geom = &table[&node];
        let frags = &geom.fragments;
        assert!(frags.len() >= 4, "{frags:?}");
        let (top, bottom) = geom.line_split_insets.expect("insets recorded");
        assert_eq!((top.to_f32(), bottom.to_f32()), (10.0, 10.0));
        for f in frags {
            assert!(
                f.y.to_f32() + 10.0 + f.height.to_f32() <= 200.5,
                "lines must end inside the page: {f:?}"
            );
        }
        // 600px of lines in 180px strips: 9 lines each (widows pull the last strip up).
        let total: f32 = frags.iter().map(|f| f.height.to_f32()).sum();
        assert!((total - 600.0).abs() < 0.5, "{total}");
    }

    #[test]
    fn the_frame_of_a_wrapper_closes_below_its_last_child() {
        let (doc, table) = nested_geometry(
            &format!(
                "<div id=\"w\" style=\"border:1px solid #000;padding:15px\"><p>{}</p><p id=\"last\">end</p></div><p id=\"next\">next</p>",
                br_lines(30)
            ),
            200.0,
        );
        let w = fragments_of(&doc, &table, "w");
        let last_frame = w.last().unwrap();
        let last = &fragments_of(&doc, &table, "last")[0];
        // The frame ends 16px (padding 15 + border 1) below the last line.
        let frame_bottom = last_frame.y.to_f32() + last_frame.height.to_f32();
        let last_bottom = last.y.to_f32() + last.height.to_f32();
        assert!(
            (frame_bottom - last_bottom - 16.0).abs() < 0.5,
            "frame {frame_bottom}, text {last_bottom}"
        );
        // And the next sibling starts below the frame.
        let next = &fragments_of(&doc, &table, "next")[0];
        assert!(
            next.y.to_f32() >= frame_bottom - 0.5,
            "{next:?} vs {frame_bottom}"
        );
    }

    #[test]
    fn a_padded_flex_column_box_closes_below_a_split_paragraph_in_its_item() {
        // F30b: a single-column flex box is block flow for fragmentation, so its bottom
        // padding and border follow the last item like a block's (the F30 test above).
        for display in ["display:flex;flex-direction:column", "display:grid"] {
            let (doc, table) = nested_geometry(
                &format!(
                    "<div id=\"w\" style=\"{display};border:1px solid #000;padding:15px\">\
                     <div><p>{}</p></div><p id=\"last\">end</p></div><p id=\"next\">next</p>",
                    br_lines(30)
                ),
                200.0,
            );
            let w = fragments_of(&doc, &table, "w");
            let last_frame = w.last().unwrap();
            let last = &fragments_of(&doc, &table, "last")[0];
            let frame_bottom = last_frame.y.to_f32() + last_frame.height.to_f32();
            let last_bottom = last.y.to_f32() + last.height.to_f32();
            assert!(
                (frame_bottom - last_bottom - 16.0).abs() < 0.5,
                "{display}: frame {frame_bottom}, text {last_bottom}"
            );
            let next = &fragments_of(&doc, &table, "next")[0];
            assert!(next.y.to_f32() >= frame_bottom - 0.5, "{display}: {next:?}");
        }
    }

    fn stacks(body: &str) -> bool {
        let (doc, _) = nested_geometry(body, 200.0);
        let c = doc.get_node(find_by_id(doc.deref(), "c").unwrap()).unwrap();
        let kids = c.layout_children.borrow();
        let ids: Vec<usize> = kids.as_deref().map(<[usize]>::to_vec).unwrap_or_default();
        super::items_stack_vertically(doc.deref(), &ids)
    }

    #[test]
    fn items_stack_vertically_tells_columns_from_rows() {
        let items = "<div style=\"height:30px\">a</div><div style=\"height:30px\">b</div>";
        assert!(stacks(&format!(
            "<div id=\"c\" style=\"display:flex;flex-direction:column\">{items}</div>"
        )));
        assert!(stacks(&format!(
            "<div id=\"c\" style=\"display:grid;gap:8px\">{items}</div>"
        )));
        assert!(stacks(
            "<div id=\"c\" style=\"display:flex\"><div style=\"height:30px\">a</div></div>"
        ));
        assert!(!stacks(&format!(
            "<div id=\"c\" style=\"display:flex\">{items}</div>"
        )));
        assert!(!stacks(&format!(
            "<div id=\"c\" style=\"display:grid;grid-template-columns:1fr 1fr\">{items}</div>"
        )));
    }

    #[test]
    fn the_gap_below_a_split_paragraph_is_kept() {
        let (doc, table) = nested_geometry(
            &format!(
                "<div><p id=\"p\">{}</p><div style=\"height:20px\" id=\"after\"></div></div>",
                br_lines(25)
            ),
            200.0,
        );
        // 25 lines: strips of 10, 10 (widows are fine: 5 left).
        let p = fragments_of(&doc, &table, "p");
        let after = &fragments_of(&doc, &table, "after")[0];
        let p_last = p.last().unwrap();
        assert_eq!(after.page_index, p_last.page_index);
        assert!(
            (after.y.to_f32() - (p_last.y.to_f32() + p_last.height.to_f32())).abs() < 0.5,
            "{p_last:?} {after:?}"
        );
    }

    #[test]
    fn a_paragraph_that_fits_a_page_still_moves_whole() {
        // 8 lines (160px) after a 100px spacer on a 200px page: moved, not split.
        let (doc, table) = nested_geometry(
            &format!(
                "<div><div style=\"height:100px\"></div><p id=\"p\">{}</p></div>",
                br_lines(8)
            ),
            200.0,
        );
        let p = fragments_of(&doc, &table, "p");
        assert_eq!(p.len(), 1, "{p:?}");
        assert_eq!(p[0].page_index, 1);
    }

    #[test]
    fn paragraphs_in_flex_grid_items_and_table_cells_are_split() {
        // F30b: an inline root that is itself a flex item, a grid item or a table cell splits
        // between its lines like a paragraph in a div (30 lines of 20px on 200px pages).
        for (what, markup) in [
            (
                "flex row",
                format!(
                    "<div style=\"display:flex\"><p id=\"x\">{}</p></div>",
                    br_lines(30)
                ),
            ),
            (
                "flex column",
                format!(
                    "<div style=\"display:flex;flex-direction:column\"><p id=\"x\">{}</p></div>",
                    br_lines(30)
                ),
            ),
            (
                "grid",
                format!(
                    "<div style=\"display:grid\"><p id=\"x\">{}</p></div>",
                    br_lines(30)
                ),
            ),
            (
                "cell",
                format!(
                    "<table><tr><td id=\"x\" style=\"line-height:20px\">{}</td></tr></table>",
                    br_lines(30)
                ),
            ),
        ] {
            let (doc, table) = nested_geometry(&markup, 200.0);
            let frags = fragments_of(&doc, &table, "x");
            assert!(frags.len() >= 3, "{what}: {frags:?}");
            let lines: f32 = frags.iter().map(|f| f.height.to_f32()).sum();
            assert!(
                (lines - 600.0).abs() < 0.5,
                "{what}: every line once: {frags:?}"
            );
        }
    }

    /// `n` 20px lines of the paragraph `frags` are the fragments of, each once.
    fn assert_lines_once(frags: &[Fragment], n: usize, what: &str) {
        assert!(frags.len() >= 3, "{what}: {frags:?}");
        let total: f32 = frags.iter().map(|f| f.height.to_f32()).sum();
        assert!(
            (total - 20.0 * n as f32).abs() < 0.5,
            "{what}: every line once: {frags:?}"
        );
    }

    /// The geometry of the node with `id`, or none (never entered).
    fn geometry_of<'t>(
        doc: &blitz_html::HtmlDocument,
        table: &'t PaginationGeometryTable,
        id: &str,
    ) -> Option<&'t PaginationGeometry> {
        table.get(&find_by_id(doc.deref(), id).unwrap())
    }

    /// A document laid out at 600px wide, as `nested_geometry` does, without the fragmenter.
    fn laid_out(body: &str) -> blitz_html::HtmlDocument {
        parse(
            &format!("<html><head>{LINE_CSS}</head><body>{body}</body></html>"),
            600.0,
        )
    }

    fn node_of<'d>(doc: &'d blitz_html::HtmlDocument, id: &str) -> &'d blitz_dom::Node {
        doc.deref()
            .get_node(find_by_id(doc.deref(), id).unwrap())
            .unwrap()
    }

    #[test]
    fn a_paragraph_in_a_content_sized_clip_box_is_split() {
        // css-break-3 §4.1: a box with `overflow: hidden` and no definite block size is fragmentable, and Chrome
        // fragments `clip`, `auto` and `scroll` boxes in print too: every line once, a frame on every page.
        for style in [
            "overflow:hidden",
            "overflow:clip",
            "overflow:auto",
            "overflow:scroll",
            "overflow-x:hidden",
            "overflow-x:auto",
        ] {
            let (doc, table) = nested_geometry(
                &format!(
                    "<div id=\"w\" style=\"{style}\"><p id=\"x\">{}</p></div>",
                    br_lines(30)
                ),
                200.0,
            );
            assert_lines_once(fragments_of(&doc, &table, "x"), 30, style);
            assert_eq!(fragments_of(&doc, &table, "w").len(), 3, "{style}");
        }
    }

    #[test]
    fn direct_text_in_a_clip_box_is_split() {
        let (doc, table) = nested_geometry(
            &format!(
                "<div><div id=\"x\" style=\"overflow:hidden\">{}</div></div>",
                br_lines(30)
            ),
            200.0,
        );
        assert_lines_once(fragments_of(&doc, &table, "x"), 30, "overflow:hidden text");
    }

    /// The (page, y, height) of the fragments of `id`, in px.
    fn frames_of(
        doc: &blitz_html::HtmlDocument,
        table: &PaginationGeometryTable,
        id: &str,
    ) -> Vec<(u32, f32, f32)> {
        fragments_of(doc, table, id)
            .iter()
            .map(|f| (f.page_index, f.y.to_f32(), f.height.to_f32()))
            .collect()
    }

    fn assert_frames(got: Vec<(u32, f32, f32)>, expected: &[(u32, f32, f32)], what: &str) {
        assert_eq!(got.len(), expected.len(), "{what}: {got:?} vs {expected:?}");
        for (g, e) in got.iter().zip(expected) {
            assert!(
                g.0 == e.0 && (g.1 - e.1).abs() < 0.6 && (g.2 - e.2).abs() < 0.6,
                "{what}: {got:?} vs {expected:?}"
            );
        }
    }

    #[test]
    fn a_clip_box_shorter_than_its_paragraph_is_fragmented_at_its_block_size() {
        // F30f-b (this was `..._keeps_it_whole` before): the clip hides 300px of the 600px paragraph, and Chrome
        // fragments a `height` / `max-height` box taller than the page, clipping each fragment. The box is 300px over
        // 200px pages: 15 of the 30 lines show, 10 on the first page and 5 on the second, the frames are 200px and 100px.
        for style in ["height:300px", "max-height:300px"] {
            let (doc, table) = nested_geometry(
                &format!(
                    "<div id=\"w\" style=\"overflow:hidden;{style}\"><p id=\"x\">{}</p></div><p id=\"a\">AFTER</p>",
                    br_lines(30)
                ),
                200.0,
            );
            assert_frames(
                frames_of(&doc, &table, "x"),
                &[(0, 0.0, 200.0), (1, 0.0, 100.0)],
                style,
            );
            assert_frames(
                frames_of(&doc, &table, "w"),
                &[(0, 0.0, 200.0), (1, 0.0, 100.0)],
                style,
            );
            // What follows the box starts at the box's bottom.
            assert_frames(frames_of(&doc, &table, "a"), &[(1, 100.0, 20.0)], style);
        }
    }

    #[test]
    fn clips_block_axis_reads_overflow_y() {
        // css-overflow-3 §3: the block axis is clipped when the computed `overflow-y` is not `visible`; a `visible`
        // axis next to a clipping one computes to `auto`, but `clip` next to `visible` stays as it is.
        let cases = [
            ("overflow:hidden", true),
            ("overflow:clip", true),
            ("overflow:auto", true),
            ("overflow:scroll", true),
            ("overflow-y:hidden", true),
            ("overflow-y:clip", true),
            ("overflow-x:hidden", true),
            ("overflow-x:auto", true),
            ("overflow-x:clip", false),
            ("overflow:visible", false),
            ("", false),
        ];
        for (style, expected) in cases {
            let doc = laid_out(&format!("<div id=\"x\" style=\"{style}\"><p>L</p></div>"));
            assert_eq!(clips_block_axis(node_of(&doc, "x")), expected, "`{style}`");
        }
        // The viewport's overflow, not a clip of the box.
        let doc = laid_out("<p>L</p>");
        let body = doc
            .deref()
            .get_node(find_body_id(doc.deref()).unwrap())
            .unwrap();
        assert!(!clips_block_axis(body));
    }

    #[test]
    fn clip_block_limit_is_the_bottom_of_the_padding_box_when_content_reaches_past_it() {
        let lines = br_lines(30);
        let cases: [(&str, String, Option<f32>); 7] = [
            (
                "auto height",
                format!("<div id=\"x\" style=\"overflow:hidden\"><p>{lines}</p></div>"),
                None,
            ),
            (
                "height",
                format!(
                    "<div id=\"x\" style=\"overflow:hidden;height:300px\"><p>{lines}</p></div>"
                ),
                Some(300.0),
            ),
            (
                "max-height",
                format!(
                    "<div id=\"x\" style=\"overflow:hidden;max-height:250px\"><p>{lines}</p></div>"
                ),
                Some(250.0),
            ),
            (
                "border below the padding box",
                format!(
                    "<div id=\"x\" style=\"overflow:hidden;height:300px;border-bottom:7px solid\"><p>{lines}</p></div>"
                ),
                Some(300.0),
            ),
            (
                "padding is inside the padding box",
                format!(
                    "<div id=\"x\" style=\"overflow:hidden;box-sizing:border-box;height:300px;padding:10px\"><p>{lines}</p></div>"
                ),
                Some(300.0),
            ),
            (
                "overflow-x: clip hides nothing in the block axis",
                format!(
                    "<div id=\"x\" style=\"overflow-x:clip;height:300px\"><p>{lines}</p></div>"
                ),
                None,
            ),
            (
                "content within the box",
                "<div id=\"x\" style=\"overflow:hidden;height:300px\"><p>L1<br>L2</p></div>"
                    .to_string(),
                None,
            ),
        ];
        for (what, markup, expected) in cases {
            let doc = laid_out(&markup);
            assert_eq!(
                clip_block_limit(doc.deref(), node_of(&doc, "x")),
                expected,
                "{what}"
            );
        }
    }

    #[test]
    fn block_size_constrained_reads_height_and_max_height() {
        let cases: [(&str, &str, bool); 14] = [
            ("auto", "", false),
            ("height", "height:300px", true),
            ("max-height", "max-height:300px", true),
            ("max-height none", "max-height:none", false),
            ("min-height only", "min-height:300px", false),
            ("height keyword", "height:fit-content", false),
            ("height min-content", "height:min-content", false),
            ("max-height keyword", "max-height:max-content", false),
            (
                "height calc without percentage",
                "height:calc(100px + 2em)",
                true,
            ),
            // The parent is auto-height: a percentage block size computes to `auto` (css-sizing-3 §5.2.1).
            ("percentage of an auto parent", "height:50%", false),
            (
                "max-height percentage of an auto parent",
                "max-height:50%",
                false,
            ),
            (
                "box-sizing does not matter",
                "box-sizing:border-box;height:300px",
                true,
            ),
            (
                "overflow does not matter",
                "overflow:hidden;height:300px",
                true,
            ),
            ("overflow alone", "overflow:hidden", false),
        ];
        for (what, style, expected) in cases {
            let doc = laid_out(&format!(
                "<div><div id=\"x\" style=\"{style}\"><p>L</p></div></div>"
            ));
            assert_eq!(
                block_size_constrained(node_of(&doc, "x")),
                expected,
                "{what}"
            );
        }
        // A percentage of a parent with a definite height is definite; of a parent whose own height is a percentage
        // of an auto grandparent it is not.
        let doc = laid_out(
            "<div style=\"height:400px\"><div id=\"x\" style=\"height:50%\"><p>L</p></div></div>",
        );
        assert!(block_size_constrained(node_of(&doc, "x")));
        let doc = laid_out(
            "<div><div style=\"height:50%\"><div id=\"x\" style=\"height:50%\"><p>L</p></div></div></div>",
        );
        assert!(!block_size_constrained(node_of(&doc, "x")));
        // <html> and <body> clip the viewport, not themselves.
        let doc = laid_out("<p>L</p>");
        let body = doc
            .deref()
            .get_node(find_body_id(doc.deref()).unwrap())
            .unwrap();
        assert!(!block_size_constrained(body));
    }

    #[test]
    fn a_content_sized_clip_box_has_no_limit_however_tall_its_line_boxes_are() {
        // 16 px text in 6 px lines: Blitz's line boxes are as tall as the font (about 19 px) and 6 px apart, so the last
        // one ends 13 px below the content-sized box: `clip_hides_nothing` is false, and the first version of F30f-b
        // cut the box at its own height (words lost). With no `height` and no `max-height` the box hides nothing.
        let lines = br_lines(30);
        let doc = laid_out(&format!(
            "<div id=\"x\" style=\"overflow:hidden;font-size:16px;line-height:6px\">{lines}</div>"
        ));
        let node = node_of(&doc, "x");
        assert!(
            !clip_hides_nothing(doc.deref(), node),
            "the premise: the line boxes reach past the box"
        );
        assert_eq!(clip_block_limit(doc.deref(), node), None);
        assert_eq!(tightest_clip_limit(doc.deref(), node), None);
        // The same box with a `height` equal to its content is a clip box that hides its last lines' boxes: limit set.
        let doc = laid_out(&format!(
            "<div id=\"x\" style=\"overflow:hidden;font-size:16px;line-height:6px;height:180px\">{lines}</div>"
        ));
        assert_eq!(
            clip_block_limit(doc.deref(), node_of(&doc, "x")),
            Some(180.0)
        );
        // Descendants of the content-sized box are not clipped by it either.
        let doc = laid_out(&format!(
            "<div style=\"overflow:hidden;font-size:16px;line-height:6px\"><p id=\"x\">{lines}</p></div>"
        ));
        assert_eq!(ancestor_clip_limit(doc.deref(), node_of(&doc, "x")), None);
    }

    #[test]
    fn a_content_sized_clip_box_keeps_the_barrier_it_had_and_a_sized_one_loses_it_in_block_flow() {
        let lines = br_lines(30);
        let doc = laid_out(&format!(
            "<div id=\"x\" style=\"overflow:hidden;font-size:16px;line-height:6px\">{lines}</div>\
             <div id=\"y\" style=\"overflow:hidden;font-size:16px;line-height:6px;height:100px\">{lines}</div>"
        ));
        for through in [false, true] {
            assert_eq!(
                box_barrier(doc.deref(), node_of(&doc, "x"), through),
                Some(LineBarrier::ClipsItsContent),
                "content-sized, through_clips {through}: as before F30f-b"
            );
        }
        assert_eq!(
            box_barrier(doc.deref(), node_of(&doc, "y"), false),
            Some(LineBarrier::ClipsItsContent)
        );
        assert_eq!(box_barrier(doc.deref(), node_of(&doc, "y"), true), None);
    }

    #[test]
    fn ancestor_clip_limit_is_in_the_coordinates_of_the_node() {
        // A clip box 300px high holds a div (20px of padding above) that holds the paragraph: the clip is 280px below
        // the top of the div's content... the paragraph's top, 20px down in the box.
        let doc = laid_out(&format!(
            "<div style=\"overflow:hidden;height:300px\"><div style=\"padding-top:20px\"><p id=\"x\">{}</p></div></div>",
            br_lines(30)
        ));
        let limit = ancestor_clip_limit(doc.deref(), node_of(&doc, "x"));
        assert!(limit.is_some_and(|l| (l - 280.0).abs() < 0.5), "{limit:?}");
        // Two clip boxes: the tighter wins.
        let doc = laid_out(&format!(
            "<div style=\"overflow:hidden;height:300px\"><div style=\"overflow:hidden;height:100px\"><p id=\"x\">{}</p></div></div>",
            br_lines(30)
        ));
        let limit = ancestor_clip_limit(doc.deref(), node_of(&doc, "x"));
        assert!(limit.is_some_and(|l| (l - 100.0).abs() < 0.5), "{limit:?}");
        // Nothing clips above it.
        let doc = laid_out(&format!("<div><p id=\"x\">{}</p></div>", br_lines(30)));
        assert_eq!(ancestor_clip_limit(doc.deref(), node_of(&doc, "x")), None);
    }

    #[test]
    fn clip_to_drops_the_lines_that_start_at_or_below_the_limit() {
        let lines: Vec<(f32, f32)> = (0..10)
            .map(|k| (20.0 * k as f32, 20.0 * (k + 1) as f32))
            .collect();
        let split = |inset_top: f32| LineSplit {
            lines: lines.clone(),
            inset_top,
            inset_bottom: 0.0,
            hidden_tail: 0,
            own_box_height: None,
            slice: false,
        };
        // 100px: lines at 0..80 show, the one at 100 does not.
        let clipped = split(0.0).clip_to(100.0);
        assert_eq!((clipped.lines.len(), clipped.hidden_tail), (5, 5));
        // 110px: the sixth line shows 10px.
        let clipped = split(0.0).clip_to(110.0);
        assert_eq!((clipped.lines.len(), clipped.hidden_tail), (6, 4));
        // The inset above the first line counts.
        let clipped = split(17.0).clip_to(100.0);
        assert_eq!((clipped.lines.len(), clipped.hidden_tail), (5, 5));
        // A hair of a line (0.3px) does not show.
        let clipped = split(0.0).clip_to(100.3);
        assert_eq!(clipped.lines.len(), 5);
        // The first line is always kept.
        let clipped = split(0.0).clip_to(0.0);
        assert_eq!((clipped.lines.len(), clipped.hidden_tail), (1, 9));
    }

    #[test]
    fn a_padded_body_level_paragraph_is_sliced_at_its_page_breaks() {
        // html-to-pdf F30f-d: 30 lines of 20px in a box with border 1 + padding 9 = 10px above and below, on 200px
        // pages. The first strip leaves the top inset (190px: 9 lines); a continuation strip starts at the top of its
        // page and keeps no top inset (200px: 10 lines), because the box is sliced (css-break-3 §5.4); the bottom inset
        // is on the last fragment only. Main fitted the lines as if they sat at the top of the border box and the
        // renderer painted them 10px lower: the last line of each page stood below it.
        let (doc, table) = nested_geometry(
            &format!(
                "<div id=\"b\" style=\"border:1px solid #000;padding:9px\">{}</div><p id=\"a\">AFTER</p>",
                br_lines(30)
            ),
            200.0,
        );
        let geom = geometry_of(&doc, &table, "b").expect("geometry");
        assert!(geom.line_split_slice, "sliced");
        let (top, bottom) = geom.line_split_insets.expect("insets recorded");
        assert_eq!((top.to_f32(), bottom.to_f32()), (10.0, 10.0));
        let slices: Vec<(u32, u32)> = geom.line_slices.iter().map(|l| (l.first, l.end)).collect();
        assert_eq!(slices, vec![(0, 9), (9, 19), (19, 28), (28, 30)]);
        let frags = &geom.fragments;
        // The first fragment is where the box starts; every continuation one is at the top of its page.
        assert_eq!(frags[0].y.to_f32(), 0.0);
        for f in &frags[1..] {
            assert_eq!(f.y.to_f32(), 0.0, "{frags:?}");
        }
        // Lines end inside the page: the first strip after its 10px inset, the others from the top.
        assert!(frags[0].y.to_f32() + 10.0 + 9.0 * 20.0 <= 200.0);
        assert!(frags[1].y.to_f32() + 10.0 * 20.0 <= 200.0);
        // A fragment that is not the last fills the page (the frame runs to the bottom), the last one is its lines.
        let heights: Vec<f32> = frags.iter().map(|f| f.height.to_f32()).collect();
        assert_eq!(heights, vec![190.0, 200.0, 200.0, 40.0], "{frags:?}");
        // What follows starts below the last line and the bottom inset: 40 + 10 + 10 = 60px... lines 40, top 0, bottom 10.
        assert_frames(
            frames_of(&doc, &table, "a"),
            &[(3, 50.0, 20.0)],
            "after the box",
        );
    }

    #[test]
    fn a_padded_box_with_a_height_whose_lines_fit_keeps_its_path() {
        // html-to-pdf F30f-d: the box is taller than the page only through `height` (300px on 200px pages), its four lines
        // and their insets fit: it is not cut between its lines (F83c), whatever the room below the cursor.
        for style in ["height:300px", "min-height:300px"] {
            let (doc, table) = nested_geometry(
                &format!(
                    "<div style=\"height:60px\"></div><div id=\"b\" style=\"border:1px solid #000;padding:9px;{style}\">{}</div>",
                    br_lines(4)
                ),
                200.0,
            );
            let geom = geometry_of(&doc, &table, "b").expect("geometry");
            assert!(!geom.line_split_slice, "{style}");
            assert_eq!(geom.line_split_insets, None, "{style}");
        }
    }

    #[test]
    fn a_padded_body_level_paragraph_that_fits_is_not_sliced() {
        let (doc, table) = nested_geometry(
            &format!(
                "<div id=\"b\" style=\"border:1px solid #000;padding:9px\">{}</div><p id=\"a\">AFTER</p>",
                br_lines(5)
            ),
            200.0,
        );
        let geom = geometry_of(&doc, &table, "b").expect("geometry");
        assert!(!geom.line_split_slice);
        assert_eq!(geom.fragments.len(), 1);
        // (The fitting path leaves `AFTER` at 100px, over the box's last line: `fragment_inline_root` returns the lines'
        // end without the insets. Not this change's: html-to-pdf finding F30f-h.)
        assert_frames(frames_of(&doc, &table, "a"), &[(0, 100.0, 20.0)], "after");
    }

    #[test]
    fn a_body_level_clip_box_that_fits_is_placed_whole() {
        // 30 lines of direct text in a box of 60px, and the same in a paragraph: the box fits the room, so it is one
        // fragment of 60px (the lines it hides are the writer's to leave out), and what follows starts at 60px, on page 0.
        for (what, markup) in [
            (
                "direct text",
                format!(
                    "<div id=\"w\" style=\"overflow:hidden;height:60px\">{}</div><p id=\"a\">AFTER</p>",
                    br_lines(30)
                ),
            ),
            (
                "paragraph",
                format!(
                    "<div id=\"w\" style=\"overflow:hidden;height:60px\"><p id=\"x\">{}</p></div><p id=\"a\">AFTER</p>",
                    br_lines(30)
                ),
            ),
            (
                "nested paragraph",
                format!(
                    "<div id=\"w\" style=\"overflow:hidden;height:60px\"><div><p id=\"x\">{}</p></div></div><p id=\"a\">AFTER</p>",
                    br_lines(30)
                ),
            ),
        ] {
            let (doc, table) = nested_geometry(&markup, 200.0);
            assert_frames(frames_of(&doc, &table, "w"), &[(0, 0.0, 60.0)], what);
            assert_frames(frames_of(&doc, &table, "a"), &[(0, 60.0, 20.0)], what);
        }
    }

    #[test]
    fn a_clip_box_taller_than_the_room_is_fragmented_at_its_block_size() {
        // 500px box over 200px pages: 25 of the 60 lines show (10 + 10 + 5); the frames are 200, 200 and 100px, the
        // paragraph after the box starts at 100px on page 2, and nothing is placed on a fourth page.
        for (what, markup) in [
            (
                "paragraph",
                format!(
                    "<div id=\"w\" style=\"overflow:hidden;height:500px\"><p id=\"x\">{}</p></div><p id=\"a\">AFTER</p>",
                    br_lines(60)
                ),
            ),
            (
                "nested paragraph",
                format!(
                    "<div id=\"w\" style=\"overflow:hidden;height:500px\"><div><p id=\"x\">{}</p></div></div><p id=\"a\">AFTER</p>",
                    br_lines(60)
                ),
            ),
            (
                "direct text",
                format!(
                    "<div id=\"x\" style=\"overflow:hidden;height:500px\">{}</div><p id=\"a\">AFTER</p>",
                    br_lines(60)
                ),
            ),
        ] {
            let (doc, table) = nested_geometry(&markup, 200.0);
            let frames = [(0, 0.0, 200.0), (1, 0.0, 200.0), (2, 0.0, 100.0)];
            assert_frames(frames_of(&doc, &table, "x"), &frames, what);
            if what != "direct text" {
                assert_frames(frames_of(&doc, &table, "w"), &frames, what);
            }
            assert_frames(frames_of(&doc, &table, "a"), &[(2, 100.0, 20.0)], what);
            // The slices tile all 60 lines: 25 shown, the hidden 35 after them.
            let x = geometry_of(&doc, &table, "x").unwrap();
            assert!(
                line_slices_tile(&x.line_slices, 60),
                "{what}: {:?}",
                x.line_slices
            );
            let shown: Vec<(u32, u32)> = x.line_slices.iter().map(|s| (s.first, s.end)).collect();
            assert_eq!(shown, vec![(0, 10), (10, 20), (20, 25), (25, 60)], "{what}");
            assert_eq!(x.fragments.len(), 3, "{what}");
        }
    }

    #[test]
    fn a_clip_box_that_straddles_the_page_bottom_is_fragmented_in_place() {
        // The box starts 150px down a 200px page: 50px of room, two lines. The box is 100px: 5 lines show, 2 on the
        // first page, 3 on the second; the frames are 50 and 50px and the paragraph after starts at 50px on page 1.
        for (what, body) in [
            (
                "direct text",
                format!(
                    "<div id=\"x\" style=\"overflow:hidden;height:100px\">{}</div>",
                    br_lines(30)
                ),
            ),
            (
                "paragraph",
                format!(
                    "<div id=\"w\" style=\"overflow:hidden;height:100px\"><p id=\"x\">{}</p></div>",
                    br_lines(30)
                ),
            ),
        ] {
            let (doc, table) = nested_geometry(
                &format!("<div style=\"height:150px\"></div>{body}<p id=\"a\">AFTER</p>"),
                200.0,
            );
            if what == "paragraph" {
                // The paragraph's own fragments are its lines (2 and 3 of them); the box's frames are 50px each.
                assert_frames(
                    frames_of(&doc, &table, "x"),
                    &[(0, 150.0, 40.0), (1, 0.0, 60.0)],
                    what,
                );
                assert_frames(
                    frames_of(&doc, &table, "w"),
                    &[(0, 150.0, 50.0), (1, 0.0, 50.0)],
                    what,
                );
            } else {
                assert_frames(
                    frames_of(&doc, &table, "x"),
                    &[(0, 150.0, 50.0), (1, 0.0, 50.0)],
                    what,
                );
            }
            assert_frames(frames_of(&doc, &table, "a"), &[(1, 50.0, 20.0)], what);
            let x = geometry_of(&doc, &table, "x").unwrap();
            let shown: Vec<(u32, u32)> = x.line_slices.iter().map(|s| (s.first, s.end)).collect();
            assert_eq!(shown, vec![(0, 2), (2, 5), (5, 30)], "{what}");
        }
    }

    #[test]
    fn a_padded_clip_box_with_a_paragraph_is_fragmented_in_place_like_one_with_direct_text() {
        // html-to-pdf F30f-b, plan 7 (padded boxes): the paragraph sits `padding-top` below the box's top, so it was not
        // "the first thing in the strip" and, cut by the clip to 6 lines (120px, a page is 200px), it fitted a page and
        // moved whole to the next one: the box's frame on page 0 was its padding alone and its first lines were gone
        // from the page (Chrome: 3 lines on the first page, 3 on the second, like the same box with the words directly
        // in it). 100px of height, 16px of padding or a border of 2px, the box starts 120px down the page.
        for (what, style) in [
            ("padding", "padding:16px;height:100px"),
            ("border", "border:2px solid;height:100px"),
            ("padding-top", "padding-top:16px;height:100px"),
        ] {
            let lines = br_lines(30);
            let (pdoc, ptable) = nested_geometry(
                &format!(
                    "<div style=\"height:120px\"></div><div id=\"w\" style=\"overflow:hidden;{style}\">\
                     <p id=\"x\">{lines}</p></div><p id=\"a\">AFTER</p>"
                ),
                200.0,
            );
            let (ddoc, dtable) = nested_geometry(
                &format!(
                    "<div style=\"height:120px\"></div><div id=\"x\" style=\"overflow:hidden;{style}\">{lines}</div>\
                     <p id=\"a\">AFTER</p>"
                ),
                200.0,
            );
            let slices = |doc: &blitz_html::HtmlDocument, table: &PaginationGeometryTable| {
                geometry_of(doc, table, "x")
                    .unwrap()
                    .line_slices
                    .iter()
                    .map(|s| (s.first, s.end))
                    .collect::<Vec<_>>()
            };
            assert_eq!(
                slices(&pdoc, &ptable),
                slices(&ddoc, &dtable),
                "{what}: the paragraph is cut where the direct text is"
            );
            let on_first_page = frames_of(&pdoc, &ptable, "x");
            assert_eq!(on_first_page.first().map(|f| f.0), Some(0), "{what}");
            let w = frames_of(&pdoc, &ptable, "w");
            assert_eq!(
                w.len(),
                2,
                "{what}: the box has a frame on each page: {w:?}"
            );
            assert_eq!(
                frames_of(&pdoc, &ptable, "a"),
                frames_of(&ddoc, &dtable, "a"),
                "{what}: what follows is where it follows the box with direct text"
            );
        }
    }

    #[test]
    fn a_table_box_has_no_constrained_block_size() {
        // CSS 2.1 §17.5.3: `height` on a table, a row group, a row or a cell is a minimum. The box grows with its content
        // (Chrome shows all of a `td{height:300px;overflow:hidden}`), so no limit, no band, no fragmentation by it.
        let doc = laid_out(
            "<table id=\"t\" style=\"height:100px\"><tbody id=\"g\" style=\"height:100px\">\
             <tr id=\"r\" style=\"height:100px\"><td id=\"c\" style=\"height:100px;overflow:hidden\"><p>L</p></td></tr>\
             </tbody></table>\
             <div id=\"d\" style=\"display:table-cell;height:100px;overflow:hidden\">L</div>\
             <div id=\"x\" style=\"height:100px;overflow:hidden\">L</div>",
        );
        for id in ["t", "g", "r", "c", "d"] {
            assert!(
                !block_size_constrained(node_of(&doc, id)),
                "{id}: a table box is as tall as its content at least"
            );
        }
        assert!(block_size_constrained(node_of(&doc, "x")));
        // A cell with 30 lines in 100px: not a clip box of that height, so no limit.
        let doc = laid_out(&format!(
            "<table><tr><td id=\"c\" style=\"height:100px;overflow:hidden\">{}</td></tr></table>",
            br_lines(30)
        ));
        assert_eq!(clip_block_limit(doc.deref(), node_of(&doc, "c")), None);
        assert_eq!(tightest_clip_limit(doc.deref(), node_of(&doc, "c")), None);
    }

    #[test]
    fn a_paragraph_in_a_padded_box_inside_a_clip_box_is_split_where_the_box_starts() {
        // html-to-pdf F30f-b, plan 7 (padded boxes), the validator's `a_in_c_pad`: `overflow:hidden;height;padding` >
        // `padding` (with or without `overflow:hidden`; the inner box is content-sized, the outer one clips it) > `<p>`.
        // The inner box's padding sits above the paragraph, so the paragraph was not first in its strip and moved whole
        // to the next page, uncut: 636px of frame on page 0 and no line of the paragraph there (page 1 was empty in the
        // PDF where main showed the lines and Chrome shows 15).
        let lines = br_lines(30);
        for (what, inner) in [
            ("hidden", "overflow:hidden;padding:10px"),
            ("visible", "padding:10px"),
            ("border", "border:3px solid"),
        ] {
            let (doc, table) = nested_geometry(
                &format!(
                    "<div style=\"height:120px\"></div><div id=\"w\" style=\"overflow:hidden;height:100px;padding:16px\">\
                     <div id=\"i\" style=\"{inner}\"><p id=\"x\">{lines}</p></div></div><p id=\"a\">AFTER</p>"
                ),
                200.0,
            );
            let x = frames_of(&doc, &table, "x");
            assert_eq!(
                x.iter().map(|f| f.0).collect::<Vec<_>>(),
                vec![0, 1],
                "{what}: the paragraph starts on the page of the box: {x:?}"
            );
            let w = frames_of(&doc, &table, "w");
            assert!(
                w.iter().all(|f| f.2 <= 200.0 - f.1 + 0.5),
                "{what}: the frames of the clip box stay inside the page: {w:?}"
            );
            assert_eq!(
                frames_of(&doc, &table, "a"),
                vec![(1, 52.0, 20.0)],
                "{what}: AFTER follows the box"
            );
        }
    }

    #[test]
    fn clip_shape_supported_is_direct_text_or_one_paragraph_at_the_content_top() {
        // html-to-pdf F30f-b, plan 7 ("Supported shapes"): the third validator pass. Anything above the paragraph (a
        // heading, a margin, a float, an image), a second paragraph, a percentage vertical padding, a flex wrapper:
        // not supported; the box keeps main's behaviour.
        let l = br_lines(30);
        let big = "overflow:hidden;height:100px";
        let cases: Vec<(&str, String, bool)> = vec![
            (
                "direct text",
                format!("<div id=\"x\" style=\"{big}\">{l}</div>"),
                true,
            ),
            (
                "paragraph",
                format!("<div id=\"x\" style=\"{big}\"><p>{l}</p></div>"),
                true,
            ),
            (
                "padded, paragraph",
                format!("<div id=\"x\" style=\"{big};padding:16px\"><p>{l}</p></div>"),
                true,
            ),
            (
                "bordered, paragraph",
                format!("<div id=\"x\" style=\"{big};border:4px solid\"><p>{l}</p></div>"),
                true,
            ),
            (
                "content-sized wrapper around a paragraph",
                format!(
                    "<div id=\"x\" style=\"{big};padding:20px\"><div style=\"overflow:hidden;padding:10px\"><p>{l}</p></div></div>"
                ),
                true,
            ),
            (
                "two wrappers",
                format!("<div id=\"x\" style=\"{big}\"><div><div><p>{l}</p></div></div></div>"),
                true,
            ),
            (
                "heading above",
                format!(
                    "<div id=\"x\" style=\"{big}\"><h2 style=\"margin:0\">H</h2><p>{l}</p></div>"
                ),
                false,
            ),
            (
                "second paragraph",
                format!("<div id=\"x\" style=\"{big}\"><p>{l}</p><p>{l}</p></div>"),
                false,
            ),
            (
                "margin above the paragraph",
                format!("<div id=\"x\" style=\"{big}\"><p style=\"margin-top:14px\">{l}</p></div>"),
                false,
            ),
            (
                "margin above the paragraph of a wrapper",
                format!(
                    "<div id=\"x\" style=\"{big}\"><div><p style=\"margin-top:14px\">{l}</p></div></div>"
                ),
                false,
            ),
            (
                "float above",
                format!(
                    "<div id=\"x\" style=\"{big}\"><div style=\"float:left;width:10px;height:10px\"></div><p>{l}</p></div>"
                ),
                false,
            ),
            (
                "image above",
                format!(
                    "<div id=\"x\" style=\"{big}\"><img style=\"display:block\" width=\"10\" height=\"10\"><p>{l}</p></div>"
                ),
                false,
            ),
            (
                "heading and direct text",
                format!("<div id=\"x\" style=\"{big}\"><h2 style=\"margin:0\">H</h2>{l}</div>"),
                false,
            ),
            (
                "absolutely positioned sibling",
                format!(
                    "<div id=\"x\" style=\"{big};position:relative\"><p>{l}</p><div style=\"position:absolute;top:0\">B</div></div>"
                ),
                false,
            ),
            (
                "flex wrapper",
                format!(
                    "<div id=\"x\" style=\"{big}\"><div style=\"display:flex\"><p>{l}</p></div></div>"
                ),
                false,
            ),
            (
                "percentage padding",
                format!("<div id=\"x\" style=\"{big};padding:5%\"><p>{l}</p></div>"),
                false,
            ),
            (
                "percentage padding-bottom of the paragraph",
                format!(
                    "<div id=\"x\" style=\"{big}\"><p style=\"padding-bottom:5%\">{l}</p></div>"
                ),
                false,
            ),
            (
                "empty",
                format!("<div id=\"x\" style=\"{big}\"></div>"),
                false,
            ),
            (
                "flex clip box with a paragraph",
                format!("<div id=\"x\" style=\"{big};display:flex\"><p>{l}</p></div>"),
                true,
            ),
        ];
        for (what, markup, supported) in cases {
            let doc = laid_out(&markup);
            assert_eq!(
                clip_shape_supported(node_of(&doc, "x")),
                supported,
                "{what}"
            );
            assert_eq!(
                clip_semantics_apply(node_of(&doc, "x")),
                supported,
                "{what}: constrained"
            );
        }
    }

    #[test]
    fn a_clip_box_is_supported_only_in_block_flow() {
        // The validator's `tw_clip_grid_card.s880`: two grid items with `overflow:hidden;height`, one holding a heading
        // and a paragraph, one a paragraph. The second was a supported shape with its clip semantics half on (the fragmenter
        // lifts the clip only below a block container) and the pair came out worse than main. A flex, grid, table,
        // multicol or inline-block parent keeps main's output.
        let l = br_lines(30);
        let item = format!("<div id=\"x\" style=\"overflow:hidden;height:100px\"><p>{l}</p></div>");
        for (what, markup, supported) in [
            ("block parent", format!("<section>{item}</section>"), true),
            (
                "grid item",
                format!("<section style=\"display:grid\">{item}</section>"),
                false,
            ),
            (
                "flex item",
                format!("<section style=\"display:flex\">{item}</section>"),
                false,
            ),
            (
                "multicol child",
                format!("<section style=\"columns:2\">{item}</section>"),
                false,
            ),
            (
                "inline-block child",
                format!("<section style=\"display:inline-block\">{item}</section>"),
                false,
            ),
            (
                "table cell child",
                format!("<table><tr><td>{item}</td></tr></table>"),
                true,
            ),
        ] {
            let doc = laid_out(&markup);
            let node = node_of(&doc, "x");
            assert_eq!(clip_shape_supported(node), supported, "{what}");
        }
    }

    #[test]
    fn a_percentage_padding_is_reported_without_its_vertical_part_by_the_layout() {
        // Why `padding: 5%` is not a supported shape: Taffy resolves the vertical part against the (indefinite)
        // height, so the box has no top padding and a height without it, while its content sits below the padding.
        let doc = laid_out(&format!(
            "<div id=\"x\" style=\"overflow:hidden;height:100px;padding:5%\"><p id=\"p\">{}</p></div>",
            br_lines(30)
        ));
        let (x, p) = (node_of(&doc, "x"), node_of(&doc, "p"));
        assert_eq!(x.final_layout.padding.left, 30.0);
        assert_eq!(x.final_layout.padding.top, 0.0, "the layout drops it");
        assert_eq!(x.final_layout.size.height, 100.0);
        assert_eq!(p.final_layout.location.y, 30.0, "the content is below it");
        assert!(!clip_shape_supported(x));
    }

    #[test]
    fn a_clip_box_of_an_unsupported_shape_has_no_limit_and_keeps_the_barrier_of_main() {
        let l = br_lines(30);
        for (what, inner) in [
            (
                "heading",
                format!("<h2 style=\"margin:0\">H</h2><p id=\"p\">{l}</p>"),
            ),
            ("second paragraph", format!("<p id=\"p\">{l}</p><p>{l}</p>")),
        ] {
            let doc = laid_out(&format!(
                "<div id=\"x\" style=\"overflow:hidden;height:100px;padding:16px\">{inner}</div>"
            ));
            let (x, p) = (node_of(&doc, "x"), node_of(&doc, "p"));
            assert!(block_size_constrained(x), "{what}");
            assert_eq!(clip_block_limit(doc.deref(), x), None, "{what}");
            assert_eq!(tightest_clip_limit(doc.deref(), p), None, "{what}");
            assert_eq!(
                box_barrier(doc.deref(), x, true),
                Some(LineBarrier::ClipsItsContent),
                "{what}: kept whole as on main, whatever through_clips says"
            );
        }
        // The supported neighbour has both.
        let doc = laid_out(&format!(
            "<div id=\"x\" style=\"overflow:hidden;height:100px;padding:16px\"><p id=\"p\">{l}</p></div>"
        ));
        assert_eq!(
            clip_block_limit(doc.deref(), node_of(&doc, "x")),
            Some(132.0)
        );
        assert_eq!(box_barrier(doc.deref(), node_of(&doc, "x"), true), None);
    }

    #[test]
    fn a_paragraph_that_avoids_a_break_inside_makes_the_box_an_unsupported_shape() {
        // `break-inside: avoid` is read from the stylesheet by `column_css`; the engine opens the scope that tells
        // the predicate. The line splitter would split the paragraph that is first in a padded clip box regardless.
        let l = br_lines(30);
        for style in ["break-inside:avoid", "page-break-inside:avoid"] {
            let doc = laid_out(&format!(
                "<div id=\"x\" style=\"overflow:hidden;height:100px;padding:16px\"><p id=\"p\" style=\"{style}\">{l}</p></div>\
                 <div id=\"y\" style=\"overflow:hidden;height:100px;padding:16px;{style}\"><p>{l}</p></div>"
            ));
            let table = crate::blitz_adapter::extract_column_style_table(&doc);
            assert!(
                clip_shape_supported(node_of(&doc, "x")),
                "{style}: no scope, no avoid"
            );
            {
                let _scope = BreakAvoidScope::enter(&table);
                assert!(
                    !clip_shape_supported(node_of(&doc, "x")),
                    "{style}: the paragraph avoids"
                );
                assert!(
                    !clip_shape_supported(node_of(&doc, "y")),
                    "{style}: the box avoids"
                );
                assert_eq!(clip_block_limit(doc.deref(), node_of(&doc, "x")), None);
            }
            assert!(
                clip_shape_supported(node_of(&doc, "x")),
                "{style}: the scope is closed"
            );
        }
    }

    #[test]
    fn a_clip_box_with_two_paragraphs_is_not_fragmented_by_its_block_size() {
        // F30f-f (plan 7, "Supported shapes"): the second paragraph starts at 200px, below the clip of the 100px box that
        // straddles the page bottom. Content below the clip getting no page needs the fragmenter to place more than the
        // first thing on a strip (F30c); until then a box with two paragraphs has no limit and is laid out as on main.
        let (doc, table) = nested_geometry(
            &format!(
                "<div style=\"height:150px\"></div><div id=\"w\" style=\"overflow:hidden;height:100px\">\
                 <p id=\"x\">{}</p><p id=\"y\">{}</p></div><p id=\"a\">AFTER</p>",
                br_lines(10),
                br_lines(10)
            ),
            200.0,
        );
        // As on main: the first paragraph on the page the box starts on, the second on the next one, AFTER after it.
        assert_frames(frames_of(&doc, &table, "x"), &[(1, 0.0, 200.0)], "x");
        assert_frames(frames_of(&doc, &table, "y"), &[(2, 0.0, 200.0)], "y");
        assert_frames(frames_of(&doc, &table, "a"), &[(3, 0.0, 20.0)], "after");
        assert_eq!(clip_block_limit(doc.deref(), node_of(&doc, "w")), None);
    }

    #[test]
    fn a_clip_box_with_direct_text_that_fits_is_not_split_by_the_body_line_path() {
        // R1 of the plan: the body-level line path split every multi-line child at the page bottom, and the box's
        // per-fragment clip then showed what it hides.
        let (doc, table) = nested_geometry(
            &format!(
                "<div style=\"height:150px\"></div><div id=\"x\" style=\"overflow:hidden;height:40px\">{}</div>",
                br_lines(30)
            ),
            200.0,
        );
        assert_frames(frames_of(&doc, &table, "x"), &[(0, 150.0, 40.0)], "fits");
    }

    #[test]
    fn a_paragraph_in_an_inline_block_alone_on_its_line_is_split() {
        let (doc, table) = nested_geometry(
            &format!(
                "<div id=\"ib\" style=\"display:inline-block;width:100%\"><p id=\"x\">{}</p></div>",
                br_lines(30)
            ),
            200.0,
        );
        assert_lines_once(fragments_of(&doc, &table, "x"), 30, "ib > p");
        assert_eq!(fragments_of(&doc, &table, "ib").len(), 3);
    }

    #[test]
    fn an_inline_block_with_direct_text_is_split() {
        // A carrier `div` holds the box; the box holds its text itself.
        let (doc, table) = nested_geometry(
            &format!(
                "<div id=\"c\"><div id=\"ib\" style=\"display:inline-block\">{}</div></div>",
                br_lines(30)
            ),
            200.0,
        );
        assert_lines_once(fragments_of(&doc, &table, "ib"), 30, "ib text");
    }

    #[test]
    fn the_carrier_line_is_on_the_page_where_its_inline_box_starts() {
        // The carrier's one line holds the box. Once the box has a fragment per page, the carrier has one per
        // page too, and its line has to be painted exactly once, where the box starts (F30f).
        let (doc, table) = nested_geometry(
            &format!(
                "<div id=\"c\"><div id=\"ib\" style=\"display:inline-block;width:100%\"><p>{}</p></div></div>",
                br_lines(30)
            ),
            200.0,
        );
        let carrier = geometry_of(&doc, &table, "c").unwrap();
        let got: Vec<(u32, u32)> = carrier
            .line_slices
            .iter()
            .map(|s| (s.first, s.end))
            .collect();
        assert_eq!(got, vec![(0, 1), (1, 1), (1, 1)], "{carrier:?}");
        assert_eq!(carrier.fragments.len(), 3);
        assert!(carrier_slices_ok(carrier));
        assert_eq!(fragments_of(&doc, &table, "ib")[0].page_index, 0);
    }

    #[test]
    fn an_inline_block_moved_to_the_next_page_takes_its_carrier_line_along() {
        // 170px of padding leave one line on page 0: ORPHANS moves the box to page 1. The carrier has a fragment
        // on page 0 (its padding) with no line, and its line is on page 1 with the box.
        let (doc, table) = nested_geometry(
            &format!(
                "<div id=\"c\" style=\"padding-top:170px\"><div id=\"ib\" style=\"display:inline-block;width:100%\">\
                 <p>{}</p></div></div>",
                br_lines(30)
            ),
            200.0,
        );
        let ib = fragments_of(&doc, &table, "ib");
        assert_eq!(ib[0].page_index, 1, "{ib:?}");
        let carrier = geometry_of(&doc, &table, "c").unwrap();
        let got: Vec<(u32, u32)> = carrier
            .line_slices
            .iter()
            .map(|s| (s.first, s.end))
            .collect();
        assert_eq!(got, vec![(0, 0), (0, 1), (1, 1), (1, 1)], "{carrier:?}");
        assert_eq!(carrier.fragments.len(), 4);
        assert!(carrier_slices_ok(carrier));
    }

    #[test]
    fn an_inline_block_sharing_its_line_keeps_its_paragraph_whole() {
        // Text on the line: the box is one unit (Chrome slices it, F30f-b).
        let (doc, table) = nested_geometry(
            &format!(
                "<div>Hi <span id=\"x\" style=\"display:inline-block\">{}</span> end</div>",
                br_lines(30)
            ),
            200.0,
        );
        let n = geometry_of(&doc, &table, "x").map_or(0, |g| g.fragments.len());
        assert!(n <= 1, "{n} fragments");
    }

    #[test]
    fn paragraphs_that_are_not_in_plain_flow_stay_whole() {
        // The exclusions of `line_split_candidate` that hold: an atomic inline that is not an `inline-block` alone on
        // its line, and a clipping parent that hides content, place a paragraph as one unit. (A non-cell child of a
        // `display: table` box used to be one too; since F50 it sits in an anonymous cell and splits like any block,
        // see below. A content-sized clipping parent splits since F30f, see above.)
        for (what, markup) in [
            (
                "overflow:hidden flex, height",
                format!(
                    "<div style=\"display:flex;overflow:hidden;height:300px\"><p id=\"x\">{}</p></div>",
                    br_lines(30)
                ),
            ),
            (
                "inline-flex",
                format!(
                    "<div style=\"display:inline-flex\"><p id=\"x\">{}</p></div>",
                    br_lines(30)
                ),
            ),
        ] {
            let (doc, table) = nested_geometry(&markup, 200.0);
            // Not entered at all (no entry), or one whole fragment: never split between lines.
            let n = geometry_of(&doc, &table, "x").map_or(0, |g| g.fragments.len());
            assert!(n <= 1, "{what}: {n} fragments");
        }
    }

    #[test]
    fn a_paragraph_in_a_content_sized_flex_clip_box_is_split() {
        // Chrome (P17): `display:flex; overflow:hidden` around a paragraph fragments it: every line once.
        let (doc, table) = nested_geometry(
            &format!(
                "<div style=\"display:flex;overflow:hidden\"><p id=\"x\">{}</p></div>",
                br_lines(30)
            ),
            200.0,
        );
        assert_lines_once(fragments_of(&doc, &table, "x"), 30, "flex overflow:hidden");
    }

    #[test]
    fn clip_hides_nothing_reads_the_block_axis() {
        let two = br_lines(2);
        let cases: [(&str, String, bool); 13] = [
            (
                "auto",
                format!("<div id=\"x\" style=\"overflow:hidden\"><p>{two}</p></div>"),
                true,
            ),
            (
                "height below",
                format!("<div id=\"x\" style=\"overflow:hidden;height:30px\"><p>{two}</p></div>"),
                false,
            ),
            (
                "height above",
                format!("<div id=\"x\" style=\"overflow:hidden;height:100px\"><p>{two}</p></div>"),
                true,
            ),
            (
                "max-height",
                format!(
                    "<div id=\"x\" style=\"overflow:hidden;max-height:30px\"><p>{two}</p></div>"
                ),
                false,
            ),
            (
                "padding",
                format!("<div id=\"x\" style=\"overflow:hidden;padding:10px\"><p>{two}</p></div>"),
                true,
            ),
            (
                "content over the bottom padding",
                format!(
                    "<div id=\"x\" style=\"overflow:hidden;box-sizing:border-box;height:50px;padding:10px\"><p>{two}</p></div>"
                ),
                false,
            ),
            (
                "float",
                format!(
                    "<div id=\"x\" style=\"overflow:hidden\"><div style=\"float:left;height:100px\"></div><p>{two}</p></div>"
                ),
                true,
            ),
            (
                "float over a height",
                format!(
                    "<div id=\"x\" style=\"overflow:hidden;height:50px\"><div style=\"float:left;height:100px\"></div><p>{two}</p></div>"
                ),
                false,
            ),
            (
                "overflow-x only",
                format!("<div id=\"x\" style=\"overflow-x:hidden\"><p>{two}</p></div>"),
                true,
            ),
            (
                "inline root",
                format!("<div id=\"x\" style=\"overflow:hidden\">{two}</div>"),
                true,
            ),
            (
                "inline root with a height",
                format!("<div id=\"x\" style=\"overflow:hidden;height:30px\">{two}</div>"),
                false,
            ),
            (
                "paragraph taller than its own box",
                format!(
                    "<div id=\"x\" style=\"overflow:hidden\"><p style=\"height:30px\">{two}</p></div>"
                ),
                false,
            ),
            (
                "stretched flex item taller than the box",
                format!(
                    "<div id=\"x\" style=\"display:flex;overflow:hidden;height:30px\"><p>{two}</p></div>"
                ),
                false,
            ),
        ];
        for (what, markup, expected) in cases {
            let doc = laid_out(&markup);
            assert_eq!(
                clip_hides_nothing(doc.deref(), node_of(&doc, "x")),
                expected,
                "{what}"
            );
        }
    }

    #[test]
    fn alone_on_its_line_wants_one_line_with_only_the_box_on_it() {
        let cases: [(&str, &str, bool); 10] = [
            (
                "alone",
                "<div><div id=\"x\" style=\"display:inline-block\">a</div></div>",
                true,
            ),
            (
                "inline-table alone",
                "<div><div id=\"x\" style=\"display:inline-table\">a</div></div>",
                true,
            ),
            (
                "inline-table with text",
                "<div>Hi <span id=\"x\" style=\"display:inline-table\">a</span></div>",
                false,
            ),
            (
                "blanks around it",
                "<div> <span id=\"x\" style=\"display:inline-block\">a</span> </div>",
                true,
            ),
            (
                "two boxes",
                "<div><span id=\"x\" style=\"display:inline-block\">a</span><span style=\"display:inline-block\">b</span></div>",
                false,
            ),
            (
                "text before",
                "<div>Hi <span id=\"x\" style=\"display:inline-block\">a</span></div>",
                false,
            ),
            (
                "text after",
                "<div><span id=\"x\" style=\"display:inline-block\">a</span> end</div>",
                false,
            ),
            (
                "second line",
                "<div>Hi<br><span id=\"x\" style=\"display:inline-block\">a</span></div>",
                false,
            ),
            (
                "inline-flex",
                "<div><span id=\"x\" style=\"display:inline-flex\">a</span></div>",
                false,
            ),
            ("a block", "<div><div id=\"x\">a</div></div>", false),
        ];
        for (what, markup, expected) in cases {
            let doc = laid_out(markup);
            assert_eq!(
                alone_on_its_line(doc.deref(), node_of(&doc, "x")),
                expected,
                "{what}"
            );
        }
    }

    #[test]
    fn split_barrier_names_each_arm() {
        use LineBarrier::*;
        let two = br_lines(2);
        // (what, markup, parent id, child id, expected)
        let cases: Vec<(&str, String, &str, &str, Option<LineBarrier>)> = vec![
            (
                "plain",
                format!("<div id=\"p\"><p id=\"c\">{two}</p></div>"),
                "p",
                "c",
                None,
            ),
            (
                "inline-block alone",
                format!(
                    "<div><div id=\"p\" style=\"display:inline-block\"><p id=\"c\">{two}</p></div></div>"
                ),
                "p",
                "c",
                None,
            ),
            (
                "inline-block sharing",
                format!(
                    "<div>Hi <span id=\"p\" style=\"display:inline-block\"><p id=\"c\">{two}</p></span></div>"
                ),
                "p",
                "c",
                Some(SharesItsLine),
            ),
            (
                "inline-flex",
                format!(
                    "<div><div id=\"p\" style=\"display:inline-flex\"><p id=\"c\">{two}</p></div></div>"
                ),
                "p",
                "c",
                Some(AtomicNotFlowRoot),
            ),
            (
                "clip, content hidden",
                format!(
                    "<div id=\"p\" style=\"overflow:hidden;height:30px\"><p id=\"c\">{two}</p></div>"
                ),
                "p",
                "c",
                Some(ClipsItsContent),
            ),
            (
                "clip, content inside",
                format!("<div id=\"p\" style=\"overflow:hidden\"><p id=\"c\">{two}</p></div>"),
                "p",
                "c",
                None,
            ),
            (
                "clip child",
                format!(
                    "<div id=\"p\"><div id=\"c\" style=\"overflow:hidden;height:30px\">{two}</div></div>"
                ),
                "p",
                "c",
                Some(ClipsItsContent),
            ),
            (
                "multicol parent",
                format!("<div id=\"p\" style=\"columns:2\"><p id=\"c\">{two}</p></div>"),
                "p",
                "c",
                Some(Multicol),
            ),
            (
                "multicol child",
                format!("<div id=\"p\"><div id=\"c\" style=\"columns:2\">{two}</div></div>"),
                "p",
                "c",
                Some(Multicol),
            ),
            (
                "table part",
                format!("<div id=\"p\" style=\"display:table\"><div id=\"c\">{two}</div></div>"),
                "p",
                "c",
                Some(TablePart),
            ),
            (
                "floating child",
                format!("<div id=\"p\"><p id=\"c\" style=\"float:left\">{two}</p></div>"),
                "p",
                "c",
                Some(Floating),
            ),
            (
                "transformed child",
                format!(
                    "<div id=\"p\"><p id=\"c\" style=\"transform:rotate(1deg)\">{two}</p></div>"
                ),
                "p",
                "c",
                Some(Transformed),
            ),
        ];
        for (what, markup, parent, child, expected) in cases {
            let doc = laid_out(&markup);
            assert_eq!(
                split_barrier(doc.deref(), node_of(&doc, parent), node_of(&doc, child)),
                expected,
                "{what}"
            );
        }
    }

    #[test]
    fn carrier_slices_ok_wants_one_line_slice_between_empty_ones() {
        let geom = |slices: &[(u32, u32)]| PaginationGeometry {
            fragments: slices
                .iter()
                .enumerate()
                .map(|(k, _)| Fragment {
                    page_index: k as u32,
                    x: 0.0_f32.as_px(),
                    y: 0.0_f32.as_px(),
                    width: 10.0_f32.as_px(),
                    height: 10.0_f32.as_px(),
                })
                .collect(),
            line_slices: slices
                .iter()
                .map(|&(first, end)| LineSlice {
                    first,
                    end,
                    offset: 0.0_f32.as_px(),
                })
                .collect(),
            ..Default::default()
        };
        assert!(carrier_slices_ok(&geom(&[(0, 1), (1, 1), (1, 1)])));
        assert!(carrier_slices_ok(&geom(&[(0, 0), (0, 1), (1, 1)])));
        assert!(carrier_slices_ok(&geom(&[(0, 1)])));
        assert!(!carrier_slices_ok(&geom(&[(0, 0), (1, 1)])), "no line page");
        assert!(
            !carrier_slices_ok(&geom(&[(0, 1), (0, 1)])),
            "two line pages"
        );
        assert!(
            !carrier_slices_ok(&geom(&[(1, 1), (0, 1)])),
            "line before an after"
        );
        assert!(
            !carrier_slices_ok(&geom(&[(0, 1), (0, 0)])),
            "empty before the line page after it"
        );
        assert!(
            !carrier_slices_ok(&PaginationGeometry::default()),
            "no fragments"
        );
    }

    #[test]
    fn unprinted_lines_in_a_clip_box_count_only_what_the_clip_shows() {
        // 30 lines of 20px on a 200px page.
        let lines = br_lines(30);
        // The clip box ends at 100px, above the page bottom: nothing the clip shows is below the page.
        let doc = laid_out(&format!(
            "<div id=\"p\" style=\"overflow:hidden;height:100px\"><p id=\"c\">{lines}</p></div>"
        ));
        assert!(
            unprinted_inline_lines(
                doc.deref(),
                node_of(&doc, "p"),
                node_of(&doc, "c"),
                0.0,
                200.0
            )
            .is_none()
        );
        // It ends at 300px: 100px of it are below the page.
        let doc = laid_out(&format!(
            "<div id=\"p\" style=\"overflow:hidden;height:300px\"><p id=\"c\">{lines}</p></div>"
        ));
        let (n, below, barrier) = unprinted_inline_lines(
            doc.deref(),
            node_of(&doc, "p"),
            node_of(&doc, "c"),
            0.0,
            200.0,
        )
        .expect("lines below the page");
        assert_eq!(n, 30);
        assert!((below - 100.0).abs() < 0.5, "{below}");
        assert_eq!(barrier, Some(LineBarrier::ClipsItsContent));
        // A paragraph kept whole by an inline-block that shares its line says so.
        let doc = laid_out(&format!(
            "<div>Hi <span id=\"p\" style=\"display:inline-block\"><p id=\"c\">{lines}</p></span></div>"
        ));
        let (_, _, barrier) = unprinted_inline_lines(
            doc.deref(),
            node_of(&doc, "p"),
            node_of(&doc, "c"),
            0.0,
            200.0,
        )
        .expect("lines below the page");
        assert_eq!(barrier, Some(LineBarrier::SharesItsLine));
    }

    #[test]
    fn a_paragraph_that_is_a_non_cell_child_of_a_table_box_splits_in_its_anonymous_cell() {
        // F50: Blitz wraps the `<p>` in an anonymous row and cell (CSS 2.1 §17.2.1), whose content is
        // an ordinary block flow, so its lines are split between pages like those of a real cell.
        let (doc, table) = nested_geometry(
            &format!(
                "<div style=\"display:table\"><p id=\"x\">{}</p></div>",
                br_lines(30)
            ),
            200.0,
        );
        let node = find_by_id(doc.deref(), "x").unwrap();
        let n = table.get(&node).map_or(0, |g| g.fragments.len());
        assert!(n > 1, "{n} fragments");
    }

    /// F30e: the first layout child of the table box `id`, the anonymous cell Blitz makes for text that stands directly in
    /// the table or in one of its rows (F50).
    fn first_layout_child(doc: &blitz_html::HtmlDocument, id: &str) -> usize {
        let node = node_of(doc, id);
        let children = node.layout_children.borrow();
        *children
            .as_deref()
            .and_then(|v| v.first())
            .expect("a layout child")
    }

    /// F30e: `n` 20px lines whose fragments are `frags` are each printed once, as one tiling of line slices.
    fn assert_slices_tile(table: &PaginationGeometryTable, node: usize, n: usize, what: &str) {
        let geom = &table[&node];
        assert_lines_once(&geom.fragments, n, what);
        assert!(
            line_slices_tile(&geom.line_slices, n),
            "{what}: {:?}",
            geom.line_slices
        );
    }

    #[test]
    fn direct_text_in_a_table_box_splits_in_its_anonymous_cell() {
        // CSS 2.1 §17.2.1: the text is wrapped in an anonymous row and cell; the cell is a block container whose
        // lines are break points, like the direct-text `<td>` of F30b.
        for (what, content) in [
            ("text", br_lines(30)),
            ("span", format!("<span>{}</span>", br_lines(30))),
        ] {
            let (doc, table) = nested_geometry(
                &format!("<div id=\"t\" style=\"display:table\">{content}</div>"),
                200.0,
            );
            let cell = first_layout_child(&doc, "t");
            assert_slices_tile(&table, cell, 30, what);
        }
    }

    #[test]
    fn direct_text_in_a_table_row_splits_in_its_anonymous_cell() {
        let (doc, table) = nested_geometry(
            &format!(
                "<div id=\"t\" style=\"display:table\"><div style=\"display:table-row\">{}</div></div>",
                br_lines(30)
            ),
            200.0,
        );
        let cell = first_layout_child(&doc, "t");
        assert_slices_tile(&table, cell, 30, "row text");
    }

    #[test]
    fn a_table_cell_outside_a_table_is_split() {
        // Blitz lays such a cell out as a block container (CSS 2.1 §9.4.1).
        for (what, markup) in [
            (
                "cell",
                format!(
                    "<div><div id=\"x\" style=\"display:table-cell\">{}</div></div>",
                    br_lines(30)
                ),
            ),
            (
                "row and cell",
                format!(
                    "<div style=\"display:table-row\"><div id=\"x\" style=\"display:table-cell\">{}</div></div>",
                    br_lines(30)
                ),
            ),
        ] {
            let (doc, table) = nested_geometry(&markup, 200.0);
            let node = find_by_id(doc.deref(), "x").unwrap();
            assert_slices_tile(&table, node, 30, what);
        }
    }

    #[test]
    fn an_inline_table_alone_on_its_line_is_split() {
        // css-break-3 §4.1 lets a UA treat an `inline-table` as monolithic; like an `inline-block` alone on its line
        // (ADR-0004) it is split between its line boxes instead.
        let (doc, table) = nested_geometry(
            &format!(
                "<div><div id=\"it\" style=\"display:inline-table;width:100%\"><p id=\"x\">{}</p></div></div>",
                br_lines(30)
            ),
            200.0,
        );
        assert_slices_tile(&table, find_by_id(doc.deref(), "x").unwrap(), 30, "it > p");
        let (doc, table) = nested_geometry(
            &format!(
                "<div><div id=\"it\" style=\"display:inline-table;width:100%\">{}</div></div>",
                br_lines(30)
            ),
            200.0,
        );
        let cell = first_layout_child(&doc, "it");
        assert_slices_tile(&table, cell, 30, "it text");
    }

    #[test]
    fn the_carrier_line_of_an_inline_table_is_on_the_page_where_it_starts() {
        let (doc, table) = nested_geometry(
            &format!(
                "<div id=\"c\"><div id=\"it\" style=\"display:inline-table;width:100%\"><p>{}</p></div></div>",
                br_lines(30)
            ),
            200.0,
        );
        let carrier = geometry_of(&doc, &table, "c").unwrap();
        let got: Vec<(u32, u32)> = carrier
            .line_slices
            .iter()
            .map(|s| (s.first, s.end))
            .collect();
        assert_eq!(got, vec![(0, 1), (1, 1), (1, 1)], "{carrier:?}");
        assert!(carrier_slices_ok(carrier));
        assert_eq!(fragments_of(&doc, &table, "it")[0].page_index, 0);
    }

    #[test]
    fn an_inline_table_sharing_its_line_keeps_its_paragraph_whole() {
        let (doc, table) = nested_geometry(
            &format!(
                "<div>Hi <span id=\"it\" style=\"display:inline-table\">{}</span> end</div>",
                br_lines(30)
            ),
            200.0,
        );
        let cell = first_layout_child(&doc, "it");
        let n = table.get(&cell).map_or(0, |g| g.fragments.len());
        assert!(n <= 1, "{n} fragments");
    }

    #[test]
    fn split_barrier_looks_through_anonymous_and_orphan_cells() {
        use LineBarrier::*;
        let two = br_lines(2);
        // (what, markup, parent id, child id or the first layout child of the parent, expected)
        type Case = (
            &'static str,
            String,
            &'static str,
            Option<&'static str>,
            Option<LineBarrier>,
        );
        let cases: Vec<Case> = vec![
            (
                "anonymous cell of a table box",
                format!("<div id=\"p\" style=\"display:table\">{two}</div>"),
                "p",
                None,
                None,
            ),
            (
                "anonymous cell of a row",
                format!(
                    "<div id=\"p\" style=\"display:table\"><div style=\"display:table-row\">{two}</div></div>"
                ),
                "p",
                None,
                None,
            ),
            (
                "cell outside a table",
                format!(
                    "<div id=\"p\"><div id=\"c\" style=\"display:table-cell\">{two}</div></div>"
                ),
                "p",
                Some("c"),
                None,
            ),
            (
                "inline-table alone",
                format!("<div><div id=\"p\" style=\"display:inline-table\">{two}</div></div>"),
                "p",
                None,
                None,
            ),
            (
                "inline-table sharing",
                format!("<div>Hi <span id=\"p\" style=\"display:inline-table\">{two}</span></div>"),
                "p",
                None,
                Some(SharesItsLine),
            ),
        ];
        for (what, markup, parent, child, expected) in cases {
            let doc = laid_out(&markup);
            let child = child.map_or_else(
                || first_layout_child(&doc, parent),
                |c| find_by_id(doc.deref(), c).unwrap(),
            );
            assert_eq!(
                split_barrier(
                    doc.deref(),
                    node_of(&doc, parent),
                    doc.deref().get_node(child).unwrap()
                ),
                expected,
                "{what}"
            );
        }
    }

    #[test]
    fn a_padded_flex_column_box_closes_below_its_direct_split_item() {
        let (doc, table) = nested_geometry(
            &format!(
                "<div id=\"w\" style=\"display:flex;flex-direction:column;border:1px solid #000;padding:15px\">\
                 <p id=\"p\">{}</p></div><p id=\"next\">next</p>",
                br_lines(30)
            ),
            200.0,
        );
        let w = fragments_of(&doc, &table, "w");
        let (last_frame, p) = (
            w.last().unwrap(),
            fragments_of(&doc, &table, "p").last().unwrap(),
        );
        let frame_bottom = last_frame.y.to_f32() + last_frame.height.to_f32();
        let p_bottom = p.y.to_f32() + p.height.to_f32();
        assert!(
            (frame_bottom - p_bottom - 16.0).abs() < 0.5,
            "frame {frame_bottom}, text {p_bottom}"
        );
        let next = &fragments_of(&doc, &table, "next")[0];
        assert!(next.y.to_f32() >= frame_bottom - 0.5, "{next:?}");
    }

    #[test]
    fn a_grid_row_whose_first_item_cannot_start_here_moves_with_its_mates() {
        // Second row: room for one line only (orphans). The paragraph, first in its row, moves to
        // the next page and the square beside it comes along instead of staying behind.
        let (doc, table) = nested_geometry(
            &format!(
                "<div style=\"display:grid;grid-template-columns:100px 100px;align-items:start\">\
                 <div style=\"height:170px\"></div><div></div>\
                 <p id=\"p\">{}</p><div id=\"sq\" style=\"height:20px\"></div></div>",
                br_lines(30)
            ),
            200.0,
        );
        let p = fragments_of(&doc, &table, "p");
        let sq = &fragments_of(&doc, &table, "sq")[0];
        assert_eq!(p[0].page_index, 1, "{p:?}");
        assert_eq!((sq.page_index, sq.y.to_f32()), (1, 0.0), "{sq:?}");
    }

    #[test]
    fn what_follows_a_row_of_two_split_cells_starts_below_the_taller_one() {
        // Cells of one row split across pages: the row ends where its tallest cell ends, whichever
        // cell is processed last (F30b: direct text; the same holds for `<td><p>`).
        for (what, a, b) in [
            ("direct text", br_lines(30), br_lines(20)),
            (
                "paragraphs",
                format!("<p>{}</p>", br_lines(30)),
                format!("<p>{}</p>", br_lines(20)),
            ),
            ("taller last", br_lines(20), br_lines(30)),
        ] {
            let (doc, table) = nested_geometry(
                &format!(
                    "<table style=\"width:100%\"><tr><td id=\"a\" style=\"line-height:20px;vertical-align:top\">{a}</td>\
                     <td id=\"b\" style=\"line-height:20px;vertical-align:top\">{b}</td></tr></table>\
                     <p id=\"after\">AFTER</p>"
                ),
                200.0,
            );
            let after = &fragments_of(&doc, &table, "after")[0];
            for id in ["a", "b"] {
                let last = fragments_of(&doc, &table, id).last().unwrap();
                let below = last.y.to_f32() + last.height.to_f32();
                assert!(
                    after.page_index > last.page_index
                        || (after.page_index == last.page_index && after.y.to_f32() >= below - 0.5),
                    "{what}: AFTER {after:?} is not below cell {id} ending {last:?}"
                );
            }
        }
    }

    #[test]
    fn a_row_of_paragraph_cells_ends_below_the_tallest_cells_content() {
        // `<td><p>` cells are stretched to the row: the stretch below a shorter cell's last line is
        // not content and must not decide where what follows the row starts, whatever the order.
        for (what, n) in [
            ("tallest last", [18, 12, 24]),
            ("tallest first", [24, 18, 12]),
            ("tallest in the middle", [12, 24, 18]),
        ] {
            let cells: String = ["pa", "pb", "pc"]
                .iter()
                .zip(n)
                .map(|(id, n)| {
                    format!(
                        "<td style=\"line-height:20px;vertical-align:top\"><p id=\"{id}\">{}</p></td>",
                        br_lines(n)
                    )
                })
                .collect();
            let (doc, table) = nested_geometry(
                &format!(
                    "<table style=\"width:100%\"><tr>{cells}</tr></table><p id=\"after\">AFTER</p>"
                ),
                200.0,
            );
            let tallest = ["pa", "pb", "pc"][n.iter().position(|&x| x == 24).unwrap()];
            let last = fragments_of(&doc, &table, tallest).last().unwrap();
            let after = &fragments_of(&doc, &table, "after")[0];
            let bottom = last.y.to_f32() + last.height.to_f32();
            assert_eq!(
                after.page_index, last.page_index,
                "{what}: {after:?} vs {last:?}"
            );
            assert!(
                after.y.to_f32() >= bottom - 0.5 && after.y.to_f32() <= bottom + 10.0,
                "{what}: AFTER at {} but the tallest cell's text ends at {bottom}",
                after.y.to_f32()
            );
        }
    }

    #[test]
    fn the_last_childs_bottom_margin_stays_part_of_a_split_cells_row() {
        // A margin below the last child of a cell is inside the cell (a table cell is a block
        // formatting context): what follows the row starts that far below the last line, also when
        // the margin sits on a wrapper, collapses through one, or a shorter cell is stretched.
        for (what, a, margin) in [
            (
                "p",
                format!(
                    "<p id=\"last\" style=\"margin-bottom:40px\">{}</p>",
                    br_lines(30)
                ),
                40.0,
            ),
            (
                "wrapper",
                format!(
                    "<div style=\"margin-bottom:40px\"><p id=\"last\">{}</p></div>",
                    br_lines(30)
                ),
                40.0,
            ),
            (
                "collapsed through a wrapper",
                format!(
                    "<div><p id=\"last\" style=\"margin-bottom:40px\">{}</p></div>",
                    br_lines(30)
                ),
                40.0,
            ),
        ] {
            let (doc, table) = nested_geometry(
                &format!(
                    "<table style=\"width:100%\"><tr><td style=\"line-height:20px;vertical-align:top\">{a}</td>\
                     <td style=\"line-height:20px;vertical-align:top\"><p>{}</p></td></tr></table>\
                     <p id=\"after\">AFTER</p>",
                    br_lines(12)
                ),
                200.0,
            );
            let last = fragments_of(&doc, &table, "last").last().unwrap();
            let after = &fragments_of(&doc, &table, "after")[0];
            let bottom = last.y.to_f32() + last.height.to_f32();
            assert_eq!(
                after.page_index, last.page_index,
                "{what}: {after:?} vs {last:?}"
            );
            assert!(
                (after.y.to_f32() - (bottom + margin)).abs() < 1.5,
                "{what}: AFTER at {} but the last line ends at {bottom} and the margin is {margin}",
                after.y.to_f32()
            );
        }
    }

    #[test]
    fn a_later_item_of_a_grid_row_does_not_move_the_row() {
        // F28: only the first item of a row decides. In the second row the paragraph sits beside
        // a square that fits where it is; the paragraph starts there too, with the one line that
        // fits, instead of cutting the row in two with a page break.
        let (doc, table) = nested_geometry(
            &format!(
                "<div style=\"display:grid;grid-template-columns:100px 100px;align-items:start\">\
                 <div style=\"height:170px\"></div><div></div>\
                 <div id=\"sq\" style=\"height:20px\"></div><p id=\"p\">{}</p></div>",
                br_lines(30)
            ),
            200.0,
        );
        let sq = &fragments_of(&doc, &table, "sq")[0];
        let p = fragments_of(&doc, &table, "p");
        assert_eq!((sq.page_index, sq.y.to_f32()), (0, 170.0), "{sq:?}");
        assert_eq!((p[0].page_index, p[0].y.to_f32()), (0, 170.0), "{p:?}");
    }

    #[test]
    fn a_table_row_starts_in_place_beside_a_direct_text_cell_taller_than_a_page() {
        let (doc, _) = nested_geometry(
            &format!(
                "<table><tr><td id=\"short\">x</td><td id=\"tall\">{}</td></tr></table>",
                br_lines(30)
            ),
            200.0,
        );
        let table_node = doc
            .get_node(find_by_id(doc.deref(), "short").unwrap())
            .and_then(|c| c.parent)
            .and_then(|p| doc.get_node(p))
            .and_then(|p| p.parent)
            .and_then(|p| doc.get_node(p))
            .and_then(|p| p.parent)
            .and_then(|p| doc.get_node(p))
            .expect("table");
        let kids = table_node.layout_children.borrow();
        let ids: Vec<usize> = kids.as_deref().map(<[usize]>::to_vec).unwrap_or_default();
        let short = find_by_id(doc.deref(), "short").unwrap();
        let tall = find_by_id(doc.deref(), "tall").unwrap();
        assert!(super::row_mate_can_split(
            doc.deref(),
            table_node,
            &ids,
            short,
            200.0
        ));
        // The tall cell itself has no taller mate.
        assert!(!super::row_mate_can_split(
            doc.deref(),
            table_node,
            &ids,
            tall,
            200.0
        ));
    }

    #[test]
    fn leading_block_size_counts_two_lines_of_a_paragraph_taller_than_a_page() {
        let html = format!(
            "<html><head>{LINE_CSS}</head><body><div id=\"d\"><p>{}</p></div></body></html>",
            br_lines(30)
        );
        let doc = parse(&html, 600.0);
        let d = find_by_id(doc.deref(), "d").unwrap();
        let tall = super::leading_block_size(doc.deref(), d, None, 0.0, 0, 200.0);
        assert!((tall - 40.0).abs() < 0.5, "{tall}");
        // On a page the paragraph fits, it counts whole.
        let whole = super::leading_block_size(doc.deref(), d, None, 0.0, 0, 10_000.0);
        assert!((whole - 600.0).abs() < 0.5, "{whole}");
    }

    #[test]
    fn inline_lines_below_the_page_are_reported_for_unsplit_paragraphs() {
        // A paragraph in a clipping box is placed whole, so its lines past the page are reported.
        let html = format!(
            "<html><head>{LINE_CSS}</head><body><div style=\"display:flex;overflow:hidden\"><p id=\"f\">{}</p></div></body></html>",
            br_lines(30)
        );
        let doc = parse(&html, 600.0);
        let f = doc.get_node(find_by_id(doc.deref(), "f").unwrap()).unwrap();
        let (lines, below) = inline_lines_below_page(f, 0.0, 200.0).expect("overflows");
        assert_eq!(lines, 30);
        assert!((below - 400.0).abs() < 0.5, "{below}");
        assert!(inline_lines_below_page(f, 0.0, 1000.0).is_none());
    }

    #[test]
    fn plan_line_breaks_leaves_room_below_the_last_line() {
        // 5 lines fit; the last strip also needs 10px below its last line: a line moves on
        // (and a widow is avoided by taking two).
        let r: Vec<_> = plan_line_breaks(&uniform_lines(10), 100.0, 100.0, 10.0, usize::MAX)
            .into_iter()
            .map(|r| (r.start, r.end))
            .collect();
        assert_eq!(r, vec![(0, 5), (5, 8), (8, 10)]);
        // Without the extra, the same lines end exactly on the strip.
        assert_eq!(ranges_of(10, 100.0, 100.0), vec![(0, 5), (5, 10)]);
        // A single line cannot move: it overflows rather than vanish.
        let r = plan_line_breaks(&uniform_lines(1), 20.0, 20.0, 10.0, usize::MAX);
        assert_eq!(r, vec![0..1]);
    }

    #[test]
    fn the_body_offset_is_not_extra_room_on_page_zero() {
        // Body margin 40px: the renderer shifts page 0 down by it, so 160px of 200px are left.
        let html = format!(
            "<html><head>{LINE_CSS}<style>body{{margin:40px}}</style></head><body><div><p id=\"p\">{}</p></div></body></html>",
            br_lines(30)
        );
        let mut doc = parse(&html, 600.0);
        let table = run_pass(&mut doc, 200.0);
        let frags = fragments_of(&doc, &table, "p");
        assert!(
            (frags[0].height.to_f32() - 160.0).abs() < 0.5 && frags[0].page_index == 0,
            "{frags:?}"
        );
        assert_eq!(frags[1].page_index, 1);
        assert!((frags[1].height.to_f32() - 200.0).abs() < 0.5, "{frags:?}");
    }

    #[test]
    fn outer_frames_end_at_the_page_bottom_where_a_nested_paragraph_splits() {
        let (doc, table) = nested_geometry(
            &format!(
                "<div id=\"outer\" style=\"padding:10px\"><div id=\"inner\"><p>{}</p></div></div>",
                br_lines(30)
            ),
            200.0,
        );
        for id in ["outer", "inner"] {
            let frags = fragments_of(&doc, &table, id);
            assert!(frags.len() >= 3, "{id}: {frags:?}");
            for f in &frags[..frags.len() - 1] {
                assert!(
                    (f.y.to_f32() + f.height.to_f32() - 200.0).abs() < 0.5,
                    "{id}: {f:?}"
                );
            }
        }
    }

    #[test]
    fn the_last_strip_leaves_room_for_the_padding_of_the_boxes_around() {
        // 50 lines of 20px on a 1000px page strip: the box padding (2 x 20px) does not fit under the
        // last line, so a line moves to the next page.
        let (doc, table) = nested_geometry(
            &format!(
                "<div id=\"b\" style=\"padding:20px\"><p id=\"p\">{}</p></div>",
                br_lines(60)
            ),
            1000.0,
        );
        let frags = fragments_of(&doc, &table, "p");
        let last = frags.last().unwrap();
        let b_last = fragments_of(&doc, &table, "b").last().unwrap().clone();
        assert!(
            b_last.y.to_f32() + b_last.height.to_f32() <= 1000.5,
            "{b_last:?} {last:?}"
        );
    }

    #[test]
    fn table_cells_after_a_split_cell_start_at_the_row_top() {
        let (doc, table) = nested_geometry(
            &format!(
                "<table style=\"width:100%\"><tr><td id=\"a\"><p>{}</p></td><td id=\"b\">x</td><td id=\"c\">y</td></tr></table><p id=\"after\">after</p>",
                br_lines(30)
            ),
            200.0,
        );
        let a = fragments_of(&doc, &table, "a");
        assert!(a.len() >= 3, "{a:?}");
        for id in ["b", "c"] {
            let f = fragments_of(&doc, &table, id);
            assert_eq!(f.len(), 1, "{id}: {f:?}");
            assert_eq!(f[0].page_index, 0, "{id}");
            assert!(
                (f[0].y.to_f32() - a[0].y.to_f32()).abs() < 0.5,
                "{id}: {f:?} vs {a:?}"
            );
        }
        // What follows the table continues below the split cell.
        let after = &fragments_of(&doc, &table, "after")[0];
        let a_last = a.last().unwrap();
        assert_eq!(after.page_index, a_last.page_index);
        assert!(after.y.to_f32() >= a_last.y.to_f32() + a_last.height.to_f32() - 0.5);
    }

    #[test]
    fn a_bottom_padding_taller_than_a_page_neither_adds_pages_nor_runs_past_the_page() {
        let (doc, table) = nested_geometry(
            &format!(
                "<div id=\"b\" style=\"padding-bottom:300px\"><div><p id=\"p\">{}</p></div></div>",
                br_lines(25)
            ),
            200.0,
        );
        // 25 lines of 20px on 200px pages: 10 + 10 + 5 lines.
        let p = fragments_of(&doc, &table, "p");
        assert_eq!(p.len(), 3, "{p:?}");
        assert!((p[2].height.to_f32() - 100.0).abs() < 0.5, "{p:?}");
        for f in fragments_of(&doc, &table, "b") {
            assert!(f.y.to_f32() + f.height.to_f32() <= 200.5, "{f:?}");
        }
    }

    // ---- F29: page 0 is shorter by the body's own offset -----------------

    /// `body` styled with `body_css` (after `LINE_CSS`, so it wins), paginated on
    /// `page_h`-px pages.
    fn offset_geometry(
        body_css: &str,
        body: &str,
        page_h: f32,
    ) -> (blitz_html::HtmlDocument, PaginationGeometryTable) {
        let mut doc = parse(
            &format!(
                "<html><head>{LINE_CSS}<style>{body_css}</style></head><body>{body}</body></html>"
            ),
            600.0,
        );
        let table = run_pass(&mut doc, page_h);
        (doc, table)
    }

    #[test]
    fn two_monolithic_blocks_under_a_body_margin_do_not_share_page_zero() {
        // 40 + 500 + 500 > 1000: the second block moves to page 1 (it used to stay at y = 500, ending 1040px
        // down the page, below the content box).
        let body = "<div id=\"a\" style=\"height:500px\"></div><div id=\"b\" style=\"height:500px\"></div>";
        let (doc, table) = offset_geometry("body{margin:40px}", body, 1000.0);
        let a = &fragments_of(&doc, &table, "a")[0];
        let b = &fragments_of(&doc, &table, "b")[0];
        assert_eq!((a.page_index, b.page_index), (0, 1), "{a:?} {b:?}");
        assert!(b.y.to_f32().abs() < 0.5, "{b:?}");
        // Without an offset the pair still fits.
        let (doc, table) = offset_geometry("body{margin:0}", body, 1000.0);
        assert_eq!(fragments_of(&doc, &table, "b")[0].page_index, 0);
    }

    #[test]
    fn a_body_level_paragraph_splits_at_the_shortened_page_zero() {
        // 30 lines of 20px on 200px pages under a 40px body margin: 8 lines (160px) on page 0, then 10 + 10 + 2.
        let (doc, table) = offset_geometry(
            "body{margin:40px}",
            &format!("<p id=\"p\">{}</p>", br_lines(30)),
            200.0,
        );
        let f = fragments_of(&doc, &table, "p");
        let heights: Vec<(u32, i32)> = f
            .iter()
            .map(|f| (f.page_index, f.height.to_f32().round() as i32))
            .collect();
        assert_eq!(
            heights,
            vec![(0, 160), (1, 200), (2, 200), (3, 40)],
            "{f:?}"
        );
    }

    #[test]
    fn a_paragraph_after_a_spacer_moves_whole_when_page_zero_has_no_room() {
        // 100px spacer, 40px offset, 5 lines of 20px: 100 + 100 fits a 200px page but not the 160px that
        // page 0 has, so the paragraph (which is not taller than a page) starts page 1.
        let body = format!(
            "<div style=\"height:100px\"></div><p id=\"p\">{}</p>",
            br_lines(5)
        );
        let (doc, table) = offset_geometry("body{margin:40px}", &body, 200.0);
        let f = fragments_of(&doc, &table, "p");
        assert_eq!((f.len(), f[0].page_index), (1, 1), "{f:?}");
        let (doc, table) = offset_geometry("body{margin:0}", &body, 200.0);
        assert_eq!(fragments_of(&doc, &table, "p")[0].page_index, 0);
    }

    #[test]
    fn a_margin_collapsed_through_body_is_page_zero_s_offset() {
        // The first paragraph's 60px top margin collapses through <body>: page 0 starts 60px down, 140px
        // are left. The second paragraph sits at 140px in body coordinates, 200px down the page: page 1.
        let body = "<p id=\"a\" style=\"margin-top:60px;height:20px\"></p>\
                    <p id=\"b\" style=\"margin-top:120px;height:20px\"></p>";
        let (doc, table) = offset_geometry("body{margin:0}", body, 200.0);
        let b = &fragments_of(&doc, &table, "b")[0];
        assert_eq!(b.page_index, 1, "{b:?}");
        assert!(
            b.y.to_f32().abs() < 0.5,
            "margins truncate at the break: {b:?}"
        );
        assert_eq!(fragments_of(&doc, &table, "a")[0].page_index, 0);
    }

    #[test]
    fn a_block_taller_than_a_page_is_sliced_to_the_shortened_page_zero() {
        // 1500px block under a 40px margin on 1000px pages: 960 + 540.
        let (doc, table) = offset_geometry(
            "body{margin:40px}",
            "<div id=\"d\" style=\"height:1500px\"></div>",
            1000.0,
        );
        let f = fragments_of(&doc, &table, "d");
        let slices: Vec<(u32, i32)> = f
            .iter()
            .map(|f| (f.page_index, f.height.to_f32().round() as i32))
            .collect();
        assert_eq!(slices, vec![(0, 960), (1, 540)], "{f:?}");
    }

    #[test]
    fn a_box_whose_first_row_does_not_fit_page_zero_moves_to_page_one() {
        // Spacer 140px, offset 40px, first row 30px: 140 + 30 > 160. Entering the box would hang the row
        // 10px past the content box of a 200px page.
        let rows: String = (0..5)
            .map(|i| format!("<div id=\"r{i}\" style=\"height:30px\"></div>"))
            .collect();
        let body = format!("<div style=\"height:140px\"></div><section id=\"s\">{rows}</section>");
        let (doc, table) = offset_geometry("body{margin:40px}", &body, 200.0);
        let r0 = &fragments_of(&doc, &table, "r0")[0];
        assert_eq!(r0.page_index, 1, "{r0:?}");
        assert!(r0.y.to_f32().abs() < 0.5, "{r0:?}");
        // With an offset of 0 the row fits where it is (140 + 30 <= 200).
        let (doc, table) = offset_geometry("body{margin:0}", &body, 200.0);
        assert_eq!(fragments_of(&doc, &table, "r0")[0].page_index, 0);
    }

    #[test]
    fn a_table_under_a_body_margin_breaks_its_rows_at_the_shortened_page_zero() {
        // 10 rows of 30px on 200px pages, 40px body margin: 5 rows (150px; the 6th would end at 180 > 160) on
        // page 0, the rest on page 1 from y = 0.
        let rows: String = (0..10)
            .map(|i| format!("<tr><td id=\"r{i}\" style=\"height:30px;padding:0\">x</td></tr>"))
            .collect();
        let body = format!("<table style=\"border-spacing:0;width:100%\">{rows}</table>");
        let (doc, table) = offset_geometry("body{margin:40px}", &body, 200.0);
        let pages: Vec<u32> = (0..10)
            .map(|i| fragments_of(&doc, &table, &format!("r{i}"))[0].page_index)
            .collect();
        assert_eq!(pages, vec![0, 0, 0, 0, 0, 1, 1, 1, 1, 1]);
        let r5 = &fragments_of(&doc, &table, "r5")[0];
        assert!(r5.y.to_f32().abs() < 0.5, "{r5:?}");
    }

    #[test]
    fn nested_monolithic_blocks_under_a_body_margin_do_not_share_page_zero() {
        let body = "<main><div id=\"a\" style=\"height:50px\"></div><div id=\"b\" style=\"height:50px\"></div>\
                    <div id=\"c\" style=\"height:50px\"></div><div id=\"d\" style=\"height:50px\"></div></main>";
        // 40 + 4 x 50 > 200 (the last block would end 20px past the content box): `d` moves to page 1.
        let (doc, table) = offset_geometry("body{margin:40px}", body, 200.0);
        let pages: Vec<u32> = ["a", "b", "c", "d"]
            .iter()
            .map(|id| fragments_of(&doc, &table, id)[0].page_index)
            .collect();
        assert_eq!(pages, vec![0, 0, 0, 1]);
        assert!(fragments_of(&doc, &table, "d")[0].y.to_f32().abs() < 0.5);
        let (doc, table) = offset_geometry("body{margin:0}", body, 200.0);
        assert_eq!(fragments_of(&doc, &table, "d")[0].page_index, 0);
    }

    #[test]
    fn a_nested_box_whose_first_row_does_not_fit_page_zero_moves_to_page_one() {
        // As at body level, one box deeper: 140px spacer + 30px row > 160px.
        let rows: String = (0..5)
            .map(|i| format!("<div id=\"r{i}\" style=\"height:30px\"></div>"))
            .collect();
        let body = format!(
            "<main><div style=\"height:140px\"></div><section id=\"s\">{rows}</section></main>"
        );
        let (doc, table) = offset_geometry("body{margin:40px}", &body, 200.0);
        let r0 = &fragments_of(&doc, &table, "r0")[0];
        assert_eq!(r0.page_index, 1, "{r0:?}");
        assert!(r0.y.to_f32().abs() < 0.5, "{r0:?}");
        let (doc, table) = offset_geometry("body{margin:0}", &body, 200.0);
        assert_eq!(fragments_of(&doc, &table, "r0")[0].page_index, 0);
    }

    #[test]
    fn a_nested_paragraph_after_a_spacer_starts_at_the_shortened_page_zero() {
        // 100px spacer, 40px offset: 60px are left on page 0 (3 lines), the paragraph of 30 lines is taller
        // than a page and splits there (F30) instead of hanging 3 lines below the content box.
        let body = format!(
            "<main><div style=\"height:100px\"></div><p id=\"p\">{}</p></main>",
            br_lines(30)
        );
        let (doc, table) = offset_geometry("body{margin:40px}", &body, 200.0);
        let f = fragments_of(&doc, &table, "p");
        assert_eq!(f[0].page_index, 0, "{f:?}");
        assert!(
            f[0].y.to_f32() + f[0].height.to_f32() <= 160.5,
            "page 0 ends at 160px: {f:?}"
        );
    }

    // ── F30d: plan_multicol_pages ───────────────────────────────────────────

    use crate::column_css::ColumnFill;
    use crate::multicol_layout::FlowItem;

    fn lines(source: usize, count: usize, h: f32) -> FlowItem {
        FlowItem::Lines {
            source,
            advances: vec![h.as_px(); count],
        }
    }

    /// [`plan_multicol_pages`] for a flow without tables.
    fn plan_no_tables(
        flow: &[FlowItem],
        n: u32,
        first_room: f32,
        page_room: f32,
        fill: ColumnFill,
        last_inset: f32,
    ) -> Vec<McPage> {
        plan_multicol_pages(
            flow,
            n,
            first_room,
            page_room,
            fill,
            last_inset,
            &BTreeMap::new(),
        )
    }

    fn block(node: usize, h: f32) -> FlowItem {
        FlowItem::Block {
            node,
            height: h.as_px(),
        }
    }

    /// `(first, end)` of the single `Lines` piece of `source` in `page.columns[col]`.
    fn line_range(page: &McPage, col: usize, source: usize) -> Option<(u32, u32)> {
        page.columns[col].iter().find_map(|p| match p {
            McPiece::Lines {
                source: s,
                first,
                end,
                ..
            } if *s == source => Some((*first, *end)),
            _ => None,
        })
    }

    /// Every line of `source` once, in order: pages in order, columns in order.
    fn assert_lines_tile(pages: &[McPage], source: usize, count: u32) {
        let mut next = 0;
        for (p, page) in pages.iter().enumerate() {
            for (c, col) in page.columns.iter().enumerate() {
                for piece in col {
                    if let McPiece::Lines {
                        source: s,
                        first,
                        end,
                        ..
                    } = piece
                    {
                        assert_eq!(*s, source);
                        assert_eq!(*first, next, "page {p} column {c} starts at {first}");
                        assert!(end > first, "empty piece on page {p} column {c}");
                        next = *end;
                    }
                }
            }
        }
        assert_eq!(next, count, "every line placed once");
    }

    #[test]
    fn plan_lines_over_many_pages_tile_without_gaps_or_repeats() {
        // 1000 lines of 20 px, two columns of 50 lines per page after a first page of 10 lines per column.
        let pages = plan_no_tables(
            &[lines(7, 1000, 20.0)],
            2,
            200.0,
            1000.0,
            ColumnFill::Balance,
            0.0,
        );
        assert_lines_tile(&pages, 7, 1000);
        assert_eq!(
            pages.len(),
            1 + 10,
            "10 + 10 on the first page, 100 per page after: 980 = 9.8 pages"
        );
        assert_eq!(line_range(&pages[0], 0, 7), Some((0, 10)));
        assert_eq!(line_range(&pages[0], 1, 7), Some((10, 20)));
        assert_eq!(pages[0].height.to_f32(), 200.0);
        assert_eq!(line_range(&pages[1], 0, 7), Some((20, 70)));
        assert_eq!(line_range(&pages[1], 1, 7), Some((70, 120)));
        // 980 lines after page 0 = 9 full pages (900) + 80 lines balanced 40 / 40.
        let last = pages.last().unwrap();
        let a = line_range(last, 0, 7).unwrap();
        let b = line_range(last, 1, 7).unwrap();
        assert_eq!((a.1 - a.0, b.1 - b.0), (40, 40));
        assert_eq!(last.height.to_f32(), 800.0);
        for page in &pages[..pages.len() - 1] {
            assert_eq!(page.height.to_f32() as u32 % 200, 0);
        }
    }

    #[test]
    fn plan_places_one_unit_per_column_when_the_first_room_is_tiny() {
        // A first room smaller than a line: the first column still takes a line (the caller moves the container
        // to the next page when its first unit does not fit, F28), the second does not: a line below the room of
        // a partial page goes to the next page, which it fits (html-to-pdf F161).
        let pages = plan_no_tables(
            &[lines(1, 10, 20.0)],
            2,
            5.0,
            100.0,
            ColumnFill::Balance,
            0.0,
        );
        assert_lines_tile(&pages, 1, 10);
        assert_eq!(line_range(&pages[0], 0, 1), Some((0, 1)));
        assert_eq!(line_range(&pages[0], 1, 1), None);
        assert_eq!(pages[0].height.to_f32(), 20.0);
        // The remaining 9 lines are balanced on page 2: 5 + 4.
        assert_eq!(pages.len(), 2);
        assert_eq!(line_range(&pages[1], 0, 1), Some((1, 6)));
        assert_eq!(line_range(&pages[1], 1, 1), Some((6, 10)));
        // On fresh pages every column takes a unit, whatever its height.
        let pages = plan_no_tables(&[lines(1, 4, 20.0)], 2, 5.0, 5.0, ColumnFill::Balance, 0.0);
        assert_lines_tile(&pages, 1, 4);
        assert_eq!(pages.len(), 2);
    }

    #[test]
    fn plan_balance_and_auto_differ_only_on_the_last_page() {
        let flow = [lines(1, 30, 20.0)];
        let balanced = plan_no_tables(&flow, 2, 1000.0, 1000.0, ColumnFill::Balance, 0.0);
        assert_eq!(balanced.len(), 1);
        assert_eq!(line_range(&balanced[0], 0, 1), Some((0, 15)));
        assert_eq!(line_range(&balanced[0], 1, 1), Some((15, 30)));
        assert_eq!(balanced[0].height.to_f32(), 300.0);
        // `auto` never balances: the first column takes all that fits.
        let auto = plan_no_tables(&flow, 2, 1000.0, 1000.0, ColumnFill::Auto, 0.0);
        assert_eq!(auto.len(), 1);
        assert_eq!(line_range(&auto[0], 0, 1), Some((0, 30)));
        assert_eq!(line_range(&auto[0], 1, 1), None);
        assert_eq!(auto[0].height.to_f32(), 600.0);
        // Earlier pages are filled in both modes: 45 lines over pages of 10 lines per column.
        for fill in [ColumnFill::Balance, ColumnFill::Auto] {
            let pages = plan_no_tables(&[lines(1, 45, 20.0)], 2, 200.0, 200.0, fill, 0.0);
            assert_lines_tile(&pages, 1, 45);
            assert_eq!(line_range(&pages[0], 1, 1), Some((10, 20)));
            assert_eq!(line_range(&pages[1], 0, 1), Some((20, 30)));
            assert_eq!(pages.len(), 3);
        }
    }

    #[test]
    fn plan_a_box_taller_than_the_room_of_a_partial_page_moves_to_the_next_page() {
        // html-to-pdf F161 (fuzz seed 2434): 5 lines, then a 300 px box, in two columns with 200 px of room left
        // on the first page (under a spanner) and 800 on a fresh one. The box does not fit the second column of
        // the first page but fits a fresh page: it goes there instead of running past the page bottom.
        let flow = [lines(1, 5, 20.0), block(2, 300.0), lines(3, 4, 20.0)];
        let pages = plan_no_tables(&flow, 2, 200.0, 800.0, ColumnFill::Balance, 0.0);
        assert_eq!(pages.len(), 2);
        assert_eq!(line_range(&pages[0], 0, 1), Some((0, 5)));
        assert!(pages[0].height.to_f32() <= 200.0, "{:?}", pages[0].height);
        assert!(pages[0].columns[1].is_empty(), "{:?}", pages[0].columns[1]);
        assert_eq!(
            pages[1].columns[0][0],
            McPiece::Block {
                node: 2,
                y: 0.0.as_px()
            }
        );
        // A box taller than a fresh page still goes in whole (and overflows) rather than loop.
        let pages = plan_no_tables(
            &[block(4, 900.0)],
            2,
            200.0,
            800.0,
            ColumnFill::Balance,
            0.0,
        );
        assert_eq!(pages.len(), 1);
        let pages = plan_no_tables(
            &[lines(5, 1, 20.0), block(4, 900.0)],
            2,
            200.0,
            800.0,
            ColumnFill::Balance,
            0.0,
        );
        assert_eq!(pages.len(), 2);
        assert_eq!(
            pages[1].columns[0],
            vec![McPiece::Block {
                node: 4,
                y: 0.0.as_px()
            }]
        );
    }

    #[test]
    fn plan_mixed_blocks_and_lines() {
        // A heading, a paragraph of 12 lines, another box: room of 160 per column.
        let flow = [block(10, 30.0), lines(11, 12, 20.0), block(12, 50.0)];
        let pages = plan_no_tables(&flow, 2, 160.0, 160.0, ColumnFill::Balance, 0.0);
        // Page 0: heading 30 + 6 lines (150 <= 160); column 2: 6 lines (120), then the 50 px box does not fit.
        assert_eq!(pages.len(), 2);
        assert_eq!(
            pages[0].columns[0][0],
            McPiece::Block {
                node: 10,
                y: 0.0.as_px()
            }
        );
        assert_eq!(line_range(&pages[0], 0, 11), Some((0, 6)));
        assert_eq!(
            pages[0].columns[0][1],
            McPiece::Lines {
                source: 11,
                first: 0,
                end: 6,
                y: 30.0.as_px()
            }
        );
        assert_eq!(line_range(&pages[0], 1, 11), Some((6, 12)));
        assert_eq!(
            pages[1].columns[0],
            vec![McPiece::Block {
                node: 12,
                y: 0.0.as_px()
            }]
        );
        assert!(pages[1].columns[1].is_empty());
        assert_eq!(pages[1].height.to_f32(), 50.0);
    }

    #[test]
    fn plan_a_block_taller_than_a_page_is_placed_alone() {
        let flow = [lines(1, 3, 20.0), block(2, 3000.0), lines(3, 3, 20.0)];
        let pages = plan_no_tables(&flow, 2, 1000.0, 1000.0, ColumnFill::Balance, 0.0);
        // The block does not fit under the three lines: it opens column 2 and overflows it.
        assert_eq!(line_range(&pages[0], 0, 1), Some((0, 3)));
        assert_eq!(
            pages[0].columns[1],
            vec![McPiece::Block {
                node: 2,
                y: 0.0.as_px()
            }]
        );
        assert_eq!(pages[0].height.to_f32(), 3000.0);
        // Everything after it goes on the next page.
        assert_eq!(pages.len(), 2);
        assert_eq!(line_range(&pages[1], 0, 3), Some((0, 2)));
        assert_eq!(line_range(&pages[1], 1, 3), Some((2, 3)));
        // A block taller than the page is also alone in the first column of a page.
        let pages = plan_no_tables(
            &[block(2, 3000.0)],
            2,
            100.0,
            100.0,
            ColumnFill::Balance,
            0.0,
        );
        assert_eq!(pages.len(), 1);
        assert_eq!(pages[0].height.to_f32(), 3000.0);
    }

    #[test]
    fn plan_three_columns() {
        let pages = plan_no_tables(
            &[lines(1, 100, 20.0)],
            3,
            400.0,
            400.0,
            ColumnFill::Balance,
            0.0,
        );
        assert_lines_tile(&pages, 1, 100);
        assert_eq!(pages.len(), 2);
        assert_eq!(pages[0].columns.len(), 3);
        // Page 0 holds 3 x 20 lines; the 40 lines left balance to a budget of 14 lines: 14 / 14 / 12.
        assert_eq!(line_range(&pages[0], 2, 1), Some((40, 60)));
        let sizes: Vec<u32> = (0..3)
            .map(|c| {
                let (a, b) = line_range(&pages[1], c, 1).unwrap();
                b - a
            })
            .collect();
        assert_eq!(sizes, vec![14, 14, 12]);
    }

    #[test]
    fn plan_keeps_the_bottom_inset_free_on_the_last_page_only() {
        // 50 lines of 20 px, columns of 10 lines, 30 px of bottom padding under the last page's columns.
        let flow = [lines(1, 50, 20.0)];
        let pages = plan_no_tables(&flow, 2, 200.0, 200.0, ColumnFill::Auto, 30.0);
        assert_lines_tile(&pages, 1, 50);
        assert_eq!(pages.len(), 3);
        // Not the last page: filled to the room, ignoring the inset.
        assert_eq!(line_range(&pages[0], 1, 1), Some((10, 20)));
        assert_eq!(pages[0].height.to_f32(), 200.0);
        // The last page keeps 30 px free: 8 lines of 160 px fit 170, a 9th would not.
        assert_eq!(line_range(&pages[2], 0, 1), Some((40, 48)));
        assert_eq!(line_range(&pages[2], 1, 1), Some((48, 50)));
        // A rest that fits the room but not the room less the inset hangs the padding below the page and
        // is balanced like any last page.
        let pages = plan_no_tables(
            &[lines(1, 18, 20.0)],
            2,
            200.0,
            200.0,
            ColumnFill::Balance,
            30.0,
        );
        assert_eq!(pages.len(), 1);
        assert_eq!(line_range(&pages[0], 0, 1), Some((0, 9)));
        assert_eq!(line_range(&pages[0], 1, 1), Some((9, 18)));
    }

    #[test]
    fn plan_empty_flow_and_empty_paragraph() {
        assert!(plan_no_tables(&[], 2, 100.0, 100.0, ColumnFill::Balance, 0.0).is_empty());
        assert!(
            plan_no_tables(
                &[lines(1, 0, 20.0)],
                2,
                100.0,
                100.0,
                ColumnFill::Balance,
                0.0
            )
            .is_empty()
        );
    }

    // ── F174: tables in the multicol planner ────────────────────────────────

    /// A table of bands of `heights`, the first `head_bands` of which are a header of block-size `head_h`.
    fn table_of(heights: &[f32], head: Option<(u32, f32)>) -> McTable {
        let mut top = 0.0;
        let bands = heights
            .iter()
            .enumerate()
            .map(|(i, &h)| {
                let band = RowBand {
                    cells: i as u32..i as u32 + 1,
                    top,
                    height: h,
                };
                top += h;
                band
            })
            .collect();
        McTable {
            bands,
            head: head.map(|(bands, height)| McHead {
                group: 900,
                bands,
                height,
            }),
        }
    }

    fn tables_of(node: usize, table: McTable) -> BTreeMap<usize, McTable> {
        BTreeMap::from([(node, table)])
    }

    /// `(first, end, y, head)` of the `Rows` pieces of `table` in `page.columns[col]`.
    fn row_pieces(page: &McPage, col: usize, table: usize) -> Vec<(u32, u32, f32, bool)> {
        page.columns[col]
            .iter()
            .filter_map(|p| match p {
                McPiece::Rows {
                    table: t,
                    first,
                    end,
                    y,
                    head,
                } if *t == table => Some((*first, *end, y.to_f32(), *head)),
                _ => None,
            })
            .collect()
    }

    /// Every band of `table` once, in order: pages in order, columns in order, pieces in order.
    fn assert_rows_tile(pages: &[McPage], table: usize, bands: u32) {
        let mut next = 0;
        for (p, page) in pages.iter().enumerate() {
            for c in 0..page.columns.len() {
                for (first, end, _, _) in row_pieces(page, c, table) {
                    assert_eq!(first, next, "page {p} column {c} starts at band {first}");
                    assert!(end > first, "empty piece on page {p} column {c}");
                    next = end;
                }
            }
        }
        assert_eq!(next, bands, "every band placed once");
    }

    #[test]
    fn plan_table_rows_tile_and_add_up_to_the_table() {
        // 30 bands of 20 px in two columns of 100 px, 200 px pages: 5 + 5 bands on every full page.
        let table = table_of(&[20.0; 30], None);
        assert_eq!(table.height_of(0, 30), 600.0);
        let pages = plan_multicol_pages(
            &[block(7, 600.0)],
            2,
            200.0,
            200.0,
            ColumnFill::Auto,
            0.0,
            &tables_of(7, table),
        );
        assert_rows_tile(&pages, 7, 30);
        assert_eq!(pages.len(), 2, "20 bands a page, 10 on the second");
        assert_eq!(row_pieces(&pages[0], 0, 7), vec![(0, 10, 0.0, false)]);
        assert_eq!(row_pieces(&pages[0], 1, 7), vec![(10, 20, 0.0, false)]);
        assert_eq!(row_pieces(&pages[1], 0, 7), vec![(20, 30, 0.0, false)]);
        assert!(pages[1].columns[1].is_empty());
        assert_eq!(pages[0].height.to_f32(), 200.0);
        assert_eq!(pages[1].height.to_f32(), 200.0);
    }

    #[test]
    fn plan_table_balances_its_last_page_and_a_short_table_splits_over_the_columns() {
        // 20 bands of 20 px, a 1000 px page: balanced 10 + 10 (c_short).
        let pages = plan_multicol_pages(
            &[block(7, 400.0)],
            2,
            1000.0,
            1000.0,
            ColumnFill::Balance,
            0.0,
            &tables_of(7, table_of(&[20.0; 20], None)),
        );
        assert_eq!(pages.len(), 1);
        assert_eq!(row_pieces(&pages[0], 0, 7), vec![(0, 10, 0.0, false)]);
        assert_eq!(row_pieces(&pages[0], 1, 7), vec![(10, 20, 0.0, false)]);
        assert_eq!(pages[0].height.to_f32(), 200.0);
    }

    #[test]
    fn plan_table_reserves_the_header_band_at_continuation_columns() {
        // A header band of 20 px and 40 body bands of 20 px; columns of 100 px.
        let table = table_of(&[vec![20.0], vec![20.0; 40]].concat(), Some((1, 20.0)));
        let pages = plan_multicol_pages(
            &[block(7, 820.0)],
            2,
            100.0,
            100.0,
            ColumnFill::Auto,
            0.0,
            &tables_of(7, table),
        );
        assert_rows_tile(&pages, 7, 41);
        // Column 1: the header and four rows. Every later column: the repeated header (20 px) and four rows.
        assert_eq!(row_pieces(&pages[0], 0, 7), vec![(0, 5, 0.0, false)]);
        assert_eq!(row_pieces(&pages[0], 1, 7), vec![(5, 9, 0.0, true)]);
        assert_eq!(row_pieces(&pages[1], 0, 7), vec![(9, 13, 0.0, true)]);
        assert_eq!(row_pieces(&pages[1], 1, 7), vec![(13, 17, 0.0, true)]);
        for page in &pages {
            for col in 0..2 {
                for (first, end, _, head) in row_pieces(page, col, 7) {
                    if head {
                        assert_eq!(end - first, 4);
                    }
                }
            }
            assert_eq!(page.height.to_f32(), 100.0);
        }
        // 41 bands: 5 + 4 * k; the last page holds the rest.
        let last = pages.last().unwrap();
        assert!(last.height.to_f32() <= 100.0);
    }

    #[test]
    fn plan_table_header_and_first_row_never_end_a_column() {
        // 80 px of lines fill a 100 px column but 20 px; the header (20 px) and the first row (20 px) are one
        // atom, which does not fit the 20 px left: the table starts in the second column.
        let table = table_of(&[vec![20.0], vec![20.0; 6]].concat(), Some((1, 20.0)));
        let pages = plan_multicol_pages(
            &[lines(1, 4, 20.0), block(7, 140.0)],
            2,
            100.0,
            100.0,
            ColumnFill::Auto,
            0.0,
            &tables_of(7, table),
        );
        assert_eq!(pages[0].columns[0].len(), 1, "only the lines");
        assert_eq!(row_pieces(&pages[0], 1, 7), vec![(0, 5, 0.0, false)]);
        assert_rows_tile(&pages, 7, 7);
        // Without a header the first row alone fits the 20 px left.
        let pages = plan_multicol_pages(
            &[lines(1, 4, 20.0), block(7, 140.0)],
            2,
            100.0,
            100.0,
            ColumnFill::Auto,
            0.0,
            &tables_of(7, table_of(&[20.0; 7], None)),
        );
        assert_eq!(row_pieces(&pages[0], 0, 7), vec![(0, 1, 80.0, false)]);
    }

    #[test]
    fn plan_table_row_that_does_not_fit_with_the_band_gets_none() {
        // Columns of 100 px, a header of 30 px: a 75 px row fits a column alone but not with the band.
        let table = table_of(
            &[vec![30.0], vec![40.0], vec![75.0; 3]].concat(),
            Some((1, 30.0)),
        );
        let pages = plan_multicol_pages(
            &[block(7, 295.0)],
            2,
            100.0,
            100.0,
            ColumnFill::Auto,
            0.0,
            &tables_of(7, table),
        );
        assert_rows_tile(&pages, 7, 5);
        assert_eq!(row_pieces(&pages[0], 0, 7), vec![(0, 2, 0.0, false)]);
        assert_eq!(row_pieces(&pages[0], 1, 7), vec![(2, 3, 0.0, false)]);
        assert_eq!(row_pieces(&pages[1], 0, 7), vec![(3, 4, 0.0, false)]);
        // The rows fit alone: no band was reserved.
        assert_eq!(pages[0].columns[1].len(), 1);
    }

    #[test]
    fn plan_table_balanced_last_page_repeats_the_header_over_every_piece() {
        // 100 px columns, a header of 20 px and 12 bands of 20 px: 4 + 4 on the first page (the header in the
        // second column), the last 4 are balanced 2 + 2, each column with the header: 60 px, not 40.
        let table = table_of(&[vec![20.0], vec![20.0; 12]].concat(), Some((1, 20.0)));
        let pages = plan_multicol_pages(
            &[block(7, 260.0)],
            2,
            100.0,
            100.0,
            ColumnFill::Balance,
            0.0,
            &tables_of(7, table),
        );
        assert_rows_tile(&pages, 7, 13);
        assert_eq!(row_pieces(&pages[0], 0, 7), vec![(0, 5, 0.0, false)]);
        assert_eq!(row_pieces(&pages[0], 1, 7), vec![(5, 9, 0.0, true)]);
        let last = pages.last().unwrap();
        assert_eq!(row_pieces(last, 0, 7), vec![(9, 11, 0.0, true)]);
        assert_eq!(row_pieces(last, 1, 7), vec![(11, 13, 0.0, true)]);
        assert_eq!(last.height.to_f32(), 60.0);
        // Two rows left: one per column, each below the header.
        let table = table_of(&[vec![20.0], vec![20.0; 10]].concat(), Some((1, 20.0)));
        let pages = plan_multicol_pages(
            &[block(7, 220.0)],
            2,
            100.0,
            100.0,
            ColumnFill::Balance,
            0.0,
            &tables_of(7, table),
        );
        let last = pages.last().unwrap();
        assert_eq!(row_pieces(last, 0, 7), vec![(9, 10, 0.0, true)]);
        assert_eq!(row_pieces(last, 1, 7), vec![(10, 11, 0.0, true)]);
        assert_eq!(last.height.to_f32(), 40.0);
    }

    #[test]
    fn plan_table_band_taller_than_a_column_is_placed_alone() {
        let table = table_of(&[20.0, 250.0, 20.0], None);
        let pages = plan_multicol_pages(
            &[block(7, 290.0)],
            2,
            100.0,
            100.0,
            ColumnFill::Auto,
            0.0,
            &tables_of(7, table),
        );
        assert_rows_tile(&pages, 7, 3);
        assert_eq!(row_pieces(&pages[0], 0, 7), vec![(0, 1, 0.0, false)]);
        assert_eq!(row_pieces(&pages[0], 1, 7), vec![(1, 2, 0.0, false)]);
        assert_eq!(pages.len(), 2);
    }

    #[test]
    fn plan_flow_without_tables_places_a_table_box_whole() {
        // A box whose node is not in the map is one atom, as before F174.
        let pages = plan_no_tables(&[block(7, 300.0)], 2, 100.0, 100.0, ColumnFill::Auto, 0.0);
        assert_eq!(pages.len(), 1);
        assert_eq!(
            pages[0].columns[0],
            vec![McPiece::Block {
                node: 7,
                y: 0.0.as_px()
            }]
        );
        assert!(pages[0].columns[1].is_empty());
    }

    #[test]
    fn flow_first_unit_of_a_table_is_its_header_and_first_row() {
        let flow = [block(7, 400.0)];
        assert_eq!(flow_first_unit(&flow, &BTreeMap::new()), 400.0);
        let with_head = tables_of(
            7,
            table_of(&[vec![30.0], vec![20.0; 18]].concat(), Some((1, 30.0))),
        );
        assert_eq!(flow_first_unit(&flow, &with_head), 50.0);
        let plain = tables_of(7, table_of(&[20.0; 20], None));
        assert_eq!(flow_first_unit(&flow, &plain), 20.0);
    }

    // ── F174: mc_table ──────────────────────────────────────────────────────

    /// Lay `html` out at 600 px and return the document and the node with class `t`.
    fn table_doc(html: &str) -> (blitz_html::HtmlDocument, usize) {
        let doc = parse(
            &format!(
                "<!doctype html><html><head><style>body{{margin:0}}\
                 table{{border-spacing:0}}td,th{{padding:0;line-height:20px}}</style></head>\
                 <body>{html}</body></html>"
            ),
            600.0,
        );
        let id = node_by_class(&doc, "t");
        (doc, id)
    }

    #[test]
    fn mc_table_bands_tile_the_table() {
        let (doc, t) = table_doc(
            "<table class=\"t\"><tbody>\
             <tr><td>a</td><td>b</td></tr><tr><td>c</td><td>d</td></tr>\
             <tr><td>e</td><td>f</td></tr></tbody></table>",
        );
        let table = mc_table(&doc, t, None, 1000.0).expect("a table with bands");
        let h = doc.get_node(t).unwrap().final_layout.size.height;
        assert_eq!(h, 60.0);
        let spans: Vec<_> = table.bands.iter().map(|b| b.cells.clone()).collect();
        assert_eq!(spans, vec![0..2, 2..4, 4..6]);
        assert_eq!(
            table.bands.iter().map(|b| b.top).collect::<Vec<_>>(),
            vec![0.0, 20.0, 40.0]
        );
        assert_eq!(table.height_of(0, 3), h);
        assert!(table.head.is_none());
    }

    #[test]
    fn row_bands_join_rowspans_and_refuse_cells_out_of_order() {
        // Two cells in the first row, one of them spanning the next (a `rowspan="2"` cell is 40 px tall), then
        // two rows: the first two rows are one band.
        let cells = [
            (0.0, 40.0),
            (0.0, 20.0),
            (20.0, 20.0),
            (40.0, 20.0),
            (40.0, 20.0),
            (60.0, 20.0),
        ];
        let bands = row_bands(&cells, 80.0).expect("bands");
        assert_eq!(
            bands.iter().map(|b| b.cells.clone()).collect::<Vec<_>>(),
            vec![0..3, 3..5, 5..6]
        );
        assert_eq!(
            bands.iter().map(|b| (b.top, b.height)).collect::<Vec<_>>(),
            vec![(0.0, 40.0), (40.0, 20.0), (60.0, 20.0)]
        );
        // A rowspan over every row leaves one band: nothing to cut between.
        assert!(row_bands(&[(0.0, 60.0), (20.0, 20.0), (40.0, 20.0)], 60.0).is_none());
        // Cells that are not in order are not cut.
        assert!(row_bands(&[(0.0, 20.0), (40.0, 20.0), (20.0, 20.0)], 60.0).is_none());
        // Spacing between the rows belongs to the band above; the first band starts at the table's top.
        let spaced = row_bands(&[(2.0, 20.0), (24.0, 20.0)], 46.0).expect("bands");
        assert_eq!(
            spaced.iter().map(|b| (b.top, b.height)).collect::<Vec<_>>(),
            vec![(0.0, 24.0), (24.0, 22.0)]
        );
        assert!(row_bands(&[(0.0, f32::NAN), (20.0, 20.0)], 40.0).is_none());
        assert!(row_bands(&[], 40.0).is_none());
    }

    #[test]
    fn mc_table_finds_the_header_bands() {
        let (doc, t) = table_doc(
            "<table class=\"t\"><thead><tr><th>h</th><th>i</th></tr></thead><tbody>\
             <tr><td>a</td><td>b</td></tr><tr><td>c</td><td>d</td></tr></tbody></table>",
        );
        let table = mc_table(&doc, t, None, 1000.0).expect("table");
        assert_eq!(table.bands.len(), 3);
        let head = table.head.as_ref().expect("header");
        assert_eq!((head.bands, head.height), (1, 20.0));
        // A header taller than a quarter of the page is not repeated (F12), the table is still cut.
        let cut = mc_table(&doc, t, None, 60.0).expect("table");
        assert!(cut.head.is_none());
        assert_eq!(cut.bands.len(), 3);
    }

    #[test]
    fn mc_table_refuses_what_it_cannot_cut() {
        // One row is one band.
        let (doc, t) = table_doc("<table class=\"t\"><tr><td>a</td><td>b</td></tr></table>");
        assert!(mc_table(&doc, t, None, 1000.0).is_none());
        // A box that is not a table.
        let (doc, t) = table_doc("<div class=\"t\"><p>a</p><p>b</p></div>");
        assert!(mc_table(&doc, t, None, 1000.0).is_none());
        // A clipping table.
        let (doc, t) = table_doc(
            "<table class=\"t\" style=\"overflow:hidden\"><tr><td>a</td></tr><tr><td>b</td></tr></table>",
        );
        assert!(mc_table(&doc, t, None, 1000.0).is_none());
    }

    #[test]
    fn mc_table_keeps_an_avoid_table_that_fits_a_page_whole() {
        let html = "<table class=\"t\" style=\"break-inside:avoid\">\
                    <tr><td>a</td></tr><tr><td>b</td></tr><tr><td>c</td></tr></table>";
        let doc = parse(
            &format!(
                "<!doctype html><html><head><style>body{{margin:0}}\
                 table{{border-spacing:0}}td{{padding:0;height:20px}}</style></head><body>{html}</body></html>"
            ),
            600.0,
        );
        let styles = blitz_adapter::extract_column_style_table(&doc);
        let t = node_by_class(&doc, "t");
        // Fits a 1000 px page: not cut. Taller than the page: relaxed, cut.
        assert!(mc_table(&doc, t, Some(&styles), 1000.0).is_none());
        assert!(mc_table(&doc, t, Some(&styles), 30.0).is_some());
        assert!(mc_table(&doc, t, None, 1000.0).is_some());
    }

    // ── F30d: fragment_multicol_container ───────────────────────────────────

    /// Lay `html` out, run the multicol pass, then the fragmenter on a `page_h` px strip.
    fn multicol_fragments(
        html: &str,
        page_h: f32,
    ) -> (
        blitz_html::HtmlDocument,
        crate::multicol_layout::MulticolGeometryTable,
        PaginationGeometryTable,
    ) {
        let mut doc = parse(html, 600.0);
        let table = blitz_adapter::extract_column_style_table(&doc);
        let mc = crate::multicol_layout::run_pass(doc.deref_mut(), &table);
        let geom = run_pass_with_multicol(doc.deref_mut(), page_h, &table, None, &mc);
        (doc, mc, geom)
    }

    fn nodes_by_class(doc: &BaseDocument, class: &str) -> Vec<usize> {
        fn walk(doc: &BaseDocument, id: usize, class: &str, out: &mut Vec<usize>) {
            let Some(n) = doc.get_node(id) else { return };
            if n.element_data()
                .and_then(|e| blitz_adapter::get_attr(e, "class"))
                .is_some_and(|c| c.split_whitespace().any(|x| x == class))
            {
                out.push(id);
            }
            for &c in &n.children {
                walk(doc, c, class, out);
            }
        }
        let mut out = Vec::new();
        walk(doc, doc.root_element().id, class, &mut out);
        out
    }

    fn node_by_class(doc: &BaseDocument, class: &str) -> usize {
        *nodes_by_class(doc, class)
            .first()
            .unwrap_or_else(|| panic!("no .{class}"))
    }

    /// Twelve 40 px boxes in two columns on 200 px pages: 5 + 5 on the first page, the last two balanced.
    #[test]
    fn multicol_blocks_are_cut_over_pages() {
        let html = format!(
            "<!doctype html><html><head><style>body{{margin:0}}\
             .mc{{column-count:2;column-gap:20px}}.b{{height:40px}}</style></head><body>\
             <div class=\"mc\">{}</div></body></html>",
            "<div class=\"b\"></div>".repeat(12)
        );
        let (doc, mc, geom) = multicol_fragments(&html, 200.0);
        let mc_id = node_by_class(&doc, "mc");
        let g = &geom[&mc_id];
        // Blitz laid the container out as 240 px tall (balanced against the one-column height), too tall for
        // a 200 px page.
        assert_eq!(g.fragments.len(), 2, "{:?}", g.fragments);
        assert_eq!(g.multicol_pages.len(), 2);
        assert_eq!(
            g.fragments.iter().map(|f| f.page_index).collect::<Vec<_>>(),
            vec![0, 1]
        );
        assert_eq!(g.fragments[0].y.to_f32(), 0.0);
        assert_eq!(g.fragments[0].height.to_f32(), 200.0);
        assert_eq!(g.fragments[1].height.to_f32(), 40.0);
        let col_w = mc[&mc_id].groups[0].col_w.to_f32();
        // Every box once, at the column and y the plan gave it.
        let boxes = nodes_by_class(&doc, "b");
        let mut placed: Vec<(u32, f32, f32)> = Vec::new();
        for (id, gg) in &geom {
            if boxes.contains(id) {
                for f in &gg.fragments {
                    placed.push((f.page_index, f.x.to_f32(), f.y.to_f32()));
                }
            }
        }
        placed.sort_by(|a, b| a.partial_cmp(b).unwrap());
        assert_eq!(placed.len(), 12);
        let col2 = col_w + 20.0;
        let expect: Vec<(u32, f32, f32)> = (0..5)
            .map(|i| (0, 0.0, i as f32 * 40.0))
            .chain((0..5).map(|i| (0, col2, i as f32 * 40.0)))
            .chain([(1, 0.0, 0.0), (1, col2, 0.0)])
            .collect();
        let mut expect = expect;
        expect.sort_by(|a, b| a.partial_cmp(b).unwrap());
        for (got, want) in placed.iter().zip(&expect) {
            assert_eq!(got.0, want.0);
            assert!((got.1 - want.1).abs() < 0.6, "{got:?} vs {want:?}");
            assert!((got.2 - want.2).abs() < 0.6, "{got:?} vs {want:?}");
        }
    }

    /// A paragraph in two columns over pages: every line is in exactly one piece, in order, one fragment of the
    /// paragraph per page.
    #[test]
    fn multicol_paragraph_lines_are_placed_once() {
        let words = (0..400)
            .map(|i| format!("word{i:03}"))
            .collect::<Vec<_>>()
            .join(" ");
        let html = format!(
            "<!doctype html><html><head><style>body{{margin:0;font-size:16px;line-height:20px}}\
             .mc{{column-count:2;column-gap:20px}}p{{margin:0}}</style></head><body>\
             <div class=\"mc\"><p class=\"t\">{words}</p></div></body></html>"
        );
        let (doc, mc, geom) = multicol_fragments(&html, 200.0);
        let mc_id = node_by_class(&doc, "mc");
        let p_id = node_by_class(&doc, "t");
        let lines: usize = mc[&mc_id].groups[0]
            .flow
            .iter()
            .map(|f| match f {
                crate::multicol_layout::FlowItem::Lines { advances, .. } => advances.len(),
                _ => 0,
            })
            .sum();
        assert!(lines > 40, "the paragraph has {lines} lines");
        let g = &geom[&mc_id];
        assert!(
            g.multicol_pages.len() >= 3,
            "{} pages",
            g.multicol_pages.len()
        );
        let mut next = 0u32;
        for (_, page) in &g.multicol_pages {
            for col in &page.columns {
                for piece in col {
                    let McPiece::Lines {
                        source, first, end, ..
                    } = piece
                    else {
                        panic!("only lines expected, got {piece:?}");
                    };
                    assert_eq!(*source, p_id);
                    assert_eq!(*first, next);
                    next = *end;
                }
            }
        }
        assert_eq!(next as usize, lines, "every line placed once");
        // One fragment of the paragraph and of the container on every page; all pages but the last are full.
        let pages: Vec<u32> = g.multicol_pages.iter().map(|(p, _)| *p).collect();
        assert_eq!(
            geom[&p_id]
                .fragments
                .iter()
                .map(|f| f.page_index)
                .collect::<Vec<_>>(),
            pages
        );
        assert_eq!(
            g.fragments.iter().map(|f| f.page_index).collect::<Vec<_>>(),
            pages
        );
        for (_, page) in &g.multicol_pages[..g.multicol_pages.len() - 1] {
            assert_eq!(page.height.to_f32(), 200.0);
        }
    }

    /// F161: a container with a `column-span: all` child that does not fit its page is cut by the multicol
    /// fragmenter: every box once, nothing above the page top, the spanner full width below the balanced row above it
    /// and the columns continuing under it, rows named by group.
    #[test]
    fn multicol_span_all_is_cut_over_pages() {
        // 8 + 8 boxes of 40 px in two columns on 200 px pages, a 30 px spanner between the groups.
        let html = format!(
            "<!doctype html><html><head><style>body{{margin:0}}\
             .mc{{column-count:2;column-gap:20px}}.b{{height:40px}}.s{{height:30px;column-span:all}}</style></head><body>\
             <div class=\"mc\">{a}<div class=\"s\"></div>{a}</div></body></html>",
            a = "<div class=\"b\"></div>".repeat(8)
        );
        let (doc, mc, geom) = multicol_fragments(&html, 200.0);
        let mc_id = node_by_class(&doc, "mc");
        let g = &geom[&mc_id];
        assert_eq!(mc[&mc_id].groups.len(), 2);
        // Group 0: 8 boxes balanced in two columns of 4 (160 px). Spanner at 160..190. Group 1 has 10 px left on the
        // page: its first box does not fit, the group starts on page 1: 4 + 4 balanced.
        let rows: Vec<(u32, u32, f32)> = g
            .multicol_pages
            .iter()
            .map(|(page, row)| (*page, row.group, row.y.to_f32()))
            .collect();
        assert_eq!(rows, vec![(0, 0, 0.0), (1, 1, 0.0)], "{rows:?}");
        assert_eq!(
            g.fragments.iter().map(|f| f.page_index).collect::<Vec<_>>(),
            vec![0, 1]
        );
        for (id, gg) in &geom {
            for f in &gg.fragments {
                assert!(f.y.to_f32() >= 0.0, "node {id}: fragment at y {:?}", f.y);
            }
        }
        // The spanner sits on page 0 below the first row, at the container's x, full width.
        let span = &geom[&node_by_class(&doc, "s")];
        assert_eq!(span.fragments.len(), 1);
        assert_eq!(span.fragments[0].page_index, 0);
        assert!((span.fragments[0].y.to_f32() - 160.0).abs() < 0.6);
        assert_eq!(span.fragments[0].x.to_f32(), 0.0);
        // Every box once: 16.
        let boxes = nodes_by_class(&doc, "b");
        let placed: usize = geom
            .iter()
            .filter(|(id, _)| boxes.contains(id))
            .map(|(_, gg)| gg.fragments.len())
            .sum();
        assert_eq!(placed, 16);
    }

    /// F161: a spanner taller than a page is cut like a block; the container has a fragment on every page it
    /// covers, and the group after it continues below its end.
    #[test]
    fn multicol_span_all_taller_than_a_quarter_page_keeps_the_old_path() {
        let html = format!(
            "<!doctype html><html><head><style>body{{margin:0}}\
             .mc{{column-count:2;column-gap:20px}}.b{{height:40px}}\
             .s{{column-span:all;display:flex;flex-direction:column}}.r{{height:90px}}</style></head><body>\
             <div class=\"mc\">{before}\
             <div class=\"s\"><div class=\"r\"></div><div class=\"r\"></div><div class=\"r\"></div>\
             <div class=\"r\"></div></div>\
             {after}</div></body></html>",
            before = "<div class=\"b\"></div>".repeat(14),
            after = "<div class=\"b\"></div>".repeat(6)
        );
        let (doc, _mc, geom) = multicol_fragments(&html, 200.0);
        let g = &geom[&node_by_class(&doc, "mc")];
        // html-to-pdf F161 review: the fragmenter's cut of a spanner taller than a page painted it over the
        // group above (F173), so such a container keeps the old path it took before F161.
        assert!(
            g.multicol_pages.is_empty(),
            "a container with a spanner taller than a quarter page took the multicol fragmenter"
        );
    }

    /// A container that fits the page keeps the path it had: one fragment, no pages recorded.
    #[test]
    fn multicol_that_fits_is_left_alone() {
        let html = format!(
            "<!doctype html><html><head><style>body{{margin:0}}\
             .mc{{column-count:2;column-gap:20px}}.b{{height:40px}}</style></head><body>\
             <div class=\"mc\">{}</div></body></html>",
            "<div class=\"b\"></div>".repeat(4)
        );
        let (doc, _, geom) = multicol_fragments(&html, 200.0);
        let g = &geom[&node_by_class(&doc, "mc")];
        assert_eq!(g.fragments.len(), 1);
        assert!(g.multicol_pages.is_empty());
    }

    // ── F174: a table in columns ────────────────────────────────────────────

    /// A container of two columns around a table of `rows` body rows of 20 px (and a one-row header when `thead`),
    /// every row one cell `r<N>`; `after` is a paragraph below the container.
    fn table_columns_html(rows: usize, thead: bool, extra: &str) -> String {
        let head = if thead {
            "<thead><tr><th class=\"h\">Head</th></tr></thead>"
        } else {
            ""
        };
        let body: String = (1..=rows)
            .map(|i| format!("<tr><td class=\"c\">r{i}</td></tr>"))
            .collect();
        format!(
            "<!doctype html><html><head><style>body{{margin:0;line-height:20px}}\
             .mc{{column-count:2;column-gap:20px}}table{{border-spacing:0;width:100%}}\
             td,th{{padding:0;text-align:left}}p{{margin:0}}</style></head><body>\
             <div class=\"mc\"><table class=\"t\">{head}<tbody>{body}</tbody></table></div>{extra}</body></html>"
        )
    }

    /// `(page, x, y)` of the fragments of the nodes with class `class`, in row order.
    fn placed(
        doc: &BaseDocument,
        geom: &PaginationGeometryTable,
        class: &str,
    ) -> Vec<(u32, f32, f32)> {
        let mut out = Vec::new();
        for id in nodes_by_class(doc, class) {
            for f in &geom[&id].fragments {
                out.push((f.page_index, f.x.to_f32(), f.y.to_f32()));
            }
        }
        out
    }

    /// F174: a table of 30 rows and a header in two columns on 200 px pages: cut between rows, every cell once,
    /// the cells of the second column at its x, the header repeated at the top of the three pieces that continue
    /// the table (one source fragment and three repeats), nothing below the page.
    #[test]
    fn multicol_table_is_cut_between_rows_with_the_header_repeated() {
        let (doc, mc, geom) = multicol_fragments(&table_columns_html(30, true, ""), 200.0);
        let mc_id = node_by_class(&doc, "mc");
        let table = node_by_class(&doc, "t");
        let col2 = mc[&mc_id].groups[0].col_w.to_f32() + 20.0;
        let cells = placed(&doc, &geom, "c");
        assert_eq!(cells.len(), 30, "every body cell once: {cells:?}");
        let mut by_row = cells.clone();
        by_row.sort_by(|a, b| a.partial_cmp(b).unwrap());
        // Column 1 of page 0: Head + r1-r9 (the header is the first 20 px), column 2: Head + r10-r18.
        assert_eq!(by_row[0], (0, 0.0, 20.0));
        assert_eq!(cells[8], (0, 0.0, 180.0), "r9 is the last row of column 1");
        assert_eq!(
            cells[9],
            (0, col2, 20.0),
            "r10 is below the header copy of column 2"
        );
        assert_eq!(
            cells[18],
            (1, 0.0, 20.0),
            "r19 starts page 1 below its header copy"
        );
        for (page, _, y) in &cells {
            assert!(
                y + 20.0 <= 200.5,
                "page {page}: a row at {y} runs past the page"
            );
        }
        // The table: one closed fragment per piece, x of its column, as tall as the piece.
        let pieces = &geom[&table].fragments;
        assert!(geom[&table].table_pieces);
        assert_eq!(pieces.len(), 4, "{pieces:?}");
        assert_eq!(
            pieces
                .iter()
                .map(|f| (f.page_index, f.x.to_f32(), f.y.to_f32()))
                .collect::<Vec<_>>(),
            vec![(0, 0.0, 0.0), (0, col2, 0.0), (1, 0.0, 0.0), (1, col2, 0.0)]
        );
        assert_eq!(pieces[0].height.to_f32(), 200.0);
        assert_eq!(pieces[1].height.to_f32(), 200.0);
        // The header: the source and one repeat per continuing piece; the header cell has one fragment.
        let thead = find_group(&doc, "thead");
        let group = &geom[&thead];
        assert!(group.is_repeat);
        assert_eq!(
            group
                .fragments
                .iter()
                .map(|f| (f.page_index, f.x.to_f32(), f.y.to_f32(), f.height.to_f32()))
                .collect::<Vec<_>>(),
            vec![
                (0, 0.0, 0.0, 20.0),
                (0, col2, 0.0, 20.0),
                (1, 0.0, 0.0, 20.0),
                (1, col2, 0.0, 20.0)
            ]
        );
        assert_eq!(placed(&doc, &geom, "h"), vec![(0, 0.0, 0.0)]);
    }

    fn find_group(doc: &BaseDocument, tag: &str) -> usize {
        fn walk(doc: &BaseDocument, id: usize, tag: &str) -> Option<usize> {
            let n = doc.get_node(id)?;
            if n.element_data()
                .is_some_and(|e| e.name.local.as_ref() == tag)
            {
                return Some(id);
            }
            n.children.iter().find_map(|&c| walk(doc, c, tag))
        }
        walk(doc, doc.root_element().id, tag).unwrap_or_else(|| panic!("no <{tag}>"))
    }

    /// F174: a table without a header is cut the same way and nothing is repeated.
    #[test]
    fn multicol_table_without_a_header_repeats_nothing() {
        let (doc, mc, geom) = multicol_fragments(&table_columns_html(50, false, ""), 200.0);
        let mc_id = node_by_class(&doc, "mc");
        let col2 = mc[&mc_id].groups[0].col_w.to_f32() + 20.0;
        let cells = placed(&doc, &geom, "c");
        assert_eq!(cells.len(), 50);
        assert_eq!(cells[0], (0, 0.0, 0.0));
        assert_eq!(cells[9], (0, 0.0, 180.0));
        assert_eq!(cells[10], (0, col2, 0.0));
        assert_eq!(cells[20], (1, 0.0, 0.0));
        assert!(
            !geom.contains_key(&find_group(&doc, "tbody"))
                || !geom[&find_group(&doc, "tbody")].is_repeat
        );
        assert!(geom.values().all(|g| !g.is_repeat));
    }

    /// F174: a table that fits a column is not split further than balancing does, and the content after the
    /// container sits below the balanced columns (they are shorter than the one-column table Blitz measured).
    #[test]
    fn multicol_table_that_fits_the_page_is_balanced_over_the_columns() {
        let html = table_columns_html(20, false, "<p class=\"after\">after</p>");
        let (doc, _mc, geom) = multicol_fragments(&html, 1000.0);
        let table = node_by_class(&doc, "t");
        // Taffy lays the table out whole in one column: 400 px. Balanced it is 10 + 10 rows.
        let mc_id = node_by_class(&doc, "mc");
        assert!(
            !geom[&mc_id].multicol_pages.is_empty(),
            "the planner placed it"
        );
        let cells = placed(&doc, &geom, "c");
        assert_eq!(cells.iter().filter(|c| c.1 == 0.0).count(), 10);
        assert_eq!(cells.iter().filter(|c| c.1 > 0.0).count(), 10);
        assert_eq!(geom[&table].fragments.len(), 2);
        assert!(geom[&mc_id].fragments[0].height.to_f32() <= 200.5);
    }

    /// F174: a table taller than a page with a `column-span: all` sibling is cut by the multicol fragmenter now
    /// (F161 kept such a container on the old path because the table is a box taller than a page).
    #[test]
    fn multicol_table_with_a_spanner_is_cut_between_rows() {
        let html = "<!doctype html><html><head><style>body{margin:0;line-height:20px}\
                    .mc{column-count:2;column-gap:20px}table{border-spacing:0}td{padding:0}\
                    .s{column-span:all;height:20px}</style></head><body><div class=\"mc\">\
                    <div class=\"s\">title</div><table class=\"t\"><tbody>"
            .to_string()
            + &(1..=60)
                .map(|i| format!("<tr><td class=\"c\">r{i}</td></tr>"))
                .collect::<String>()
            + "</tbody></table></div></body></html>";
        let (doc, _mc, geom) = multicol_fragments(&html, 200.0);
        let mc_id = node_by_class(&doc, "mc");
        assert!(!geom[&mc_id].multicol_pages.is_empty());
        let cells = placed(&doc, &geom, "c");
        assert_eq!(cells.len(), 60, "every row once");
        for (page, _, y) in &cells {
            assert!(
                y + 20.0 <= 200.5,
                "page {page}: a row at {y} runs past the page"
            );
        }
    }

    /// F174: a row taller than a page is placed alone in its column and warned about; the planner does not loop.
    #[test]
    fn multicol_table_with_a_row_taller_than_a_page_is_placed_alone() {
        let html = "<!doctype html><html><head><style>body{margin:0;line-height:20px}\
                    .mc{column-count:2;column-gap:20px}table{border-spacing:0}td{padding:0}\
                    </style></head><body><div class=\"mc\"><table class=\"t\"><tbody>\
                    <tr><td class=\"c\">a</td></tr><tr><td class=\"c\" style=\"height:300px\">tall</td></tr>\
                    <tr><td class=\"c\">b</td></tr></tbody></table></div></body></html>";
        let (doc, _mc, geom) = multicol_fragments(html, 200.0);
        let cells = placed(&doc, &geom, "c");
        assert_eq!(cells.len(), 3, "{cells:?}");
        assert_eq!(cells[0], (0, 0.0, 0.0));
        assert_eq!(cells[1].0, 0);
        assert!(
            cells[1].1 > 0.0,
            "the tall row opens the second column: {cells:?}"
        );
    }

    /// F144: the geometry is the same with the full used page-name table (all `(None, None)` when no node names a
    /// page, which is what the pass used to build), with `None` (what it builds now), with the walks and with the
    /// `SubtreeFacts`. Forced breaks and named pages are in the documents, so the gates fire.
    #[test]
    fn geometry_does_not_depend_on_the_used_page_name_table_or_the_facts() {
        let rows = |n: usize| "<tr><td>row</td><td>cell</td></tr>".repeat(n);
        let docs = [
            // No page names: the table is skipped.
            format!(
                "<html><body style=\"margin:0\"><div><table><tbody>{}</tbody></table>\
                 <div style=\"break-before:page\">after</div><p>tail</p></div>\
                 <section><div style=\"height:300px\"></div><div style=\"height:200px;break-after:page\"></div>\
                 <div style=\"height:100px\"></div></section></body></html>",
                rows(60)
            ),
            // No breaks of any kind.
            format!(
                "<html><body style=\"margin:0\"><table><tbody>{}</tbody></table></body></html>",
                rows(80)
            ),
            // Named pages: the table is built.
            format!(
                "<html><body style=\"margin:0\"><main><section style=\"page:a\"><table><tbody>{}</tbody></table></section>\
                 <section style=\"page:b\"><div style=\"height:120px\"></div></section>\
                 <section style=\"page:a;break-before:page\"><div style=\"height:120px\"></div></section></main></body></html>",
                rows(40)
            ),
        ];
        for (i, html) in docs.iter().enumerate() {
            let mut doc = parse(html, 600.0);
            let cs = blitz_adapter::extract_column_style_table(&doc);
            let named = crate::column_css::has_named_page(&cs);
            assert_eq!(named, i == 2, "document {i}");
            let current =
                run_pass_with_break_styles(doc.deref_mut(), 300.0_f32.as_pt().in_px(), &cs);
            let mut with = |facts: bool, full_table: bool| {
                let table = blitz_adapter::compute_used_page_names(&doc, &cs);
                let mut tree = PaginationLayoutTree::new(doc.deref_mut(), 400.0);
                tree.column_styles = Some(&cs);
                tree.used_page_names = full_table.then_some(table);
                if facts {
                    tree.subtree_facts = Some(SubtreeFacts::build(
                        tree.doc,
                        Some(&cs),
                        tree.used_page_names.as_ref(),
                    ));
                }
                tree.fragment_pagination_root();
                tree.take_geometry()
            };
            let want = format!("{current:?}");
            assert!(current.len() > 5, "document {i} has geometry");
            // Without a used page-name table the named document would lose its implicit breaks: that is not the
            // same geometry, and the pass never builds it.
            for (facts, full) in [(false, true), (true, true), (false, false)] {
                if named && !full {
                    continue;
                }
                assert_eq!(
                    format!("{:?}", with(facts, full)),
                    want,
                    "document {i}: facts {facts}, full used page-name table {full}"
                );
            }
        }
    }
}
