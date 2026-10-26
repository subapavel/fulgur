//! Convert a Blitz DOM (after style resolution + layout) into a `Drawables`
//! struct holding per-NodeId draw payload.
//!
//! A single DOM walk writes directly into `Drawables`'s per-NodeId maps.
//! Bookmark / string-set / counter-op / running-element side-channels are
//! read from their respective stores by the fragmenter and render pass
//! independently.

use crate::asset::AssetBundle;
use crate::blitz_adapter::{BaseDocument, Node, NodeData};
use crate::draw_primitives::{BlockStyle, Size};
use crate::drawables::{ImageMarker, ListItemMarker};
use crate::gcpm::CounterOp;
use crate::gcpm::running::RunningElementStore;
use crate::image::ImageRender;
use crate::paragraph::{
    InlineImage, LineFontMetrics, LineItem, LinkSpan, LinkTarget, ShapedGlyph, ShapedGlyphRun,
    ShapedLine, TextDecoration, TextDecorationLine, TextDecorationStyle, VerticalAlign,
};
use crate::units::{F32Units, Pt, Px};
use blitz_html::HtmlDocument;
use skrifa::MetadataProvider;
use std::collections::{BTreeMap, HashMap};
use std::ops::Deref;
use std::sync::Arc;

use crate::MAX_DOM_DEPTH;

mod block;
mod inline_root;
mod list_item;
mod list_marker;
mod positioned;
mod pseudo;
mod replaced;
// Local submodule shadows the Stylo extern crate `style` for any sibling that
// uses `use super::*;`. Such siblings must reach Stylo via `::style::...`.
// Code inside `convert/style/*.rs` is unaffected — `style::...` there resolves
// to the extern crate via Rust 2018 absolute-path rules.
mod style;
mod table;

use self::style::{absolute_to_rgba, extract_block_style, extract_opacity_visible};

/// Convert a Taffy `Layout` (CSS px) to PDF pt as `(x, y, width, height)`.
#[inline]
fn layout_in_pt(layout: &taffy::Layout) -> (Pt, Pt, Pt, Pt) {
    (
        layout.location.x.as_px().in_pt(),
        layout.location.y.as_px().in_pt(),
        layout.size.width.as_px().in_pt(),
        layout.size.height.as_px().in_pt(),
    )
}

/// Convert a Taffy `Size<f32>` (CSS px) to PDF pt as `(width, height)`.
#[inline]
fn size_in_pt(size: taffy::Size<f32>) -> (Pt, Pt) {
    (size.width.as_px().in_pt(), size.height.as_px().in_pt())
}

/// Default CSS line-height multiplier when the actual computed value is
/// unavailable (CSS 2 §10.8.1 initial value for `line-height: normal`).
const DEFAULT_LINE_HEIGHT_RATIO: f32 = 1.2;

/// Map a stylo `text-align` keyword to the corresponding parley
/// `Alignment`. Mirrors blitz-dom's own mapping
/// (`blitz-dom-0.2.4/src/layout/inline.rs:142-152`) so split paragraph
/// fragments render with the same alignment Blitz uses for the
/// non-split path. CSS values not directly representable in
/// `parley::Alignment` (e.g. legacy `-moz-*` keywords) collapse to
/// their nearest equivalent; anything entirely unknown falls back to
/// `Alignment::Start`.
fn css_text_align_to_parley_alignment(
    text_align: ::style::values::specified::TextAlignKeyword,
) -> parley::Alignment {
    use ::style::values::specified::TextAlignKeyword;
    match text_align {
        TextAlignKeyword::Start => parley::Alignment::Start,
        TextAlignKeyword::Left => parley::Alignment::Left,
        TextAlignKeyword::Right => parley::Alignment::Right,
        TextAlignKeyword::Center => parley::Alignment::Center,
        TextAlignKeyword::Justify => parley::Alignment::Justify,
        TextAlignKeyword::End => parley::Alignment::End,
        TextAlignKeyword::MozCenter => parley::Alignment::Center,
        TextAlignKeyword::MozLeft => parley::Alignment::Left,
        TextAlignKeyword::MozRight => parley::Alignment::Right,
    }
}

/// Text that no glyph run covers: it is laid out but left out of the PDF, because no registered font face
/// matched its `font-family` (or, with host fonts off, a generic family resolved to nothing).
/// `ConvertContext` collects one per distinct (element, text) pair; the engine logs them or, with
/// `EngineBuilder::strict_text(true)`, fails with `Error::TextWithoutFont`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DroppedText {
    /// The element whose text it is: tag name, plus `#id` when it has one (`p`, `td#total`).
    pub element: String,
    /// The uncovered text, whitespace and invisible characters left out, shortened to a few words.
    pub excerpt: String,
}

impl std::fmt::Display for DroppedText {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "<{}> \"{}\"", self.element, self.excerpt)
    }
}

/// The [`DroppedText`]s found so far, in the order they were found, each distinct (element, excerpt) pair once.
/// The pairs are also kept in a set: a document with thousands of dropped paragraphs must not search the list
/// for each of them.
#[derive(Debug, Default)]
pub struct DroppedTexts {
    list: Vec<DroppedText>,
    seen: std::collections::BTreeSet<(String, String)>,
}

impl DroppedTexts {
    fn push(&mut self, d: DroppedText) {
        if self.seen.insert((d.element.clone(), d.excerpt.clone())) {
            self.list.push(d);
        }
    }

    pub fn is_empty(&self) -> bool {
        self.list.is_empty()
    }

    pub fn into_vec(self) -> Vec<DroppedText> {
        self.list
    }
}

/// Context for DOM-to-Drawables conversion, bundling all shared state.
pub struct ConvertContext<'a> {
    pub running_store: &'a RunningElementStore,
    pub assets: Option<&'a AssetBundle>,
    /// The `Arc` every glyph run holds for its font's bytes; bundled fonts are shared, never copied (see [`FontBytes`]).
    pub(crate) font_bytes: FontBytes,
    /// String-set entries from DOM walk, keyed by node_id for O(1) lookup.
    pub string_set_by_node: HashMap<usize, Vec<(String, String)>>,
    /// Counter operations from CounterPass, keyed by node_id for O(1) lookup.
    pub counter_ops_by_node: HashMap<usize, Vec<CounterOp>>,
    /// Resolved bookmark entries from [`crate::blitz_adapter::BookmarkPass`],
    /// keyed by node_id for O(1) lookup. `dom_to_drawables` snapshots this
    /// map before walking the DOM and uses the snapshot to populate
    /// `drawables.bookmark_anchors`; the convert path itself no longer
    /// drains entries.
    pub bookmark_by_node: HashMap<usize, crate::blitz_adapter::BookmarkInfo>,
    /// Phase A `column-*` side-table harvested by
    /// [`crate::blitz_adapter::extract_column_style_table`]. `record_multicol_rule`
    /// reads `rule` properties from here when registering multicol containers
    /// in `drawables.multicol_rules`.
    pub(crate) column_styles: crate::column_css::ColumnStyleTable,
    /// Per-multicol-container geometry recorded by the Taffy multicol hook
    /// (see [`crate::multicol_layout::run_pass`]). `record_multicol_rule`
    /// reads this to register `column-rule` paint specs without re-running
    /// layout.
    pub multicol_geometry: crate::multicol_layout::MulticolGeometryTable,
    /// fulgur-cj6u Phase 1.1: per-body-child page-fragment geometry
    /// recorded by [`crate::pagination_layout::run_pass_with_break_styles`].
    pub pagination_geometry: crate::pagination_layout::PaginationGeometryTable,
    /// Anchor (`<a href>`) resolution cache shared across the entire
    /// conversion. Lifted out of `extract_paragraph` because inline-box
    /// extraction recurses through `convert_node → extract_paragraph`, and a
    /// per-paragraph cache would hand back two distinct `Arc<LinkSpan>` for
    /// the same anchor — one for the outer inline-box rect and one for the
    /// glyphs inside the box — producing duplicate `/Link` annotations in
    /// the emitted PDF (LinkCollector dedupes by `Arc::ptr_eq`). A single
    /// long-lived cache guarantees pointer identity across the whole tree.
    pub(crate) link_cache: LinkCache,
    /// Initial CB approximation: page area dimensions in CSS px.
    /// `position: fixed` resolves its containing block against the viewport
    /// (CSS 2.1 §10.1.5). See `positioned::resolve_cb_for_absolute`.
    pub viewport_size_px: Option<(f32, f32)>,
    /// Text found in inline formatting contexts that no glyph run covers, in document order, each
    /// (element, excerpt) pair once. Filled by `extract_paragraph`; read by the engine after conversion.
    pub dropped_text: DroppedTexts,
}

impl ConvertContext<'_> {
    /// Remember `text` of `node` as dropped, unless the same element and text were recorded already.
    fn record_dropped_text(&mut self, node: &Node, excerpt: String) {
        let element = node
            .element_data()
            .map(|e| {
                let name = e.name.local.as_ref();
                match e.id.as_ref() {
                    Some(id) => format!("{name}#{id}"),
                    None => name.to_string(),
                }
            })
            .unwrap_or_default();
        self.dropped_text.push(DroppedText { element, excerpt });
    }

    /// The shared bytes of `font` (see [`FontBytes`]).
    fn get_or_insert_font(&mut self, font: &parley::FontData) -> Arc<Vec<u8>> {
        self.font_bytes.get_or_copy(font)
    }
}

/// The `Arc<Vec<u8>>` of the font bytes of every glyph run of one conversion.
///
/// Parley hands out a [`parley::FontData`] whose [`Blob`](parley::fontique::Blob) is the blob a font was registered
/// with. For the bundled fonts that blob wraps the very `Arc<Vec<u8>>` of the [`AssetBundle`] (both the main document
/// and the margin-box documents register the bundle's `Arc`, see `blitz_adapter::parse_inner` and `MarginDocEnv`),
/// so the data address of the blob is the address of the bundle's buffer. [`FontBytes::seeded`] maps those addresses to
/// the bundle's `Arc`s, and a run then shares the bundle's bytes instead of a copy of about 400 KB per context. Only a
/// blob that is not in the bundle (a system font, a font a document brings itself) is copied, once per context.
///
/// The key is the data address, checked against the length of the blob. The addresses stay valid for the lifetime of
/// the context: a seeded entry holds an `Arc` of its buffer, and the document that owns any other blob outlives the
/// conversion. The map is only looked up, never iterated, so a `HashMap` does not affect the PDF.
#[derive(Default)]
pub(crate) struct FontBytes {
    by_address: HashMap<usize, Arc<Vec<u8>>>,
}

impl FontBytes {
    /// A cache that already knows the bundled fonts: their blobs resolve to the bundle's own `Arc`s.
    pub(crate) fn seeded(bundle: &[Arc<Vec<u8>>]) -> Self {
        let mut by_address = HashMap::with_capacity(bundle.len());
        for font in bundle {
            if !font.is_empty() {
                by_address
                    .entry(font.as_ptr() as usize)
                    .or_insert_with(|| Arc::clone(font));
            }
        }
        Self { by_address }
    }

    /// The bytes of `font` as an `Arc`: the bundle's own for a seeded blob, else a copy made on first use.
    pub(crate) fn get_or_copy(&mut self, font: &parley::FontData) -> Arc<Vec<u8>> {
        let bytes = font.data.data();
        let key = bytes.as_ptr() as usize;
        if let Some(known) = self.by_address.get(&key)
            && known.len() == bytes.len()
        {
            return Arc::clone(known);
        }
        let copy = Arc::new(bytes.to_vec());
        self.by_address.insert(key, Arc::clone(&copy));
        copy
    }
}

