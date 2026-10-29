//! F144: the fragmenter's "is there a forced break below" and "is there a page-name change below" questions, answered
//! from two tables built once per pagination pass instead of by a walk of the subtree at every gate.
//!
//! The gates in [`super::fragment_block_subtree`] and `fragment_pagination_root` ask both questions for every in-flow
//! child that has children, once or twice, at every level they enter. Each answer used to walk the child's whole
//! subtree ([`super::has_forced_break_below`], [`super::has_page_name_change_below`]), which for a table of n rows is
//! about 2 questions x 2 evaluations x 3 levels visits of every cell. Both answers depend only on the node and its
//! descendants, so one memoised pass over the DOM gives them all.
//!
//! The two walks stay in `pagination_layout.rs` as the oracle: the gate helpers compare against them under
//! `debug_assert_eq!`, and the tests below compare every slab key of many trees.
//!
//! # The levels
//!
//! `flevel(n)` is the smallest `k >= 1` such that a node `k` levels below `n` declares a forced page break
//! (`break-before: page` / `break-after: page`), or [`NO_LEVEL`]. `has_forced_break_below(n, d)` is true exactly when
//! `flevel(n) <= MAX_DOM_DEPTH - d`: the walk bails at the depth bound, and a break on a child of the node at depth
//! `MAX_DOM_DEPTH - 1` is still seen.
//!
//! `plevel(n)` is the smallest `k >= 0` such that the node `k` levels below `n` has two consecutive in-flow children
//! with different used page-names, following only the nodes the walk follows, or [`NO_LEVEL`]. A node the walk
//! treats as opaque has `NO_LEVEL`. `has_page_name_change_below(n, d)` is true exactly when
//! `plevel(n) < MAX_DOM_DEPTH - d`.
//!
//! Both are minima over the children, so the graph may share nodes. Blitz's anonymous block wrappers do: their
//! `children` are nodes that also belong to the real parent. The table therefore covers every slab key, not only the
//! nodes reachable from the root. (A DOM cycle, which html5ever and Blitz never create, is cut at the edge that closes
//! it; the walks would recurse to the depth bound instead.)

use super::{BaseDocument, page_name_walk_child, page_name_walk_is_opaque};
use crate::blitz_adapter::{self, UsedPageNameTable};
use crate::column_css::{self, ColumnStyleTable};
use crate::draw_primitives::{BreakAfter, BreakBefore};

/// "No such level": the answer to either question is false for this node.
pub(crate) const NO_LEVEL: u16 = u16::MAX;

const NOT_VISITED: u8 = 0;
const ON_STACK: u8 = 1;
const DONE: u8 = 2;

/// The start / end page-names of a node that has no entry in the used page-name table (as the walk reads it).
static UNNAMED: (Option<String>, Option<String>) = (None, None);

/// Per slab key: how far below the node the nearest forced break and the nearest page-name change are. See the module
/// docs. A table is `None` when its question is false everywhere (no forced break in `column_styles`, no used
/// page-name table), so a document without either costs nothing.
pub(crate) struct SubtreeFacts {
    forced_level: Option<Vec<u16>>,
    page_name_level: Option<Vec<u16>>,
}

fn level_of(levels: &Option<Vec<u16>>, node_id: usize) -> u16 {
    levels
        .as_ref()
        .and_then(|v| v.get(node_id).copied())
        .unwrap_or(NO_LEVEL)
}

