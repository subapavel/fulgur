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
//! Nothing here, or anywhere else fulgur parses an SVG, reads the host's filesystem (html-to-pdf F177b). usvg's
//! default string resolver treats every `href` that is not a `data:` URL as a path and reads it; the resolver
//! of [`SvgImageChecks`] never does: an inline `<svg>` finds a non-`data:` href by name in the
//! [`AssetBundle`] (the rule of `<img src>`), and an SVG used as an image (`<img>`, `background-image`,
//! `list-style-image`) loads no external file at all, as SVG Integration's secure static mode says. What
//! is left out is reported with an [`SvgImageCause`]. Code that parses an SVG without checks uses
//! [`offline_options`].
//!
//! The same options carry the fonts of the SVG's `<text>` (html-to-pdf F152): the bundled faces of
//! [`SvgFonts`](crate::svg_fonts::SvgFonts) and a resolver that reports a `font-family` list matching none of them as
//! an [`SvgFontFallback`] (the text is drawn in the default family instead).
//!
//! One [`SvgImageChecks`] serves one layout pass of a render. Every place fulgur parses an SVG that can hold images
//! or text gets its options from [`SvgImageChecks::options`] (the SVG assets of `<img>`, `background-image` and
//! `list-style-image`) or its factory ([`SvgImageChecks::factory`], the inline `<svg>` elements Blitz parses).

use std::collections::{BTreeMap, BTreeSet};
use std::sync::{Arc, Mutex, MutexGuard};

use crate::asset::AssetBundle;
use crate::svg_fonts::SvgFonts;

/// Longest href, in characters, that is named in a finding as it is; a longer one (a malformed `data:` URL) is cut.
const MAX_HREF_NAME: usize = 120;

/// Why a raster image inside an SVG is left out of the SVG.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum SvgImageCause {
    /// The image is there and cannot be drawn: truncated, a header that claims more than
    /// [`crate::image::MAX_IMAGE_PIXELS`], bytes that are no image.
    Undecodable,
    /// The `href` of an `<image>` in an inline `<svg>` that is not the name of a bundled image (the lookup of
    /// `<img src>`: [`AssetBundle::get_image`]), or any `href` of an SVG in a page margin box, which has no
    /// bundle.
    NotBundled,
    /// The `href` of an SVG used as an image (`<img>`, `background-image`, `list-style-image`) that is not a
    /// `data:` URL. Such an SVG loads no external file, bundled or not.
    External,
}

/// A raster image inside an SVG that is left out of the SVG, and why.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord)]
pub struct DroppedSvgImage {
    /// The SVG the image is in: `<svg id="logo">`, `<svg> no. 2` (the second inline `<svg>` of the document),
    /// `<img src="logo.svg">`, `background-image: url(logo.svg)` or `list-style-image: url(logo.svg)`.
    pub svg: String,
    /// The image: `data:image/png (4096 bytes)` for a `data:` URL, the href otherwise.
    pub image: String,
    /// Why: `unexpected end of file`, `100000 x 100000 pixels is more than the 134217728 pixel limit`,
    /// `not a bundled image`, …
    pub reason: String,
    pub cause: SvgImageCause,
}

impl std::fmt::Display for DroppedSvgImage {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}: {}: {}", self.svg, self.image, self.reason)
    }
}

/// An SVG whose `font-family` list matches no bundled face, so its text is drawn in the default family (the serif
/// mapping) instead of being dropped (css-fonts-4 §5).
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord)]
pub struct SvgFontFallback {
    /// The SVG, named as in [`DroppedSvgImage::svg`].
    pub svg: String,
    /// The family list as written, e.g. `Georgia, Palatino`.
    pub font_family: String,
}

impl std::fmt::Display for SvgFontFallback {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}: font-family `{}`", self.svg, self.font_family)
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
    /// Family lists of SVG text that matched no bundled face, by site then list.
    pub font_fallbacks: Vec<SvgFontFallback>,
}

#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord)]
struct Finding {
    site: SvgSite,
    image: String,
    reason: String,
    cause: SvgImageCause,
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
    /// The SVG and the family list as written; once each.
    font_fallbacks: BTreeSet<(SvgSite, String)>,
}

/// usvg's resolvers with the filesystem taken out: `data:` URLs as usvg resolves them, every other href not at all.
pub(crate) fn offline_resolver() -> usvg::ImageHrefResolver<'static> {
    usvg::ImageHrefResolver {
        resolve_data: usvg::ImageHrefResolver::default_data_resolver(),
        resolve_string: Box::new(|_, _| None),
    }
}

