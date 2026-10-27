//! The check a font file passes when it is registered (`AssetBundle::add_font_bytes`).
//!
//! Parley (fontique, skrifa) and Krilla parse a font separately, and they disagree about what a usable font is:
//! a face without a `head` table is registered and shaped with by Parley, but `krilla::text::Font::new` returns
//! `None` for it, so at draw time the run was skipped without a word. With the `glyf` table gone Krilla accepts the
//! face, finds nothing it can subset (`FontInfo::can_be_cid_font()` is false) and embeds a non-subset Type 3 font
//! whose glyph procedures are empty: the text layer says the words, the page shows nothing. Both are data loss that
//! looks like success (html-to-pdf F178).
//!
//! The check runs on the bytes the bundle keeps (a WOFF2 is decoded by then) and asks Krilla's own parser, because
//! it decides what gets embedded. A file is accepted only if every face in it passes: Parley registers a blob with all
//! of its faces, so a collection with one bad face is left out whole (OTS, Chrome's sanitiser, does the same).
//!
//! A face Krilla accepts can still fail later, when the PDF is written: `unitsPerEm` 0 panics in its font descriptor,
//! and a malformed `hmtx`, `maxp`, `post` or `loca` fails the subsetter, which fails the whole document. So a face with
//! outlines is also subset the way Krilla subsets it (`.notdef` plus a few mapped glyphs, `subsetter` with no
//! variation coordinates) and the result must open again.
//!
//! The cost is reading table directories, `head`, `hhea`, `OS/2`, `name` and the outline table headers of each face
//! (Krilla's `Font::new`), plus a subset of a few glyphs: well under a millisecond per face.

use std::fmt;
use std::sync::Arc;

use skrifa::bitmap::{BitmapData, BitmapStrikes};
use skrifa::instance::Size;
use skrifa::string::StringId;
use skrifa::{FontRef, MetadataProvider, Tag, raw::FileRef, raw::TableProvider};

/// How many glyphs the `cmap` maps are sampled: for PNG bitmap data when a face has no outlines, and for the trial
/// subset when it has.
const GLYPH_SAMPLE: usize = 16;

/// The `unitsPerEm` range OpenType allows (`head` table). Krilla divides by it; 0 panics, a tiny value puts the text
/// thousands of points off the page.
const UNITS_PER_EM: std::ops::RangeInclusive<u16> = 16..=16384;

/// One face of a font file the PDF writer cannot embed.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RejectedFace {
    /// The face's index in the file (0 for a single font).
    pub index: u32,
    /// The face's full name (`name` ID 4), when it could be read.
    pub name: Option<String>,
    /// Why, in a few words.
    pub reason: String,
}

impl fmt::Display for RejectedFace {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match &self.name {
            Some(name) => write!(f, "face {} ({name}): {}", self.index, self.reason),
            None => write!(f, "face {}: {}", self.index, self.reason),
        }
    }
}

/// Every face of a font file that the PDF writer cannot embed, in index order. Never empty.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FontRejection {
    pub faces: Vec<RejectedFace>,
}

impl fmt::Display for FontRejection {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        for (i, face) in self.faces.iter().enumerate() {
            if i > 0 {
                f.write_str("; ")?;
            }
            write!(f, "{face}")?;
        }
        Ok(())
    }
}

impl std::error::Error for FontRejection {}

/// `Ok(())` when every face of `data` can be embedded in a PDF, otherwise the faces that cannot.
///
/// `data` is an `Arc` so Krilla's `Font::new` shares the bytes instead of copying them (a collection can be tens of
/// megabytes).
pub fn check_font(data: &Arc<Vec<u8>>) -> Result<(), FontRejection> {
    let bytes: &[u8] = data;
    let count = match FileRef::new(bytes) {
        Ok(FileRef::Font(_)) => 1,
        Ok(FileRef::Collection(c)) => c.len(),
        Err(e) => return Err(not_a_font(0, e)),
    };
    if count == 0 {
        return Err(FontRejection {
            faces: vec![RejectedFace {
                index: 0,
                name: None,
                reason: "a font collection with no fonts".into(),
            }],
        });
    }
    let faces: Vec<RejectedFace> = (0..count)
        .filter_map(|index| check_face(data, index).err())
        .collect();
    if faces.is_empty() {
        Ok(())
    } else {
        Err(FontRejection { faces })
    }
}

fn not_a_font(index: u32, e: impl fmt::Display) -> FontRejection {
    FontRejection {
        faces: vec![RejectedFace {
            index,
            name: None,
            reason: format!("not an OpenType font: {e}"),
        }],
    }
}