impl SubtreeFacts {
    /// Builds the tables for `doc` from the two side tables the fragmenter reads. Never reads layout, and the
    /// fragmenter does not change the DOM or the styles, so the result is valid for the whole pass.
    pub(crate) fn build(
        doc: &BaseDocument,
        column_styles: Option<&ColumnStyleTable>,
        used_page_names: Option<&UsedPageNameTable>,
    ) -> Self {
        let forced_styles = column_styles.filter(|cs| column_css::has_forced_page_break(cs));
        if forced_styles.is_none() && used_page_names.is_none() {
            return Self {
                forced_level: None,
                page_name_level: None,
            };
        }
        let len = doc.tree().iter().map(|(k, _)| k + 1).max().unwrap_or(0);
        let mut forced = forced_styles.map(|_| vec![NO_LEVEL; len]);
        let mut names = used_page_names.map(|_| vec![NO_LEVEL; len]);
        let mut state = vec![NOT_VISITED; len];
        // One iterative post-order walk over `children`, started from every slab key in key order. A node is
        // finished after all its children, so their levels are final (an edge to a node still on the stack can only
        // close a cycle and counts as no level).
        let mut stack: Vec<(usize, usize)> = Vec::new();
        for (root, _) in doc.tree().iter() {
            if state[root] != NOT_VISITED {
                continue;
            }
            state[root] = ON_STACK;
            stack.push((root, 0));
            while let Some(&mut (node_id, ref mut next)) = stack.last_mut() {
                let node = &doc.tree()[node_id];
                if let Some(&child_id) = node.children.get(*next) {
                    *next += 1;
                    if child_id < len
                        && state[child_id] == NOT_VISITED
                        && doc.get_node(child_id).is_some()
                    {
                        state[child_id] = ON_STACK;
                        stack.push((child_id, 0));
                    }
                    continue;
                }
                if let (Some(levels), Some(cs)) = (forced.as_mut(), forced_styles) {
                    levels[node_id] = forced_level_of(node.children.as_slice(), cs, levels);
                }
                if let (Some(levels), Some(table)) = (names.as_mut(), used_page_names) {
                    levels[node_id] = page_name_level_of(doc, node_id, table, levels);
                }
                state[node_id] = DONE;
                stack.pop();
            }
        }
        Self {
            forced_level: forced,
            page_name_level: names,
        }
    }

    /// `has_forced_break_below(doc, node_id, column_styles, 0)`.
    pub(crate) fn forced_break_below(&self, node_id: usize) -> bool {
        usize::from(level_of(&self.forced_level, node_id)) <= crate::MAX_DOM_DEPTH
    }

    /// `has_page_name_change_below(doc, node_id, used_page_names, 0)`.
    pub(crate) fn page_name_change_below(&self, node_id: usize) -> bool {
        usize::from(level_of(&self.page_name_level, node_id)) < crate::MAX_DOM_DEPTH
    }
}

/// `flevel` of a node with these children: 1 if a child declares a forced break, else one more than the smallest
/// level of a child.
fn forced_level_of(children: &[usize], column_styles: &ColumnStyleTable, levels: &[u16]) -> u16 {
    let mut best = NO_LEVEL;
    for &child_id in children {
        if let Some(props) = column_styles.get(&child_id)
            && (matches!(props.break_before, Some(BreakBefore::Page))
                || matches!(props.break_after, Some(BreakAfter::Page)))
        {
            return 1;
        }
        let below = levels.get(child_id).copied().unwrap_or(NO_LEVEL);
        best = best.min(below.saturating_add(1));
    }
    best
}

/// `plevel` of a node: the same predicates as `has_page_name_change_below` (they are the same functions).
fn page_name_level_of(
    doc: &BaseDocument,
    node_id: usize,
    table: &UsedPageNameTable,
    levels: &[u16],
) -> u16 {
    let Some(node) = doc.get_node(node_id) else {
        return NO_LEVEL;
    };
    if page_name_walk_is_opaque(doc, node) {
        return NO_LEVEL;
    }
    let suppress_direct_compare = blitz_adapter::is_flex_or_grid_container_node(node);
    let mut prev_end: Option<&Option<String>> = None;
    let mut best = NO_LEVEL;
    for &child_id in &node.children {
        if page_name_walk_child(doc, node, child_id).is_none() {
            continue;
        }
        let (child_start, child_end) = table.get(&child_id).unwrap_or(&UNNAMED);
        if !suppress_direct_compare {
            if prev_end.is_some_and(|p| p != child_start) {
                return 0;
            }
            prev_end = Some(child_end);
        }
        let below = levels.get(child_id).copied().unwrap_or(NO_LEVEL);
        best = best.min(below.saturating_add(1));
    }
    best
}

#[cfg(test)]
mod tests {
    use super::super::{has_forced_break_below, has_page_name_change_below};
    use super::*;
    use crate::blitz_adapter::{self, UsedPageNameTable};
    use crate::column_css::{ColumnStyleProps, ColumnStyleTable};
    use crate::draw_primitives::{BreakAfter, BreakBefore};
    use blitz_dom::BaseDocument;
    use std::sync::Arc;

    fn parse(html: &str) -> blitz_html::HtmlDocument {
        let fonts: Vec<Arc<Vec<u8>>> = Vec::new();
        let mut doc = blitz_adapter::parse(html, 600.0, &fonts);
        blitz_adapter::resolve(&mut doc);
        doc
    }

