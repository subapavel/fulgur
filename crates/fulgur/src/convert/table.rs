use super::*;

/// Dispatcher entry for `<table>` elements. Returns `true` when an entry
/// was inserted into `out.tables` (and any cell descendants registered).
pub(super) fn try_convert(
    doc: &BaseDocument,
    node_id: usize,
    ctx: &mut super::ConvertContext<'_>,
    depth: usize,
    out: &mut crate::drawables::Drawables,
) -> bool {
    let Some(node) = doc.get_node(node_id) else {
        return false;
    };
    let Some(elem_data) = node.element_data() else {
        return false;
    };
    if elem_data.name.local.as_ref() != "table" {
        return false;
    }
    convert_table(doc, node, ctx, depth, out);
    true
}

fn convert_table(
    doc: &BaseDocument,
    node: &Node,
    ctx: &mut ConvertContext<'_>,
    depth: usize,
    out: &mut crate::drawables::Drawables,
) {
    let (width, height) = size_in_pt(node.final_layout.size);
    let style = extract_block_style(node, ctx.assets);
    let clipping = style.has_overflow_clip();
    let (opacity, visible) = extract_opacity_visible(node);

    out.tables.insert(
        node.id,
        crate::drawables::TableEntry {
            style,
            opacity,
            visible,
            id: extract_block_id(node),
            layout_size: Some(Size { width, height }),
            width,
            cached_height: height,
            clip_descendants: Vec::new(),
        },
    );

    let mark = clipping.then(|| out.draw_mark());

    // F12: the header group whose cells may be painted again on
    // continuation pages (the fragmenter decides whether it is).
    let header_group = crate::blitz_adapter::first_table_header_group(doc, node.id);

    // Walk table children to recurse cells.
    for &child_id in &node.children {
        let Some(child_node) = doc.get_node(child_id) else {
            continue;
        };
        let is_thead = is_table_section(child_node, "thead");
        let header_mark = (Some(child_id) == header_group).then(|| out.draw_mark());
        collect_table_cells(doc, child_id, is_thead, ctx, depth, out);
        if let Some(mark) = header_mark {
            let ids = out.drawn_since(mark);
            if !ids.is_empty() {
                out.table_header_groups.insert(child_id, ids);
            }
        }
    }

    // F19: row and row-group backgrounds, painted from the table.
    let backgrounds = collect_row_backgrounds(doc, node, ctx.assets);
    if !backgrounds.is_empty() {
        if let Some(group) = header_group
            && backgrounds
                .iter()
                .any(|b| b.source == group || b.group == Some(group))
        {
            out.table_header_background_owners.insert(group, node.id);
        }
        out.table_row_backgrounds.insert(node.id, backgrounds);
    }

    if let Some(mark) = mark {
        let descendants: Vec<usize> = out
            .drawn_since(mark)
            .into_iter()
            .filter(|&id| id != node.id)
            .collect();
        if let Some(entry) = out.tables.get_mut(&node.id) {
            entry.clip_descendants = descendants;
        }
    }
}

/// A cell's border box in its parent table's layout space (CSS px).
#[derive(Clone, Copy)]
struct CellBox {
    id: usize,
    x: f32,
    y: f32,
    width: f32,
    height: f32,
}

