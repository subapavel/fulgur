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
