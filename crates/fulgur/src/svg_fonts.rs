//! The fonts SVG `<text>` is shaped with: the bundle's, never the host's (html-to-pdf F152).
//!
//! Blitz used to parse every inline `<svg>` with a process-wide `fontdb::Database` that called
//! `load_system_fonts()`, so the PDF depended on the machine and the first SVG paid for the scan; an SVG used as an
//! image was parsed with usvg's default `Options`, whose database is empty, so its text was not drawn at all. Now
//! every place fulgur parses an SVG gets its options from [`crate::svg_images::SvgImageChecks`], and those carry a
//! database built here: every bundled face, in bundle order, from the bundle's own bytes (no copy, no file, no
//! host directory), the generic families mapped as [`AssetBundle::generic_families`](crate::asset::AssetBundle)
//! says (the map HTML text uses), and a resolver that picks the face the way CSS Fonts 4 §5 describes:
//! the families of the list in order, a generic keyword through the map, and when nothing matches the UA's
//! default face (the serif mapping) instead of dropping the text. That substitution is reported as a finding.

use std::collections::BTreeMap;
use std::sync::{Arc, OnceLock};

use usvg::fontdb;

/// The five generic families `fontdb` has a setter for.
const FONTDB_GENERICS: [&str; 5] = ["serif", "sans-serif", "monospace", "cursive", "fantasy"];

/// The bundled faces as a `fontdb::Database`, built the first time an SVG asks for it: a document without SVG text
/// pays nothing.
pub struct SvgFonts {
    fonts: Vec<Arc<Vec<u8>>>,
    generics: BTreeMap<String, String>,
    built: OnceLock<Built>,
}

struct Built {
    db: Arc<fontdb::Database>,
    /// Family names of the faces, ASCII-lower-cased, to the name as the face spells it: CSS Fonts 4 §5.1 matches
    /// family names ASCII case-insensitively, `fontdb` compares them exactly.
    names: BTreeMap<String, String>,
    /// What an SVG that names no family is drawn in: the serif mapping.
    default_family: String,
}

impl SvgFonts {
    /// `fonts` are the bundle's faces ([`AssetBundle::fonts`](crate::asset::AssetBundle::fonts)), `generics` its
    /// generic-family map. Nothing is parsed until [`SvgFonts::database`] or a resolver needs it.
    pub fn new(fonts: &[Arc<Vec<u8>>], generics: &BTreeMap<String, String>) -> Arc<Self> {
        Arc::new(Self {
            fonts: fonts.to_vec(),
            generics: generics.clone(),
            built: OnceLock::new(),
        })
    }

    /// The database: the bundled faces, nothing else. Built once.
    pub fn database(&self) -> Arc<fontdb::Database> {
        Arc::clone(&self.built().db)
    }

    /// The family an SVG text with no `font-family` is drawn in (usvg's `Options::font_family`): the serif
    /// mapping, or the first bundled family when the map has none. Empty without bundled fonts.
    pub fn default_family(&self) -> String {
        self.built().default_family.clone()
    }

    fn built(&self) -> &Built {
        self.built.get_or_init(|| self.build())
    }

    fn build(&self) -> Built {
        let mut db = fontdb::Database::new();
        for bytes in &self.fonts {
            let shared: Arc<dyn AsRef<[u8]> + Send + Sync> = Arc::clone(bytes) as _;
            db.load_font_source(fontdb::Source::Binary(shared));
        }
        let mut names = BTreeMap::new();
        for face in db.faces() {
            for (name, _) in &face.families {
                names
                    .entry(name.to_ascii_lowercase())
                    .or_insert_with(|| name.clone());
            }
        }
        let first = db.faces().next().map(|face| {
            face.families
                .iter()
                .find(|f| f.1 == fontdb::Language::English_UnitedStates)
                .unwrap_or(&face.families[0])
                .0
                .clone()
        });
        let registered = |name: &str| names.get(&name.to_ascii_lowercase()).cloned();
        // A mapping to a family that is not bundled maps to nothing, as for HTML text (`apply_generic_families`).
        let mapped = |keyword: &str| self.generics.get(keyword).and_then(|f| registered(f));
        let serif = mapped("serif").or(first).unwrap_or_default();
        db.set_serif_family(serif.clone());
        for keyword in &FONTDB_GENERICS[1..] {
            if let Some(family) = mapped(keyword) {
                match *keyword {
                    "sans-serif" => db.set_sans_serif_family(family),
                    "monospace" => db.set_monospace_family(family),
                    "cursive" => db.set_cursive_family(family),
                    _ => db.set_fantasy_family(family),
                }
            }
        }
        Built {
            db: Arc::new(db),
            names,
            default_family: serif,
        }
    }