/// Phase 4 (fulgur-9t3z) + PR 8i: convert a resolved Blitz document into a
/// `Drawables` struct holding per-NodeId draw payload, walking the DOM
/// directly and writing entries into `drawables` as it goes.
pub fn dom_to_drawables(
    doc: &HtmlDocument,
    ctx: &mut ConvertContext<'_>,
) -> crate::drawables::Drawables {
    // Snapshot the bookmark map up-front so deletions from
    // `ctx.bookmark_by_node` later in the pipeline (none in convert today,
    // but kept for symmetry with engine-level callers) don't perturb the
    // outline projection.
    let bookmark_snapshot = ctx.bookmark_by_node.clone();
    let mut drawables = crate::drawables::Drawables::new();
    let root = doc.root_element();
    if std::env::var("FULGUR_DEBUG").is_ok() {
        debug_print_tree(doc.deref(), root.id, 0);
    }
    convert_node(doc.deref(), root.id, ctx, 0, &mut drawables);
    drawables.bookmark_anchors = extract_bookmark_anchors(doc, &bookmark_snapshot, ctx.assets);
    drawables.body_offset_pt = extract_body_offset_pt(doc);
    drawables.root_dir_rtl = extract_root_dir_rtl(doc);
    drawables.root_id = Some(root.id);
    drawables.body_id = find_body_id_in_dom(doc);
    record_semantics_pass(doc, ctx.running_store, &mut drawables);
    drawables
}

/// Locate the `<body>` element id by walking the html root's children.
/// Mirrors `pagination_layout::find_body_id` but operates on the
/// `HtmlDocument` API (the latter is private to that module).
fn find_body_id_in_dom(doc: &HtmlDocument) -> Option<usize> {
    use std::ops::Deref;
    let base = doc.deref();
    let root = doc.root_element();
    let root_node = base.get_node(root.id)?;
    for &child_id in &root_node.children {
        let Some(child) = base.get_node(child_id) else {
            continue;
        };
        if let blitz_dom::NodeData::Element(elem) = &child.data
            && elem.name.local.as_ref() == "body"
        {
            return Some(child_id);
        }
    }
    None
}

/// Walk the DOM to find the first `<body>` and return its
/// `(location.x, location.y)` in pt. The fragmenter records body's own
/// fragment at `(body_x, 0)` (body-content-area relative); the html →
/// body offset that CSS margin collapsing puts onto `body.location` lives
/// here so `render_v2` can add it to per-fragment draw positions.
fn extract_body_offset_pt(doc: &HtmlDocument) -> (crate::units::Pt, crate::units::Pt) {
    use std::ops::Deref;
    let base = doc.deref();
    let root = doc.root_element();
    let Some(root_node) = base.get_node(root.id) else {
        return (crate::units::Pt::ZERO, crate::units::Pt::ZERO);
    };
    for &child_id in &root_node.children {
        let Some(child) = base.get_node(child_id) else {
            continue;
        };
        if let blitz_dom::NodeData::Element(elem) = &child.data
            && elem.name.local.as_ref() == "body"
        {
            let (x, y, _, _) = layout_in_pt(&child.final_layout);
            return (x, y);
        }
    }
    (crate::units::Pt::ZERO, crate::units::Pt::ZERO)
}

/// Return `true` when the root `<html>` element has `direction: rtl`.
/// Used by `render_v2` to determine which page is the first `:left` page
/// (CSS Paged Media §5: RTL docs start on a `:left` page, LTR on `:right`).
fn extract_root_dir_rtl(doc: &HtmlDocument) -> bool {
    use ::style::properties::longhands::direction::computed_value::T as Dir;
    let base = doc.deref();
    base.get_node(doc.root_element().id)
        .and_then(|n| n.primary_styles())
        .is_some_and(|s| matches!(s.get_inherited_box().clone_direction(), Dir::Rtl))
}

/// O(1) probe answering "did *any* per-NodeId map grow?" — sums `.len()`
/// across the same six maps `Drawables::draw_mark` / `drawn_since` track. Callers
/// that only need the boolean "produced anything?" answer (not the
/// descendant set) should compare this value before/after a recursion
/// instead of constructing two `BTreeSet<usize>`s. Convert never removes
/// entries from these maps, so the sum is monotonic and a strict
/// inequality is exactly equivalent to set-difference being non-empty.
/// (fulgur-vrkv)
pub(super) fn drawables_total_len(out: &crate::drawables::Drawables) -> usize {
    out.block_styles.len()
        + out.paragraphs.len()
        + out.images.len()
        + out.svgs.len()
        + out.tables.len()
        + out.list_items.len()
}

/// Build the bookmark anchor map. The `bookmark_by_node` map on
/// `ConvertContext` is populated upstream (`engine.rs` runs
/// `BookmarkPass` before `dom_to_drawables`); we only project it into
/// the `Drawables` shape. The `_doc` / `_assets` arguments are
/// reserved for future enrichment.
fn extract_bookmark_anchors(
    _doc: &HtmlDocument,
    bookmark_by_node: &std::collections::HashMap<usize, crate::blitz_adapter::BookmarkInfo>,
    _assets: Option<&crate::asset::AssetBundle>,
) -> std::collections::BTreeMap<usize, crate::drawables::BookmarkAnchorEntry> {
    let mut out = std::collections::BTreeMap::new();
    for (&node_id, info) in bookmark_by_node {
        out.insert(
            node_id,
            crate::drawables::BookmarkAnchorEntry {
                level: info.level,
                label: info.label.clone(),
            },
        );
    }
    out
}

fn debug_print_tree(doc: &BaseDocument, node_id: usize, depth: usize) {
    if depth >= MAX_DOM_DEPTH {
        eprintln!("{}... (max depth reached)", "  ".repeat(depth));
        return;
    }
    let Some(node) = doc.get_node(node_id) else {
        return;
    };
    let (x, y, width, height) = layout_in_pt(&node.final_layout);
    let indent = "  ".repeat(depth);
    let tag = match &node.data {
        NodeData::Element(e) => e.name.local.to_string(),
        NodeData::Text(_) => "#text".to_string(),
        NodeData::Comment => "#comment".to_string(),
        _ => "#other".to_string(),
    };
    // `Pt` has no `Display` by design (a length must not silently format as a
    // bare number), so this dev-only dump formats the typed values with `{:?}`
    // rather than `.to_f32()`. Bonus: since the branch is FULGUR_DEBUG-gated and
    // never runs under test, `{:?}` keeps the migrated diff to one format-string
    // line instead of four uncovered `.to_f32()` arg lines in the patch.
    eprintln!(
        "{indent}{tag} id={} pos=({:?},{:?}) size={:?}x{:?} inline_root={}",
        node_id,
        x,
        y,
        width,
        height,
        node.flags.is_inline_root()
    );
    for &child_id in &node.children {
        debug_print_tree(doc, child_id, depth + 1);
    }
}

/// Convert a single DOM node into Drawables entries.
///
/// Wraps `convert_node_inner` with the post-pass that records `transform` /
/// `multicol-rule` entries by snapshotting the per-NodeId map keys before
/// recursion and diffing afterwards to find every descendant the inner
/// walk added. Bookmark / string-set / counter-op / running-element
/// wrapping is handled separately:
///
/// - `bookmark_anchors` is populated from `dom_to_drawables`'s up-front
///   snapshot of `ctx.bookmark_by_node`.
/// - String-set / counter-op / running-element side-channels feed the
///   fragmenter and render pass directly via the corresponding stores
///   (see `engine.rs`).
pub(super) fn convert_node(
    doc: &BaseDocument,
    node_id: usize,
    ctx: &mut ConvertContext<'_>,
    depth: usize,
    out: &mut crate::drawables::Drawables,
) {
    if depth >= MAX_DOM_DEPTH {
        return;
    }
    // F102: a running element is shown by `element()` in a margin box and nowhere else (CSS GCPM 3 §2.2). The
    // engine sets `display: none` on it, but an author `!important` `display` beats that, and the node would
    // be painted, linked and tagged a second time in the body. Margin-box documents carry an empty store.
    if ctx.running_store.instance_for_node(node_id).is_some() {
        return;
    }
    // Only `record_transform` consumes `before`, and `record_transform`
    // is a no-op for any node without a CSS transform. Snapshotting
    // every drawables map for every walked node was the dominant
    // O(N²) cost in document-grade documents (each `convert_node` call
    // copying every NodeId already inserted, summing to ~N²/2 inserts
    // for N nodes). Gate the snapshot on the transform check so the
    // common case stays O(N). (fulgur-v1cm)
    let mark = node_has_transform(doc, node_id).then(|| out.draw_mark());
    convert_node_inner(doc, node_id, ctx, depth, out);
    record_multicol_rule(doc, node_id, ctx, out);
    convert_multicol_paragraph_slices(doc, node_id, ctx, out);
    if let Some(mark) = mark {
        record_transform(doc, node_id, mark, out);
    }
}

/// Cheap pre-check that mirrors `record_transform`'s own bail conditions:
/// returns true only when this node would actually need a snapshot to
/// compute its transform descendants. Computing the matrix here costs
/// the same as `record_transform` would (and is bypassed by the early
/// `return`s in nodes without a `<style transform>`), but it keeps the
/// branch-free invariant that `record_transform`'s snapshot consumer
/// only fires for nodes that produce a `TransformEntry`.
fn node_has_transform(doc: &BaseDocument, node_id: usize) -> bool {
    doc.get_node(node_id)
        .and_then(|node| {
            let styles = node.primary_styles()?;
            let (w, h) = size_in_pt(node.final_layout.size);
            crate::blitz_adapter::compute_transform(&styles, w.to_f32(), h.to_f32())
        })
        .is_some()
}

/// Walk the DOM top-down from `<body>` and populate `out.semantics`
/// with one `SemanticEntry` per element whose local name is recognised
/// by `crate::tagging::classify_element`. Runs as a standalone pass
/// after `convert_node` so the classification covers elements (e.g.
/// `<thead>`, `<tbody>`) that the per-type converters traverse via
/// custom child walks instead of recursing through `convert_node`.
///
/// `<head>` and its descendants are intentionally skipped — none of
/// them participate in the StructTree, and starting from `<body>`
/// keeps later expansions of `classify_element` (e.g. promoting
/// `<header>` / `<footer>` to dedicated tags) from accidentally
/// classifying `<head>`'s `<title>` / `<style>` etc.
///
/// fulgur-izp.3: pure data layer. The render path does not consume
/// these entries yet, so PDF byte equality is preserved across this
/// change.
fn record_semantics_pass(
    doc: &HtmlDocument,
    running_store: &RunningElementStore,
    out: &mut crate::drawables::Drawables,
) {
    use std::ops::Deref;
    let base = doc.deref();
    let Some(body_id) = out.body_id else {
        return;
    };
    walk_semantics(base, body_id, 0, None, running_store, out);
}

