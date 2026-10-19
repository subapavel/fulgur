//! F22: painting the collapsed borders of a table (CSS 2.1 §17.6.2, CSS Tables 3).
//!
//! Blitz resolves, for every segment of every grid line of a table in the collapsing border model, the one border that
//! wins (see [`crate::drawables::CollapsedEdges`]) and lays the cells out with half of it on each side. The cells and
//! the table do not paint their own borders (convert sets their border styles to `none`); this module paints the
//! lines instead, centred on the grid lines, above the cells' backgrounds and content (CSS 2.1 §17.5.1, Appendix E).
//!
//! Where the lines are comes from the cells' fragments on the page: a horizontal line is the shared edge of two rows
//! (the bottom of the upper row, the top of the lower one), the vertical lines are at the same x on every page
//! (`CollapsedEdges::track_x`, anchored to a cell of the page). A horizontal line of width `w` is a band of `w` px
//! centred on the edge, snapped down to the px grid, which is where Chrome puts it.
//!
//! Page breaks (Chrome 141, probes `brk_rows` / `brk_head` of `docs/plans/2026-10-19-f22-row-borders.md` §7): a line
//! that falls on a break is cut in two. The half that lies in the upper row ends the page, the half that lies in the
//! lower row starts the next one (`ceil(w / 2)` px each). The repeated header (F12) is painted with its own lines, and
//! the line under it is the better of the header's bottom line and the line the page break cut (rule 3-5 of
//! §17.6.2.1: Chrome shows the row's rule under the header copy when it is as wide as the header's own).
//!
//! Where two lines cross, a run of a horizontal line is extended over half of the widest vertical segment at each of
//! its ends and a vertical run over half of the widest horizontal one, and the vertical lines are painted after the
//! horizontal ones, so the crossing square has the colour of the vertical line (Chrome, probe `junction`).

use std::collections::BTreeMap;

use crate::draw_primitives::{BorderStyleValue, Canvas, alpha_to_opacity, draw_border_line};
use crate::drawables::{CollapsedEdges, Drawables, EdgeSegment};
use crate::pagination_layout::PaginationGeometryTable;
use crate::units::F32Units;

/// A row's extent on the page being painted, in CSS px (target coordinates).
#[derive(Debug, Clone, Copy)]
struct RowSpan {
    top: f32,
    bottom: f32,
    /// No cell of the row was on an earlier page: its top edge is on this page.
    started: bool,
    /// A cell of the row goes on on a later page: its bottom edge is not on this page.
    continues: bool,
}

/// One horizontal grid line to paint: where, which halves of it are on this page, and its segments.
#[derive(Debug, Clone)]
struct HLine {
    y: f32,
    upper: bool,
    lower: bool,
    /// The rows whose vertical segments end at this line, above and below it.
    upper_row: Option<u32>,
    lower_row: Option<u32>,
    segs: Vec<EdgeSegment>,
}

/// What is painted: a page's own lines, or the copy of the table's header rows on a continuation page.
#[derive(Debug, Clone, Copy)]
enum Mode {
    /// `skip_line`: the line at the top of the page that the header copy paints (the copy's bottom line).
    Page { skip_line: Option<u32> },
    /// The header rows `0..header_rows`, standing on `first_row`'s page whose first row is `first_row`.
    Copy { first_row: Option<u32> },
}

/// Which tables paint where: tables in a wrapper (transform, overflow clip, opacity group) are painted by the wrapper,
/// inside its scope.
pub(crate) struct Scope<'a> {
    pub skip: &'a dyn Fn(usize) -> bool,
}

/// Paints the collapsed borders of every table that has a cell on `page_index`, except those `scope.skip` names (they
/// are painted inside their wrapper by [`paint_tables`]). `page_node_ids` is the page's node list, which finds the
/// cells.
#[allow(clippy::too_many_arguments)]
pub(crate) fn paint_page(
    canvas: &mut Canvas<'_, '_>,
    page_index: u32,
    margin_left_pt: f32,
    margin_top_pt: f32,
    geometry: &PaginationGeometryTable,
    drawables: &Drawables,
    page_node_ids: &[usize],
    scope: &Scope<'_>,
) {
    if drawables.collapsed_cell_of.is_empty() {
        return;
    }
    let mut per_table: BTreeMap<usize, Vec<u32>> = BTreeMap::new();
    for node in page_node_ids {
        if let Some(&(table, index)) = drawables.collapsed_cell_of.get(node) {
            per_table.entry(table).or_default().push(index);
        }
    }
    for (table, cells) in per_table {
        if (scope.skip)(table) {
            continue;
        }
        paint_table(
            canvas,
            table,
            &cells,
            page_index,
            margin_left_pt,
            margin_top_pt,
            geometry,
            drawables,
        );
    }
}

