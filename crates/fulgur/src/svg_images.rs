//! The raster images (`<image href="data:image/png;base64,…">`) inside an SVG are checked before Krilla sees them
//! (html-to-pdf F177).
//!
//! usvg turns an `<image>` href into an [`usvg::ImageKind`] with a resolver that looks at the MIME type or the
//! magic bytes and never decodes; `krilla-svg` then hands the bytes to Krilla, which reads the header when the
//! image is created and decodes the pixels in `Document::finish()`. A truncated PNG fails the whole document and
//! a header that claims 100000 x 100000 pixels aborts the process. [`SvgImageChecks`] wraps usvg's default
//! resolvers: a raster that [`crate::image::validate_svg_raster`] refuses is not returned, so usvg skips the
//! `<image>` as it skips an href that does not resolve; one that passes is returned as it is, with the same bytes,
//! so the tree and the PDF do not change.
//!
//! One [`SvgImageChecks`] serves one layout pass of a render. Every place fulgur parses an SVG that can hold images
//! gets its options from [`SvgImageChecks::options`] (the SVG assets of `<img>`, `background-image` and
//! `list-style-image`) or its factory ([`SvgImageChecks::factory`], the inline `<svg>` elements Blitz parses).

use std::collections::{BTreeMap, BTreeSet};
use std::sync::{Arc, Mutex, MutexGuard};

/// A raster image inside an SVG that is left out of the SVG, and why.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord)]
pub struct DroppedSvgImage {
    /// The SVG the image is in: `<svg id="logo">`, `<svg> no. 2` (the second inline `<svg>` of the document),
    /// `<img src="logo.svg">`, `background-image: url(logo.svg)` or `list-style-image: url(logo.svg)`.
    pub svg: String,
    /// The image: `data:image/png (4096 bytes)` for a `data:` URL, the href otherwise.
    pub image: String,
    /// Why: `unexpected end of file`, `100000 x 100000 pixels is more than the 134217728 pixel limit`, …
    pub reason: String,
}

impl std::fmt::Display for DroppedSvgImage {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}: {}: {}", self.svg, self.image, self.reason)
    }
}

/// Where an SVG comes from. The order is the order of the report: inline SVGs by node id (document order), then
/// by the name of the asset.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord)]
pub enum SvgSite {
    /// An inline `<svg>`, by the id of its Blitz node.
    Inline(usize),
    /// The asset `src` of an `<img>`.
    Img(String),
    /// The asset of a `background-image: url(…)`.
    Background(String),
    /// The asset of a `list-style-image: url(…)`.
    ListMarker(String),
}

/// The images of one render's SVGs that were left out, and the JPEGs that were embedded although damaged, each in
/// document order and independent of the thread that parsed the SVG.
#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub struct SvgImageReport {
    pub dropped: Vec<DroppedSvgImage>,
    /// `reason` is what is wrong with the JPEG (F32a M1: Krilla passes it through and viewers draw what is there).
    pub damaged: Vec<DroppedSvgImage>,
}

#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord)]
struct Finding {
    site: SvgSite,
    image: String,
    reason: String,
}

#[derive(Clone)]
enum Verdict {
    /// Can be drawn; `Some` for a JPEG that is damaged.
    Valid(Option<String>),
    Refused(String),
}

#[derive(Default)]
struct State {
    /// By the bytes: usvg decodes a `data:` URL into a fresh buffer every time it parses the SVG.
    verdicts: BTreeMap<Arc<Vec<u8>>, Verdict>,
    dropped: BTreeSet<Finding>,
    damaged: BTreeSet<Finding>,
}

/// See the module documentation.
pub struct SvgImageChecks {
    state: Mutex<State>,
    /// Log a finding when it is first made instead of keeping it for [`SvgImageChecks::take`]: for the documents of
    /// the `@page` margin boxes, which are converted after the engine has reported.
    immediate: bool,
}

impl SvgImageChecks {
    pub fn new() -> Arc<Self> {
        Arc::new(Self {
            state: Mutex::default(),
            immediate: false,
        })
    }