/// `usvg::Options::default()` with resolvers that read nothing from the host: a `data:` image is loaded as usvg
/// loads it (and not checked, see [`SvgImageChecks`]), any other `href` is not loaded. For parsing an SVG whose
/// images are not drawn, or are checked by something else. Its font database is empty: text in an SVG parsed with
/// these options is not drawn (use [`SvgImageChecks::options`] for SVGs that are rendered).
pub fn offline_options() -> usvg::Options<'static> {
    usvg::Options {
        image_href_resolver: offline_resolver(),
        // Nothing to choose from, so say nothing: usvg's default selector logs "No match for '…' font-family" for
        // every text it cannot draw, which is noise for a parse that only validates or measures.
        font_resolver: usvg::FontResolver {
            select_font: Box::new(|_, _| None),
            select_fallback: Box::new(|_, _, _| None),
        },
        ..usvg::Options::default()
    }
}

/// See the module documentation.
pub struct SvgImageChecks {
    state: Mutex<State>,
    /// The bundle an inline `<svg>` finds its non-`data:` hrefs in; `None` finds nothing there (the SVGs of the
    /// `@page` margin boxes).
    images: Option<Arc<AssetBundle>>,
    /// The fonts of every SVG this serves.
    fonts: Arc<SvgFonts>,
    /// Log a finding when it is first made instead of keeping it for [`SvgImageChecks::take`]: for the documents of
    /// the `@page` margin boxes, which are converted after the engine has reported.
    immediate: bool,
}

impl SvgImageChecks {
    /// Checks for one render. `images` is the render's bundle (an `Arc` the engine holds: no copy per render).
    /// `fonts` are the faces the SVGs' text is shaped with (one [`SvgFonts`] per render).
    pub fn new(images: Option<Arc<AssetBundle>>, fonts: Arc<SvgFonts>) -> Arc<Self> {
        Arc::new(Self {
            state: Mutex::default(),
            images,
            fonts,
            immediate: false,
        })
    }

    /// Checks that `log::warn!` each finding as it is made (once per image) and are not read with
    /// [`SvgImageChecks::take`]. They have no bundle.
    pub(crate) fn warning_immediately(fonts: Arc<SvgFonts>) -> Arc<Self> {
        Arc::new(Self {
            state: Mutex::default(),
            images: None,
            fonts,
            immediate: true,
        })
    }