fn walk_semantics(
    doc: &BaseDocument,
    node_id: usize,
    depth: usize,
    parent_override: Option<usize>,
    running_store: &RunningElementStore,
    out: &mut crate::drawables::Drawables,
) {
    if depth >= MAX_DOM_DEPTH {
        return;
    }
    // F102: a running element and its subtree are not in the body (`convert_node` skips them too).
    if running_store.instance_for_node(node_id).is_some() {
        return;
    }
    let Some(node) = doc.get_node(node_id) else {
        return;
    };

    let child_override = if let Some(elem) = node.element_data() {
        if let Some(mut tag) = crate::tagging::classify_element(elem.name.local.as_ref()) {
            // CSS list-style-type を読んで ListNumbering をオーバーライド
            if matches!(tag, crate::tagging::PdfTag::L { .. }) {
                if let Some(styles) = node.primary_styles() {
                    use ::style::properties::longhands::list_style_type::computed_value::T as LST;
                    use krilla::tagging::ListNumbering;
                    let numbering = match styles.clone_list_style_type() {
                        LST::Disc => ListNumbering::Disc,
                        LST::Circle => ListNumbering::Circle,
                        LST::Square => ListNumbering::Square,
                        LST::Decimal => ListNumbering::Decimal,
                        LST::LowerAlpha => ListNumbering::LowerAlpha,
                        LST::UpperAlpha => ListNumbering::UpperAlpha,
                        _ => ListNumbering::None,
                    };
                    tag = crate::tagging::PdfTag::L { numbering };
                }
            }

            // parent を決定: override があればそれを使い、なければ DOM walk-up
            let parent_node_id = if let Some(ov) = parent_override {
                Some(ov)
            } else {
                let mut p = node.parent;
                loop {
                    let Some(pid) = p else { break None };
                    if out.semantics.contains_key(&pid) {
                        break Some(pid);
                    }
                    p = doc.get_node(pid).and_then(|n| n.parent);
                }
            };

            // `visibility: hidden` / `collapse` excludes the subtree from
            // the accessibility tree (CSS 2 §11.2 + WAI-ARIA; matched by
            // Chromium and Firefox), so an invisible <img> must not
            // surface its `alt` text through the Figure tag's `/Alt`
            // attribute either. Web-aware authoring uses sr-only
            // (position:absolute + clip) or aria-hidden to keep alt
            // available to assistive tech while hiding the visual — not
            // `visibility: hidden`. Gate here (rather than in
            // `pdf_tag_to_krilla_tag`) so `SemanticEntry.alt_text` stays
            // the authoritative value that downstream tag-tree code can
            // trust.
            let alt_text = if matches!(tag, crate::tagging::PdfTag::Figure) {
                let (_opacity, visible) = extract_opacity_visible(node);
                if visible {
                    node.element_data()
                        .and_then(|e| get_attr(e, "alt"))
                        .map(|v| v.to_owned())
                } else {
                    None
                }
            } else {
                None
            };

            let is_li = matches!(tag, crate::tagging::PdfTag::Li);

            if matches!(tag, crate::tagging::PdfTag::Th { .. }) {
                let scope = get_attr(elem, "scope")
                    .and_then(|s| match s {
                        "row" => Some(krilla::tagging::TableHeaderScope::Row),
                        "col" | "column" => Some(krilla::tagging::TableHeaderScope::Column),
                        _ => None,
                    })
                    .unwrap_or(krilla::tagging::TableHeaderScope::Both);
                tag = crate::tagging::PdfTag::Th { scope };
            }

            out.semantics.insert(
                node_id,
                crate::tagging::SemanticEntry {
                    tag,
                    parent: parent_node_id,
                    alt_text,
                },
            );

            if is_li {
                // Lbl / LBody 合成エントリを作成。
                // alloc_synthetic_id() は降順に ID を払い出すため、先に lbody_id を取ると
                // lbl_id > lbody_id となり、BTreeMap のキー昇順イテレーション
                // (build_struct_tree) で Lbl → LBody の正しい PDF/UA 順序が得られる。
                let lbody_id = out.alloc_synthetic_id();
                let lbl_id = out.alloc_synthetic_id();
                out.semantics.insert(
                    lbl_id,
                    crate::tagging::SemanticEntry {
                        tag: crate::tagging::PdfTag::Lbl,
                        parent: Some(node_id),
                        alt_text: None,
                    },
                );
                out.semantics.insert(
                    lbody_id,
                    crate::tagging::SemanticEntry {
                        tag: crate::tagging::PdfTag::LBody,
                        parent: Some(node_id),
                        alt_text: None,
                    },
                );
                out.li_lbl_ids.insert(node_id, lbl_id);
                out.li_lbody_ids.insert(node_id, lbody_id);
                // li の子は lbody_id を parent として再帰
                for &child_id in &node.children {
                    walk_semantics(doc, child_id, depth + 1, Some(lbody_id), running_store, out);
                }
                return; // 通常の再帰をスキップ
            }

            None // 分類済み要素 (Li 以外): 子への override はリセット
        } else {
            parent_override // 非分類要素: override を引き継ぐ
        }
    } else {
        parent_override // 要素でないノード: override を引き継ぐ
    };

    for &child_id in &node.children {
        walk_semantics(doc, child_id, depth + 1, child_override, running_store, out);
    }
}

/// Inner dispatcher. Tries each specialized converter in order; falls
/// through to `block::convert` as the catch-all.
fn convert_node_inner(
    doc: &BaseDocument,
    node_id: usize,
    ctx: &mut ConvertContext<'_>,
    depth: usize,
    out: &mut crate::drawables::Drawables,
) {
    // List-item dispatch: outside marker / display:list-item fallback / inside marker.
    if list_item::try_convert(doc, node_id, ctx, depth, out) {
        return;
    }

    // Table dispatch: <table>.
    if table::try_convert(doc, node_id, ctx, depth, out) {
        return;
    }

    // Replaced-element dispatch: <img>, <svg>, content: url().
    if replaced::try_convert(doc, node_id, ctx, out) {
        return;
    }

    // Inline-root dispatch: paragraph + inline pseudo images.
    if inline_root::try_convert(doc, node_id, ctx, depth, out) {
        return;
    }

    block::convert(doc, node_id, ctx, depth, out);
}

/// Register a `TransformEntry` for `node_id` if its computed style
/// resolves to a non-identity transform. `mark` was captured before this
/// node's walk; the `NodeId`s inserted since then (excluding `node_id`
/// itself) are the strict descendant list the render pass needs to paint
/// inside the transform's `push_transform` / `pop` group. See
/// [`crate::drawables::Drawables::drawn_since`].
fn record_transform(
    doc: &BaseDocument,
    node_id: usize,
    mark: crate::drawables::DrawMark,
    out: &mut crate::drawables::Drawables,
) {
    let Some(node) = doc.get_node(node_id) else {
        return;
    };
    let Some(styles) = node.primary_styles() else {
        return;
    };
    // PR 8i note: `compute_transform` is documented to take CSS px (per
    // `.claude/rules/coordinate-system.md` and Stylo's `LengthPercentage`
    // contract), but is fed pt-valued box dims below (`width_pt`/`height_pt`),
    // technically violating that contract. This is intentional and remains
    // correct for percentage resolution (Length is unitless from Stylo's
    // perspective, so `%` round-trips identically against a pt basis or a
    // px basis — `transform-origin: 50%` is unaffected either way).
    //
    // Absolute-length components (`translate(Npx)`, `matrix()` tx/ty,
    // `transform-origin: Npx ...`) are a different story: `resolve()` ignores
    // this basis for them and returns the literal CSS px value, so
    // `compute_transform`/`op_to_matrix` fold px → pt for real
    // (`resolve_length_component`, fulgur-9vw5) before returning. The px →
    // origin → pt conversion this note used to defer to render as "a
    // separate cleanup tracked for a future PR" is therefore already done
    // by the time `render::draw_under_transform` consumes `tx.origin` and
    // the matrix — do not fold again there. Re-enabled by
    // `transform_integration::rotate_90_at_default_center_origin_fixes_center`.
    let (width_pt, height_pt) = size_in_pt(node.final_layout.size);
    let Some((matrix, origin)) =
        crate::blitz_adapter::compute_transform(&styles, width_pt.to_f32(), height_pt.to_f32())
    else {
        return;
    };
    let descendants: Vec<usize> = out
        .drawn_since(mark)
        .into_iter()
        .filter(|&id| id != node_id)
        .collect();
    out.transforms.insert(
        node_id,
        crate::drawables::TransformEntry {
            matrix,
            origin,
            descendants,
        },
    );
}

/// Register a `MulticolRuleEntry` for `node_id` if it is a multicol
/// container with a renderable `column-rule` spec and Taffy-recorded
/// geometry. No-op for non-multicol containers.
fn record_multicol_rule(
    doc: &BaseDocument,
    node_id: usize,
    ctx: &ConvertContext<'_>,
    out: &mut crate::drawables::Drawables,
) {
    let Some(node) = doc.get_node(node_id) else {
        return;
    };
    if !crate::blitz_adapter::is_multicol_container(node) {
        return;
    }
    let Some(rule) = ctx
        .column_styles
        .get(&node_id)
        .and_then(|props| props.rule)
        .filter(|r| {
            r.style != crate::column_css::ColumnRuleStyle::None && r.width > crate::units::Pt::ZERO
        })
    else {
        return;
    };
    let Some(geometry) = ctx.multicol_geometry.get(&node_id) else {
        return;
    };
    // `ColumnGroupGeometry` is recorded in CSS px; convert to the pt-typed
    // ColumnRuleGeometry carrier so downstream paint matches every other
    // Drawables entry's units. Each conversion is one multiply (byte-neutral).
    let groups: Vec<crate::drawables::ColumnRuleGeometry> = geometry
        .groups
        .iter()
        .map(|g| crate::drawables::ColumnRuleGeometry {
            x_offset: g.x_offset.in_pt(),
            y_offset: g.y_offset.in_pt(),
            col_w: g.col_w.in_pt(),
            gap: g.gap.in_pt(),
            n: g.n,
            col_heights: g.col_heights.iter().copied().map(|h| h.in_pt()).collect(),
        })
        .collect();
    out.multicol_rules.insert(
        node_id,
        crate::drawables::MulticolRuleEntry { rule, groups },
    );
}