    fn slab_len(doc: &BaseDocument) -> usize {
        doc.tree().iter().map(|(k, _)| k + 1).max().unwrap_or(0)
    }

    /// The node ids whose `id` attribute is `name`, in slab order.
    fn id_of(doc: &BaseDocument, name: &str) -> usize {
        doc.tree()
            .iter()
            .find(|(_, n)| {
                n.element_data().is_some_and(|e| {
                    e.attrs()
                        .iter()
                        .any(|a| a.name.local.as_ref() == "id" && a.value.as_str() == name)
                })
            })
            .map(|(k, _)| k)
            .unwrap_or_else(|| panic!("no element with id {name}"))
    }

    /// For every slab key (and three past the end), the table answers what the old walk answers. Returns the keys for
    /// which the forced-break and the page-name question is true.
    fn compare(
        doc: &BaseDocument,
        cs: Option<&ColumnStyleTable>,
        used: Option<&UsedPageNameTable>,
    ) -> (Vec<usize>, Vec<usize>) {
        let facts = SubtreeFacts::build(doc, cs, used);
        let (mut forced, mut names) = (Vec::new(), Vec::new());
        for id in 0..slab_len(doc) + 3 {
            let f = has_forced_break_below(doc, id, cs, 0);
            let p = has_page_name_change_below(doc, id, used, 0);
            assert_eq!(
                facts.forced_break_below(id),
                f,
                "forced break below, node {id}"
            );
            assert_eq!(
                facts.page_name_change_below(id),
                p,
                "name change below, node {id}"
            );
            if f {
                forced.push(id);
            }
            if p {
                names.push(id);
            }
        }
        (forced, names)
    }

    /// The document of `html` with the tables the pipeline builds; all four present / absent combinations are
    /// compared. Returns the lists of the full one.
    fn check(html: &str) -> (blitz_html::HtmlDocument, Vec<usize>, Vec<usize>) {
        let doc = parse(html);
        let cs = blitz_adapter::extract_column_style_table(&doc);
        let used = blitz_adapter::compute_used_page_names(&doc, &cs);
        let base: &BaseDocument = &doc;
        let (f, p) = compare(base, Some(&cs), Some(&used));
        let (_, p2) = compare(base, Some(&cs), None);
        assert!(p2.is_empty(), "no used page-name table: no name change");
        let (f3, p3) = compare(base, None, Some(&used));
        assert!(f3.is_empty(), "no column styles: no forced break");
        assert_eq!(p3, p, "the name questions do not read the column styles");
        let (f4, p4) = compare(base, None, None);
        assert!(f4.is_empty() && p4.is_empty());
        (doc, f, p)
    }

    fn nested(levels: usize, inner: &str) -> String {
        format!(
            "{}{inner}{}",
            "<div>".repeat(levels),
            "</div>".repeat(levels)
        )
    }

    #[test]
    fn forced_break_one_three_and_ten_levels_deep_in_a_table_cell() {
        for levels in [0, 2, 9] {
            let html = format!(
                "<html><body><table id=t><tbody><tr id=r><td id=c>{}</td><td>b</td></tr></tbody></table>\
                 <p id=after>x</p></body></html>",
                nested(levels, "<div id=b style=\"break-before:page\">x</div>")
            );
            let (doc, forced, names) = check(&html);
            for name in ["t", "r", "c"] {
                assert!(
                    forced.contains(&id_of(&doc, name)),
                    "{name} at {} levels",
                    levels + 1
                );
            }
            assert!(!forced.contains(&id_of(&doc, "after")));
            assert!(
                !forced.contains(&id_of(&doc, "b")),
                "a break is below its parent, not below itself"
            );
            assert!(names.is_empty());
        }
        let (_, forced, names) = check(
            "<html><body><table><tbody><tr><td><div>x</div></td></tr></tbody></table></body></html>",
        );
        assert!(forced.is_empty() && names.is_empty());
    }

    #[test]
    fn break_after_on_the_last_child() {
        let (doc, forced, _) = check(
            "<html><body><div id=a><p>1</p><p id=last style=\"break-after:page\">2</p></div><div id=b><p>3</p></div></body></html>",
        );
        assert!(forced.contains(&id_of(&doc, "a")));
        assert!(!forced.contains(&id_of(&doc, "b")));
        assert!(!forced.contains(&id_of(&doc, "last")));
    }