fn check_face(data: &Arc<Vec<u8>>, index: u32) -> Result<(), RejectedFace> {
    let reject = |name: Option<String>, reason: &str| RejectedFace {
        index,
        name,
        reason: reason.to_string(),
    };
    let font = FontRef::from_index(data, index).map_err(|e| RejectedFace {
        index,
        name: None,
        reason: format!("not an OpenType font: {e}"),
    })?;
    let name = font
        .localized_strings(StringId::FULL_NAME)
        .english_or_first()
        .map(|n| n.to_string())
        .filter(|n| !n.is_empty());

    // Krilla decides what is embedded: ask it first, then say why when it says no.
    if krilla::text::Font::new(Arc::clone(data).into(), index).is_none() {
        let reason = match font.head() {
            Err(_) => "no `head` table",
            Ok(head) if head.units_per_em() == 0 => "unitsPerEm is 0",
            Ok(_) => "the PDF writer cannot read its metrics (not finite)",
        };
        return Err(reject(name, reason));
    }
    if let Ok(head) = font.head()
        && !UNITS_PER_EM.contains(&head.units_per_em())
    {
        return Err(RejectedFace {
            index,
            name,
            reason: format!("unitsPerEm {} is outside 16..16384", head.units_per_em()),
        });
    }

    // Krilla accepts a face without outlines and embeds invisible Type 3 glyphs; that is no better than no face. Without
    // outlines the only glyphs Krilla draws are PNG bitmaps (`CBDT` / `sbix`; `glyph/bitmap.rs`: "We only support
    // PNG"): `COLR` layers are outline glyphs, and fulgur installs no SVG-glyph renderer, so a `COLR` or `SVG ` table
    // alone paints nothing.
    if font.outline_glyphs().format().is_some() {
        return trial_subset(data, index, &font).map_err(|e| RejectedFace {
            index,
            name,
            reason: format!("the PDF writer cannot subset it: {e}"),
        });
    }
    if has_png_bitmaps(&font) {
        return Ok(());
    }
    let has_table = |t: &[u8; 4]| font.table_data(Tag::new(t)).is_some();
    let reason = if has_table(b"glyf") || has_table(b"CFF ") || has_table(b"CFF2") {
        "its glyph outlines do not parse (`glyf` / `loca`, `CFF ` or `CFF2`) and it has no PNG bitmap glyphs"
    } else {
        "no glyph outlines (no `glyf`, `CFF ` or `CFF2` table) and no PNG bitmap glyphs"
    };
    Err(reject(name, reason))
}

/// Subsets `.notdef` and the first glyphs the `cmap` maps as Krilla does when it writes a CID font (`text/cid.rs`
/// `subset_font`), and opens the result again.
fn trial_subset(data: &Arc<Vec<u8>>, index: u32, font: &FontRef<'_>) -> Result<(), String> {
    let mut glyphs: Vec<u16> = vec![0];
    glyphs.extend(
        font.charmap()
            .mappings()
            .take(GLYPH_SAMPLE)
            .filter_map(|(_, g)| u16::try_from(g.to_u32()).ok()),
    );
    let mut remapper = subsetter::GlyphRemapper::new();
    for g in glyphs {
        remapper.remap(g);
    }
    let subset = subsetter::subset_with_variations(data, index, &[], &remapper)
        .map_err(|e| e.to_string())?;
    krilla::text::Font::new(Arc::new(subset).into(), 0)
        .map(|_| ())
        .ok_or_else(|| "the subset does not open".to_string())
}

/// Whether one of the first glyphs the `cmap` maps has PNG bitmap data, the bitmap kind Krilla draws.
fn has_png_bitmaps(font: &FontRef<'_>) -> bool {
    let strikes = BitmapStrikes::new(font);
    if strikes.is_empty() {
        return false;
    }
    font.charmap()
        .mappings()
        .take(GLYPH_SAMPLE)
        .filter_map(|(_, glyph)| strikes.glyph_for_size(Size::unscaled(), glyph))
        .any(|g| matches!(g.data, BitmapData::Png(_)))
}