/// fulgur-6q5 Task 7: populate `Drawables.paragraph_slices` for a multicol
/// container from `MulticolGeometry::paragraph_splits`.
///
/// For every `ParagraphSplitEntry` recorded by `multicol_layout` against
/// this container, materialise one `ParagraphSlice` per non-empty column.
/// Each slice carries `Vec<ShapedLine>` rebased to the slice's own top
/// edge, using the same rebase convention as continuation fragments after
/// a page break (commit 9c0e092).
///
/// Two source-layout sources, distinguished by whether
/// `source_node_id == node_id`:
///
/// - **Case A** (container is itself an inline root): the container's
///   `inline_layout_data.layout` was shaped at the container's content
///   width; multicol recorded line indices against a clone re-broken at
///   `col_w`. Convert reproduces that re-broken clone here.
/// - **Case B** (a child element is the inline root): Blitz already
///   re-broke the child's parley layout at `col_w` during
///   `compute_child_layout`, so the line indices stored against
///   `inline_layout_data` line up directly.
///
/// Scope: this path covers plain-text inline-root paragraphs only. Inline
/// boxes / replaced content aren't handled here — the paragraphs that
/// `multicol_layout` actually splits across columns never carry inline
/// boxes (Task 4 / 5 only emit `ParagraphSplitEntry` for pure-text inline
/// roots), so the simpler `GlyphRun`-only loop is sufficient and keeps
/// us out of `convert_inline_box_node`'s out-mutating descendant
/// machinery.
fn convert_multicol_paragraph_slices(
    doc: &BaseDocument,
    node_id: usize,
    ctx: &mut ConvertContext<'_>,
    out: &mut crate::drawables::Drawables,
) {
    let Some(node) = doc.get_node(node_id) else {
        return;
    };
    if !crate::blitz_adapter::is_multicol_container(node) {
        return;
    }
    let Some(geometry) = ctx.multicol_geometry.get(&node_id).cloned() else {
        return;
    };
    // F30d: a container the fragmenter cut over pages says itself which lines every column holds on every page.
    if let Some(pages) = ctx
        .pagination_geometry
        .get(&node_id)
        .map(|g| g.multicol_pages.clone())
        .filter(|pages| !pages.is_empty())
    {
        convert_paged_multicol_slices(doc, node_id, &geometry.groups, &pages, ctx, out);
        return;
    }
    for group in &geometry.groups {
        if group.paragraph_splits.is_empty() {
            continue;
        }
        let group_x_pt = group.x_offset.in_pt();
        let group_y_pt = group.y_offset.in_pt();
        let col_w_pt = group.col_w.in_pt();

        for split in &group.paragraph_splits {
            let source_id = split.source_node_id;
            let all_lines = shape_multicol_source(doc, node_id, source_id, group.col_w, ctx);
            if all_lines.is_empty() {
                continue;
            }

            let mut slices = Vec::new();
            for col_slice in &split.column_slices {
                if col_slice.line_range.is_empty() {
                    continue;
                }
                if col_slice.line_range.end > all_lines.len() {
                    debug_assert!(
                        false,
                        "ParagraphSplitEntry line_range {:?} exceeds shaped line count {}",
                        col_slice.line_range,
                        all_lines.len(),
                    );
                    continue;
                }

                // Rebase per-line baselines into slice-local space.
                // Same rebase used for continuation fragments after a page
                // break (commit 9c0e092):
                //
                // 1. `line.baseline -= consumed` shifts each line's
                //    baseline from parley-layout space (paragraph top →
                //    baseline) to slice-local space (slice top →
                //    baseline). `consumed` is the cumulative height of
                //    all *prior* parley lines that this slice does not
                //    own.
                // 2. `recalculate_paragraph_line_boxes` then defensively
                //    re-accumulates per-line `line_box` heights starting
                //    at zero — for GlyphRun-only slices the per-line
                //    height is already parley-final so this is a no-op
                //    on baselines, but the call keeps the slice contract
                //    aligned with `ParagraphEntry::lines`'s contract
                //    (every consumer of `Vec<ShapedLine>` expects
                //    `recalculate_paragraph_line_boxes` to have run).
                let consumed: crate::units::Pt = all_lines[..col_slice.line_range.start]
                    .iter()
                    .map(|l| l.height)
                    .sum();
                let mut lines: Vec<crate::paragraph::ShapedLine> = all_lines
                    [col_slice.line_range.clone()]
                .iter()
                .cloned()
                .map(|mut l| {
                    l.baseline -= consumed;
                    l
                })
                .collect();
                inline_root::recalculate_paragraph_line_boxes(&mut lines);

                // `group_*_pt`/`col_w_pt` (Pt, hoisted above) plus the `Px`
                // `col_slice` geometry, summed in Pt space. Same fold as the
                // pre-migration f32 code — byte-neutral.
                let origin_pt = (
                    group_x_pt + col_slice.origin.x.in_pt(),
                    group_y_pt + col_slice.origin.y.in_pt(),
                );
                let size_pt = (col_w_pt, col_slice.size.height.in_pt());

                slices.push(crate::drawables::ParagraphSlice {
                    origin_pt,
                    size_pt,
                    lines,
                    page_index: None,
                });
            }

            if !slices.is_empty() {
                out.paragraph_slices.insert(
                    source_id,
                    crate::drawables::ParagraphSlicesEntry {
                        container_node_id: node_id,
                        slices,
                    },
                );
            }
        }
    }
}

/// F30d: the paragraph slices of a multicol container the fragmenter cut over pages: one per
/// [`crate::pagination_layout::McPiece::Lines`], painted on its page and in its column as the plan placed it,
/// not derived again from heights (the F78 rule).
///
/// Each slice holds the lines `[first, end)` of its source, rebased like the continuation fragments of a split
/// paragraph (by the advances of the lines before `first`), and sits at the column's x and the piece's y below
/// the top of its row (F161: [`crate::pagination_layout::McPage::y`], measured from the top of the container's
/// fragment on that page, below the container's top padding and border on the first page). `groups` are the
/// container's column groups; every row names its own.
fn convert_paged_multicol_slices(
    doc: &BaseDocument,
    node_id: usize,
    groups: &[crate::multicol_layout::ColumnGroupGeometry],
    pages: &[(u32, crate::pagination_layout::McPage)],
    ctx: &mut ConvertContext<'_>,
    out: &mut crate::drawables::Drawables,
) {
    use crate::pagination_layout::McPiece;
    use crate::units::F32Units;
    let mut shaped: BTreeMap<usize, Vec<ShapedLine>> = BTreeMap::new();
    let mut slices_by_source: BTreeMap<usize, Vec<crate::drawables::ParagraphSlice>> =
        BTreeMap::new();
    for (page, mc_page) in pages {
        let Some(group) = groups.get(mc_page.group as usize) else {
            debug_assert!(
                false,
                "multicol row names the missing group {}",
                mc_page.group
            );
            continue;
        };
        let stride = group.col_w.to_f32() + group.gap.to_f32();
        let col_w_pt = group.col_w.in_pt();
        let top = mc_page.y.to_f32();
        for (col, pieces) in mc_page.columns.iter().enumerate() {
            for piece in pieces {
                let McPiece::Lines {
                    source,
                    first,
                    end,
                    y,
                } = piece
                else {
                    continue;
                };
                let all_lines = shaped.entry(*source).or_insert_with(|| {
                    shape_multicol_source(doc, node_id, *source, group.col_w, ctx)
                });
                let (first, end) = (*first as usize, *end as usize);
                if first >= end || end > all_lines.len() {
                    debug_assert!(
                        false,
                        "multicol piece {first}..{end} exceeds the shaped line count {}",
                        all_lines.len()
                    );
                    continue;
                }
                let consumed: crate::units::Pt = all_lines[..first].iter().map(|l| l.height).sum();
                let mut lines: Vec<ShapedLine> = all_lines[first..end]
                    .iter()
                    .cloned()
                    .map(|mut l| {
                        l.baseline -= consumed;
                        l
                    })
                    .collect();
                inline_root::recalculate_paragraph_line_boxes(&mut lines);
                let height: crate::units::Pt = lines.iter().map(|l| l.height).sum();
                let origin_pt = (
                    (group.x_offset.to_f32() + col as f32 * stride)
                        .as_px()
                        .in_pt(),
                    (top + y.to_f32()).as_px().in_pt(),
                );
                slices_by_source.entry(*source).or_default().push(
                    crate::drawables::ParagraphSlice {
                        origin_pt,
                        size_pt: (col_w_pt, height),
                        lines,
                        page_index: Some(*page),
                    },
                );
            }
        }
    }
    for (source, slices) in slices_by_source {
        out.paragraph_slices.insert(
            source,
            crate::drawables::ParagraphSlicesEntry {
                container_node_id: node_id,
                slices,
            },
        );
    }
}

/// The shaped lines of the paragraph `source_id` of the multicol container `container_id`, at the column
/// width `col_w`: every line of its Parley layout, baselines measured from the paragraph's top.
///
/// Two source-layout sources, distinguished by whether `source_id == container_id`:
///
/// - **Case A** (container is itself an inline root): the container's `inline_layout_data.layout` was shaped
///   at the container's content width; multicol recorded line indices against a clone re-broken at `col_w`.
///   This reproduces that re-broken clone.
/// - **Case B** (a child element is the inline root): Blitz already re-broke the child's parley layout at
///   `col_w` during `compute_child_layout`, so the line indices stored against `inline_layout_data` line up
///   directly.
///
/// Empty when the node has no inline layout.
fn shape_multicol_source(
    doc: &BaseDocument,
    container_id: usize,
    source_id: usize,
    col_w: crate::units::Px,
    ctx: &mut ConvertContext<'_>,
) -> Vec<ShapedLine> {
    let case_a = source_id == container_id;
    // Pull the source `(layout, text)` pair from the source
    // node's `inline_layout_data`. For Case A we additionally
    // clone the layout and re-break at `col_w` so the line
    // indices recorded by `layout_self_inline_root_container`
    // resolve correctly. (Case B's layout was already re-broken
    // by Blitz during `compute_child_layout`.)
    let Some(source_node) = doc.get_node(source_id) else {
        return Vec::new();
    };
    let Some(elem) = source_node.element_data() else {
        return Vec::new();
    };
    let Some(text_layout) = elem.inline_layout_data.as_ref() else {
        return Vec::new();
    };
    let text: std::sync::Arc<str> = std::sync::Arc::from(text_layout.text.as_str());

    // Hold the rebroken clone alive across the per-line loop in
    // Case A. In Case B the borrowed reference into Blitz is
    // sufficient because Blitz already broke at `col_w`.
    //
    // For alignment: Blitz's own inline layout pass aligns each
    // inline-root layout with that node's resolved
    // `text-align`. Case B inherits that alignment for free
    // because we read the existing parley layout in place. Case
    // A re-clones + re-breaks here, which would otherwise
    // discard alignment unless we re-apply it. The container
    // (which IS the inline root in Case A — `source_id ==
    // node_id`) supplies the keyword via its primary styles,
    // matching the mapping at
    // `blitz-dom-0.2.4/src/layout/inline.rs:142-152`.
    let owned_layout: Option<parley::Layout<blitz_dom::node::TextBrush>> = if case_a {
        let alignment = source_node
            .primary_styles()
            .map(|s| css_text_align_to_parley_alignment(s.clone_text_align()))
            .unwrap_or(parley::Alignment::Start);
        let mut cloned = text_layout.layout.clone();
        cloned.break_all_lines(Some(col_w.to_f32()));
        cloned.align(
            Some(col_w.to_f32()),
            alignment,
            parley::AlignmentOptions::default(),
        );
        Some(cloned)
    } else {
        None
    };
    let layout_ref: &parley::Layout<blitz_dom::node::TextBrush> = match &owned_layout {
        Some(cloned) => cloned,
        None => &text_layout.layout,
    };

    // Materialise one `ShapedLine` per parley line. This is a
    // simplified version of the per-line shaping in
    // `inline_root::extract_paragraph` covering only `GlyphRun`s
    // — see this function's doc comment for scope notes.
    shape_paragraph_glyph_runs(doc, layout_ref, &text, ctx)
}