    /// The font selection of an SVG (`usvg::Options::font_resolver`). `on_fallback` is called with the family list
    /// as written when no entry of it matches a bundled face and the text is drawn in the serif mapping instead.
    /// The fallback selection of a character no chosen face has is usvg's default (now over bundled faces only, in
    /// bundle order).
    pub fn resolver(
        self: &Arc<Self>,
        on_fallback: impl Fn(String) + Send + Sync + 'static,
    ) -> usvg::FontResolver<'static> {
        let fonts = Arc::clone(self);
        usvg::FontResolver {
            select_font: Box::new(move |font, db| {
                let built = fonts.built();
                let query = |families: &[fontdb::Family<'_>]| {
                    db.query(&fontdb::Query {
                        families,
                        weight: fontdb::Weight(font.weight()),
                        stretch: stretch(font.stretch()),
                        style: style(font.style()),
                    })
                };
                // Owned names first: `fontdb::Family::Name` borrows.
                let names: Vec<Option<String>> = font
                    .families()
                    .iter()
                    .map(|family| match family {
                        usvg::FontFamily::Named(name) => Some(fonts.named(built, name)),
                        _ => None,
                    })
                    .collect();
                let list: Vec<fontdb::Family<'_>> = font
                    .families()
                    .iter()
                    .zip(&names)
                    .map(|(family, name)| match (family, name) {
                        (usvg::FontFamily::Serif, _) => fontdb::Family::Serif,
                        (usvg::FontFamily::SansSerif, _) => fontdb::Family::SansSerif,
                        (usvg::FontFamily::Cursive, _) => fontdb::Family::Cursive,
                        (usvg::FontFamily::Fantasy, _) => fontdb::Family::Fantasy,
                        (usvg::FontFamily::Monospace, _) => fontdb::Family::Monospace,
                        (usvg::FontFamily::Named(_), Some(name)) => fontdb::Family::Name(name),
                        (usvg::FontFamily::Named(_), None) => {
                            unreachable!("named families have names")
                        }
                    })
                    .collect();
                if let Some(id) = query(&list) {
                    return Some(id);
                }
                on_fallback(family_list(font.families()));
                query(&[fontdb::Family::Serif])
            }),
            select_fallback: usvg::FontResolver::default_fallback_selector(),
        }
    }

    /// The name a `Named` family is looked up under: a bundled family of that name (any ASCII case), else the
    /// family a generic keyword that usvg reads as a plain name (`system-ui`, `ui-monospace`, …) is mapped to, else
    /// the name as written.
    fn named(&self, built: &Built, name: &str) -> String {
        let folded = name.to_ascii_lowercase();
        if let Some(registered) = built.names.get(&folded) {
            return registered.clone();
        }
        if let Some(mapped) = self.generics.get(&folded) {
            return built
                .names
                .get(&mapped.to_ascii_lowercase())
                .cloned()
                .unwrap_or_else(|| mapped.clone());
        }
        name.to_string()
    }
}

/// A family list as it reads in CSS: names that need it in quotes.
pub(crate) fn family_list(families: &[usvg::FontFamily]) -> String {
    families
        .iter()
        .map(|family| match family {
            usvg::FontFamily::Named(name)
                if name.is_empty()
                    || name
                        .chars()
                        .any(|c| !(c.is_alphanumeric() || c == '-' || c == '_')) =>
            {
                format!("'{}'", name.replace('\'', "\\'"))
            }
            usvg::FontFamily::Named(name) => name.clone(),
            keyword => keyword.to_string(),
        })
        .collect::<Vec<_>>()
        .join(", ")
}