    #[test]
    fn forced_break_inside_multicol_with_and_without_column_span_all() {
        let (doc, forced, _) = check(
            "<html><body><div id=mc style=\"columns:2\"><p>a</p><p style=\"break-before:page\">b</p></div>\
             <div id=span style=\"columns:2\"><p>a</p><h2 style=\"column-span:all;break-after:page\">t</h2><p>c</p></div>\
             <div id=plain style=\"columns:2\"><p>a</p><h2 style=\"column-span:all\">t</h2></div></body></html>",
        );
        assert!(forced.contains(&id_of(&doc, "mc")));
        assert!(forced.contains(&id_of(&doc, "span")));
        assert!(!forced.contains(&id_of(&doc, "plain")));
    }

    #[test]
    fn out_of_flow_descendants_count_for_breaks_but_not_for_names() {
        for style in ["position:absolute", "position:fixed", "float:left"] {
            let html = format!(
                "<html><body><div id=p style=\"page:a\"><div style=\"page:a\">1</div>\
                 <div style=\"{style};break-before:page;page:b\">out</div><div style=\"page:a\">2</div></div>\
                 <div id=q><div style=\"page:a\">1</div><div style=\"{style};page:b\">out</div></div></body></html>"
            );
            let (doc, forced, names) = check(&html);
            assert!(
                forced.contains(&id_of(&doc, "p")),
                "{style}: the forced walk counts it"
            );
            assert!(!forced.contains(&id_of(&doc, "q")));
            assert!(
                !names.contains(&id_of(&doc, "p")),
                "{style}: not a sibling in flow"
            );
            assert!(!names.contains(&id_of(&doc, "q")), "{style}");
        }
    }

    #[test]
    fn display_contents_is_walked_like_any_other_node() {
        let (doc, _, names) = check(
            "<html><body><div id=w><div id=ct style=\"display:contents\"><div style=\"page:a\">1</div>\
             <div style=\"page:b\">2</div></div></div>\
             <div id=ok><div style=\"display:contents\"><div style=\"page:a\">1</div><div style=\"page:a\">2</div></div></div>\
             <div id=sib><div style=\"display:contents\"><div style=\"page:a\">1</div></div><div style=\"page:b\">2</div></div></body></html>",
        );
        assert!(names.contains(&id_of(&doc, "w")));
        assert!(names.contains(&id_of(&doc, "ct")));
        assert!(!names.contains(&id_of(&doc, "ok")));
        assert!(names.contains(&id_of(&doc, "sib")));
    }

    #[test]
    fn atomic_inlines_are_opaque_for_names_only() {
        for display in ["inline-block", "inline-flex", "inline-grid"] {
            let html = format!(
                "<html><body><div id=p><span id=ib style=\"display:{display}\"><div style=\"page:a\">1</div>\
                 <div style=\"page:b;break-before:page\">2</div></span></div></body></html>"
            );
            let (doc, forced, names) = check(&html);
            assert!(forced.contains(&id_of(&doc, "p")), "{display}");
            assert!(forced.contains(&id_of(&doc, "ib")), "{display}");
            assert!(!names.contains(&id_of(&doc, "p")), "{display}");
            assert!(
                !names.contains(&id_of(&doc, "ib")),
                "{display}: the queried node itself"
            );
        }
    }

    #[test]
    fn an_orthogonal_child_and_an_orthogonal_queried_node_are_opaque() {
        let (doc, _, names) = check(
            "<html><body><div id=p><div id=v style=\"writing-mode:vertical-rl\"><div style=\"page:a\">1</div>\
             <div style=\"page:b\">2</div></div></div>\
             <div id=same style=\"writing-mode:vertical-rl\"><div style=\"writing-mode:vertical-lr\">\
             <div style=\"page:a\">1</div><div style=\"page:b\">2</div></div></div></body></html>",
        );
        let base: &BaseDocument = &doc;
        let node = |name: &str| base.get_node(id_of(base, name)).unwrap();
        if blitz_adapter::is_orthogonal_to_parent(node("p"), node("v")) {
            assert!(
                !names.contains(&id_of(&doc, "p")),
                "the child is atomic for its parent"
            );
            assert!(
                !names.contains(&id_of(&doc, "v")),
                "queried directly, it is opaque as well"
            );
            assert!(
                names.contains(&id_of(&doc, "same")),
                "vertical-lr in vertical-rl is not orthogonal"
            );
        } else {
            // Stylo drops a vertical `writing-mode` unless the pref `layout.writing-mode.enabled` is set. Blitz does
            // not set it and fulgur has no way to (no `style_config` dependency), so here every node is horizontal and
            // the branch above is out of reach; the table and the walk still have to agree (`check`).
            for name in ["p", "v", "same"] {
                assert!(
                    names.contains(&id_of(&doc, name)),
                    "{name}: an ordinary block flow"
                );
            }
        }
    }