/// Shape one `Vec<ShapedLine>` out of a parley layout, covering only
/// `GlyphRun` items (no inline boxes). Used by
/// [`convert_multicol_paragraph_slices`] — see that function for scope
/// notes. Mirrors the `GlyphRun` arm of `inline_root::extract_paragraph`'s
/// per-line loop.
///
/// `ShapedLine.baseline` for each emitted line is the **parley layout's
/// cumulative offset from the layout top edge to that line's baseline**
/// (not line-local; not page-absolute). For line `i`, this equals
/// `Σ_{k=0..i} line_height[k] - leading_below[i] + ascent[i]` as parley
/// reports it via `LineMetrics::baseline`. Convert consumers must rebase
/// this value (subtract the slice's prior consumed height, then call
/// `inline_root::recalculate_paragraph_line_boxes`) when emitting
/// per-slice fragments — see `convert_multicol_paragraph_slices`.
fn shape_paragraph_glyph_runs(
    doc: &BaseDocument,
    parley_layout: &parley::Layout<blitz_dom::node::TextBrush>,
    text: &std::sync::Arc<str>,
    ctx: &mut ConvertContext<'_>,
) -> Vec<ShapedLine> {
    let mut shaped_lines = Vec::new();
    // Parley keeps a cluster's offset within its run as `u16`, so in a paragraph over 65 535 bytes the glyphs'
    // text ranges wrap (F70, `extract_paragraph`): the text layer would repeat earlier words in place of the
    // later ones. Undone here the same way, for left-to-right text; the clusters of a run are visited in text
    // order, once per run on a line, as the un-wrapping needs.
    let mut unwrap = (text.len() > u16::MAX as usize
        && !parley_layout.lines().any(|l| l.runs().any(|r| r.is_rtl())))
    .then(inline_root::ClusterUnwrap::default);
    let mut run_ranges: Vec<std::ops::Range<usize>> = Vec::new();
    for line in parley_layout.lines() {
        let metrics = line.metrics();
        let mut items = Vec::new();
        let mut prev_run_key = usize::MAX;
        let mut run_glyph_offset = 0usize;
        for item in line.items() {
            if let parley::PositionedLayoutItem::GlyphRun(glyph_run) = item {
                let run = glyph_run.run();
                let font_ref = run.font();
                let font_index = font_ref.index;
                let font_arc = ctx.get_or_insert_font(font_ref);
                let font_size_parley = run.font_size();
                let font_size = font_size_parley.as_px().in_pt();

                let brush = &glyph_run.style().brush;
                let color = get_text_color(doc, brush.id);
                let decoration = get_text_decoration(doc, brush.id);
                let link = ctx.link_cache.lookup(doc, brush.id);

                let run_key = run.cluster_range().start;
                if run_key != prev_run_key {
                    prev_run_key = run_key;
                    run_glyph_offset = 0;
                    if let Some(u) = unwrap.as_mut() {
                        run_ranges.clear();
                        run_ranges.extend(run.visual_clusters().map(|c| u.fix(c.text_range())));
                    }
                }
                let glyph_start = run_glyph_offset;

                let mut annotated = run
                    .visual_clusters()
                    .enumerate()
                    .flat_map(|(i, cluster)| {
                        let r = match unwrap {
                            Some(_) => run_ranges[i].clone(),
                            None => cluster.text_range(),
                        };
                        cluster.glyphs().map(move |g| (r.clone(), g))
                    })
                    .skip(glyph_start);

                let mut glyphs = Vec::new();
                for g in glyph_run.glyphs() {
                    let (text_range, _) = annotated.next().unwrap_or_else(|| {
                        panic!(
                            "annotated cluster iterator exhausted before glyph_run.glyphs(); \
                             run cluster_range={:?}, glyph_start={glyph_start}",
                            run.cluster_range()
                        )
                    });
                    run_glyph_offset += 1;
                    glyphs.push(ShapedGlyph {
                        id: g.id,
                        x_advance: ShapedGlyph::normalize_by_font_size(g.advance, font_size_parley),
                        x_offset: ShapedGlyph::normalize_by_font_size(g.x, font_size_parley),
                        y_offset: ShapedGlyph::normalize_by_font_size(g.y, font_size_parley),
                        text_range,
                    });
                }

                if !glyphs.is_empty() {
                    let run_text = std::sync::Arc::clone(text);
                    let run_x_offset = glyph_run.offset().as_px().in_pt();
                    items.push(LineItem::Text(ShapedGlyphRun {
                        font_data: font_arc,
                        font_index,
                        font_size,
                        color,
                        decoration,
                        glyphs,
                        text: run_text,
                        x_offset: run_x_offset,
                        link,
                    }));
                }
            }
            // InlineBox items are intentionally not handled — see
            // `convert_multicol_paragraph_slices`'s scope note.
        }

        let line_height = metrics.line_height.as_px().in_pt();
        shaped_lines.push(ShapedLine {
            height: line_height,
            baseline: metrics.baseline.as_px().in_pt(),
            items,
        });
    }
    shaped_lines
}

use crate::blitz_adapter::{extract_inline_svg_tree, get_attr};

/// Extract a trimmed, non-empty HTML `id` attribute from `node` and wrap it
/// in an `Arc<String>` so split fragments can share without cloning the string.
///
/// Returns `None` if the node has no element data, no `id` attribute, or an
/// empty/whitespace-only value.
fn extract_block_id(node: &Node) -> Option<Arc<String>> {
    let el = node.element_data()?;
    let raw = get_attr(el, "id")?.trim();
    if raw.is_empty() {
        None
    } else {
        Some(Arc::new(raw.to_string()))
    }
}

/// Whether `node` is a `::before` / `::after` pseudo-element, detected by
/// checking that its parent's `before` / `after` slot points back to it.
fn is_pseudo_node(doc: &BaseDocument, node: &Node) -> bool {
    node.parent
        .and_then(|pid| doc.get_node(pid))
        .is_some_and(|p| p.before == Some(node.id) || p.after == Some(node.id))
}

/// Geometry of a parent's content-box, used by the pseudo-image helpers so
/// `::before`/`::after` land at the content-box corners (not the border-box
/// corners) and percentage sizes resolve against the content-box dimensions.
///
/// `origin_x` / `origin_y` were once used by `wrap_with_block_pseudo_images`
/// to position pseudo images at the content-box top-left / bottom-left.
/// In v2 the render path derives those positions from `pagination_geometry`,
/// so the fields are kept (for the eventual abs/fixed migration) but
/// allowed to be dead-code-eliminated.
#[derive(Clone, Copy)]
#[allow(dead_code)]
struct ContentBox {
    origin_x: Px,
    origin_y: Px,
    width: Px,
    height: Px,
}

/// Compute the content-box of `node` from its computed style + Taffy layout.
/// Thin wrapper that projects `Node` + `BlockStyle` to primitives and
/// delegates the arithmetic to `content_box_from_geometry` (which is unit-
/// tested).
fn compute_content_box(node: &Node, style: &BlockStyle) -> ContentBox {
    let border_w = node.final_layout.size.width.as_px();
    let border_h = node.final_layout.size.height.as_px();
    content_box_from_geometry(border_w, border_h, &style.border_widths, &style.padding)
}

/// Pure arithmetic behind `compute_content_box`. Extracted so the
/// contract can be locked in by unit tests without fabricating a Blitz
/// `Node`.
///
/// All returned values are in **CSS px** (Blitz/Taffy layout space) — see
/// `.claude/rules/coordinate-system.md`.
///
/// Each inset side runs `.in_px()` on the border and padding operand
/// *separately* before summing (`(border[i].in_px()) + (padding[i].in_px())`
/// rather than `(border[i] + padding[i]).in_px()`). This is intentional:
/// it matches the float-op order that fulgur-m0xl's byte-neutral caller
/// stopgap performed, so any drift beyond the reordering documented in the
/// fulgur-bfu9 commit message stays out of the goldens. Do not fold the
/// two `.in_px()` calls per side into one.
fn content_box_from_geometry(
    border_w: Px,
    border_h: Px,
    border_widths: &[Pt; 4],
    padding: &[Pt; 4],
) -> ContentBox {
    // `border_widths` / `padding` are stored in CSS TRBL order:
    // [0]=top, [1]=right, [2]=bottom, [3]=left.
    let left_inset = border_widths[3].in_px() + padding[3].in_px();
    let top_inset = border_widths[0].in_px() + padding[0].in_px();
    let right_inset = border_widths[1].in_px() + padding[1].in_px();
    let bottom_inset = border_widths[2].in_px() + padding[2].in_px();
    ContentBox {
        origin_x: left_inset,
        origin_y: top_inset,
        width: (border_w - left_inset - right_inset).max(Px::ZERO),
        height: (border_h - top_inset - bottom_inset).max(Px::ZERO),
    }
}

/// Memoized lookup of the enclosing `<a href>` for a node.
#[derive(Default)]
pub(crate) struct LinkCache {
    by_start: HashMap<usize, Option<usize>>,
    by_anchor: HashMap<usize, Arc<LinkSpan>>,
}

impl LinkCache {
    pub(crate) fn lookup(&mut self, doc: &BaseDocument, start_id: usize) -> Option<Arc<LinkSpan>> {
        if let Some(cached) = self.by_start.get(&start_id) {
            let anchor_id = (*cached)?;
            return self.by_anchor.get(&anchor_id).cloned();
        }
        match inline_root::resolve_enclosing_anchor(doc, start_id) {
            Some((anchor_id, span)) => {
                self.by_start.insert(start_id, Some(anchor_id));
                let arc = self
                    .by_anchor
                    .entry(anchor_id)
                    .or_insert_with(|| Arc::new(span))
                    .clone();
                Some(arc)
            }
            None => {
                self.by_start.insert(start_id, None);
                None
            }
        }
    }
}

/// Extract the asset name from a URL that Stylo may have resolved to absolute.
fn extract_asset_name(url: &str) -> &str {
    url.strip_prefix("file:///").unwrap_or(url)
}

/// Check if a node is a non-visual element (head, script, style, etc.)
fn is_non_visual_element(node: &Node) -> bool {
    if let Some(elem) = node.element_data() {
        let tag = elem.name.local.as_ref();
        matches!(
            tag,
            "head" | "script" | "style" | "link" | "meta" | "title" | "noscript"
        )
    } else {
        false
    }
}

/// Get text color from a DOM node's computed styles.
fn get_text_color(doc: &BaseDocument, node_id: usize) -> [u8; 4] {
    if let Some(node) = doc.get_node(node_id)
        && let Some(styles) = node.primary_styles()
    {
        return absolute_to_rgba(styles.clone_color());
    }
    [0, 0, 0, 255] // Default: black
}

/// Get text-decoration properties from a DOM node's computed styles.
fn get_text_decoration(doc: &BaseDocument, node_id: usize) -> TextDecoration {
    if let Some(node) = doc.get_node(node_id)
        && let Some(styles) = node.primary_styles()
    {
        let current_color = styles.clone_color();

        // text-decoration-line (bitflags)
        let stylo_line = styles.clone_text_decoration_line();
        let mut line = TextDecorationLine::NONE;
        if stylo_line.contains(::style::values::specified::TextDecorationLine::UNDERLINE) {
            line = line | TextDecorationLine::UNDERLINE;
        }
        if stylo_line.contains(::style::values::specified::TextDecorationLine::OVERLINE) {
            line = line | TextDecorationLine::OVERLINE;
        }
        if stylo_line.contains(::style::values::specified::TextDecorationLine::LINE_THROUGH) {
            line = line | TextDecorationLine::LINE_THROUGH;
        }

        // text-decoration-style
        use ::style::properties::longhands::text_decoration_style::computed_value::T as StyloTDS;
        let style = match styles.clone_text_decoration_style() {
            StyloTDS::Solid => TextDecorationStyle::Solid,
            StyloTDS::Dashed => TextDecorationStyle::Dashed,
            StyloTDS::Dotted => TextDecorationStyle::Dotted,
            StyloTDS::Double => TextDecorationStyle::Double,
            StyloTDS::Wavy => TextDecorationStyle::Wavy,
            _ => TextDecorationStyle::Solid,
        };

        // text-decoration-color (resolve currentcolor)
        let deco_color = styles.clone_text_decoration_color();
        let color = absolute_to_rgba(deco_color.resolve_to_absolute(&current_color));

        return TextDecoration { line, style, color };
    }
    TextDecoration::default()
}

#[cfg(test)]
mod bookmark_outline_tests {
    //! Single-test mod kept post PR 8i: the migrated end-to-end check that
    //! bookmark anchors reach the v2 outline pipeline. The v1-extractor unit
    //! tests that previously lived in `extract_drawables_tests` were
    //! redundant after the convert layer started writing into Drawables
    //! directly, so they were deleted along with the extractor.

    /// Regression: `dom_to_drawables` must snapshot `ctx.bookmark_by_node`
    /// **before** walking the DOM. End-to-end test through `Engine::render_html`
    /// (defaulted to v2 in PR 7) with `bookmarks(true)`: the rendered PDF
    /// must contain `/Outlines` because the v2 path builds the outline.
    #[test]
    fn dom_to_drawables_preserves_bookmark_anchors_for_outline() {
        use crate::config::PageSize;
        use crate::engine::Engine;

        let html = "<!DOCTYPE html><html><head><style>body{margin:0;padding:0}</style></head><body><h1>Heading</h1></body></html>";
        let engine = Engine::builder()
            .page_size(PageSize::A4)
            .bookmarks(true)
            .build();
        let pdf = engine.render(html).expect("render v2");
        let pdf_str = String::from_utf8_lossy(&pdf);
        assert!(
            pdf_str.contains("/Outlines"),
            "bookmark anchors must reach the v2 outline pipeline; PDF missing /Outlines"
        );
    }
}