/// F19: the backgrounds of the table's row groups and rows, classified by
/// computed `display` (a `<thead style="display: block">` is not a row group).
///
/// Blitz lays a table out as a grid of cells; row groups and rows get no box
/// (`final_layout` stays zero), so a background on them is never painted by the
/// generic block path. Each entry records the cells it covers and the area
/// (union of their border boxes) that images are positioned against.
///
/// Returns row groups first, then rows, both in DOM order: the paint order of
/// CSS 2.1 §17.5.1. Elements without a background colour or image, or with
/// `visibility: hidden | collapse`, are left out.
fn collect_row_backgrounds(
    doc: &BaseDocument,
    table: &Node,
    assets: Option<&AssetBundle>,
) -> Vec<crate::drawables::RowBackground> {
    use crate::blitz_adapter::{TablePart, is_table_box, table_part};

    if !is_table_box(table) {
        return Vec::new();
    }
    let mut groups = Vec::new();
    let mut rows = Vec::new();
    for &child_id in &table.children {
        let Some(child) = doc.get_node(child_id) else {
            continue;
        };
        match table_part(child) {
            TablePart::RowGroup => {
                let mut group_cells = Vec::new();
                for &row_id in &child.children {
                    let Some(row) = doc.get_node(row_id) else {
                        continue;
                    };
                    if table_part(row) != TablePart::Row {
                        continue;
                    }
                    let cells = row_cells(doc, row);
                    rows.extend(row_background(row, Some(child_id), &cells, assets));
                    group_cells.extend(cells);
                }
                groups.extend(row_background(child, None, &group_cells, assets));
            }
            TablePart::Row => {
                let cells = row_cells(doc, child);
                rows.extend(row_background(child, None, &cells, assets));
            }
            TablePart::Cell | TablePart::Other => {}
        }
    }
    groups.extend(rows);
    groups
}

/// The `display: table-cell` children of `row` that have a laid-out box.
fn row_cells(doc: &BaseDocument, row: &Node) -> Vec<CellBox> {
    use crate::blitz_adapter::{TablePart, table_part};

    row.children
        .iter()
        .filter_map(|&id| {
            let cell = doc.get_node(id)?;
            let layout = &cell.final_layout;
            (table_part(cell) == TablePart::Cell
                && layout.size.width > 0.0
                && layout.size.height > 0.0)
                .then_some(CellBox {
                    id,
                    x: layout.location.x,
                    y: layout.location.y,
                    width: layout.size.width,
                    height: layout.size.height,
                })
        })
        .collect()
}

/// The [`RowBackground`](crate::drawables::RowBackground) of one row or row
/// group, or `None` when it covers no cell, is not visible or paints nothing.
fn row_background(
    node: &Node,
    group: Option<usize>,
    cells: &[CellBox],
    assets: Option<&AssetBundle>,
) -> Option<crate::drawables::RowBackground> {
    use crate::drawables::{RowBackground, RowBackgroundCell};

    let first = cells.first()?;
    if !extract_opacity_visible(node).1 {
        return None;
    }
    let mut style = extract_block_style(node, assets);
    if style.background_color.is_none() && style.background_layers.is_empty() {
        return None;
    }
    // Rows and row groups paint a background only: their borders (F22) and
    // shadows are not painted, padding and radii do not apply to them.
    style.border_widths = [Pt::ZERO; 4];
    style.padding = [Pt::ZERO; 4];
    style.border_radii = [[Pt::ZERO; 2]; 4];
    style.box_shadows.clear();

    let (mut left, mut top) = (first.x, first.y);
    let (mut right, mut bottom) = (first.x + first.width, first.y + first.height);
    for c in cells {
        left = left.min(c.x);
        top = top.min(c.y);
        right = right.max(c.x + c.width);
        bottom = bottom.max(c.y + c.height);
    }
    Some(RowBackground {
        source: node.id,
        group,
        style,
        area: Size {
            width: (right - left).as_px().in_pt(),
            height: (bottom - top).as_px().in_pt(),
        },
        cells: cells
            .iter()
            .map(|c| RowBackgroundCell {
                cell: c.id,
                dx: (c.x - left).as_px().in_pt(),
                dy: (c.y - top).as_px().in_pt(),
            })
            .collect(),
    })
}

fn is_table_section(node: &Node, section_name: &str) -> bool {
    if let Some(elem) = node.element_data() {
        elem.name.local.as_ref() == section_name
    } else {
        false
    }
}

