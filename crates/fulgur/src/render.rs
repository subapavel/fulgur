use crate::config::Config;
use crate::draw_primitives::Canvas;
use crate::drawables::Drawables;
use crate::error::{Error, Result};
use crate::gcpm::GcpmContext;
use crate::gcpm::counter::resolve_content_to_html_with_anchor;
use crate::gcpm::margin_box::{Edge, MarginBoxPosition, MarginBoxRect, compute_edge_layout};
use crate::gcpm::running::RunningElementStore;
use crate::gcpm::target_ref::AnchorMap;
use crate::units::F32Units;
use krilla::SerializeSettings;
use krilla::configure::{Configuration, Validator};
use krilla::tagging::{Identifier, Node, TagGroup, TagTree};
use std::collections::{BTreeMap, HashMap};
use std::sync::Arc;

/// Geometry-driven page render.
///
/// Walks `geometry` per page and dispatches each (node_id, fragment)
/// to per-type draw functions sourced from `drawables`. Page settings
/// (size, margins, landscape, GCPM `@page` overrides) resolve once per
/// page.
///
/// Returns the PDF and the glyph runs that were left out because the PDF writer could not read their face
/// (`KrillaFonts::skip_run`; the caller reports them).
#[allow(clippy::too_many_arguments)]
pub fn render_v2(
    config: &Config,
    geometry: &crate::pagination_layout::PaginationGeometryTable,
    drawables: &Drawables,
    gcpm: &GcpmContext,
    running_store: &RunningElementStore,
    font_data: &[Arc<Vec<u8>>],
    system_fonts: bool,
    generics: &BTreeMap<String, String>,
    string_set_by_node: &HashMap<usize, Vec<(String, String)>>,
    counter_ops_by_node: &BTreeMap<usize, Vec<crate::gcpm::CounterOp>>,
    html_title: Option<String>,
    serialize_settings: SerializeSettings,
    anchor_map: Option<&AnchorMap>,
    implicit_href_by_page: &BTreeMap<usize, String>,
) -> Result<(Vec<u8>, Vec<crate::convert::DroppedText>)> {
    let mut document = if config.effective_tagging() {
        let configuration = if config.pdf_ua {
            Configuration::new_with_validator(Validator::UA1)
        } else {
            Configuration::new()
        };
        krilla::Document::new_with(SerializeSettings {
            enable_tagging: true,
            configuration,
            ..serialize_settings
        })
    } else {
        krilla::Document::new_with(serialize_settings)
    };

    let mut bookmark_collector = if config.effective_bookmarks() {
        Some(crate::draw_primitives::BookmarkCollector::new())
    } else {
        None
    };

    let mut tag_collector = if config.effective_tagging() {
        Some(crate::draw_primitives::TagCollector::new())
    } else {
        None
    };

    let tagging = tag_collector.is_some();
    let page_count = crate::pagination_layout::implied_page_count(geometry).max(1) as usize;

    // Pre-pass: register `id` anchors for `href="#..."` resolution.
    // A node may appear in both `paragraphs` and `block_styles`
    // (shared node_id case — see `convert::replaced` /
    // `convert::inline_root`); paragraph wins so the shared-id case
    // resolves to the inline-root's anchor.
    let mut dest_registry = crate::draw_primitives::DestinationRegistry::new();
    for (&node_id, geom) in geometry {
        let Some(first_frag) = geom.fragments.first() else {
            continue;
        };
        let para_id = drawables
            .paragraphs
            .get(&node_id)
            .and_then(|p| p.id.as_ref());
        let block_id = drawables
            .block_styles
            .get(&node_id)
            .and_then(|b| b.id.as_ref());
        let table_id = drawables.tables.get(&node_id).and_then(|t| t.id.as_ref());
        let id = para_id.or(block_id).or(table_id);
        if let Some(id) = id
            && !id.is_empty()
        {
            let page_idx = first_frag.page_index as usize;
            dest_registry.set_current_page(page_idx);
            // The fragment is in body content-area-relative CSS px;
            // resolve the page-specific margin so destination y_pt is
            // page-absolute (matches v1's `collect_ids` semantics).
            let page_num = page_idx + 1;
            let (_resolved_size, resolved_margin, _resolved_landscape) =
                crate::gcpm::page_settings::resolve_page_settings(
                    &gcpm.page_settings,
                    page_num,
                    page_count,
                    config,
                    drawables.root_dir_rtl,
                );
            // `frag.x` is page-area-relative (already includes body's x
            // offset from the fragmenter; geometry for boxes placed in the
            // initial containing block is stored in the same frame, see
            // `pagination_layout::icb_to_fragment`); only y needs
            // `body_offset_pt` applied, and only on page 0 (continuation pages
            // are already page-content-area-relative after the fragmenter
            // resets cursor_y).
            let body_y_off = if page_idx == 0 {
                drawables.body_offset_pt.1
            } else {
                crate::units::Pt::ZERO
            };
            let x_pt = resolved_margin.left.as_pt() + first_frag.x.in_pt();
            let y_pt = resolved_margin.top.as_pt() + body_y_off + first_frag.y.in_pt();
            dest_registry.record(id.as_str(), x_pt, y_pt);
        }
    }

    // Body content and margin boxes get separate LinkCollectors. Body
    // occurrences are clipped to the content area before emission so a
    // body link positioned into a page-margin strip (via CSS positioning
    // or overflow) cannot leave a click target under a later-painted
    // margin box — the paint order would otherwise let the visible text
    // and the clickable URI disagree. Margin-box occurrences are emitted
    // unclipped so `@page @bottom-*` anchors remain clickable.
    let mut body_link_collector = crate::draw_primitives::LinkCollector::new();
    let mut margin_link_collector = crate::draw_primitives::LinkCollector::new();

    let mut link_annot_ids: BTreeMap<usize, Vec<krilla::tagging::Identifier>> = BTreeMap::new();

    // Build the GCPM margin-box renderer once. Reused across pages so
    // measure / layout / render caches survive between pages and the
    // pre-computed `string_set_states` / `counter_states` /
    // `running_states` are paid for once.
    let mut margin_box_renderer = MarginBoxRenderer::new(
        gcpm,
        running_store,
        font_data,
        system_fonts,
        generics,
        geometry,
        string_set_by_node,
        counter_ops_by_node,
        page_count,
        implicit_href_by_page,
    );

    // Page-independent skip sets. These reference only `drawables.*`
    // (transforms / block_styles / tables) and never change across
    // pages, so building them inside `draw_v2_page` once per page made
    // the per-page work O(N_blocks + N_tables + N_transforms). See
    // fulgur-v1cm.
    let (transformed_descendants, clipped_descendants, opacity_wrapped_descendants) =
        build_page_skip_sets(drawables);

    // Per-page dispatch buckets: which `node_id`s have at least one
    // fragment on this page index. The previous shape — walking the
    // entire `geometry` BTreeMap once per page — was the dominant
    // O(P²) cost in document-grade documents (N tables × P
    // page-sections), since `geometry` grows with N which itself
    // scales with P. Bucketing collapses that to O(N) build + O(F)
    // dispatch where F is the total fragment count. (fulgur-v1cm)
    //
    // Iteration order inside each bucket follows `BTreeMap<NodeId, _>`
    // which is approximately document order, preserving v1 stacking
    // (parents before children, backgrounds before foregrounds). This
    // is the same invariant the original loop relied on. Each node is
    // inserted at most once per page even when it has multiple
    // fragments on that page — `dispatch_fragment` and the per-fragment
    // arms below already iterate `geom.fragments` themselves and skip
    // mismatched `frag.page_index`, so a single dispatch entry per
    // (node, page) is sufficient.
    let mut per_page_node_ids: Vec<Vec<usize>> = vec![Vec::new(); page_count];
    for (&node_id, geom) in geometry {
        let mut seen_pages: std::collections::BTreeSet<u32> = std::collections::BTreeSet::new();
        for frag in &geom.fragments {
            let p = frag.page_index;
            if (p as usize) < page_count && seen_pages.insert(p) {
                per_page_node_ids[p as usize].push(node_id);
            }
        }
    }

    // Raster images are validated once per render, not once per draw (F32a).
    let images = crate::image::ImageCache::default();
    // One Krilla font per face of the render, not one per glyph run (F99e).
    let mut krilla_fonts = crate::draw_primitives::KrillaFonts::default();

    // The content box the fragmenter paginated every page against is page 1's (F109).
    let page_one_box = {
        let (size, margin, landscape) = crate::gcpm::page_settings::resolve_page_settings(
            &gcpm.page_settings,
            1,
            page_count,
            config,
            drawables.root_dir_rtl,
        );
        let size = if landscape { size.landscape() } else { size };
        content_box_pt(size, margin)
    };
    let mut warned_empty_box = false;
    let mut warned_other_box = false;

    for (page_idx, page_node_ids) in per_page_node_ids.iter().enumerate() {
        let page_num = page_idx + 1;
        // Pass the full `gcpm.page_settings` (including selector
        // rules: `:first`, `:left`, `:right`) so per-page overrides
        // fire identically to the v1 GCPM path.
        let (resolved_size, resolved_margin, resolved_landscape) =
            crate::gcpm::page_settings::resolve_page_settings(
                &gcpm.page_settings,
                page_num,
                page_count,
                config,
                drawables.root_dir_rtl,
            );
        let page_size = if resolved_landscape {
            resolved_size.landscape()
        } else {
            resolved_size
        };
        let this_box = content_box_pt(page_size, resolved_margin);
        if !warned_empty_box && (this_box.0 <= 0.0 || this_box.1 <= 0.0) {
            warned_empty_box = true;
            log::warn!(
                "the content box of page {page_num} is {:.1} x {:.1} pt (page {:.1} x {:.1} pt, margins {:.1} / {:.1} / {:.1} / {:.1} pt): the margins leave no room, so nothing of the body is drawn",
                this_box.0,
                this_box.1,
                page_size.width,
                page_size.height,
                resolved_margin.top,
                resolved_margin.right,
                resolved_margin.bottom,
                resolved_margin.left
            );
        }
        if page_idx > 0
            && !warned_other_box
            && page_one_box.0 > 0.0
            && page_one_box.1 > 0.0
            && ((this_box.0 - page_one_box.0).abs() > 0.5
                || (this_box.1 - page_one_box.1).abs() > 0.5)
        {
            warned_other_box = true;
            let effect = if this_box.0 < page_one_box.0 - 0.5 || this_box.1 < page_one_box.1 - 0.5 {
                "smaller, so content may be clipped or lost, or drawn into the margin"
            } else {
                "larger, so the page is filled only as far as page 1's box"
            };
            log::warn!(
                "pages are paginated against page 1's content box ({:.1} x {:.1} pt); the box of page {page_num} is {:.1} x {:.1} pt, {effect} (F109)",
                page_one_box.0,
                page_one_box.1,
                this_box.0,
                this_box.1
            );
        }
        let settings = krilla::page::PageSettings::from_wh(page_size.width, page_size.height)
            .ok_or_else(|| Error::PdfGeneration("Invalid page dimensions".into()))?;
        let content_area = resolved_content_area(page_size, resolved_margin);
        let mut page = document.start_page_with(settings);
        if let Some(c) = bookmark_collector.as_mut() {
            c.set_current_page(page_idx);
        }
        body_link_collector.set_current_page(page_idx);
        margin_link_collector.set_current_page(page_idx);
        let page_has_margin_box_paint;
        {
            let mut surface = page.surface();
            {
                let mut canvas = crate::draw_primitives::Canvas {
                    surface: &mut surface,
                    bookmark_collector: bookmark_collector.as_mut(),
                    link_collector: Some(&mut body_link_collector),
                    tag_collector: tag_collector.as_mut(),
                    link_run_node_id: None,
                    in_marked_content: false,
                    images: images.clone(),
                    fonts: &mut krilla_fonts,
                    paint_extent: Default::default(),
                    clip_band: Default::default(),
                };
                // Root `<html>` + `<body>` background pre-pass. Root
                // `<html>` / `<body>` backgrounds repeat per page
                // (CSS Paged Media §5.4), but the fragmenter only
                // records a single fragment for each on page 0, so a
                // pure per-fragment dispatch would lose their fills on
                // continuation pages. Paint each here at its own offset
                // (`(margin.left, margin.top)` for html, plus
                // `body_offset_pt` for body) using `layout_size` from
                // the entry. The main dispatch loop skips both
                // `root_id` and `body_id` to avoid double-painting on
                // page 0.
                //
                // F184: the background of `<html>` (or of `<body>`, when `<html>` has none) is the background of the
                // canvas, painted first and under everything: over the page area of every page, positioned against
                // the root element's box as it sits on this page, and not again on the box itself.
                let canvas_source = canvas_background_source(drawables);
                if let Some(source_id) = canvas_source
                    && let Some(source) = drawables.block_styles.get(&source_id)
                    && let Some(root_id) = drawables.root_id
                    && let Some(root_block) = drawables.block_styles.get(&root_id)
                    && let Some(root_size) = root_block.layout_size
                    && source.visible
                {
                    // The root is one box cut into strips of page 1's content height (F109): page k shows it from
                    // k strips down.
                    let strip_pt = page_one_box.1.max(0.0);
                    let positioning = crate::draw_primitives::Rect {
                        x: content_area.x,
                        y: content_area.y - (page_idx as f32 * strip_pt).as_pt(),
                        width: root_size.width,
                        height: root_size.height,
                    };
                    // Not under the root's `opacity`: the canvas is not part of the root's stacking context, and Chrome
                    // paints it at full colour under `html { opacity: .5 }` (F184 vrt-validator).
                    crate::background::draw_canvas_background(
                        &mut canvas,
                        &source.style,
                        &root_block.style,
                        positioning,
                        content_area,
                    );
                }
                if let Some(root_id) = drawables.root_id
                    && let Some(root_block) = drawables.block_styles.get(&root_id)
                {
                    paint_root_block_v2(
                        &mut canvas,
                        root_block,
                        resolved_margin.left,
                        resolved_margin.top,
                        None,
                        canvas_source != Some(root_id),
                    );
                }
                // `frag.x` is page-area-relative (fragmenter folds body's x
                // offset in); `frag.y` is page-area-relative less body's y
                // offset on page 0 only. On page 0 body_offset_pt.1 translates
                // that back to the page area; on continuation pages the
                // fragmenter resets cursor_y=0 per page so fragments are already
                // page-content-area-relative and the offset must not apply.
                // Boxes placed in the initial containing block (absolute
                // children of body, fixed boxes) are stored in this frame by
                // `pagination_layout::icb_to_fragment` (html-to-pdf F187).
                let body_top_pt = if page_idx == 0 {
                    resolved_margin.top + drawables.body_offset_pt.1.to_f32()
                } else {
                    resolved_margin.top
                };
                // Body's own box (F37): background, border and shadow, painted as the slice of the box that is on this
                // page (`PaginationGeometry::box_slices`), not as a box of one page's height on every page. Page 0's
                // slice starts at `body_top_pt` (it has body's `margin-top` above it), the others at the page-area top.
                // `draw_v2_page` skips body's block paint, so this is its only painter; when body is the canvas source
                // (F184) its background is already painted and only the border and shadow are left.
                if let Some(body_id) = drawables.body_id
                    && let Some(body_block) = drawables.block_styles.get(&body_id)
                {
                    let slices = geometry
                        .get(&body_id)
                        .map_or(&[][..], |g| g.box_slices.as_slice());
                    paint_body_box_v2(
                        &mut canvas,
                        body_block,
                        slices,
                        page_idx as u32,
                        resolved_margin.left + drawables.body_offset_pt.0.to_f32(),
                        body_top_pt,
                        (page_size.width, page_size.height),
                        canvas_source != Some(body_id),
                    );
                }
                draw_v2_page(
                    &mut canvas,
                    page_idx as u32,
                    resolved_margin.left,
                    body_top_pt,
                    geometry,
                    drawables,
                    &transformed_descendants,
                    &clipped_descendants,
                    &opacity_wrapped_descendants,
                    page_node_ids,
                );
                check_continuations_painted(page_idx as u32, geometry, drawables);
                paint_repeated_table_headers(
                    &mut canvas,
                    page_idx as u32,
                    resolved_margin.left,
                    body_top_pt,
                    geometry,
                    drawables,
                    &transformed_descendants,
                    &clipped_descendants,
                    &opacity_wrapped_descendants,
                    tagging,
                    page_node_ids,
                );
                // Content past the right edge of the page is clipped by the page box (CSS Page 3 §3.2 lets it
                // overflow the margins; the page box is the medium). Say so: Chrome shrinks the page instead.
                let overflow = canvas.paint_extent.right() - page_size.width;
                if overflow > PAGE_OVERFLOW_TOLERANCE_PT {
                    log::warn!(
                        "content extends {overflow:.1} pt past the right page edge on page {page_num} and is clipped"
                    );
                }
                // Text below the bottom edge is not printed: every line is painted on exactly one page (F78), so a
                // line that sits below its page's edge is on no page. The fragmenter keeps some boxes whole (an
                // `inline-flex`, an oversized leaf); this says so where it cannot have been split.
                let below = canvas.paint_extent.bottom() - page_size.height;
                if below > PAGE_OVERFLOW_TOLERANCE_PT {
                    log::warn!(
                        "content extends {below:.1} pt past the bottom page edge on page {page_num} and is not printed"
                    );
                }
            }
            // Paint margin boxes after body content so page headers /
            // footers are not hidden by page-filling body backgrounds.
            // Keep bookmarks disabled for repeated running elements.
            // Margin-box links go into their own collector so their
            // occurrences do not get clipped to the body content area
            // (they live in the margin strips by design).
            let mut margin_canvas = crate::draw_primitives::Canvas {
                surface: &mut surface,
                bookmark_collector: None,
                link_collector: Some(&mut margin_link_collector),
                tag_collector: None,
                link_run_node_id: None,
                in_marked_content: false,
                images: images.clone(),
                fonts: &mut krilla_fonts,
                paint_extent: Default::default(),
                clip_band: Default::default(),
            };
            let page_content_width = content_area.width.to_f32();
            let any_margin_box_painted = margin_box_renderer.render_page(
                &mut margin_canvas,
                page_idx,
                page_num,
                page_count,
                page_size,
                resolved_margin,
                page_content_width,
                anchor_map,
            );
            page_has_margin_box_paint = any_margin_box_painted;
        }
        // Body occurrences are clipped to the content area rect ONLY
        // when at least one `@page` margin box actually painted on
        // this page — that's the only scenario where a later-painted
        // margin box could visually cover a body link and leave the
        // click target out of sync with the visible text. When no
        // margin box painted, nothing overlays the body layer, and
        // clipping would drop visible body-drawn page furniture
        // (`position: fixed; bottom: 0` page numbers, `margin-top`
        // overflow into the strip, etc.) making it unclickable
        // without any security benefit. `content_area` is the same
        // rect the body background clamp and the margin-box renderer
        // used earlier this iteration — see `resolved_content_area`.
        let raw_body_page = body_link_collector.take_page(page_idx);
        let body_per_page = if page_has_margin_box_paint {
            crate::link::clip_body_occurrences_to_content_area(raw_body_page, &content_area)
        } else {
            raw_body_page
        };
        let margin_per_page = margin_link_collector.take_page(page_idx);
        // Only span_ptrs that are wired into the struct tree via
        // ParagraphRunItem::LinkContent entries should use add_tagged_annotation;
        // others fall back to add_annotation so that Krilla's invariant
        // (every tagged annotation appears in the tag tree) is not violated for
        // link types not yet wired (e.g. InlineBox links).
        let wired_ptrs = tag_collector.as_ref().map(|tc| tc.wired_link_span_ptrs());
        for (ptr, id) in crate::link::emit_link_annotations(
            &mut page,
            &body_per_page,
            &dest_registry,
            wired_ptrs.as_ref(),
        ) {
            link_annot_ids.entry(ptr).or_default().push(id);
        }
        for (ptr, id) in crate::link::emit_link_annotations(
            &mut page,
            &margin_per_page,
            &dest_registry,
            wired_ptrs.as_ref(),
        ) {
            link_annot_ids.entry(ptr).or_default().push(id);
        }
    }

    if let Some(tc) = tag_collector {
        let mut tree = TagTree::new().with_lang(config.lang.clone());
        build_struct_tree(tc, drawables, &link_annot_ids, &mut tree);
        document.set_tag_tree(tree);
    }

    if let Some(c) = bookmark_collector {
        let entries = c.into_entries();
        if !entries.is_empty() {
            document.set_outline(crate::outline::build_outline(&entries));
        }
    }

    document.set_metadata(build_metadata(config, html_title.as_deref()));
    let pdf = document
        .finish()
        .map_err(|e| Error::PdfGeneration(format!("{e:?}")))?;
    Ok((pdf, krilla_fonts.into_skipped()))
}

/// Phase 4 v2 per-page draw dispatcher. Walks every `(node_id,
/// fragment)` pair whose fragment is on `page_index` and routes each
/// to a per-type draw function sourced from `drawables`.
///
/// Iteration is by `BTreeMap<NodeId, _>` order which is approximately
/// document order (Blitz allocates NodeIds during parse). That keeps
/// stacking order — backgrounds before foregrounds, parents before
/// children — consistent with the v1 traversal.
///
/// PR 2 covers `Drawables.images`, `.svgs`, and `.bookmark_anchors`
/// (first-fragment-only). Subsequent PRs add match arms for the
/// other maps.
/// Build the three page-independent skip sets used by `draw_v2_page`.
///
/// Content-area rect for a page: the region inside `resolved_margin`,
/// where body content is laid out. Single source of truth for the three
/// downstream consumers — body background clamp height, GCPM margin-box
/// renderer width, and body-link annotation clip rect — so a future
/// change to how `resolved_margin` subtracts from `page_size` shifts
/// all three together instead of leaving one behind.
/// Width and height in pt of the box a page's margins leave, which may be zero or negative.
fn content_box_pt(size: crate::config::PageSize, margin: crate::config::Margin) -> (f32, f32) {
    (
        size.width - margin.left - margin.right,
        size.height - margin.top - margin.bottom,
    )
}

fn resolved_content_area(
    page_size: crate::config::PageSize,
    resolved_margin: crate::config::Margin,
) -> crate::draw_primitives::Rect {
    crate::draw_primitives::Rect {
        x: resolved_margin.left.as_pt(),
        y: resolved_margin.top.as_pt(),
        width: (page_size.width - resolved_margin.left - resolved_margin.right).as_pt(),
        height: (page_size.height - resolved_margin.top - resolved_margin.bottom).as_pt(),
    }
}

/// - `transformed_descendants`: every node listed in some
///   `TransformEntry::descendants` — drawn inside that transform's
///   `push_transform / pop` group, so the main per-fragment loop must
///   skip them to avoid double-painting outside the transform.
/// - `clipped_descendants`: every node listed in the
///   `clip_descendants` of an `overflow:hidden|clip` block or table
///   (excluding body and root, see `render_v2` rationale).
/// - `opacity_wrapped_descendants`: every node listed in
///   `opacity_descendants` of a fractional-opacity block (excluding
///   body and root).
///
/// All three depend only on `drawables.*` and are page-independent.
/// `render_v2` builds them once and passes references into every
/// `draw_v2_page` call. (fulgur-v1cm)
fn build_page_skip_sets(
    drawables: &Drawables,
) -> (
    std::collections::BTreeSet<usize>,
    std::collections::BTreeSet<usize>,
    std::collections::BTreeSet<usize>,
) {
    let transformed_descendants: std::collections::BTreeSet<usize> = drawables
        .transforms
        .values()
        .flat_map(|tx| tx.descendants.iter().copied())
        .collect();

    let mut clipped_descendants: std::collections::BTreeSet<usize> =
        std::collections::BTreeSet::new();
    for (&node_id, block) in drawables.block_styles.iter() {
        // Exclude body and root: body's only fragment lives on
        // page 0 (so `draw_under_clip(body)` only fires there) and
        // root is never recorded in `geometry`. Including either
        // would silently blank descendants on pages 1+ via the
        // `clipped_descendants.contains(&node_id)` guard.
        if block.style.has_overflow_clip()
            && Some(node_id) != drawables.body_id
            && Some(node_id) != drawables.root_id
        {
            clipped_descendants.extend(block.clip_descendants.iter().copied());
        }
    }
    for table in drawables.tables.values() {
        if table.style.has_overflow_clip() && !table.clip_descendants.is_empty() {
            clipped_descendants.extend(table.clip_descendants.iter().copied());
        }
    }

    let mut opacity_wrapped_descendants: std::collections::BTreeSet<usize> =
        std::collections::BTreeSet::new();
    for (&node_id, block) in drawables.block_styles.iter() {
        if !block.opacity_descendants.is_empty()
            && Some(node_id) != drawables.body_id
            && Some(node_id) != drawables.root_id
        {
            opacity_wrapped_descendants.extend(block.opacity_descendants.iter().copied());
        }
    }

    (
        transformed_descendants,
        clipped_descendants,
        opacity_wrapped_descendants,
    )
}

#[allow(clippy::too_many_arguments)]
fn draw_v2_page(
    canvas: &mut crate::draw_primitives::Canvas<'_, '_>,
    page_index: u32,
    margin_left_pt: f32,
    margin_top_pt: f32,
    geometry: &crate::pagination_layout::PaginationGeometryTable,
    drawables: &Drawables,
    transformed_descendants: &std::collections::BTreeSet<usize>,
    clipped_descendants: &std::collections::BTreeSet<usize>,
    opacity_wrapped_descendants: &std::collections::BTreeSet<usize>,
    page_node_ids: &[usize],
) {
    // Skip sets and per-page dispatch list are built once in
    // `render_v2`. Walking only `page_node_ids` (instead of the full
    // `geometry` map per page) is what fixes the O(P²) regression
    // documented in fulgur-v1cm.

    for &node_id in page_node_ids {
        dispatch_page_node(
            canvas,
            node_id,
            page_index,
            margin_left_pt,
            margin_top_pt,
            geometry,
            drawables,
            transformed_descendants,
            clipped_descendants,
            opacity_wrapped_descendants,
        );
    }

    // Post-pass: paint multicol column rules between columns. Column
    // rules paint AFTER column content so rule lines sit on top of
    // the column contents (CSS Multi-column §4.5). Every per-NodeId
    // payload is already drawn by the main loop above.
    //
    // Skip multicol containers that live inside a transform (whether
    // they ARE a transform key or are a descendant of one): when a
    // multicol container carries a transform, the rules must paint
    // under the composed matrix (CSS Transforms §11 stacking-context
    // rules), so the transform version is dispatched from
    // `draw_under_transform`'s tail while the composed transform is
    // still active. Painting them here unconditionally would emit the
    // rules twice — once in page coords (wrong) and once inside the
    // transform (correct) — and visually misalign the
    // page-coord copy. (PR #305 follow-up Devin)
    for (&container_id, entry) in &drawables.multicol_rules {
        if transformed_descendants.contains(&container_id)
            || drawables.transforms.contains_key(&container_id)
        {
            continue;
        }
        let Some(container_geom) = geometry.get(&container_id) else {
            continue;
        };
        paint_multicol_rule_for_page(
            canvas,
            entry,
            container_geom,
            margin_left_pt,
            margin_top_pt,
            page_index,
        );
    }
    // F22: the collapsed borders of the tables on this page, above their cells. A table inside a transform, an overflow
    // clip or an opacity group is painted by that wrapper (`paint_wrapper_tables`), inside its scope.
    crate::table_borders::paint_page(
        canvas,
        page_index,
        margin_left_pt,
        margin_top_pt,
        geometry,
        drawables,
        page_node_ids,
        &crate::table_borders::Scope {
            skip: &|table| {
                transformed_descendants.contains(&table)
                    || clipped_descendants.contains(&table)
                    || opacity_wrapped_descendants.contains(&table)
                    // Inside an inline box: painted by `dispatch_inline_box_content`, under the box's translation.
                    || drawables.inline_box_subtree_skip.contains(&table)
                    || is_wrapper_key(drawables, table)
            },
        },
    );
}

/// F12: paint the table headers the fragmenter repeated on `page_index`.
///
/// For each `table-header-group` in `drawables.table_header_groups` whose
/// geometry holds a repeat fragment on this page (every fragment after the
/// first; see `pagination_layout::reserve_header_band`), dispatch the group's
/// drawables as if they sat on the page of the first fragment, shifted by the
/// difference between the two fragments' page-local positions. The margins
/// passed in are this page's, so per-page `@page` margins apply.
///
/// The header's row-group and row backgrounds (F19) are painted first, under
/// the cells, on the same canvas.
///
/// The copy is page furniture, not content: it is painted on a canvas without
/// tag, link and bookmark collectors (no second set of TH structure elements,
/// no duplicate link targets or outline entries) and, when the document is
/// tagged, inside an `/Artifact` marked-content sequence (ISO 32000-1
/// §14.8.2.2; PDF/UA-1 §7.1). `ArtifactType::Other` is used on purpose:
/// krilla maps `Header` to `/Pagination /Attached [/Top]`, which describes
/// running headers, not table headers.
///
/// Iteration is by `BTreeMap` order so the output is deterministic.
///
/// `page_node_ids` is the node list of `page_index` (the nodes with a fragment on it); the collapsed borders of the
/// copy find the first body row of the page in it instead of scanning the table (F99c).
#[allow(clippy::too_many_arguments)]
fn paint_repeated_table_headers(
    canvas: &mut crate::draw_primitives::Canvas<'_, '_>,
    page_index: u32,
    margin_left_pt: f32,
    margin_top_pt: f32,
    geometry: &crate::pagination_layout::PaginationGeometryTable,
    drawables: &Drawables,
    transformed_descendants: &std::collections::BTreeSet<usize>,
    clipped_descendants: &std::collections::BTreeSet<usize>,
    opacity_wrapped_descendants: &std::collections::BTreeSet<usize>,
    tagging: bool,
    page_node_ids: &[usize],
) {
    if drawables.table_header_groups.is_empty()
        && drawables.table_header_background_owners.is_empty()
        && drawables.table_collapsed_borders.is_empty()
    {
        return;
    }
    // A header group is repeated when it has drawables to paint again or a
    // row / row-group background (F19) behind its cells; a header whose cells
    // draw nothing but whose `<tr>` has a background is in the second map only.
    let groups: std::collections::BTreeSet<usize> = drawables
        .table_header_groups
        .keys()
        .chain(drawables.table_header_background_owners.keys())
        .copied()
        // F22: a header whose rows have collapsed borders is repeated for them as well.
        .chain(
            drawables
                .table_collapsed_borders
                .values()
                .filter_map(|edges| edges.header_group),
        )
        .collect();
    for group_id in &groups {
        let node_ids: &[usize] = drawables
            .table_header_groups
            .get(group_id)
            .map_or(&[], Vec::as_slice);
        let Some(group_geom) = geometry.get(group_id).filter(|g| g.is_repeat) else {
            continue;
        };
        let Some(source) = group_geom.fragments.first() else {
            continue;
        };
        for repeat in group_geom.fragments[1..]
            .iter()
            .filter(|f| f.page_index == page_index)
        {
            let left_pt = margin_left_pt + (repeat.x - source.x).in_pt().to_f32();
            let top_pt = margin_top_pt + (repeat.y - source.y).in_pt().to_f32();
            let mut copy_canvas = crate::draw_primitives::Canvas {
                surface: &mut *canvas.surface,
                bookmark_collector: None,
                link_collector: None,
                tag_collector: None,
                link_run_node_id: None,
                in_marked_content: false,
                images: canvas.images.clone(),
                fonts: &mut *canvas.fonts,
                paint_extent: Default::default(),
                clip_band: Default::default(),
            };
            if tagging {
                copy_canvas
                    .surface
                    .start_tagged(krilla::tagging::ContentTag::Artifact(
                        krilla::tagging::ArtifactType::Other,
                    ));
                // Nested `start_tagged` panics in krilla; this flag makes every
                // paragraph / marker entry point skip its own sequence.
                copy_canvas.in_marked_content = true;
            }
            // F19: the header's row-group and row backgrounds go first, below
            // the cells, in the same artifact.
            if let Some(&table_id) = drawables.table_header_background_owners.get(group_id) {
                paint_table_row_backgrounds(
                    &mut copy_canvas,
                    table_id,
                    Some(*group_id),
                    None,
                    geometry,
                    drawables,
                    left_pt,
                    top_pt,
                    source.page_index,
                );
            }
            for node_id in node_ids {
                if !geometry.contains_key(node_id) {
                    continue;
                }
                dispatch_page_node(
                    &mut copy_canvas,
                    *node_id,
                    source.page_index,
                    left_pt,
                    top_pt,
                    geometry,
                    drawables,
                    transformed_descendants,
                    clipped_descendants,
                    opacity_wrapped_descendants,
                );
            }
            // F22: the header rows' collapsed borders, above the cells of the copy, in the same artifact.
            if let Some((&table_id, _)) = drawables
                .table_collapsed_borders
                .iter()
                .find(|(_, edges)| edges.header_group == Some(*group_id))
            {
                crate::table_borders::paint_header_copy(
                    &mut copy_canvas,
                    table_id,
                    source.page_index,
                    page_index,
                    margin_left_pt,
                    margin_top_pt,
                    (repeat.x - source.x).to_f32(),
                    (repeat.y - source.y).to_f32(),
                    repeat.x.to_f32(),
                    geometry,
                    drawables,
                    page_node_ids,
                );
            }
            if tagging {
                copy_canvas.surface.end_tagged();
            }
        }
    }
}

/// Paint everything the geometry records for `node_id` on `page_index`:
/// the per-node body of [`draw_v2_page`]'s dispatch loop, also used to paint
/// a repeated table header (F12, [`paint_repeated_table_headers`]).
///
/// `node_id` must be a key of `geometry`. Applies the transform / clip /
/// opacity skip sets, so a node painted inside an ancestor wrapper is not
/// painted a second time here.
#[allow(clippy::too_many_arguments)]
fn dispatch_page_node(
    canvas: &mut crate::draw_primitives::Canvas<'_, '_>,
    node_id: usize,
    page_index: u32,
    margin_left_pt: f32,
    margin_top_pt: f32,
    geometry: &crate::pagination_layout::PaginationGeometryTable,
    drawables: &Drawables,
    transformed_descendants: &std::collections::BTreeSet<usize>,
    clipped_descendants: &std::collections::BTreeSet<usize>,
    opacity_wrapped_descendants: &std::collections::BTreeSet<usize>,
) {
    // SAFETY: `per_page_node_ids` is populated from `geometry`'s keys
    // in `render_v2`, so every `node_id` here is guaranteed to be in
    // `geometry`. Indexing keeps the access on a single line so line
    // coverage isn't dragged down by an else-arm that never fires.
    let geom = &geometry[&node_id];
    // Bookmark anchor: emit on the page where the node's *first*
    // fragment lands. Run BEFORE the `transformed_descendants`
    // skip so headings nested inside a transformed ancestor (e.g.
    // `<div style="transform:..."><h1>`) still register in the PDF
    // outline — record unconditionally using the untransformed y
    // position so the outline destination is stable. (`/Link`
    // annotation rects still receive the transform via the link
    // collector; only the bookmark destination is keyed by raw y.)
    if let Some(first_frag) = geom.fragments.first()
        && first_frag.page_index == page_index
        && let Some(anchor) = drawables.bookmark_anchors.get(&node_id)
        && let Some(c) = canvas.bookmark_collector.as_deref_mut()
    {
        let y_pt = margin_top_pt.as_pt() + first_frag.y.in_pt();
        c.record(anchor.level, anchor.label.clone(), y_pt);
    }

    if transformed_descendants.contains(&node_id) {
        // Drawn inside an ancestor transform group elsewhere in
        // this loop. Skipping prevents double-painting. Bookmark
        // anchor recording above already ran unconditionally.
        return;
    }
    if clipped_descendants.contains(&node_id) {
        // Drawn inside an ancestor `overflow: hidden|clip` block's
        // `push_clip_path / pop` group elsewhere in this loop.
        return;
    }
    if opacity_wrapped_descendants.contains(&node_id) {
        // Drawn inside an ancestor `draw_with_opacity` group via
        // `draw_under_opacity` elsewhere in this loop. Mirrors the
        // `clipped_descendants` skip — without it, the descendant
        // paints once at full opacity here and once under the
        // parent's opacity wrap. (fulgur-gdb9)
        return;
    }
    if drawables.inline_box_subtree_skip.contains(&node_id) {
        // PR 8g: belongs to inline-box content (or its descendants)
        // dispatched explicitly by `paragraph::draw_shaped_lines`
        // under an offset transform. Skipping here avoids
        // double-rendering at the body-relative geometry position
        // (which doesn't include the CSS 2.1 §10.8.1 baseline_shift
        // that fulgur applies at convert time, see
        // `convert/inline_root.rs:493`).
        //
        // F30f: except the pages of a box fragmented over several pages after the one its carrier painted it on.
        paint_inline_box_continuation(
            canvas,
            node_id,
            page_index,
            geometry,
            drawables,
            margin_left_pt,
            margin_top_pt,
        );
        return;
    }
    // Skip the html root: its bg / border / shadow are painted
    // per-page in the pre-pass (`paint_root_block_v2`) above.
    if Some(node_id) == drawables.root_id {
        return;
    }

    // Body block bg is painted by the per-page pre-pass (paint_root_block_v2)
    // with content_area_h clamping. Skip the block here to avoid a double-paint
    // of the background.
    //
    // F111: body is the inline root when all its in-flow children are inline-level
    // (CSS 2.1 §9.4.2). It then carries a paragraph of its own, next to the block
    // style that a background, border, padding or shadow gives it; that paragraph
    // is body's text and nothing else paints it. Draw it on every page body has a
    // fragment on (the fragmenter splits it between its line boxes), at the
    // content box of the block, under the block's opacity.
    if Some(node_id) == drawables.body_id
        && let Some(block) = drawables.block_styles.get(&node_id)
    {
        if let Some(para) = drawables.paragraphs.get(&node_id) {
            let (inset_x, inset_y) = block.style.content_inset();
            for frag in geom.fragments.iter().filter(|f| f.page_index == page_index) {
                let x_pt = margin_left_pt + frag.x.in_pt().to_f32() + inset_x;
                let y_pt = margin_top_pt + frag.y.in_pt().to_f32() + inset_y;
                crate::draw_primitives::draw_with_opacity(canvas, block.opacity, |canvas| {
                    draw_paragraph_dispatch(
                        canvas,
                        node_id,
                        para,
                        geom,
                        x_pt,
                        y_pt,
                        drawables,
                        geometry,
                        margin_left_pt,
                        margin_top_pt,
                        page_index,
                    );
                });
            }
        }
        return;
    }

    // Per-fragment leaf draws.
    for frag in &geom.fragments {
        if frag.page_index != page_index {
            continue;
        }
        let x_pt = margin_left_pt + frag.x.in_pt().to_f32();
        let y_pt = margin_top_pt + frag.y.in_pt().to_f32();

        if let Some(tx) = drawables.transforms.get(&node_id) {
            draw_under_transform(
                canvas,
                tx,
                node_id,
                geom,
                frag,
                x_pt,
                y_pt,
                geometry,
                drawables,
                margin_left_pt,
                margin_top_pt,
                page_index,
            );
            continue;
        }
        // `overflow: hidden | clip` block: paint bg / border /
        // shadow OUTSIDE the clip, then push the clip path,
        // dispatch self's inner content + every strict descendant
        // INSIDE the clip, then pop (CSS 2.1 §11.1.1 painting
        // order for overflow clipping). Same shape as
        // `draw_under_transform` but with `push_clip_path`.
        //
        // No `!clip_descendants.is_empty()` guard: shared-node_id
        // inner content (inline-root paragraph from
        // `convert::inline_root`, replaced image / svg from
        // `convert::replaced`) lands at the same `node_id` as the
        // wrapper and so produces an empty `clip_descendants`. The
        // clip must still be pushed unconditionally when
        // `has_overflow_clip()` is true so a
        // `<div style="overflow:hidden;width:50px">long text</div>`
        // still gets the text clipped at the 50px box even with
        // no separate descendant NodeIds.
        // Body is intentionally excluded from `draw_under_clip`:
        // body has only a page-0 fragment so the clip would only
        // wrap page-0 content, but body's `clip_descendants`
        // include every block in the document. Descendants on
        // page 1+ are dispatched by the main loop via
        // `dispatch_fragment` (they're omitted from
        // `clipped_descendants` above). Without this skip, body's
        // page-0 clip would also re-dispatch every descendant
        // already painted by the main loop, causing a double
        // paint. See the `clipped_descendants` collection block
        // for the rest of the body-overflow rationale.
        if let Some(block) = drawables.block_styles.get(&node_id)
            && block.style.has_overflow_clip()
            && Some(node_id) != drawables.body_id
        {
            draw_under_clip(
                canvas,
                block,
                node_id,
                geom,
                frag,
                x_pt,
                y_pt,
                geometry,
                drawables,
                margin_left_pt,
                margin_top_pt,
                page_index,
            );
            continue;
        }
        // Table with `overflow: hidden | clip`: same painting
        // order as the block clip arm above (CSS 2.1 §11.1.1).
        // Route through `draw_under_clip_table`, which paints the
        // outer frame outside the clip and dispatches each cell
        // descendant inside.
        if let Some(table) = drawables
            .tables
            .get(&node_id)
            .filter(|t| t.style.has_overflow_clip())
        {
            draw_under_clip_table(
                canvas,
                node_id,
                table,
                geom,
                frag,
                x_pt,
                y_pt,
                geometry,
                drawables,
                margin_left_pt,
                margin_top_pt,
                page_index,
            );
            continue;
        }
        // Fractional-opacity block with descendants: wrap own
        // paint + descendant fragments in a single
        // `draw_with_opacity` group so descendants paint inside the
        // parent's opacity transparency group (CSS opacity §2).
        // Without this, `<div opacity:0.4><svg>..</svg></div>`
        // paints the svg outside the parent's opacity wrap,
        // dropping the parent's opacity from the svg. (fulgur-gdb9)
        //
        // Body is excluded for the same reason as the clip arm
        // above: body has only a page-0 fragment so a
        // `draw_under_opacity(body)` would only wrap page-0
        // content while the `opacity_wrapped_descendants` skip
        // (collected above) excludes body explicitly so the main
        // loop dispatches body's descendants on pages 1+
        // normally. Without this exclusion `body { opacity: 0.5 }`
        // would silently blank pages 1+. (PR #314 follow-up Devin
        // Review)
        if let Some(block) = drawables
            .block_styles
            .get(&node_id)
            .filter(|b| !b.opacity_descendants.is_empty())
            .filter(|_| Some(node_id) != drawables.body_id)
        {
            draw_under_opacity(
                canvas,
                block,
                node_id,
                geom,
                frag,
                x_pt,
                y_pt,
                geometry,
                drawables,
                margin_left_pt,
                margin_top_pt,
                page_index,
            );
            continue;
        }
        dispatch_fragment(
            canvas,
            node_id,
            geom,
            frag,
            x_pt,
            y_pt,
            drawables,
            geometry,
            margin_left_pt,
            margin_top_pt,
            page_index,
        );
    }
}

/// Per-fragment leaf-draw dispatch shared by the main loop and the
/// transform special-case. Walks `node_id`'s payload maps and emits
/// the appropriate per-type draw — exactly the same logic as before
/// the transform refactor, just hoisted into a function so
/// `draw_under_transform` can re-use it for descendants.
/// PR 8g: dispatch inline-box content (and its descendants) under the
/// caller's `push_transform(translate(off_x, off_y))`. Mirrors the main
/// loop's transform / clip / opacity routing so CSS `transform`,
/// `overflow:hidden`, and fractional opacity on the inline-block are
/// honoured.
///
/// Called by `paragraph::draw_shaped_lines` for `LineItem::InlineBox`.
/// `(x_pt, y_pt)` is the body-relative dispatch position
/// (`margin + body_offset + content_frag.x.in_pt()`); the active
/// translate transform shifts the entire subtree to the inline-flow
/// position computed by the paragraph render path.
#[allow(clippy::too_many_arguments)]
pub(crate) fn dispatch_inline_box_content(
    canvas: &mut crate::draw_primitives::Canvas<'_, '_>,
    content_id: usize,
    content_geom: &crate::pagination_layout::PaginationGeometry,
    content_frag: &crate::pagination_layout::Fragment,
    x_pt: f32,
    y_pt: f32,
    drawables: &Drawables,
    geometry: &crate::pagination_layout::PaginationGeometryTable,
    margin_left_pt: f32,
    margin_top_pt: f32,
    page_index: u32,
    inline_box_subtree_descendants: &std::collections::BTreeMap<usize, Vec<usize>>,
) {
    if let Some(tx) = drawables.transforms.get(&content_id) {
        draw_under_transform(
            canvas,
            tx,
            content_id,
            content_geom,
            content_frag,
            x_pt,
            y_pt,
            geometry,
            drawables,
            margin_left_pt,
            margin_top_pt,
            page_index,
        );
        return;
    }
    if let Some(block) = drawables.block_styles.get(&content_id)
        && block.style.has_overflow_clip()
    {
        draw_under_clip(
            canvas,
            block,
            content_id,
            content_geom,
            content_frag,
            x_pt,
            y_pt,
            geometry,
            drawables,
            margin_left_pt,
            margin_top_pt,
            page_index,
        );
        return;
    }
    if let Some(block) = drawables.block_styles.get(&content_id)
        && !block.opacity_descendants.is_empty()
    {
        draw_under_opacity(
            canvas,
            block,
            content_id,
            content_geom,
            content_frag,
            x_pt,
            y_pt,
            geometry,
            drawables,
            margin_left_pt,
            margin_top_pt,
            page_index,
        );
        return;
    }
    // No wrapper effect on the inline-box content itself: dispatch at
    // body-relative `(x_pt, y_pt)` and walk its strict descendants
    // (`inline_box_subtree_descendants[content_id]`) at the same body-
    // relative frame. The caller's translate transform shifts everything.
    dispatch_fragment(
        canvas,
        content_id,
        content_geom,
        content_frag,
        x_pt,
        y_pt,
        drawables,
        geometry,
        margin_left_pt,
        margin_top_pt,
        page_index,
    );
    let descs: &[usize] = inline_box_subtree_descendants
        .get(&content_id)
        .map_or(&[], Vec::as_slice);
    {
        for &desc_id in descs {
            let Some(desc_geom) = geometry.get(&desc_id) else {
                continue;
            };
            let Some(desc_frag) = desc_geom
                .fragments
                .iter()
                .find(|f| f.page_index == page_index)
            else {
                continue;
            };
            let desc_x_pt =
                margin_left_pt + drawables.body_offset_pt.0.to_f32() + desc_frag.x.in_pt().to_f32();
            let desc_y_pt =
                margin_top_pt + drawables.body_offset_pt.1.to_f32() + desc_frag.y.in_pt().to_f32();
            dispatch_fragment(
                canvas,
                desc_id,
                desc_geom,
                desc_frag,
                desc_x_pt,
                desc_y_pt,
                drawables,
                geometry,
                margin_left_pt,
                margin_top_pt,
                page_index,
            );
        }
        // F22: the collapsed borders of the tables in the box (the box itself when it is an `inline-table`), where
        // their cells were dispatched: body-relative, under the caller's translation.
        paint_wrapper_tables(
            canvas,
            content_id,
            descs,
            page_index,
            margin_left_pt + drawables.body_offset_pt.0.to_f32(),
            margin_top_pt + drawables.body_offset_pt.1.to_f32(),
            geometry,
            drawables,
        );
    }
}

// F30f: which `(inline box, page)` continuations `paint_inline_box_continuation` painted on the page being drawn. A box
// can be reached from more than one walk (the main loop, a scope walk, the continuation of the box that carries it);
// the first one paints it, inside the groups it is called in, and the others find it in this set. Every box that
// `inline_box_continues_on` that page must be in it when the page is done (`check_continuations_painted`, debug
// builds): a split box whose later fragments no walk reaches prints nothing there.
thread_local! {
    static CONTINUATIONS_PAINTED: std::cell::RefCell<std::collections::BTreeSet<(usize, u32)>> =
        const { std::cell::RefCell::new(std::collections::BTreeSet::new()) };
}

/// Records the continuation of `ib_id` on `page_index`; `false` when it was recorded (painted) already.
fn note_continuation_painted(ib_id: usize, page_index: u32) -> bool {
    CONTINUATIONS_PAINTED.with(|m| m.borrow_mut().insert((ib_id, page_index)))
}

/// Forgets what was painted on `page_index` once the page is done. In debug builds first checks that every inline
/// box that continues on it was painted: the fragmenter may only split a box whose later fragments the renderer
/// reaches, through the main loop or through the scope walks (`draw_under_clip`, `draw_under_opacity`,
/// `draw_under_transform`) and the continuation of the box that carries it.
fn check_continuations_painted(
    page_index: u32,
    geometry: &crate::pagination_layout::PaginationGeometryTable,
    drawables: &Drawables,
) {
    let painted: std::collections::BTreeSet<usize> = CONTINUATIONS_PAINTED.with(|m| {
        let mut m = m.borrow_mut();
        let mine = m
            .iter()
            .filter(|(_, p)| *p == page_index)
            .map(|(id, _)| *id)
            .collect();
        m.clear();
        mine
    });
    if cfg!(debug_assertions) {
        for &ib_id in drawables.inline_box_subtree_descendants.keys() {
            if geometry
                .get(&ib_id)
                .is_some_and(|g| inline_box_continues_on(g, page_index))
            {
                debug_assert!(
                    painted.contains(&ib_id),
                    "inline box {ib_id} continues on page {page_index} and no walk painted it"
                );
            }
        }
    }
}

/// The inline boxes carried by the paragraphs of the inline box `ib_id` and of its descendants: the boxes nested in
/// it. They are not in its descendant list (their own subtrees are recorded under their own keys), and the carrier
/// that would paint their first fragment has no line on a continuation page.
fn nested_inline_boxes(ib_id: usize, drawables: &Drawables) -> Vec<usize> {
    let mut found = Vec::new();
    let descendants = drawables
        .inline_box_subtree_descendants
        .get(&ib_id)
        .map_or(&[][..], Vec::as_slice);
    for id in std::iter::once(&ib_id).chain(descendants) {
        let Some(para) = drawables.paragraphs.get(id) else {
            continue;
        };
        for item in para.lines.iter().flat_map(|line| line.items.iter()) {
            if let crate::paragraph::LineItem::InlineBox(nested) = item
                && let Some(nested_id) = nested.node_id
            {
                found.push(nested_id);
            }
        }
    }
    found
}

/// F30f: whether the inline box with geometry `geom` has a fragment on `page_index` that no paragraph paints: the box
/// was fragmented over several pages and this one is after the first.
///
/// An inline box (`inline-block`) is painted by the paragraph whose line holds it (`paragraph::draw_shaped_lines`),
/// once, with the fragment on the page that line is drawn on; its subtree is in `Drawables::inline_box_subtree_skip`,
/// which every walk leaves alone. That is all the box ever needed while it was one fragment. A box the fragmenter
/// splits between its line boxes has a fragment on every page, and the carrier's one line is on the first of them
/// (`record_carrier_slices`), so the later fragments have no one to paint them. Geometry that repeats a box on every
/// page (`position: fixed`) is painted whole by its carrier each time, and a box with one fragment never continues.
fn inline_box_continues_on(
    geom: &crate::pagination_layout::PaginationGeometry,
    page_index: u32,
) -> bool {
    geom.is_split()
        && geom
            .fragments
            .first()
            .is_some_and(|f| f.page_index < page_index)
        && geom.fragments.iter().any(|f| f.page_index == page_index)
}

/// F30f: the opacity of the paragraph that carries the inline box `ib_id`, 1 when it has none.
///
/// The carrier paints the box inside its paragraph's opacity group, which holds the opacity of the box that is the
/// carrier when that box has no block entry of its own (a box with an opacity scope and a block entry paints its
/// descendants itself, `draw_under_opacity`, and reaches the continuation from there). The continuation pages have
/// no carrier line, so the group has to be made for the box here, or it is painted at full opacity from page 2 on.
fn carrier_opacity(ib_id: usize, drawables: &Drawables) -> f32 {
    drawables
        .inline_box_carrier
        .get(&ib_id)
        .and_then(|carrier| drawables.paragraphs.get(carrier))
        .map_or(1.0, |para| para.opacity)
}

/// F30f: paint the fragment of the inline box `ib_id` that is on `page_index`, when
/// [`inline_box_continues_on`]: the pages after the one its carrier painted it on. Returns whether it painted.
///
/// The carrier moves the box to its place in the line with a translate (`paragraph.rs`, the `InlineBox` arm). Here
/// the box is where the fragmenter put it, so only the body offset that [`dispatch_inline_box_content`] adds to
/// the root and to every descendant is cancelled again: `margin_top_pt` is the page's own on a continuation page
/// (the body offset is page 0 only, `render_v2`), and `frag.x` is html-relative.
///
/// The descendants are painted by the helper (`inline_box_subtree_descendants`), so callers leave them alone, as they
/// do for the first page.
fn paint_inline_box_continuation(
    canvas: &mut crate::draw_primitives::Canvas<'_, '_>,
    ib_id: usize,
    page_index: u32,
    geometry: &crate::pagination_layout::PaginationGeometryTable,
    drawables: &Drawables,
    margin_left_pt: f32,
    margin_top_pt: f32,
) -> bool {
    if !drawables
        .inline_box_subtree_descendants
        .contains_key(&ib_id)
    {
        return false;
    }
    let Some(geom) = geometry.get(&ib_id) else {
        return false;
    };
    if !inline_box_continues_on(geom, page_index) {
        return false;
    }
    let Some(frag) = geom.fragments.iter().find(|f| f.page_index == page_index) else {
        return false;
    };
    if !note_continuation_painted(ib_id, page_index) {
        return false;
    }
    // The carrier skips a box that is not visible, with everything in it.
    let hidden = drawables
        .block_styles
        .get(&ib_id)
        .is_some_and(|b| !b.visible)
        || drawables.paragraphs.get(&ib_id).is_some_and(|p| !p.visible);
    if hidden {
        return false;
    }
    let (body_x, body_y) = (
        drawables.body_offset_pt.0.to_f32(),
        drawables.body_offset_pt.1.to_f32(),
    );
    let geo_x_pt = margin_left_pt + body_x + frag.x.in_pt().to_f32();
    let geo_y_pt = margin_top_pt + body_y + frag.y.in_pt().to_f32();
    let transform = krilla::geom::Transform::from_translate(-body_x, -body_y);
    let link_affine =
        crate::draw_primitives::Affine2D::translation((-body_x).as_pt(), (-body_y).as_pt());
    crate::draw_primitives::draw_with_opacity(
        canvas,
        carrier_opacity(ib_id, drawables),
        |canvas| {
            if let Some(lc) = canvas.link_collector.as_deref_mut() {
                lc.push_transform(link_affine);
            }
            canvas.surface.push_transform(&transform);
            canvas.paint_extent.enter_local();
            // F30f-b: shifted by the body offset, so not page coordinates: no block-axis clip band here.
            let saved_clip_band =
                std::mem::replace(&mut canvas.clip_band, crate::draw_primitives::ClipBand::Off);
            dispatch_inline_box_content(
                canvas,
                ib_id,
                geom,
                frag,
                geo_x_pt,
                geo_y_pt,
                drawables,
                geometry,
                margin_left_pt,
                margin_top_pt,
                page_index,
                &drawables.inline_box_subtree_descendants,
            );
            canvas.clip_band = saved_clip_band;
            canvas.paint_extent.leave_local();
            canvas.surface.pop();
            if let Some(lc) = canvas.link_collector.as_deref_mut() {
                lc.pop_transform();
            }
        },
    );
    // The boxes nested in this one: their carrier is a descendant that has no line on this page.
    for nested in nested_inline_boxes(ib_id, drawables) {
        paint_inline_box_continuation(
            canvas,
            nested,
            page_index,
            geometry,
            drawables,
            margin_left_pt,
            margin_top_pt,
        );
    }
    true
}

/// Returns `true` if any line item in `entry` is a text run or inline image
/// with a non-`None` link. Used to decide whether to activate per-run tagging
/// mode in `dispatch_fragment` rather than the normal paragraph-level tagging.
fn para_has_link_runs(entry: &crate::drawables::ParagraphEntry) -> bool {
    entry.lines.iter().any(|line| {
        line.items.iter().any(|item| match item {
            crate::paragraph::LineItem::Text(run) => run.link.is_some(),
            crate::paragraph::LineItem::Image(img) => img.link.is_some(),
            crate::paragraph::LineItem::InlineBox(_) => false,
        })
    })
}

/// The struct-tree node id that per-run link tags for `node_id` should nest
/// under, or `None` when the node has no home in the tag tree — in which case
/// per-run tagging MUST be skipped. Otherwise `RunRegionTracker` records a
/// `ParagraphRunItem::LinkContent` that marks the span "wired", so
/// `emit_link_annotations` uses `add_tagged_annotation`, but `build_struct_tree`
/// only walks ids present in `drawables.semantics`; the annotation's OBJR never
/// enters the StructTree and Krilla panics during serialization
/// ("annotation identifier ... doesn't appear in the tag tree").
///
/// A semantic `<li>` remaps to its synthetic LBody id. Any other node must
/// itself be present in `drawables.semantics`. A CSS list item or inline root
/// whose element has no `SemanticEntry` — `<a>`, a custom element, `<body>`,
/// etc., for which `classify_element` returns `None` — yields `None` here and
/// keeps its link annotation untagged.
fn run_tag_target(
    drawables: &Drawables,
    node_id: crate::drawables::NodeId,
) -> Option<crate::drawables::NodeId> {
    drawables.li_lbody_ids.get(&node_id).copied().or_else(|| {
        drawables
            .semantics
            .contains_key(&node_id)
            .then_some(node_id)
    })
}

/// Collect the plain-text title from a paragraph's shaped lines.
///
/// Returns the concatenated text of all `Text` run items across all lines.
/// Used to populate the `/T` (Title) attribute on heading tags required by
/// PDF/UA-1.
fn extract_heading_title(para: &crate::drawables::ParagraphEntry) -> String {
    para.lines
        .iter()
        .flat_map(|line| line.items.iter())
        .filter_map(|item| match item {
            crate::paragraph::LineItem::Text(run) => Some(run.text.as_ref()),
            _ => None,
        })
        .collect()
}

/// Compute the `/T` (Title) attribute value for a heading tag, honouring
/// `ParagraphEntry.visible`.
///
/// Returns `None` when the paragraph is invisible (CSS `visibility: hidden`
/// or `visibility: collapse` — stored as `visible = false`) so that the
/// heading's text does not leak into the tag tree while the paint path
/// suppresses it (`draw_paragraph_inner_paint` returns early for
/// `!entry.visible`). Also returns `None` for a visible-but-empty title,
/// which keeps `Tag::Hn` from emitting a zero-length `/T`.
///
/// Called from both title-extraction sinks — `try_start_tagged` (the normal
/// paragraph tagging path) and the `build_struct_tree` per-run backfill
/// (heading whose content is per-run tagged because of an inner link) — so
/// the same `visible` gate applies to every path that can produce a
/// heading `/T` attribute.
fn heading_title_of(para: &crate::drawables::ParagraphEntry) -> Option<String> {
    if !para.visible {
        return None;
    }
    let title = extract_heading_title(para);
    (!title.is_empty()).then_some(title)
}

/// Start a Krilla tagged content sequence for a paragraph-bearing node when
/// tagging is enabled and the node has a P / H / Span semantic entry.
///
/// Returns `Some((tag, id))` on success so that `finish_tagged` can close it.
/// Returns `None` when tagging is disabled, the node has no recognised
/// semantic entry, the node carries no paragraph content (pure containers
/// must not call `start_tagged` — it is not nestable and would panic on a
/// second call before `end_tagged`), OR the canvas is already inside an
/// outer marked-content sequence (`canvas.in_marked_content == true`). The
/// last case fires when a tagged inline-root block dispatches an inline-box
/// whose content would itself want a P/Span/LBody tag; nested opens would
/// panic in Krilla (`can't start marked content twice`), so the inner tag is
/// dropped and its sub-tree renders as a graceful, untagged extension of the
/// outer sequence. Link annotations under the skipped tag still land in the
/// PDF via `emit_link_annotations`'s unwired-fallback path (`add_annotation`).
fn try_start_tagged(
    canvas: &mut crate::draw_primitives::Canvas<'_, '_>,
    node_id: usize,
    drawables: &Drawables,
) -> Option<(
    usize, // record_id (TagCollector.record() に渡す NodeId)
    crate::tagging::PdfTag,
    krilla::tagging::Identifier,
    Option<String>,
)> {
    canvas.tag_collector.as_ref()?;
    if canvas.in_marked_content {
        return None;
    }
    let semantic = drawables.semantics.get(&node_id)?;
    let result = match &semantic.tag {
        crate::tagging::PdfTag::P | crate::tagging::PdfTag::Span => {
            use krilla::tagging::{ContentTag, SpanTag};
            let id = canvas
                .surface
                .start_tagged(ContentTag::Span(SpanTag::empty()));
            Some((node_id, semantic.tag.clone(), id, None))
        }
        crate::tagging::PdfTag::H { .. } => {
            let heading_title = drawables
                .paragraphs
                .get(&node_id)
                .and_then(heading_title_of);
            use krilla::tagging::{ContentTag, SpanTag};
            let id = canvas
                .surface
                .start_tagged(ContentTag::Span(SpanTag::empty()));
            Some((node_id, semantic.tag.clone(), id, heading_title))
        }
        crate::tagging::PdfTag::Li => {
            // inline-root li: コンテンツを lbody_id で記録（LBody 配下に収める）
            let &lbody_id = drawables.li_lbody_ids.get(&node_id)?;
            use krilla::tagging::{ContentTag, SpanTag};
            let id = canvas
                .surface
                .start_tagged(ContentTag::Span(SpanTag::empty()));
            Some((lbody_id, crate::tagging::PdfTag::LBody, id, None))
        }
        _ => None,
    };
    if result.is_some() {
        canvas.in_marked_content = true;
    }
    result
}

/// Close a tagged content sequence opened by `try_start_tagged` and record
/// the resulting `Identifier` in the `TagCollector` for StructTree assembly.
///
/// No-op when `tag_info` is `None` (tagging disabled or not applicable).
///
/// # Invariant
/// `tag_info` must only hold values produced by `try_start_tagged` on the
/// same `canvas`. `try_start_tagged` returns `None` when `tag_collector` is
/// absent, so if `tag_info` is `Some`, `canvas.tag_collector` is guaranteed
/// to be `Some` as well.
fn finish_tagged(
    canvas: &mut crate::draw_primitives::Canvas<'_, '_>,
    tag_info: Option<(
        usize, // record_id
        crate::tagging::PdfTag,
        krilla::tagging::Identifier,
        Option<String>,
    )>,
) {
    if let Some((record_id, tag, id, heading_title)) = tag_info {
        canvas.surface.end_tagged();
        canvas.in_marked_content = false;
        canvas
            .tag_collector
            .as_mut()
            .expect("tag_collector is Some when tag_info is Some")
            .record(record_id, tag, id, heading_title);
    }
}

#[allow(clippy::too_many_arguments)]
pub(crate) fn dispatch_fragment(
    canvas: &mut crate::draw_primitives::Canvas<'_, '_>,
    node_id: usize,
    geom: &crate::pagination_layout::PaginationGeometry,
    frag: &crate::pagination_layout::Fragment,
    x_pt: f32,
    y_pt: f32,
    drawables: &Drawables,
    geometry: &crate::pagination_layout::PaginationGeometryTable,
    margin_left_pt: f32,
    margin_top_pt: f32,
    page_index: u32,
) {
    if let Some(table) = drawables.tables.get(&node_id) {
        draw_table_v2(
            canvas,
            node_id,
            table,
            x_pt,
            y_pt,
            frag,
            geom.table_pieces,
            geometry,
            drawables,
            margin_left_pt,
            margin_top_pt,
            page_index,
        );
        return;
    }
    // F30: a paragraph split between line boxes records the span of its
    // lines as the fragment height; its frame (border + padding) is
    // `line_split_insets` bigger. The paragraph slicing below still reads
    // `geom.fragments`, so only the frame sees the adjusted fragment.
    // html-to-pdf F198: a paragraph that is a box cut over columns has a fragment (a frame) in each of them, and the
    // slices of all its columns on a page are painted once, with the last of its fragments there (after every frame).
    let last_piece_on_page = is_last_piece_on_page(geom, frag);
    let (frame_frag, edges) = frame_fragment(geom, frag);
    let frag = &frame_frag;
    // F30f-d: a box sliced at its breaks paints its open edges without border, padding or radius.
    let sliced_block;
    let sliced_block_ref = match drawables.block_styles.get(&node_id) {
        Some(block) if edges.any() => {
            sliced_block = crate::drawables::BlockEntry {
                style: crate::draw_primitives::sliced_style(&block.style, edges).into_owned(),
                ..block.clone()
            };
            Some(&sliced_block)
        }
        other => other,
    };
    // True when this block / list-item / paragraph spans multiple
    // pages (the fragmenter recorded one fragment per page slice).
    // Passed down so `draw_block_inner_paint` can use the per-page
    // slice height (`frag.height`) instead of `layout_size.height`
    // — without it, every slice paints the FULL block, which
    // doubled the callout in `examples/break-inside` (fulgur-bq6i).
    let is_split = geom.is_split();
    // ListItem case: marker + body block + inline-root paragraph
    // share a single opacity group. See `draw_list_item_with_block`
    // for the v1 mirror.
    if let Some(li) = drawables.list_items.get(&node_id) {
        let block_for_li = sliced_block_ref;
        let para_for_li = drawables.paragraphs.get(&node_id);
        draw_list_item_with_block(
            canvas,
            node_id,
            li,
            block_for_li,
            para_for_li,
            x_pt,
            y_pt,
            frag,
            &geom.fragments,
            &geom.line_slices,
            page_index,
            is_split,
            drawables,
            geometry,
            margin_left_pt,
            margin_top_pt,
        );
        return;
    }
    // fulgur-6q5 Task 8: when a multicol container splits a paragraph
    // across columns, `convert_multicol_paragraph_slices` records a
    // per-source-NodeId override in `drawables.paragraph_slices`. The
    // standard `paragraphs[node_id]` path would render every line at
    // the source's body-relative position (column 0 only); the override
    // suppresses that path and paints one slice per non-empty column at
    // the slice origin (computed from the multicol container's
    // border-box top-left + per-column line frame).
    let has_paragraph_slices = drawables.paragraph_slices.contains_key(&node_id);
    // Block + inner content (paragraph / image / svg) sharing the
    // same node_id: combine into one `draw_with_opacity` group. See
    // `draw_block_with_inner_content` for the v1 mirror.
    if let Some(block) = sliced_block_ref {
        // Suppress the inline-paragraph branch of
        // `draw_block_with_inner_content` when paragraph_slices owns
        // this NodeId (Case A: container is itself the inline root, so
        // it carries both `block_styles` and `paragraphs` entries — we
        // still need the block's bg / border / shadow but the lines
        // come from the slice override painted afterward).
        let para_for_block = if has_paragraph_slices {
            None
        } else {
            drawables.paragraphs.get(&node_id)
        };
        let img_for_block = drawables.images.get(&node_id);
        let svg_for_block = drawables.svgs.get(&node_id);
        if para_for_block.is_some() || img_for_block.is_some() || svg_for_block.is_some() {
            let run_tag_node_id = run_tag_target(drawables, node_id);
            let use_run_tagging = para_for_block
                .map(|p| canvas.tag_collector.is_some() && para_has_link_runs(p))
                .unwrap_or(false)
                && run_tag_node_id.is_some();
            let tag_info = if use_run_tagging {
                canvas.link_run_node_id = run_tag_node_id;
                None
            } else if para_for_block.is_some() {
                try_start_tagged(canvas, node_id, drawables)
            } else {
                None
            };
            draw_block_with_inner_content(
                canvas,
                block,
                para_for_block,
                img_for_block,
                svg_for_block,
                x_pt,
                y_pt,
                frag,
                &geom.fragments,
                &geom.line_slices,
                page_index,
                is_split,
                drawables,
                geometry,
                margin_left_pt,
                margin_top_pt,
            );
            if has_paragraph_slices && last_piece_on_page {
                paint_multicol_paragraph_slices(
                    canvas,
                    drawables,
                    geometry,
                    node_id,
                    margin_left_pt,
                    margin_top_pt,
                    page_index,
                );
            }
            finish_tagged(canvas, tag_info);
            if use_run_tagging {
                canvas.link_run_node_id = None;
            }
            return;
        }
        draw_block_v2(canvas, block, x_pt, y_pt, frag, is_split);
    }
    if let Some(img) = drawables.images.get(&node_id) {
        draw_image_v2(canvas, img, x_pt, y_pt);
        return;
    }
    if let Some(svg) = drawables.svgs.get(&node_id) {
        draw_svg_v2(canvas, svg, x_pt, y_pt);
        return;
    }
    if has_paragraph_slices {
        if last_piece_on_page {
            paint_multicol_paragraph_slices(
                canvas,
                drawables,
                geometry,
                node_id,
                margin_left_pt,
                margin_top_pt,
                page_index,
            );
        }
        return;
    }
    if let Some(para) = drawables.paragraphs.get(&node_id) {
        draw_paragraph_dispatch(
            canvas,
            node_id,
            para,
            geom,
            x_pt,
            y_pt,
            drawables,
            geometry,
            margin_left_pt,
            margin_top_pt,
            page_index,
        );
    }
}

/// The paragraph of `node_id` at `(x_pt, y_pt)` for the fragment of `geom` on `page_index`, with its tagging (a
/// marked-content sequence, or per-run link tags when it holds links). The tail of [`dispatch_fragment`] for a node
/// whose paragraph is not drawn together with a block, and what [`dispatch_page_node`] draws for `<body>` as the
/// inline root (F111).
#[allow(clippy::too_many_arguments)]
fn draw_paragraph_dispatch(
    canvas: &mut crate::draw_primitives::Canvas<'_, '_>,
    node_id: usize,
    para: &crate::drawables::ParagraphEntry,
    geom: &crate::pagination_layout::PaginationGeometry,
    x_pt: f32,
    y_pt: f32,
    drawables: &Drawables,
    geometry: &crate::pagination_layout::PaginationGeometryTable,
    margin_left_pt: f32,
    margin_top_pt: f32,
    page_index: u32,
) {
    let run_tag_node_id = run_tag_target(drawables, node_id);
    let use_run_tagging =
        canvas.tag_collector.is_some() && para_has_link_runs(para) && run_tag_node_id.is_some();
    let tag_info = if use_run_tagging {
        canvas.link_run_node_id = run_tag_node_id;
        None
    } else {
        try_start_tagged(canvas, node_id, drawables)
    };
    draw_paragraph_v2(
        canvas,
        para,
        x_pt,
        y_pt,
        &geom.fragments,
        &geom.line_slices,
        page_index,
        geom.is_split(),
        drawables,
        geometry,
        margin_left_pt,
        margin_top_pt,
    );
    finish_tagged(canvas, tag_info);
    if use_run_tagging {
        canvas.link_run_node_id = None;
    }
}

/// fulgur-6q5 Task 8: paint a paragraph that `multicol_layout`
/// distributed across columns. The standard `draw_paragraph_v2` path
/// renders every line at the source's body-relative position (column 0
/// only); this override paints one slice per non-empty column at the
/// slice origin recorded by `convert_multicol_paragraph_slices`.
///
/// The slice's `origin_pt` is **multicol container border-box-relative**
/// in PDF pt (per the `ParagraphSlice` doc on `drawables.rs`). Resolve
/// the container's body-relative page position via
/// `geometry[container_node_id]` — same shape as the main loop's
/// `(margin_left_pt + frag.x.in_pt(), margin_top_pt + frag.y.in_pt())`
/// — and add the slice origin. Honour the source paragraph's `visible`
/// and `opacity` exactly like `draw_paragraph_v2`. `lines` are
/// pre-rebased (Task 7) so each slice's first line has
/// `baseline = ascent` from the slice top — no further rebase here.
///
/// **Case A opacity limitation (fulgur-6q5 follow-up):** when the
/// multicol container itself has `opacity:N` (Case A:
/// `<div opacity:0.5; column-count:2>text</div>`),
/// `paragraphs[node_id].opacity` is forced to 1.0 by `extract_paragraph`
/// (because `needs_block` is true), so this helper applies no alpha to
/// the slice text. The container's block layer already paints at
/// `block.opacity`, but slices are emitted **outside** that
/// `draw_with_opacity` group — so text loses block opacity. This is a
/// strict improvement over pre-Task-8 (which kept all text in col 0).
/// Revisit if Task 10 VRT or downstream usage exposes a regression;
/// the fix likely requires invoking the slice paint inside the block's
/// opacity group, not at the dispatch level.
///
/// **Multi-page container handling (fulgur-6q5 Fix 4):** when the
/// multicol container straddles a page boundary, the container has
/// multiple fragments and `slice.origin_pt` is measured from the
/// **container border-box top** (i.e. the start of the FIRST
/// fragment), not from the current page's fragment. The helper
/// detects the multi-fragment case (`container_geom.fragments.len() >
/// 1`), computes `consumed = sum of prior fragments' heights`,
/// rebases each slice's y into the current fragment's frame, and
/// paints only the slices that fit *wholly* within the visible strip
/// on this page. Slices that straddle a page boundary are NOT
/// painted on either page — mid-line page splitting of multicol
/// slices is out of scope (fulgur-6q5 follow-up).
///
/// The single-fragment fast path (the common case) preserves the
/// pre-Fix-4 behaviour: paint every slice at `container_origin +
/// origin_pt`, no rebase, no visibility filter. This is required
/// because the per-fragment `height` excludes the container's own
/// padding, so a per-slice visibility filter would false-positive on
/// padded single-page containers — slice `origin_pt.y + size_pt.1`
/// legitimately exceeds `target_frag.height` (in pt) when the
/// container has padding (see VRT
/// `multicol-inline-root-split-case-a`).
fn paint_multicol_paragraph_slices(
    canvas: &mut crate::draw_primitives::Canvas<'_, '_>,
    drawables: &Drawables,
    geometry: &crate::pagination_layout::PaginationGeometryTable,
    source_node_id: usize,
    margin_left_pt: f32,
    margin_top_pt: f32,
    page_index: u32,
) {
    use crate::draw_primitives::draw_with_opacity;
    let Some(slices_entry) = drawables.paragraph_slices.get(&source_node_id) else {
        return;
    };
    let para = drawables.paragraphs.get(&source_node_id);
    if let Some(p) = para
        && !p.visible
    {
        return;
    }
    let opacity = para.map(|p| p.opacity).unwrap_or(1.0);
    // Resolve the container's body-relative position on this page from
    // the geometry table — same convention as `dispatch_fragment`'s
    // caller. Skip when the container has no fragment on `page_index`
    // (e.g. earlier-page-only containers shouldn't repaint here).
    let Some(container_geom) = geometry.get(&slices_entry.container_node_id) else {
        return;
    };
    let Some(target_pos) = container_geom
        .fragments
        .iter()
        .position(|f| f.page_index == page_index)
    else {
        return;
    };
    let target_frag = &container_geom.fragments[target_pos];

    // fulgur-6q5 Fix 4: mirror `paint_multicol_rule_for_page`'s
    // partitioning. `consumed` = sum of prior fragments' heights, so
    // a slice's effective top in the current fragment frame is
    // `slice.origin_pt.1 - consumed`. `cutoff` is the visible strip's
    // height on this page.
    let consumed: f32 = container_geom.fragments[..target_pos]
        .iter()
        .map(|f| f.height)
        .sum::<crate::units::Px>()
        .in_pt()
        .to_f32();
    let cutoff = target_frag.height.in_pt().to_f32();

    let container_x_pt = margin_left_pt.as_pt() + target_frag.x.in_pt();
    let container_y_pt = margin_top_pt.as_pt() + target_frag.y.in_pt();

    // Single-fragment containers (the common case) keep the original
    // behaviour: paint every slice at `container_origin + origin_pt`,
    // unconditionally. The container's fragment height is reported in
    // a "visible strip" frame that excludes the container's own
    // padding, so a per-slice `slice_bottom <= cutoff` filter would
    // false-positive on padded single-page containers (e.g. VRT
    // `multicol-inline-root-split-case-a`: padding 40px, cutoff
    // ≈ 88pt, but valid slices reach origin_pt.1 + size_pt.1 ≈ 105pt).
    //
    // `is_split()` (false when `is_repeat=true`) is the right gate:
    // multicol containers can't be `position: fixed` (the only producer
    // of `is_repeat=true` geometry; see `pagination_layout.rs:2251`),
    // so `is_split() == fragments.len() > 1` for any valid input here.
    // Using the predicate documents the intent.
    let needs_partition = container_geom.is_split();
    let run_tag_node_id = run_tag_target(drawables, source_node_id);
    let use_run_tagging = canvas.tag_collector.is_some()
        && drawables
            .paragraphs
            .get(&source_node_id)
            .is_some_and(para_has_link_runs)
        && run_tag_node_id.is_some();
    if use_run_tagging {
        canvas.link_run_node_id = run_tag_node_id;
    }
    draw_with_opacity(canvas, opacity, |canvas| {
        for slice in &slices_entry.slices {
            // F30d: a slice the fragmenter placed on a page is painted on that page alone, at the origin it
            // was given relative to the container's fragment there. No height arithmetic decides it.
            if let Some(slice_page) = slice.page_index {
                if slice_page == page_index {
                    let abs_x = container_x_pt + slice.origin_pt.0;
                    let abs_y = container_y_pt + slice.origin_pt.1;
                    draw_slice_tagged(
                        canvas,
                        drawables,
                        geometry,
                        source_node_id,
                        use_run_tagging,
                        slice,
                        abs_x,
                        abs_y,
                        page_index,
                        margin_left_pt,
                        margin_top_pt,
                    );
                }
                continue;
            }
            let slice_top = slice.origin_pt.1.to_f32() - consumed;
            let slice_bottom = slice_top + slice.size_pt.1.to_f32();
            if needs_partition {
                // Multi-fragment container: rebase + visibility filter.
                // Skip slices that don't intersect this page's visible
                // range. Also skip slices that straddle the page
                // boundary — mid-line page splitting of multicol
                // slices is out of scope (fulgur-6q5 follow-up).
                // Skipping prevents off-page replay; the trade-off is
                // a slice that crosses a fragment break disappears on
                // every page it would have crossed. Revisit once
                // split-aware rendering of multicol slices lands.
                let above = slice_bottom <= 0.0;
                let below = slice_top >= cutoff;
                let straddles = slice_top < 0.0 || slice_bottom > cutoff;
                if above || below || straddles {
                    continue;
                }
            }
            let abs_x = container_x_pt + slice.origin_pt.0;
            let abs_y = container_y_pt + slice_top.as_pt();
            draw_slice_tagged(
                canvas,
                drawables,
                geometry,
                source_node_id,
                use_run_tagging,
                slice,
                abs_x,
                abs_y,
                page_index,
                margin_left_pt,
                margin_top_pt,
            );
        }
    });
    if use_run_tagging {
        canvas.link_run_node_id = None;
    }
}

/// Paint one multicol paragraph slice. Unless per-run link tagging is on (it tags the runs itself), the slice is
/// one marked-content sequence of its source's tag, like a paragraph painted by `draw_paragraph_v2`: a paragraph
/// cut over columns and pages is several sequences under the same structure element (F30d review: its text was
/// outside any marked content and its `/P` elements were empty).
///
/// html-to-pdf F198b: the inline boxes of its lines (an image, an `<svg>`, an inline-block) are painted like any paragraph's
/// (`inline_box_ctx`): the fragmenter recorded each of them on the page of its piece. It is a no-op for a slice without
/// inline boxes.
#[allow(clippy::too_many_arguments)]
fn draw_slice_tagged(
    canvas: &mut crate::draw_primitives::Canvas<'_, '_>,
    drawables: &Drawables,
    geometry: &crate::pagination_layout::PaginationGeometryTable,
    source_node_id: usize,
    use_run_tagging: bool,
    slice: &crate::drawables::ParagraphSlice,
    abs_x: crate::units::Pt,
    abs_y: crate::units::Pt,
    page_index: u32,
    margin_left_pt: f32,
    margin_top_pt: f32,
) {
    let tag_info = if use_run_tagging {
        None
    } else {
        try_start_tagged(canvas, source_node_id, drawables)
    };
    let inline_box_ctx = Some(crate::paragraph::InlineBoxRenderCtx {
        drawables,
        geometry,
        page_index,
        margin_left_pt,
        margin_top_pt,
    });
    crate::paragraph::draw_shaped_lines(canvas, &slice.lines, abs_x, abs_y, inline_box_ctx);
    finish_tagged(canvas, tag_info);
}

/// Collect the inline-box subtrees that paragraphs drawn *inside a scope*
/// already paint, so the scope's own descendant walk does not paint them a
/// second time.
///
/// `scope_ids` must yield the scope owner plus every id in the walk's
/// descendant list: any paragraph in that subtree is drawn inside the
/// scope, so whatever inline boxes it carries are its to paint.
///
/// This is deliberately narrower than `Drawables::inline_box_subtree_skip`.
/// That set is global, which is right for the top-level dispatch loop —
/// every member is painted by *some* paragraph — but wrong inside a scope.
/// A scoped inline-block's own subtree lands in the global set only because
/// the inline-block is an inline box in its *parent's* paragraph; when the
/// inline-block has no paragraph of its own, as in
///
/// ```html
/// <div style="display:inline-block;overflow:hidden">
///   <div style="background:red"></div>
/// </div>
/// ```
///
/// the scope walk is the only thing that would paint that child. Skipping
/// it there drops the content outright — the regression PR #677 shipped by
/// reaching for the global set here (fulgur-49xw).
fn paragraph_owned_inline_boxes(
    scope_ids: impl IntoIterator<Item = usize>,
    drawables: &Drawables,
) -> std::collections::BTreeSet<usize> {
    let mut skip = std::collections::BTreeSet::new();
    for id in scope_ids {
        let Some(para) = drawables.paragraphs.get(&id) else {
            continue;
        };
        for item in para.lines.iter().flat_map(|line| line.items.iter()) {
            let crate::paragraph::LineItem::InlineBox(ib) = item else {
                continue;
            };
            let Some(ib_id) = ib.node_id else {
                continue;
            };
            // `paragraph::draw_shaped_lines` hands the box to
            // `dispatch_inline_box_content`, which paints the box itself
            // and walks `inline_box_subtree_descendants[ib_id]`.
            skip.insert(ib_id);
            if let Some(descs) = drawables.inline_box_subtree_descendants.get(&ib_id) {
                skip.extend(descs.iter().copied());
            }
        }
    }
    skip
}

/// Push the transform onto the surface + link collector, dispatch the
/// wrapper node's own payload and every descendant fragment that lands
/// on `page_index`, then pop:
///
/// ```text
/// canvas.surface.push_transform(matrix);
/// inner.draw(canvas, x, y, ...);
/// canvas.surface.pop();
/// ```
///
/// The link collector also receives the transform so `/Link`
/// annotation rects are mapped into device space.
#[allow(clippy::too_many_arguments)]
fn draw_under_transform(
    canvas: &mut crate::draw_primitives::Canvas<'_, '_>,
    tx: &crate::drawables::TransformEntry,
    node_id: usize,
    geom: &crate::pagination_layout::PaginationGeometry,
    frag: &crate::pagination_layout::Fragment,
    x_pt: f32,
    y_pt: f32,
    geometry: &crate::pagination_layout::PaginationGeometryTable,
    drawables: &Drawables,
    margin_left_pt: f32,
    margin_top_pt: f32,
    page_index: u32,
) {
    // Apply the transform around the box's transform-origin
    // (CSS Transforms §12): the effective matrix is
    //   T(x + ox, y + oy) · M · T(-(x + ox), -(y + oy))
    let ox = x_pt + tx.origin.x.to_f32();
    let oy = y_pt + tx.origin.y.to_f32();
    use crate::draw_primitives::Affine2D;
    let full = Affine2D::translation(ox.as_pt(), oy.as_pt())
        * tx.matrix
        * Affine2D::translation((-ox).as_pt(), (-oy).as_pt());

    if let Some(lc) = canvas.link_collector.as_deref_mut() {
        lc.push_transform(full);
    }
    canvas.surface.push_transform(&full.to_krilla());
    canvas.paint_extent.enter_local();
    // F30f-b: under the matrix the coordinates are not page coordinates: no block-axis clip band here.
    let saved_clip_band =
        std::mem::replace(&mut canvas.clip_band, crate::draw_primitives::ClipBand::Off);

    // Dispatch the wrapper's own payload first (the same NodeId
    // carries both the transform and its inner content), before
    // recursing into descendants.
    dispatch_fragment(
        canvas,
        node_id,
        geom,
        frag,
        x_pt,
        y_pt,
        drawables,
        geometry,
        margin_left_pt,
        margin_top_pt,
        page_index,
    );

    // Then dispatch every strict descendant on this page. Each
    // descendant's fragment carries its own (x, y) in untransformed
    // local coordinates — the surface transform applies on top so the
    // descendants paint under their ancestor's matrix.
    //
    // Descendants that have their OWN `TransformEntry` recurse into
    // `draw_under_transform` so their matrix composes with the outer
    // push (nested transforms compose right-to-left; each level
    // pushes its own matrix — CSS Transforms §11). Without this
    // recursion the inner transform would be silently dropped,
    // breaking `<div style="transform:rotate"><div style="transform:scale">`
    // (PR #305 Devin).
    //
    // Pre-skip the strict descendants of any nested transform so they
    // are not dispatched twice — the nested `draw_under_transform`
    // already iterates `desc_tx.descendants` and paints them under
    // the composed matrix; iterating them again here via
    // `dispatch_fragment` would emit a SECOND draw under the outer
    // transform only (missing the inner matrix). Bug confirmed by
    // PR #305 follow-up Devin trace for
    // `<div transform:A><div transform:B><p>text</p></div></div>`.
    let nested_skip: std::collections::BTreeSet<usize> = tx
        .descendants
        .iter()
        .filter_map(|id| drawables.transforms.get(id))
        .flat_map(|inner_tx| inner_tx.descendants.iter().copied())
        .collect();
    // Symmetric pre-skip for `overflow:hidden` descendants. When a
    // clip block sits inside this transform, `draw_under_clip` (called
    // below) iterates its own `clip_descendants` to paint them inside
    // the clip; iterating those nodes again here via
    // `dispatch_fragment` would double-paint them outside the clip.
    // Mirrors the symmetric handling in `draw_under_clip`'s descendant
    // loop (PR #309 follow-up Devin).
    let mut clip_skip: std::collections::BTreeSet<usize> = tx
        .descendants
        .iter()
        .filter_map(|id| drawables.block_styles.get(id))
        .filter(|b| b.style.has_overflow_clip())
        .flat_map(|b| b.clip_descendants.iter().copied())
        .collect();
    // Tables with `overflow: hidden | clip` carry `clip_descendants`
    // too — `draw_under_clip_table` (called below) iterates them
    // inside the clip path, so the dispatch loop must skip them
    // here just like the block-clip case (fulgur-bvhw PR #320 Devin).
    clip_skip.extend(
        tx.descendants
            .iter()
            .filter_map(|id| drawables.tables.get(id))
            .filter(|t| t.style.has_overflow_clip())
            .flat_map(|t| t.clip_descendants.iter().copied()),
    );
    // Symmetric pre-skip for opacity-scoped descendants. When an
    // opacity block sits inside this transform, `draw_under_opacity`
    // (called below) iterates its own `opacity_descendants` to paint
    // them inside the opacity wrap; iterating those nodes again here
    // via `dispatch_fragment` would emit a second draw outside the
    // opacity group. Mirrors `clip_skip`. (fulgur-gdb9)
    let opacity_skip: std::collections::BTreeSet<usize> = tx
        .descendants
        .iter()
        .filter_map(|id| drawables.block_styles.get(id))
        .flat_map(|b| b.opacity_descendants.iter().copied())
        .collect();
    // Inline boxes carried by paragraphs inside this transform are painted
    // by those paragraphs' `LineItem::InlineBox` arms. Re-dispatching them
    // here paints them twice — and because the duplicate re-enters
    // `draw_under_transform` for a nested transformed inline-block, the
    // cost doubles per nesting level rather than staying constant.
    let paragraph_ib_skip = paragraph_owned_inline_boxes(
        std::iter::once(node_id).chain(tx.descendants.iter().copied()),
        drawables,
    );
    for &desc_id in &tx.descendants {
        if nested_skip.contains(&desc_id)
            || clip_skip.contains(&desc_id)
            || opacity_skip.contains(&desc_id)
        {
            continue;
        }
        if paragraph_ib_skip.contains(&desc_id) {
            // F30f: a box fragmented over several pages is painted by its carrier on the first of them only; the
            // later pages are painted here, under the transform like the first (a transformed ancestor does not stop
            // the fragmenter: its `div` has a fragment on every page the box touches).
            paint_inline_box_continuation(
                canvas,
                desc_id,
                page_index,
                geometry,
                drawables,
                margin_left_pt,
                margin_top_pt,
            );
            continue;
        }
        let Some(desc_geom) = geometry.get(&desc_id) else {
            continue;
        };
        for desc_frag in &desc_geom.fragments {
            if desc_frag.page_index != page_index {
                continue;
            }
            let desc_x = margin_left_pt + desc_frag.x.in_pt().to_f32();
            let desc_y = margin_top_pt + desc_frag.y.in_pt().to_f32();
            if let Some(desc_tx) = drawables.transforms.get(&desc_id) {
                draw_under_transform(
                    canvas,
                    desc_tx,
                    desc_id,
                    desc_geom,
                    desc_frag,
                    desc_x,
                    desc_y,
                    geometry,
                    drawables,
                    margin_left_pt,
                    margin_top_pt,
                    page_index,
                );
            } else if let Some(desc_block) = drawables
                .block_styles
                .get(&desc_id)
                .filter(|b| b.style.has_overflow_clip())
                .filter(|_| Some(desc_id) != drawables.body_id)
            {
                // Descendant carries `overflow:hidden|clip` — push its
                // clip path the same way the main loop does. Without
                // this, transforms wrapping a clipping block would
                // emit the inner block's bg/border via
                // `dispatch_fragment` but never push the clip, leaking
                // overflow content past the clip boundary
                // (PR #309 follow-up Devin).
                draw_under_clip(
                    canvas,
                    desc_block,
                    desc_id,
                    desc_geom,
                    desc_frag,
                    desc_x,
                    desc_y,
                    geometry,
                    drawables,
                    margin_left_pt,
                    margin_top_pt,
                    page_index,
                );
            } else if let Some(desc_table) = drawables
                .tables
                .get(&desc_id)
                .filter(|t| t.style.has_overflow_clip())
            {
                // Table descendant with `overflow:hidden|clip` —
                // mirror the block-clip arm above so the table's clip
                // path is pushed inside the transform scope. Without
                // this, `<div style="transform:..."><table style=
                // "overflow:hidden">` paints cells under the transform
                // but loses the table boundary (fulgur-bvhw PR #320
                // Devin).
                draw_under_clip_table(
                    canvas,
                    desc_id,
                    desc_table,
                    desc_geom,
                    desc_frag,
                    desc_x,
                    desc_y,
                    geometry,
                    drawables,
                    margin_left_pt,
                    margin_top_pt,
                    page_index,
                );
            } else if let Some(desc_block) = drawables
                .block_styles
                .get(&desc_id)
                .filter(|b| !b.opacity_descendants.is_empty())
            {
                // Opacity-scoped descendant inside this transform.
                // Without this branch, transforms wrapping an opacity
                // block would dispatch the descendant's own paint via
                // `dispatch_fragment` but skip the opacity wrap of
                // the descendant's children, dropping the descendant
                // block's opacity from its sub-children.
                // (fulgur-gdb9)
                draw_under_opacity(
                    canvas,
                    desc_block,
                    desc_id,
                    desc_geom,
                    desc_frag,
                    desc_x,
                    desc_y,
                    geometry,
                    drawables,
                    margin_left_pt,
                    margin_top_pt,
                    page_index,
                );
            } else {
                dispatch_fragment(
                    canvas,
                    desc_id,
                    desc_geom,
                    desc_frag,
                    desc_x,
                    desc_y,
                    drawables,
                    geometry,
                    margin_left_pt,
                    margin_top_pt,
                    page_index,
                );
            }
        }
    }

    // F22: the collapsed borders of the tables in this scope, above their cells.
    paint_wrapper_tables(
        canvas,
        node_id,
        &tx.descendants,
        page_index,
        margin_left_pt,
        margin_top_pt,
        geometry,
        drawables,
    );

    // Paint multicol column rules for any multicol container in this
    // transform's direct scope. When the multicol container carries
    // a transform, the column rules must paint under the composed
    // matrix (CSS Transforms §11 stacking-context rules), so this
    // post-pass runs inside the `push_transform / pop` group instead
    // of in page coordinates.
    //
    // Direct scope = `tx.descendants` (or the transform key itself,
    // covered when `node_id` is also a multicol container) MINUS any
    // descendant that lives inside a NESTED transform — those are
    // painted by the inner `draw_under_transform` recursion to compose
    // both matrices. Without this filter, a multicol container nested
    // two transforms deep would paint its rules in the outer
    // transform's space, missing the inner matrix.
    // (PR #305 follow-up Devin)
    let nested_tx_desc: std::collections::BTreeSet<usize> = tx
        .descendants
        .iter()
        .filter_map(|id| drawables.transforms.get(id))
        .flat_map(|inner| inner.descendants.iter().copied())
        .collect();
    for (&container_id, entry) in &drawables.multicol_rules {
        let in_my_scope = container_id == node_id || tx.descendants.contains(&container_id);
        if !in_my_scope || nested_tx_desc.contains(&container_id) {
            continue;
        }
        let Some(container_geom) = geometry.get(&container_id) else {
            continue;
        };
        paint_multicol_rule_for_page(
            canvas,
            entry,
            container_geom,
            margin_left_pt,
            margin_top_pt,
            page_index,
        );
    }

    canvas.clip_band = saved_clip_band;
    canvas.paint_extent.leave_local();
    canvas.surface.pop();
    if let Some(lc) = canvas.link_collector.as_deref_mut() {
        lc.pop_transform();
    }
}

/// Push the block's overflow-clip path onto the surface, dispatch the
/// wrapper's own bg / border / shadow + every descendant fragment that
/// lands on `page_index`, then pop. Painting order per
/// CSS 2.1 §11.1.1 for overflow clipping:
///
/// ```text
/// // bg/border/shadow paint OUTSIDE the clip
/// draw_with_opacity(canvas, opacity, |c| {
///     bg + border + shadow at (x, y, total_w, total_h);
///     if let Some(clip) = compute_overflow_clip_path(...) {
///         c.surface.push_clip_path(&clip, FillRule::default());
///         for child in children { child.draw(c, x + child.x, y + child.y, ..); }
///         c.surface.pop();
///     }
/// });
/// ```
///
/// The wrapper's own inner content (paragraph / image / svg sharing
/// the same `node_id`) is dispatched as part of the clipped region,
/// then strict descendants iterate inside the clip. The block
/// dispatcher's "shared node_id" combined helper
/// (`draw_block_with_inner_content`) already handles the outer
/// opacity wrap when used; here we replicate that ordering manually
/// — bg/border outside clip, inner content + descendants inside clip,
/// all wrapped in one opacity group.
#[allow(clippy::too_many_arguments)]
fn draw_under_clip(
    canvas: &mut crate::draw_primitives::Canvas<'_, '_>,
    block: &crate::drawables::BlockEntry,
    node_id: usize,
    geom: &crate::pagination_layout::PaginationGeometry,
    frag: &crate::pagination_layout::Fragment,
    x_pt: f32,
    y_pt: f32,
    geometry: &crate::pagination_layout::PaginationGeometryTable,
    drawables: &Drawables,
    margin_left_pt: f32,
    margin_top_pt: f32,
    page_index: u32,
) {
    use crate::draw_primitives::draw_with_opacity;

    let total_width = block
        .layout_size
        .map(|s| s.width.to_f32())
        .unwrap_or_else(|| frag.width.in_pt().to_f32());
    // Mirror `draw_block_inner_paint` / `draw_under_opacity`'s
    // `is_split` height fix. When this `overflow: hidden | clip`
    // block spans multiple pages (one fragment per page slice), use
    // `frag.height` so each slice paints its per-page bg / border /
    // shadow at the slice height — and pushes a clip rectangle of
    // the slice height too. Without this the bg / border overflows
    // the page bottom on earlier slices, double-paints on
    // continuation pages, AND the clip rect on continuation pages
    // covers content that should be cut off.
    // (PR #313 follow-up Devin Review — completes the PR #316 fix.)
    let is_split = geom.is_split();
    let (frame_frag, edges) = frame_fragment(geom, frag);
    // F30f-d: the edges of a box sliced at its breaks are open on this page: no border, padding or radius there, and
    // the clip is the padding box of this fragment.
    let sliced = crate::draw_primitives::sliced_style(&block.style, edges);
    let style: &crate::draw_primitives::BlockStyle = &sliced;
    let total_height = block
        .layout_size
        .map(|s| {
            if is_split && frame_frag.height > crate::units::Px::ZERO {
                frame_frag.height.in_pt().to_f32()
            } else {
                s.height.to_f32()
            }
        })
        .unwrap_or_else(|| frame_frag.height.in_pt().to_f32());

    let para_for_block = drawables.paragraphs.get(&node_id);
    let img_for_block = drawables.images.get(&node_id);
    let svg_for_block = drawables.svgs.get(&node_id);
    let inner_inset = style.content_inset();

    // When this node is a list-item with overflow clip, honour the
    // list-item painting order: outer opacity uses `list_item.opacity`
    // (the body block inside is built with default opacity=1.0 in
    // `convert::list_item::build_list_item_body`, so `block.opacity`
    // here would silently drop CSS opacity), and the marker draws
    // before `push_clip_path` (markers sit at negative x outside the
    // body box, so they must not be clipped). Without this,
    // `<li style="overflow:hidden">` loses its marker entirely and
    // any opacity set on the `<li>` is ignored. (PR #310 Devin)
    let list_item = drawables.list_items.get(&node_id);
    let opacity = list_item.map_or(block.opacity, |li| li.opacity);

    draw_with_opacity(canvas, opacity, |canvas| {
        // List-item marker paints first, OUTSIDE the clip — the
        // marker is emitted before the body's bg / border / shadow.
        // Markers sit at negative x relative to (x_pt, y_pt), so they
        // must also stay outside the clip path pushed below.
        if let Some(li) = list_item
            && li.visible
        {
            draw_list_item_marker_tagged(canvas, li, node_id, drawables, x_pt, y_pt);
        }

        // bg / border / shadow outside the clip — same as
        // `draw_block_inner_paint` but inlined so the opacity wrap
        // covers the entire clipped region too.
        if block.visible {
            note_painted_box(canvas, style, x_pt, total_width);
            crate::background::draw_box_shadows(
                canvas,
                style,
                x_pt,
                y_pt,
                total_width,
                total_height,
            );
            crate::background::draw_background(
                canvas,
                style,
                x_pt,
                y_pt,
                total_width,
                total_height,
            );
            crate::draw_primitives::draw_block_border(
                canvas,
                style,
                x_pt,
                y_pt,
                total_width,
                total_height,
            );
        }

        // Push clip — fall through to inner content + descendants if
        // `compute_overflow_clip_path` returns `None` (style somehow
        // changed since extract decided this block clips).
        let clip_pushed = if let Some(clip_path) =
            crate::draw_primitives::compute_overflow_clip_path(
                style,
                x_pt,
                y_pt,
                total_width,
                total_height,
            ) {
            canvas
                .surface
                .push_clip_path(&clip_path, &krilla::paint::FillRule::default());
            canvas.paint_extent.enter_local();
            true
        } else {
            false
        };
        // F30f-b: a box that clips its block axis leaves only its padding box visible: no line box wholly outside it
        // is written (ADR-0009 of html-to-pdf). The band is in page coordinates; it is intersected with the one of the
        // clip boxes around this one and restored below.
        let outer_clip_band = canvas.clip_band;
        // A box whose block size the author does not constrain hides nothing: it gets no band.
        if clip_pushed
            && style.overflow_y == crate::draw_primitives::Overflow::Clip
            && style.block_size_constrained
        {
            let (top_border, bottom_border) = (
                style.border_widths[0].to_f32(),
                style.border_widths[2].to_f32(),
            );
            canvas.clip_band = outer_clip_band.intersect(
                (y_pt + top_border).as_pt(),
                (y_pt + total_height - bottom_border).as_pt(),
            );
        }

        // Inner content sharing `node_id` (inline-root paragraph,
        // replaced image / svg) paints at the content-box top-left,
        // not the border-box. Mirrors `draw_block_with_inner_content`.
        let inner_x = x_pt + inner_inset.0;
        let inner_y = y_pt + inner_inset.1;
        if let Some(p) = para_for_block {
            let run_tag_node_id = run_tag_target(drawables, node_id);
            let use_run_tagging = canvas.tag_collector.is_some()
                && para_has_link_runs(p)
                && run_tag_node_id.is_some();
            let tag_info = if use_run_tagging {
                canvas.link_run_node_id = run_tag_node_id;
                None
            } else {
                try_start_tagged(canvas, node_id, drawables)
            };
            draw_paragraph_inner_paint(
                canvas,
                p,
                inner_x,
                inner_y,
                &geom.fragments,
                &geom.line_slices,
                page_index,
                is_split,
                drawables,
                geometry,
                margin_left_pt,
                margin_top_pt,
            );
            finish_tagged(canvas, tag_info);
            if use_run_tagging {
                canvas.link_run_node_id = None;
            }
        }
        if let Some(i) = img_for_block {
            draw_image_inner_paint(canvas, i, inner_x, inner_y);
        }
        if let Some(s) = svg_for_block {
            draw_svg_inner_paint(canvas, s, inner_x, inner_y);
        }

        // Strict descendants — each at its own fragment's coords.
        //
        // Transform-aware dispatch: a descendant that has its own
        // `TransformEntry` must enter `draw_under_transform` so the
        // surface transform composes correctly. The main loop skips
        // these nodes via `clipped_descendants.contains(...)` BEFORE
        // it reaches the per-fragment transform check, so without the
        // recursion below v2 silently drops transforms inside an
        // `overflow: hidden` ancestor (`<div style="overflow:hidden">
        // <div style="transform:..."/></div>` — PR #310 Devin).
        //
        // Pre-skip the strict descendants of those transforms so they
        // are not dispatched twice — once via `draw_under_transform`
        // (which iterates `tx.descendants`) and once via the loop
        // body's `dispatch_fragment`.
        let transform_skip: std::collections::BTreeSet<usize> = block
            .clip_descendants
            .iter()
            .filter_map(|id| drawables.transforms.get(id))
            .flat_map(|tx| tx.descendants.iter().copied())
            .collect();
        // Symmetric pre-skip for nested `overflow:hidden` descendants.
        // The recursive `draw_under_clip` call below paints the inner
        // clip's children inside its push/pop group; iterating the
        // outer's `clip_descendants` for those same nodes here would
        // re-dispatch them outside the inner clip
        // (PR #309 follow-up Devin).
        let mut nested_clip_skip: std::collections::BTreeSet<usize> = block
            .clip_descendants
            .iter()
            .filter_map(|id| drawables.block_styles.get(id))
            .filter(|b| b.style.has_overflow_clip())
            .flat_map(|b| b.clip_descendants.iter().copied())
            .collect();
        // Tables with overflow-clip nested inside this block's clip
        // scope carry their own `clip_descendants` — the recursive
        // `draw_under_clip_table` arm below paints them inside the
        // table's clip path, so skip them here too (fulgur-bvhw PR
        // #320 Devin).
        nested_clip_skip.extend(
            block
                .clip_descendants
                .iter()
                .filter_map(|id| drawables.tables.get(id))
                .filter(|t| t.style.has_overflow_clip())
                .flat_map(|t| t.clip_descendants.iter().copied()),
        );
        // Symmetric pre-skip for opacity-scoped descendants nested
        // inside this clip. Mirrors `nested_clip_skip` — without it,
        // an opacity descendant's sub-children would be dispatched by
        // the loop AND by `draw_under_opacity` below, double-painting
        // them. (fulgur-gdb9)
        let nested_opacity_skip: std::collections::BTreeSet<usize> = block
            .clip_descendants
            .iter()
            .filter_map(|id| drawables.block_styles.get(id))
            .flat_map(|b| b.opacity_descendants.iter().copied())
            .collect();
        // Inline boxes carried by paragraphs inside this clip are painted
        // by those paragraphs' `LineItem::InlineBox` arms. Re-dispatching
        // them here paints them twice — and because the duplicate re-enters
        // `draw_under_clip` for a nested clipped inline-block, the cost
        // doubles per nesting level rather than staying constant.
        let paragraph_ib_skip = paragraph_owned_inline_boxes(
            std::iter::once(node_id).chain(block.clip_descendants.iter().copied()),
            drawables,
        );
        for &desc_id in &block.clip_descendants {
            if transform_skip.contains(&desc_id)
                || nested_clip_skip.contains(&desc_id)
                || nested_opacity_skip.contains(&desc_id)
            {
                continue;
            }
            if paragraph_ib_skip.contains(&desc_id) {
                // F30f: a box fragmented over several pages is painted by its carrier on the first of them only.
                paint_inline_box_continuation(
                    canvas,
                    desc_id,
                    page_index,
                    geometry,
                    drawables,
                    margin_left_pt,
                    margin_top_pt,
                );
                continue;
            }
            let Some(desc_geom) = geometry.get(&desc_id) else {
                continue;
            };
            for desc_frag in &desc_geom.fragments {
                if desc_frag.page_index != page_index {
                    continue;
                }
                let desc_x = margin_left_pt + desc_frag.x.in_pt().to_f32();
                let desc_y = margin_top_pt + desc_frag.y.in_pt().to_f32();
                if let Some(desc_tx) = drawables.transforms.get(&desc_id) {
                    draw_under_transform(
                        canvas,
                        desc_tx,
                        desc_id,
                        desc_geom,
                        desc_frag,
                        desc_x,
                        desc_y,
                        geometry,
                        drawables,
                        margin_left_pt,
                        margin_top_pt,
                        page_index,
                    );
                } else if let Some(desc_block) = drawables
                    .block_styles
                    .get(&desc_id)
                    .filter(|b| b.style.has_overflow_clip())
                    .filter(|_| Some(desc_id) != drawables.body_id)
                {
                    // Nested `overflow:hidden|clip` block — recurse so
                    // its own clip path is pushed. Without this, a
                    // `<div style="overflow:hidden"><div style="
                    // overflow:hidden;width:30px"><p>text</p></div>
                    // </div>` paints the inner block's bg/border via
                    // `dispatch_fragment` but never pushes the inner
                    // clip, losing the inner boundary
                    // (PR #309 follow-up Devin).
                    draw_under_clip(
                        canvas,
                        desc_block,
                        desc_id,
                        desc_geom,
                        desc_frag,
                        desc_x,
                        desc_y,
                        geometry,
                        drawables,
                        margin_left_pt,
                        margin_top_pt,
                        page_index,
                    );
                } else if let Some(desc_table) = drawables
                    .tables
                    .get(&desc_id)
                    .filter(|t| t.style.has_overflow_clip())
                {
                    // Nested table with overflow-clip — recurse into
                    // `draw_under_clip_table` so the table boundary
                    // is pushed inside this block's clip scope.
                    // (fulgur-bvhw PR #320 Devin)
                    draw_under_clip_table(
                        canvas,
                        desc_id,
                        desc_table,
                        desc_geom,
                        desc_frag,
                        desc_x,
                        desc_y,
                        geometry,
                        drawables,
                        margin_left_pt,
                        margin_top_pt,
                        page_index,
                    );
                } else if let Some(desc_block) = drawables
                    .block_styles
                    .get(&desc_id)
                    .filter(|b| !b.opacity_descendants.is_empty())
                {
                    // Nested opacity-scoped block — recurse so its
                    // descendants paint inside its `draw_with_opacity`
                    // wrap. (fulgur-gdb9)
                    draw_under_opacity(
                        canvas,
                        desc_block,
                        desc_id,
                        desc_geom,
                        desc_frag,
                        desc_x,
                        desc_y,
                        geometry,
                        drawables,
                        margin_left_pt,
                        margin_top_pt,
                        page_index,
                    );
                } else {
                    dispatch_fragment(
                        canvas,
                        desc_id,
                        desc_geom,
                        desc_frag,
                        desc_x,
                        desc_y,
                        drawables,
                        geometry,
                        margin_left_pt,
                        margin_top_pt,
                        page_index,
                    );
                }
            }
        }

        // F22: the collapsed borders of the tables in this scope, above their cells.
        paint_wrapper_tables(
            canvas,
            node_id,
            &block.clip_descendants,
            page_index,
            margin_left_pt,
            margin_top_pt,
            geometry,
            drawables,
        );

        canvas.clip_band = outer_clip_band;
        if clip_pushed {
            canvas.paint_extent.leave_local();
            canvas.surface.pop();
        }
    });
}

/// Wrap the block's `dispatch_fragment` + every strict descendant in a
/// single `draw_with_opacity` group. Used for blocks that have
/// fractional opacity but no overflow clip (the clip arm,
/// `draw_under_clip`, already handles its own opacity wrap).
///
/// The whole subtree must live in a single opacity transparency group
/// (CSS opacity §2):
///
/// ```text
/// draw_with_opacity(canvas, self.opacity, |c| {
///     bg + border + shadow at (x, y, total_w, total_h);
///     for pc in self.children { pc.child.draw(c, x + pc.x, y + pc.y, ..); }
/// });
/// ```
///
/// A single transparency-group XObject covers the whole subtree; a
/// flat dispatch without scope tracking would emit the block's own
/// paint inside opacity but every descendant outside, dropping the
/// parent's opacity on those descendants. Mirrors `draw_under_clip`
/// minus the `push_clip_path` / `pop` calls and the list-item marker
/// arm (a list-item with opacity uses `draw_list_item_with_block`,
/// not this path, since list-item markers are owned by `ListItemEntry`
/// rather than `BlockEntry`).
#[allow(clippy::too_many_arguments)]
fn draw_under_opacity(
    canvas: &mut crate::draw_primitives::Canvas<'_, '_>,
    block: &crate::drawables::BlockEntry,
    node_id: usize,
    geom: &crate::pagination_layout::PaginationGeometry,
    frag: &crate::pagination_layout::Fragment,
    x_pt: f32,
    y_pt: f32,
    geometry: &crate::pagination_layout::PaginationGeometryTable,
    drawables: &Drawables,
    margin_left_pt: f32,
    margin_top_pt: f32,
    page_index: u32,
) {
    use crate::draw_primitives::draw_with_opacity;

    draw_with_opacity(canvas, block.opacity, |canvas| {
        // Block's own paint + shared-node_id inner content. We can
        // re-use `dispatch_fragment` because it already handles the
        // shared-node_id case via `draw_block_with_inner_content`,
        // which itself opens a `draw_with_opacity(block.opacity, ..)`
        // wrap. That nested wrap is harmless: krilla composes nested
        // opacity groups by multiplication (0.4 × 1.0 = 0.4, since the
        // inner block-with-inner-content uses the SAME opacity),
        // matching the v1 chain `draw_with_opacity(0.4) → child draws`
        // where the child happens to also be a block at full opacity.
        //
        // For pure-block-with-descendants (the case we're fixing —
        // `<div opacity:0.4><svg>..</svg></div>`), `dispatch_fragment`
        // calls `draw_block_v2` which wraps own paint in
        // `draw_with_opacity(0.4)`. The outer wrap here multiplies to
        // 0.16 — wrong! Inline the block's own paint without the inner
        // opacity wrap to avoid this.
        if drawables.list_items.contains_key(&node_id) {
            // Inside an opacity-scoped block path we never reach a
            // list-item: list-items dispatch through their own
            // `draw_list_item_with_block` which composes the marker +
            // body block + paragraph in one opacity group. If a
            // list-item carries opacity, it would not have entered
            // `draw_under_opacity` because list-item's opacity comes
            // from `ListItemEntry`, not `BlockEntry`. Defensive guard
            // only — should be unreachable.
            dispatch_fragment(
                canvas,
                node_id,
                geom,
                frag,
                x_pt,
                y_pt,
                drawables,
                geometry,
                margin_left_pt,
                margin_top_pt,
                page_index,
            );
        } else {
            // Block bg / border / shadow without the inner opacity
            // wrap. The shared-node_id (`paragraph` / `image` / `svg`
            // at the same node_id) inner content paints at the
            // content-box top-left; mirrors
            // `draw_block_with_inner_content`'s body but without its
            // own `draw_with_opacity` since the outer wrap already
            // covers it.
            let para_for_block = drawables.paragraphs.get(&node_id);
            let img_for_block = drawables.images.get(&node_id);
            let svg_for_block = drawables.svgs.get(&node_id);
            let total_width = block
                .layout_size
                .map(|s| s.width.to_f32())
                .unwrap_or_else(|| frag.width.in_pt().to_f32());
            // Mirror `draw_block_inner_paint`'s `is_split` height fix.
            // When this opacity-scoped block spans multiple pages
            // (one fragment per page slice), use `frag.height` so each
            // slice paints its per-page bg / border height instead of
            // the full layout height — without this the bg / border
            // overflows the page bottom on earlier slices and double-
            // paints on continuation pages, exactly the bug the
            // `draw_block_inner_paint` fix addresses for non-opacity
            // blocks (PR #316). (PR #314 follow-up Devin Review)
            let is_split = geom.is_split();
            let (frame_frag, edges) = frame_fragment(geom, frag);
            // F30f-d: open edges of a box sliced at its breaks (see `draw_under_clip`).
            let sliced = crate::draw_primitives::sliced_style(&block.style, edges);
            let style: &crate::draw_primitives::BlockStyle = &sliced;
            let total_height = block
                .layout_size
                .map(|s| {
                    if is_split && frame_frag.height > crate::units::Px::ZERO {
                        frame_frag.height.in_pt().to_f32()
                    } else {
                        s.height.to_f32()
                    }
                })
                .unwrap_or_else(|| frame_frag.height.in_pt().to_f32());
            if block.visible {
                crate::background::draw_box_shadows(
                    canvas,
                    style,
                    x_pt,
                    y_pt,
                    total_width,
                    total_height,
                );
                crate::background::draw_background(
                    canvas,
                    style,
                    x_pt,
                    y_pt,
                    total_width,
                    total_height,
                );
                crate::draw_primitives::draw_block_border(
                    canvas,
                    style,
                    x_pt,
                    y_pt,
                    total_width,
                    total_height,
                );
            }
            let inner_inset = style.content_inset();
            let inner_x = x_pt + inner_inset.0;
            let inner_y = y_pt + inner_inset.1;
            if let Some(p) = para_for_block {
                let run_tag_node_id = run_tag_target(drawables, node_id);
                let use_run_tagging = canvas.tag_collector.is_some()
                    && para_has_link_runs(p)
                    && run_tag_node_id.is_some();
                let tag_info = if use_run_tagging {
                    canvas.link_run_node_id = run_tag_node_id;
                    None
                } else {
                    try_start_tagged(canvas, node_id, drawables)
                };
                draw_paragraph_inner_paint(
                    canvas,
                    p,
                    inner_x,
                    inner_y,
                    &geom.fragments,
                    &geom.line_slices,
                    page_index,
                    is_split,
                    drawables,
                    geometry,
                    margin_left_pt,
                    margin_top_pt,
                );
                finish_tagged(canvas, tag_info);
                if use_run_tagging {
                    canvas.link_run_node_id = None;
                }
            }
            if let Some(i) = img_for_block {
                draw_image_inner_paint(canvas, i, inner_x, inner_y);
            }
            if let Some(s) = svg_for_block {
                draw_svg_inner_paint(canvas, s, inner_x, inner_y);
            }
        }

        // Descendants — same dispatch tree as `draw_under_clip` minus
        // the nested-clip recursion (an opacity-scoped block by
        // construction has `clipping == false`, so its descendants
        // can still individually have clip / transform / opacity, and
        // those need their own scope helpers).
        let transform_skip: std::collections::BTreeSet<usize> = block
            .opacity_descendants
            .iter()
            .filter_map(|id| drawables.transforms.get(id))
            .flat_map(|tx| tx.descendants.iter().copied())
            .collect();
        let mut nested_clip_skip: std::collections::BTreeSet<usize> = block
            .opacity_descendants
            .iter()
            .filter_map(|id| drawables.block_styles.get(id))
            .filter(|b| b.style.has_overflow_clip())
            .flat_map(|b| b.clip_descendants.iter().copied())
            .collect();
        // Tables with overflow-clip nested inside this opacity scope
        // recurse into `draw_under_clip_table`; pre-skip their cell
        // descendants here so they don't double-paint outside the
        // table's clip path (fulgur-bvhw PR #320 Devin).
        nested_clip_skip.extend(
            block
                .opacity_descendants
                .iter()
                .filter_map(|id| drawables.tables.get(id))
                .filter(|t| t.style.has_overflow_clip())
                .flat_map(|t| t.clip_descendants.iter().copied()),
        );
        let nested_opacity_skip: std::collections::BTreeSet<usize> = block
            .opacity_descendants
            .iter()
            .filter_map(|id| drawables.block_styles.get(id))
            .flat_map(|b| b.opacity_descendants.iter().copied())
            .collect();
        // Same narrowing as `draw_under_clip`: paragraphs inside this
        // opacity group paint their own inline boxes, so the walk must not
        // paint them again. Unlike the clip and transform paths this
        // duplicate does not compound — `draw_under_opacity` has no
        // self-recursive arm — but it is still a double paint.
        let paragraph_ib_skip = paragraph_owned_inline_boxes(
            std::iter::once(node_id).chain(block.opacity_descendants.iter().copied()),
            drawables,
        );
        for &desc_id in &block.opacity_descendants {
            if transform_skip.contains(&desc_id)
                || nested_clip_skip.contains(&desc_id)
                || nested_opacity_skip.contains(&desc_id)
            {
                continue;
            }
            if paragraph_ib_skip.contains(&desc_id) {
                // F30f: as in `draw_under_clip`.
                paint_inline_box_continuation(
                    canvas,
                    desc_id,
                    page_index,
                    geometry,
                    drawables,
                    margin_left_pt,
                    margin_top_pt,
                );
                continue;
            }
            let Some(desc_geom) = geometry.get(&desc_id) else {
                continue;
            };
            for desc_frag in &desc_geom.fragments {
                if desc_frag.page_index != page_index {
                    continue;
                }
                let desc_x = margin_left_pt + desc_frag.x.in_pt().to_f32();
                let desc_y = margin_top_pt + desc_frag.y.in_pt().to_f32();
                if let Some(desc_tx) = drawables.transforms.get(&desc_id) {
                    draw_under_transform(
                        canvas,
                        desc_tx,
                        desc_id,
                        desc_geom,
                        desc_frag,
                        desc_x,
                        desc_y,
                        geometry,
                        drawables,
                        margin_left_pt,
                        margin_top_pt,
                        page_index,
                    );
                } else if let Some(desc_block) = drawables
                    .block_styles
                    .get(&desc_id)
                    .filter(|b| b.style.has_overflow_clip())
                    .filter(|_| Some(desc_id) != drawables.body_id)
                {
                    draw_under_clip(
                        canvas,
                        desc_block,
                        desc_id,
                        desc_geom,
                        desc_frag,
                        desc_x,
                        desc_y,
                        geometry,
                        drawables,
                        margin_left_pt,
                        margin_top_pt,
                        page_index,
                    );
                } else if let Some(desc_table) = drawables
                    .tables
                    .get(&desc_id)
                    .filter(|t| t.style.has_overflow_clip())
                {
                    // Table with overflow-clip nested inside this
                    // opacity scope. (fulgur-bvhw PR #320 Devin)
                    draw_under_clip_table(
                        canvas,
                        desc_id,
                        desc_table,
                        desc_geom,
                        desc_frag,
                        desc_x,
                        desc_y,
                        geometry,
                        drawables,
                        margin_left_pt,
                        margin_top_pt,
                        page_index,
                    );
                } else if let Some(desc_block) = drawables
                    .block_styles
                    .get(&desc_id)
                    .filter(|b| !b.opacity_descendants.is_empty())
                {
                    draw_under_opacity(
                        canvas,
                        desc_block,
                        desc_id,
                        desc_geom,
                        desc_frag,
                        desc_x,
                        desc_y,
                        geometry,
                        drawables,
                        margin_left_pt,
                        margin_top_pt,
                        page_index,
                    );
                } else {
                    dispatch_fragment(
                        canvas,
                        desc_id,
                        desc_geom,
                        desc_frag,
                        desc_x,
                        desc_y,
                        drawables,
                        geometry,
                        margin_left_pt,
                        margin_top_pt,
                        page_index,
                    );
                }
            }
        }
        // F22: the collapsed borders of the tables in this scope, above their cells.
        paint_wrapper_tables(
            canvas,
            node_id,
            &block.opacity_descendants,
            page_index,
            margin_left_pt,
            margin_top_pt,
            geometry,
            drawables,
        );
    });
}

/// F22: whether the table `table` is painted by a wrapper function of its own (a transform, or an overflow clip) and so
/// not by the page or by the wrapper around it.
fn is_wrapper_key(drawables: &Drawables, table: usize) -> bool {
    drawables.transforms.contains_key(&table)
        || drawables
            .tables
            .get(&table)
            .is_some_and(|t| t.style.has_overflow_clip())
}

/// F22: paints the collapsed borders of the tables in the scope of a wrapper (the wrapper node itself, when it is a
/// table, and its `descendants`), after the wrapper's descendants were dispatched, inside the wrapper's transform, clip
/// or opacity group. A table in a wrapper nested in this one is left to that one.
#[allow(clippy::too_many_arguments)]
fn paint_wrapper_tables(
    canvas: &mut crate::draw_primitives::Canvas<'_, '_>,
    node_id: usize,
    descendants: &[usize],
    page_index: u32,
    margin_left_pt: f32,
    margin_top_pt: f32,
    geometry: &crate::pagination_layout::PaginationGeometryTable,
    drawables: &Drawables,
) {
    if drawables.table_collapsed_borders.is_empty() {
        return;
    }
    let nested: std::collections::BTreeSet<usize> = descendants
        .iter()
        .flat_map(|id| {
            let mut inner: Vec<usize> = Vec::new();
            if let Some(tx) = drawables.transforms.get(id) {
                inner.extend(&tx.descendants);
            }
            if let Some(block) = drawables.block_styles.get(id) {
                if block.style.has_overflow_clip() {
                    inner.extend(&block.clip_descendants);
                }
                inner.extend(&block.opacity_descendants);
            }
            if let Some(table) = drawables.tables.get(id)
                && table.style.has_overflow_clip()
            {
                inner.extend(&table.clip_descendants);
            }
            inner
        })
        .collect();
    let tables = std::iter::once(node_id)
        .chain(
            descendants
                .iter()
                .copied()
                .filter(|id| !is_wrapper_key(drawables, *id)),
        )
        .filter(|id| drawables.table_collapsed_borders.contains_key(id))
        .filter(|id| *id == node_id || !nested.contains(id))
        .collect::<Vec<usize>>();
    crate::table_borders::paint_tables(
        canvas,
        tables.into_iter(),
        page_index,
        margin_left_pt,
        margin_top_pt,
        geometry,
        drawables,
    );
}

/// Paint multicol column-rule lines on `page_index` for one
/// `MulticolRuleEntry`. Partitions `entry.groups` by accumulating the
/// container's per-page heights so each page only emits the rule
/// segments that fit on it.
fn paint_multicol_rule_for_page(
    canvas: &mut crate::draw_primitives::Canvas<'_, '_>,
    entry: &crate::drawables::MulticolRuleEntry,
    container_geom: &crate::pagination_layout::PaginationGeometry,
    margin_left_pt: f32,
    margin_top_pt: f32,
    page_index: u32,
) {
    use crate::draw_primitives::stroke_line;
    use crate::units::{F32Units, Pt};

    let Some(stroke) = build_multicol_stroke(&entry.rule) else {
        return;
    };

    let target_pos = container_geom
        .fragments
        .iter()
        .position(|f| f.page_index == page_index);
    let Some(target_pos) = target_pos else {
        return;
    };
    let target_frag = &container_geom.fragments[target_pos];

    let consumed = container_geom.fragments[..target_pos]
        .iter()
        .map(|f| f.height)
        .sum::<crate::units::Px>()
        .in_pt();
    let cutoff = target_frag.height.in_pt();

    let x_base = margin_left_pt.as_pt() + target_frag.x.in_pt();
    let y_base = margin_top_pt.as_pt() + target_frag.y.in_pt();

    for group in &entry.groups {
        if group.n < 2 || group.col_heights.len() != group.n as usize {
            continue;
        }
        let group_top = group.y_offset - consumed;
        let max_h = group.col_heights.iter().copied().fold(Pt::ZERO, Pt::max);
        let group_bottom = group_top + max_h;
        if group_bottom <= Pt::ZERO || group_top >= cutoff {
            continue;
        }
        let visible_top = group_top.max(Pt::ZERO);
        let y_top = y_base + visible_top;
        // Subtract the portion of each column already painted on
        // prior pages BEFORE clamping to the visible strip on this
        // page. Without this, a column rule segment whose group
        // straddles a page boundary extends past the actual visible
        // column content.
        let consumed_above = (visible_top - group_top).max(Pt::ZERO);
        let visible_h = (group_bottom.min(cutoff) - visible_top).max(Pt::ZERO);
        for i in 0..(group.n as usize - 1) {
            let h_left = (group.col_heights[i] - consumed_above)
                .max(Pt::ZERO)
                .min(visible_h);
            let h_right = (group.col_heights[i + 1] - consumed_above)
                .max(Pt::ZERO)
                .min(visible_h);
            if h_left <= Pt::ZERO || h_right <= Pt::ZERO {
                continue;
            }
            let rule_x = x_base
                + group.x_offset
                + (i as f32 + 1.0) * group.col_w
                + i as f32 * group.gap
                + group.gap / 2.0;
            let y_bot = y_top + h_left.min(h_right);
            stroke_line(
                canvas,
                rule_x.to_f32(),
                y_top.to_f32(),
                rule_x.to_f32(),
                y_bot.to_f32(),
                stroke.clone(),
            );
        }
    }
    canvas.surface.set_stroke(None);
}

/// Build the krilla stroke for the configured column-rule spec.
/// Returns `None` when the rule is invisible (style `None` or
/// non-positive width).
fn build_multicol_stroke(
    rule: &crate::column_css::ColumnRuleSpec,
) -> Option<krilla::paint::Stroke> {
    use crate::column_css::ColumnRuleStyle;
    use crate::draw_primitives::{alpha_to_opacity, colored_stroke};

    if rule.width <= crate::units::Pt::ZERO || rule.style == ColumnRuleStyle::None {
        return None;
    }
    let opacity = alpha_to_opacity(rule.color[3]);
    let base = colored_stroke(&rule.color, rule.width.to_f32(), opacity);
    let w = rule.width.to_f32();
    let stroke = match rule.style {
        ColumnRuleStyle::None => return None,
        ColumnRuleStyle::Solid => base,
        ColumnRuleStyle::Dashed => krilla::paint::Stroke {
            dash: Some(krilla::paint::StrokeDash {
                array: vec![w * 3.0, w * 2.0],
                offset: 0.0,
            }),
            ..base
        },
        ColumnRuleStyle::Dotted => krilla::paint::Stroke {
            line_cap: krilla::paint::LineCap::Round,
            dash: Some(krilla::paint::StrokeDash {
                array: vec![0.0, w * 2.0],
                offset: 0.0,
            }),
            ..base
        },
    };
    Some(stroke)
}

/// v2 image draw. Mirrors `image::ImageRender::draw` but operates on
/// the side-channel `ImageEntry` data; the `width`/`height` are the
/// CSS-resolved size in pt that fulgur stores on the original
/// `ImageRender`.
fn draw_image_v2(
    canvas: &mut crate::draw_primitives::Canvas<'_, '_>,
    entry: &crate::drawables::ImageEntry,
    x: f32,
    y: f32,
) {
    use crate::draw_primitives::draw_with_opacity;
    draw_with_opacity(canvas, entry.opacity, |canvas| {
        draw_image_inner_paint(canvas, entry, x, y);
    });
}

/// Image paint without `draw_with_opacity` wrapper. Used by
/// `draw_block_with_inner_content` so a `<img>` whose wrapping
/// inline-root block shares its node_id (`convert::replaced`)
/// composes with the block bg/border under one opacity group
/// (CSS opacity §2 — single transparency group for the combined
/// paint).
fn draw_image_inner_paint(
    canvas: &mut crate::draw_primitives::Canvas<'_, '_>,
    entry: &crate::drawables::ImageEntry,
    x: f32,
    y: f32,
) {
    if !entry.visible {
        return;
    }
    let Some(image) = decode_image_for_v2(&canvas.images, entry) else {
        return;
    };
    let Some(size) = krilla::geom::Size::from_wh(entry.width.to_f32(), entry.height.to_f32())
    else {
        return;
    };
    let transform = krilla::geom::Transform::from_translate(x, y);
    canvas.surface.push_transform(&transform);
    canvas.surface.draw_image(image, size);
    canvas.surface.pop();
}

fn decode_image_for_v2(
    cache: &crate::image::ImageCache,
    entry: &crate::drawables::ImageEntry,
) -> Option<krilla::image::Image> {
    crate::image::krilla_image(cache, &entry.image_data, entry.format)
}

/// v2 SVG draw. Mirrors `svg::SvgRender::draw`.
fn draw_svg_v2(
    canvas: &mut crate::draw_primitives::Canvas<'_, '_>,
    entry: &crate::drawables::SvgEntry,
    x: f32,
    y: f32,
) {
    use crate::draw_primitives::draw_with_opacity;
    draw_with_opacity(canvas, entry.opacity, |canvas| {
        draw_svg_inner_paint(canvas, entry, x, y);
    });
}

/// SVG paint without `draw_with_opacity` wrapper. See
/// `draw_image_inner_paint` for the rationale (inline-root `<svg>`
/// shares node_id with the wrapping block).
fn draw_svg_inner_paint(
    canvas: &mut crate::draw_primitives::Canvas<'_, '_>,
    entry: &crate::drawables::SvgEntry,
    x: f32,
    y: f32,
) {
    use krilla_svg::{SurfaceExt, SvgSettings};
    if !entry.visible {
        return;
    }
    let Some(size) = krilla::geom::Size::from_wh(entry.width.to_f32(), entry.height.to_f32())
    else {
        return;
    };
    let transform = krilla::geom::Transform::from_translate(x, y);
    canvas.surface.push_transform(&transform);
    let _ = canvas
        .surface
        .draw_svg(&entry.tree, size, SvgSettings::default());
    canvas.surface.pop();
}

/// html-to-pdf F198: whether `frag` is the last fragment of `geom` on its page. Always true for a box that was not cut
/// into column pieces ([`PaginationGeometry::piece_edges`]): it has one fragment on a page. The slices of a paragraph
/// that is a box cut over columns are painted once per page, after the frames of all its pieces on that page (the
/// background of a later piece would cover the text of the columns before it).
fn is_last_piece_on_page(
    geom: &crate::pagination_layout::PaginationGeometry,
    frag: &crate::pagination_layout::Fragment,
) -> bool {
    geom.piece_edges.is_empty()
        || geom
            .fragments
            .iter()
            .rfind(|f| f.page_index == frag.page_index)
            .is_none_or(|last| std::ptr::eq(last, frag) || last == frag)
}

/// F30: the fragment whose height is the height of the box's frame on this
/// page. For a paragraph that `fragment_nested_inline_root` split between line
/// boxes, every fragment records the span of its lines only; the frame also
/// covers the border and padding above them (reserved on every page, because the
/// lines are painted at `y + inset_top`) and, on the last page, below them.
/// Everything else gets `frag` back unchanged.
///
/// html-to-pdf F30f-d: a box that is sliced at its breaks (`geom.line_split_slice`, css-break-3 §5.4) has the top
/// inset on its first fragment only and the bottom one on its last; a fragment that is not the last was extended to
/// the page bottom by the fragmenter. The second value says which edges of the frame are open (no border, padding or
/// radius there: [`crate::draw_primitives::sliced_style`], which also takes the top inset off the lines of a continuation
/// page).
pub(crate) fn frame_fragment(
    geom: &crate::pagination_layout::PaginationGeometry,
    frag: &crate::pagination_layout::Fragment,
) -> (
    crate::pagination_layout::Fragment,
    crate::draw_primitives::FrameEdges,
) {
    use crate::draw_primitives::FrameEdges;
    // html-to-pdf F198: a box a multicol container cut between its lines has one fragment per column piece, each its
    // own closed or open frame (the plan knows which). Two pieces share a page, so the page cannot say.
    if geom.piece_edges.len() == geom.fragments.len()
        && !geom.piece_edges.is_empty()
        && let Some(i) = geom
            .fragments
            .iter()
            .position(|f| std::ptr::eq(f, frag))
            .or_else(|| geom.fragments.iter().position(|f| f == frag))
    {
        return (frag.clone(), geom.piece_edges[i]);
    }
    let mut frame = frag.clone();
    let mut edges = FrameEdges::default();
    if let Some((top, bottom)) = geom.line_split_insets
        && geom.is_split()
    {
        let is_last = geom
            .fragments
            .last()
            .is_some_and(|last| last.page_index == frag.page_index);
        let is_first = geom
            .fragments
            .first()
            .is_some_and(|first| first.page_index == frag.page_index);
        // A sliced box has the top inset on its first fragment only (the bottom one is on the last either way).
        let top = if geom.line_split_slice {
            edges = FrameEdges {
                top_open: !is_first,
                bottom_open: !is_last,
            };
            if is_first {
                top
            } else {
                crate::units::Px::ZERO
            }
        } else {
            top
        };
        frame.height = top
            + frag.height
            + if is_last {
                bottom
            } else {
                crate::units::Px::ZERO
            };
    }
    (frame, edges)
}

/// Block draw: emits background / border / box-shadow for the
/// wrapper. Children paint themselves via their own per-NodeId
/// dispatch in `draw_v2_page`, so this fn does **not** recurse into
/// block children.
///
/// Overflow clip (`overflow: hidden`) is intentionally not pushed
/// here — the clip scope is owned by `draw_under_clip`, which paints
/// bg/border/shadow outside the clip and pushes the clip path around
/// the descendant dispatch (see that function for the full painting
/// order).
fn draw_block_v2(
    canvas: &mut crate::draw_primitives::Canvas<'_, '_>,
    entry: &crate::drawables::BlockEntry,
    x: f32,
    y: f32,
    frag: &crate::pagination_layout::Fragment,
    is_split: bool,
) {
    use crate::draw_primitives::draw_with_opacity;

    draw_with_opacity(canvas, entry.opacity, |canvas| {
        draw_block_inner_paint(canvas, entry, x, y, frag, is_split);
    });
}

/// Root-element (`<html>`) background pre-pass. The fragmenter's
/// `geometry` table only carries body + descendants, so the standard
/// per-(node_id, fragment) dispatch never visits html. Paint bg /
/// border / shadow at `(margin.left, margin.top)` using html's own
/// `layout_size` (which equals body's outer height including
/// collapsed margins). Root `<html>` / `<body>` backgrounds repeat
/// per page (CSS Paged Media §5.4), so this runs once per page.
///
/// Called from `render_v2`; intentionally bypasses the
/// `body_offset_pt.y` adjustment that the main dispatch loop applies,
/// because html's bg paints at the page's margin top, not at body's
/// content origin.
///
/// `paint_background` is false when the box's background is the canvas background of the page
/// ([`canvas_background_source`]); shadows and borders are still the box's own.
fn paint_root_block_v2(
    canvas: &mut crate::draw_primitives::Canvas<'_, '_>,
    entry: &crate::drawables::BlockEntry,
    margin_left_pt: f32,
    margin_top_pt: f32,
    height_override: Option<f32>,
    paint_background: bool,
) {
    use crate::draw_primitives::draw_with_opacity;
    let Some(size) = entry.layout_size else {
        return;
    };

    let h = height_override.unwrap_or(size.height.to_f32());
    let w = size.width.to_f32();

    draw_with_opacity(canvas, entry.opacity, |canvas| {
        if entry.visible {
            crate::background::draw_box_shadows(
                canvas,
                &entry.style,
                margin_left_pt,
                margin_top_pt,
                w,
                h,
            );
            if paint_background {
                crate::background::draw_background(
                    canvas,
                    &entry.style,
                    margin_left_pt,
                    margin_top_pt,
                    w,
                    h,
                );
            }
            crate::draw_primitives::draw_block_border(
                canvas,
                &entry.style,
                margin_left_pt,
                margin_top_pt,
                w,
                h,
            );
        }
    });
}

/// F37: the slice of `<body>`'s box (`PaginationGeometry::box_slices`) that is on `page`, with the height of the slices
/// before it and of all of them, in px. `None` when body's box has no slice on this page.
fn slice_on_page(
    slices: &[crate::pagination_layout::Fragment],
    page: u32,
) -> Option<(
    crate::units::Px,
    crate::units::Px,
    &crate::pagination_layout::Fragment,
)> {
    let slice = slices.iter().find(|f| f.page_index == page)?;
    let before = slices
        .iter()
        .filter(|f| f.page_index < page)
        .map(|f| f.height)
        .sum();
    let total = slices.iter().map(|f| f.height).sum();
    Some((before, total, slice))
}

/// Whether the part of `<body>`'s box on one page is just a flat rectangle of its own height: no image or gradient layer
/// (those are one image over the stitched box), no border, no shadow, and no radius under a colour. `paint_background`
/// is false when the background is the canvas background (F184), which paints none on the box.
fn is_flat_slice(style: &crate::draw_primitives::BlockStyle, paint_background: bool) -> bool {
    let layers = paint_background && !style.background_layers.is_empty();
    let rounded_colour = paint_background && style.background_color.is_some() && style.has_radius();
    let border = style
        .border_widths
        .iter()
        .zip(&style.border_styles)
        .any(|(w, s)| {
            *w > crate::units::Pt::ZERO && *s != crate::draw_primitives::BorderStyleValue::None
        });
    !layers && !rounded_colour && !border && style.box_shadows.is_empty()
}

/// F37: paint `<body>`'s own background, border and shadow for `page` (css-break-3 section 5.4, `slice`): the unbroken
/// box is drawn at the position it has when its earlier slices are stacked above this page's, with the height of all
/// slices, and cut to the y span of this slice, so that a background image or gradient is one image over the stitched
/// box and the border, radius and padding appear only at the real start and end. Where the box really starts (the
/// first slice) and ends (the last) the cut opens to the page edge, so a shadow is not clipped there.
///
/// A single slice, and a flat one ([`is_flat_slice`]: a plain colour is the same on every page), is drawn as the
/// rectangle it is, without the cut: the common card-body case adds nothing to the page.
///
/// `top_pt` is where this page's slice begins (body's top on page 0, the page-area top after); `x_pt` is body's left.
/// Nothing is drawn on a page without a slice.
#[allow(clippy::too_many_arguments)]
fn paint_body_box_v2(
    canvas: &mut crate::draw_primitives::Canvas<'_, '_>,
    entry: &crate::drawables::BlockEntry,
    slices: &[crate::pagination_layout::Fragment],
    page: u32,
    x_pt: f32,
    top_pt: f32,
    (page_width_pt, page_height_pt): (f32, f32),
    paint_background: bool,
) {
    let Some((before, total, slice)) = slice_on_page(slices, page) else {
        return;
    };
    let here_pt = slice.height.in_pt().to_f32();
    let total_pt = total.in_pt().to_f32();
    // A box taller than its slices (`min-height: 100vh`, a fixed `height`) goes on past the last page that holds
    // content (F37c): its last slice is a cut, not its end, so the bottom border and radius stay off that page.
    let box_pt = entry
        .layout_size
        .map_or(total_pt, |size| size.height.to_f32().max(total_pt));
    let continues = box_pt > total_pt + 0.75;
    // A flat slice is the same rectangle whether or not the box goes on (and a body that paints nothing is flat).
    if is_flat_slice(&entry.style, paint_background) || (slices.len() == 1 && !continues) {
        paint_root_block_v2(canvas, entry, x_pt, top_pt, Some(here_pt), paint_background);
        return;
    }
    let first = slices.first().is_some_and(|f| f.page_index == page);
    let last = !continues && slices.last().is_some_and(|f| f.page_index == page);
    let cut_top = if first { 0.0 } else { top_pt };
    let cut_bottom = if last {
        page_height_pt
    } else {
        top_pt + here_pt
    };
    let cut = krilla::geom::Rect::from_xywh(0.0, cut_top, page_width_pt, cut_bottom - cut_top)
        .and_then(|rect| {
            let mut builder = krilla::geom::PathBuilder::new();
            builder.push_rect(rect);
            builder.finish()
        });
    if let Some(path) = &cut {
        canvas
            .surface
            .push_clip_path(path, &krilla::paint::FillRule::default());
    }
    paint_root_block_v2(
        canvas,
        entry,
        x_pt,
        top_pt - before.in_pt().to_f32(),
        Some(box_pt),
        paint_background,
    );
    if cut.is_some() {
        canvas.surface.pop();
    }
}

/// Whether a block paints a background of its own: a colour that is not fully transparent, or an image layer.
fn has_background(style: &crate::draw_primitives::BlockStyle) -> bool {
    style.background_color.is_some_and(|c| c[3] > 0) || !style.background_layers.is_empty()
}

/// The element whose background is the canvas background (css-backgrounds-3 §2.11): the root element's if it has one,
/// otherwise the first `<body>`'s (§2.11.2: the root has no image and a transparent colour). `None` when neither
/// paints a background. The source paints no background of its own box.
fn canvas_background_source(d: &Drawables) -> Option<crate::drawables::NodeId> {
    let has = |id: Option<crate::drawables::NodeId>| {
        id.filter(|id| {
            d.block_styles
                .get(id)
                .is_some_and(|e| has_background(&e.style))
        })
    };
    has(d.root_id).or_else(|| has(d.body_id))
}

/// Overflow smaller than this (pt) is rounding, not content: nothing is reported.
const PAGE_OVERFLOW_TOLERANCE_PT: f32 = 0.5;

/// Records the right edge of a box that paints a background or a border, for the page-overflow warning.
fn note_painted_box(
    canvas: &mut crate::draw_primitives::Canvas<'_, '_>,
    style: &crate::draw_primitives::BlockStyle,
    x: f32,
    width: f32,
) {
    let background =
        style.background_color.is_some_and(|c| c[3] > 0) || !style.background_layers.is_empty();
    let border = style
        .border_widths
        .iter()
        .zip(&style.border_styles)
        .any(|(w, s)| {
            *w > crate::units::Pt::ZERO && *s != crate::draw_primitives::BorderStyleValue::None
        });
    if background || border {
        canvas.paint_extent.note_right(x + width);
    }
}

/// Block bg / border / shadow paint without the outer `draw_with_opacity`
/// wrap. Used by `draw_list_item_with_block` so the list-item's
/// marker and body block share a single opacity transparency group
/// (CSS opacity §2).
fn draw_block_inner_paint(
    canvas: &mut crate::draw_primitives::Canvas<'_, '_>,
    entry: &crate::drawables::BlockEntry,
    x: f32,
    y: f32,
    frag: &crate::pagination_layout::Fragment,
    is_split: bool,
) {
    let total_width = entry
        .layout_size
        .map(|s| s.width.to_f32())
        .unwrap_or_else(|| frag.width.in_pt().to_f32());
    // For split blocks (one fragment per page), `frag.height` reports
    // the per-page slice height. `entry.layout_size.height` always
    // carries Taffy's full block height, so painting bg / border with
    // `layout_size` would draw the FULL block on every slice — visible
    // as a callout-box overflowing page bottom on page 1 AND repeating
    // full-size on page 2 (fulgur-bq6i: `examples/break-inside`).
    //
    // Recover the slice-correct height from `frag.height` when the
    // dispatcher signals this is a split fragment
    // (`is_split = geom.fragments.len() > 1`). Using the multi-
    // fragment signal — not a `frag_h < layout_h` comparison —
    // avoids spurious flips for single-page blocks where the two
    // values may differ by 1 ULP after CSS-px → pt conversion
    // rounding.
    let total_height = entry
        .layout_size
        .map(|s| {
            if is_split && frag.height > crate::units::Px::ZERO {
                frag.height.in_pt().to_f32()
            } else {
                s.height.to_f32()
            }
        })
        .unwrap_or_else(|| frag.height.in_pt().to_f32());

    if entry.visible {
        note_painted_box(canvas, &entry.style, x, total_width);
        crate::background::draw_box_shadows(canvas, &entry.style, x, y, total_width, total_height);
        crate::background::draw_background(canvas, &entry.style, x, y, total_width, total_height);
        crate::draw_primitives::draw_block_border(
            canvas,
            &entry.style,
            x,
            y,
            total_width,
            total_height,
        );
    }
}

/// Table draw: emits the outer-frame background / border / shadow.
/// Cell paint (each `<th>` / `<td>` is a block with its own NodeId in
/// geometry) lands through the standard per-NodeId dispatch.
///
/// Tables with `overflow: hidden | clip` route through
/// [`draw_under_clip_table`] instead so the clip path wraps every
/// cell dispatched in the same scope. A repeated `<thead>` on
/// continuation pages is painted separately, after the page's own
/// content, by [`paint_repeated_table_headers`].
#[allow(clippy::too_many_arguments)]
fn draw_table_v2(
    canvas: &mut crate::draw_primitives::Canvas<'_, '_>,
    table_id: usize,
    entry: &crate::drawables::TableEntry,
    x: f32,
    y: f32,
    frag: &crate::pagination_layout::Fragment,
    piece: bool,
    geometry: &crate::pagination_layout::PaginationGeometryTable,
    drawables: &Drawables,
    margin_left_pt: f32,
    margin_top_pt: f32,
    page_index: u32,
) {
    use crate::draw_primitives::draw_with_opacity;

    draw_with_opacity(canvas, entry.opacity, |canvas| {
        let (total_width, mut total_height) = table_box_size(entry, frag, piece);
        if piece {
            total_height += table_piece_bottom(
                entry,
                frag,
                geometry
                    .get(&table_id)
                    .map_or(&[][..], |g| &g.fragments[..]),
            );
        }
        if entry.visible {
            paint_table_outer_frame(canvas, entry, x, y, total_width, total_height);
        }
        // Row-group and row backgrounds sit above the table's own and below
        // every cell (CSS 2.1 §17.5.1). Cells are dispatched after the table
        // because their NodeIds are larger.
        paint_table_row_backgrounds(
            canvas,
            table_id,
            None,
            piece.then_some(frag),
            geometry,
            drawables,
            margin_left_pt,
            margin_top_pt,
            page_index,
        );
    });
}

/// F19: paint the backgrounds of a table's row groups and rows behind its
/// cells on `page_index`.
///
/// Blitz gives rows and row groups no box, so each background is painted per
/// cell, into the cell's border box on this page (CSS Tables 3 / CSS 2.1
/// §17.5.1: a row background shows only inside the cells of the row, and the
/// area between cells shows the table's). Colour-only backgrounds fill the
/// cell rect directly. Images and gradients are laid out against the whole row
/// (or group) area, shifted by the cell's offset in it, and clipped to the
/// cell, so one image runs across the row instead of restarting in each cell.
///
/// `only_group` restricts the paint to the backgrounds of one row group and its
/// rows (the repeated header of F12). `piece` is the fragment of a table that a multicol container cut into
/// pieces (F174): only the cells inside it are painted, so that each piece paints its own rows once, above its own
/// frame (the table has several fragments on a page, each of which draws the table). `margin_*_pt` are the same
/// offsets the table's cells are dispatched with, so the rects line up with them.
#[allow(clippy::too_many_arguments)]
fn paint_table_row_backgrounds(
    canvas: &mut crate::draw_primitives::Canvas<'_, '_>,
    table_id: usize,
    only_group: Option<usize>,
    piece: Option<&crate::pagination_layout::Fragment>,
    geometry: &crate::pagination_layout::PaginationGeometryTable,
    drawables: &Drawables,
    margin_left_pt: f32,
    margin_top_pt: f32,
    page_index: u32,
) {
    let Some(backgrounds) = drawables.table_row_backgrounds.get(&table_id) else {
        return;
    };
    for bg in backgrounds
        .iter()
        .filter(|b| only_group.is_none_or(|g| b.source == g || b.group == Some(g)))
    {
        for cell in &bg.cells {
            let Some(cell_geom) = geometry.get(&cell.cell) else {
                continue;
            };
            for frag in cell_geom
                .fragments
                .iter()
                .filter(|f| f.page_index == page_index)
            {
                // A cell whose direct text was split between lines (F30b) holds the
                // span of its lines in the raw fragment; its frame also covers the
                // padding and border above and below them.
                let (frag, _) = frame_fragment(cell_geom, frag);
                if let Some(piece) = piece {
                    // The middle of the cell is in the piece.
                    let cx = frag.x.to_f32() + frag.width.to_f32() / 2.0;
                    let cy = frag.y.to_f32() + frag.height.to_f32() / 2.0;
                    if cx < piece.x.to_f32()
                        || cx >= piece.x.to_f32() + piece.width.to_f32()
                        || cy < piece.y.to_f32()
                        || cy >= piece.y.to_f32() + piece.height.to_f32()
                    {
                        continue;
                    }
                }
                let x = margin_left_pt + frag.x.in_pt().to_f32();
                let y = margin_top_pt + frag.y.in_pt().to_f32();
                let w = frag.width.in_pt().to_f32();
                let h = frag.height.in_pt().to_f32();
                if w <= 0.0 || h <= 0.0 {
                    continue;
                }
                if bg.style.background_layers.is_empty() {
                    crate::background::draw_background(canvas, &bg.style, x, y, w, h);
                } else {
                    crate::background::draw_background_clipped(
                        canvas,
                        &bg.style,
                        (x, y, w, h),
                        x - cell.dx.to_f32(),
                        y - cell.dy.to_f32(),
                        bg.area.width.to_f32(),
                        bg.area.height.to_f32(),
                    );
                }
            }
        }
    }
}

/// Resolve the table's outer-frame width/height from the cached
/// layout. Falls back to the current Fragment height (and finally
/// `cached_height`) when `layout_size` is unset (test-only paths).
///
/// F174: a `piece` of a table cut by a multicol container is a closed box as tall as its fragment
/// (`layout_size.height` is the height of the whole table: every piece would paint the frame that tall, down the
/// columns).
fn table_box_size(
    entry: &crate::drawables::TableEntry,
    frag: &crate::pagination_layout::Fragment,
    piece: bool,
) -> (f32, f32) {
    let total_width = entry
        .layout_size
        .map(|s| s.width.to_f32())
        .unwrap_or(entry.width.to_f32());
    let total_height = entry
        .layout_size
        .filter(|_| !piece)
        .map(|s| s.height.to_f32())
        .unwrap_or_else(|| {
            let from_frag = frag.height.in_pt().to_f32();
            if from_frag > 0.0 {
                from_frag
            } else {
                entry.cached_height.to_f32()
            }
        });
    (total_width, total_height)
}

/// F174: the room a piece of a cut table needs below its last row for the table's bottom border, pt. A piece's
/// fragment ends at its last row; only the table's last piece holds the bottom border (and padding) in its height, so
/// every other piece is closed by a border drawn below its rows, not over the last one (a row background painted
/// after the frame would cover it).
fn table_piece_bottom(
    entry: &crate::drawables::TableEntry,
    frag: &crate::pagination_layout::Fragment,
    fragments: &[crate::pagination_layout::Fragment],
) -> f32 {
    if fragments.last().is_none_or(|last| last == frag) {
        0.0
    } else {
        entry.style.border_widths[2].to_f32()
    }
}

/// Paint the table's outer-frame bg / border / shadow at the current
/// (x, y, width, height). Shared between the no-clip path
/// ([`draw_table_v2`]) and the clip path ([`draw_under_clip_table`])
/// so the two emit identical PDF operators for the same input.
fn paint_table_outer_frame(
    canvas: &mut crate::draw_primitives::Canvas<'_, '_>,
    entry: &crate::drawables::TableEntry,
    x: f32,
    y: f32,
    total_width: f32,
    total_height: f32,
) {
    crate::background::draw_box_shadows(canvas, &entry.style, x, y, total_width, total_height);
    crate::background::draw_background(canvas, &entry.style, x, y, total_width, total_height);
    crate::draw_primitives::draw_block_border(
        canvas,
        &entry.style,
        x,
        y,
        total_width,
        total_height,
    );
}

/// Push a `compute_overflow_clip_path` clip around the table's outer
/// frame, dispatch each cell descendant inside the clip, then pop.
/// Mirrors [`draw_under_clip`]'s shape for blocks but specialised for
/// tables (no list-item marker, no shared-node_id inner content).
#[allow(clippy::too_many_arguments)]
fn draw_under_clip_table(
    canvas: &mut crate::draw_primitives::Canvas<'_, '_>,
    table_id: usize,
    table: &crate::drawables::TableEntry,
    geom: &crate::pagination_layout::PaginationGeometry,
    frag: &crate::pagination_layout::Fragment,
    x_pt: f32,
    y_pt: f32,
    geometry: &crate::pagination_layout::PaginationGeometryTable,
    drawables: &Drawables,
    margin_left_pt: f32,
    margin_top_pt: f32,
    page_index: u32,
) {
    use crate::draw_primitives::draw_with_opacity;

    let (total_width, mut total_height) = table_box_size(table, frag, geom.table_pieces);
    if geom.table_pieces {
        total_height += table_piece_bottom(table, frag, &geom.fragments);
    }

    draw_with_opacity(canvas, table.opacity, |canvas| {
        // bg / border / shadow OUTSIDE the clip, mirroring
        // `draw_under_clip` for blocks (`pageable.rs:1796-1827`).
        if table.visible {
            paint_table_outer_frame(canvas, table, x_pt, y_pt, total_width, total_height);
        }

        // Push clip — fall through to descendant dispatch even if
        // `compute_overflow_clip_path` returns `None` so the cells
        // still paint (defensive, mirrors `draw_under_clip`).
        let clip_pushed = if let Some(clip_path) =
            crate::draw_primitives::compute_overflow_clip_path(
                &table.style,
                x_pt,
                y_pt,
                total_width,
                total_height,
            ) {
            canvas
                .surface
                .push_clip_path(&clip_path, &krilla::paint::FillRule::default());
            canvas.paint_extent.enter_local();
            true
        } else {
            false
        };

        // Row-group and row backgrounds: inside the clip like the cells they
        // sit behind, above the outer frame painted above.
        paint_table_row_backgrounds(
            canvas,
            table_id,
            None,
            geom.table_pieces.then_some(frag),
            geometry,
            drawables,
            margin_left_pt,
            margin_top_pt,
            page_index,
        );

        // Mirror `draw_under_clip`'s nested-scope skip sets so cells
        // carrying their own `transform` / `overflow:hidden` /
        // fractional opacity recurse into the proper helper rather
        // than fall through to plain `dispatch_fragment` (which would
        // silently lose the inner clip / transform / opacity wrap).
        // (PR #320 Devin Review)
        let transform_skip: std::collections::BTreeSet<usize> = table
            .clip_descendants
            .iter()
            .filter_map(|id| drawables.transforms.get(id))
            .flat_map(|tx| tx.descendants.iter().copied())
            .collect();
        let mut nested_clip_skip: std::collections::BTreeSet<usize> = table
            .clip_descendants
            .iter()
            .filter_map(|id| drawables.block_styles.get(id))
            .filter(|b| b.style.has_overflow_clip())
            .flat_map(|b| b.clip_descendants.iter().copied())
            .collect();
        nested_clip_skip.extend(
            table
                .clip_descendants
                .iter()
                .filter_map(|id| drawables.tables.get(id))
                .filter(|t| t.style.has_overflow_clip())
                .flat_map(|t| t.clip_descendants.iter().copied()),
        );
        let nested_opacity_skip: std::collections::BTreeSet<usize> = table
            .clip_descendants
            .iter()
            .filter_map(|id| drawables.block_styles.get(id))
            .flat_map(|b| b.opacity_descendants.iter().copied())
            .collect();

        // A table has no paragraph of its own, so the skip set comes purely
        // from the cell paragraphs inside `clip_descendants` — that is
        // where the duplicate paint of a cell's inline box originates.
        let paragraph_ib_skip =
            paragraph_owned_inline_boxes(table.clip_descendants.iter().copied(), drawables);
        for &desc_id in &table.clip_descendants {
            if transform_skip.contains(&desc_id)
                || nested_clip_skip.contains(&desc_id)
                || nested_opacity_skip.contains(&desc_id)
                || paragraph_ib_skip.contains(&desc_id)
            {
                continue;
            }
            let Some(desc_geom) = geometry.get(&desc_id) else {
                continue;
            };
            for desc_frag in &desc_geom.fragments {
                if desc_frag.page_index != page_index {
                    continue;
                }
                let desc_x = margin_left_pt + desc_frag.x.in_pt().to_f32();
                let desc_y = margin_top_pt + desc_frag.y.in_pt().to_f32();
                if let Some(desc_tx) = drawables.transforms.get(&desc_id) {
                    draw_under_transform(
                        canvas,
                        desc_tx,
                        desc_id,
                        desc_geom,
                        desc_frag,
                        desc_x,
                        desc_y,
                        geometry,
                        drawables,
                        margin_left_pt,
                        margin_top_pt,
                        page_index,
                    );
                } else if let Some(desc_block) = drawables
                    .block_styles
                    .get(&desc_id)
                    .filter(|b| b.style.has_overflow_clip())
                    .filter(|_| Some(desc_id) != drawables.body_id)
                {
                    draw_under_clip(
                        canvas,
                        desc_block,
                        desc_id,
                        desc_geom,
                        desc_frag,
                        desc_x,
                        desc_y,
                        geometry,
                        drawables,
                        margin_left_pt,
                        margin_top_pt,
                        page_index,
                    );
                } else if let Some(desc_table) = drawables
                    .tables
                    .get(&desc_id)
                    .filter(|t| t.style.has_overflow_clip())
                {
                    draw_under_clip_table(
                        canvas,
                        desc_id,
                        desc_table,
                        desc_geom,
                        desc_frag,
                        desc_x,
                        desc_y,
                        geometry,
                        drawables,
                        margin_left_pt,
                        margin_top_pt,
                        page_index,
                    );
                } else if let Some(desc_block) = drawables
                    .block_styles
                    .get(&desc_id)
                    .filter(|b| !b.opacity_descendants.is_empty())
                {
                    draw_under_opacity(
                        canvas,
                        desc_block,
                        desc_id,
                        desc_geom,
                        desc_frag,
                        desc_x,
                        desc_y,
                        geometry,
                        drawables,
                        margin_left_pt,
                        margin_top_pt,
                        page_index,
                    );
                } else {
                    dispatch_fragment(
                        canvas,
                        desc_id,
                        desc_geom,
                        desc_frag,
                        desc_x,
                        desc_y,
                        drawables,
                        geometry,
                        margin_left_pt,
                        margin_top_pt,
                        page_index,
                    );
                }
            }
        }

        // F22: the collapsed borders of the tables in this scope, above their cells.
        paint_wrapper_tables(
            canvas,
            table_id,
            &table.clip_descendants,
            page_index,
            margin_left_pt,
            margin_top_pt,
            geometry,
            drawables,
        );

        if clip_pushed {
            canvas.paint_extent.leave_local();
            canvas.surface.pop();
        }
    });
}

/// Block + inner content combined draw. Wraps bg/border **and** the
/// inner content in ONE `draw_with_opacity(self.opacity, ...)` group
/// (CSS opacity §2 — a single transparency group covers the whole
/// composed paint).
///
/// The shared-node_id patterns from `convert::inline_root` (block
/// wraps an inline-root paragraph) and `convert::replaced` (block
/// wraps `<img>` / `<svg>`) deliberately leave the inner draw payload
/// at `opacity: 1.0` — the wrapping block carries the real opacity.
/// Composing them all under a single
/// `draw_with_opacity(block.opacity, ...)` keeps the `q .. Q` framing
/// intact for `<p style="opacity:0.5; background:red">text</p>` and
/// friends.
#[allow(clippy::too_many_arguments)]
fn draw_block_with_inner_content(
    canvas: &mut crate::draw_primitives::Canvas<'_, '_>,
    block: &crate::drawables::BlockEntry,
    paragraph: Option<&crate::drawables::ParagraphEntry>,
    image: Option<&crate::drawables::ImageEntry>,
    svg: Option<&crate::drawables::SvgEntry>,
    x: f32,
    y: f32,
    frag: &crate::pagination_layout::Fragment,
    fragments: &[crate::pagination_layout::Fragment],
    line_slices: &[crate::pagination_layout::LineSlice],
    page_index: u32,
    is_split: bool,
    drawables: &Drawables,
    geometry: &crate::pagination_layout::PaginationGeometryTable,
    margin_left_pt: f32,
    margin_top_pt: f32,
) {
    use crate::draw_primitives::draw_with_opacity;

    // Inner content (inline-root paragraph from `convert::inline_root`
    // or replaced image / svg from `convert::replaced`) is positioned
    // at the block's content-box top-left, not its border-box top-left.
    // Because the inner payload shares the block's `node_id`, it
    // would otherwise paint at the block's border-box origin,
    // dropping `padding + border` worth of offset for every
    // inline-root or replaced element. Read the inset from the
    // BlockStyle and add it before recursing.
    let (ix, iy) = block.style.content_inset();
    let inner_x = x + ix;
    let inner_y = y + iy;

    draw_with_opacity(canvas, block.opacity, |canvas| {
        draw_block_inner_paint(canvas, block, x, y, frag, is_split);
        if let Some(p) = paragraph {
            draw_paragraph_inner_paint(
                canvas,
                p,
                inner_x,
                inner_y,
                fragments,
                line_slices,
                page_index,
                is_split,
                drawables,
                geometry,
                margin_left_pt,
                margin_top_pt,
            );
        }
        if let Some(i) = image {
            draw_image_inner_paint(canvas, i, inner_x, inner_y);
        }
        if let Some(s) = svg {
            draw_svg_inner_paint(canvas, s, inner_x, inner_y);
        }
    });
}

/// List-item combined draw. Wraps the marker plus everything painted
/// by the body block in a single
/// `draw_with_opacity(self.opacity, ...)` group so
/// `<li style="opacity:..">` composites as one transparency group
/// (CSS opacity §2).
///
/// The `<li>` and its body block share the same node_id
/// (`convert/list_item.rs:81`); the body is built with `opacity: 1.0`
/// on purpose. When the body holds inline content, the inline-root
/// paragraph also lands at the same node_id (see
/// `convert::inline_root`). Painting marker + block frame + paragraph
/// glyphs in one compositing group avoids emitting multiple
/// `q .. Q` pairs that would break the single-group semantics.
#[allow(clippy::too_many_arguments)]
fn draw_list_item_with_block(
    canvas: &mut crate::draw_primitives::Canvas<'_, '_>,
    node_id: usize,
    list_item: &crate::drawables::ListItemEntry,
    block: Option<&crate::drawables::BlockEntry>,
    paragraph: Option<&crate::drawables::ParagraphEntry>,
    x: f32,
    y: f32,
    frag: &crate::pagination_layout::Fragment,
    fragments: &[crate::pagination_layout::Fragment],
    line_slices: &[crate::pagination_layout::LineSlice],
    page_index: u32,
    is_split: bool,
    drawables: &Drawables,
    geometry: &crate::pagination_layout::PaginationGeometryTable,
    margin_left_pt: f32,
    margin_top_pt: f32,
) {
    use crate::draw_primitives::draw_with_opacity;

    // Same content-box offset trick as `draw_block_with_inner_content`
    // — when the body block carries `padding` / `border`, the
    // inline-root paragraph that shares the list item's node_id has to
    // paint at the body block's content-box top-left.
    let inset = block.map(|b| b.style.content_inset()).unwrap_or((0.0, 0.0));
    let inner_x = x + inset.0;
    let inner_y = y + inset.1;

    // F30: an item split across pages has one marker, beside its first line.
    let is_continuation = is_split
        && fragments
            .first()
            .is_some_and(|first| first.page_index != page_index);
    draw_with_opacity(canvas, list_item.opacity, |canvas| {
        if list_item.visible && !is_continuation {
            draw_list_item_marker_tagged(canvas, list_item, node_id, drawables, x, y);
        }
        if let Some(b) = block {
            draw_block_inner_paint(canvas, b, x, y, frag, is_split);
        }
        if let Some(p) = paragraph {
            let run_tag_node_id = run_tag_target(drawables, node_id);
            let use_run_tagging = canvas.tag_collector.is_some()
                && para_has_link_runs(p)
                && run_tag_node_id.is_some();
            let tag_info = if use_run_tagging {
                canvas.link_run_node_id = run_tag_node_id;
                None
            } else {
                try_start_tagged(canvas, node_id, drawables)
            };
            draw_paragraph_inner_paint(
                canvas,
                p,
                inner_x,
                inner_y,
                fragments,
                line_slices,
                page_index,
                is_split,
                drawables,
                geometry,
                margin_left_pt,
                margin_top_pt,
            );
            finish_tagged(canvas, tag_info);
            if use_run_tagging {
                canvas.link_run_node_id = None;
            }
        }
    });
}

/// List-item marker paint without opacity wrapper or visibility gate
/// — caller (`draw_list_item_with_block`) handles both so the marker
/// and the body block share one compositing group.
fn draw_list_item_marker(
    canvas: &mut crate::draw_primitives::Canvas<'_, '_>,
    entry: &crate::drawables::ListItemEntry,
    x: f32,
    y: f32,
) {
    use crate::drawables::{ImageMarker, ListItemMarker};

    match &entry.marker {
        ListItemMarker::Text { lines, width } if !lines.is_empty() => {
            crate::paragraph::draw_shaped_lines(canvas, lines, x.as_pt() - *width, y.as_pt(), None);
        }
        ListItemMarker::Image {
            marker,
            width,
            height,
        } => {
            let marker_x = x - width.to_f32();
            let marker_y = (y.as_pt() + (entry.marker_line_height - *height) / 2.0).to_f32();
            match marker {
                ImageMarker::Raster(img) => draw_image_v2(canvas, img, marker_x, marker_y),
                ImageMarker::Svg(svg) => draw_svg_v2(canvas, svg, marker_x, marker_y),
            }
        }
        _ => {}
    }
}

/// List-item marker を描画し、タグ付きモードでは Lbl 構造要素でラップする。
///
/// When `canvas.in_marked_content` is already set (a nested tagged marker
/// inside an outer marked-content sequence — e.g. a `<ul><li>...</li></ul>`
/// dispatched from a tagged block's inline-box), the Lbl tag is skipped so
/// Krilla's non-nestable `start_tagged` does not panic. The marker glyphs
/// / image still paint; only the structural Lbl association is dropped for
/// the nested case (graceful degradation).
fn draw_list_item_marker_tagged(
    canvas: &mut crate::draw_primitives::Canvas<'_, '_>,
    li: &crate::drawables::ListItemEntry,
    node_id: usize,
    drawables: &Drawables,
    x: f32,
    y: f32,
) {
    let lbl_id = canvas
        .tag_collector
        .as_ref()
        .and_then(|_| drawables.li_lbl_ids.get(&node_id).copied());
    let can_open_marker_tag = lbl_id.is_some() && !canvas.in_marked_content;
    let marker_tag_id = can_open_marker_tag.then(|| {
        let id = canvas
            .surface
            .start_tagged(krilla::tagging::ContentTag::Span(
                krilla::tagging::SpanTag::empty(),
            ));
        canvas.in_marked_content = true;
        id
    });

    draw_list_item_marker(canvas, li, x, y);

    if let (Some(lid), Some(id)) = (lbl_id, marker_tag_id) {
        canvas.surface.end_tagged();
        canvas.in_marked_content = false;
        canvas
            .tag_collector
            .as_mut()
            .expect("tag_collector is Some because marker_tag_id was issued from it")
            .record(lid, crate::tagging::PdfTag::Lbl, id, None);
    }
}

/// v2 paragraph draw. Mirrors `paragraph::ParagraphRender::draw`:
/// honour `visible`, wrap with `draw_with_opacity`, then call the
/// existing `paragraph::draw_shaped_lines` which already handles glyph
/// runs / inline images / inline boxes / link rect emission /
/// decoration spans. Reusing the helper keeps the per-glyph PDF output
/// byte-identical between v1 and v2.
#[allow(clippy::too_many_arguments)]
fn draw_paragraph_v2(
    canvas: &mut crate::draw_primitives::Canvas<'_, '_>,
    entry: &crate::drawables::ParagraphEntry,
    x: f32,
    y: f32,
    fragments: &[crate::pagination_layout::Fragment],
    line_slices: &[crate::pagination_layout::LineSlice],
    page_index: u32,
    is_split: bool,
    drawables: &Drawables,
    geometry: &crate::pagination_layout::PaginationGeometryTable,
    margin_left_pt: f32,
    margin_top_pt: f32,
) {
    use crate::draw_primitives::draw_with_opacity;
    draw_with_opacity(canvas, entry.opacity, |canvas| {
        draw_paragraph_inner_paint(
            canvas,
            entry,
            x,
            y,
            fragments,
            line_slices,
            page_index,
            is_split,
            drawables,
            geometry,
            margin_left_pt,
            margin_top_pt,
        );
    });
}

/// Paragraph paint without the outer `draw_with_opacity` wrap. Used
/// by `draw_list_item_with_block` so a list-item containing inline
/// content (the body block holds an inline-root paragraph at the
/// same node_id) can compose marker + block paint + glyph runs into
/// a single opacity transparency group (CSS opacity §2).
#[allow(clippy::too_many_arguments)]
fn draw_paragraph_inner_paint(
    canvas: &mut crate::draw_primitives::Canvas<'_, '_>,
    entry: &crate::drawables::ParagraphEntry,
    x: f32,
    y: f32,
    fragments: &[crate::pagination_layout::Fragment],
    line_slices: &[crate::pagination_layout::LineSlice],
    page_index: u32,
    is_split: bool,
    drawables: &Drawables,
    geometry: &crate::pagination_layout::PaginationGeometryTable,
    margin_left_pt: f32,
    margin_top_pt: f32,
) {
    if !entry.visible {
        return;
    }
    let Some(slice) =
        paragraph_lines_for_page(&entry.lines, fragments, line_slices, page_index, is_split)
    else {
        return;
    };
    // PR 8g: build an InlineBoxRenderCtx so `draw_shaped_lines` can
    // dispatch any `LineItem::InlineBox` content through
    // `dispatch_inline_box_content` under a translate transform that
    // moves the body-relative geometry dispatch position to the
    // inline-flow position computed from `ib.x_offset` and
    // `ib.computed_y` (the latter folds in the CSS 2.1 §10.8.1
    // baseline_shift correction from `convert/inline_root.rs:493`).
    let inline_box_ctx = Some(crate::paragraph::InlineBoxRenderCtx {
        drawables,
        geometry,
        page_index,
        margin_left_pt,
        margin_top_pt,
    });
    crate::paragraph::draw_shaped_lines(canvas, &slice, x.as_pt(), y.as_pt(), inline_box_ctx);
}

/// Phase 4 PR 3 follow-up (PR #302 Devin): mirror
/// `ParagraphRender::slice_for_page` so multi-page paragraphs only
/// emit the lines belonging to the requested page.
///
/// `is_split` is the parent `PaginationGeometry::is_split()` —
/// `false` when the paragraph fits one page OR when the geometry
/// represents per-page repetition (`is_repeat=true`, e.g.
/// `position: fixed`). Either case means each fragment carries the
/// full content, so the function returns every line unmodified.
/// `true` slices the lines.
///
/// F78: a fragmenter that split the paragraph between line boxes recorded which
/// lines each fragment holds (`line_slices`, parallel to `fragments`); those
/// are painted as given, moved up by the slice's offset, which puts the first
/// line of every page where line 0 sits on the first. Measuring the lines again
/// from float heights cannot be exact: Blitz quantises line boxes but Parley
/// advances by the unrounded line height, so boxes overlap or leave gaps and a
/// whole line is skipped or painted twice after a few dozen pages. Only
/// fragments from some other emitter (no or non-matching slices) are still
/// measured that way, with a warning.
fn paragraph_lines_for_page(
    all_lines: &[crate::paragraph::ShapedLine],
    fragments: &[crate::pagination_layout::Fragment],
    line_slices: &[crate::pagination_layout::LineSlice],
    page_index: u32,
    is_split: bool,
) -> Option<Vec<crate::paragraph::ShapedLine>> {
    let target_pos = fragments.iter().position(|f| f.page_index == page_index)?;

    if !is_split {
        return Some(all_lines.to_vec());
    }

    // The slices describe these lines: one per fragment, ending at the last line. F30f-b: a clip box that hides the
    // last lines of a paragraph has one more slice, for the hidden lines, which no fragment paints.
    if (line_slices.len() == fragments.len() || line_slices.len() == fragments.len() + 1)
        && line_slices
            .last()
            .is_some_and(|last| last.end as usize == all_lines.len())
    {
        let slice = &line_slices[target_pos];
        let (first, end) = (slice.first as usize, slice.end as usize);
        if first >= end || end > all_lines.len() {
            return None;
        }
        return Some(rebase_lines(&all_lines[first..end], slice.offset.in_pt()));
    }

    let target_h = fragments[target_pos].height.in_pt();
    let consumed: crate::units::Pt = fragments[..target_pos]
        .iter()
        .map(|f| f.height)
        .sum::<crate::units::Px>()
        .in_pt();

    let eps = 0.01_f32.as_pt();
    let mut line_top = crate::units::Pt::ZERO;
    let mut start_idx = 0usize;
    while start_idx < all_lines.len() {
        let next_top = line_top + all_lines[start_idx].height;
        if next_top > consumed + eps {
            break;
        }
        line_top = next_top;
        start_idx += 1;
    }

    let mut end_idx = start_idx;
    let mut accum = crate::units::Pt::ZERO;
    while end_idx < all_lines.len() {
        let line_h = all_lines[end_idx].height;
        if accum + line_h > target_h + eps {
            break;
        }
        accum += line_h;
        end_idx += 1;
    }

    if end_idx <= start_idx {
        return None;
    }

    // Measuring can only be wrong when the paragraph really is cut between lines: more than one line, and this page
    // gets some of them. A whole paragraph inside a block the block fragmenter split (one line, or every line on one
    // page) is drawn once either way.
    if measured_cut_is_partial(all_lines.len(), start_idx, end_idx) {
        log::warn!(
            "paragraph \"{}\" is split over {} pages but its fragmenter recorded no matching line ranges \
             ({} slices for {} fragments, {} lines); measuring its lines from the fragment heights, \
             which can skip or repeat a line",
            paragraph_start(all_lines),
            fragments.len(),
            line_slices.len(),
            fragments.len(),
            all_lines.len()
        );
    }

    Some(rebase_lines(&all_lines[start_idx..end_idx], consumed))
}

/// Whether a page whose lines `[start, end)` were measured from fragment heights holds only some of a paragraph of
/// `n` lines, the one case where that measuring can skip or repeat a line.
fn measured_cut_is_partial(n: usize, start: usize, end: usize) -> bool {
    n > 1 && end > start && (start > 0 || end < n)
}

/// Copies of `lines` moved up by `by`: paragraph-absolute coords (baseline +
/// inline image `computed_y`) become fragment-local. Mirror
/// `ParagraphRender::slice_for_page` exactly.
fn rebase_lines(
    lines: &[crate::paragraph::ShapedLine],
    by: crate::units::Pt,
) -> Vec<crate::paragraph::ShapedLine> {
    lines
        .iter()
        .cloned()
        .map(|mut line| {
            line.baseline -= by;
            for item in &mut line.items {
                if let crate::paragraph::LineItem::Image(img) = item {
                    img.computed_y -= by;
                }
            }
            line
        })
        .collect()
}

/// The first words of a paragraph, to name it in a warning.
fn paragraph_start(lines: &[crate::paragraph::ShapedLine]) -> String {
    let mut text = String::new();
    for item in lines.iter().flat_map(|l| &l.items) {
        if let crate::paragraph::LineItem::Text(run) = item {
            text.push_str(&run.text);
            if text.chars().count() >= 40 {
                break;
            }
        }
    }
    let mut start: String = text.chars().take(40).collect();
    if text.chars().count() > 40 {
        start.push_str("...");
    }
    start
}

/// Build krilla Metadata from Config.
fn build_metadata(config: &Config, html_title: Option<&str>) -> krilla::metadata::Metadata {
    let mut metadata = krilla::metadata::Metadata::new();
    let effective_title = config.title.as_deref().or(html_title);
    if let Some(title) = effective_title {
        metadata = metadata.title(title.to_string());
    }
    if !config.authors.is_empty() {
        metadata = metadata.authors(config.authors.clone());
    }
    if let Some(ref description) = config.description {
        metadata = metadata.description(description.clone());
    }
    if !config.keywords.is_empty() {
        metadata = metadata.keywords(config.keywords.clone());
    }
    if let Some(ref lang) = config.lang {
        metadata = metadata.language(lang.clone());
    }
    if let Some(ref creator) = config.creator {
        metadata = metadata.creator(creator.clone());
    }
    if let Some(ref producer) = config.producer {
        metadata = metadata.producer(producer.clone());
    }
    if let Some(ref date_str) = config.creation_date {
        if let Some(dt) = parse_datetime(date_str) {
            metadata = metadata.creation_date(dt);
        }
    }
    metadata
}

/// Parse an ISO 8601 date string into a krilla DateTime.
/// Supports: "YYYY", "YYYY-MM", "YYYY-MM-DD", "YYYY-MM-DDThh:mm:ss".
/// Returns None if any component fails to parse.
fn parse_datetime(s: &str) -> Option<krilla::metadata::DateTime> {
    let parts: Vec<&str> = s.splitn(2, 'T').collect();
    let date_tokens: Vec<&str> = parts[0].split('-').collect();
    let year: u16 = date_tokens.first()?.parse().ok()?;
    let mut dt = krilla::metadata::DateTime::new(year);
    if let Some(month_str) = date_tokens.get(1) {
        let month: u8 = month_str.parse().ok()?;
        dt = dt.month(month);
    }
    if let Some(day_str) = date_tokens.get(2) {
        let day: u8 = day_str.parse().ok()?;
        dt = dt.day(day);
    }
    if let Some(time_str) = parts.get(1) {
        // Strip trailing 'Z' for UTC
        let time_str = time_str.trim_end_matches('Z');
        let time_tokens: Vec<&str> = time_str.split(':').collect();
        if let Some(hour_str) = time_tokens.first() {
            let hour: u8 = hour_str.parse().ok()?;
            dt = dt.hour(hour);
        }
        if let Some(minute_str) = time_tokens.get(1) {
            let minute: u8 = minute_str.parse().ok()?;
            dt = dt.minute(minute);
        }
        if let Some(second_str) = time_tokens.get(2) {
            let second: u8 = second_str.parse().ok()?;
            dt = dt.second(second);
        }
    }
    Some(dt)
}

/// Cached max-content width and rendered `Drawables` for margin boxes.
/// Measure cache: (html, page_height as bits) → max-content width.
/// Render cache: (html, final_width as bits, final_height as bits) →
/// (`Drawables`, `PaginationGeometryTable`).
type MeasureCache = HashMap<(String, u32, u32), crate::units::Pt>;
type RenderCache = HashMap<
    (String, u32, u32),
    (
        crate::drawables::Drawables,
        crate::pagination_layout::PaginationGeometryTable,
    ),
>;

fn width_key(w: f32) -> u32 {
    w.to_bits()
}

/// Per-page state and caches required to render `@page` margin boxes
/// (`@top-center`, `@bottom-center`, `@left-middle`, etc.). Built once
/// per render and reused across pages so measure / layout passes for
/// repeated content (e.g. a page-number footer) hit the cache.
///
/// Used by both `render_to_pdf_with_gcpm` (v1 path) and `render_v2`
/// (Phase 4 v2 path) — both call `render_page` per page.
pub(crate) struct MarginBoxRenderer<'a> {
    pub gcpm: &'a GcpmContext,
    pub running_store: &'a RunningElementStore,
    pub font_data: &'a [Arc<Vec<u8>>],
    pub system_fonts: bool,
    pub generics: &'a BTreeMap<String, String>,
    pub margin_css: String,
    pub string_set_states: Vec<BTreeMap<String, crate::pagination_layout::StringSetPageState>>,
    pub running_states: Vec<BTreeMap<String, crate::pagination_layout::PageRunningState>>,
    pub counter_states: Vec<BTreeMap<String, i32>>,
    pub measure_cache: MeasureCache,
    pub height_cache: HashMap<(String, u32, u32), crate::units::Pt>,
    pub render_cache: RenderCache,
    /// fulgur-qgy7: per-page implicit `href` for
    /// `target-*(attr(href), ...)` evaluated inside `@page` margin
    /// boxes. Built once per render pass by `engine::Engine::layout_to_drawables`.
    pub implicit_href_by_page: &'a BTreeMap<usize, String>,
    /// The style and font environment all margin-box documents of this render are built from (F99b). Created on
    /// the first cache miss, so a document without margin boxes pays nothing. Owned by this renderer, hence by
    /// one `render_v2` call; nothing outlives it.
    env: std::cell::OnceCell<crate::blitz_adapter::MarginDocEnv>,
}

/// The environment of `cell`, created on first use from the render's CSS, fonts and generic families.
fn margin_env<'e>(
    cell: &'e std::cell::OnceCell<crate::blitz_adapter::MarginDocEnv>,
    margin_css: &str,
    font_data: &[Arc<Vec<u8>>],
    system_fonts: bool,
    generics: &BTreeMap<String, String>,
) -> &'e crate::blitz_adapter::MarginDocEnv {
    cell.get_or_init(|| {
        crate::blitz_adapter::MarginDocEnv::new(margin_css, font_data, system_fonts, generics)
    })
}

/// The `<style>` element a margin-box wrapper carries: none when the environment attaches the render's CSS.
fn margin_wrapper_style(env: &crate::blitz_adapter::MarginDocEnv, margin_css: &str) -> String {
    if env.author_sheet_attached() {
        String::new()
    } else {
        format!("<style>{margin_css}</style>")
    }
}

/// One margin box of a page after its `content` was resolved.
struct ResolvedBox<'r> {
    /// The HTML the measure passes (stages 1a, 1b) and the distribution (stage 2) use: the content, in a `<div>`
    /// with the author's declarations when the box has any.
    measure_html: String,
    /// The resolved content alone.
    content_html: String,
    /// The author's declaration block of the box, as written.
    declarations: &'r str,
}

impl<'r> ResolvedBox<'r> {
    /// `measure_html` is what stages 1a, 1b and 2 measured before the box was aligned in its rectangle (F103): the
    /// content in a `<div>` with the author's declarations, or the bare content when there are none. It is kept
    /// as it was, so every measured size, every rectangle and every measure-cache entry is the one it always was.
    fn new(content_html: String, declarations: &'r str) -> Self {
        let measure_html = if declarations.is_empty() {
            content_html.clone()
        } else {
            format!(
                "<div style=\"{}\">{}</div>",
                escape_attr(declarations),
                content_html
            )
        };
        Self {
            measure_html,
            content_html,
            declarations,
        }
    }
}

/// The HTML stage 3 lays out at the confirmed rectangle: the content in the one cell of a table that is exactly
/// as large as the rectangle, so that the cell aligns it (css-page-3: `vertical-align` in a margin box acts as in
/// a table cell, CSS 2.1 section 17.5.3; `text-align` per line, section 16.2). The position's UA defaults
/// (`MarginBoxPosition::default_text_align` / `default_vertical_align`) come first in the cell's style and the
/// author's declarations after them, so the author wins.
///
/// - The three boxes are `div`s with `display: table*`, not `<table>`: the element brings the UA
///   `border-spacing: 2px` and Tailwind preflight's `border-collapse: collapse`, and both would move the content.
/// - `table-layout: fixed` keeps the cell exactly as wide as the rectangle; an unbreakable word overflows it, as
///   it overflowed the block before.
/// - The cell carries the width itself (`box-sizing: border-box`, so an author padding stays inside it). Without
///   it Blitz sizes the cell from its content: a `flex: 1; min-width: 0` item that holds 400 characters (the
///   `leader()` fill, fixture `gcpm/leader-toc`) then stretches the cell past the rectangle and the page number
///   behind it is pushed out of the box.
/// - Margins do not apply to a table cell (CSS 2.1 section 17.5), so the cell's margin is forced to 0 after the
///   author's declarations. Left alone, Blitz lays a cell with a `margin-top` out below the row's box and the
///   fragmenter moves it off the margin document's first page: the whole box stopped being painted
///   (`examples/header-footer-split`, `@top-right { margin-top: 12px }`). A margin on a margin box used to move
///   its content (a workaround for the missing alignment, F103); it no longer does, `padding` still does.
/// - The explicit row keeps the cell off the anonymous-box path.
/// - The table height is a minimum: content taller than the rectangle grows the row and starts at the top.
fn margin_box_cell_html(
    pos: MarginBoxPosition,
    resolved: &ResolvedBox<'_>,
    rect: MarginBoxRect,
) -> String {
    format!(
        "<div style=\"display:table;table-layout:fixed;width:{}px;height:{}px\">\
         <div style=\"display:table-row\">\
         <div style=\"display:table-cell;box-sizing:border-box;width:{}px;text-align:{};vertical-align:{};{};margin:0 !important\">{}</div>\
         </div></div>",
        rect.width.in_px().to_f32(),
        rect.height.in_px().to_f32(),
        rect.width.in_px().to_f32(),
        pos.default_text_align(),
        pos.default_vertical_align(),
        escape_attr(resolved.declarations),
        resolved.content_html
    )
}

impl<'a> MarginBoxRenderer<'a> {
    /// Build a renderer from raw inputs. `string_set_by_node` /
    /// `counter_ops_by_node` are the per-node maps drained out of
    /// `ConvertContext` before `dom_to_drawables` consumed them.
    #[allow(clippy::too_many_arguments)]
    pub(crate) fn new(
        gcpm: &'a GcpmContext,
        running_store: &'a RunningElementStore,
        font_data: &'a [Arc<Vec<u8>>],
        system_fonts: bool,
        generics: &'a BTreeMap<String, String>,
        pagination_geometry: &crate::pagination_layout::PaginationGeometryTable,
        string_set_by_node: &HashMap<usize, Vec<(String, String)>>,
        counter_ops_by_node: &BTreeMap<usize, Vec<crate::gcpm::CounterOp>>,
        total_pages: usize,
        implicit_href_by_page: &'a BTreeMap<usize, String>,
    ) -> Self {
        let string_set_states = if gcpm.string_set_mappings.is_empty() {
            vec![BTreeMap::new(); total_pages]
        } else {
            let by_node_btree: BTreeMap<usize, Vec<(String, String)>> = string_set_by_node
                .iter()
                .map(|(k, v)| (*k, v.clone()))
                .collect();
            crate::pagination_layout::collect_string_set_states(pagination_geometry, &by_node_btree)
        };
        let running_states = if gcpm.running_mappings.is_empty() {
            vec![BTreeMap::new(); total_pages]
        } else {
            crate::pagination_layout::collect_running_element_states(
                pagination_geometry,
                running_store,
            )
        };
        let counter_states =
            if gcpm.counter_mappings.is_empty() && gcpm.content_counter_mappings.is_empty() {
                vec![BTreeMap::new(); total_pages]
            } else {
                crate::pagination_layout::collect_counter_states(
                    pagination_geometry,
                    counter_ops_by_node,
                )
            };
        Self {
            gcpm,
            running_store,
            font_data,
            system_fonts,
            generics,
            margin_css: strip_display_none(&gcpm.cleaned_css),
            string_set_states,
            running_states,
            counter_states,
            measure_cache: HashMap::new(),
            height_cache: HashMap::new(),
            render_cache: HashMap::new(),
            implicit_href_by_page,
            env: std::cell::OnceCell::new(),
        }
    }

    /// Render every margin box that applies to `page_idx` onto
    /// `canvas`. Mirrors the per-page block from
    /// `render_to_pdf_with_gcpm`'s pre-Phase-4 implementation:
    ///
    /// 1. Filter `gcpm.margin_boxes` by `@page` selector matching
    ///    (`:first` / `:left` / `:right`), preferring more-specific
    ///    selectors over the default.
    /// 2. Resolve each box's HTML content (substituting `counter()` /
    ///    `element()` / `string()` from per-page state).
    /// 3. Measure max-content width (top/bottom) or height (left/right).
    /// 4. Distribute boxes along each edge with `compute_edge_layout`.
    /// 5. Render each box at its final rect via Blitz parse + layout +
    ///    `dom_to_drawables`, then dispatched through the v2 paint path.
    ///
    /// `content_width` is the page content area width in pt — used as
    /// the available width during measure passes for top/bottom boxes.
    ///
    /// `anchor_map` is the pass-2 cross-reference table; when `Some`,
    /// `target-counter()` / `target-counters()` / `target-text()` inside
    /// margin-box `content` resolve via the supplied map. When `None`
    /// (pass 1, or single-pass renders without target refs), those
    /// resolvers return empty strings — see
    /// `gcpm::counter::resolve_content_to_html_with_anchor`.
    ///
    /// fulgur-qgy7: margin-box `target-*(attr(href), ...)` resolves
    /// against the per-page implicit `href` stored on
    /// `implicit_href_by_page`. The map is populated by
    /// `engine::build_implicit_href_map` from the first
    /// `<a href="#...">` whose first fragment lands on each page.
    /// Pages with no such anchor look up `None` and the resolver
    /// returns an empty string.
    /// Returns `true` if any effective margin box resolved to non-empty
    /// content on this page (regardless of edge). Callers use this to
    /// decide whether body-link annotations that landed in a page-
    /// margin strip must be clipped: on a page with zero margin boxes
    /// nothing paints on top of body content in the strips, so
    /// preserving body annotations there keeps `position: fixed;
    /// bottom: 0`-style body-drawn page furniture clickable.
    #[allow(clippy::too_many_arguments)]
    pub(crate) fn render_page(
        &mut self,
        canvas: &mut Canvas<'_, '_>,
        page_idx: usize,
        page_num: usize,
        total_pages: usize,
        page_size: crate::config::PageSize,
        resolved_margin: crate::config::Margin,
        content_width: f32,
        anchor_map: Option<&AnchorMap>,
    ) -> bool {
        // Resolve effective boxes: pick the most specific matching rule
        // per position. Pseudo-class selectors (`:first`, `:left`,
        // `:right`) override the default `@page` rule.
        let mut effective_boxes: BTreeMap<MarginBoxPosition, &crate::gcpm::MarginBoxRule> =
            BTreeMap::new();
        for margin_box in &self.gcpm.margin_boxes {
            let matches = match &margin_box.page_selector {
                None => true,
                Some(sel) => match sel.as_str() {
                    ":first" => page_num == 1,
                    ":left" => page_num % 2 == 0,
                    ":right" => page_num % 2 != 0,
                    _ => true,
                },
            };
            if !matches {
                continue;
            }
            let should_replace = effective_boxes
                .get(&margin_box.position)
                .map(|existing| {
                    existing.page_selector.is_none() && margin_box.page_selector.is_some()
                })
                .unwrap_or(true);
            if should_replace {
                effective_boxes.insert(margin_box.position, margin_box);
            }
        }

        // Resolve HTML for each effective box. Margin-box content goes
        // through `_with_anchor` so `target-*` resolves when
        // `anchor_map` is present (pass 2).
        let implicit_href = self
            .implicit_href_by_page
            .get(&page_idx)
            .map(String::as_str);
        let mut resolved_htmls: BTreeMap<MarginBoxPosition, ResolvedBox<'_>> = BTreeMap::new();
        for (&pos, rule) in &effective_boxes {
            let content_html = resolve_content_to_html_with_anchor(
                &rule.content,
                self.running_store,
                &self.running_states,
                &self.string_set_states[page_idx],
                page_num,
                total_pages,
                page_idx,
                &self.counter_states[page_idx],
                anchor_map,
                implicit_href,
            );
            if !content_html.is_empty() {
                resolved_htmls.insert(pos, ResolvedBox::new(content_html, &rule.declarations));
            }
        }

        // Stage 1a: measure max-content width for top / bottom boxes.
        for (&pos, resolved) in &resolved_htmls {
            if !pos.edge().is_some_and(|e| e.is_horizontal()) {
                continue;
            }
            let html = &resolved.measure_html;
            // Cache key includes `content_width` because `@page :first` /
            // `:left` / `:right` can override margins per page, changing
            // the available viewport width that Blitz lays the
            // `display: inline-block` measure document at. Pre-Phase-4
            // v1 had a single global `content_width` so two-tuple keys
            // were complete; the v2 port made `content_width` a
            // per-page parameter and a stale cache entry could
            // misalign margin boxes on pages with overridden margins
            // (PR #309 Devin).
            let measure_key = (
                html.clone(),
                width_key(page_size.height),
                width_key(content_width),
            );
            self.measure_cache.entry(measure_key).or_insert_with(|| {
                let env = margin_env(
                    &self.env,
                    &self.margin_css,
                    self.font_data,
                    self.system_fonts,
                    self.generics,
                );
                let measure_html = format!(
                    "<html><head>{}</head><body style=\"margin:0;padding:0;\"><div style=\"display:inline-block\">{}</div></body></html>",
                    margin_wrapper_style(env, &self.margin_css),
                    html
                );
                let measure_doc = crate::blitz_adapter::parse_and_layout_margin_doc(
                    env,
                    &measure_html,
                    content_width.as_pt().in_px(),
                    page_size.height.as_pt().in_px(),
                );
                get_body_child_dimension(&measure_doc, true)
            });
        }

        // Stage 1b: measure max-content height for left / right boxes.
        for (&pos, resolved) in &resolved_htmls {
            let fixed_width = match pos.edge() {
                Some(Edge::Left) => resolved_margin.left,
                Some(Edge::Right) => resolved_margin.right,
                _ => continue,
            };
            let html = &resolved.measure_html;
            // Include `page_size.height` in the key so a `@page :first`
            // (or other matched-page selector) that overrides the page
            // SIZE — not just margins — gets a fresh measurement on
            // the second page. Mirrors `measure_cache`'s key (which
            // already records `page_size.height`); without this, two
            // pages with the same fixed margin width but different
            // page heights would share a stale entry. (PR #309 Devin)
            let hc_key = (
                html.clone(),
                width_key(fixed_width),
                width_key(page_size.height),
            );
            self.height_cache.entry(hc_key).or_insert_with(|| {
                let env = margin_env(
                    &self.env,
                    &self.margin_css,
                    self.font_data,
                    self.system_fonts,
                    self.generics,
                );
                let measure_html = format!(
                    "<html><head>{}</head><body style=\"margin:0;padding:0;\"><div>{}</div></body></html>",
                    margin_wrapper_style(env, &self.margin_css),
                    html
                );
                let measure_doc = crate::blitz_adapter::parse_and_layout_margin_doc(
                    env,
                    &measure_html,
                    fixed_width.as_pt().in_px(),
                    page_size.height.as_pt().in_px(),
                );
                get_body_child_dimension(&measure_doc, false)
            });
        }

        // Stage 2: distribute each edge's boxes against the page rect.
        let mut edge_defined: BTreeMap<Edge, BTreeMap<MarginBoxPosition, crate::units::Pt>> =
            BTreeMap::new();
        for (&pos, resolved) in &resolved_htmls {
            let edge = match pos.edge() {
                Some(e) => e,
                None => continue,
            };
            let html = &resolved.measure_html;
            let size = if edge.is_horizontal() {
                self.measure_cache
                    .get(&(
                        html.clone(),
                        width_key(page_size.height),
                        width_key(content_width),
                    ))
                    .copied()
            } else {
                let fixed_width = if edge == Edge::Left {
                    resolved_margin.left
                } else {
                    resolved_margin.right
                };
                self.height_cache
                    .get(&(
                        html.clone(),
                        width_key(fixed_width),
                        width_key(page_size.height),
                    ))
                    .copied()
            };
            if let Some(s) = size {
                edge_defined.entry(edge).or_default().insert(pos, s);
            }
        }
        let mut all_rects: HashMap<MarginBoxPosition, MarginBoxRect> = HashMap::new();
        for (edge, defined) in &edge_defined {
            all_rects.extend(compute_edge_layout(
                *edge,
                defined,
                page_size,
                resolved_margin,
            ));
        }

        // Stage 3: render at the confirmed rect.
        for (&pos, resolved) in &resolved_htmls {
            let rect = all_rects
                .get(&pos)
                .copied()
                .unwrap_or_else(|| pos.bounding_rect(page_size, resolved_margin));
            // The content is aligned in the rectangle (F103); the position's defaults are part of the HTML, so
            // the cache key tells `@top-left` from `@top-right` with the same text and size.
            let html = margin_box_cell_html(pos, resolved, rect);

            let cache_key = (
                html.clone(),
                width_key(rect.width.to_f32()),
                width_key(rect.height.to_f32()),
            );
            if !self.render_cache.contains_key(&cache_key) {
                let env = margin_env(
                    &self.env,
                    &self.margin_css,
                    self.font_data,
                    self.system_fonts,
                    self.generics,
                );
                let render_html = format!(
                    "<html><head>{}</head><body style=\"margin:0;padding:0;\">{}</body></html>",
                    margin_wrapper_style(env, &self.margin_css),
                    html
                );
                let mut render_doc = crate::blitz_adapter::parse_and_layout_margin_doc(
                    env,
                    &render_html,
                    rect.width.in_px(),
                    rect.height.in_px(),
                );
                let empty_column_styles = crate::column_css::ColumnStyleTable::new();
                let geometry = crate::pagination_layout::run_pass_with_break_styles(
                    &mut render_doc,
                    rect.height.in_px(),
                    &empty_column_styles,
                );
                let dummy_store = RunningElementStore::new();
                let mut dummy_ctx = crate::convert::ConvertContext {
                    running_store: &dummy_store,
                    assets: None,
                    font_bytes: crate::convert::FontBytes::seeded(self.font_data),
                    string_set_by_node: HashMap::new(),
                    counter_ops_by_node: HashMap::new(),
                    bookmark_by_node: HashMap::new(),
                    column_styles: crate::column_css::ColumnStyleTable::new(),
                    multicol_geometry: crate::multicol_layout::MulticolGeometryTable::new(),
                    pagination_geometry: geometry,
                    link_cache: Default::default(),
                    dropped_text: Default::default(),
                    svg_checks: None,
                    viewport_size_px: None,
                };
                let drawables = crate::convert::dom_to_drawables(&render_doc, &mut dummy_ctx);
                let geometry = dummy_ctx.pagination_geometry;
                self.render_cache
                    .insert(cache_key.clone(), (drawables, geometry));
            }

            if let Some((drawables, geometry)) = self.render_cache.get(&cache_key) {
                if let Some(root_id) = drawables.root_id {
                    if let Some(root_block) = drawables.block_styles.get(&root_id) {
                        paint_root_block_v2(
                            canvas,
                            root_block,
                            rect.x.to_f32(),
                            rect.y.to_f32(),
                            None,
                            true,
                        );
                    }
                }
                // `body_offset_pt` is (0, 0) here because the wrapper HTML fixes
                // `body { margin: 0; padding: 0; }` via an inline style, which
                // takes higher specificity than `self.margin_css`. No adjustment
                // needed unlike `render_v2`.
                let (txd, cd, owd) = build_page_skip_sets(drawables);
                // Margin-box content is always single-page (page 0).
                let page_nodes: Vec<usize> = geometry
                    .iter()
                    .filter_map(|(&id, g)| {
                        g.fragments.iter().any(|f| f.page_index == 0).then_some(id)
                    })
                    .collect();
                draw_v2_page(
                    canvas,
                    0,
                    rect.x.to_f32(),
                    rect.y.to_f32(),
                    geometry,
                    drawables,
                    &txd,
                    &cd,
                    &owd,
                    &page_nodes,
                );
            }
        }
        // Body-link clip gate: `true` iff at least one effective margin
        // box on this page produced non-empty content, i.e. something
        // was actually painted on top of the body layer. When `false`,
        // the caller must NOT clip body annotations that landed in a
        // margin strip — nothing covers them, so dropping the click
        // target would leave visible body-drawn page furniture
        // unclickable.
        !resolved_htmls.is_empty()
    }
}

/// Get a layout dimension of the first non-zero child of `<body>` in a Blitz document.
/// When `use_width` is true, returns max-content width; otherwise returns height.
///
/// Returns a [`crate::units::Pt`]. Blitz's internal layout is in CSS px, so the
/// raw Taffy size is tagged `Px` and converted with `Px::in_pt` on the way out —
/// matching the convention used at the convert.rs boundary (`layout_in_pt`). This
/// keeps the GCPM margin-box measure caches in the same unit (pt) as `page_size` /
/// `margin`, which `compute_edge_layout` assumes when distributing along the edge.
fn get_body_child_dimension(doc: &blitz_html::HtmlDocument, use_width: bool) -> crate::units::Pt {
    use std::ops::Deref;
    let root = doc.root_element();
    let base_doc = doc.deref();

    let px: f32 = 'outer: {
        if let Some(root_node) = base_doc.get_node(root.id) {
            for &child_id in &root_node.children {
                if let Some(child) = base_doc.get_node(child_id) {
                    if let blitz_dom::NodeData::Element(elem) = &child.data {
                        if elem.name.local.as_ref() == "body" {
                            for &body_child_id in &child.children {
                                if let Some(body_child) = base_doc.get_node(body_child_id) {
                                    let size = &body_child.final_layout.size;
                                    let v = if use_width { size.width } else { size.height };
                                    if v > 0.0 {
                                        break 'outer v;
                                    }
                                }
                            }
                            let size = &child.final_layout.size;
                            break 'outer if use_width { size.width } else { size.height };
                        }
                    }
                }
            }
        }
        0.0
    };
    px.as_px().in_pt()
}

/// Escape a string for use in an HTML attribute value.
fn escape_attr(s: &str) -> String {
    s.replace('&', "&amp;")
        .replace('"', "&quot;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
}

/// Strip `display: none` declarations from CSS.
/// Used to build margin-box CSS where running elements need to be visible.
fn strip_display_none(css: &str) -> String {
    css.replace("display: none", "").replace("display:none", "")
}

/// Build a hierarchical [`TagTree`] from [`TagCollector`] entries and
/// the `semantics` map in [`Drawables`].
///
/// The flat approach used before fulgur-izp.5 created one top-level
/// [`TagGroup`] per tagged NodeId. This function instead uses
/// [`crate::tagging::SemanticEntry::parent`] to nest groups, so that
/// `<section><h1>…</h1></section>` produces a Div group containing an
/// Hn group rather than two sibling groups.
fn build_struct_tree(
    tc: crate::draw_primitives::TagCollector,
    drawables: &Drawables,
    link_annot_ids: &BTreeMap<usize, Vec<Identifier>>,
    tree: &mut TagTree,
) {
    let mut identifiers: BTreeMap<crate::drawables::NodeId, Vec<Identifier>> = BTreeMap::new();
    let mut heading_titles: BTreeMap<crate::drawables::NodeId, String> = BTreeMap::new();
    // `into_parts` returns `entries` and `run_entries` together so neither
    // field needs to be borrowed before the other is moved.
    let (tc_entries, run_entries) = tc.into_parts();
    for (node_id, _tag, id, heading_title) in tc_entries {
        identifiers.entry(node_id).or_default().push(id);
        if let Some(title) = heading_title {
            heading_titles.entry(node_id).or_insert(title);
        }
    }
    // Nodes that use per-run tagging (run_entries) bypass try_start_tagged and
    // therefore never record a heading_title through tc_entries. Backfill their
    // titles here so <h1><a href>…</a></h1> still gets the /T attribute.
    // `heading_title_of` filters out invisible headings so the tag tree does
    // not leak `visibility: hidden` content that the paint path suppressed.
    for &node_id in run_entries.keys() {
        if heading_titles.contains_key(&node_id) {
            continue;
        }
        if let Some(entry) = drawables.semantics.get(&node_id) {
            if matches!(entry.tag, crate::tagging::PdfTag::H { .. }) {
                if let Some(title) = drawables
                    .paragraphs
                    .get(&node_id)
                    .and_then(heading_title_of)
                {
                    heading_titles.insert(node_id, title);
                }
            }
        }
    }

    let mut children_map: BTreeMap<crate::drawables::NodeId, Vec<crate::drawables::NodeId>> =
        BTreeMap::new();
    for (&node_id, entry) in &drawables.semantics {
        if let Some(parent_id) = entry.parent {
            children_map.entry(parent_id).or_default().push(node_id);
        }
    }

    let roots: Vec<crate::drawables::NodeId> = drawables
        .semantics
        .iter()
        .filter(|(_, e)| e.parent.is_none())
        .map(|(&id, _)| id)
        .collect();

    let mut attached_links = std::collections::BTreeSet::new();
    for root_id in roots {
        let group = build_tag_group(
            root_id,
            drawables,
            &identifiers,
            &heading_titles,
            &children_map,
            &run_entries,
            link_annot_ids,
            &mut attached_links,
        );
        tree.push(Node::Group(group));
    }
}

#[allow(clippy::too_many_arguments)] // one recursive walk over seven read-only maps and the set of attached links
fn build_tag_group(
    node_id: crate::drawables::NodeId,
    drawables: &Drawables,
    identifiers: &BTreeMap<crate::drawables::NodeId, Vec<Identifier>>,
    heading_titles: &BTreeMap<crate::drawables::NodeId, String>,
    children_map: &BTreeMap<crate::drawables::NodeId, Vec<crate::drawables::NodeId>>,
    run_entries: &BTreeMap<crate::drawables::NodeId, Vec<crate::draw_primitives::ParagraphRunItem>>,
    link_annot_ids: &BTreeMap<usize, Vec<Identifier>>,
    attached_links: &mut std::collections::BTreeSet<usize>,
) -> TagGroup {
    let entry = &drawables.semantics[&node_id]; // invariant: node_id always derived from drawables.semantics
    let title = heading_titles.get(&node_id).cloned();
    let mut group = TagGroup::new(crate::tagging::pdf_tag_to_krilla_tag(
        &entry.tag,
        title,
        entry.alt_text.clone(),
    ));

    // Paragraphs with per-run link tagging use run_entries instead of the
    // single identifiers path.
    if let Some(run_items) = run_entries.get(&node_id) {
        let mut i = 0;
        while i < run_items.len() {
            use crate::draw_primitives::ParagraphRunItem;
            match &run_items[i] {
                ParagraphRunItem::Content(id) => {
                    group.push(Node::Leaf(*id));
                    i += 1;
                }
                ParagraphRunItem::LinkContent { span_ptr, .. } => {
                    let ptr = *span_ptr;
                    let mut link_group = TagGroup::new(crate::tagging::pdf_tag_to_krilla_tag(
                        &crate::tagging::PdfTag::Link,
                        None,
                        None,
                    ));
                    // Collect all consecutive items with the same span_ptr.
                    while let Some(ParagraphRunItem::LinkContent {
                        span_ptr: p,
                        identifier: id,
                    }) = run_items.get(i)
                        && *p == ptr
                    {
                        link_group.push(Node::Leaf(*id));
                        i += 1;
                    }
                    // Annotation identifiers (OBJR) follow content identifiers.
                    // A cross-page link produces one identifier per page.
                    // Every annotation has exactly one parent in the tag tree
                    // (ISO 32000-1 §14.7.4.3, PDF/UA-1 7.18.5), and one link
                    // can span several paragraphs (`<a><p>A</p><div>B</div></a>`,
                    // or an `<a>` with `display: contents` around flex items):
                    // the first `/Link` group that holds the link takes its
                    // annotations, the later ones hold their content only.
                    if attached_links.insert(ptr)
                        && let Some(annot_ids) = link_annot_ids.get(&ptr)
                    {
                        for &annot_id in annot_ids {
                            link_group.push(Node::Leaf(annot_id));
                        }
                    }
                    group.push(Node::Group(link_group));
                }
            }
        }
    } else if let Some(ids) = identifiers.get(&node_id) {
        for &id in ids {
            group.push(Node::Leaf(id));
        }
    }

    if let Some(children) = children_map.get(&node_id) {
        for &child_id in children {
            let child = build_tag_group(
                child_id,
                drawables,
                identifiers,
                heading_titles,
                children_map,
                run_entries,
                link_annot_ids,
                attached_links,
            );
            group.push(Node::Group(child));
        }
    }

    group
}

#[cfg(test)]
mod tests {
    #[test]
    fn content_box_pt_may_be_empty() {
        use crate::config::{Margin, PageSize};
        let (w, h) = content_box_pt(PageSize::LETTER, Margin::uniform(72.0));
        assert_eq!((w, h), (468.0, 648.0));
        // Margins larger than the page leave a negative box, which the caller reports.
        let (w, h) = content_box_pt(PageSize::LETTER, Margin::uniform(400.0));
        assert!(w < 0.0 && h < 0.0);
    }

    use super::*;

    // --- escape_attr ---

    #[test]
    fn escape_attr_no_special_chars() {
        assert_eq!(escape_attr("plain text"), "plain text");
    }

    #[test]
    fn escape_attr_ampersand() {
        assert_eq!(escape_attr("foo&bar"), "foo&amp;bar");
    }

    #[test]
    fn escape_attr_double_quote() {
        assert_eq!(escape_attr(r#"foo"bar"#), "foo&quot;bar");
    }

    #[test]
    fn escape_attr_less_than() {
        assert_eq!(escape_attr("foo<bar"), "foo&lt;bar");
    }

    #[test]
    fn escape_attr_greater_than() {
        assert_eq!(escape_attr("foo>bar"), "foo&gt;bar");
    }

    #[test]
    fn escape_attr_all_specials_combined() {
        assert_eq!(
            escape_attr(r#"<"a" & "b">"#),
            "&lt;&quot;a&quot; &amp; &quot;b&quot;&gt;"
        );
    }

    #[test]
    fn escape_attr_empty_string() {
        assert_eq!(escape_attr(""), "");
    }

    // --- strip_display_none ---

    #[test]
    fn strip_display_none_spaced_variant() {
        let css = ".x { display: none; color: red; }";
        let result = strip_display_none(css);
        assert!(
            !result.contains("display: none"),
            "should remove 'display: none'"
        );
        assert!(
            result.contains("color: red"),
            "should preserve other properties"
        );
    }

    #[test]
    fn strip_display_none_unspaced_variant() {
        let css = ".x { display:none; margin: 0; }";
        let result = strip_display_none(css);
        assert!(
            !result.contains("display:none"),
            "should remove 'display:none'"
        );
        assert!(
            result.contains("margin: 0"),
            "should preserve other properties"
        );
    }

    #[test]
    fn strip_display_none_no_match_is_noop() {
        let css = "body { color: blue; }";
        assert_eq!(strip_display_none(css), css);
    }

    #[test]
    fn strip_display_none_both_variants_in_same_string() {
        let css = "a { display: none; } b { display:none; }";
        let result = strip_display_none(css);
        assert!(!result.contains("display: none"));
        assert!(!result.contains("display:none"));
    }

    // --- width_key ---

    #[test]
    fn width_key_matches_to_bits() {
        let w = 42.5_f32;
        assert_eq!(width_key(w), w.to_bits());
    }

    #[test]
    fn width_key_distinct_values_differ() {
        assert_ne!(width_key(1.0), width_key(2.0));
    }

    #[test]
    fn width_key_zero() {
        assert_eq!(width_key(0.0_f32), 0_f32.to_bits());
    }

    // --- parse_datetime ---

    #[test]
    fn parse_datetime_valid_year_only() {
        assert!(parse_datetime("2024").is_some());
    }

    #[test]
    fn parse_datetime_valid_year_month() {
        assert!(parse_datetime("2024-06").is_some());
    }

    #[test]
    fn parse_datetime_valid_year_month_day() {
        assert!(parse_datetime("2024-06-15").is_some());
    }

    #[test]
    fn parse_datetime_valid_full_datetime() {
        assert!(parse_datetime("2024-06-15T10:30:45").is_some());
    }

    #[test]
    fn parse_datetime_valid_full_datetime_with_z() {
        assert!(parse_datetime("2024-06-15T10:30:45Z").is_some());
    }

    #[test]
    fn parse_datetime_valid_midnight() {
        assert!(parse_datetime("2024-01-01T00:00:00").is_some());
    }

    #[test]
    fn parse_datetime_valid_hour_only_in_time() {
        // only hour field present in time part → still valid
        assert!(parse_datetime("2024-01-01T12").is_some());
    }

    #[test]
    fn parse_datetime_valid_hour_minute_in_time() {
        assert!(parse_datetime("2024-01-01T12:30").is_some());
    }

    #[test]
    fn parse_datetime_invalid_empty_string() {
        assert!(parse_datetime("").is_none());
    }

    #[test]
    fn parse_datetime_invalid_non_numeric_year() {
        assert!(parse_datetime("abcd").is_none());
    }

    #[test]
    fn parse_datetime_invalid_non_numeric_month() {
        assert!(parse_datetime("2024-ab").is_none());
    }

    #[test]
    fn parse_datetime_invalid_non_numeric_day() {
        assert!(parse_datetime("2024-06-ab").is_none());
    }

    #[test]
    fn parse_datetime_invalid_non_numeric_hour() {
        assert!(parse_datetime("2024-06-15Tabc:30:45").is_none());
    }

    #[test]
    fn parse_datetime_invalid_non_numeric_minute() {
        assert!(parse_datetime("2024-06-15T10:abc:45").is_none());
    }

    #[test]
    fn parse_datetime_invalid_non_numeric_second() {
        assert!(parse_datetime("2024-06-15T10:30:abc").is_none());
    }

    // --- build_tag_group: LinkContent branch ---

    use crate::draw_primitives::run_tag_tests::make_identifier;

    /// Build a minimal [`Drawables`] with a single semantic paragraph node.
    fn drawables_with_para(node_id: crate::drawables::NodeId) -> Drawables {
        let mut d = Drawables::new();
        d.semantics.insert(
            node_id,
            crate::tagging::SemanticEntry {
                tag: crate::tagging::PdfTag::P,
                parent: None,
                alt_text: None,
            },
        );
        d
    }

    #[test]
    fn build_tag_group_link_content_creates_nested_link_group() {
        // Paragraph node_id = 1 has one non-link run and one link run (ptr=99).
        let node_id: crate::drawables::NodeId = 1;
        let content_id = make_identifier();
        let link_id = make_identifier();
        let annot_id = make_identifier();

        let mut run_entries: BTreeMap<
            crate::drawables::NodeId,
            Vec<crate::draw_primitives::ParagraphRunItem>,
        > = BTreeMap::new();
        run_entries.insert(
            node_id,
            vec![
                crate::draw_primitives::ParagraphRunItem::Content(content_id),
                crate::draw_primitives::ParagraphRunItem::LinkContent {
                    span_ptr: 99,
                    identifier: link_id,
                },
            ],
        );

        let mut link_annot_ids: BTreeMap<usize, Vec<Identifier>> = BTreeMap::new();
        link_annot_ids.entry(99).or_default().push(annot_id);

        let drawables = drawables_with_para(node_id);
        let identifiers: BTreeMap<crate::drawables::NodeId, Vec<Identifier>> = BTreeMap::new();
        let heading_titles: BTreeMap<crate::drawables::NodeId, String> = BTreeMap::new();
        let children_map: BTreeMap<crate::drawables::NodeId, Vec<crate::drawables::NodeId>> =
            BTreeMap::new();

        let group = build_tag_group(
            node_id,
            &drawables,
            &identifiers,
            &heading_titles,
            &children_map,
            &run_entries,
            &link_annot_ids,
            &mut std::collections::BTreeSet::new(),
        );

        // The group should have two children: one Leaf (Content) and one Group (Link).
        assert_eq!(group.children.len(), 2, "expected Leaf + Group(Link)");
        // Second child should be a Group (the Link TagGroup).
        assert!(
            matches!(group.children[1], Node::Group(_)),
            "second child should be a Link Group"
        );
    }

    #[test]
    fn build_tag_group_attaches_a_links_annotations_to_one_group_only() {
        // One link (ptr 55) with text in two paragraphs: the annotation identifier may have only one parent
        // in the tag tree, so only the first paragraph's `/Link` group holds it.
        let (a, b): (crate::drawables::NodeId, crate::drawables::NodeId) = (3, 4);
        let (id_a, id_b, annot) = (make_identifier(), make_identifier(), make_identifier());
        let mut run_entries: BTreeMap<
            crate::drawables::NodeId,
            Vec<crate::draw_primitives::ParagraphRunItem>,
        > = BTreeMap::new();
        for (node, id) in [(a, id_a), (b, id_b)] {
            run_entries.insert(
                node,
                vec![crate::draw_primitives::ParagraphRunItem::LinkContent {
                    span_ptr: 55,
                    identifier: id,
                }],
            );
        }
        let mut link_annot_ids: BTreeMap<usize, Vec<Identifier>> = BTreeMap::new();
        link_annot_ids.entry(55).or_default().push(annot);
        let mut drawables = drawables_with_para(a);
        drawables.semantics.insert(
            b,
            crate::tagging::SemanticEntry {
                tag: crate::tagging::PdfTag::P,
                parent: None,
                alt_text: None,
            },
        );
        let identifiers = BTreeMap::new();
        let heading_titles = BTreeMap::new();
        let children_map = BTreeMap::new();
        let mut attached = std::collections::BTreeSet::new();
        let mut build = |node| {
            build_tag_group(
                node,
                &drawables,
                &identifiers,
                &heading_titles,
                &children_map,
                &run_entries,
                &link_annot_ids,
                &mut attached,
            )
        };
        let (first, second) = (build(a), build(b));
        let link_children = |g: &TagGroup| match &g.children[0] {
            Node::Group(l) => l.children.len(),
            _ => panic!("expected a Link group"),
        };
        assert_eq!(link_children(&first), 2, "content + annotation");
        assert_eq!(link_children(&second), 1, "content only");
    }

    #[test]
    fn build_tag_group_link_content_no_annot_id_still_builds_group() {
        // Same as above but no annotation identifier in link_annot_ids.
        let node_id: crate::drawables::NodeId = 2;
        let link_id = make_identifier();

        let mut run_entries: BTreeMap<
            crate::drawables::NodeId,
            Vec<crate::draw_primitives::ParagraphRunItem>,
        > = BTreeMap::new();
        run_entries.insert(
            node_id,
            vec![crate::draw_primitives::ParagraphRunItem::LinkContent {
                span_ptr: 77,
                identifier: link_id,
            }],
        );

        let link_annot_ids: BTreeMap<usize, Vec<Identifier>> = BTreeMap::new(); // empty — no OBJR

        let drawables = drawables_with_para(node_id);
        let identifiers: BTreeMap<crate::drawables::NodeId, Vec<Identifier>> = BTreeMap::new();
        let heading_titles: BTreeMap<crate::drawables::NodeId, String> = BTreeMap::new();
        let children_map: BTreeMap<crate::drawables::NodeId, Vec<crate::drawables::NodeId>> =
            BTreeMap::new();

        let group = build_tag_group(
            node_id,
            &drawables,
            &identifiers,
            &heading_titles,
            &children_map,
            &run_entries,
            &link_annot_ids,
            &mut std::collections::BTreeSet::new(),
        );

        assert_eq!(
            group.children.len(),
            1,
            "link without OBJR should still produce one Link Group"
        );
        assert!(
            matches!(group.children[0], Node::Group(_)),
            "child should be a Link Group"
        );
    }

    // --- para_has_link_runs ---

    fn make_glyph_run(
        text: &str,
        link: Option<std::sync::Arc<crate::paragraph::LinkSpan>>,
    ) -> crate::paragraph::ShapedGlyphRun {
        crate::paragraph::ShapedGlyphRun {
            font_data: std::sync::Arc::new(vec![]),
            font_index: 0,
            font_size: 12.0_f32.as_pt(),
            color: [0, 0, 0, 255],
            decoration: crate::paragraph::TextDecoration::default(),
            glyphs: vec![],
            text: std::sync::Arc::from(text),
            x_offset: crate::units::Pt::ZERO,
            link,
        }
    }

    fn make_shaped_line(items: Vec<crate::paragraph::LineItem>) -> crate::paragraph::ShapedLine {
        crate::paragraph::ShapedLine {
            height: 16.0_f32.as_pt(),
            baseline: 12.0_f32.as_pt(),
            items,
        }
    }

    fn make_para(lines: Vec<crate::paragraph::ShapedLine>) -> crate::drawables::ParagraphEntry {
        crate::drawables::ParagraphEntry {
            lines,
            opacity: 1.0,
            visible: true,
            id: None,
        }
    }

    fn make_hidden_para(
        lines: Vec<crate::paragraph::ShapedLine>,
    ) -> crate::drawables::ParagraphEntry {
        crate::drawables::ParagraphEntry {
            lines,
            opacity: 1.0,
            visible: false,
            id: None,
        }
    }

    #[test]
    fn para_has_link_runs_empty_paragraph_returns_false() {
        let para = make_para(vec![]);
        assert!(!para_has_link_runs(&para));
    }

    #[test]
    fn para_has_link_runs_text_without_link_returns_false() {
        let run = make_glyph_run("hello", None);
        let line = make_shaped_line(vec![crate::paragraph::LineItem::Text(run)]);
        let para = make_para(vec![line]);
        assert!(!para_has_link_runs(&para));
    }

    #[test]
    fn para_has_link_runs_text_with_link_returns_true() {
        let span = std::sync::Arc::new(crate::paragraph::LinkSpan {
            target: crate::paragraph::LinkTarget::External(std::sync::Arc::new(
                "https://example.com".to_string(),
            )),
            alt_text: None,
        });
        let run = make_glyph_run("click me", Some(span));
        let line = make_shaped_line(vec![crate::paragraph::LineItem::Text(run)]);
        let para = make_para(vec![line]);
        assert!(para_has_link_runs(&para));
    }

    #[test]
    fn para_has_link_runs_inline_box_returns_false() {
        let item = crate::paragraph::InlineBoxItem {
            node_id: None,
            width: 10.0_f32.as_pt(),
            height: 10.0_f32.as_pt(),
            x_offset: crate::units::Pt::ZERO,
            computed_y: crate::units::Pt::ZERO,
            link: None,
            opacity: 1.0,
            visible: true,
        };
        let line = make_shaped_line(vec![crate::paragraph::LineItem::InlineBox(item)]);
        let para = make_para(vec![line]);
        assert!(!para_has_link_runs(&para));
    }

    /// Build a paragraph carrying one inline box with the given node id.
    fn para_with_inline_box(node_id: Option<usize>) -> crate::drawables::ParagraphEntry {
        let item = crate::paragraph::InlineBoxItem {
            node_id,
            width: 10.0_f32.as_pt(),
            height: 10.0_f32.as_pt(),
            x_offset: crate::units::Pt::ZERO,
            computed_y: crate::units::Pt::ZERO,
            link: None,
            opacity: 1.0,
            visible: true,
        };
        make_para(vec![make_shaped_line(vec![
            crate::paragraph::LineItem::InlineBox(item),
        ])])
    }

    #[test]
    fn paragraph_owned_inline_boxes_expands_the_scope_owner_paragraph() {
        let mut d = Drawables::new();
        // Scope owner 1 owns a paragraph carrying inline box 2, whose
        // subtree is {3, 4}.
        d.paragraphs.insert(1, para_with_inline_box(Some(2)));
        d.inline_box_subtree_descendants.insert(2, vec![3, 4]);

        let skip = paragraph_owned_inline_boxes([1, 3, 4], &d);
        assert_eq!(
            skip,
            [2, 3, 4]
                .into_iter()
                .collect::<std::collections::BTreeSet<_>>(),
            "the box and its recorded subtree are painted by the paragraph",
        );
    }

    #[test]
    fn paragraph_owned_inline_boxes_expands_descendant_paragraphs() {
        let mut d = Drawables::new();
        // The scope owner (1) has no paragraph; a descendant (5) does. A
        // table is exactly this shape — the duplicate originates in a cell.
        d.paragraphs.insert(5, para_with_inline_box(Some(6)));
        d.inline_box_subtree_descendants.insert(6, vec![7]);

        let skip = paragraph_owned_inline_boxes([1, 5, 6, 7], &d);
        assert_eq!(
            skip,
            [6, 7]
                .into_iter()
                .collect::<std::collections::BTreeSet<_>>(),
        );
    }

    #[test]
    fn paragraph_owned_inline_boxes_ignores_ids_without_a_paragraph() {
        let mut d = Drawables::new();
        // 9 is a plain block drawable, not a paragraph — nothing to skip.
        // This is the block-child case: the id is in the scope's descendant
        // list but no paragraph paints it, so the walk must still dispatch
        // it. Skipping it here is what PR #677 got wrong (fulgur-49xw).
        d.inline_box_subtree_descendants.insert(8, vec![9]);

        let skip = paragraph_owned_inline_boxes([8, 9], &d);
        assert!(
            skip.is_empty(),
            "no paragraph in scope means nothing is paragraph-owned, got {skip:?}",
        );
    }

    #[test]
    fn paragraph_owned_inline_boxes_skips_boxes_without_a_node_id() {
        let mut d = Drawables::new();
        d.paragraphs.insert(1, para_with_inline_box(None));

        let skip = paragraph_owned_inline_boxes([1], &d);
        assert!(
            skip.is_empty(),
            "an inline box with no node id has nothing to skip"
        );
    }

    #[test]
    fn paragraph_owned_inline_boxes_handles_a_box_with_no_recorded_subtree() {
        let mut d = Drawables::new();
        // `inline_box_subtree_descendants` has no entry for 2 — a leaf
        // inline box. The box itself is still paragraph-owned.
        d.paragraphs.insert(1, para_with_inline_box(Some(2)));

        let skip = paragraph_owned_inline_boxes([1], &d);
        assert_eq!(
            skip,
            [2].into_iter().collect::<std::collections::BTreeSet<_>>(),
        );
    }

    #[test]
    fn para_has_link_runs_image_with_link_returns_true() {
        let span = std::sync::Arc::new(crate::paragraph::LinkSpan {
            target: crate::paragraph::LinkTarget::Internal(std::sync::Arc::new(
                "section".to_string(),
            )),
            alt_text: None,
        });
        let img = crate::paragraph::InlineImage {
            data: std::sync::Arc::new(vec![]),
            format: crate::image::ImageFormat::Png,
            width: 20.0_f32.as_pt(),
            height: 20.0_f32.as_pt(),
            x_offset: crate::units::Pt::ZERO,
            vertical_align: crate::paragraph::VerticalAlign::Baseline,
            opacity: 1.0,
            visible: true,
            computed_y: crate::units::Pt::ZERO,
            link: Some(span),
        };
        let line = make_shaped_line(vec![crate::paragraph::LineItem::Image(img)]);
        let para = make_para(vec![line]);
        assert!(para_has_link_runs(&para));
    }

    #[test]
    fn para_has_link_runs_image_without_link_returns_false() {
        let img = crate::paragraph::InlineImage {
            data: std::sync::Arc::new(vec![]),
            format: crate::image::ImageFormat::Png,
            width: 10.0_f32.as_pt(),
            height: 10.0_f32.as_pt(),
            x_offset: crate::units::Pt::ZERO,
            vertical_align: crate::paragraph::VerticalAlign::Baseline,
            opacity: 1.0,
            visible: true,
            computed_y: crate::units::Pt::ZERO,
            link: None,
        };
        let line = make_shaped_line(vec![crate::paragraph::LineItem::Image(img)]);
        let para = make_para(vec![line]);
        assert!(!para_has_link_runs(&para));
    }

    #[test]
    fn para_has_link_runs_link_in_second_line_returns_true() {
        let plain_line = make_shaped_line(vec![crate::paragraph::LineItem::Text(make_glyph_run(
            "first", None,
        ))]);
        let span = std::sync::Arc::new(crate::paragraph::LinkSpan {
            target: crate::paragraph::LinkTarget::External(std::sync::Arc::new(
                "https://example.com".to_string(),
            )),
            alt_text: None,
        });
        let link_line = make_shaped_line(vec![crate::paragraph::LineItem::Text(make_glyph_run(
            "second",
            Some(span),
        ))]);
        let para = make_para(vec![plain_line, link_line]);
        assert!(para_has_link_runs(&para));
    }

    // --- run_tag_target ---

    #[test]
    fn run_tag_target_li_lbody_ids_hit_returns_lbody_id() {
        // When li_lbody_ids contains node_id, the synthetic LBody id is returned
        // (which differs from the input node_id).
        let mut d = Drawables::new();
        let li_id: crate::drawables::NodeId = 10;
        let lbody_id: crate::drawables::NodeId = 9999;
        d.li_lbody_ids.insert(li_id, lbody_id);
        assert_eq!(run_tag_target(&d, li_id), Some(lbody_id));
    }

    #[test]
    fn run_tag_target_li_lbody_ids_returns_mapped_not_input_id() {
        // The returned id is the LBody synthetic id, NOT the li input id.
        let mut d = Drawables::new();
        let li_id: crate::drawables::NodeId = 20;
        let lbody_id: crate::drawables::NodeId = 8888;
        d.li_lbody_ids.insert(li_id, lbody_id);
        let result = run_tag_target(&d, li_id);
        assert_ne!(result, Some(li_id));
        assert_eq!(result, Some(lbody_id));
    }

    #[test]
    fn run_tag_target_semantics_hit_returns_node_id() {
        // When only semantics contains node_id, returns Some(node_id) unchanged.
        let node_id: crate::drawables::NodeId = 5;
        let d = drawables_with_para(node_id);
        assert_eq!(run_tag_target(&d, node_id), Some(node_id));
    }

    #[test]
    fn run_tag_target_neither_map_returns_none() {
        // A node absent from both li_lbody_ids and semantics yields None.
        let d = Drawables::new();
        assert_eq!(run_tag_target(&d, 42), None);
    }

    #[test]
    fn run_tag_target_li_lbody_ids_takes_priority_over_semantics() {
        // When node_id is in both maps, li_lbody_ids wins and lbody_id is returned.
        let mut d = Drawables::new();
        let node_id: crate::drawables::NodeId = 7;
        let lbody_id: crate::drawables::NodeId = 7777;
        d.li_lbody_ids.insert(node_id, lbody_id);
        d.semantics.insert(
            node_id,
            crate::tagging::SemanticEntry {
                tag: crate::tagging::PdfTag::Li,
                parent: None,
                alt_text: None,
            },
        );
        assert_eq!(run_tag_target(&d, node_id), Some(lbody_id));
    }

    // --- extract_heading_title ---

    #[test]
    fn extract_heading_title_empty_paragraph_returns_empty() {
        let para = make_para(vec![]);
        assert_eq!(extract_heading_title(&para), "");
    }

    #[test]
    fn extract_heading_title_single_text_run() {
        let line = make_shaped_line(vec![crate::paragraph::LineItem::Text(make_glyph_run(
            "Hello", None,
        ))]);
        let para = make_para(vec![line]);
        assert_eq!(extract_heading_title(&para), "Hello");
    }

    #[test]
    fn extract_heading_title_multiple_runs_across_lines() {
        let line1 = make_shaped_line(vec![crate::paragraph::LineItem::Text(make_glyph_run(
            "Foo", None,
        ))]);
        let line2 = make_shaped_line(vec![crate::paragraph::LineItem::Text(make_glyph_run(
            "Bar", None,
        ))]);
        let para = make_para(vec![line1, line2]);
        assert_eq!(extract_heading_title(&para), "FooBar");
    }

    #[test]
    fn extract_heading_title_skips_image_and_inline_box() {
        let img = crate::paragraph::InlineImage {
            data: std::sync::Arc::new(vec![]),
            format: crate::image::ImageFormat::Png,
            width: 10.0_f32.as_pt(),
            height: 10.0_f32.as_pt(),
            x_offset: crate::units::Pt::ZERO,
            vertical_align: crate::paragraph::VerticalAlign::Baseline,
            opacity: 1.0,
            visible: true,
            computed_y: crate::units::Pt::ZERO,
            link: None,
        };
        let inline_box = crate::paragraph::InlineBoxItem {
            node_id: None,
            width: 10.0_f32.as_pt(),
            height: 10.0_f32.as_pt(),
            x_offset: crate::units::Pt::ZERO,
            computed_y: crate::units::Pt::ZERO,
            link: None,
            opacity: 1.0,
            visible: true,
        };
        let line = make_shaped_line(vec![
            crate::paragraph::LineItem::Image(img),
            crate::paragraph::LineItem::Text(make_glyph_run("text", None)),
            crate::paragraph::LineItem::InlineBox(inline_box),
        ]);
        let para = make_para(vec![line]);
        assert_eq!(extract_heading_title(&para), "text");
    }

    // --- heading_title_of ---
    //
    // `heading_title_of` gates `extract_heading_title` on
    // `ParagraphEntry.visible` and drops zero-length titles. It is the
    // single choke point shared by `try_start_tagged` (H arm) and
    // `build_struct_tree`'s per-run backfill, so a `None` here means the
    // heading `/T` attribute is never emitted for that paragraph.

    #[test]
    fn heading_title_of_visible_paragraph_with_text_returns_title() {
        // Sanity: the visible-with-text path still forwards a title.
        let line = make_shaped_line(vec![crate::paragraph::LineItem::Text(make_glyph_run(
            "Chapter", None,
        ))]);
        let para = make_para(vec![line]);
        assert_eq!(heading_title_of(&para), Some("Chapter".to_string()));
    }

    #[test]
    fn heading_title_of_visible_empty_paragraph_returns_none() {
        // An empty paragraph would have produced a zero-length `/T`
        // before; the helper drops that in a single place so both call
        // sites see identical behaviour.
        let para = make_para(vec![]);
        assert_eq!(heading_title_of(&para), None);
    }

    #[test]
    fn heading_title_of_hidden_paragraph_returns_none() {
        // Core security regression guard: a `visibility: hidden` /
        // `collapse` paragraph must not surface its text through the tag
        // tree even though `extract_heading_title` would happily collect
        // it. Without this gate the text would end up in `Tag::Hn`'s
        // `/T` attribute for both the normal and per-run backfill sinks
        // while the paint path skips the same paragraph, leaking hidden
        // content to any tool that reads the struct tree.
        let line = make_shaped_line(vec![crate::paragraph::LineItem::Text(make_glyph_run(
            "hidden secret",
            None,
        ))]);
        let para = make_hidden_para(vec![line]);
        assert_eq!(heading_title_of(&para), None);
    }

    #[test]
    fn heading_title_of_hidden_empty_paragraph_returns_none() {
        // The visibility gate short-circuits before the text collection,
        // so an empty-and-hidden paragraph is `None` too — same result
        // as the visible-empty case, exercised via the other branch.
        let para = make_hidden_para(vec![]);
        assert_eq!(heading_title_of(&para), None);
    }

    // --- paragraph_lines_for_page ---

    fn make_fragment(page_index: u32, height: f32) -> crate::pagination_layout::Fragment {
        crate::pagination_layout::Fragment {
            page_index,
            x: 0.0_f32.as_px(),
            y: 0.0_f32.as_px(),
            width: 100.0_f32.as_px(),
            height: height.as_px(),
        }
    }

    fn make_line(height_pt: f32, baseline_pt: f32) -> crate::paragraph::ShapedLine {
        crate::paragraph::ShapedLine {
            height: height_pt.as_pt(),
            baseline: baseline_pt.as_pt(),
            items: vec![],
        }
    }

    fn geometry_of(
        heights: &[f32],
        insets: Option<(f32, f32)>,
    ) -> crate::pagination_layout::PaginationGeometry {
        crate::pagination_layout::PaginationGeometry {
            fragments: heights
                .iter()
                .enumerate()
                .map(|(i, h)| make_fragment(i as u32, *h))
                .collect(),
            is_repeat: false,
            line_split_insets: insets.map(|(t, b)| (t.as_px(), b.as_px())),
            ..Default::default()
        }
    }

    #[test]
    fn frame_fragment_adds_the_top_inset_everywhere_and_the_bottom_on_the_last_page() {
        let geom = geometry_of(&[100.0, 100.0, 40.0], Some((17.0, 5.0)));
        let heights: Vec<f32> = geom
            .fragments
            .iter()
            .map(|f| frame_fragment(&geom, f).0.height.to_f32())
            .collect();
        assert_eq!(heights, vec![117.0, 117.0, 62.0]);
        // A closed frame has no open edge.
        for f in &geom.fragments {
            assert!(!frame_fragment(&geom, f).1.any());
        }
    }

    #[test]
    fn frame_fragment_of_a_sliced_paragraph_opens_the_broken_edges() {
        // html-to-pdf F30f-d: three fragments of a box with 17px above and 5px below its lines, sliced at its breaks.
        // The fragmenter extended the first two to the page bottom (100px frames: 83 + 17 and 100). The first has the top
        // inset and an open bottom, the middle one is open on both sides, the last has the bottom inset and an open top.
        let mut geom = geometry_of(&[83.0, 100.0, 40.0], Some((17.0, 5.0)));
        geom.line_split_slice = true;
        let got: Vec<(f32, bool, bool)> = geom
            .fragments
            .iter()
            .map(|f| {
                let (frame, edges) = frame_fragment(&geom, f);
                (frame.height.to_f32(), edges.top_open, edges.bottom_open)
            })
            .collect();
        assert_eq!(
            got,
            vec![
                (100.0, false, true),
                (100.0, true, true),
                (45.0, true, false)
            ]
        );
    }

    #[test]
    fn sliced_style_zeroes_the_open_edges_only() {
        use crate::draw_primitives::{BlockStyle, FrameEdges, sliced_style};
        use crate::units::{F32Units, Pt};
        let style = BlockStyle {
            border_widths: [1.0_f32.as_pt(); 4],
            padding: [12.0_f32.as_pt(); 4],
            border_radii: [[6.0_f32.as_pt(); 2]; 4],
            ..Default::default()
        };
        let closed = sliced_style(&style, FrameEdges::default());
        assert!(matches!(closed, std::borrow::Cow::Borrowed(_)));
        let open = sliced_style(
            &style,
            FrameEdges {
                top_open: true,
                bottom_open: false,
            },
        );
        assert_eq!(open.border_widths[0], Pt::ZERO);
        assert_eq!(open.padding[0], Pt::ZERO);
        // The painters of the border and the shadow read which edges are cut.
        assert_eq!(
            open.open_edges,
            FrameEdges {
                top_open: true,
                bottom_open: false
            }
        );
        assert_eq!(closed.open_edges, FrameEdges::default());
        assert_eq!(open.content_inset(), (13.0, 0.0));
        assert_eq!(open.border_radii[0], [Pt::ZERO; 2]);
        assert_eq!(open.border_radii[1], [Pt::ZERO; 2]);
        assert_eq!(open.border_radii[2], style.border_radii[2]);
        assert_eq!(open.border_widths[2], style.border_widths[2]);
        assert_eq!(open.border_widths[1], style.border_widths[1]);
        let both = sliced_style(
            &style,
            FrameEdges {
                top_open: true,
                bottom_open: true,
            },
        );
        assert_eq!(both.border_widths[2], Pt::ZERO);
        assert_eq!(both.padding[2], Pt::ZERO);
        assert_eq!(both.border_radii[3], [Pt::ZERO; 2]);
        assert_eq!(both.border_widths[3], style.border_widths[3]);
    }

    #[test]
    fn frame_fragment_leaves_other_geometry_alone() {
        // No insets recorded: a block split by the block fragmenter.
        let geom = geometry_of(&[100.0, 40.0], None);
        for f in &geom.fragments {
            assert_eq!(frame_fragment(&geom, f).0, *f);
        }
        // Insets but a single fragment (nothing was split): the box's own layout height is used.
        let geom = geometry_of(&[100.0], Some((17.0, 5.0)));
        assert_eq!(
            frame_fragment(&geom, &geom.fragments[0]).0,
            geom.fragments[0]
        );
    }

    #[test]
    fn paragraph_lines_for_page_no_matching_fragment_returns_none() {
        let lines = vec![make_line(16.0, 12.0)];
        let fragments = vec![make_fragment(0, 16.0)];
        // Ask for page 1, which has no fragment.
        let result = paragraph_lines_for_page(&lines, &fragments, &[], 1, false);
        assert!(result.is_none());
    }

    #[test]
    fn paragraph_lines_for_page_not_split_returns_all_lines() {
        // 64px × 0.75 ≈ 48pt (3 × 16pt lines). Non-split path returns
        // all lines unchanged — baseline values are preserved as-is.
        let lines = vec![
            make_line(16.0, 12.0),
            make_line(16.0, 28.0),
            make_line(16.0, 44.0),
        ];
        let fragments = vec![make_fragment(0, 64.0)];
        let result = paragraph_lines_for_page(&lines, &fragments, &[], 0, false);
        assert!(result.is_some());
        let sliced = result.unwrap();
        assert_eq!(sliced.len(), 3);
        assert!(
            (sliced[0].baseline.to_f32() - 12.0).abs() < 0.01,
            "baseline unchanged on non-split"
        );
    }

    #[test]
    fn paragraph_lines_for_page_split_first_page_returns_first_lines() {
        // Lines are 12pt each. Fragment heights are in CSS px;
        // 16px × PX_TO_PT (0.75) = 12pt, so 16px per line.
        // Page 0: consumed = 0, baseline unchanged.
        let lines = vec![make_line(12.0, 9.0), make_line(12.0, 21.0)];
        let fragments = vec![
            make_fragment(0, 16.0), // 16px = 12pt → first line
            make_fragment(1, 16.0),
        ];
        let result = paragraph_lines_for_page(&lines, &fragments, &[], 0, true);
        assert!(result.is_some());
        let sliced = result.unwrap();
        assert_eq!(sliced.len(), 1, "page 0 should contain exactly one line");
        assert!(
            (sliced[0].baseline.to_f32() - 9.0).abs() < 0.01,
            "page 0 baseline unchanged (consumed=0)"
        );
    }

    #[test]
    fn paragraph_lines_for_page_split_second_page_returns_remaining_lines() {
        // Page 1: consumed = 12pt (one line). The function subtracts consumed
        // from baseline, so paragraph-absolute 21pt → fragment-local 9pt.
        let lines = vec![make_line(12.0, 9.0), make_line(12.0, 21.0)];
        let fragments = vec![
            make_fragment(0, 16.0), // 16px = 12pt → first line
            make_fragment(1, 16.0),
        ];
        let result = paragraph_lines_for_page(&lines, &fragments, &[], 1, true);
        assert!(result.is_some());
        let sliced = result.unwrap();
        assert_eq!(sliced.len(), 1, "page 1 should contain exactly one line");
        assert!(
            (sliced[0].baseline.to_f32() - 9.0).abs() < 0.01,
            "baseline rebased: 21pt - 12pt consumed = 9pt"
        );
    }

    #[test]
    fn paragraph_lines_for_page_split_empty_range_returns_none() {
        // Fragment height is 0px → no lines fit.
        let lines = vec![make_line(12.0, 9.0)];
        let fragments = vec![
            make_fragment(0, 100.0), // page 0 gets the line
            make_fragment(1, 0.0),   // page 1 has zero height
        ];
        let result = paragraph_lines_for_page(&lines, &fragments, &[], 1, true);
        assert!(result.is_none());
    }

    // --- paragraph_lines_for_page: F78, lines decided once by the fragmenter ---

    /// `n` lines that advance by `advance_px` while their quantised boxes are `box_px` tall: what Parley lays out
    /// (`ShapedLine`s, in pt) and what the fragmenter measures (`(top, bottom)` of the boxes, in px). Baselines
    /// are paragraph-absolute and 11px below the box top.
    fn synthetic_paragraph(
        n: usize,
        advance_px: f32,
        box_px: f32,
    ) -> (Vec<crate::paragraph::ShapedLine>, Vec<(f32, f32)>) {
        let tops: Vec<f32> = (0..n)
            .map(|i| (i as f32 * advance_px).round() - 1.0)
            .collect();
        let lines = tops
            .iter()
            .map(|top| make_line(advance_px * 0.75, (top + 11.0) * 0.75))
            .collect();
        let boxes = tops.iter().map(|top| (*top, top + box_px)).collect();
        (lines, boxes)
    }

    /// Every page's slice of `lines` against what the fragmenter decided: the same lines, each once, and the
    /// first line of every page where line 0 sits on page 0.
    fn assert_pages_hold_the_planned_lines(
        lines: &[crate::paragraph::ShapedLine],
        boxes: &[(f32, f32)],
    ) {
        let mut geom = crate::pagination_layout::PaginationGeometryTable::new();
        let (last_page, ..) = crate::pagination_layout::fragment_inline_root(
            &mut geom,
            1,
            0.0,
            100.0,
            0.0,
            0,
            crate::pagination_layout::PageStrips::new(1000.0, 0.0),
            boxes,
        );
        let g = &geom[&1];
        assert!(last_page >= 50, "about 60 pages, got {}", last_page + 1);
        let mut printed = 0;
        for (k, (frag, planned)) in g.fragments.iter().zip(&g.line_slices).enumerate() {
            let page = paragraph_lines_for_page(
                lines,
                &g.fragments,
                &g.line_slices,
                frag.page_index,
                true,
            )
            .unwrap_or_else(|| panic!("page {k}: no lines, planned {planned:?}"));
            let want = (planned.end - planned.first) as usize;
            assert_eq!(
                page.len(),
                want,
                "page {k}: planned lines {}..{}, painted {}",
                planned.first,
                planned.end,
                page.len()
            );
            let drift = page[0].baseline.to_f32() - lines[0].baseline.to_f32();
            assert!(
                drift.abs() < 0.01,
                "page {k}: its first line sits {drift}pt from where line 0 sits on page 0"
            );
            // The slice is the planned one: line `first + j` rebased by the offset.
            for (j, line) in page.iter().enumerate() {
                let orig = &lines[planned.first as usize + j];
                let want_baseline = orig.baseline.to_f32() - planned.offset.in_pt().to_f32();
                assert!(
                    (line.baseline.to_f32() - want_baseline).abs() < 0.01,
                    "page {k} line {j}: baseline {} want {want_baseline}",
                    line.baseline.to_f32()
                );
            }
            printed += page.len();
        }
        assert_eq!(printed, lines.len(), "every line is painted exactly once");
    }

    #[test]
    fn paragraph_lines_for_page_boxes_taller_than_the_advance_lose_no_line() {
        // A 16px Inter line at `line-height: normal`: advances 19.2px, box 20px (they overlap by 0.8px). The
        // renderer used to count 0.8px too much per page, skipping a line after about 24 pages.
        let (lines, boxes) = synthetic_paragraph(3000, 19.2, 20.0);
        assert_pages_hold_the_planned_lines(&lines, &boxes);
    }

    #[test]
    fn paragraph_lines_for_page_boxes_shorter_than_the_advance_repeat_no_line() {
        // The mirror: positive leading (advance 20.4px, box 20px). Walking the advance from the boxes' heights
        // counted too little per page and painted a line on two pages.
        let (lines, boxes) = synthetic_paragraph(3000, 20.4, 20.0);
        assert_pages_hold_the_planned_lines(&lines, &boxes);
    }

    #[test]
    fn measured_cut_is_partial_only_for_some_lines_of_several() {
        assert!(!measured_cut_is_partial(1, 0, 1), "one line");
        assert!(!measured_cut_is_partial(3, 0, 3), "whole paragraph");
        assert!(!measured_cut_is_partial(3, 3, 3), "no lines on this page");
        assert!(measured_cut_is_partial(3, 0, 2), "head");
        assert!(measured_cut_is_partial(3, 2, 3), "tail");
        assert!(measured_cut_is_partial(3, 1, 2), "middle");
    }

    // --- F30f: a carrier's line, and the continuation pages of an inline box ---

    fn geometry_over_pages(pages: &[u32]) -> crate::pagination_layout::PaginationGeometry {
        crate::pagination_layout::PaginationGeometry {
            fragments: pages.iter().map(|&p| make_fragment(p, 16.0)).collect(),
            ..Default::default()
        }
    }

    fn slice(first: u32, end: u32) -> crate::pagination_layout::LineSlice {
        crate::pagination_layout::LineSlice {
            first,
            end,
            offset: 0.0_f32.as_px(),
        }
    }

    #[test]
    fn paragraph_lines_for_page_paints_a_carrier_line_on_its_page_only() {
        // The carrier of a split `inline-block` has one line and a fragment per page; the fragmenter put the line on
        // the page where the box starts (page 1) and gave the others none (`record_carrier_slices`). The line is
        // taller than every fragment, so measuring would paint it nowhere.
        let lines = vec![make_line(120.0, 90.0)];
        let fragments = vec![
            make_fragment(0, 16.0),
            make_fragment(1, 16.0),
            make_fragment(2, 16.0),
        ];
        let slices = [slice(0, 0), slice(0, 1), slice(1, 1)];
        for (page, painted) in [(0, false), (1, true), (2, false)] {
            let got = paragraph_lines_for_page(&lines, &fragments, &slices, page, true);
            assert_eq!(got.is_some(), painted, "page {page}");
        }
        let line = paragraph_lines_for_page(&lines, &fragments, &slices, 1, true).unwrap();
        assert_eq!(line.len(), 1);
        assert!(
            (line[0].baseline.to_f32() - 90.0).abs() < 0.01,
            "painted where the box starts, not moved"
        );
        // Without the slices the line is measured from the fragment heights and is painted on no page.
        for page in 0..3 {
            assert!(
                paragraph_lines_for_page(&lines, &fragments, &[], page, true).is_none(),
                "page {page}"
            );
        }
    }

    #[test]
    fn inline_box_continues_on_only_after_its_first_page() {
        let g = geometry_over_pages(&[0, 1, 2]);
        assert!(!inline_box_continues_on(&g, 0), "its carrier paints page 0");
        assert!(inline_box_continues_on(&g, 1));
        assert!(inline_box_continues_on(&g, 2));
        assert!(!inline_box_continues_on(&g, 3), "no fragment on page 3");
        // Starting on page 1: the carrier paints it there.
        let g = geometry_over_pages(&[1, 2]);
        assert!(!inline_box_continues_on(&g, 1));
        assert!(inline_box_continues_on(&g, 2));
        // One fragment never continues.
        assert!(!inline_box_continues_on(&geometry_over_pages(&[0]), 0));
        assert!(!inline_box_continues_on(&geometry_over_pages(&[2]), 2));
        // Repeated whole on every page (`position: fixed`): its carrier paints it each time.
        let mut g = geometry_over_pages(&[0, 1, 2]);
        g.is_repeat = true;
        assert!(!inline_box_continues_on(&g, 1));
    }

    /// Runs `f` with a canvas on a scratch page.
    fn with_scratch_canvas<R>(
        f: impl FnOnce(&mut crate::draw_primitives::Canvas<'_, '_>) -> R,
    ) -> R {
        let mut doc = krilla::Document::new();
        let settings = krilla::page::PageSettings::from_wh(200.0, 200.0).expect("valid page size");
        let mut page = doc.start_page_with(settings);
        let mut surface = page.surface();
        let mut canvas = crate::draw_primitives::Canvas {
            surface: &mut surface,
            bookmark_collector: None,
            link_collector: None,
            tag_collector: None,
            link_run_node_id: None,
            in_marked_content: false,
            images: Default::default(),
            fonts: &mut Default::default(),
            paint_extent: Default::default(),
            clip_band: Default::default(),
        };
        f(&mut canvas)
    }

    #[test]
    fn an_inline_box_continuation_is_painted_once_per_page() {
        // Box 7 (subtree {8}) has a fragment on pages 0 to 3; box 9 is not an inline box.
        let mut d = Drawables::new();
        d.inline_box_subtree_skip.extend([7, 8]);
        d.inline_box_subtree_descendants.insert(7, vec![8]);
        let mut geometry = crate::pagination_layout::PaginationGeometryTable::new();
        for id in [7, 8, 9] {
            geometry.insert(id, geometry_over_pages(&[0, 1, 2, 3]));
        }
        let paint = |id: usize, page: u32, d: &Drawables| {
            with_scratch_canvas(|canvas| {
                paint_inline_box_continuation(canvas, id, page, &geometry, d, 10.0, 10.0)
            })
        };
        let painted: Vec<bool> = (0..5)
            .map(|page| {
                let painted = paint(7, page, &d);
                // Every box that continues on the page was painted (debug builds assert it).
                check_continuations_painted(page, &geometry, &d);
                painted
            })
            .collect();
        assert_eq!(painted, [false, true, true, true, false]);
        // A descendant is walked by its box, and a node that is no inline box is not this function's.
        assert!(!paint(8, 2, &d));
        assert!(!paint(9, 2, &d));
        // A second walk that reaches a box that was painted already paints nothing.
        assert!(paint(7, 1, &d));
        assert!(!paint(7, 1, &d));
        check_continuations_painted(1, &geometry, &d);
        // A box that is not visible is skipped with everything in it, as its carrier does.
        let mut hidden = Drawables::new();
        hidden.inline_box_subtree_descendants.insert(7, vec![8]);
        hidden.block_styles.insert(
            7,
            crate::drawables::BlockEntry {
                style: crate::draw_primitives::BlockStyle::default(),
                opacity: 1.0,
                visible: false,
                id: None,
                layout_size: None,
                clip_descendants: vec![],
                opacity_descendants: vec![],
            },
        );
        assert!(!paint(7, 2, &hidden));
        check_continuations_painted(2, &geometry, &hidden);
    }

    #[test]
    fn a_continuation_has_the_opacity_of_its_carrier_paragraph() {
        let mut d = Drawables::new();
        let mut para = make_para(vec![make_shaped_line(vec![])]);
        para.opacity = 0.5;
        d.paragraphs.insert(1, para);
        d.inline_box_carrier.insert(2, 1);
        assert_eq!(carrier_opacity(2, &d), 0.5);
        assert_eq!(carrier_opacity(3, &d), 1.0, "no carrier recorded");
        // The carrier of an opacity scope with a block entry holds no opacity itself.
        d.paragraphs
            .insert(4, make_para(vec![make_shaped_line(vec![])]));
        d.inline_box_carrier.insert(5, 4);
        assert_eq!(carrier_opacity(5, &d), 1.0);
    }

    #[test]
    fn a_box_nested_in_a_continuation_is_painted_with_it_and_only_once() {
        // Box 7 holds a descendant 8 whose paragraph carries box 10: on a page after its first, 8 has no line, so
        // nothing else paints 10 (a clip box inside an inline-block, F30f validator H2).
        let mut d = Drawables::new();
        d.inline_box_subtree_skip.extend([7, 8, 10]);
        d.inline_box_subtree_descendants.insert(7, vec![8]);
        d.inline_box_subtree_descendants.insert(10, vec![]);
        d.paragraphs.insert(
            8,
            make_para(vec![make_shaped_line(vec![
                crate::paragraph::LineItem::InlineBox(crate::paragraph::InlineBoxItem {
                    node_id: Some(10),
                    width: 10.0_f32.as_pt(),
                    height: 10.0_f32.as_pt(),
                    x_offset: crate::units::Pt::ZERO,
                    computed_y: crate::units::Pt::ZERO,
                    link: None,
                    opacity: 1.0,
                    visible: true,
                }),
            ])]),
        );
        assert_eq!(nested_inline_boxes(7, &d), vec![10]);
        let mut geometry = crate::pagination_layout::PaginationGeometryTable::new();
        for id in [7, 8, 10] {
            geometry.insert(id, geometry_over_pages(&[0, 1, 2]));
        }
        with_scratch_canvas(|canvas| {
            assert!(paint_inline_box_continuation(
                canvas, 7, 1, &geometry, &d, 10.0, 10.0
            ));
            // 10 was painted with 7: a walk that reaches it too paints nothing.
            assert!(!paint_inline_box_continuation(
                canvas, 10, 1, &geometry, &d, 10.0, 10.0
            ));
        });
        check_continuations_painted(1, &geometry, &d);
    }

    #[test]
    fn paragraph_lines_for_page_slices_that_do_not_match_the_lines_fall_back_to_measuring() {
        // Two 12pt lines, fragments of one line each, but slices that end at line 5: they describe some other
        // paragraph, so the lines are measured from the fragment heights (and a warning names the paragraph).
        let lines = vec![make_line(12.0, 9.0), make_line(12.0, 21.0)];
        let fragments = vec![make_fragment(0, 16.0), make_fragment(1, 16.0)];
        let stale = vec![
            crate::pagination_layout::LineSlice {
                first: 0,
                end: 3,
                offset: 0.0_f32.as_px(),
            },
            crate::pagination_layout::LineSlice {
                first: 3,
                end: 5,
                offset: 99.0_f32.as_px(),
            },
        ];
        let page = paragraph_lines_for_page(&lines, &fragments, &stale, 1, true).expect("measured");
        assert_eq!(page.len(), 1);
        assert!(
            (page[0].baseline.to_f32() - 9.0).abs() < 0.01,
            "rebased by the consumed height"
        );
    }

    /// F78 on a real document: 7000 numbered words in Noto Sans over A4 pages (about 14), where the line
    /// boxes do not tile (`normal` line height: advance 1.2 em, box 1.4 em rounded). Every line is painted
    /// on exactly one page, in order.
    #[test]
    fn a_long_paragraph_paints_every_line_on_exactly_one_page() {
        let font = std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR"))
            .join("../../examples/.fonts/NotoSans-Regular.ttf");
        let mut assets = crate::asset::AssetBundle::default();
        assets.add_font_file(&font).expect("Noto Sans");
        let engine = crate::engine::Engine::builder()
            .system_fonts(false)
            .assets(assets)
            .build();
        let words: Vec<String> = (0..7000).map(|i| format!("w{i:05}")).collect();
        let html = format!(
            "<!doctype html><html><body style=\"font-family:'Noto Sans'\"><p>{}</p></body></html>",
            words.join(" ")
        );
        let (drawables, geometry) = engine.build_drawables_and_geometry_for_testing_no_gcpm(&html);
        let (id, para) = drawables
            .paragraphs
            .iter()
            .max_by_key(|(_, p)| p.lines.len())
            .expect("a paragraph");
        let geom = &geometry[id];
        assert!(geom.is_split() && geom.fragments.len() >= 10, "{geom:?}");
        assert_eq!(geom.line_slices.len(), geom.fragments.len());

        let mut printed: Vec<String> = Vec::new();
        let mut lines_printed = 0;
        for frag in &geom.fragments {
            let page = paragraph_lines_for_page(
                &para.lines,
                &geom.fragments,
                &geom.line_slices,
                frag.page_index,
                true,
            )
            .expect("every page has lines");
            lines_printed += page.len();
            for line in &page {
                let mut line_text = String::new();
                for item in &line.items {
                    if let crate::paragraph::LineItem::Text(run) = item {
                        // A run's text is the whole paragraph; its glyphs cover this line's part.
                        line_text.extend(
                            run.glyphs
                                .iter()
                                .filter_map(|g| run.text.get(g.text_range.clone())),
                        );
                    }
                }
                printed.extend(line_text.split_whitespace().map(str::to_string));
            }
        }
        assert_eq!(lines_printed, para.lines.len(), "each line on one page");
        let first_diff = printed.iter().zip(&words).position(|(a, b)| a != b);
        assert!(
            printed == words,
            "every word once, in order: {} printed of {}, first difference at {first_diff:?} ({:?})",
            printed.len(),
            words.len(),
            first_diff.map(|i| (&printed[i], &words[i]))
        );
    }

    // --- slice_on_page / is_flat_slice (F37) ---

    fn body_slice(page: u32, height: f32) -> crate::pagination_layout::Fragment {
        crate::pagination_layout::Fragment {
            page_index: page,
            x: 0.0_f32.as_px(),
            y: 0.0_f32.as_px(),
            width: 100.0_f32.as_px(),
            height: height.as_px(),
        }
    }

    #[test]
    fn slice_on_page_gives_the_heights_before_and_in_total() {
        let slices = [
            body_slice(0, 900.0),
            body_slice(1, 1000.0),
            body_slice(2, 250.0),
        ];
        for (page, before, here) in [(0, 0.0, 900.0), (1, 900.0, 1000.0), (2, 1900.0, 250.0)] {
            let (b, total, slice) = slice_on_page(&slices, page).expect("a slice on the page");
            assert_eq!(b.to_f32(), before, "page {page}");
            assert_eq!(total.to_f32(), 2150.0);
            assert_eq!(slice.height.to_f32(), here);
        }
        assert!(slice_on_page(&slices, 3).is_none(), "no slice past the end");
        assert!(slice_on_page(&[], 0).is_none(), "no slices at all");
    }

    #[test]
    fn a_slice_is_flat_unless_it_has_layers_a_border_a_shadow_or_a_rounded_colour() {
        use crate::draw_primitives::{BlockStyle, BorderStyleValue};
        assert!(is_flat_slice(&BlockStyle::default(), true));
        let coloured = BlockStyle {
            background_color: Some([255, 0, 0, 255]),
            ..Default::default()
        };
        assert!(is_flat_slice(&coloured, true), "a flat colour");
        let rounded = BlockStyle {
            background_color: Some([255, 0, 0, 255]),
            border_radii: [[3.0_f32.as_pt(), 3.0_f32.as_pt()]; 4],
            ..Default::default()
        };
        assert!(!is_flat_slice(&rounded, true), "a colour with a radius");
        assert!(
            is_flat_slice(&rounded, false),
            "the canvas background is not the box's own"
        );
        let bordered = BlockStyle {
            border_widths: [2.0_f32.as_pt(); 4],
            border_styles: [BorderStyleValue::Solid; 4],
            ..Default::default()
        };
        assert!(!is_flat_slice(&bordered, false));
        let unseen_border = BlockStyle {
            border_widths: [2.0_f32.as_pt(); 4],
            border_styles: [BorderStyleValue::None; 4],
            ..Default::default()
        };
        assert!(is_flat_slice(&unseen_border, true));
        let shadowed = BlockStyle {
            box_shadows: vec![Default::default()],
            ..Default::default()
        };
        assert!(!is_flat_slice(&shadowed, false));
    }

    // --- canvas_background_source (F184) ---

    fn entry_with_background(
        colour: Option<[u8; 4]>,
        layers: bool,
    ) -> crate::drawables::BlockEntry {
        let mut entry = block_entry_with_overflow_clip(vec![]);
        entry.style = crate::draw_primitives::BlockStyle {
            background_color: colour,
            ..Default::default()
        };
        if layers {
            entry
                .style
                .background_layers
                .push(crate::draw_primitives::BackgroundLayer {
                    content: crate::draw_primitives::BgImageContent::Raster {
                        data: Arc::new(vec![]),
                        format: crate::image::ImageFormat::Png,
                    },
                    intrinsic_width: crate::units::Pt::ZERO,
                    intrinsic_height: crate::units::Pt::ZERO,
                    size: crate::draw_primitives::BgSize::Auto,
                    position_x: crate::draw_primitives::BgLengthPercentage::Percentage(0.0),
                    position_y: crate::draw_primitives::BgLengthPercentage::Percentage(0.0),
                    repeat_x: crate::draw_primitives::BgRepeat::Repeat,
                    repeat_y: crate::draw_primitives::BgRepeat::Repeat,
                    origin: crate::draw_primitives::BgBox::PaddingBox,
                    clip: crate::draw_primitives::BgClip::BorderBox,
                });
        }
        entry
    }

    /// `html` is node 1, `body` node 2.
    fn root_and_body(
        root: Option<crate::drawables::BlockEntry>,
        body: Option<crate::drawables::BlockEntry>,
    ) -> Drawables {
        let mut d = Drawables::new();
        d.root_id = Some(1);
        d.body_id = Some(2);
        if let Some(e) = root {
            d.block_styles.insert(1, e);
        }
        if let Some(e) = body {
            d.block_styles.insert(2, e);
        }
        d
    }

    #[test]
    fn canvas_background_source_is_the_root_when_it_has_a_colour_or_an_image() {
        let white = Some([255, 255, 255, 255]);
        for root in [
            entry_with_background(white, false),
            entry_with_background(None, true),
            entry_with_background(white, true),
        ] {
            let d = root_and_body(Some(root), Some(entry_with_background(white, true)));
            assert_eq!(canvas_background_source(&d), Some(1));
        }
    }

    #[test]
    fn canvas_background_source_falls_back_to_body_when_root_is_transparent() {
        let body = || Some(entry_with_background(Some([255, 255, 221, 255]), false));
        for root in [
            None,
            Some(entry_with_background(None, false)),
            // a colour with alpha 0 is transparent (css-backgrounds-3 §2.11.2)
            Some(entry_with_background(Some([10, 20, 30, 0]), false)),
        ] {
            let d = root_and_body(root, body());
            assert_eq!(canvas_background_source(&d), Some(2));
        }
        let d = root_and_body(None, Some(entry_with_background(None, true)));
        assert_eq!(canvas_background_source(&d), Some(2));
    }

    #[test]
    fn canvas_background_source_is_none_without_a_background() {
        let d = root_and_body(
            Some(entry_with_background(Some([1, 2, 3, 0]), false)),
            Some(entry_with_background(None, false)),
        );
        assert_eq!(canvas_background_source(&d), None);
        assert_eq!(canvas_background_source(&Drawables::new()), None);
    }

    // --- build_page_skip_sets ---

    fn block_entry_with_overflow_clip(descendants: Vec<usize>) -> crate::drawables::BlockEntry {
        let style = crate::draw_primitives::BlockStyle {
            overflow_x: crate::draw_primitives::Overflow::Clip,
            ..Default::default()
        };
        crate::drawables::BlockEntry {
            style,
            opacity: 1.0,
            visible: true,
            id: None,
            layout_size: None,
            clip_descendants: descendants,
            opacity_descendants: vec![],
        }
    }

    fn block_entry_with_opacity_descendants(
        descendants: Vec<usize>,
    ) -> crate::drawables::BlockEntry {
        crate::drawables::BlockEntry {
            style: crate::draw_primitives::BlockStyle::default(),
            opacity: 0.5,
            visible: true,
            id: None,
            layout_size: None,
            clip_descendants: vec![],
            opacity_descendants: descendants,
        }
    }

    #[test]
    fn build_page_skip_sets_empty_drawables_returns_empty_sets() {
        let d = Drawables::new();
        let (tx, clip, opacity) = build_page_skip_sets(&d);
        assert!(tx.is_empty());
        assert!(clip.is_empty());
        assert!(opacity.is_empty());
    }

    #[test]
    fn build_page_skip_sets_transform_descendants_collected() {
        let mut d = Drawables::new();
        d.transforms.insert(
            10,
            crate::drawables::TransformEntry {
                matrix: crate::draw_primitives::Affine2D::translation(
                    0.0_f32.as_pt(),
                    0.0_f32.as_pt(),
                ),
                origin: crate::draw_primitives::Point2::new(
                    crate::units::Pt::ZERO,
                    crate::units::Pt::ZERO,
                ),
                descendants: vec![11, 12],
            },
        );
        let (tx, clip, opacity) = build_page_skip_sets(&d);
        assert!(tx.contains(&11));
        assert!(tx.contains(&12));
        assert!(!tx.contains(&10), "wrapper itself is not a descendant");
        assert!(clip.is_empty());
        assert!(opacity.is_empty());
    }

    #[test]
    fn build_page_skip_sets_overflow_clip_block_collects_descendants() {
        let mut d = Drawables::new();
        let node_id: usize = 20;
        d.block_styles
            .insert(node_id, block_entry_with_overflow_clip(vec![21, 22]));
        let (_, clip, _) = build_page_skip_sets(&d);
        assert!(clip.contains(&21));
        assert!(clip.contains(&22));
    }

    #[test]
    fn build_page_skip_sets_body_excluded_from_clip_descendants() {
        let mut d = Drawables::new();
        let body_id: usize = 5;
        d.body_id = Some(body_id);
        d.block_styles
            .insert(body_id, block_entry_with_overflow_clip(vec![6, 7]));
        let (_, clip, _) = build_page_skip_sets(&d);
        assert!(!clip.contains(&6), "body clip descendants must be excluded");
        assert!(!clip.contains(&7));
    }

    #[test]
    fn build_page_skip_sets_opacity_descendants_collected() {
        let mut d = Drawables::new();
        let node_id: usize = 30;
        d.block_styles
            .insert(node_id, block_entry_with_opacity_descendants(vec![31, 32]));
        let (_, _, opacity) = build_page_skip_sets(&d);
        assert!(opacity.contains(&31));
        assert!(opacity.contains(&32));
    }

    #[test]
    fn build_page_skip_sets_body_excluded_from_opacity_descendants() {
        let mut d = Drawables::new();
        let body_id: usize = 3;
        d.body_id = Some(body_id);
        d.block_styles
            .insert(body_id, block_entry_with_opacity_descendants(vec![4, 5]));
        let (_, _, opacity) = build_page_skip_sets(&d);
        assert!(
            !opacity.contains(&4),
            "body opacity descendants must be excluded"
        );
        assert!(!opacity.contains(&5));
    }

    #[test]
    fn build_page_skip_sets_table_overflow_clip_collects_descendants() {
        let mut d = Drawables::new();
        let style = crate::draw_primitives::BlockStyle {
            overflow_x: crate::draw_primitives::Overflow::Clip,
            ..Default::default()
        };
        d.tables.insert(
            40,
            crate::drawables::TableEntry {
                style,
                opacity: 1.0,
                visible: true,
                id: None,
                layout_size: None,
                width: 200.0_f32.as_pt(),
                cached_height: 100.0_f32.as_pt(),
                clip_descendants: vec![41, 42],
            },
        );
        let (_, clip, _) = build_page_skip_sets(&d);
        assert!(clip.contains(&41));
        assert!(clip.contains(&42));
    }

    #[test]
    fn build_page_skip_sets_root_excluded_from_clip_descendants() {
        let mut d = Drawables::new();
        let root_id: usize = 2;
        d.root_id = Some(root_id);
        d.block_styles
            .insert(root_id, block_entry_with_overflow_clip(vec![3, 4]));
        let (_, clip, _) = build_page_skip_sets(&d);
        assert!(!clip.contains(&3), "root clip descendants must be excluded");
        assert!(!clip.contains(&4));
    }

    #[test]
    fn build_page_skip_sets_root_excluded_from_opacity_descendants() {
        let mut d = Drawables::new();
        let root_id: usize = 7;
        d.root_id = Some(root_id);
        d.block_styles
            .insert(root_id, block_entry_with_opacity_descendants(vec![8, 9]));
        let (_, _, opacity) = build_page_skip_sets(&d);
        assert!(
            !opacity.contains(&8),
            "root opacity descendants must be excluded"
        );
        assert!(!opacity.contains(&9));
    }

    // --- table_box_size ---

    fn make_table_entry_for_size(
        layout_size: Option<crate::draw_primitives::Size>,
        width: f32,
        cached_height: f32,
    ) -> crate::drawables::TableEntry {
        use crate::units::F32Units;
        crate::drawables::TableEntry {
            style: crate::draw_primitives::BlockStyle::default(),
            opacity: 1.0,
            visible: true,
            id: None,
            layout_size,
            width: width.as_pt(),
            cached_height: cached_height.as_pt(),
            clip_descendants: vec![],
        }
    }

    fn make_frag_with_height(height: f32) -> crate::pagination_layout::Fragment {
        crate::pagination_layout::Fragment {
            page_index: 0,
            x: 0.0_f32.as_px(),
            y: 0.0_f32.as_px(),
            width: 0.0_f32.as_px(),
            height: height.as_px(),
        }
    }

    #[test]
    fn table_box_size_with_layout_size_uses_layout_dimensions() {
        // When layout_size is Some, both width and height come from it,
        // ignoring entry.width, entry.cached_height, and frag.height.
        use crate::units::F32Units;
        let sz = crate::draw_primitives::Size {
            width: 120.0_f32.as_pt(),
            height: 80.0_f32.as_pt(),
        };
        let entry = make_table_entry_for_size(Some(sz), 200.0, 50.0);
        let frag = make_frag_with_height(99.0);
        let (w, h) = table_box_size(&entry, &frag, false);
        assert!((w - 120.0).abs() < 0.001, "width from layout_size");
        assert!((h - 80.0).abs() < 0.001, "height from layout_size");
    }

    #[test]
    fn table_box_size_of_a_piece_uses_the_fragment_height_not_the_table_height() {
        // F174: a table cut over columns paints each piece as tall as the piece, whatever the layout says.
        use crate::units::F32Units;
        let entry = make_table_entry_for_size(
            Some(crate::draw_primitives::Size {
                width: 120.0_f32.as_pt(),
                height: 80.0_f32.as_pt(),
            }),
            200.0,
            50.0,
        );
        let frag = make_frag_with_height(20.0);
        let (w, h) = table_box_size(&entry, &frag, true);
        assert!((w - 120.0).abs() < 0.001, "the width stays the layout's");
        assert!((h - 15.0).abs() < 0.001, "20 px is 15 pt, got {h}");
        let (_, whole) = table_box_size(&entry, &frag, false);
        assert!((whole - 80.0).abs() < 0.001);
    }

    #[test]
    fn a_piece_that_is_not_the_last_has_room_for_the_bottom_border() {
        // F174: every piece but the last is closed by the table's bottom border below its rows.
        use crate::units::F32Units;
        let mut entry = make_table_entry_for_size(None, 200.0, 50.0);
        entry.style.border_widths = [1.5, 1.5, 1.5, 1.5].map(|v| v.as_pt());
        let (a, b) = (make_frag_with_height(20.0), make_frag_with_height(30.0));
        let pieces = [a.clone(), b.clone()];
        assert!((table_piece_bottom(&entry, &a, &pieces) - 1.5).abs() < 0.001);
        assert_eq!(table_piece_bottom(&entry, &b, &pieces), 0.0);
        assert_eq!(table_piece_bottom(&entry, &a, &[]), 0.0);
    }

    #[test]
    fn table_box_size_no_layout_size_nonzero_frag_uses_in_pt_height() {
        // frag.height = 40 CSS px → 40 × 0.75 = 30 PDF pt (via Px::in_pt)
        let entry = make_table_entry_for_size(None, 150.0, 99.0);
        let frag = make_frag_with_height(40.0);
        let (w, h) = table_box_size(&entry, &frag, false);
        assert!((w - 150.0).abs() < 0.001, "width falls back to entry.width");
        assert!((h - 30.0).abs() < 0.01, "height = frag.height.in_pt()");
    }

    #[test]
    fn table_box_size_no_layout_size_zero_frag_falls_back_to_cached_height() {
        // When frag.height is 0, Px(0).in_pt() = Pt(0.0) which fails the `> 0.0`
        // guard, so cached_height is used instead.
        let entry = make_table_entry_for_size(None, 150.0, 55.0);
        let frag = make_frag_with_height(0.0);
        let (w, h) = table_box_size(&entry, &frag, false);
        assert!((w - 150.0).abs() < 0.001, "width falls back to entry.width");
        assert!(
            (h - 55.0).abs() < 0.001,
            "height falls back to cached_height when frag.height is zero"
        );
    }

    // --- build_struct_tree ---

    fn make_p_semantic_entry(parent: Option<usize>) -> crate::tagging::SemanticEntry {
        crate::tagging::SemanticEntry {
            tag: crate::tagging::PdfTag::P,
            parent,
            alt_text: None,
        }
    }

    fn make_h1_semantic_entry(parent: Option<usize>) -> crate::tagging::SemanticEntry {
        crate::tagging::SemanticEntry {
            tag: crate::tagging::PdfTag::H { level: 1 },
            parent,
            alt_text: None,
        }
    }

    #[test]
    fn build_struct_tree_empty_inputs_yields_empty_tree() {
        let tc = crate::draw_primitives::TagCollector::new();
        let d = Drawables::new();
        let link_annot_ids: BTreeMap<usize, Vec<Identifier>> = BTreeMap::new();
        let mut tree = TagTree::new();
        build_struct_tree(tc, &d, &link_annot_ids, &mut tree);
        assert!(
            tree.children.is_empty(),
            "empty inputs should produce no tree nodes"
        );
    }

    #[test]
    fn build_struct_tree_single_p_node_produces_one_root_group() {
        let node_id: usize = 1;
        let id = make_identifier();
        let mut tc = crate::draw_primitives::TagCollector::new();
        tc.record(node_id, crate::tagging::PdfTag::P, id, None);
        let mut d = Drawables::new();
        d.semantics.insert(node_id, make_p_semantic_entry(None));
        let link_annot_ids: BTreeMap<usize, Vec<Identifier>> = BTreeMap::new();
        let mut tree = TagTree::new();
        build_struct_tree(tc, &d, &link_annot_ids, &mut tree);
        assert_eq!(tree.children.len(), 1, "one semantic root → one Group");
        assert!(matches!(tree.children[0], Node::Group(_)));
    }

    #[test]
    fn build_struct_tree_tc_entry_heading_title_is_forwarded() {
        // An H2 node recorded via tc.record with an explicit heading_title should
        // result in a single root Group in the tree (title is opaque to test code,
        // so we only verify the structure).
        let node_id: usize = 10;
        let id = make_identifier();
        let mut tc = crate::draw_primitives::TagCollector::new();
        tc.record(
            node_id,
            crate::tagging::PdfTag::H { level: 2 },
            id,
            Some("Section title".to_string()),
        );
        let mut d = Drawables::new();
        d.semantics.insert(node_id, make_h1_semantic_entry(None));
        let link_annot_ids: BTreeMap<usize, Vec<Identifier>> = BTreeMap::new();
        let mut tree = TagTree::new();
        build_struct_tree(tc, &d, &link_annot_ids, &mut tree);
        assert_eq!(tree.children.len(), 1, "one root Group");
        assert!(matches!(tree.children[0], Node::Group(_)));
    }

    #[test]
    fn build_struct_tree_run_entry_h_tag_backfills_title_from_paragraph() {
        // Node uses per-run tagging (tc.record_run, not tc.record), so
        // heading_titles starts empty. The backfill loop in build_struct_tree
        // should detect the H tag + non-empty paragraph text and insert the title.
        let node_id: usize = 20;
        let id = make_identifier();
        let mut tc = crate::draw_primitives::TagCollector::new();
        tc.record_run(
            node_id,
            crate::draw_primitives::ParagraphRunItem::Content(id),
        );
        let mut d = Drawables::new();
        d.semantics.insert(node_id, make_h1_semantic_entry(None));
        let text_line = make_shaped_line(vec![crate::paragraph::LineItem::Text(make_glyph_run(
            "Intro", None,
        ))]);
        d.paragraphs.insert(node_id, make_para(vec![text_line]));
        let link_annot_ids: BTreeMap<usize, Vec<Identifier>> = BTreeMap::new();
        let mut tree = TagTree::new();
        build_struct_tree(tc, &d, &link_annot_ids, &mut tree);
        assert_eq!(tree.children.len(), 1, "one root Group from run_entries");
        assert!(matches!(tree.children[0], Node::Group(_)));
    }

    #[test]
    fn build_struct_tree_child_semantic_is_nested_under_parent() {
        // parent_id has no parent (root); child_id has parent = parent_id.
        // After build_struct_tree only parent_id appears at the tree root,
        // with child_id nested one level deeper.
        let parent_id: usize = 1;
        let child_id: usize = 2;
        let parent_tc_id = make_identifier();
        let child_tc_id = make_identifier();
        let mut tc = crate::draw_primitives::TagCollector::new();
        tc.record(parent_id, crate::tagging::PdfTag::P, parent_tc_id, None);
        tc.record(child_id, crate::tagging::PdfTag::Span, child_tc_id, None);
        let mut d = Drawables::new();
        d.semantics.insert(parent_id, make_p_semantic_entry(None));
        d.semantics.insert(
            child_id,
            crate::tagging::SemanticEntry {
                tag: crate::tagging::PdfTag::Span,
                parent: Some(parent_id),
                alt_text: None,
            },
        );
        let link_annot_ids: BTreeMap<usize, Vec<Identifier>> = BTreeMap::new();
        let mut tree = TagTree::new();
        build_struct_tree(tc, &d, &link_annot_ids, &mut tree);
        assert_eq!(tree.children.len(), 1, "only parent_id at root level");
        // parent group: Leaf(parent_tc_id) + Group(child). Use a matches! guard
        // to inspect the inner structure without a let-else panic arm that would
        // be unreachable in a passing test and flagged by coverage tools.
        assert!(
            matches!(
                &tree.children[0],
                Node::Group(g)
                    if g.children.len() == 2 && matches!(g.children[1], Node::Group(_))
            ),
            "root Group should contain Leaf + nested child Group"
        );
    }

    // --- build_metadata ---
    //
    // krilla::metadata::Metadata fields are pub(crate), so we use Debug formatting
    // to assert field values where meaningful behavior needs verification.

    #[test]
    fn build_metadata_default_config_does_not_panic() {
        // Config::default() has producer=Some("fulgur") — exercises the producer
        // branch. All other optional fields are None / empty, so their branches
        // are skipped without panicking.
        let config = Config::default();
        let meta = build_metadata(&config, None);
        let debug = format!("{meta:?}");
        assert!(
            debug.contains("\"fulgur\""),
            "default producer should appear in metadata"
        );
    }

    #[test]
    fn build_metadata_config_title_overrides_html_title() {
        // config.title is Some → effective_title = Some("Config Title"), html_title ignored.
        let config = Config {
            title: Some("Config Title".to_string()),
            ..Default::default()
        };
        let meta = build_metadata(&config, Some("HTML Title"));
        let debug = format!("{meta:?}");
        assert!(
            debug.contains("\"Config Title\""),
            "config.title should take priority"
        );
        assert!(
            !debug.contains("\"HTML Title\""),
            "html_title should be ignored when config.title is set"
        );
    }

    #[test]
    fn build_metadata_html_title_fallback_when_config_title_none() {
        // config.title is None → effective_title = html_title via .or().
        let config = Config::default();
        let meta = build_metadata(&config, Some("HTML Title"));
        let debug = format!("{meta:?}");
        assert!(
            debug.contains("\"HTML Title\""),
            "html_title should be used as fallback when config.title is None"
        );
    }

    #[test]
    fn build_metadata_with_authors() {
        let config = Config {
            authors: vec!["Alice".to_string(), "Bob".to_string()],
            ..Default::default()
        };
        let _ = build_metadata(&config, None);
    }

    #[test]
    fn build_metadata_with_description() {
        let config = Config {
            description: Some("A test document.".to_string()),
            ..Default::default()
        };
        let _ = build_metadata(&config, None);
    }

    #[test]
    fn build_metadata_with_keywords() {
        let config = Config {
            keywords: vec!["rust".to_string(), "pdf".to_string()],
            ..Default::default()
        };
        let _ = build_metadata(&config, None);
    }

    #[test]
    fn build_metadata_with_lang() {
        let config = Config {
            lang: Some("ja".to_string()),
            ..Default::default()
        };
        let _ = build_metadata(&config, None);
    }

    #[test]
    fn build_metadata_with_creator() {
        let config = Config {
            creator: Some("FulgurTest".to_string()),
            ..Default::default()
        };
        let _ = build_metadata(&config, None);
    }

    #[test]
    fn build_metadata_with_valid_creation_date() {
        // Exercises the `if let Some(dt) = parse_datetime(date_str)` true branch.
        // Debug output shows `creation_date: Some(...)` when the date is valid.
        let config = Config {
            creation_date: Some("2026-05-12T10:30:00".to_string()),
            ..Default::default()
        };
        let meta = build_metadata(&config, None);
        let debug = format!("{meta:?}");
        assert!(
            debug.contains("creation_date: Some"),
            "valid ISO-8601 date should be parsed and set: {debug}"
        );
    }

    #[test]
    fn build_metadata_with_invalid_creation_date_does_not_set_date() {
        // parse_datetime returns None → the inner `if let Some(dt)` arm is skipped,
        // leaving creation_date unset on the Metadata struct.
        let config = Config {
            creation_date: Some("not-a-date".to_string()),
            ..Default::default()
        };
        let meta = build_metadata(&config, None);
        let debug = format!("{meta:?}");
        assert!(
            debug.contains("creation_date: None"),
            "invalid date should leave creation_date as None: {debug}"
        );
    }

    #[test]
    fn build_metadata_all_optional_fields_set() {
        let config = Config {
            title: Some("Full Test Title".to_string()),
            authors: vec!["Author A".to_string()],
            description: Some("Full description.".to_string()),
            keywords: vec!["key1".to_string(), "key2".to_string()],
            lang: Some("en".to_string()),
            creator: Some("Creator App".to_string()),
            producer: Some("Fulgur PDF".to_string()),
            creation_date: Some("2026-01-01".to_string()),
            ..Default::default()
        };
        let _ = build_metadata(&config, None);
    }

    // --- paragraph_lines_for_page: inline-image computed_y rebasing ---

    #[test]
    fn paragraph_lines_for_page_split_rebases_inline_image_computed_y() {
        // Two 12pt lines (each corresponds to a 16px CSS-px fragment).
        // 16px × PX_TO_PT (0.75) = 12pt.
        // Line 1 contains an Image whose computed_y = 15.0 is paragraph-absolute.
        // On page 1: consumed = Px(16).in_pt() = 12pt.
        // After rebasing: img.computed_y = 15.0 − 12.0 = 3.0.
        let img = crate::paragraph::InlineImage {
            data: Arc::new(vec![]),
            format: crate::image::ImageFormat::Png,
            width: 10.0_f32.as_pt(),
            height: 8.0_f32.as_pt(),
            x_offset: crate::units::Pt::ZERO,
            vertical_align: crate::paragraph::VerticalAlign::Baseline,
            opacity: 1.0,
            visible: true,
            computed_y: 15.0_f32.as_pt(),
            link: None,
        };
        let line0 = make_line(12.0, 9.0); // page 0: no items
        let line1 = crate::paragraph::ShapedLine {
            height: 12.0_f32.as_pt(),
            baseline: 21.0_f32.as_pt(),
            items: vec![crate::paragraph::LineItem::Image(img)],
        };
        let fragments = vec![
            make_fragment(0, 16.0), // 16px → 12pt
            make_fragment(1, 16.0),
        ];
        let result = paragraph_lines_for_page(&[line0, line1], &fragments, &[], 1, true);
        assert!(result.is_some(), "page 1 should yield Some");
        let sliced = result.unwrap();
        assert_eq!(sliced.len(), 1, "one line on page 1");
        if let crate::paragraph::LineItem::Image(img) = &sliced[0].items[0] {
            assert!(
                (img.computed_y.to_f32() - 3.0).abs() < 0.01,
                "expected computed_y=3.0 after rebasing, got {:?}",
                img.computed_y,
            );
        } else {
            panic!("expected Image item in sliced line");
        }
    }

    // --- build_struct_tree: H run_entry with missing / empty paragraph ---

    #[test]
    fn build_struct_tree_run_entry_h_no_paragraph_no_backfill() {
        // H node uses per-run tagging but has NO paragraph in drawables.
        // The backfill loop's `if let Some(para)` arm yields None → silent skip.
        let node_id: usize = 40;
        let id = make_identifier();
        let mut tc = crate::draw_primitives::TagCollector::new();
        tc.record_run(
            node_id,
            crate::draw_primitives::ParagraphRunItem::Content(id),
        );
        let mut d = Drawables::new();
        d.semantics.insert(node_id, make_h1_semantic_entry(None));
        let link_annot_ids: BTreeMap<usize, Vec<Identifier>> = BTreeMap::new();
        let mut tree = TagTree::new();
        build_struct_tree(tc, &d, &link_annot_ids, &mut tree);
        assert_eq!(tree.children.len(), 1);
        assert!(matches!(tree.children[0], Node::Group(_)));
    }

    #[test]
    fn build_struct_tree_run_entry_h_empty_title_no_backfill() {
        // H node uses per-run tagging; paragraph exists but contains only an
        // InlineBox (no Text runs). extract_heading_title returns "" →
        // `if !title.is_empty()` is false → heading_titles entry is NOT inserted.
        let node_id: usize = 41;
        let id = make_identifier();
        let mut tc = crate::draw_primitives::TagCollector::new();
        tc.record_run(
            node_id,
            crate::draw_primitives::ParagraphRunItem::Content(id),
        );
        let mut d = Drawables::new();
        d.semantics.insert(node_id, make_h1_semantic_entry(None));
        let ib = crate::paragraph::InlineBoxItem {
            node_id: None,
            width: 50.0_f32.as_pt(),
            height: 20.0_f32.as_pt(),
            x_offset: crate::units::Pt::ZERO,
            computed_y: crate::units::Pt::ZERO,
            link: None,
            opacity: 1.0,
            visible: true,
        };
        let line = crate::paragraph::ShapedLine {
            height: 16.0_f32.as_pt(),
            baseline: 12.0_f32.as_pt(),
            items: vec![crate::paragraph::LineItem::InlineBox(ib)],
        };
        d.paragraphs.insert(node_id, make_para(vec![line]));
        let link_annot_ids: BTreeMap<usize, Vec<Identifier>> = BTreeMap::new();
        let mut tree = TagTree::new();
        build_struct_tree(tc, &d, &link_annot_ids, &mut tree);
        assert_eq!(tree.children.len(), 1);
        assert!(matches!(tree.children[0], Node::Group(_)));
    }

    // --- build_page_skip_sets: table overflow_clip with empty descendants ---

    #[test]
    fn build_page_skip_sets_table_overflow_clip_empty_descendants_not_added() {
        // A table with has_overflow_clip() = true but clip_descendants empty must
        // NOT extend clipped_descendants — the `&& !table.clip_descendants.is_empty()`
        // guard short-circuits before `extend`.
        let mut d = Drawables::new();
        let style = crate::draw_primitives::BlockStyle {
            overflow_x: crate::draw_primitives::Overflow::Clip,
            ..Default::default()
        };
        d.tables.insert(
            50,
            crate::drawables::TableEntry {
                style,
                opacity: 1.0,
                visible: true,
                id: None,
                layout_size: None,
                width: 100.0_f32.as_pt(),
                cached_height: 50.0_f32.as_pt(),
                clip_descendants: vec![],
            },
        );
        let (_, clip, _) = build_page_skip_sets(&d);
        assert!(clip.is_empty(), "empty clip_descendants → nothing added");
    }

    // --- build_multicol_stroke ---

    fn rule_spec(
        width: f32,
        style: crate::column_css::ColumnRuleStyle,
    ) -> crate::column_css::ColumnRuleSpec {
        use crate::units::F32Units;
        crate::column_css::ColumnRuleSpec {
            width: width.as_pt(),
            style,
            color: [0, 0, 0, 255],
        }
    }

    #[test]
    fn build_multicol_stroke_none_style_returns_none() {
        let rule = rule_spec(2.0, crate::column_css::ColumnRuleStyle::None);
        assert!(build_multicol_stroke(&rule).is_none());
    }

    #[test]
    fn build_multicol_stroke_zero_width_returns_none() {
        let rule = rule_spec(0.0, crate::column_css::ColumnRuleStyle::Solid);
        assert!(build_multicol_stroke(&rule).is_none());
    }

    #[test]
    fn build_multicol_stroke_negative_width_returns_none() {
        let rule = rule_spec(-1.0, crate::column_css::ColumnRuleStyle::Solid);
        assert!(build_multicol_stroke(&rule).is_none());
    }

    #[test]
    fn build_multicol_stroke_solid_returns_some_without_dash() {
        let rule = rule_spec(3.0, crate::column_css::ColumnRuleStyle::Solid);
        let stroke = build_multicol_stroke(&rule).expect("solid rule should return Some");
        assert!(stroke.dash.is_none(), "solid rule should have no dash");
        assert_eq!(
            stroke.line_cap,
            krilla::paint::LineCap::Butt,
            "solid default cap is Butt"
        );
        assert!(
            (stroke.width - 3.0).abs() < 0.001,
            "width should match rule.width"
        );
    }

    #[test]
    fn build_multicol_stroke_dashed_has_expected_dash_array() {
        let w = 4.0_f32;
        let rule = rule_spec(w, crate::column_css::ColumnRuleStyle::Dashed);
        let stroke = build_multicol_stroke(&rule).expect("dashed rule should return Some");
        let dash = stroke.dash.expect("dashed rule should have a dash");
        assert_eq!(dash.array, vec![w * 3.0, w * 2.0], "dash array is [3w, 2w]");
        assert!((dash.offset - 0.0).abs() < 0.001, "dash offset should be 0");
    }

    #[test]
    fn build_multicol_stroke_dotted_has_round_cap_and_expected_dash_array() {
        let w = 2.0_f32;
        let rule = rule_spec(w, crate::column_css::ColumnRuleStyle::Dotted);
        let stroke = build_multicol_stroke(&rule).expect("dotted rule should return Some");
        assert_eq!(
            stroke.line_cap,
            krilla::paint::LineCap::Round,
            "dotted cap is Round"
        );
        let dash = stroke.dash.expect("dotted rule should have a dash");
        assert_eq!(dash.array, vec![0.0, w * 2.0], "dash array is [0, 2w]");
        assert!((dash.offset - 0.0).abs() < 0.001, "dash offset should be 0");
    }

    // --- build_struct_tree: missing branches ---

    #[test]
    fn build_struct_tree_run_entry_p_tag_skips_heading_backfill() {
        // A node in run_entries with PdfTag::P (not H).
        // The backfill loop's `if matches!(entry.tag, PdfTag::H { .. })` is false → skip.
        // The tree should still build correctly via build_tag_group's run_entries branch.
        let node_id: usize = 50;
        let id = make_identifier();
        let mut tc = crate::draw_primitives::TagCollector::new();
        tc.record_run(
            node_id,
            crate::draw_primitives::ParagraphRunItem::Content(id),
        );
        let mut d = Drawables::new();
        d.semantics.insert(node_id, make_p_semantic_entry(None));
        let link_annot_ids: BTreeMap<usize, Vec<Identifier>> = BTreeMap::new();
        let mut tree = TagTree::new();
        build_struct_tree(tc, &d, &link_annot_ids, &mut tree);
        assert_eq!(tree.children.len(), 1, "one root Group");
        assert!(matches!(tree.children[0], Node::Group(_)));
    }

    #[test]
    fn build_struct_tree_run_entry_h_already_in_heading_titles_continues() {
        // A node appears in both tc_entries (populating heading_titles) AND run_entries.
        // The backfill loop's `if heading_titles.contains_key(&node_id) { continue; }` fires.
        let node_id: usize = 60;
        let tc_id = make_identifier();
        let run_id = make_identifier();
        let mut tc = crate::draw_primitives::TagCollector::new();
        tc.record(
            node_id,
            crate::tagging::PdfTag::H { level: 1 },
            tc_id,
            Some("Pre-existing title".to_string()),
        );
        tc.record_run(
            node_id,
            crate::draw_primitives::ParagraphRunItem::Content(run_id),
        );
        let mut d = Drawables::new();
        d.semantics.insert(node_id, make_h1_semantic_entry(None));
        let text_line = make_shaped_line(vec![crate::paragraph::LineItem::Text(make_glyph_run(
            "Different title",
            None,
        ))]);
        d.paragraphs.insert(node_id, make_para(vec![text_line]));
        let link_annot_ids: BTreeMap<usize, Vec<Identifier>> = BTreeMap::new();
        let mut tree = TagTree::new();
        build_struct_tree(tc, &d, &link_annot_ids, &mut tree);
        assert_eq!(tree.children.len(), 1, "one root Group");
        assert!(matches!(tree.children[0], Node::Group(_)));
    }

    // --- decode_image_for_v2: Jpeg and Gif format branches ---

    fn make_image_entry(
        format: crate::image::ImageFormat,
        data: Vec<u8>,
    ) -> crate::drawables::ImageEntry {
        crate::drawables::ImageEntry {
            image_data: Arc::new(data),
            format,
            width: 10.0_f32.as_pt(),
            height: 10.0_f32.as_pt(),
            opacity: 1.0,
            visible: true,
        }
    }

    #[test]
    fn decode_image_for_v2_jpeg_invalid_data_returns_none() {
        let entry = make_image_entry(crate::image::ImageFormat::Jpeg, vec![0xFF, 0xD8, 0xFF]);
        let result = decode_image_for_v2(&Default::default(), &entry);
        assert!(result.is_none(), "invalid JPEG bytes should return None");
    }

    #[test]
    fn decode_image_for_v2_gif_invalid_data_returns_none() {
        let entry = make_image_entry(crate::image::ImageFormat::Gif, b"GIF89a".to_vec());
        let result = decode_image_for_v2(&Default::default(), &entry);
        assert!(result.is_none(), "invalid GIF bytes should return None");
    }

    // --- MarginBoxRenderer::new: non-empty GCPM mapping branches ---

    fn empty_geometry() -> crate::pagination_layout::PaginationGeometryTable {
        BTreeMap::new()
    }

    #[test]
    fn margin_box_renderer_new_with_string_set_mappings_takes_else_branch() {
        // Non-empty string_set_mappings forces the else branch at
        // `collect_string_set_states(pagination_geometry, &by_node_btree)`.
        let gcpm = crate::gcpm::GcpmContext {
            string_set_mappings: vec![crate::gcpm::StringSetMapping {
                parsed: crate::gcpm::ParsedSelector::Class("hd".to_string()),
                name: "chapter".to_string(),
                values: vec![crate::gcpm::StringSetValue::ContentText],
            }],
            ..Default::default()
        };
        let store = RunningElementStore::default();
        let geom = empty_geometry();
        let implicit: BTreeMap<usize, String> = BTreeMap::new();
        let _mbr = MarginBoxRenderer::new(
            &gcpm,
            &store,
            &[],
            false,
            &BTreeMap::new(),
            &geom,
            &HashMap::new(),
            &BTreeMap::new(),
            1,
            &implicit,
        );
    }

    #[test]
    fn margin_box_renderer_new_with_running_mappings_takes_else_branch() {
        // Non-empty running_mappings forces the else branch at
        // `collect_running_element_states(pagination_geometry, running_store)`.
        let gcpm = crate::gcpm::GcpmContext {
            running_mappings: vec![crate::gcpm::RunningMapping {
                parsed: crate::gcpm::ParsedSelector::Class("hd".to_string()),
                running_name: "header".to_string(),
            }],
            ..Default::default()
        };
        let store = RunningElementStore::default();
        let geom = empty_geometry();
        let implicit: BTreeMap<usize, String> = BTreeMap::new();
        let _mbr = MarginBoxRenderer::new(
            &gcpm,
            &store,
            &[],
            false,
            &BTreeMap::new(),
            &geom,
            &HashMap::new(),
            &BTreeMap::new(),
            1,
            &implicit,
        );
    }

    #[test]
    fn margin_box_renderer_new_with_counter_mappings_takes_else_branch() {
        // Non-empty counter_mappings forces the else branch at
        // `collect_counter_states(pagination_geometry, counter_ops_by_node)`.
        let gcpm = crate::gcpm::GcpmContext {
            counter_mappings: vec![crate::gcpm::CounterMapping {
                parsed: crate::gcpm::ParsedSelector::Tag("h1".to_string()),
                ops: vec![crate::gcpm::CounterOp::Reset {
                    name: "chapter".to_string(),
                    value: 0,
                }],
            }],
            ..Default::default()
        };
        let store = RunningElementStore::default();
        let geom = empty_geometry();
        let implicit: BTreeMap<usize, String> = BTreeMap::new();
        let _mbr = MarginBoxRenderer::new(
            &gcpm,
            &store,
            &[],
            false,
            &BTreeMap::new(),
            &geom,
            &HashMap::new(),
            &BTreeMap::new(),
            1,
            &implicit,
        );
    }

    // --- F103: margin-box content aligned in its rectangle ---

    fn rect(w_pt: f32, h_pt: f32) -> MarginBoxRect {
        MarginBoxRect {
            x: 0.0.as_pt(),
            y: 0.0.as_pt(),
            width: w_pt.as_pt(),
            height: h_pt.as_pt(),
        }
    }

    #[test]
    fn resolved_box_measure_html_is_what_it_always_was() {
        // The measure passes and the edge distribution see the content exactly as before the alignment wrapper,
        // so the measured sizes, the rectangles and the measure-cache keys do not change.
        let bare = ResolvedBox::new("x".to_string(), "");
        assert_eq!(bare.measure_html, "x");
        assert_eq!(bare.content_html, "x");
        let styled = ResolvedBox::new("x".to_string(), "font-size: 9pt; content: \"a\"");
        assert_eq!(
            styled.measure_html,
            "<div style=\"font-size: 9pt; content: &quot;a&quot;\">x</div>"
        );
        assert_eq!(styled.content_html, "x");
    }

    #[test]
    fn cell_html_puts_the_defaults_first_and_the_authors_declarations_after() {
        let b = ResolvedBox::new("Hello".to_string(), "text-align: left; vertical-align: top");
        let html = margin_box_cell_html(MarginBoxPosition::BottomCenter, &b, rect(300.0, 30.0));
        let defaults = html
            .find("text-align:center;vertical-align:middle;")
            .expect("the UA defaults of @bottom-center");
        let author = html
            .find("text-align: left; vertical-align: top")
            .expect("the author's declarations");
        assert!(
            defaults < author,
            "the author's declarations come last: {html}"
        );
        assert!(html.contains(">Hello</div>"), "{html}");
        // 300 pt = 400 px, 30 pt = 40 px: the table is exactly the rectangle.
        assert!(html.contains("width:400px;height:40px"), "{html}");
        assert!(html.contains("table-layout:fixed"), "{html}");
        assert!(
            html.contains("box-sizing:border-box;width:400px;"),
            "the cell is as wide as the rectangle: {html}"
        );
    }

    #[test]
    fn cell_html_escapes_the_declarations() {
        let b = ResolvedBox::new(
            "x".to_string(),
            "font-family: \"A\"; background: url(\"a&b\") /* <b> */",
        );
        let html = margin_box_cell_html(MarginBoxPosition::TopLeft, &b, rect(100.0, 30.0));
        assert!(
            html.contains(
                "font-family: &quot;A&quot;; background: url(&quot;a&amp;b&quot;) /* &lt;b&gt; */"
            ),
            "{html}"
        );
        assert!(!html.contains("<b>"), "{html}");
        // The forced zero margin comes after the author's declarations (Blitz cannot place a cell with a margin).
        assert!(
            html.find("a&amp;b").unwrap() < html.find("margin:0 !important").unwrap(),
            "{html}"
        );
    }

    #[test]
    fn top_left_and_top_right_same_text_differ() {
        // The render cache is keyed by the HTML and the size: two boxes with the same text and the same
        // rectangle size must not share one entry, or one of them would be painted with the other's alignment.
        let b = ResolvedBox::new("Same".to_string(), "");
        let r = rect(150.0, 40.0);
        let left = margin_box_cell_html(MarginBoxPosition::TopLeft, &b, r);
        let right = margin_box_cell_html(MarginBoxPosition::TopRight, &b, r);
        assert_ne!(left, right);
        assert!(left.contains("text-align:left;"), "{left}");
        assert!(right.contains("text-align:right;"), "{right}");
        // Boxes with the same alignment share the entry, as before.
        let center_a = margin_box_cell_html(MarginBoxPosition::TopCenter, &b, r);
        let center_b = margin_box_cell_html(MarginBoxPosition::BottomCenter, &b, r);
        assert_eq!(center_a, center_b);
    }

    // --- F99b: margin-box documents built from the render's shared environment ---

    fn noto_faces() -> Vec<Arc<Vec<u8>>> {
        ["Regular", "Bold"]
            .iter()
            .map(|w| {
                Arc::new(
                    std::fs::read(format!(
                        "{}/../../examples/.fonts/NotoSans-{w}.ttf",
                        env!("CARGO_MANIFEST_DIR")
                    ))
                    .expect("bundled Noto Sans"),
                )
            })
            .collect()
    }

    /// Every generic family the UA sheet and the default `font-family` use, mapped to the bundled face: without a
    /// mapping the text of a document with bundled fonts only has no glyph run, and the dumps below would be empty.
    fn noto_generics() -> BTreeMap<String, String> {
        ["serif", "sans-serif", "monospace"]
            .iter()
            .map(|g| (g.to_string(), "Noto Sans".to_string()))
            .collect()
    }

    /// What `MarginBoxRenderer::render_page` does with a laid-out margin document (stage 3), as a string.
    fn margin_doc_dump(mut doc: blitz_html::HtmlDocument, height_px: crate::units::Px) -> String {
        let geometry = crate::pagination_layout::run_pass_with_break_styles(
            &mut doc,
            height_px,
            &crate::column_css::ColumnStyleTable::new(),
        );
        let store = RunningElementStore::new();
        let mut ctx = crate::convert::ConvertContext {
            running_store: &store,
            assets: None,
            font_bytes: Default::default(),
            string_set_by_node: HashMap::new(),
            counter_ops_by_node: HashMap::new(),
            bookmark_by_node: HashMap::new(),
            column_styles: crate::column_css::ColumnStyleTable::new(),
            multicol_geometry: crate::multicol_layout::MulticolGeometryTable::new(),
            pagination_geometry: geometry,
            link_cache: Default::default(),
            dropped_text: Default::default(),
            svg_checks: None,
            viewport_size_px: None,
        };
        let drawables = crate::convert::dom_to_drawables(&doc, &mut ctx);
        // `ParagraphEntry`'s `Debug` prints only the line count: spell out what is shaped.
        let mut text = String::new();
        for (id, entry) in drawables.paragraphs.iter() {
            for line in &entry.lines {
                text.push_str(&format!(
                    "node {id}: line h={:?} baseline={:?}\n",
                    line.height, line.baseline
                ));
                for item in &line.items {
                    if let crate::paragraph::LineItem::Text(run) = item {
                        text.push_str(&format!(
                            "  run {:?} size={:?} color={:?} x={:?} font={}#{} glyphs={:?}\n",
                            run.text,
                            run.font_size,
                            run.color,
                            run.x_offset,
                            run.font_data.len(),
                            run.font_index,
                            run.glyphs
                        ));
                    }
                }
            }
        }
        format!("{drawables:#?}\n{text}{:#?}", ctx.pagination_geometry)
    }

    /// The document of `margin_wrapper_style`: the old wrapper (`<style>` in `<head>`) or, when the environment
    /// carries the CSS, the new one. The new one has a `<title>` where the old had a `<style>`, so that both
    /// documents have the same node ids (a `<title>` with text is two nodes, like a `<style>` with text) and
    /// their drawables can be compared as they are. The production wrapper has no such element.
    fn wrapper_pair(
        css: &str,
        body: &str,
        env: &crate::blitz_adapter::MarginDocEnv,
    ) -> (String, String) {
        let old = format!(
            "<html><head><style>{css}</style></head><body style=\"margin:0;padding:0;\">{body}</body></html>"
        );
        let head = if env.author_sheet_attached() {
            format!("<title>{}</title>", if css.is_empty() { "" } else { "x" })
        } else {
            margin_wrapper_style(env, css)
        };
        let new = format!(
            "<html><head>{head}</head><body style=\"margin:0;padding:0;\">{body}</body></html>"
        );
        (old, new)
    }

    /// The margin documents of `MarginDocEnv` and the ones the old per-document wrapper built are the same
    /// document: same drawables, same geometry, same measured widths.
    #[test]
    fn margin_env_matches_fresh_document() {
        let fonts = noto_faces();
        let generics = noto_generics();
        let contents = [
            "Strana 7 / 200",
            "<div style=\"font-weight:700;letter-spacing:1px\">Faktura <b>2026</b> <i>x</i></div>",
            "<div class=\"c\">klasa</div><p class=\"d\">Odstavec</p><pre>mono</pre>",
            // A `<style>` inside the content comes after the shared sheet.
            "<style>.c{color:green;font-size:23px}</style><div class=\"c\">vlastni styl</div>",
            // ... also when it repeats the render's CSS.
            "<style>.c{color:blue;font-size:20px}</style><div class=\"c\">stejny styl</div>",
            "<div style=\"position:fixed;bottom:0\">pevne</div>",
        ];
        let sheets = [
            "",
            ".c{color:blue;font-size:20px}",
            "a{color:red}\r\n.c{font-size:20px}\r\n.d{margin:3px}\r\n",
            ".c::before{content:\"a &amp; b \"} .c{font-size:11px} .d{color:#123}",
            "@import url(other.css); .c{margin:3px;font-weight:700}",
            "@media print{.c{display:none}} @media screen{.c{letter-spacing:2px}}",
            // Ends the wrapper's own `<style>` early, so the environment keeps the old wrapper for it.
            ".c{color:red} /* </style> */ .d{font-size:9px}",
        ];
        let (w, h) = (300.0_f32.as_px(), 60.0_f32.as_px());
        let mut attached = 0;
        for css in sheets {
            let env = crate::blitz_adapter::MarginDocEnv::new(css, &fonts, false, &generics);
            attached += usize::from(env.author_sheet_attached());
            for body in contents {
                for (what, body) in [
                    ("render", body.to_string()),
                    (
                        "measure",
                        format!("<div style=\"display:inline-block\">{body}</div>"),
                    ),
                ] {
                    let (old, new) = wrapper_pair(css, &body, &env);
                    let old_doc = crate::blitz_adapter::parse_and_layout_with_generics(
                        &old, w, h, &fonts, false, &generics,
                    );
                    let new_doc =
                        crate::blitz_adapter::parse_and_layout_margin_doc(&env, &new, w, h);
                    assert_eq!(
                        get_body_child_dimension(&old_doc, true),
                        get_body_child_dimension(&new_doc, true),
                        "{what} width: css {css:?}, content {body:?}"
                    );
                    assert_eq!(
                        get_body_child_dimension(&old_doc, false),
                        get_body_child_dimension(&new_doc, false),
                        "{what} height: css {css:?}, content {body:?}"
                    );
                    let (old_dump, new_dump) =
                        (margin_doc_dump(old_doc, h), margin_doc_dump(new_doc, h));
                    assert_eq!(
                        old_dump, new_dump,
                        "{what} drawables: css {css:?}, content {body:?}"
                    );
                    // (Text directly in `<body>` gets no paragraph in either path; boxes with declarations, which
                    // is every real one, wrap their content in a `<div>`.)
                    assert!(
                        !body.contains('<') || old_dump.contains("  run "),
                        "no shaped text, the comparison is empty: {what}, css {css:?}, content {body:?}\n{old_dump:.2000}"
                    );
                }
            }
        }
        assert_eq!(
            attached,
            sheets.len() - 1,
            "all but the `</style>` sheet are attached"
        );
    }

    /// The wrapper of the production path really has no `<style>` when the environment carries the CSS, and the
    /// CSS reaches the document anyway.
    #[test]
    fn margin_env_attaches_the_render_css_without_a_style_element() {
        let fonts = noto_faces();
        let env = crate::blitz_adapter::MarginDocEnv::new(
            ".c{font-size:30px}",
            &fonts,
            false,
            &noto_generics(),
        );
        assert!(env.author_sheet_attached());
        assert_eq!(margin_wrapper_style(&env, ".c{font-size:30px}"), "");
        let (w, h) = (300.0_f32.as_px(), 60.0_f32.as_px());
        let doc = crate::blitz_adapter::parse_and_layout_margin_doc(
            &env,
            "<html><head></head><body style=\"margin:0\"><div class=\"c\">x</div></body></html>",
            w,
            h,
        );
        let big = get_body_child_dimension(&doc, false);
        let plain = crate::blitz_adapter::parse_and_layout_margin_doc(
            &env,
            "<html><head></head><body style=\"margin:0\"><div>x</div></body></html>",
            w,
            h,
        );
        assert!(
            big > get_body_child_dimension(&plain, false),
            "the class rule of the render's CSS applies"
        );
    }

    // ── Inline smoke tests: exercise rendering paths for --lib coverage ──────
    //
    // Integration smoke tests in crates/fulgur/tests/render_smoke.rs are
    // excluded from `cargo llvm-cov --lib` measurement. These inline tests
    // exercise the same rendering functions (draw_block_v2, draw_paragraph_v2,
    // draw_image_v2, draw_svg_v2, draw_table_v2, draw_under_transform/clip/
    // opacity, try_start_tagged, paint_multicol_paragraph_slices, …) through
    // Engine::render_html so the coverage gate sees them.

    fn render_html(html: &str) -> Vec<u8> {
        crate::engine::Engine::builder()
            .build()
            .render(html)
            .expect("render failed")
    }

    // Valid 1×1 red PNG for image tests (same fixture used in render_smoke.rs).
    const RED_1X1_PNG: &[u8] = &[
        0x89, 0x50, 0x4E, 0x47, 0x0D, 0x0A, 0x1A, 0x0A, 0x00, 0x00, 0x00, 0x0D, 0x49, 0x48, 0x44,
        0x52, 0x00, 0x00, 0x00, 0x01, 0x00, 0x00, 0x00, 0x01, 0x08, 0x02, 0x00, 0x00, 0x00, 0x90,
        0x77, 0x53, 0xDE, 0x00, 0x00, 0x00, 0x0C, 0x49, 0x44, 0x41, 0x54, 0x78, 0x9C, 0x63, 0xF8,
        0xCF, 0xC0, 0x00, 0x00, 0x03, 0x01, 0x01, 0x00, 0xC9, 0xFE, 0x92, 0xEF, 0x00, 0x00, 0x00,
        0x00, 0x49, 0x45, 0x4E, 0x44, 0xAE, 0x42, 0x60, 0x82,
    ];

    // --- draw_block_v2 / draw_block_inner_paint / draw_paragraph_v2 ---

    #[test]
    fn render_smoke_block_with_background_and_border() {
        // Exercises draw_block_v2 → draw_block_inner_paint (bg + border paint).
        let pdf = render_html(
            r#"<!doctype html><html><body>
            <div style="width:100px;height:60px;background:#cef;
                        border:2px solid #44a;padding:8px;">
              content
            </div>
            </body></html>"#,
        );
        assert!(pdf.starts_with(b"%PDF"));
    }

    #[test]
    fn render_smoke_paragraph_with_text() {
        // Exercises draw_paragraph_v2 → draw_paragraph_inner_paint →
        // paragraph_lines_for_page (non-split) → draw_shaped_lines text arm.
        let pdf = render_html(r#"<!doctype html><html><body><p>Hello, world.</p></body></html>"#);
        assert!(pdf.starts_with(b"%PDF"));
    }

    #[test]
    fn render_smoke_block_with_inline_text_content() {
        // Exercises draw_block_with_inner_content (block + inline paragraph)
        // and dispatch_fragment's paragraph-without-block path.
        let pdf = render_html(
            r#"<!doctype html><html><body>
            <p style="background:#fee;border:1px solid #c88;padding:4px;">
              Styled inline text block.
            </p>
            </body></html>"#,
        );
        assert!(pdf.starts_with(b"%PDF"));
    }

    // --- draw_list_item_with_block / draw_list_item_marker ---

    #[test]
    fn render_smoke_unordered_list_with_disc_markers() {
        // Exercises draw_list_item_with_block + draw_list_item_marker
        // (outside/disc default marker path).
        let pdf = render_html(
            r#"<!doctype html><html><body>
            <ul><li>Alpha</li><li>Beta</li><li>Gamma</li></ul>
            </body></html>"#,
        );
        assert!(pdf.starts_with(b"%PDF"));
    }

    #[test]
    fn render_smoke_ordered_list_with_decimal_markers() {
        // Exercises draw_list_item_marker's counter text path.
        let pdf = render_html(
            r#"<!doctype html><html><body>
            <ol><li>One</li><li>Two</li><li>Three</li></ol>
            </body></html>"#,
        );
        assert!(pdf.starts_with(b"%PDF"));
    }

    #[test]
    fn render_smoke_list_style_inside() {
        // list-style-position: inside routes through a different marker
        // placement path in draw_list_item_marker.
        let pdf = render_html(
            r#"<!doctype html><html><body>
            <ul style="list-style-position:inside">
              <li>Item A</li><li>Item B</li>
            </ul>
            </body></html>"#,
        );
        assert!(pdf.starts_with(b"%PDF"));
    }

    // --- draw_image_v2 / draw_image_inner_paint / decode_image_for_v2 ---

    #[test]
    fn render_smoke_raster_image_in_flow() {
        // Exercises draw_image_v2 → draw_image_inner_paint → decode_image_for_v2
        // PNG branch. The image is registered via AssetBundle.
        let mut bundle = crate::asset::AssetBundle::default();
        bundle.add_image("img.png", RED_1X1_PNG.to_vec());
        let pdf = crate::engine::Engine::builder()
            .assets(bundle)
            .build()
            .render(
                r#"<!doctype html><html><body>
                <img src="img.png" style="width:64px;height:64px;">
                </body></html>"#,
            )
            .expect("render");
        assert!(pdf.starts_with(b"%PDF"));
    }

    // --- draw_svg_v2 / draw_svg_inner_paint ---

    #[test]
    fn render_smoke_svg_inline_in_flow() {
        // Exercises draw_svg_v2 → draw_svg_inner_paint.
        let pdf = render_html(
            r##"<!doctype html><html><body>
            <svg xmlns="http://www.w3.org/2000/svg" width="60" height="60">
              <rect x="5" y="5" width="50" height="50" fill="#3af"/>
            </svg>
            </body></html>"##,
        );
        assert!(pdf.starts_with(b"%PDF"));
    }

    // --- draw_table_v2 / paint_table_outer_frame ---

    #[test]
    fn render_smoke_table_with_borders() {
        // Exercises draw_table_v2 → table_box_size + paint_table_outer_frame.
        let pdf = render_html(
            r#"<!doctype html><html><body>
            <table border="1" style="border-collapse:collapse">
              <tr><th>A</th><th>B</th></tr>
              <tr><td>1</td><td>2</td></tr>
            </table>
            </body></html>"#,
        );
        assert!(pdf.starts_with(b"%PDF"));
    }

    /// F22: a table in the collapsing border model paints its lines from the table: on the page, inside the wrappers
    /// (transform, overflow clip, opacity group) and under the repeated header of a table that breaks over pages.
    fn collapsed_rows(n: usize) -> String {
        (0..n)
            .map(|i| format!("<tr><td>Row {i}</td><td>x</td></tr>"))
            .collect()
    }

    const COLLAPSED_CSS: &str = "<style>table{border-collapse:collapse;width:100%}\
                                 td,th{padding:4px;border:1px solid #888}\
                                 thead{border-bottom:3px solid blue}tr{border-bottom:2px solid red}</style>";

    #[test]
    fn render_smoke_collapsed_table_every_wrapper() {
        for wrapper in [
            "<div>",
            "<div style=\"transform:rotate(2deg)\">",
            "<div style=\"overflow:hidden\">",
            "<div style=\"opacity:0.5\">",
            "<div style=\"overflow:hidden;opacity:0.5\">",
            "<div style=\"transform:scale(0.9)\"><div style=\"overflow:hidden\">",
        ] {
            let close = "</div>".repeat(wrapper.matches("<div").count());
            let pdf = render_html(&format!(
                "<!doctype html><html><head>{COLLAPSED_CSS}</head><body>{wrapper}\
                 <table><thead><tr><th>A</th><th>B</th></tr></thead><tbody>{}</tbody></table>{close}\
                 </body></html>",
                collapsed_rows(4)
            ));
            assert!(pdf.starts_with(b"%PDF"), "{wrapper}");
        }
    }

    #[test]
    fn render_smoke_collapsed_table_overflow_clip_and_opacity_on_the_table() {
        for style in [
            "overflow:hidden",
            "opacity:0.5",
            "transform:rotate(1deg)",
            "visibility:hidden",
        ] {
            let pdf = render_html(&format!(
                "<!doctype html><html><head>{COLLAPSED_CSS}</head><body>\
                 <table style=\"{style}\"><thead><tr><th>A</th><th>B</th></tr></thead>\
                 <tbody>{}</tbody></table></body></html>",
                collapsed_rows(4)
            ));
            assert!(pdf.starts_with(b"%PDF"), "{style}");
        }
    }

    #[test]
    fn render_smoke_collapsed_table_over_pages_with_a_repeated_header_tagged() {
        for tagged in [false, true] {
            let pdf = crate::engine::Engine::builder()
                .tagged(tagged)
                .build()
                .render(&format!(
                    "<!doctype html><html><head>{COLLAPSED_CSS}</head><body>\
                     <table><thead><tr><th>A</th><th>B</th></tr></thead>\
                     <tbody>{}</tbody><tfoot><tr><td>Sum</td><td>1</td></tr></tfoot></table>\
                     </body></html>",
                    collapsed_rows(120)
                ))
                .expect("render failed");
            assert!(pdf.starts_with(b"%PDF"));
        }
    }

    #[test]
    fn render_smoke_collapsed_table_with_spans_hidden_and_mixed_styles() {
        let pdf = render_html(
            r#"<!doctype html><html><head><style>table{border-collapse:collapse}
              td{padding:4px}</style></head><body>
            <table style="border:4px double navy">
              <tbody style="border-top:3px dashed green">
                <tr style="border-bottom:2px dotted red"><td style="border:2px groove gray">A</td>
                  <td style="border-left:hidden">B</td><td style="border:3px inset #444">C</td></tr>
                <tr><td colspan="2" style="border-bottom:5px ridge teal">AB</td><td>C</td></tr>
                <tr><td>x</td></tr>
              </tbody></table></body></html>"#,
        );
        assert!(pdf.starts_with(b"%PDF"));
    }

    #[test]
    fn render_smoke_table_overflow_clip() {
        // Exercises draw_under_clip_table when a table has overflow:hidden
        // and descendant cells need clipping.
        let pdf = render_html(
            r#"<!doctype html><html><body>
            <table style="width:200px;overflow:hidden;background:#eef">
              <tr>
                <td style="background:#cef;padding:4px">Cell 1</td>
                <td style="background:#fce;padding:4px">Cell 2</td>
              </tr>
            </table>
            </body></html>"#,
        );
        assert!(pdf.starts_with(b"%PDF"));
    }

    // --- draw_under_transform ---

    #[test]
    fn render_smoke_transform_rotate() {
        // Exercises draw_under_transform (the `transform: rotate(…)` path).
        let pdf = render_html(
            r#"<!doctype html><html><body>
            <div style="width:80px;height:40px;background:#cef;transform:rotate(15deg)">
              rotated
            </div>
            </body></html>"#,
        );
        assert!(pdf.starts_with(b"%PDF"));
    }

    // --- draw_under_clip ---

    #[test]
    fn render_smoke_overflow_hidden_clips_child() {
        // Exercises draw_under_clip with a child that overflows its container.
        let pdf = render_html(
            r#"<!doctype html><html><body>
            <div style="width:80px;height:40px;overflow:hidden;background:#cef">
              <div style="width:200px;height:20px;background:#f99">clipped</div>
            </div>
            </body></html>"#,
        );
        assert!(pdf.starts_with(b"%PDF"));
    }

    // --- draw_under_opacity ---

    #[test]
    fn render_smoke_opacity_on_block_with_child_svg() {
        // Exercises draw_under_opacity: a block with fractional opacity wrapping
        // an SVG child forces the opacity_descendants path.
        let pdf = render_html(
            r##"<!doctype html><html><body>
            <div style="opacity:0.5">
              <svg xmlns="http://www.w3.org/2000/svg" width="40" height="40">
                <circle cx="20" cy="20" r="15" fill="#e74"/>
              </svg>
            </div>
            </body></html>"##,
        );
        assert!(pdf.starts_with(b"%PDF"));
    }

    // --- try_start_tagged / finish_tagged ---

    #[test]
    fn render_smoke_tagged_pdf_headings_and_paragraphs() {
        // Exercises try_start_tagged + finish_tagged for H and P entries.
        let pdf = crate::engine::Engine::builder()
            .tagged(true)
            .lang("en")
            .build()
            .render(
                r#"<!doctype html><html><body>
                <h1>Heading One</h1>
                <p>A paragraph of text.</p>
                <h2>Heading Two</h2>
                <p>Another paragraph.</p>
                </body></html>"#,
            )
            .expect("tagged render");
        assert!(pdf.starts_with(b"%PDF"));
    }

    // --- paint_multicol_paragraph_slices ---

    #[test]
    fn render_smoke_multicol_with_paragraph_text() {
        // Exercises paint_multicol_paragraph_slices: a multicol container whose
        // columns each hold inline text. Without this, the slice-distribution
        // logic for paragraph spans across multicol columns is untouched.
        let pdf = render_html(
            r#"<!doctype html><html><body>
            <div style="column-count:2;column-gap:20px;width:400px">
              <p>Column text alpha beta gamma delta epsilon zeta eta theta.</p>
              <p>Second paragraph iota kappa lambda mu nu xi omicron pi rho.</p>
            </div>
            </body></html>"#,
        );
        assert!(pdf.starts_with(b"%PDF"));
    }

    // --- render_page (MarginBoxRenderer) / draw_v2_page ---

    #[test]
    fn render_smoke_page_margin_box_bottom_center() {
        // Exercises draw_v2_page → MarginBoxRenderer::render_page for an
        // @bottom-center margin box — the most common GCPM use case.
        let pdf = render_html(
            r#"<!doctype html><html><head><style>
            @page {
              size: A4; margin: 20mm;
              @bottom-center { content: "Page footer"; }
            }
            </style></head><body><p>Body content.</p></body></html>"#,
        );
        assert!(pdf.starts_with(b"%PDF"));
    }

    #[test]
    fn render_smoke_page_with_body_background_multi_page() {
        // Exercises paint_root_block_v2 (html + body pre-passes on every page)
        // and draw_v2_page across multiple pages.
        let pdf = render_html(
            r#"<!doctype html><html><head><style>
            html,body{margin:0;background:#fafafa}
            .tall{height:900px;background:#cef}
            </style></head><body>
            <div class="tall"></div><div class="tall"></div>
            </body></html>"#,
        );
        assert!(pdf.starts_with(b"%PDF"));
    }

    // --- dispatch_fragment: visibility skip path ---

    #[test]
    fn render_smoke_visibility_hidden_element_skipped() {
        // Exercises the `!block.visible` early-return in dispatch_fragment
        // (and draw_block_v2). The element should not paint but render must succeed.
        let pdf = render_html(
            r#"<!doctype html><html><body>
            <div style="visibility:hidden;width:100px;height:50px;background:red"></div>
            <div style="width:100px;height:50px;background:#cef">visible</div>
            </body></html>"#,
        );
        assert!(pdf.starts_with(b"%PDF"));
    }

    // --- decode_image_for_v2: valid PNG path (non-trivial decode) ---

    #[test]
    fn render_smoke_png_image_decode_success() {
        // Exercises decode_image_for_v2's Png branch with valid data, ensuring
        // ImageFormat::to_krilla_image returns Ok and draw_image_inner_paint fires.
        let mut bundle = crate::asset::AssetBundle::default();
        bundle.add_image("red.png", RED_1X1_PNG.to_vec());
        let pdf = crate::engine::Engine::builder()
            .assets(bundle)
            .build()
            .render(
                r#"<!doctype html><html><body style="margin:0">
                <div style="background:url(red.png);width:80px;height:80px"></div>
                </body></html>"#,
            )
            .expect("render");
        assert!(pdf.starts_with(b"%PDF"));
    }

    // --- bookmarks path in render_v2 ---

    #[test]
    fn render_smoke_bookmarks_from_headings() {
        // Exercises the bookmark_collector path in render_v2:
        // bookmark_collector.set_current_page(page_idx) and the BookmarkPass
        // in engine.rs (UA CSS bookmark_mappings → effective_bookmarks).
        let pdf = crate::engine::Engine::builder()
            .bookmarks(true)
            .build()
            .render(
                r#"<!doctype html><html><body>
                <h1>Chapter One</h1>
                <p>Body content for chapter one.</p>
                <h2>Section 1.1</h2>
                <p>Content for section 1.1.</p>
                <h2>Section 1.2</h2>
                <p>Content for section 1.2.</p>
                </body></html>"#,
            )
            .expect("render");
        assert!(pdf.starts_with(b"%PDF"));
    }

    // --- PDF/UA mode (Validator::UA1) ---

    #[test]
    fn render_smoke_pdf_ua_mode() {
        // Exercises the Validator::UA1 configuration branch in render_v2:
        // Configuration::new_with_validator(Validator::UA1) when config.pdf_ua = true.
        // pdf_ua implies both enable_tagging and effective_bookmarks.
        // PDF/UA-1 requires a document title (NoDocumentTitle validation).
        let pdf = crate::engine::Engine::builder()
            .pdf_ua(true)
            .lang("en")
            .title("PDF/UA Test Document")
            .build()
            .render(
                r#"<!doctype html><html><body>
                <h1>Title</h1>
                <p>A paragraph of body text.</p>
                </body></html>"#,
            )
            .expect("render");
        assert!(pdf.starts_with(b"%PDF"));
    }

    // --- try_start_tagged: PdfTag::Li branch + draw_list_item_marker_tagged ---

    #[test]
    fn render_smoke_tagged_unordered_list() {
        // Exercises try_start_tagged's PdfTag::Li branch (lines ~924-931),
        // draw_list_item_marker_tagged with lbl_id set, and the li_lbody_ids
        // path in draw_list_item_with_block. Requires tagged mode to activate
        // the tagging collector that populates li_lbl_ids / li_lbody_ids.
        let pdf = crate::engine::Engine::builder()
            .tagged(true)
            .lang("en")
            .build()
            .render(
                r#"<!doctype html><html><body>
                <ul>
                  <li>First item</li>
                  <li>Second item</li>
                  <li>Third item</li>
                </ul>
                </body></html>"#,
            )
            .expect("render");
        assert!(pdf.starts_with(b"%PDF"));
    }

    // --- use_run_tagging = true path (tagged paragraph containing a hyperlink) ---

    #[test]
    fn render_smoke_tagged_paragraph_with_hyperlink() {
        // When tagging is enabled and a paragraph contains an <a href>, the
        // paragraph's glyph runs include LinkSpan entries → para_has_link_runs
        // returns true → use_run_tagging = true in dispatch_fragment.
        // Exercises: link_run_node_id assignment, per-run TagCollector recording.
        let pdf = crate::engine::Engine::builder()
            .tagged(true)
            .lang("en")
            .build()
            .render(
                r#"<!doctype html><html><body>
                <p>Visit <a href="https://example.com">our website</a> for more info.</p>
                <p>Another <a href="https://other.example.com">link here</a> too.</p>
                </body></html>"#,
            )
            .expect("render");
        assert!(pdf.starts_with(b"%PDF"));
    }

    // --- link annotation path (non-tagged, link_collector emission) ---

    #[test]
    fn render_smoke_paragraph_with_external_link() {
        // In non-tagged mode, <a href="https://..."> produces a link annotation
        // via link_collector. Exercises draw_shaped_lines link annotation path
        // and dest_registry external link emission.
        let pdf = render_html(
            r#"<!doctype html><html><body>
            <p>See <a href="https://example.com">example.com</a> for details.</p>
            <p>Also see <a href="https://other.org">other.org</a>.</p>
            </body></html>"#,
        );
        assert!(pdf.starts_with(b"%PDF"));
    }

    // --- dest_registry.record: internal anchor id on a paragraph node ---

    #[test]
    fn render_smoke_internal_anchor_link() {
        // An element with an `id` attribute is registered in dest_registry
        // (render_v2 pre-pass, lines ~93-121). An <a href="#target"> produces
        // an internal link annotation pointing to that destination.
        let pdf = render_html(
            r##"<!doctype html><html><body>
            <p id="intro">Introduction paragraph.</p>
            <p>Go to <a href="#intro">the intro</a>.</p>
            </body></html>"##,
        );
        assert!(pdf.starts_with(b"%PDF"));
    }

    // --- nested overflow:hidden blocks (draw_under_clip recursive call) ---

    #[test]
    fn render_smoke_nested_overflow_clip_blocks() {
        // A block with overflow:hidden containing another block with overflow:hidden
        // triggers the nested-clip recursion in draw_under_clip (lines ~1866-1893):
        // the outer clip's descendant loop dispatches inner clip blocks via a
        // recursive draw_under_clip call so each has its own push_clip_path.
        let pdf = render_html(
            r#"<!doctype html><html><body>
            <div style="width:120px;height:80px;overflow:hidden;background:#def">
              <div style="width:90px;height:60px;overflow:hidden;background:#fed">
                <p style="width:200px">Clipped text in nested overflow hidden blocks.</p>
              </div>
            </div>
            </body></html>"#,
        );
        assert!(pdf.starts_with(b"%PDF"));
    }

    // --- image inside overflow:hidden (draw_under_clip img_for_block path) ---

    #[test]
    fn render_smoke_image_inside_overflow_clip() {
        // A block sharing node_id with an image, inside an overflow:hidden container,
        // triggers draw_under_clip's img_for_block path (lines ~1770-1772):
        // draw_image_inner_paint is called at the content-box-offset coordinates
        // inside the pushed clip path.
        let mut bundle = crate::asset::AssetBundle::default();
        bundle.add_image("red.png", RED_1X1_PNG.to_vec());
        let pdf = crate::engine::Engine::builder()
            .assets(bundle)
            .build()
            .render(
                r#"<!doctype html><html><body>
                <div style="width:60px;height:40px;overflow:hidden">
                  <img src="red.png" style="width:100px;height:100px">
                </div>
                </body></html>"#,
            )
            .expect("render");
        assert!(pdf.starts_with(b"%PDF"));
    }

    // --- build_metadata: producer=None branch ---

    #[test]
    fn build_metadata_producer_none_skips_producer_field() {
        // Config::default() sets producer=Some("fulgur"). Override with None to
        // exercise the `if let Some(ref producer)` false branch in build_metadata.
        let config = crate::config::Config {
            producer: None,
            ..Default::default()
        };
        let meta = build_metadata(&config, None);
        let debug = format!("{meta:?}");
        assert!(
            debug.contains("producer: None"),
            "producer:None should leave the metadata producer field unset: {debug}"
        );
    }

    // --- multi-page document with tall content (is_split paths) ---

    #[test]
    fn render_smoke_tall_content_multi_page_split() {
        // A block taller than one page causes the fragmenter to record multiple
        // fragments per node, setting is_split=true. Exercises draw_block_inner_paint
        // and draw_under_opacity's is_split height-fix branches (fulgur-bq6i).
        let pdf = render_html(
            r#"<!doctype html><html><body>
            <div style="height:1200px;background:#f0f4ff;border:2px solid #89c">
              Tall block spanning multiple A4 pages.
            </div>
            <p>Second page paragraph.</p>
            </body></html>"#,
        );
        assert!(pdf.starts_with(b"%PDF"));
    }

    // --- opacity block inside overflow:hidden (draw_under_clip → draw_under_opacity) ---

    #[test]
    fn render_smoke_opacity_block_inside_overflow_clip() {
        // A block with overflow:hidden whose clip_descendants include a node with
        // fractional opacity (non-empty opacity_descendants) triggers the
        // draw_under_clip → draw_under_opacity recursion (lines ~1916-1937).
        let pdf = render_html(
            r##"<!doctype html><html><body>
            <div style="width:150px;height:100px;overflow:hidden;background:#eef">
              <div style="opacity:0.5">
                <svg xmlns="http://www.w3.org/2000/svg" width="80" height="60">
                  <rect x="5" y="5" width="70" height="50" fill="#37f"/>
                </svg>
              </div>
            </div>
            </body></html>"##,
        );
        assert!(pdf.starts_with(b"%PDF"));
    }

    // --- draw_under_opacity: block container with block-level children ---

    #[test]
    fn render_smoke_opacity_block_with_block_child_div() {
        // Triggers draw_under_opacity. The outer div has opacity:0.5 and contains
        // a block-level child div (not inline text), so block::convert processes
        // the outer div with opacity_scope=true and populates opacity_descendants.
        // draw_v2_page then calls draw_under_opacity for the outer div.
        let pdf = render_html(
            r#"<!doctype html><html><body>
            <div style="opacity:0.5">
              <div style="width:100px;height:50px;background:#c34;"></div>
            </div>
            </body></html>"#,
        );
        assert!(pdf.starts_with(b"%PDF"));
    }

    #[test]
    fn render_smoke_opacity_block_with_block_child_para_and_div() {
        // draw_under_opacity with multiple block children including a paragraph.
        // Exercises the para_for_block arm inside draw_under_opacity's else branch.
        let pdf = render_html(
            r#"<!doctype html><html><body>
            <div style="opacity:0.4">
              <div style="width:80px;height:20px;background:#eef;"></div>
              <p>Paragraph inside opacity block.</p>
            </div>
            </body></html>"#,
        );
        assert!(pdf.starts_with(b"%PDF"));
    }

    // --- paint_multicol_rule_for_page: column-rule CSS ---

    #[test]
    fn render_smoke_multicol_column_rule_solid() {
        // Triggers paint_multicol_rule_for_page and build_multicol_stroke's Solid
        // branch. column-rule requires build_multicol_stroke to return Some and
        // paint_multicol_rule_for_page to actually draw lines between columns.
        let pdf = render_html(
            r#"<!doctype html><html><body>
            <div style="column-count:2;column-gap:20px;column-rule:2px solid #444;width:400px">
              <p>First column alpha beta gamma delta epsilon zeta eta theta.</p>
              <p>Second column iota kappa lambda mu nu xi omicron pi rho sigma.</p>
            </div>
            </body></html>"#,
        );
        assert!(pdf.starts_with(b"%PDF"));
    }

    #[test]
    fn render_smoke_multicol_column_rule_dashed() {
        // build_multicol_stroke's Dashed branch — sets dash array on stroke.
        let pdf = render_html(
            r#"<!doctype html><html><body>
            <div style="column-count:2;column-gap:16px;column-rule:3px dashed #999;width:360px">
              <p>Dashed rule left column text alpha beta gamma.</p>
              <p>Dashed rule right column text delta epsilon zeta.</p>
            </div>
            </body></html>"#,
        );
        assert!(pdf.starts_with(b"%PDF"));
    }

    #[test]
    fn render_smoke_multicol_column_rule_dotted() {
        // build_multicol_stroke's Dotted branch — sets Round line cap + dash array.
        let pdf = render_html(
            r#"<!doctype html><html><body>
            <div style="column-count:2;column-gap:16px;column-rule:4px dotted #555;width:360px">
              <p>Dotted rule left column text.</p>
              <p>Dotted rule right column text.</p>
            </div>
            </body></html>"#,
        );
        assert!(pdf.starts_with(b"%PDF"));
    }

    // --- draw_under_transform: nested / clip / opacity descendants ---

    #[test]
    fn render_smoke_nested_transforms() {
        // draw_under_transform's nested-transform branch (lines ~1436-1450):
        // outer transform dispatches an inner transform via recursive
        // draw_under_transform so both matrices compose.
        let pdf = render_html(
            r#"<!doctype html><html><body>
            <div style="transform:rotate(5deg);width:100px;height:60px;background:#cef">
              <div style="transform:scale(0.8);background:#efc;width:80px;height:40px">
                nested transforms
              </div>
            </div>
            </body></html>"#,
        );
        assert!(pdf.starts_with(b"%PDF"));
    }

    #[test]
    fn render_smoke_overflow_clip_inside_transform() {
        // draw_under_transform's overflow-clip descendant branch (lines ~1451-1477):
        // an overflow:hidden block nested inside a transformed ancestor triggers
        // draw_under_clip from within draw_under_transform's descendant loop.
        let pdf = render_html(
            r#"<!doctype html><html><body>
            <div style="transform:rotate(3deg);width:120px;height:80px;background:#def">
              <div style="overflow:hidden;width:90px;height:60px;background:#fed">
                <p style="width:200px">Overflow clipped inside transform.</p>
              </div>
            </div>
            </body></html>"#,
        );
        assert!(pdf.starts_with(b"%PDF"));
    }

    #[test]
    fn render_smoke_opacity_block_inside_transform() {
        // draw_under_transform's opacity-scoped descendant branch (lines ~1503-1528):
        // a block with opacity < 1 and block children, nested inside a transformed
        // ancestor, triggers draw_under_opacity from draw_under_transform's
        // descendant loop.
        let pdf = render_html(
            r#"<!doctype html><html><body>
            <div style="transform:rotate(5deg);width:140px;height:100px;background:#eef">
              <div style="opacity:0.5">
                <div style="width:80px;height:50px;background:#c34;"></div>
              </div>
            </div>
            </body></html>"#,
        );
        assert!(pdf.starts_with(b"%PDF"));
    }

    // --- paint_multicol_paragraph_slices: long paragraph spanning columns ---

    #[test]
    fn render_smoke_multicol_paragraph_spanning_columns() {
        // A single long paragraph inside a narrow multicol container forces
        // the paragraph to span multiple columns, triggering
        // convert_multicol_paragraph_slices → paragraph_slices map →
        // paint_multicol_paragraph_slices in dispatch_fragment.
        let pdf = render_html(
            r#"<!doctype html><html><body>
            <div style="column-count:3;column-gap:10px;width:300px;font-size:12px">
              <p>Alpha beta gamma delta epsilon zeta eta theta iota kappa lambda
                 mu nu xi omicron pi rho sigma tau upsilon phi chi psi omega.
                 Lorem ipsum dolor sit amet consectetur adipiscing elit sed do
                 eiusmod tempor incididunt ut labore et dolore magna aliqua.</p>
            </div>
            </body></html>"#,
        );
        assert!(pdf.starts_with(b"%PDF"));
    }

    // --- dispatch_inline_box_content: transform on inline-block ---

    #[test]
    fn render_smoke_inline_block_with_transform_dispatches_under_transform() {
        // An inline-block element with `transform` inside a paragraph creates a
        // `LineItem::InlineBox` in the shaped line. `dispatch_inline_box_content`
        // must route it through `draw_under_transform` (the first branch,
        // `drawables.transforms.get(&content_id).is_some()`).
        let pdf = render_html(
            r#"<!doctype html><html><body>
            <p>Before
              <span style="display:inline-block;transform:rotate(15deg);
                           background:#cef;width:40px;height:20px;
                           line-height:20px;text-align:center">R</span>
            after</p>
            </body></html>"#,
        );
        assert!(pdf.starts_with(b"%PDF"));
    }

    // --- dispatch_inline_box_content: overflow:hidden clip on inline-block ---

    #[test]
    fn render_smoke_inline_block_with_overflow_hidden_dispatches_under_clip() {
        // An inline-block with `overflow:hidden` containing a wider child exercises
        // `dispatch_inline_box_content`'s clip branch (`block.style.has_overflow_clip()`).
        let pdf = render_html(
            r#"<!doctype html><html><body>
            <p>Text
              <span style="display:inline-block;overflow:hidden;
                           width:40px;height:20px;background:#fce">
                <span style="display:inline-block;width:120px;height:20px;
                             background:#e9f">wide child</span>
              </span>
            more text</p>
            </body></html>"#,
        );
        assert!(pdf.starts_with(b"%PDF"));
    }

    // --- dispatch_inline_box_content: opacity scope on inline-block ---

    #[test]
    fn render_smoke_inline_block_with_opacity_and_svg_child_dispatches_under_opacity() {
        // An inline-block with fractional `opacity` and an SVG child triggers
        // `opacity_descendants` population during convert, then
        // `dispatch_inline_box_content` routes through `draw_under_opacity`
        // (the third branch, `!block.opacity_descendants.is_empty()`).
        let pdf = render_html(
            r##"<!doctype html><html><body>
            <p>Text
              <span style="display:inline-block;opacity:0.5;width:24px;height:24px">
                <svg xmlns="http://www.w3.org/2000/svg" width="24" height="24">
                  <circle cx="12" cy="12" r="10" fill="#3af"/>
                </svg>
              </span>
            more</p>
            </body></html>"##,
        );
        assert!(pdf.starts_with(b"%PDF"));
    }

    // --- dest_registry: body_y_off = 0.0 branch on page 2+ ---

    #[test]
    fn render_smoke_id_anchor_on_second_page_sets_destination() {
        // A node with `id` whose first fragment is on page 2+ exercises the
        // `body_y_off = 0.0` branch in `render_v2`'s dest-registry loop
        // (`page_idx != 0` → else 0.0).
        let pdf = render_html(
            r#"<!doctype html><html><body>
            <div style="height:1200pt">spacer to push to page 2</div>
            <p id="page2-target">This paragraph is on page 2.</p>
            </body></html>"#,
        );
        assert!(pdf.starts_with(b"%PDF"));
    }

    // --- draw_v2_page: multicol rule skipped for transformed descendant ---

    #[test]
    fn render_smoke_multicol_rule_inside_transform_skips_outer_paint() {
        // A multicol container nested inside a `transform` element ends up in
        // `transformed_descendants`. The column-rule loop in `draw_v2_page` must
        // skip it (the `transformed_descendants.contains(&container_id)` guard at
        // the top of the loop) and instead paint the rule from inside
        // `draw_under_transform`.
        let pdf = render_html(
            r#"<!doctype html><html><body>
            <div style="transform:rotate(1deg)">
              <div style="column-count:2;column-rule:2px solid #369;
                          column-gap:20px;width:400px;font-size:12px">
                <p>Column A text here for rule test.</p>
                <p>Column B text here for rule test.</p>
              </div>
            </div>
            </body></html>"#,
        );
        assert!(pdf.starts_with(b"%PDF"));
    }

    // --- paint_multicol_paragraph_slices from dispatch_fragment (Case A) ---

    #[test]
    fn render_smoke_multicol_self_inline_root_bare_text() {
        // Exercises paint_multicol_paragraph_slices called from within
        // dispatch_fragment when the multicol container is itself the inline
        // root (Case A: bare text directly in the container, no <p> wrapper).
        // In this case the container has both a block_styles entry and a
        // paragraph_slices entry; dispatch_fragment must paint the block bg
        // first and then call paint_multicol_paragraph_slices for the lines.
        let pdf = render_html(
            r#"<!doctype html><html><body>
            <div style="column-count:2;column-gap:10px;width:400px;font-size:14px">
              alpha beta gamma delta epsilon zeta eta theta iota kappa lambda mu
              nu xi omicron pi rho sigma tau upsilon phi chi psi omega
            </div>
            </body></html>"#,
        );
        assert!(pdf.starts_with(b"%PDF"));
    }

    // --- draw_under_clip: transform / table+clip / opacity branches ---

    #[test]
    fn render_smoke_transform_inside_overflow_clip() {
        // Exercises draw_under_clip's descendant loop: when a transformed
        // child lives inside an overflow:hidden container, draw_under_clip must
        // call draw_under_transform for that descendant rather than
        // dispatch_fragment directly.
        let pdf = render_html(
            r#"<!doctype html><html><body>
            <div style="width:120px;height:80px;overflow:hidden;background:#cef">
              <div style="transform:rotate(8deg);width:80px;height:40px;background:#fce">
                rotated inside clip
              </div>
            </div>
            </body></html>"#,
        );
        assert!(pdf.starts_with(b"%PDF"));
    }

    #[test]
    fn render_smoke_table_overflow_clip_inside_overflow_clip() {
        // Exercises draw_under_clip's table-with-overflow-clip branch: a table
        // with overflow:hidden nested inside an overflow:hidden block forces
        // draw_under_clip_table to be called from within draw_under_clip's
        // descendant loop so the table's own clip boundary is pushed correctly.
        let pdf = render_html(
            r#"<!doctype html><html><body>
            <div style="width:200px;height:120px;overflow:hidden;background:#eef">
              <table style="width:300px;overflow:hidden;border:1px solid #aaa">
                <tr>
                  <td style="padding:4px;background:#cef">wide cell overflowing</td>
                  <td style="padding:4px;background:#fce">second cell</td>
                </tr>
              </table>
            </div>
            </body></html>"#,
        );
        assert!(pdf.starts_with(b"%PDF"));
    }

    // --- draw_under_transform: table+clip branch ---

    #[test]
    fn render_smoke_table_overflow_clip_inside_transform() {
        // Exercises draw_under_transform's table-with-overflow-clip branch: a
        // table with overflow:hidden nested inside a transformed element forces
        // draw_under_clip_table to be called from within draw_under_transform's
        // descendant loop so the table's clip boundary is pushed under the
        // transform.
        let pdf = render_html(
            r#"<!doctype html><html><body>
            <div style="transform:translate(10px,5px)">
              <table style="width:200px;overflow:hidden;border:1px solid #aaa">
                <tr>
                  <td style="padding:6px;background:#cef">cell inside transform</td>
                </tr>
              </table>
            </div>
            </body></html>"#,
        );
        assert!(pdf.starts_with(b"%PDF"));
    }

    // --- draw_under_opacity: transform / clip / table+clip / nested-opacity branches ---

    #[test]
    fn render_smoke_transform_inside_opacity_scope() {
        // Exercises draw_under_opacity's transform-descendant branch: a
        // transformed child nested inside an opacity-scoped block forces
        // draw_under_transform to be called from within draw_under_opacity's
        // descendant loop.
        let pdf = render_html(
            r#"<!doctype html><html><body>
            <div style="opacity:0.7">
              <div style="transform:rotate(10deg);width:80px;height:40px;background:#fce">
                rotated inside opacity
              </div>
            </div>
            </body></html>"#,
        );
        assert!(pdf.starts_with(b"%PDF"));
    }

    #[test]
    fn render_smoke_overflow_clip_inside_opacity_scope() {
        // Exercises draw_under_opacity's overflow-clip descendant branch: an
        // overflow:hidden child nested inside an opacity-scoped block forces
        // draw_under_clip to be called from within draw_under_opacity's
        // descendant loop.
        let pdf = render_html(
            r#"<!doctype html><html><body>
            <div style="opacity:0.7">
              <div style="overflow:hidden;width:100px;height:60px;background:#cef">
                <div style="width:200px;height:20px;background:#f99">clipped</div>
              </div>
            </div>
            </body></html>"#,
        );
        assert!(pdf.starts_with(b"%PDF"));
    }

    #[test]
    fn render_smoke_table_overflow_clip_inside_opacity_scope() {
        // Exercises draw_under_opacity's table-with-overflow-clip descendant
        // branch: a table with overflow:hidden nested inside an opacity-scoped
        // block forces draw_under_clip_table to be called from within
        // draw_under_opacity's descendant loop.
        let pdf = render_html(
            r#"<!doctype html><html><body>
            <div style="opacity:0.65">
              <table style="width:200px;overflow:hidden;border:1px solid #aaa">
                <tr>
                  <td style="padding:4px;background:#cef">opacity+table+clip</td>
                </tr>
              </table>
            </div>
            </body></html>"#,
        );
        assert!(pdf.starts_with(b"%PDF"));
    }

    #[test]
    fn render_smoke_nested_opacity_inside_opacity_scope() {
        // Exercises draw_under_opacity's nested-opacity descendant branch: an
        // opacity-scoped child nested inside another opacity-scoped block forces
        // a recursive draw_under_opacity call from within the outer
        // draw_under_opacity's descendant loop.
        let pdf = render_html(
            r##"<!doctype html><html><body>
            <div style="opacity:0.8">
              <div style="opacity:0.5">
                <div style="background:#c9f;width:60px;height:40px">double-faded</div>
              </div>
            </div>
            </body></html>"##,
        );
        assert!(pdf.starts_with(b"%PDF"));
    }

    // --- draw_under_clip: nested opacity branch (lines 1971-1988) ---

    #[test]
    fn render_smoke_opacity_inside_overflow_clip() {
        // draw_under_clip (lines 1971-1988): when iterating clip_descendants,
        // a descendant that is an opacity-scoped block triggers draw_under_opacity
        // from within draw_under_clip's descendant loop.
        let pdf = render_html(
            r#"<!doctype html><html><body>
            <div style="overflow:hidden;width:200px;height:100px">
              <div style="opacity:0.5">
                <div style="width:150px;height:60px;background:#cef">inner child</div>
              </div>
            </div>
            </body></html>"#,
        );
        assert!(pdf.starts_with(b"%PDF"));
    }

    // --- draw_under_clip_table: transform / clip-block / nested-table / opacity branches ---

    #[test]
    fn render_smoke_transform_inside_clip_table() {
        // draw_under_clip_table (lines 2868-2881, 2856-2857): a table with
        // overflow:hidden whose cell contains a transformed element triggers
        // draw_under_transform from within draw_under_clip_table's
        // clip_descendants loop. The transformed element's own children are
        // added to transform_skip and hit the continue at line 2856.
        let pdf = render_html(
            r#"<!doctype html><html><body>
            <table style="overflow:hidden;border:1px solid #aaa;width:200px">
              <tr>
                <td style="padding:4px">
                  <div style="transform:rotate(5deg);background:#cef;width:80px;height:40px">
                    rotated text inside table clip
                  </div>
                </td>
              </tr>
            </table>
            </body></html>"#,
        );
        assert!(pdf.starts_with(b"%PDF"));
    }

    #[test]
    fn render_smoke_overflow_clip_block_inside_clip_table() {
        // draw_under_clip_table (lines 2887-2901): a table with overflow:hidden
        // containing a block with overflow:hidden triggers draw_under_clip from
        // within draw_under_clip_table's clip_descendants loop.
        let pdf = render_html(
            r#"<!doctype html><html><body>
            <table style="overflow:hidden;border:1px solid #aaa;width:200px">
              <tr>
                <td style="padding:4px">
                  <div style="overflow:hidden;width:120px;height:50px;background:#cef">
                    <div style="width:300px;height:30px;background:#f99">overflowing child</div>
                  </div>
                </td>
              </tr>
            </table>
            </body></html>"#,
        );
        assert!(pdf.starts_with(b"%PDF"));
    }

    #[test]
    fn render_smoke_table_inside_clip_table() {
        // draw_under_clip_table (lines 2906-2919): a table with overflow:hidden
        // that contains another table with overflow:hidden triggers a recursive
        // draw_under_clip_table call from within the outer
        // draw_under_clip_table's clip_descendants loop.
        let pdf = render_html(
            r#"<!doctype html><html><body>
            <table style="overflow:hidden;border:1px solid #aaa;width:220px">
              <tr>
                <td style="padding:4px">
                  <table style="overflow:hidden;border:1px solid #c99;width:180px">
                    <tr><td style="padding:4px;background:#cef">nested table cell</td></tr>
                  </table>
                </td>
              </tr>
            </table>
            </body></html>"#,
        );
        assert!(pdf.starts_with(b"%PDF"));
    }

    #[test]
    fn render_smoke_opacity_inside_clip_table() {
        // draw_under_clip_table (lines 2924-2938): a table with overflow:hidden
        // containing an opacity-scoped block triggers draw_under_opacity from
        // within draw_under_clip_table's clip_descendants loop.
        let pdf = render_html(
            r#"<!doctype html><html><body>
            <table style="overflow:hidden;border:1px solid #aaa;width:200px">
              <tr>
                <td style="padding:4px">
                  <div style="opacity:0.5">
                    <div style="background:#cef;width:80px;height:40px">faded cell content</div>
                  </div>
                </td>
              </tr>
            </table>
            </body></html>"#,
        );
        assert!(pdf.starts_with(b"%PDF"));
    }

    // --- MarginBoxRenderer::render_page: @page :first / :left / :right selectors ---

    #[test]
    fn render_smoke_page_first_selector_margin_box() {
        // MarginBoxRenderer::render_page (lines 3565-3578): the @page :first
        // pseudo-class selector must match on page 1 and override the generic
        // @page rule's margin-box content. On subsequent pages :first does not
        // match (line 3573 `continue`). The `should_replace` logic (line 3578)
        // is also exercised when the :first rule supersedes the generic rule.
        let pdf = render_html(
            r#"<!doctype html><html><head><style>
            @page { size: A4; margin: 20mm; @top-center { content: "All pages"; } }
            @page :first { @top-center { content: "First page only"; } }
            </style></head><body>
            <p>First page content.</p>
            <div style="height:900px"></div>
            <p>Second page content.</p>
            </body></html>"#,
        );
        assert!(pdf.starts_with(b"%PDF"));
    }

    #[test]
    fn render_smoke_page_left_right_selectors() {
        // MarginBoxRenderer::render_page (lines 3565-3569, 3573): :left and
        // :right pseudo-class selectors are evaluated per page_num parity.
        // On a 3-page document both branches fire and pages where a selector
        // does not match hit the `continue` at line 3573.
        let pdf = render_html(
            r#"<!doctype html><html><head><style>
            @page { size: A4; margin: 20mm; }
            @page :left  { @top-center { content: "Left page";  } }
            @page :right { @top-center { content: "Right page"; } }
            </style></head><body>
            <p>Page one.</p>
            <div style="height:900px"></div>
            <p>Page two.</p>
            <div style="height:900px"></div>
            <p>Page three.</p>
            </body></html>"#,
        );
        assert!(pdf.starts_with(b"%PDF"));
    }

    // --- MarginBoxRenderer stage 1b + stage 2: @left-middle / @right-middle boxes ---

    #[test]
    fn render_smoke_page_left_right_margin_boxes() {
        // MarginBoxRenderer::render_page stage 1b (lines 3659-3688): left/right
        // margin boxes require a separate height-measurement pass. Stage 2 size
        // lookup for vertical edges (lines 3708-3719) uses those heights to
        // place the boxes. Stage 1a's `continue` for non-horizontal edges
        // (line 3624) is also hit.
        let pdf = render_html(
            r#"<!doctype html><html><head><style>
            @page {
              size: A4; margin: 20mm 40mm;
              @left-middle  { content: "Left margin";  }
              @right-middle { content: "Right margin"; }
            }
            </style></head><body>
            <p>Content with left and right margin boxes.</p>
            </body></html>"#,
        );
        assert!(pdf.starts_with(b"%PDF"));
    }
}