#[cfg(test)]
mod semantics_tests {
    //! fulgur-izp.3: convert-side semantic tag classification. Verifies
    //! `dom_to_drawables` populates `Drawables.semantics` with the
    //! expected `(tag, parent)` pairs for representative HTML fixtures.
    //!
    //! Render-side wire-up (`fulgur-izp.4`) and StructTree assembly
    //! (`fulgur-izp.5`) are out of scope; these tests assert the data
    //! shape only.

    use crate::tagging::PdfTag;
    use crate::units::F32Units;
    use std::ops::DerefMut;

    fn build_drawables(html: &str) -> crate::drawables::Drawables {
        // Drive the convert pipeline directly without the full Engine
        // so the assertions stay focused on `dom_to_drawables`.
        // `parse_and_layout` already runs stylo + Taffy + the
        // `position: fixed` relayout, matching what the engine feeds
        // into convert at this point.
        let mut doc = crate::blitz_adapter::parse_and_layout(
            html,
            595.0_f32.as_px(),
            842.0_f32.as_px(),
            &[],
            true,
        );

        let column_styles = crate::blitz_adapter::extract_column_style_table(&doc);
        let multicol_geometry = crate::multicol_layout::run_pass(doc.deref_mut(), &column_styles);
        let pagination_geometry = crate::pagination_layout::run_pass(doc.deref_mut(), 842.0);
        let running_store = crate::gcpm::running::RunningElementStore::new();
        let mut ctx = super::ConvertContext {
            running_store: &running_store,
            assets: None,
            font_bytes: Default::default(),
            string_set_by_node: Default::default(),
            counter_ops_by_node: Default::default(),
            bookmark_by_node: Default::default(),
            column_styles,
            multicol_geometry,
            pagination_geometry,
            link_cache: Default::default(),
            dropped_text: Default::default(),
            viewport_size_px: Some((595.0, 842.0)),
        };
        super::dom_to_drawables(&doc, &mut ctx)
    }

    /// F102: a node the running-element pass registered is not converted, whatever its computed `display`. The
    /// engine sets `display: none` on it, but an author `!important` declaration beats that, and the element
    /// would be painted (and linked and tagged) a second time in the body.
    #[test]
    fn convert_skips_running_node_with_important_display() {
        let html = "<!DOCTYPE html><html><head><style>\
            .run { position: running(pageHeader); display: block !important; }\
            </style></head><body><div class=\"run\">RUNNING <a href=\"https://example.cz/r\">link</a></div>\
            <p>Body text</p></body></html>";
        let mut doc = crate::blitz_adapter::parse(html, 595.0, &[]);
        let gcpm = crate::gcpm::parser::parse_gcpm(".run { position: running(pageHeader); }");
        let pass = crate::blitz_adapter::RunningElementPass::new(gcpm.running_mappings);
        let pass_ctx = crate::blitz_adapter::PassContext { font_data: &[] };
        crate::blitz_adapter::apply_single_pass(&pass, &mut doc, &pass_ctx);
        let running_store = pass.into_running_store();
        crate::blitz_adapter::resolve(&mut doc);

        let run_id = doc.query_selector(".run").unwrap().expect(".run node");
        assert!(running_store.instance_for_node(run_id).is_some());
        let column_styles = crate::blitz_adapter::extract_column_style_table(&doc);
        let multicol_geometry = crate::multicol_layout::run_pass(doc.deref_mut(), &column_styles);
        let pagination_geometry = crate::pagination_layout::run_pass_with_break_and_running(
            doc.deref_mut(),
            842.0,
            &column_styles,
            &running_store,
        );
        let mut ctx = super::ConvertContext {
            running_store: &running_store,
            assets: None,
            font_bytes: Default::default(),
            string_set_by_node: Default::default(),
            counter_ops_by_node: Default::default(),
            bookmark_by_node: Default::default(),
            column_styles,
            multicol_geometry,
            pagination_geometry,
            link_cache: Default::default(),
            dropped_text: Default::default(),
            viewport_size_px: Some((595.0, 842.0)),
        };
        let d = super::dom_to_drawables(&doc, &mut ctx);

        assert!(
            !d.paragraphs.contains_key(&run_id) && !d.block_styles.contains_key(&run_id),
            "the registered running node must not be converted"
        );
        assert!(
            !d.semantics.contains_key(&run_id),
            "the registered running node must not be tagged"
        );
        assert!(
            d.paragraphs.iter().count() >= 1,
            "the body paragraph is still converted"
        );
    }

    fn entries_by_tag(
        d: &crate::drawables::Drawables,
        target: &PdfTag,
    ) -> Vec<(usize, Option<usize>)> {
        d.semantics
            .iter()
            .filter(|(_, e)| e.tag == *target)
            .map(|(id, e)| (*id, e.parent))
            .collect()
    }

    #[test]
    fn dom_to_drawables_records_semantic_entries_for_block_elements() {
        let html = "<!DOCTYPE html><html><body><h1>T</h1><p>x</p><div><img src='a.png' alt='a'></div><p>y <span>inside</span> z</p></body></html>";
        let d = build_drawables(html);

        let h1s = entries_by_tag(&d, &PdfTag::H { level: 1 });
        assert_eq!(h1s.len(), 1, "expected one h1 entry");
        let ps = entries_by_tag(&d, &PdfTag::P);
        assert_eq!(ps.len(), 2, "expected two p entries");
        let divs = entries_by_tag(&d, &PdfTag::Div);
        assert_eq!(divs.len(), 1, "expected one div entry");
        let figures = entries_by_tag(&d, &PdfTag::Figure);
        assert_eq!(figures.len(), 1, "expected one figure entry for <img>");
        let spans = entries_by_tag(&d, &PdfTag::Span);
        assert_eq!(spans.len(), 1, "expected one span entry");

        let (img_id, img_parent) = figures[0];
        let (div_id, _) = divs[0];
        assert_eq!(
            img_parent,
            Some(div_id),
            "img semantic parent should be its enclosing div, got {img_parent:?} for img id {img_id}"
        );

        // span's parent must be one of the recorded paragraphs — the
        // exact NodeId depends on Blitz's parse order which is stable
        // but not part of the contract under test. Asserting set
        // membership keeps the test robust to renumbering.
        let p_ids: std::collections::BTreeSet<_> = ps.iter().map(|(id, _)| *id).collect();
        let (_, span_parent) = spans[0];
        assert!(
            span_parent.map(|p| p_ids.contains(&p)).unwrap_or(false),
            "span parent must be one of the recorded p NodeIds, got {span_parent:?}"
        );
    }

    #[test]
    fn dom_to_drawables_records_semantic_entries_for_lists() {
        let html = "<!DOCTYPE html><html><body><ul><li>a</li><li>b</li></ul></body></html>";
        let d = build_drawables(html);
        let lists = entries_by_tag(
            &d,
            &PdfTag::L {
                numbering: krilla::tagging::ListNumbering::Disc,
            },
        );
        assert_eq!(lists.len(), 1, "expected one ul entry");
        let items = entries_by_tag(&d, &PdfTag::Li);
        assert_eq!(items.len(), 2, "expected two li entries");

        let (ul_id, _) = lists[0];
        for (li_id, parent) in &items {
            assert_eq!(
                *parent,
                Some(ul_id),
                "li {li_id} parent should be ul {ul_id}, got {parent:?}"
            );
        }
    }

    #[test]
    fn dom_to_drawables_records_semantic_entries_for_ordered_lists() {
        let html = "<!DOCTYPE html><html><body><ol><li>item</li></ol></body></html>";
        let d = build_drawables(html);
        let lists = entries_by_tag(
            &d,
            &PdfTag::L {
                numbering: krilla::tagging::ListNumbering::Decimal,
            },
        );
        assert_eq!(lists.len(), 1, "one ol in semantics");
    }

    #[test]
    fn dom_to_drawables_records_semantic_entries_for_tables() {
        let html = "<!DOCTYPE html><html><body><table><thead><tr><th>h</th></tr></thead><tbody><tr><td>d</td></tr></tbody></table></body></html>";
        let d = build_drawables(html);

        let tables = entries_by_tag(&d, &PdfTag::Table);
        assert_eq!(tables.len(), 1);
        let theads = entries_by_tag(&d, &PdfTag::THead);
        assert_eq!(theads.len(), 1, "one thead");
        let tbodies = entries_by_tag(&d, &PdfTag::TBody);
        assert_eq!(tbodies.len(), 1, "one tbody");
        // Collect thead + tbody together so the parent-check loop below works
        // the same way as before (both must parent to the table).
        let row_groups: Vec<_> = theads.iter().chain(tbodies.iter()).copied().collect();
        assert_eq!(row_groups.len(), 2, "thead + tbody");
        let rows = entries_by_tag(&d, &PdfTag::Tr);
        assert_eq!(rows.len(), 2);
        let ths = entries_by_tag(
            &d,
            &PdfTag::Th {
                scope: krilla::tagging::TableHeaderScope::Both,
            },
        );
        assert_eq!(ths.len(), 1);
        let tds = entries_by_tag(&d, &PdfTag::Td);
        assert_eq!(tds.len(), 1);

        let (table_id, _) = tables[0];
        for (_, parent) in &row_groups {
            assert_eq!(*parent, Some(table_id), "row group should parent to table");
        }
        // Each row's parent must be one of the row-group ids; each
        // header/data cell's parent must be one of the row ids. We
        // assert containment rather than specific ids because Blitz
        // assigns NodeIds during parse — the order is stable but
        // hard-coding ids would couple the test to internal numbering.
        let row_group_ids: std::collections::BTreeSet<_> =
            row_groups.iter().map(|(id, _)| *id).collect();
        for (_, parent) in &rows {
            assert!(
                parent.map(|p| row_group_ids.contains(&p)).unwrap_or(false),
                "tr parent must be a row group, got {parent:?}"
            );
        }
        let row_ids: std::collections::BTreeSet<_> = rows.iter().map(|(id, _)| *id).collect();
        for (_, parent) in ths.iter().chain(tds.iter()) {
            assert!(
                parent.map(|p| row_ids.contains(&p)).unwrap_or(false),
                "th/td parent must be a tr, got {parent:?}"
            );
        }
    }

    #[test]
    fn dom_to_drawables_skips_unrecognised_elements() {
        // Fixture intentionally contains only one classifiable element
        // (`<p>`). Everything else (`<script>`, `<a>`, `<custom-tag>`,
        // `<body>`, `<html>`) must produce no `SemanticEntry`. The
        // assertions below pin the *exact* contents of `semantics` so
        // any future regression that synthesises an extra entry from
        // an unrecognised element fails immediately, regardless of
        // which tag variant it picks.
        let html = "<!DOCTYPE html><html><body><script>x=1</script><a href='#'>link</a><custom-tag>y</custom-tag><p>z</p></body></html>";
        let d = build_drawables(html);

        let tags: Vec<&PdfTag> = d.semantics.values().map(|e| &e.tag).collect();
        assert_eq!(
            d.semantics.len(),
            1,
            "expected exactly one semantic entry for the <p>, got {} entries: {tags:?}",
            d.semantics.len()
        );
        let only_entry = d.semantics.values().next().expect("one entry asserted");
        assert_eq!(
            only_entry.tag,
            PdfTag::P,
            "the single entry must be the <p>, got {:?}",
            only_entry.tag
        );
    }