/// Paints the collapsed borders of the given `tables` that have a cell on `page_index`; for the tables a wrapper
/// paints inside its scope.
#[allow(clippy::too_many_arguments)]
pub(crate) fn paint_tables(
    canvas: &mut Canvas<'_, '_>,
    tables: impl Iterator<Item = usize>,
    page_index: u32,
    margin_left_pt: f32,
    margin_top_pt: f32,
    geometry: &PaginationGeometryTable,
    drawables: &Drawables,
) {
    for table in tables {
        let Some(edges) = drawables.table_collapsed_borders.get(&table) else {
            continue;
        };
        let cells: Vec<u32> = edges
            .cells
            .iter()
            .enumerate()
            .filter(|(_, c)| on_page(geometry, c.node, page_index))
            .map(|(i, _)| i as u32)
            .collect();
        if cells.is_empty() {
            continue;
        }
        paint_table(
            canvas,
            table,
            &cells,
            page_index,
            margin_left_pt,
            margin_top_pt,
            geometry,
            drawables,
        );
    }
}

fn on_page(geometry: &PaginationGeometryTable, node: usize, page_index: u32) -> bool {
    geometry
        .get(&node)
        .is_some_and(|g| g.fragments.iter().any(|f| f.page_index == page_index))
}

/// The page-by-page painting of one table: the lines of the rows that `cells` (indices into the table's cells) have
/// on `page_index`.
#[allow(clippy::too_many_arguments)]
fn paint_table(
    canvas: &mut Canvas<'_, '_>,
    table: usize,
    cells: &[u32],
    page_index: u32,
    margin_left_pt: f32,
    margin_top_pt: f32,
    geometry: &PaginationGeometryTable,
    drawables: &Drawables,
) {
    let (Some(edges), Some(entry)) = (
        drawables.table_collapsed_borders.get(&table),
        drawables.tables.get(&table),
    ) else {
        return;
    };
    if !entry.visible {
        return;
    }
    let Some((rows, x_off)) = page_rows(edges, cells, geometry, page_index, 0.0, 0.0) else {
        return;
    };
    // The header group is painted again on this page (F12): the copy paints the line under it, which is the first
    // line of this page.
    let first_row = rows.keys().next().copied();
    let copy_here = edges.header_group.is_some_and(|group| {
        geometry.get(&group).is_some_and(|g| {
            g.is_repeat
                && g.fragments
                    .iter()
                    .skip(1)
                    .any(|f| f.page_index == page_index)
        })
    });
    let skip_line = if copy_here { first_row } else { None };
    crate::draw_primitives::draw_with_opacity(canvas, entry.opacity, |canvas| {
        paint_lines(
            canvas,
            edges,
            &rows,
            x_off,
            margin_left_pt,
            margin_top_pt,
            Mode::Page { skip_line },
        );
    });
}

/// F12: paints the lines of the header rows of `table` on a continuation page. The header's cells are those of
/// `source_page`, shifted by `(dx_px, dy_px)` to where the copy stands on `page_index`; the copy is painted into
/// `canvas`, which is the artifact the cells of the copy are in.
#[allow(clippy::too_many_arguments)]
pub(crate) fn paint_header_copy(
    canvas: &mut Canvas<'_, '_>,
    table: usize,
    source_page: u32,
    page_index: u32,
    margin_left_pt: f32,
    margin_top_pt: f32,
    dx_px: f32,
    dy_px: f32,
    geometry: &PaginationGeometryTable,
    drawables: &Drawables,
) {
    let (Some(edges), Some(entry)) = (
        drawables.table_collapsed_borders.get(&table),
        drawables.tables.get(&table),
    ) else {
        return;
    };
    if !entry.visible || edges.header_cells == 0 {
        return;
    }
    let header: Vec<u32> = (0..edges.header_cells as u32).collect();
    let Some((rows, x_off)) = page_rows(edges, &header, geometry, source_page, dx_px, dy_px) else {
        return;
    };
    // The first row of the page the copy is on: the first cell after the header with a fragment there.
    let rest = &edges.cells[edges.header_cells..];
    let first_row = rest
        .iter()
        .find(|c| on_page(geometry, c.node, page_index))
        .map(|c| c.row);
    crate::draw_primitives::draw_with_opacity(canvas, entry.opacity, |canvas| {
        paint_lines(
            canvas,
            edges,
            &rows,
            x_off,
            margin_left_pt,
            margin_top_pt,
            Mode::Copy { first_row },
        );
    });
}