    fn state(&self) -> MutexGuard<'_, State> {
        self.state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }

    /// The resolvers of an SVG at `site`: usvg's `data:` resolver, refusing the rasters that cannot be drawn, and
    /// [`SvgImageChecks::resolve_href`] for everything else.
    pub fn resolver(self: &Arc<Self>, site: SvgSite) -> usvg::ImageHrefResolver<'static> {
        let default_data = usvg::ImageHrefResolver::default_data_resolver();
        let (checks, data_site) = (Arc::clone(self), site.clone());
        let (string_checks, string_site) = (Arc::clone(self), site);
        usvg::ImageHrefResolver {
            resolve_data: Box::new(move |mime, data, opts| {
                let kind = default_data(mime, data, opts)?;
                let name = data_name(&kind);
                checks.check(&data_site, name, kind)
            }),
            resolve_string: Box::new(move |href, opts| {
                string_checks.resolve_href(&string_site, href, opts)
            }),
        }
    }

    /// An `href` that is not a `data:` URL. Never reads the filesystem (F177b).
    ///
    /// - In an inline `<svg>`: the name of a bundled image, looked up as `<img src>` is
    ///   ([`AssetBundle::get_image`]: the exact key, a leading `./` dropped; an empty href is missing). The
    ///   bytes are loaded as usvg loads a `data:` image whose type is sniffed from its content, and checked.
    ///   Not in the bundle (or no bundle): [`SvgImageCause::NotBundled`].
    /// - In an SVG used as an image: never loaded, bundled or not ([`SvgImageCause::External`]).
    fn resolve_href(
        &self,
        site: &SvgSite,
        href: &str,
        opts: &usvg::Options,
    ) -> Option<usvg::ImageKind> {
        let name = href_name(href);
        if !matches!(site, SvgSite::Inline(_)) {
            self.refuse(
                site,
                name,
                "an SVG used as an image loads no external files",
                SvgImageCause::External,
            );
            return None;
        }
        let bytes = self
            .images
            .as_ref()
            .filter(|_| !href.is_empty())
            .and_then(|bundle| bundle.get_image(href));
        let Some(bytes) = bytes else {
            self.refuse(site, name, "not a bundled image", SvgImageCause::NotBundled);
            return None;
        };
        let sniffed =
            usvg::ImageHrefResolver::default_data_resolver()("text/plain", Arc::clone(bytes), opts);
        match sniffed {
            Some(kind) => self.check(site, name, kind),
            None => {
                self.refuse(
                    site,
                    name,
                    "not a PNG, JPEG, GIF, WebP or SVG image",
                    SvgImageCause::Undecodable,
                );
                None
            }
        }
    }

    /// [`offline_options`] with [`SvgImageChecks::resolver`] and the bundled fonts: the database, the face picking
    /// (an unmatched family list is a finding) and, for text with no `font-family`, the default family.
    pub fn options(self: &Arc<Self>, site: SvgSite) -> usvg::Options<'static> {
        let checks = Arc::clone(self);
        let font_site = site.clone();
        usvg::Options {
            image_href_resolver: self.resolver(site),
            fontdb: self.fonts.database(),
            font_resolver: self
                .fonts
                .resolver(move |list| checks.record_font_fallback(&font_site, list)),
            font_family: self.fonts.default_family(),
            ..offline_options()
        }
    }

    /// The factory Blitz calls with the node id of each inline `<svg>` it parses.
    pub fn factory(self: &Arc<Self>) -> blitz_dom::SvgOptionsFactory {
        let checks = Arc::clone(self);
        Arc::new(move |node_id| checks.options(SvgSite::Inline(node_id)))
    }

    /// Keeps a family list that matched no bundled face, once, and logs it right away when asked to.
    fn record_font_fallback(&self, site: &SvgSite, list: String) {
        let new = self
            .state()
            .font_fallbacks
            .insert((site.clone(), list.clone()));
        if self.immediate && new {
            log::warn!(
                "{}: {}",
                site_name(site, None),
                font_fallback_message(&list, &self.fonts.default_family())
            );
        }
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
        drop(state);
        match verdict {
            Verdict::Valid(None) => Some(kind),
            Verdict::Valid(Some(why)) => {
                self.record(site, name, why, SvgImageCause::Undecodable, true);
                Some(kind)
            }
            Verdict::Refused(why) => {
                self.record(site, name, why, SvgImageCause::Undecodable, false);
                None
            }
        }
    }

    /// An image that is left out, not because it is damaged.
    fn refuse(&self, site: &SvgSite, name: String, why: &str, cause: SvgImageCause) {
        self.record(site, name, why.to_string(), cause, false);
    }

    /// Keeps a finding (`damaged`: an embedded JPEG that is damaged; otherwise an image left out), once, and logs
    /// it right away when asked to.
    fn record(
        &self,
        site: &SvgSite,
        image: String,
        reason: String,
        cause: SvgImageCause,
        damaged: bool,
    ) {
        let finding = Finding {
            site: site.clone(),
            image,
            reason,
            cause,
        };
        let mut state = self.state();
        let new = if damaged {
            state.damaged.insert(finding.clone())
        } else {
            state.dropped.insert(finding.clone())
        };
        drop(state);
        if self.immediate && new {
            let what = describe(&finding, None);
            if damaged {
                log::warn!(
                    "{what} is embedded although it is damaged: {}",
                    finding.reason
                );
            } else {
                log::warn!("{what} is not drawn: {}", finding.reason);
            }
        }
    }

    /// The findings so far, named after `doc` (the document the inline SVGs are in), and forgets them. The verdicts
    /// are kept.
    pub fn take(&self, doc: &blitz_dom::BaseDocument) -> SvgImageReport {
        let (dropped, damaged, font_fallbacks) = {
            let mut state = self.state();
            (
                std::mem::take(&mut state.dropped),
                std::mem::take(&mut state.damaged),
                std::mem::take(&mut state.font_fallbacks),
            )
        };
        let names = InlineNames::new(doc, &dropped, &damaged, &font_fallbacks);
        let name = |f: Finding| DroppedSvgImage {
            svg: site_name(&f.site, Some(&names)),
            image: f.image,
            reason: f.reason,
            cause: f.cause,
        };
        SvgImageReport {
            dropped: dropped.into_iter().map(name).collect(),
            damaged: damaged.into_iter().map(name).collect(),
            font_fallbacks: font_fallbacks
                .into_iter()
                .map(|(site, font_family)| SvgFontFallback {
                    svg: site_name(&site, Some(&names)),
                    font_family,
                })
                .collect(),
        }
    }
}