    #[test]
    fn walk_semantics_li_creates_lbl_and_lbody_synthetic_entries() {
        let html = "<!DOCTYPE html><html><body><ul><li>item</li></ul></body></html>";
        let d = build_drawables(html);

        // li_lbl_ids と li_lbody_ids に 1 エントリずつあること
        assert_eq!(d.li_lbl_ids.len(), 1, "one li_lbl_ids entry");
        assert_eq!(d.li_lbody_ids.len(), 1, "one li_lbody_ids entry");

        // li_lbl_ids のキー = li NodeId、値 = Lbl 合成 NodeId
        let (&li_id, &lbl_id) = d.li_lbl_ids.iter().next().unwrap();
        let lbody_id = d.li_lbody_ids[&li_id];

        // semantics に Lbl エントリがあり、parent = li_id
        let lbl_entry = &d.semantics[&lbl_id];
        assert_eq!(lbl_entry.tag, PdfTag::Lbl);
        assert_eq!(lbl_entry.parent, Some(li_id));

        // semantics に LBody エントリがあり、parent = li_id
        let lbody_entry = &d.semantics[&lbody_id];
        assert_eq!(lbody_entry.tag, PdfTag::LBody);
        assert_eq!(lbody_entry.parent, Some(li_id));
    }

    #[test]
    fn walk_semantics_li_child_span_parent_is_lbody() {
        // li 直下の <span> は lbody_id を親に持つ
        let html = "<!DOCTYPE html><html><body><ul><li><span>text</span></li></ul></body></html>";
        let d = build_drawables(html);
        let (&li_id, _) = d.li_lbl_ids.iter().next().unwrap();
        let lbody_id = d.li_lbody_ids[&li_id];

        let span_entries: Vec<_> = d
            .semantics
            .iter()
            .filter(|(_, e)| e.tag == PdfTag::Span)
            .collect();
        assert!(
            !span_entries.is_empty(),
            "span inside li should be in semantics"
        );
        for (_, entry) in &span_entries {
            assert_eq!(
                entry.parent,
                Some(lbody_id),
                "span inside li should have lbody_id as parent, not li_id"
            );
        }
    }

    #[test]
    fn walk_semantics_nested_list_structure() {
        let html = "<!DOCTYPE html><html><body>\
            <ul><li><ol><li>nested</li></ol></li></ul>\
            </body></html>";
        let d = build_drawables(html);

        // 2 つの li があるので li_lbl_ids に 2 エントリ
        assert_eq!(d.li_lbl_ids.len(), 2);

        // inner ol の L エントリが存在すること
        let decimal_lists = entries_by_tag(
            &d,
            &PdfTag::L {
                numbering: krilla::tagging::ListNumbering::Decimal,
            },
        );
        assert_eq!(decimal_lists.len(), 1, "one ol (Decimal)");
        let (_, inner_ol_parent) = decimal_lists[0];

        // inner ol の parent は outer li の lbody_id であること
        let outer_li_ids: Vec<_> = d
            .li_lbl_ids
            .keys()
            .copied()
            .filter(|&id| d.li_lbody_ids.get(&id).copied() == inner_ol_parent)
            .collect();
        assert_eq!(
            outer_li_ids.len(),
            1,
            "inner ol parent should be outer li's lbody"
        );
    }

    #[test]
    fn dom_to_drawables_records_alt_text_on_figure() {
        let figure_alt = |html: &str| {
            let d = build_drawables(html);
            let figures: Vec<_> = d
                .semantics
                .values()
                .filter(|e| e.tag == PdfTag::Figure)
                .collect();
            assert_eq!(figures.len(), 1);
            figures[0].alt_text.clone()
        };

        assert_eq!(
            figure_alt(
                "<!DOCTYPE html><html><body><img src='a.png' alt='photo of cat'></body></html>"
            )
            .as_deref(),
            Some("photo of cat"),
            "alt text should be captured"
        );
        assert_eq!(
            figure_alt("<!DOCTYPE html><html><body><img src='a.png' alt=''></body></html>")
                .as_deref(),
            Some(""),
            "empty alt should be Some(\"\")"
        );
        assert_eq!(
            figure_alt("<!DOCTYPE html><html><body><img src='a.png'></body></html>"),
            None,
            "missing alt should be None"
        );
    }

    #[test]
    fn dom_to_drawables_drops_alt_text_on_invisible_figure() {
        // CSS 2 §11.2 + WAI-ARIA: `visibility: hidden` / `collapse`
        // excludes the subtree from the accessibility tree (Chromium /
        // Firefox agreement). The Figure tag's `/Alt` attribute is an
        // accessibility payload, so an invisible <img> must not surface
        // its `alt` text through it — same regression class as the
        // heading `/T` gate (heading_title_of in render.rs).
        let figure_alt = |html: &str| {
            let d = build_drawables(html);
            let figures: Vec<_> = d
                .semantics
                .values()
                .filter(|e| e.tag == PdfTag::Figure)
                .collect();
            assert_eq!(figures.len(), 1);
            figures[0].alt_text.clone()
        };

        assert_eq!(
            figure_alt(
                "<!DOCTYPE html><html><body>\
                 <img src='a.png' alt='hidden secret' style='visibility:hidden'>\
                 </body></html>"
            ),
            None,
            "visibility:hidden should drop the alt payload"
        );
        assert_eq!(
            figure_alt(
                "<!DOCTYPE html><html><body>\
                 <img src='a.png' alt='collapsed secret' style='visibility:collapse'>\
                 </body></html>"
            ),
            None,
            "visibility:collapse should drop the alt payload"
        );
    }

    // ── walk_semantics: <th scope> attribute handling ─────────────────────

    #[test]
    fn walk_semantics_th_scope_row_overrides_default() {
        // <th scope="row"> must produce TableHeaderScope::Row, not the
        // default Both that classify_element initialises.
        let html = "<!DOCTYPE html><html><body>\
            <table><tr>\
                <th scope=\"row\">Row header</th>\
                <td>data</td>\
            </tr></table>\
            </body></html>";
        let d = build_drawables(html);
        let row_ths = entries_by_tag(
            &d,
            &PdfTag::Th {
                scope: krilla::tagging::TableHeaderScope::Row,
            },
        );
        assert_eq!(row_ths.len(), 1, "one th with scope=row");
        let both_ths = entries_by_tag(
            &d,
            &PdfTag::Th {
                scope: krilla::tagging::TableHeaderScope::Both,
            },
        );
        assert_eq!(
            both_ths.len(),
            0,
            "no default-scope th when scope=row is set"
        );
    }

    #[test]
    fn walk_semantics_th_scope_col_overrides_default() {
        // <th scope="col"> must produce TableHeaderScope::Column.
        let html = "<!DOCTYPE html><html><body>\
            <table><tr>\
                <th scope=\"col\">Col header 1</th>\
                <th scope=\"column\">Col header 2</th>\
            </tr></table>\
            </body></html>";
        let d = build_drawables(html);
        let col_ths = entries_by_tag(
            &d,
            &PdfTag::Th {
                scope: krilla::tagging::TableHeaderScope::Column,
            },
        );
        assert_eq!(col_ths.len(), 2, "two th with scope=col/column");
    }

    #[test]
    fn walk_semantics_th_unrecognised_scope_falls_back_to_both() {
        // An unrecognised scope value (e.g. "rowgroup") produces the Both
        // fallback set by the `unwrap_or` in walk_semantics.
        let html = "<!DOCTYPE html><html><body>\
            <table><tr>\
                <th scope=\"rowgroup\">Group header</th>\
            </tr></table>\
            </body></html>";
        let d = build_drawables(html);
        let both_ths = entries_by_tag(
            &d,
            &PdfTag::Th {
                scope: krilla::tagging::TableHeaderScope::Both,
            },
        );
        assert_eq!(both_ths.len(), 1, "unrecognised scope falls back to Both");
    }

    // ── walk_semantics: list-style-type CSS override ──────────────────────

    #[test]
    fn walk_semantics_list_style_type_circle_maps_to_circle() {
        // <ul style="list-style-type:circle"> must override the default Disc
        // numbering that classify_element("ul") sets.
        let html = "<!DOCTYPE html><html><body>\
            <ul style=\"list-style-type:circle\"><li>item</li></ul>\
            </body></html>";
        let d = build_drawables(html);
        let circle_lists = entries_by_tag(
            &d,
            &PdfTag::L {
                numbering: krilla::tagging::ListNumbering::Circle,
            },
        );
        assert_eq!(circle_lists.len(), 1, "one ul with Circle numbering");
        let disc_lists = entries_by_tag(
            &d,
            &PdfTag::L {
                numbering: krilla::tagging::ListNumbering::Disc,
            },
        );
        assert_eq!(disc_lists.len(), 0, "no Disc list when circle is set");
    }

    #[test]
    fn walk_semantics_list_style_type_square_maps_to_square() {
        let html = "<!DOCTYPE html><html><body>\
            <ul style=\"list-style-type:square\"><li>item</li></ul>\
            </body></html>";
        let d = build_drawables(html);
        let square_lists = entries_by_tag(
            &d,
            &PdfTag::L {
                numbering: krilla::tagging::ListNumbering::Square,
            },
        );
        assert_eq!(square_lists.len(), 1, "one ul with Square numbering");
    }

    #[test]
    fn walk_semantics_list_style_type_lower_alpha_maps_to_lower_alpha() {
        let html = "<!DOCTYPE html><html><body>\
            <ol style=\"list-style-type:lower-alpha\"><li>a</li></ol>\
            </body></html>";
        let d = build_drawables(html);
        let alpha_lists = entries_by_tag(
            &d,
            &PdfTag::L {
                numbering: krilla::tagging::ListNumbering::LowerAlpha,
            },
        );
        assert_eq!(alpha_lists.len(), 1, "one ol with LowerAlpha numbering");
    }

    #[test]
    fn walk_semantics_list_style_type_upper_alpha_maps_to_upper_alpha() {
        let html = "<!DOCTYPE html><html><body>\
            <ol style=\"list-style-type:upper-alpha\"><li>A</li></ol>\
            </body></html>";
        let d = build_drawables(html);
        let alpha_lists = entries_by_tag(
            &d,
            &PdfTag::L {
                numbering: krilla::tagging::ListNumbering::UpperAlpha,
            },
        );
        assert_eq!(alpha_lists.len(), 1, "one ol with UpperAlpha numbering");
    }

    #[test]
    fn walk_semantics_list_style_type_unhandled_maps_to_none() {
        // lower-greek is a CSS keyword Stylo/servo recognises (it is in servo's
        // single_keyword list) but is NOT in our explicit match arms, so it hits
        // `_ => ListNumbering::None`. lower-roman is NOT in servo's list, so
        // the inline style is rejected and the UA-stylesheet default takes over.
        let html = "<!DOCTYPE html><html><body>\
            <ol style=\"list-style-type:lower-greek\"><li>α</li></ol>\
            </body></html>";
        let d = build_drawables(html);
        let none_lists = entries_by_tag(
            &d,
            &PdfTag::L {
                numbering: krilla::tagging::ListNumbering::None,
            },
        );
        assert_eq!(
            none_lists.len(),
            1,
            "lower-greek maps to ListNumbering::None"
        );
    }
}

#[cfg(test)]
mod utility_fn_tests {
    //! Unit tests for pure utility functions in this module.
    //! These exercise branches that the integration tests (semantics_tests,
    //! bookmark_outline_tests) do not reach because they rely on unusual CSS
    //! keyword variants or specific URL schemes.

    use super::*;

    // --- css_text_align_to_parley_alignment ---