    #[test]
    fn flex_and_grid_items_are_not_compared_but_their_insides_are() {
        let (doc, _, names) = check(
            "<html><body><div id=f style=\"display:flex\"><div style=\"page:a\">1</div><div style=\"page:b\">2</div></div>\
             <div id=g style=\"display:grid\"><div style=\"page:a\">1</div><div style=\"page:b\">2</div></div>\
             <div id=fi style=\"display:flex\"><div><div style=\"page:a\">1</div><div style=\"page:b\">2</div></div></div>\
             <div id=gi style=\"display:grid\"><div><div style=\"page:a\">1</div><div style=\"page:b\">2</div></div></div>\
             </body></html>",
        );
        assert!(!names.contains(&id_of(&doc, "f")));
        assert!(!names.contains(&id_of(&doc, "g")));
        assert!(names.contains(&id_of(&doc, "fi")));
        assert!(names.contains(&id_of(&doc, "gi")));
    }

    #[test]
    fn anonymous_wrappers_and_mixed_inline_and_block_children() {
        let (doc, forced, names) = check(
            "<html><body><div id=m>text <b>bold</b><div style=\"page:a\">1</div>more <i style=\"display:inline-block\"><div style=\"break-before:page\">text</div></i>\
             <div style=\"page:b;break-after:page\">2</div>tail</div></body></html>",
        );
        let base: &BaseDocument = &doc;
        assert!(
            base.tree().iter().any(|(_, n)| n.is_anonymous()),
            "the mixed content makes anonymous block wrappers"
        );
        assert!(forced.contains(&id_of(&doc, "m")));
        assert!(names.contains(&id_of(&doc, "m")));
        // The wrapper that holds "more <i>" is reachable only through `layout_children`, and the gate may ask about it.
        // (The break sits in a block inside an inline-block: `break-before` does not act on an inline-level box.)
        assert!(
            base.tree()
                .iter()
                .any(|(k, n)| n.is_anonymous() && forced.contains(&k)),
            "an anonymous wrapper with a forced break below is answered"
        );
    }

    #[test]
    fn whitespace_between_named_siblings_is_skipped() {
        let (doc, _, names) = check(
            "<html><body><div id=same>\n  <div style=\"page:a\">1</div>\n  \n  <div style=\"page:a\">2</div>\n</div>\
             <div id=diff>\n  <div style=\"page:a\">1</div>\n  <div style=\"page:b\">2</div>\n</div>\
             <div id=txt><div style=\"page:a\">1</div> words <div style=\"page:a\">2</div></div></body></html>",
        );
        assert!(!names.contains(&id_of(&doc, "same")));
        assert!(names.contains(&id_of(&doc, "diff")));
        assert!(!names.contains(&id_of(&doc, "txt")));
    }

    /// The depth bound: a break on the node `MAX_DOM_DEPTH` levels below the queried one is found, one more level is not.
    #[test]
    fn the_depth_bound_is_the_same_at_511_512_and_513_levels() {
        let h = std::thread::Builder::new()
            .stack_size(512 << 20)
            .spawn(|| {
                let max = crate::MAX_DOM_DEPTH;
                for (k, forced_top, names_top) in [
                    (max - 1, true, true),
                    (max, true, false),
                    (max + 1, false, false),
                ] {
                    let (f, n, _, _) = chain_case_two_children(k);
                    assert_eq!(f, forced_top, "forced break {k} levels below");
                    assert_eq!(
                        n, names_top,
                        "name change among the children of the node {k} levels below"
                    );
                }
            })
            .unwrap();
        h.join().unwrap();
    }