/// The extent of every row that has a cell among `cells` on `page_index`, shifted by `(dx_px, dy_px)`, and the x of
/// the table's left edge on the page (the offset the table's `track_x` is added to). `None` when no cell has a
/// fragment on the page.
fn page_rows(
    edges: &CollapsedEdges,
    cells: &[u32],
    geometry: &PaginationGeometryTable,
    page_index: u32,
    dx_px: f32,
    dy_px: f32,
) -> Option<(BTreeMap<u32, RowSpan>, f32)> {
    let mut rows: BTreeMap<u32, RowSpan> = BTreeMap::new();
    let mut x_off = None;
    for &index in cells {
        let cell = edges.cells[index as usize];
        let Some(geom) = geometry.get(&cell.node) else {
            continue;
        };
        let earlier = geom.fragments.iter().any(|f| f.page_index < page_index);
        let later = geom.fragments.iter().any(|f| f.page_index > page_index);
        for frag in geom.fragments.iter().filter(|f| f.page_index == page_index) {
            let frag = crate::render::frame_fragment(geom, frag);
            let (x, y) = (frag.x.to_f32() + dx_px, frag.y.to_f32() + dy_px);
            let (w, h) = (frag.width.to_f32(), frag.height.to_f32());
            if w <= 0.0 || h <= 0.0 {
                continue;
            }
            if x_off.is_none() {
                x_off = Some(x - edges.track_x[cell.track as usize].to_f32());
            }
            let row = rows.entry(cell.row).or_insert(RowSpan {
                top: y,
                bottom: y + h,
                started: true,
                continues: false,
            });
            row.top = row.top.min(y);
            row.bottom = row.bottom.max(y + h);
            row.started &= !earlier;
            row.continues |= later;
        }
    }
    x_off.map(|x_off| (rows, x_off))
}

/// Style ranking of rule 4 of CSS 2.1 §17.6.2.1: `double`, `solid`, `dashed`, `dotted`, `ridge`, `outset`, `groove`,
/// `inset`.
fn style_rank(style: BorderStyleValue) -> u8 {
    match style {
        BorderStyleValue::Double => 8,
        BorderStyleValue::Solid => 7,
        BorderStyleValue::Dashed => 6,
        BorderStyleValue::Dotted => 5,
        BorderStyleValue::Ridge => 4,
        BorderStyleValue::Outset => 3,
        BorderStyleValue::Groove => 2,
        BorderStyleValue::Inset => 1,
        BorderStyleValue::None => 0,
    }
}

/// The segment that wins between two for the same line (rules 3 to 5 of §17.6.2.1: width, style, then the kind of
/// element); `a` wins a tie.
fn better(a: EdgeSegment, b: EdgeSegment) -> EdgeSegment {
    let key = |s: &EdgeSegment| {
        let painted = s.is_painted();
        (
            painted,
            if painted { s.width.to_f32() } else { 0.0 },
            style_rank(s.style),
            s.origin,
        )
    };
    if key(&b) > key(&a) { b } else { a }
}

/// `x` of the start of a run ending at a crossing of width `w` (`0`: nothing crosses): the left edge of the crossing's
/// band.
fn band_start(x: f32, w: f32) -> f32 {
    if w > 0.0 { (x - w / 2.0).floor() } else { x }
}

/// The end of the same band.
fn band_end(x: f32, w: f32) -> f32 {
    if w > 0.0 {
        (x - w / 2.0).floor() + w
    } else {
        x
    }
}

fn painted_width(seg: &EdgeSegment) -> f32 {
    if seg.is_painted() {
        seg.width.to_f32()
    } else {
        0.0
    }
}