    #[test]
    fn text_align_all_variants() {
        use ::style::values::specified::TextAlignKeyword;
        let cases: &[(TextAlignKeyword, parley::Alignment)] = &[
            (TextAlignKeyword::Start, parley::Alignment::Start),
            (TextAlignKeyword::Left, parley::Alignment::Left),
            (TextAlignKeyword::Right, parley::Alignment::Right),
            (TextAlignKeyword::Center, parley::Alignment::Center),
            (TextAlignKeyword::Justify, parley::Alignment::Justify),
            (TextAlignKeyword::End, parley::Alignment::End),
            // Moz* aliases map to their logical equivalents.
            // These keywords can't be produced from normal author CSS in Stylo
            // (they originate from internal browser UA/quirks paths), so the
            // only way to test them is through direct enum construction.
            (TextAlignKeyword::MozCenter, parley::Alignment::Center),
            (TextAlignKeyword::MozLeft, parley::Alignment::Left),
            (TextAlignKeyword::MozRight, parley::Alignment::Right),
        ];
        for &(keyword, expected) in cases {
            assert_eq!(
                css_text_align_to_parley_alignment(keyword),
                expected,
                "keyword {keyword:?} should map to {expected:?}"
            );
        }
    }

    // --- extract_asset_name ---

    #[test]
    fn extract_asset_name_strips_file_scheme() {
        assert_eq!(extract_asset_name("file:///foo/bar.png"), "foo/bar.png");
        assert_eq!(extract_asset_name("file:///"), "");
    }

    #[test]
    fn extract_asset_name_passthrough_non_file_url() {
        assert_eq!(extract_asset_name("logo.png"), "logo.png");
        assert_eq!(
            extract_asset_name("http://example.com/img.png"),
            "http://example.com/img.png"
        );
    }

    // --- layout_in_pt ---

    #[test]
    fn layout_in_pt_converts_px_pt() {
        // 1 CSS px = 0.75 PDF pt, so 4 px → 3 pt, 100 px → 75 pt.
        let layout = taffy::Layout {
            location: taffy::geometry::Point { x: 4.0, y: 8.0 },
            size: taffy::geometry::Size {
                width: 100.0,
                height: 200.0,
            },
            ..taffy::Layout::new()
        };
        let (x, y, w, h) = layout_in_pt(&layout);
        assert_eq!(x, 3.0_f32.as_pt());
        assert_eq!(y, 6.0_f32.as_pt());
        assert_eq!(w, 75.0_f32.as_pt());
        assert_eq!(h, 150.0_f32.as_pt());
    }

    #[test]
    fn layout_in_pt_zero_layout_stays_zero() {
        let layout = taffy::Layout::new();
        let (x, y, w, h) = layout_in_pt(&layout);
        assert_eq!(x, 0.0_f32.as_pt());
        assert_eq!(y, 0.0_f32.as_pt());
        assert_eq!(w, 0.0_f32.as_pt());
        assert_eq!(h, 0.0_f32.as_pt());
    }

    // --- size_in_pt ---

    #[test]
    fn size_in_pt_converts_px_pt() {
        // 80 px → 60 pt, 120 px → 90 pt
        let size = taffy::geometry::Size {
            width: 80.0,
            height: 120.0,
        };
        let (w, h) = size_in_pt(size);
        assert_eq!(w, 60.0_f32.as_pt());
        assert_eq!(h, 90.0_f32.as_pt());
    }

    #[test]
    fn size_in_pt_zero_size_stays_zero() {
        let size = taffy::geometry::Size {
            width: 0.0,
            height: 0.0,
        };
        let (w, h) = size_in_pt(size);
        assert_eq!(w, 0.0_f32.as_pt());
        assert_eq!(h, 0.0_f32.as_pt());
    }

    // --- content_box_from_geometry ---
    //
    // The `Node` / `BlockStyle` projection lives in `compute_content_box`;
    // these tests exercise the pure arithmetic. TRBL order: [0]=top,
    // [1]=right, [2]=bottom, [3]=left.

    fn tf(v: f32) -> crate::units::Pt {
        v.as_pt()
    }

    #[test]
    fn content_box_from_geometry_zero_insets_matches_border_box() {
        let cb = content_box_from_geometry(
            80.0_f32.as_px(),
            120.0_f32.as_px(),
            &[tf(0.0); 4],
            &[tf(0.0); 4],
        );
        assert_eq!(cb.origin_x, 0.0_f32.as_px());
        assert_eq!(cb.origin_y, 0.0_f32.as_px());
        assert_eq!(cb.width, 80.0_f32.as_px());
        assert_eq!(cb.height, 120.0_f32.as_px());
    }

    #[test]
    fn content_box_from_geometry_respects_trbl_index_mapping() {
        // Distinct border and padding per side so any transposition of
        // TRBL indices produces a different origin / width / height.
        // border TRBL = [1, 2, 3, 4] pt, padding TRBL = [5, 6, 7, 8] pt.
        let cb = content_box_from_geometry(
            80.0_f32.as_px(),
            120.0_f32.as_px(),
            &[tf(1.0), tf(2.0), tf(3.0), tf(4.0)],
            &[tf(5.0), tf(6.0), tf(7.0), tf(8.0)],
        );
        // origin_x = left  = border[3] + padding[3] = (4 + 8) pt → Px.
        assert_eq!(cb.origin_x, tf(4.0).in_px() + tf(8.0).in_px());
        // origin_y = top   = border[0] + padding[0] = (1 + 5) pt → Px.
        assert_eq!(cb.origin_y, tf(1.0).in_px() + tf(5.0).in_px());
        // width  = border_w - left - right  = 80px - (4+8)pt.in_px() - (2+6)pt.in_px()
        let left = tf(4.0).in_px() + tf(8.0).in_px();
        let right = tf(2.0).in_px() + tf(6.0).in_px();
        assert_eq!(cb.width, 80.0_f32.as_px() - left - right);
        // height = border_h - top - bottom = 120px - (1+5)pt.in_px() - (3+7)pt.in_px()
        let top = tf(1.0).in_px() + tf(5.0).in_px();
        let bottom = tf(3.0).in_px() + tf(7.0).in_px();
        assert_eq!(cb.height, 120.0_f32.as_px() - top - bottom);
    }

    #[test]
    fn content_box_from_geometry_folds_per_side_bit_identically() {
        // Guards the docstring's "do not fold" rule: summing then
        // converting must be bit-identical to converting-then-summing at
        // the values we actually feed. `Pt` addition uses raw f32 ops so
        // the equivalence is exact for these small integers, but the
        // test locks in the observable equality so a future
        // "cleanup" that changed the order would either have to update
        // the assertion or fail.
        let border = [tf(1.0), tf(2.0), tf(3.0), tf(4.0)];
        let padding = [tf(5.0), tf(6.0), tf(7.0), tf(8.0)];
        let cb = content_box_from_geometry(80.0_f32.as_px(), 120.0_f32.as_px(), &border, &padding);
        // Recompute each side out-of-line matching the callee's exact
        // per-operand `.in_px()` shape.
        let l = border[3].in_px() + padding[3].in_px();
        let t = border[0].in_px() + padding[0].in_px();
        let r = border[1].in_px() + padding[1].in_px();
        let b = border[2].in_px() + padding[2].in_px();
        assert_eq!(cb.origin_x, l);
        assert_eq!(cb.origin_y, t);
        assert_eq!(
            cb.width,
            (80.0_f32.as_px() - l - r).max(crate::units::Px::ZERO)
        );
        assert_eq!(
            cb.height,
            (120.0_f32.as_px() - t - b).max(crate::units::Px::ZERO),
        );
    }

    #[test]
    fn content_box_from_geometry_clamps_to_zero_when_insets_exceed_box() {
        // border_w = 10 px, insets = 100 pt/side border + 100 pt/side padding on left+right
        // → negative content width, must clamp to 0.
        let cb = content_box_from_geometry(
            10.0_f32.as_px(),
            10.0_f32.as_px(),
            &[tf(100.0); 4],
            &[tf(100.0); 4],
        );
        assert_eq!(cb.width, crate::units::Px::ZERO);
        assert_eq!(cb.height, crate::units::Px::ZERO);
    }
}

#[cfg(test)]
mod dropped_texts_tests {
    use super::*;

    fn d(element: &str, excerpt: &str) -> DroppedText {
        DroppedText {
            element: element.into(),
            excerpt: excerpt.into(),
        }
    }

    #[test]
    fn each_distinct_pair_is_kept_once_in_the_order_found() {
        let mut t = DroppedTexts::default();
        assert!(t.is_empty());
        for (e, x) in [("p", "b"), ("p", "a"), ("p", "b"), ("td", "b"), ("p", "a")] {
            t.push(d(e, x));
        }
        assert_eq!(t.into_vec(), [d("p", "b"), d("p", "a"), d("td", "b")]);
    }

    #[test]
    fn thousands_of_entries_do_not_search_the_list() {
        let mut t = DroppedTexts::default();
        for round in 0..2 {
            for i in 0..50_000 {
                t.push(d("p", &format!("text {i}")));
            }
            assert_eq!(t.list.len(), 50_000, "round {round}");
        }
    }
}

#[cfg(test)]
mod font_bytes_tests {
    use super::FontBytes;
    use std::sync::Arc;

    fn font_data(bytes: &Arc<Vec<u8>>, index: u32) -> parley::FontData {
        parley::FontData::new(parley::fontique::Blob::new(Arc::clone(bytes) as _), index)
    }

    #[test]
    fn a_bundled_blob_resolves_to_the_bundles_own_arc() {
        let a = Arc::new(vec![1_u8; 64]);
        let b = Arc::new(vec![2_u8; 64]);
        let mut bytes = FontBytes::seeded(&[Arc::clone(&a), Arc::clone(&b)]);
        assert!(Arc::ptr_eq(&bytes.get_or_copy(&font_data(&a, 0)), &a));
        assert!(Arc::ptr_eq(&bytes.get_or_copy(&font_data(&b, 0)), &b));
        // The face index does not matter: it is the same buffer.
        assert!(Arc::ptr_eq(&bytes.get_or_copy(&font_data(&a, 3)), &a));
    }

    #[test]
    fn an_unknown_blob_is_copied_once() {
        let bundled = Arc::new(vec![1_u8; 64]);
        let other = Arc::new(vec![7_u8; 32]);
        let mut bytes = FontBytes::seeded(&[bundled]);
        let first = bytes.get_or_copy(&font_data(&other, 0));
        assert!(!Arc::ptr_eq(&first, &other));
        assert_eq!(*first, *other);
        assert!(Arc::ptr_eq(
            &first,
            &bytes.get_or_copy(&font_data(&other, 1))
        ));
    }

    #[test]
    fn an_entry_of_another_length_is_not_reused() {
        // The same address with another length is not the bundled buffer: copy, do not trust the entry.
        struct Prefix(Arc<Vec<u8>>, usize);
        impl AsRef<[u8]> for Prefix {
            fn as_ref(&self) -> &[u8] {
                &self.0[..self.1]
            }
        }
        let a = Arc::new(vec![1_u8; 64]);
        let mut bytes = FontBytes::seeded(&[Arc::clone(&a)]);
        let prefix = parley::FontData::new(
            parley::fontique::Blob::new(Arc::new(Prefix(Arc::clone(&a), 32)) as _),
            0,
        );
        let got = bytes.get_or_copy(&prefix);
        assert_eq!(got.len(), 32);
        assert!(!Arc::ptr_eq(&got, &a));
    }

    #[test]
    fn an_empty_cache_copies_every_blob() {
        let a = Arc::new(vec![1_u8; 16]);
        let mut bytes = FontBytes::default();
        let copy = bytes.get_or_copy(&font_data(&a, 0));
        assert!(!Arc::ptr_eq(&copy, &a));
        assert_eq!(*copy, *a);
    }
}