    /// Checks that `log::warn!` each finding as it is made (once per image) and are not read with
    /// [`SvgImageChecks::take`].
    pub(crate) fn warning_immediately() -> Arc<Self> {
        Arc::new(Self {
            state: Mutex::default(),
            immediate: true,
        })
    }

    fn state(&self) -> MutexGuard<'_, State> {
        self.state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }

    /// usvg's default resolvers, refusing the rasters of `site` that cannot be drawn.
    pub fn resolver(self: &Arc<Self>, site: SvgSite) -> usvg::ImageHrefResolver<'static> {
        let default_data = usvg::ImageHrefResolver::default_data_resolver();
        let default_string = usvg::ImageHrefResolver::default_string_resolver();
        let (checks, data_site) = (Arc::clone(self), site.clone());
        let (string_checks, string_site) = (Arc::clone(self), site);
        usvg::ImageHrefResolver {
            resolve_data: Box::new(move |mime, data, opts| {
                let kind = default_data(mime, data, opts)?;
                let name = data_name(&kind);
                checks.check(&data_site, name, kind)
            }),
            resolve_string: Box::new(move |href, opts| {
                let kind = default_string(href, opts)?;
                string_checks.check(&string_site, href.to_string(), kind)
            }),
        }
    }

    /// `usvg::Options::default()` with [`SvgImageChecks::resolver`].
    pub fn options(self: &Arc<Self>, site: SvgSite) -> usvg::Options<'static> {
        usvg::Options {
            image_href_resolver: self.resolver(site),
            ..usvg::Options::default()
        }
    }

    /// The factory Blitz calls with the node id of each inline `<svg>` it parses.
    pub fn factory(self: &Arc<Self>) -> blitz_dom::SvgImageResolverFactory {
        let checks = Arc::clone(self);
        Arc::new(move |node_id| checks.resolver(SvgSite::Inline(node_id)))
    }

    /// `kind` if it can be drawn, `None` (and a finding) if it cannot.
    fn check(
        &self,
        site: &SvgSite,
        name: String,
        kind: usvg::ImageKind,
    ) -> Option<usvg::ImageKind> {
        let bytes = match &kind {
            usvg::ImageKind::PNG(d)
            | usvg::ImageKind::JPEG(d)
            | usvg::ImageKind::GIF(d)
            | usvg::ImageKind::WEBP(d) => d,
            // An SVG inside the SVG was parsed with no images of its own.
            usvg::ImageKind::SVG(_) => return Some(kind),
        };
        let mut state = self.state();
        let verdict = match state.verdicts.get(bytes) {
            Some(v) => v.clone(),
            None => {
                let v = match crate::image::validate_svg_raster(&kind) {
                    Ok(damaged) => Verdict::Valid(damaged),
                    Err(e) => Verdict::Refused(e.to_string()),
                };
                state.verdicts.insert(Arc::clone(bytes), v.clone());
                v
            }
        };
        let (set_damaged, reason) = match verdict {
            Verdict::Valid(None) => return Some(kind),
            Verdict::Valid(Some(why)) => (true, why),
            Verdict::Refused(why) => (false, why),
        };
        let finding = Finding {
            site: site.clone(),
            image: name,
            reason,
        };
        let new = if set_damaged {
            state.damaged.insert(finding.clone())
        } else {
            state.dropped.insert(finding.clone())
        };
        drop(state);
        if self.immediate && new {
            let what = describe(&finding, None);
            if set_damaged {
                log::warn!(
                    "{what} is embedded although it is damaged: {}",
                    finding.reason
                );
            } else {
                log::warn!("{what} is not drawn: {}", finding.reason);
            }
        }
        (set_damaged).then_some(kind)
    }

    /// The findings so far, named after `doc` (the document the inline SVGs are in), and forgets them. The verdicts
    /// are kept.
    pub fn take(&self, doc: &blitz_dom::BaseDocument) -> SvgImageReport {
        let (dropped, damaged) = {
            let mut state = self.state();
            (
                std::mem::take(&mut state.dropped),
                std::mem::take(&mut state.damaged),
            )
        };
        let names = InlineNames::new(doc, &dropped, &damaged);
        let name = |f: Finding| DroppedSvgImage {
            svg: site_name(&f.site, Some(&names)),
            image: f.image,
            reason: f.reason,
        };
        SvgImageReport {
            dropped: dropped.into_iter().map(name).collect(),
            damaged: damaged.into_iter().map(name).collect(),
        }
    }
}