fn collect_table_cells(
    doc: &BaseDocument,
    node_id: usize,
    is_header: bool,
    ctx: &mut ConvertContext<'_>,
    depth: usize,
    out: &mut crate::drawables::Drawables,
) {
    if depth >= MAX_DOM_DEPTH {
        return;
    }
    let Some(node) = doc.get_node(node_id) else {
        return;
    };

    let layout_children_guard = node.layout_children.borrow();
    let effective_children = layout_children_guard.as_deref().unwrap_or(&node.children);
    for &child_id in effective_children {
        let Some(child_node) = doc.get_node(child_id) else {
            continue;
        };
        if matches!(&child_node.data, NodeData::Comment) {
            continue;
        }
        if is_non_visual_element(child_node) {
            continue;
        }

        let cw = child_node.final_layout.size.width.as_px().in_pt();
        let ch = child_node.final_layout.size.height.as_px().in_pt();

        let child_effective_is_empty = child_node
            .layout_children
            .borrow()
            .as_deref()
            .unwrap_or(&child_node.children)
            .is_empty();
        if ch == Pt::ZERO && cw == Pt::ZERO && !child_effective_is_empty {
            let child_is_header = is_header || is_table_section(child_node, "thead");
            collect_table_cells(doc, child_id, child_is_header, ctx, depth + 1, out);
            continue;
        }

        if ch == Pt::ZERO && cw == Pt::ZERO {
            continue;
        }

        // Actual cell — recurse via convert_node so its block / paragraph
        // entries land in the standard maps.
        convert_node(doc, child_id, ctx, depth + 1, out);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::gcpm::running::RunningElementStore;
    use blitz_html::HtmlDocument;

    fn parse_doc(html: &str) -> HtmlDocument {
        crate::blitz_adapter::parse_and_layout(
            html,
            595.0_f32.as_px(),
            842.0_f32.as_px(),
            &[],
            false,
        )
    }

    fn find_tag(doc: &BaseDocument, id: usize, tag: &str) -> Option<usize> {
        let n = doc.get_node(id)?;
        if n.element_data()
            .is_some_and(|e| e.name.local.as_ref() == tag)
        {
            return Some(id);
        }
        n.children.iter().find_map(|&c| find_tag(doc, c, tag))
    }

    /// The row and row-group backgrounds of the document's first `<table>`.
    fn backgrounds(html: &str) -> (HtmlDocument, Vec<crate::drawables::RowBackground>) {
        let doc = parse_doc(html);
        let base = doc.deref();
        let table_id = find_tag(base, base.root_element().id, "table").expect("a <table>");
        let rows = collect_row_backgrounds(base, base.get_node(table_id).unwrap(), None);
        (doc, rows)
    }

    fn tag_of(doc: &HtmlDocument, id: usize) -> String {
        doc.deref()
            .get_node(id)
            .and_then(|n| n.element_data())
            .map(|e| e.name.local.as_ref().to_string())
            .unwrap_or_default()
    }

    const TABLE_CSS: &str =
        "<style>table{border-spacing:0}td{width:100px;height:40px;padding:0}</style>";

    #[test]
    fn groups_come_before_rows_and_both_in_dom_order() {
        let (doc, rows) = backgrounds(&format!(
            r#"<!doctype html><html><head>{TABLE_CSS}</head><body><table>
              <thead style="background:#111"><tr style="background:#222"><td></td></tr></thead>
              <tbody style="background:#333">
                <tr><td></td></tr>
                <tr style="background:#444"><td></td></tr>
              </tbody>
              <tfoot style="background:#555"><tr style="background:#666"><td></td></tr></tfoot>
            </table></body></html>"#
        ));
        let order: Vec<_> = rows.iter().map(|r| tag_of(&doc, r.source)).collect();
        // Row groups in DOM order, then rows in DOM order; the plain first body row has none.
        assert_eq!(order, ["thead", "tbody", "tfoot", "tr", "tr", "tr"]);
        let colours: Vec<_> = rows
            .iter()
            .map(|r| r.style.background_color.unwrap()[0])
            .collect();
        assert_eq!(colours, [0x11, 0x33, 0x55, 0x22, 0x44, 0x66]);
        // A row records its group, a group does not.
        assert!(rows[..3].iter().all(|r| r.group.is_none()));
        assert_eq!(tag_of(&doc, rows[3].group.unwrap()), "thead");
        assert_eq!(tag_of(&doc, rows[4].group.unwrap()), "tbody");
        assert_eq!(tag_of(&doc, rows[5].group.unwrap()), "tfoot");
    }

    #[test]
    fn cells_carry_their_offset_inside_the_area() {
        let (doc, rows) = backgrounds(&format!(
            r#"<!doctype html><html><head>{TABLE_CSS}</head><body><table>
              <tr style="background:red"><td></td><td></td></tr>
              <tr style="background:blue"><td></td><td></td></tr>
            </table></body></html>"#
        ));
        assert_eq!(rows.len(), 2);
        // Two 100x40 px cells side by side: the area is 200x40 px = 150x30 pt, the second cell
        // starts 100 px = 75 pt in. Rows are 40 px apart in y, but each row's area starts at its own top.
        for row in &rows {
            assert_eq!(row.cells.len(), 2);
            assert!(
                (row.area.width.to_f32() - 150.0).abs() < 0.01,
                "{:?}",
                row.area
            );
            assert!(
                (row.area.height.to_f32() - 30.0).abs() < 0.01,
                "{:?}",
                row.area
            );
            assert_eq!(row.cells[0].dx.to_f32(), 0.0);
            assert_eq!(row.cells[0].dy.to_f32(), 0.0);
            assert!((row.cells[1].dx.to_f32() - 75.0).abs() < 0.01);
            assert_eq!(row.cells[1].dy.to_f32(), 0.0);
            assert_eq!(tag_of(&doc, row.cells[0].cell), "td");
        }
        assert_ne!(rows[0].cells[0].cell, rows[1].cells[0].cell);
    }

    #[test]
    fn a_group_area_spans_all_its_rows() {
        let (_, rows) = backgrounds(&format!(
            r#"<!doctype html><html><head>{TABLE_CSS}</head><body><table>
              <tbody style="background:green">
                <tr><td></td><td></td></tr>
                <tr><td></td><td></td></tr>
              </tbody>
            </table></body></html>"#
        ));
        assert_eq!(rows.len(), 1);
        let group = &rows[0];
        assert_eq!(group.cells.len(), 4);
        // 200x80 px = 150x60 pt; the second row's cells are 40 px = 30 pt down.
        assert!(
            (group.area.height.to_f32() - 60.0).abs() < 0.01,
            "{:?}",
            group.area
        );
        assert!((group.cells[2].dy.to_f32() - 30.0).abs() < 0.01);
        assert_eq!(group.cells[2].dx.to_f32(), 0.0);
    }

    #[test]
    fn a_colspan_cell_is_one_cell_with_its_own_width() {
        let (_, rows) = backgrounds(&format!(
            r#"<!doctype html><html><head>{TABLE_CSS}</head><body><table>
              <tr><td></td><td></td><td></td></tr>
              <tr style="background:red"><td colspan="2" style="width:200px"></td><td></td></tr>
            </table></body></html>"#
        ));
        assert_eq!(rows.len(), 1);
        let row = &rows[0];
        assert_eq!(row.cells.len(), 2, "a spanning cell is listed once");
        // The spanning cell covers more than one 100 px (75 pt) column, so the next cell starts
        // beyond it, and the area ends at the end of that cell (75 pt wide).
        assert!(row.cells[1].dx.to_f32() > 75.0, "{:?}", row.cells);
        let end = row.cells[1].dx.to_f32() + 75.0;
        assert!(
            (row.area.width.to_f32() - end).abs() < 0.5,
            "{:?}",
            row.area
        );
    }

    #[test]
    fn rows_that_paint_nothing_are_left_out() {
        let (_, rows) = backgrounds(&format!(
            r#"<!doctype html><html><head>{TABLE_CSS}</head><body><table>
              <tr><td></td></tr>
              <tr style="background:transparent"><td></td></tr>
              <tr style="background:red;visibility:hidden"><td></td></tr>
              <tr style="background:red;visibility:collapse"><td></td></tr>
              <tr style="background:red;display:none"><td></td></tr>
              <tr style="background:red"></tr>
              <tr style="background:blue"><td></td></tr>
            </table></body></html>"#
        ));
        assert_eq!(
            rows.len(),
            1,
            "only the visible blue row with a cell remains"
        );
        assert_eq!(rows[0].style.background_color, Some([0, 0, 255, 255]));
    }

    #[test]
    fn groups_are_classified_by_display_not_tag() {
        let (doc, rows) = backgrounds(&format!(
            r#"<!doctype html><html><head>{TABLE_CSS}</head><body><table>
              <thead style="display:block;background:red"><tr><td></td></tr></thead>
              <tbody style="display:table-footer-group;background:green"><tr><td></td></tr></tbody>
            </table></body></html>"#
        ));
        let tags: Vec<_> = rows.iter().map(|r| tag_of(&doc, r.source)).collect();
        assert_eq!(tags, ["tbody"], "display:block thead is no row group");
    }

    #[test]
    fn only_the_background_survives() {
        let (_, rows) = backgrounds(&format!(
            r#"<!doctype html><html><head>{TABLE_CSS}</head><body><table>
              <tr style="background:red;border:4px solid black;border-radius:8px;padding:6px;
                         box-shadow:2px 2px 3px black"><td></td></tr>
            </table></body></html>"#
        ));
        let style = &rows[0].style;
        assert!(style.border_widths.iter().all(|w| w.to_f32() == 0.0));
        assert!(style.padding.iter().all(|p| p.to_f32() == 0.0));
        assert!(!style.has_radius());
        assert!(style.box_shadows.is_empty());
    }

    #[test]
    fn images_and_gradients_are_kept_as_layers() {
        let (_, rows) = backgrounds(&format!(
            r#"<!doctype html><html><head>{TABLE_CSS}</head><body><table>
              <tr style="background:linear-gradient(red,blue)"><td></td><td></td></tr>
            </table></body></html>"#
        ));
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].style.background_layers.len(), 1);
        assert!(rows[0].style.background_color.is_none());
    }

    #[test]
    fn not_a_table_box_has_no_backgrounds() {
        let (_, rows) = backgrounds(&format!(
            r#"<!doctype html><html><head>{TABLE_CSS}</head><body><table style="display:block">
              <tr style="background:red"><td></td></tr>
            </table></body></html>"#
        ));
        assert!(rows.is_empty());
    }

    #[test]
    fn convert_records_backgrounds_and_the_header_owner() {
        let mut doc = parse_doc(&format!(
            r#"<!doctype html><html><head>{TABLE_CSS}</head><body><table>
              <thead><tr style="background:red"><td></td></tr></thead>
              <tbody><tr><td></td></tr></tbody>
            </table></body></html>"#
        ));
        let column_styles = crate::blitz_adapter::extract_column_style_table(&doc);
        let multicol_geometry = crate::multicol_layout::run_pass(&mut doc, &column_styles);
        let pagination_geometry = crate::pagination_layout::run_pass(&mut doc, 842.0);
        let store = RunningElementStore::new();
        let mut ctx = ConvertContext {
            running_store: &store,
            assets: None,
            font_cache: Default::default(),
            string_set_by_node: Default::default(),
            counter_ops_by_node: Default::default(),
            bookmark_by_node: Default::default(),
            column_styles,
            multicol_geometry,
            pagination_geometry,
            link_cache: Default::default(),
            viewport_size_px: Some((595.0, 842.0)),
        };
        let drawables = dom_to_drawables(&doc, &mut ctx);
        let table_id = find_tag(doc.deref(), doc.root_element().id, "table").unwrap();
        let thead_id = find_tag(doc.deref(), doc.root_element().id, "thead").unwrap();
        let rows = &drawables.table_row_backgrounds[&table_id];
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].group, Some(thead_id));
        assert_eq!(
            drawables.table_header_background_owners.get(&thead_id),
            Some(&table_id)
        );
    }
}