#[allow(clippy::too_many_arguments)]
fn paint_lines(
    canvas: &mut Canvas<'_, '_>,
    edges: &CollapsedEdges,
    rows: &BTreeMap<u32, RowSpan>,
    x_off: f32,
    margin_left_pt: f32,
    margin_top_pt: f32,
    mode: Mode,
) {
    let tracks = edges.tracks as usize;
    let line_rows = match mode {
        Mode::Page { .. } => edges.rows,
        Mode::Copy { .. } => edges.header_rows,
    };
    let x_of = |line: usize| x_off + edges.track_x[line].to_f32();
    let pt_x = |x: f32| margin_left_pt + x.as_px().in_pt().to_f32();
    let pt_y = |y: f32| margin_top_pt + y.as_px().in_pt().to_f32();

    // The horizontal lines that are on this page.
    let mut hlines: BTreeMap<u32, HLine> = BTreeMap::new();
    for i in 0..=line_rows {
        let lower_row = (i < line_rows)
            .then(|| rows.get(&i).filter(|r| r.started))
            .flatten();
        let upper_row = (i > 0)
            .then(|| rows.get(&(i - 1)).filter(|r| !r.continues))
            .flatten();
        // The line under a header copy has the rest of the page below it.
        let copy_line = matches!(mode, Mode::Copy { .. }) && i == line_rows;
        // At the top of the table the upper half is the table's own border, at its bottom the lower half is.
        let upper = if i == 0 {
            lower_row.is_some()
        } else {
            upper_row.is_some()
        };
        let lower = if copy_line {
            true
        } else if i == line_rows {
            upper_row.is_some()
        } else {
            lower_row.is_some()
        };
        if !upper && !lower {
            continue;
        }
        if let Mode::Page {
            skip_line: Some(skip),
        } = mode
            && skip == i
        {
            continue;
        }
        let y = match (copy_line, lower_row, upper_row) {
            (false, Some(r), _) => r.top,
            (_, _, Some(r)) => r.bottom,
            _ => continue,
        };
        let mut segs: Vec<EdgeSegment> = edges.horizontal[i as usize * tracks..][..tracks].to_vec();
        let mut lower_index = (i < edges.rows).then_some(i);
        if copy_line {
            // The line under the copy is the better of the header's bottom line and the line the break cut.
            lower_index = match mode {
                Mode::Copy { first_row: Some(r) } => {
                    for (t, seg) in segs.iter_mut().enumerate() {
                        *seg = better(*seg, edges.horizontal[r as usize * tracks + t]);
                    }
                    Some(r)
                }
                _ => None,
            };
        }
        hlines.insert(
            i,
            HLine {
                y,
                upper,
                lower,
                upper_row: i.checked_sub(1),
                lower_row: lower_index,
                segs,
            },
        );
    }

    canvas.surface.set_fill(None);

    // Horizontal runs.
    for hl in hlines.values() {
        let width_at = |row: Option<u32>, line: usize| {
            row.map_or(0.0, |r| {
                painted_width(&edges.vertical[r as usize * (tracks + 1) + line])
            })
        };
        // The widest vertical segment that meets the line at vertical line `line`, on the sides that are painted.
        let cross = |line: usize| {
            let above = if hl.upper {
                width_at(hl.upper_row, line)
            } else {
                0.0
            };
            let below = if hl.lower {
                width_at(hl.lower_row, line)
            } else {
                0.0
            };
            above.max(below)
        };
        let mut t = 0;
        while t < tracks {
            let seg = hl.segs[t];
            if !seg.is_painted() {
                t += 1;
                continue;
            }
            let mut end = t + 1;
            while end < tracks && hl.segs[end].same_paint(&seg) {
                end += 1;
            }
            let w = seg.width.to_f32();
            let top = (hl.y - w / 2.0).floor();
            let half = (w / 2.0).ceil();
            let (lo, hi) = match (hl.upper, hl.lower) {
                (true, true) => (top, top + w),
                (false, true) => (hl.y, hl.y + half),
                _ => (hl.y - half, hl.y),
            };
            let x0 = band_start(x_of(t), cross(t));
            let x1 = band_end(x_of(end), cross(end));
            stroke(
                canvas,
                (pt_x(x0), pt_y((lo + hi) / 2.0)),
                (pt_x(x1), pt_y((lo + hi) / 2.0)),
                (hi - lo).as_px().in_pt().to_f32(),
                &seg,
                -1.0,
            );
            t = end;
        }
    }

    // Vertical runs, painted after the horizontal ones.
    let row_ids: Vec<u32> = rows.keys().copied().collect();
    for line in 0..=tracks {
        let mut k = 0;
        while k < row_ids.len() {
            let r = row_ids[k];
            let seg = edges.vertical[r as usize * (tracks + 1) + line];
            if !seg.is_painted() || r >= line_rows {
                k += 1;
                continue;
            }
            let mut last = k;
            while last + 1 < row_ids.len() {
                let (a, b) = (row_ids[last], row_ids[last + 1]);
                let next = edges.vertical[b as usize * (tracks + 1) + line];
                if b == a + 1
                    && b < line_rows
                    && !rows[&a].continues
                    && rows[&b].started
                    && next.same_paint(&seg)
                {
                    last += 1;
                } else {
                    break;
                }
            }
            let (first, end) = (row_ids[k], row_ids[last]);
            let crossing = |hl: &HLine| {
                let left = (line > 0).then(|| painted_width(&hl.segs[line - 1]));
                let right = (line < tracks).then(|| painted_width(&hl.segs[line]));
                left.unwrap_or(0.0).max(right.unwrap_or(0.0))
            };
            let y0 = match hlines.get(&first) {
                Some(hl) if hl.upper && rows[&first].started => band_start(hl.y, crossing(hl)),
                _ => rows[&first].top,
            };
            let y1 = match hlines.get(&(end + 1)) {
                Some(hl) if hl.lower && !rows[&end].continues => band_end(hl.y, crossing(hl)),
                _ => rows[&end].bottom,
            };
            let w = seg.width.to_f32();
            let left = (x_of(line) - w / 2.0).floor();
            let mid = left + w / 2.0;
            stroke(
                canvas,
                (pt_x(mid), pt_y(y0)),
                (pt_x(mid), pt_y(y1)),
                w.as_px().in_pt().to_f32(),
                &seg,
                1.0,
            );
            k = last + 1;
        }
    }
    canvas.surface.set_stroke(None);
}