/// `data:image/png (4096 bytes)`.
fn data_name(kind: &usvg::ImageKind) -> String {
    let (mime, bytes) = match kind {
        usvg::ImageKind::PNG(d) => ("png", d.len()),
        usvg::ImageKind::JPEG(d) => ("jpeg", d.len()),
        usvg::ImageKind::GIF(d) => ("gif", d.len()),
        usvg::ImageKind::WEBP(d) => ("webp", d.len()),
        usvg::ImageKind::SVG(_) => ("svg+xml", 0),
    };
    format!("data:image/{mime} ({bytes} bytes)")
}

/// The names of the inline SVGs that have findings: `<svg id="…">` or `<svg> no. k`.
struct InlineNames(BTreeMap<usize, String>);

impl InlineNames {
    fn new(
        doc: &blitz_dom::BaseDocument,
        dropped: &BTreeSet<Finding>,
        damaged: &BTreeSet<Finding>,
    ) -> Self {
        let wanted: BTreeSet<usize> = dropped
            .iter()
            .chain(damaged)
            .filter_map(|f| match f.site {
                SvgSite::Inline(id) => Some(id),
                _ => None,
            })
            .collect();
        let mut names = BTreeMap::new();
        if wanted.is_empty() {
            return Self(names);
        }
        // Blitz builds the tree of an `<svg>` that no other `<svg>` contains: those are the ones counted.
        let is_svg = |id: usize| {
            doc.get_node(id)
                .and_then(|n| n.element_data())
                .is_some_and(|e| e.name.local.as_ref() == "svg")
        };
        let mut k = 0;
        for (id, node) in doc.tree().iter() {
            if !is_svg(id) {
                continue;
            }
            let mut ancestor = node.parent;
            let mut nested = false;
            while let Some(a) = ancestor {
                if is_svg(a) {
                    nested = true;
                    break;
                }
                ancestor = doc.get_node(a).and_then(|n| n.parent);
            }
            if nested {
                continue;
            }
            k += 1;
            if wanted.contains(&id) {
                let svg_id = node
                    .element_data()
                    .and_then(|e| e.id.as_ref())
                    .map(|s| s.to_string())
                    .filter(|s| !s.is_empty());
                names.insert(
                    id,
                    match svg_id {
                        Some(s) => format!("<svg id=\"{s}\">"),
                        None => format!("<svg> no. {k}"),
                    },
                );
            }
        }
        Self(names)
    }
}

fn site_name(site: &SvgSite, names: Option<&InlineNames>) -> String {
    match site {
        SvgSite::Inline(id) => match names.and_then(|n| n.0.get(id)) {
            Some(name) => name.clone(),
            None => format!("inline <svg> (node {id}) in a page margin box"),
        },
        SvgSite::Img(src) => format!("<img src=\"{src}\">"),
        SvgSite::Background(src) => format!("background-image: url({src})"),
        SvgSite::ListMarker(src) => format!("list-style-image: url({src})"),
    }
}

/// `svg: image`, for a log line.
fn describe(f: &Finding, names: Option<&InlineNames>) -> String {
    format!("{}: {}", site_name(&f.site, names), f.image)
}

#[cfg(test)]
mod tests {
    use super::*;

    const GIF: &[u8] = include_bytes!("../tests/fixtures/images/gradient32.gif");

    fn gif_url(bytes: &[u8]) -> String {
        data_url("image/gif", bytes)
    }