/// Helpers that build broken fonts from good ones, shared with the tests of `asset`.
#[cfg(test)]
pub(crate) mod test_support {
    /// `font` with the table `from` renamed to `to`; the directory stays sorted by tag.
    pub(crate) fn rename_table(font: &[u8], from: &[u8; 4], to: &[u8; 4]) -> Vec<u8> {
        let n = u16::from_be_bytes([font[4], font[5]]) as usize;
        let mut found = false;
        let mut records: Vec<[u8; 16]> = (0..n)
            .map(|i| {
                let mut r: [u8; 16] = font[12 + 16 * i..28 + 16 * i].try_into().unwrap();
                if &r[..4] == from {
                    r[..4].copy_from_slice(to);
                    found = true;
                }
                r
            })
            .collect();
        assert!(found, "no such table");
        records.sort_by(|a, b| a[..4].cmp(&b[..4]));
        let mut out = font.to_vec();
        for (i, r) in records.iter().enumerate() {
            out[12 + 16 * i..28 + 16 * i].copy_from_slice(r);
        }
        out
    }

    /// A TrueType collection of single sfnt fonts, table offsets rebased to the start of the file.
    pub(crate) fn ttc_of(fonts: &[&[u8]]) -> Vec<u8> {
        let mut out = vec![0u8; 12 + 4 * fonts.len()];
        out[..4].copy_from_slice(b"ttcf");
        out[4..8].copy_from_slice(&0x0001_0000u32.to_be_bytes());
        out[8..12].copy_from_slice(&(fonts.len() as u32).to_be_bytes());
        for (i, f) in fonts.iter().enumerate() {
            while out.len() % 4 != 0 {
                out.push(0);
            }
            let base = out.len();
            out[12 + 4 * i..16 + 4 * i].copy_from_slice(&(base as u32).to_be_bytes());
            out.extend_from_slice(f);
            let n = u16::from_be_bytes([f[4], f[5]]) as usize;
            for t in 0..n {
                let at = base + 12 + 16 * t + 8;
                let off = u32::from_be_bytes(out[at..at + 4].try_into().unwrap()) + base as u32;
                out[at..at + 4].copy_from_slice(&off.to_be_bytes());
            }
        }
        out
    }
}

#[cfg(test)]
mod tests {
    use super::test_support::{rename_table, ttc_of};
    use super::*;

    const NOTO: &[u8] = include_bytes!("../../../examples/.fonts/NotoSans-Regular.ttf");
    const NOTO_BOLD: &[u8] = include_bytes!("../../../examples/.fonts/NotoSans-Bold.ttf");
    const NOTO_MONO: &[u8] = include_bytes!("../../../examples/.fonts/NotoSansMono-Regular.ttf");
    /// A CFF-flavoured (`OTTO`) font.
    const NOTO_JP: &[u8] = include_bytes!("../../../examples/.fonts/NotoSansJP-Regular.otf");

    fn arc(bytes: &[u8]) -> Arc<Vec<u8>> {
        Arc::new(bytes.to_vec())
    }

    fn rejection(bytes: &[u8]) -> FontRejection {
        check_font(&arc(bytes)).expect_err("the font must be rejected")
    }

    #[test]
    fn good_fonts_pass() {
        for (what, f) in [
            ("TrueType", NOTO),
            ("monospace TrueType", NOTO_MONO),
            ("CFF", NOTO_JP),
        ] {
            assert_eq!(check_font(&arc(f)), Ok(()), "{what}");
        }
    }

    #[test]
    fn a_face_without_head_is_rejected_with_its_name() {
        let bad = rename_table(NOTO_BOLD, b"head", b"hexd");
        let r = rejection(&bad);
        assert_eq!(r.faces.len(), 1);
        assert_eq!(r.faces[0].index, 0);
        assert_eq!(r.faces[0].name.as_deref(), Some("Noto Sans Bold"));
        assert_eq!(r.faces[0].reason, "no `head` table");
        assert_eq!(r.to_string(), "face 0 (Noto Sans Bold): no `head` table");
    }

    #[test]
    fn a_face_without_outlines_is_rejected_although_krilla_would_embed_it() {
        let bad = rename_table(NOTO_BOLD, b"glyf", b"glyx");
        // Krilla takes it (and writes empty Type 3 glyphs): the check must not rely on `Font::new` alone.
        assert!(krilla::text::Font::new(Arc::new(bad.clone()).into(), 0).is_some());
        let r = rejection(&bad);
        assert_eq!(r.faces.len(), 1);
        assert!(r.faces[0].reason.contains("no glyph outlines"), "{r}");
    }