    /// A chain of nested divs `c0 > c1 > ...`, with a forced break on `c{k}` (`k` levels below the queried top one)
    /// and a name change between the two children of `c{k}`. The tables are built by hand: the readers that fill them
    /// stop at the same depth bound as the questions, which would hide the boundary.
    fn chain_case_two_children(k: usize) -> (bool, bool, bool, bool) {
        let mut html = String::from("<html><body>");
        for i in 0..=k {
            html.push_str(&format!("<div id=c{i}>"));
        }
        html.push_str("<div id=c");
        html.push_str(&(k + 1).to_string());
        html.push_str("></div><div id=sib></div>");
        for _ in 0..=k {
            html.push_str("</div>");
        }
        html.push_str("</body></html>");
        let doc = parse(&html);
        let base: &BaseDocument = &doc;
        let top = id_of(base, "c0");
        let at_k = id_of(base, &format!("c{k}"));
        let next = id_of(base, &format!("c{}", k + 1));
        let sibling = id_of(base, "sib");

        let mut cs = ColumnStyleTable::new();
        cs.insert(
            at_k,
            ColumnStyleProps {
                break_before: Some(BreakBefore::Page),
                ..Default::default()
            },
        );
        let mut used = UsedPageNameTable::new();
        used.insert(next, (Some("a".into()), Some("a".into())));
        used.insert(sibling, (Some("b".into()), Some("b".into())));
        let (forced, names) = compare(base, Some(&cs), Some(&used));
        (
            forced.contains(&top),
            names.contains(&top),
            forced.contains(&at_k),
            names.contains(&at_k),
        )
    }

    /// A fixed LCG, so the trees are the same on every host.
    struct Lcg(u64);
    impl Lcg {
        fn next(&mut self, n: usize) -> usize {
            self.0 = self
                .0
                .wrapping_mul(6364136223846793005)
                .wrapping_add(1442695040888963407);
            ((self.0 >> 33) as usize) % n
        }
    }

    const RANDOM_STYLES: &[&str] = &[
        "",
        "",
        "",
        "display:flex",
        "display:grid",
        "display:contents",
        "display:inline-block",
        "display:inline-flex",
        "display:inline",
        "display:table",
        "position:absolute",
        "position:fixed",
        "float:left",
        "writing-mode:vertical-rl",
        "writing-mode:vertical-lr",
        "break-before:page",
        "break-after:page",
        "page:a",
        "page:b",
        "page:a;break-before:page",
        "page:b;display:flex",
    ];

    fn random_tree(rng: &mut Lcg, out: &mut String, depth: usize, budget: &mut usize) {
        let n_children = if depth >= 5 { 0 } else { rng.next(4) };
        for _ in 0..n_children {
            if *budget == 0 {
                return;
            }
            *budget -= 1;
            match rng.next(8) {
                0 => out.push_str(" text "),
                1 => out.push_str("\n  "),
                _ => {
                    let tag = ["div", "span", "p", "section"][rng.next(4)];
                    let style = RANDOM_STYLES[rng.next(RANDOM_STYLES.len())];
                    out.push_str(&format!("<{tag} style=\"{style}\">"));
                    random_tree(rng, out, depth + 1, budget);
                    out.push_str(&format!("</{tag}>"));
                }
            }
        }
    }

    #[test]
    fn two_hundred_seeded_random_trees_agree_with_the_walks() {
        let mut rng = Lcg(0x5eed_f144);
        let (mut forced_total, mut names_total) = (0, 0);
        for case in 0..200 {
            let mut body = String::new();
            let mut budget = 24;
            random_tree(&mut rng, &mut body, 0, &mut budget);
            let html = format!("<html><body>{body}</body></html>");
            let (doc, forced, names) = check(&html);
            forced_total += forced.len();
            names_total += names.len();

            // The same tree with tables filled at random, so that every branch of the questions is reached.
            let base: &BaseDocument = &doc;
            let mut cs = ColumnStyleTable::new();
            let mut used = UsedPageNameTable::new();
            let name = |r: usize| match r {
                0 => Some("a".to_string()),
                1 => Some("b".to_string()),
                _ => None,
            };
            for (id, _) in base.tree().iter() {
                if rng.next(3) == 0 {
                    cs.insert(
                        id,
                        ColumnStyleProps {
                            break_before: (rng.next(4) == 0).then_some(BreakBefore::Page),
                            break_after: (rng.next(4) == 0).then_some(BreakAfter::Page),
                            ..Default::default()
                        },
                    );
                }
                if rng.next(3) != 0 {
                    used.insert(id, (name(rng.next(3)), name(rng.next(3))));
                }
            }
            compare(base, Some(&cs), Some(&used));
            let _ = case;
        }
        assert!(
            forced_total > 50 && names_total > 50,
            "the trees reach the true branches: {forced_total} / {names_total}"
        );
    }
}