/// The href as a finding names it: as it is, unless it is very long.
fn href_name(href: &str) -> String {
    match href.char_indices().nth(MAX_HREF_NAME) {
        Some((end, _)) => format!("{}...", &href[..end]),
        None => href.to_string(),
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
        font_fallbacks: &BTreeSet<(SvgSite, String)>,
    ) -> Self {
        let wanted: BTreeSet<usize> = dropped
            .iter()
            .chain(damaged)
            .map(|f| &f.site)
            .chain(font_fallbacks.iter().map(|(site, _)| site))
            .filter_map(|site| match site {
                SvgSite::Inline(id) => Some(*id),
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

/// What is said of an SVG whose family list matched no bundled face.
pub(crate) fn font_fallback_message(list: &str, default_family: &str) -> String {
    format!(
        "font-family `{list}` of the SVG text matches no bundled font, so it is drawn in the default family \
         `{default_family}`"
    )
}

/// `svg: image`, for a log line.
fn describe(f: &Finding, names: Option<&InlineNames>) -> String {
    format!("{}: {}", site_name(&f.site, names), f.image)
}

#[cfg(test)]
mod tests {
    use super::*;

    const GIF: &[u8] = include_bytes!("../tests/fixtures/images/gradient32.gif");

    fn no_fonts() -> Arc<SvgFonts> {
        SvgFonts::new(&[], &BTreeMap::new())
    }

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
        let checks = SvgImageChecks::new(None, no_fonts());
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
        let checks = SvgImageChecks::new(None, no_fonts());
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
        let checks = SvgImageChecks::new(None, no_fonts());
        let inner = r#"<svg xmlns='http://www.w3.org/2000/svg' width='5' height='5'><rect width='5' height='5'/></svg>"#;
        let svg = svg_with(&data_url("image/svg+xml", inner.as_bytes()));
        assert_eq!(parse(&checks, SvgSite::Img("a.svg".into()), &svg), 1);
        let doc = blitz_dom::BaseDocument::new(blitz_dom::DocumentConfig::default());
        assert_eq!(checks.take(&doc), SvgImageReport::default());
    }
    fn bundle(entries: &[(&str, &[u8])]) -> Option<Arc<AssetBundle>> {
        let mut bundle = AssetBundle::new();
        for (name, bytes) in entries {
            bundle.add_image(*name, bytes.to_vec());
        }
        Some(Arc::new(bundle))
    }

    fn report(checks: &Arc<SvgImageChecks>) -> SvgImageReport {
        checks.take(&blitz_dom::BaseDocument::new(
            blitz_dom::DocumentConfig::default(),
        ))
    }

    #[test]
    fn an_inline_svg_finds_a_href_by_name_in_the_bundle() {
        let checks = SvgImageChecks::new(bundle(&[("logo.gif", GIF)]), no_fonts());
        for href in ["logo.gif", "./logo.gif"] {
            assert_eq!(
                parse(&checks, SvgSite::Inline(1), &svg_with(href)),
                1,
                "{href}"
            );
        }
        assert_eq!(report(&checks), SvgImageReport::default());
    }

    #[test]
    fn a_bundled_svg_in_an_inline_svg_is_loaded_without_images_of_its_own() {
        let inner = format!(
            r#"<svg xmlns="http://www.w3.org/2000/svg" width="5" height="5"><image href="{}" width="5" height="5"/></svg>"#,
            gif_url(GIF)
        );
        let checks = SvgImageChecks::new(bundle(&[("card.svg", inner.as_bytes())]), no_fonts());
        let tree = usvg::Tree::from_str(&svg_with("card.svg"), &checks.options(SvgSite::Inline(1)))
            .unwrap();
        assert_eq!(images(&tree), 1);
        assert_eq!(report(&checks), SvgImageReport::default());
    }

    #[test]
    fn an_href_that_is_not_bundled_is_dropped_whatever_is_on_the_disk() {
        let dir = tempfile::tempdir().unwrap();
        let on_disk = dir.path().join("logo.gif");
        std::fs::write(&on_disk, GIF).unwrap();
        let absolute = on_disk.to_str().unwrap().to_string();
        let file_url = format!("file://{absolute}");
        let hrefs = [
            absolute.as_str(),
            file_url.as_str(),
            "http://example.com/logo.gif",
            "other.gif",
            "",
        ];
        // With a bundle that holds something else, and with none (the SVGs of the page margin boxes).
        for images in [bundle(&[("logo.gif", GIF)]), None] {
            let bundled = images.is_some();
            let checks = SvgImageChecks::new(images, no_fonts());
            for href in hrefs {
                assert_eq!(
                    parse(&checks, SvgSite::Inline(1), &svg_with(href)),
                    0,
                    "{href}"
                );
            }
            let dropped = report(&checks).dropped;
            // An empty href reaches the resolver too, and is not a bundled image either.
            let named: Vec<_> = dropped.iter().map(|d| d.image.as_str()).collect();
            let mut want = vec![
                absolute.as_str(),
                file_url.as_str(),
                "http://example.com/logo.gif",
                "other.gif",
                "",
            ];
            want.sort();
            assert_eq!(named, want, "bundled {bundled}");
            for d in &dropped {
                assert_eq!(d.cause, SvgImageCause::NotBundled);
                assert_eq!(d.reason, "not a bundled image");
                assert_eq!(d.svg, "inline <svg> (node 1) in a page margin box");
            }
        }
    }

    #[test]
    fn an_svg_used_as_an_image_loads_no_external_file_bundled_or_not() {
        let dir = tempfile::tempdir().unwrap();
        let on_disk = dir.path().join("logo.gif");
        std::fs::write(&on_disk, GIF).unwrap();
        let absolute = on_disk.to_str().unwrap().to_string();
        let checks = SvgImageChecks::new(bundle(&[("logo.gif", GIF)]), no_fonts());
        let sites = [
            SvgSite::Img("a.svg".into()),
            SvgSite::Background("b.svg".into()),
            SvgSite::ListMarker("l.svg".into()),
        ];
        for site in &sites {
            for href in ["logo.gif", absolute.as_str()] {
                assert_eq!(parse(&checks, site.clone(), &svg_with(href)), 0, "{href}");
            }
            // The `data:` twin is drawn.
            assert_eq!(parse(&checks, site.clone(), &svg_with(&gif_url(GIF))), 1);
        }
        let dropped = report(&checks).dropped;
        assert_eq!(dropped.len(), 6, "{dropped:?}");
        for d in &dropped {
            assert_eq!(d.cause, SvgImageCause::External, "{d}");
            assert_eq!(d.reason, "an SVG used as an image loads no external files");
        }
        assert_eq!(dropped[0].svg, "<img src=\"a.svg\">");
        assert_eq!(dropped[0].image, absolute);
        assert_eq!(dropped[1].image, "logo.gif");
    }

    #[test]
    fn a_bundled_image_that_cannot_be_drawn_is_undecodable() {
        let checks = SvgImageChecks::new(
            bundle(&[
                ("cut.gif", &GIF[..GIF.len() / 2]),
                ("text.png", b"this is not an image"),
            ]),
            no_fonts(),
        );
        for href in ["cut.gif", "text.png"] {
            assert_eq!(
                parse(&checks, SvgSite::Inline(1), &svg_with(href)),
                0,
                "{href}"
            );
        }
        let dropped = report(&checks).dropped;
        let causes: Vec<_> = dropped
            .iter()
            .map(|d| (d.image.as_str(), d.cause))
            .collect();
        assert_eq!(
            causes,
            [
                ("cut.gif", SvgImageCause::Undecodable),
                ("text.png", SvgImageCause::Undecodable)
            ]
        );
    }

    #[test]
    fn a_very_long_href_is_named_in_part() {
        let long = format!("data:image/png;base64,{}", "A".repeat(5000));
        let checks = SvgImageChecks::new(None, no_fonts());
        // A malformed data URL is no data URL to usvg: it reaches the string resolver.
        parse(&checks, SvgSite::Inline(1), &svg_with(&format!("{long}%")));
        let dropped = report(&checks).dropped;
        assert!(
            dropped
                .iter()
                .all(|d| d.image.chars().count() <= MAX_HREF_NAME + 3),
            "{dropped:?}"
        );
    }

    #[test]
    fn offline_options_load_data_urls_and_nothing_else() {
        let dir = tempfile::tempdir().unwrap();
        let on_disk = dir.path().join("logo.gif");
        std::fs::write(&on_disk, GIF).unwrap();
        let load = |href: &str| {
            images(&usvg::Tree::from_str(&svg_with(href), &offline_options()).unwrap())
        };
        assert_eq!(load(&gif_url(GIF)), 1);
        assert_eq!(load(on_disk.to_str().unwrap()), 0);
        assert_eq!(load(&format!("file://{}", on_disk.display())), 0);
    }

    #[test]
    fn offline_options_have_no_fonts_and_log_nothing_for_text() {
        struct Capture(Arc<Mutex<Vec<String>>>);
        impl log::Log for Capture {
            fn enabled(&self, _: &log::Metadata<'_>) -> bool {
                true
            }
            fn log(&self, r: &log::Record<'_>) {
                if r.target().starts_with("usvg") {
                    self.0.lock().unwrap().push(r.args().to_string());
                }
            }
            fn flush(&self) {}
        }
        let seen = Arc::new(Mutex::new(Vec::new()));
        // The logger is process-wide: if another test installed one first, only the tree is checked.
        let installed = log::set_boxed_logger(Box::new(Capture(Arc::clone(&seen)))).is_ok();
        log::set_max_level(log::LevelFilter::Trace);
        let svg = r#"<svg xmlns="http://www.w3.org/2000/svg" width="50" height="20"><text y="15" font-family="sans-serif">Aa</text></svg>"#;
        let tree = usvg::Tree::from_str(svg, &offline_options()).unwrap();
        assert!(tree.root().children().is_empty(), "no font, no text");
        if installed {
            let seen = seen.lock().unwrap();
            assert!(!seen.iter().any(|m| m.contains("No match")), "{seen:?}");
        }
    }

    #[test]
    fn no_parse_reads_a_device_file() {
        let started = std::time::Instant::now();
        let href = "/dev/zero";
        let svg = svg_with(href);
        assert_eq!(
            images(&usvg::Tree::from_str(&svg, &offline_options()).unwrap()),
            0
        );
        let checks = SvgImageChecks::new(bundle(&[("logo.gif", GIF)]), no_fonts());
        for site in [
            SvgSite::Inline(1),
            SvgSite::Img("a.svg".into()),
            SvgSite::Background("b.svg".into()),
            SvgSite::ListMarker("l.svg".into()),
        ] {
            assert_eq!(parse(&checks, site, &svg), 0);
        }
        assert!(started.elapsed() < std::time::Duration::from_secs(1));
        let causes: Vec<_> = report(&checks).dropped.iter().map(|d| d.cause).collect();
        assert_eq!(
            causes,
            [
                SvgImageCause::NotBundled,
                SvgImageCause::External,
                SvgImageCause::External,
                SvgImageCause::External
            ]
        );
    }

    #[test]
    fn a_family_list_with_no_bundled_face_is_one_finding_per_site_and_list() {
        let noto = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("../../examples/.fonts/NotoSans-Regular.ttf");
        let fonts = SvgFonts::new(&[Arc::new(std::fs::read(noto).unwrap())], &BTreeMap::new());
        let checks = SvgImageChecks::new(None, fonts);
        let text = |family: &str| {
            format!(
                r#"<svg xmlns="http://www.w3.org/2000/svg" width="50" height="20"><text y="15" font-family="{family}">Aa</text></svg>"#
            )
        };
        for (site, family) in [
            (SvgSite::Img("b.svg".into()), "Georgia"),
            (SvgSite::Img("b.svg".into()), "Georgia"),
            (SvgSite::Img("a.svg".into()), "Georgia"),
            (SvgSite::Background("c.svg".into()), "Noto Sans"),
            (
                SvgSite::ListMarker("l.svg".into()),
                "Georgia, 'Times New Roman'",
            ),
        ] {
            parse(&checks, site, &text(family));
        }
        let first = report(&checks);
        let got: Vec<_> = first
            .font_fallbacks
            .iter()
            .map(|f| (f.svg.as_str(), f.font_family.as_str()))
            .collect();
        assert_eq!(
            got,
            [
                ("<img src=\"a.svg\">", "Georgia"),
                ("<img src=\"b.svg\">", "Georgia"),
                ("list-style-image: url(l.svg)", "Georgia, 'Times New Roman'"),
            ]
        );
        assert!(first.dropped.is_empty() && first.damaged.is_empty());
        // An SVG that names no family is drawn in the default family without a finding.
        let plain = r#"<svg xmlns="http://www.w3.org/2000/svg" width="50" height="20"><text y="15">Aa</text></svg>"#;
        parse(&checks, SvgSite::Img("p.svg".into()), plain);
        assert!(report(&checks).font_fallbacks.is_empty());
    }
}