fn stroke(
    canvas: &mut Canvas<'_, '_>,
    from: (f32, f32),
    to: (f32, f32),
    width_pt: f32,
    seg: &EdgeSegment,
    outward_sign: f32,
) {
    draw_border_line(
        canvas,
        from.0,
        from.1,
        to.0,
        to.1,
        width_pt,
        seg.style,
        &seg.color,
        alpha_to_opacity(seg.color[3]),
        true,
        outward_sign,
    );
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::units::F32Units;

    fn seg(width: f32, style: BorderStyleValue, color: [u8; 4], origin: u8) -> EdgeSegment {
        EdgeSegment {
            width: width.as_px(),
            style,
            color,
            origin,
        }
    }

    const BLACK: [u8; 4] = [0, 0, 0, 255];
    const RED: [u8; 4] = [255, 0, 0, 255];

    #[test]
    fn the_wider_segment_is_better_whatever_the_style() {
        let wide = seg(4.0, BorderStyleValue::Dotted, RED, 0);
        let thin = seg(2.0, BorderStyleValue::Double, BLACK, 3);
        assert_eq!(better(wide, thin), wide);
        assert_eq!(better(thin, wide), wide);
    }

    #[test]
    fn at_equal_width_the_style_then_the_origin_decide() {
        let solid = seg(2.0, BorderStyleValue::Solid, RED, 0);
        let dotted = seg(2.0, BorderStyleValue::Dotted, BLACK, 3);
        assert_eq!(better(dotted, solid), solid);
        let row = seg(2.0, BorderStyleValue::Solid, RED, 2);
        let group = seg(2.0, BorderStyleValue::Solid, BLACK, 1);
        assert_eq!(better(group, row), row);
        assert_eq!(better(row, group), row);
        // A full tie keeps the first.
        let other = seg(2.0, BorderStyleValue::Solid, BLACK, 2);
        assert_eq!(better(row, other), row);
    }

    #[test]
    fn a_painted_segment_beats_an_unpainted_one() {
        let none = seg(0.0, BorderStyleValue::None, BLACK, 3);
        let thin = seg(1.0, BorderStyleValue::Solid, RED, 0);
        assert_eq!(better(none, thin), thin);
        assert_eq!(better(thin, none), thin);
        // Transparent is not painted either.
        let clear = seg(5.0, BorderStyleValue::Solid, [0, 0, 0, 0], 3);
        assert_eq!(better(clear, thin), thin);
    }

    #[test]
    fn a_crossing_extends_a_run_over_half_of_its_band() {
        // A 2 px vertical line centred on x = 10 covers 9..11.
        assert_eq!(band_start(10.0, 2.0), 9.0);
        assert_eq!(band_end(10.0, 2.0), 11.0);
        // A 1 px one centred on 10 is snapped down to 9.5 -> 9, covering 9..10.
        assert_eq!(band_start(10.0, 1.0), 9.0);
        assert_eq!(band_end(10.0, 1.0), 10.0);
        // Nothing crosses: the run ends on the line.
        assert_eq!(band_start(10.0, 0.0), 10.0);
        assert_eq!(band_end(10.0, 0.0), 10.0);
    }

    #[test]
    fn same_paint_ignores_the_origin() {
        let a = seg(1.0, BorderStyleValue::Solid, BLACK, 1);
        let b = seg(1.0, BorderStyleValue::Solid, BLACK, 3);
        assert!(a.same_paint(&b));
        assert!(!a.same_paint(&seg(1.0, BorderStyleValue::Dashed, BLACK, 1)));
        assert!(!a.same_paint(&seg(1.5, BorderStyleValue::Solid, BLACK, 1)));
        assert!(!a.same_paint(&seg(1.0, BorderStyleValue::Solid, RED, 1)));
    }
}