fn stretch(s: usvg::FontStretch) -> fontdb::Stretch {
    match s {
        usvg::FontStretch::UltraCondensed => fontdb::Stretch::UltraCondensed,
        usvg::FontStretch::ExtraCondensed => fontdb::Stretch::ExtraCondensed,
        usvg::FontStretch::Condensed => fontdb::Stretch::Condensed,
        usvg::FontStretch::SemiCondensed => fontdb::Stretch::SemiCondensed,
        usvg::FontStretch::Normal => fontdb::Stretch::Normal,
        usvg::FontStretch::SemiExpanded => fontdb::Stretch::SemiExpanded,
        usvg::FontStretch::Expanded => fontdb::Stretch::Expanded,
        usvg::FontStretch::ExtraExpanded => fontdb::Stretch::ExtraExpanded,
        usvg::FontStretch::UltraExpanded => fontdb::Stretch::UltraExpanded,
    }
}

fn style(s: usvg::FontStyle) -> fontdb::Style {
    match s {
        usvg::FontStyle::Normal => fontdb::Style::Normal,
        usvg::FontStyle::Italic => fontdb::Style::Italic,
        usvg::FontStyle::Oblique => fontdb::Style::Oblique,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Mutex;

    fn font(name: &str) -> Arc<Vec<u8>> {
        let path = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("../../examples/.fonts")
            .join(name);
        Arc::new(std::fs::read(&path).unwrap_or_else(|e| panic!("{}: {e}", path.display())))
    }

    fn bundle() -> Vec<Arc<Vec<u8>>> {
        vec![
            font("NotoSans-Regular.ttf"),
            font("NotoSans-Bold.ttf"),
            font("NotoSansMono-Regular.ttf"),
        ]
    }

    fn generics(pairs: &[(&str, &str)]) -> BTreeMap<String, String> {
        pairs
            .iter()
            .map(|(k, v)| (k.to_string(), v.to_string()))
            .collect()
    }

    /// The full name of the face `<text font-family=attrs>` picks, and the fallbacks reported.
    fn pick(fonts: &Arc<SvgFonts>, attrs: &str) -> (Option<String>, Vec<String>) {
        let found = Arc::new(Mutex::new(Vec::new()));
        let sink = Arc::clone(&found);
        let options = usvg::Options {
            fontdb: fonts.database(),
            font_resolver: fonts.resolver(move |list| sink.lock().unwrap().push(list)),
            font_family: fonts.default_family(),
            ..usvg::Options::default()
        };
        let svg = format!(
            r#"<svg xmlns="http://www.w3.org/2000/svg" width="100" height="20"><text y="15" {attrs}>Aa</text></svg>"#
        );
        let tree = usvg::Tree::from_str(&svg, &options).unwrap();
        let used = used_faces(&tree);
        let list = found.lock().unwrap().clone();
        (used.into_iter().next(), list)
    }

    /// Post-script names of the faces the text of `tree` was laid out with.
    fn used_faces(tree: &usvg::Tree) -> Vec<String> {
        fn walk(group: &usvg::Group, out: &mut Vec<String>, db: &fontdb::Database) {
            for node in group.children() {
                match node {
                    usvg::Node::Group(g) => walk(g, out, db),
                    usvg::Node::Text(t) => {
                        for span in t.layouted() {
                            for glyph in &span.positioned_glyphs {
                                let face = db.face(glyph.font).expect("a face of the database");
                                if !out.contains(&face.post_script_name) {
                                    out.push(face.post_script_name.clone());
                                }
                            }
                        }
                    }
                    _ => {}
                }
            }
        }
        let mut out = Vec::new();
        walk(tree.root(), &mut out, tree.fontdb());
        out
    }

    #[test]
    fn the_database_holds_the_bundled_faces_in_bundle_order_and_no_host_face() {
        let fonts = SvgFonts::new(&bundle(), &BTreeMap::new());
        let db = fonts.database();
        let names: Vec<_> = db.faces().map(|f| f.post_script_name.as_str()).collect();
        assert_eq!(
            names,
            ["NotoSans-Regular", "NotoSans-Bold", "NotoSansMono-Regular"]
        );
        // The bytes are the bundle's, not a copy or a file.
        assert!(
            db.faces()
                .all(|f| matches!(f.source, fontdb::Source::Binary(_)))
        );
        assert_eq!(db.len(), 3);
        // Built once.
        assert!(Arc::ptr_eq(&db, &fonts.database()));
    }

    #[test]
    fn an_empty_bundle_is_an_empty_database() {
        let fonts = SvgFonts::new(&[], &BTreeMap::new());
        assert_eq!(fonts.database().len(), 0);
        assert_eq!(fonts.default_family(), "");
    }

    #[test]
    fn the_generic_setters_follow_the_map() {
        let map = generics(&[
            ("serif", "Noto Sans"),
            ("sans-serif", "noto sans"),
            ("monospace", "Noto Sans Mono"),
            ("cursive", "Not Bundled"),
        ]);
        let fonts = SvgFonts::new(&bundle(), &map);
        assert_eq!(fonts.default_family(), "Noto Sans");
        assert_eq!(
            pick(&fonts, "font-family=\"monospace\"").0.as_deref(),
            Some("NotoSansMono-Regular")
        );
        assert_eq!(
            pick(&fonts, "font-family=\"sans-serif\"").0.as_deref(),
            Some("NotoSans-Regular")
        );
        assert_eq!(
            pick(&fonts, "font-family=\"serif\"").0.as_deref(),
            Some("NotoSans-Regular")
        );
        // A mapping to a family that is not bundled maps to nothing: the text falls back, with a finding.
        let (face, found) = pick(&fonts, "font-family=\"cursive\"");
        assert_eq!(face.as_deref(), Some("NotoSans-Regular"));
        assert_eq!(found, ["cursive"]);
    }

    #[test]
    fn a_missing_serif_entry_is_the_first_bundled_family() {
        let fonts = SvgFonts::new(&bundle(), &BTreeMap::new());
        assert_eq!(fonts.default_family(), "Noto Sans");
        // No finding for text that names no family: usvg is given the default family by name.
        let (face, found) = pick(&fonts, "");
        assert_eq!(face.as_deref(), Some("NotoSans-Regular"));
        assert!(found.is_empty(), "{found:?}");
    }

    #[test]
    fn named_keywords_resolve_through_the_map() {
        let map = generics(&[
            ("serif", "Noto Sans"),
            ("ui-monospace", "Noto Sans Mono"),
            ("system-ui", "Noto Sans"),
        ]);
        let fonts = SvgFonts::new(&bundle(), &map);
        for (attrs, want) in [
            ("font-family=\"ui-monospace\"", "NotoSansMono-Regular"),
            (
                "font-family=\"UI-Monospace, serif\"",
                "NotoSansMono-Regular",
            ),
            (
                "font-family=\"system-ui\" font-weight=\"bold\"",
                "NotoSans-Bold",
            ),
        ] {
            let (face, found) = pick(&fonts, attrs);
            assert_eq!(face.as_deref(), Some(want), "{attrs}");
            assert!(found.is_empty(), "{attrs}: {found:?}");
        }
    }

    #[test]
    fn family_names_match_ascii_case_insensitively_and_a_later_entry_is_tried() {
        let fonts = SvgFonts::new(&bundle(), &BTreeMap::new());
        for attrs in [
            "font-family=\"noto SANS mono\"",
            "font-family=\"Georgia, 'Noto Sans Mono'\"",
        ] {
            let (face, found) = pick(&fonts, attrs);
            assert_eq!(face.as_deref(), Some("NotoSansMono-Regular"), "{attrs}");
            assert!(found.is_empty(), "{attrs}: {found:?}");
        }
    }

    #[test]
    fn a_list_with_no_match_is_a_finding_and_is_drawn_in_the_serif_mapping() {
        let map = generics(&[("serif", "Noto Sans Mono")]);
        let fonts = SvgFonts::new(&bundle(), &map);
        let (face, found) = pick(&fonts, "font-family=\"Georgia, 'Times New Roman', serif2\"");
        assert_eq!(face.as_deref(), Some("NotoSansMono-Regular"));
        assert_eq!(found, ["Georgia, 'Times New Roman', serif2"]);
    }

    #[test]
    fn weight_picks_the_closest_bundled_face() {
        let fonts = SvgFonts::new(&bundle(), &BTreeMap::new());
        assert_eq!(
            pick(&fonts, "font-weight=\"700\"").0.as_deref(),
            Some("NotoSans-Bold")
        );
        assert_eq!(
            pick(&fonts, "font-weight=\"400\"").0.as_deref(),
            Some("NotoSans-Regular")
        );
    }

    #[test]
    fn family_lists_read_as_css() {
        use usvg::FontFamily::*;
        assert_eq!(
            family_list(&[
                Named("Georgia".into()),
                Named("Times New Roman".into()),
                SansSerif
            ]),
            "Georgia, 'Times New Roman', sans-serif"
        );
    }
}