    #[test]
    fn a_colour_or_bitmap_table_without_outlines_or_png_glyphs_is_rejected() {
        // The `glyf` table renamed to a tag Krilla could paint from: still nothing it can draw.
        for tag in [b"COLR", b"SVG ", b"CBDT", b"sbix"] {
            let bad = rename_table(NOTO_BOLD, b"glyf", tag);
            let r = rejection(&bad);
            assert_eq!(r.faces.len(), 1, "{tag:?}");
            assert!(
                r.faces[0].reason.contains("no glyph outlines"),
                "{tag:?}: {r}"
            );
        }
    }

    /// `font` with the big-endian u16 at `table` offset `at` set to `value`.
    fn set_u16(font: &[u8], table: &[u8; 4], at: usize, value: u16) -> Vec<u8> {
        let n = u16::from_be_bytes([font[4], font[5]]) as usize;
        let record = (0..n)
            .map(|i| 12 + 16 * i)
            .find(|r| &font[*r..*r + 4] == table)
            .expect("no such table");
        let off = u32::from_be_bytes(font[record + 8..record + 12].try_into().unwrap()) as usize;
        let mut out = font.to_vec();
        out[off + at..off + at + 2].copy_from_slice(&value.to_be_bytes());
        out
    }

    #[test]
    fn units_per_em_outside_the_opentype_range_is_rejected() {
        // `head.unitsPerEm` is at offset 18. Krilla's `Font::new` accepts 0 and then panics writing the descriptor.
        for upm in [0u16, 1, 15, 16385] {
            let bad = set_u16(NOTO_BOLD, b"head", 18, upm);
            let r = rejection(&bad);
            assert!(r.faces[0].reason.contains("unitsPerEm"), "{upm}: {r}");
        }
        assert_eq!(
            check_font(&arc(&set_u16(NOTO_BOLD, b"head", 18, 2048))),
            Ok(())
        );
    }

    #[test]
    fn a_face_the_subsetter_cannot_subset_is_rejected() {
        // `hhea.numberOfHMetrics` (offset 34) 0: Krilla opens the face, the subsetter fails ("malformed font") and,
        // before F178, failed the whole document.
        let bad = set_u16(NOTO_BOLD, b"hhea", 34, 0);
        assert!(krilla::text::Font::new(Arc::new(bad.clone()).into(), 0).is_some());
        let r = rejection(&bad);
        assert!(r.faces[0].reason.contains("cannot subset"), "{r}");
    }

    #[test]
    fn outlines_that_do_not_parse_are_named() {
        let bad = rename_table(NOTO_BOLD, b"loca", b"locx");
        let r = rejection(&bad);
        assert!(r.faces[0].reason.contains("do not parse"), "{r}");
    }

    #[test]
    fn bytes_that_are_not_a_font_are_rejected() {
        for junk in [&b"junk"[..], b"", &[0xDE, 0xAD, 0xBE, 0xEF, 0x00][..]] {
            let r = rejection(junk);
            assert_eq!(r.faces.len(), 1, "{junk:?}");
            assert_eq!(r.faces[0].index, 0);
            assert!(r.faces[0].name.is_none());
            assert!(r.faces[0].reason.starts_with("not an OpenType font"), "{r}");
        }
    }

    #[test]
    fn a_truncated_font_is_rejected_not_a_panic() {
        for len in [0, 3, 12, 100, 4000] {
            let _ = check_font(&arc(&NOTO[..len]));
        }
    }

    #[test]
    fn a_collection_names_the_bad_face_by_index_and_passes_when_all_are_good() {
        let good = ttc_of(&[NOTO, NOTO_BOLD]);
        assert_eq!(check_font(&arc(&good)), Ok(()));

        let bad = ttc_of(&[NOTO, &rename_table(NOTO_BOLD, b"head", b"hexd")]);
        let r = rejection(&bad);
        assert_eq!(r.faces.len(), 1, "{r}");
        assert_eq!(r.faces[0].index, 1);
        assert_eq!(r.faces[0].reason, "no `head` table");

        // Every bad face is named, in index order.
        let both = ttc_of(&[
            &rename_table(NOTO, b"glyf", b"glyx"),
            NOTO_BOLD,
            &rename_table(NOTO_MONO, b"head", b"hexd"),
        ]);
        let r = rejection(&both);
        let indices: Vec<u32> = r.faces.iter().map(|f| f.index).collect();
        assert_eq!(indices, [0, 2], "{r}");
        assert!(r.to_string().contains("; face 2"), "{r}");
    }

    #[test]
    fn an_empty_collection_is_rejected() {
        let r = rejection(&ttc_of(&[]));
        assert_eq!(r.faces.len(), 1);
        assert!(r.faces[0].reason.contains("no fonts"), "{r}");
    }
}