    fn data_url(mime: &str, bytes: &[u8]) -> String {
        use std::fmt::Write;
        // No base64 crate here: `data:` URLs may be percent-encoded.
        let mut url = format!("data:{mime},");
        for b in bytes {
            write!(url, "%{b:02X}").unwrap();
        }
        url
    }

    fn svg_with(href: &str) -> String {
        format!(
            r#"<svg xmlns="http://www.w3.org/2000/svg" width="20" height="20"><image href="{href}" width="20" height="20"/></svg>"#
        )
    }

    fn images(tree: &usvg::Tree) -> usize {
        fn walk(g: &usvg::Group) -> usize {
            g.children()
                .iter()
                .map(|n| match n {
                    usvg::Node::Image(_) => 1,
                    usvg::Node::Group(g) => walk(g),
                    _ => 0,
                })
                .sum()
        }
        walk(tree.root())
    }

    fn parse(checks: &Arc<SvgImageChecks>, site: SvgSite, svg: &str) -> usize {
        images(&usvg::Tree::from_str(svg, &checks.options(site)).unwrap())
    }

    #[test]
    fn a_valid_raster_stays_and_a_truncated_one_is_dropped() {
        let checks = SvgImageChecks::new();
        let good = svg_with(&gif_url(GIF));
        let bad = svg_with(&gif_url(&GIF[..GIF.len() / 2]));
        assert_eq!(parse(&checks, SvgSite::Img("a.svg".into()), &good), 1);
        assert_eq!(parse(&checks, SvgSite::Img("b.svg".into()), &bad), 0);
        assert_eq!(parse(&checks, SvgSite::Img("b.svg".into()), &bad), 0);
        let doc = blitz_dom::BaseDocument::new(blitz_dom::DocumentConfig::default());
        let report = checks.take(&doc);
        assert!(report.damaged.is_empty());
        // Twice parsed, once reported.
        assert_eq!(report.dropped.len(), 1, "{report:?}");
        let d = &report.dropped[0];
        assert_eq!(d.svg, "<img src=\"b.svg\">");
        assert_eq!(d.image, format!("data:image/gif ({} bytes)", GIF.len() / 2));
        assert!(!d.reason.is_empty());
        assert_eq!(checks.take(&doc), SvgImageReport::default());
    }

    #[test]
    fn findings_come_out_sorted_by_site_whatever_the_order_of_parsing() {
        let checks = SvgImageChecks::new();
        let bad = svg_with(&gif_url(&GIF[..10]));
        for site in [
            SvgSite::ListMarker("l.svg".into()),
            SvgSite::Img("z.svg".into()),
            SvgSite::Background("b.svg".into()),
            SvgSite::Inline(7),
            SvgSite::Img("a.svg".into()),
        ] {
            parse(&checks, site, &bad);
        }
        let doc = blitz_dom::BaseDocument::new(blitz_dom::DocumentConfig::default());
        let svgs: Vec<_> = checks
            .take(&doc)
            .dropped
            .into_iter()
            .map(|d| d.svg)
            .collect();
        assert_eq!(
            svgs,
            [
                "inline <svg> (node 7) in a page margin box",
                "<img src=\"a.svg\">",
                "<img src=\"z.svg\">",
                "background-image: url(b.svg)",
                "list-style-image: url(l.svg)",
            ]
        );
    }

    #[test]
    fn an_svg_inside_an_svg_is_left_alone() {
        let checks = SvgImageChecks::new();
        let inner = r#"<svg xmlns='http://www.w3.org/2000/svg' width='5' height='5'><rect width='5' height='5'/></svg>"#;
        let svg = svg_with(&data_url("image/svg+xml", inner.as_bytes()));
        assert_eq!(parse(&checks, SvgSite::Img("a.svg".into()), &svg), 1);
        let doc = blitz_dom::BaseDocument::new(blitz_dom::DocumentConfig::default());
        assert_eq!(checks.take(&doc), SvgImageReport::default());
    }
}
