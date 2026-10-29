//! html-to-pdf F30f-d (pdf-reviewer defects): the frame of a box sliced at its page breaks
//! (`box-decoration-break: slice`, css-break-3 section 5.4) is painted as the unbroken box cut at the breaks.
//!
//! - The border keeps the radius of the corners that are not cut off (css-backgrounds-3 section 5.5), the same curves the
//!   background has; there is no radius at a cut edge.
//! - A `box-shadow` is not painted beyond a cut edge (the box is "rendered with no breaks present, and then sliced").
mod support;
use support::content_stream::count_ops;

use fulgur::asset::AssetBundle;
use fulgur::engine::Engine;
use std::path::PathBuf;
use std::process::Command;

fn engine() -> Engine {
    let font_path = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("../../examples/.fonts/NotoSans-Regular.ttf");
    let mut assets = AssetBundle::default();
    assets
        .add_font_file(&font_path)
        .unwrap_or_else(|e| panic!("failed to load Noto Sans from {}: {e}", font_path.display()));
    assets.add_css("body { font-family: 'Noto Sans', sans-serif; }");
    Engine::builder().assets(assets).build()
}

/// A bordered, padded, rounded box of 120 lines of 20px: three A4 pages with a 20mm margin, sliced at both breaks.
fn card(extra_style: &str) -> String {
    let lines: String = (0..120).map(|i| format!("line{i}<br>")).collect();
    format!(
        "<!DOCTYPE html><html><head><style>@page {{ size: A4; margin: 20mm }} body {{ margin: 0 }}</style></head><body>\
         <div style=\"border:2px solid #c00;padding:12px;border-radius:16px;line-height:20px;{extra_style}\">{lines}</div>\
         </body></html>"
    )
}

fn page_count(pdf: &[u8]) -> usize {
    let prefix = b"/Type /Page";
    pdf.windows(prefix.len() + 1)
        .filter(|w| &w[..prefix.len()] == prefix && !w[prefix.len()].is_ascii_alphanumeric())
        .count()
}

#[test]
fn a_sliced_rounded_border_has_curves_at_the_unbroken_corners_only() {
    // Page 1 has the top corners, the last page the bottom ones, the page between none: two curves per end, four in all.
    // (The two side lines of the middle page and the ends of the others are straight.) The border was drawn as straight
    // lines whenever a side was cut off, so the corners were square and no curve was written at all.
    let pdf = engine().render(&card("")).expect("render");
    assert_eq!(page_count(&pdf), 3);
    let Some(counts) = count_ops(&pdf) else {
        eprintln!("qpdf not installed - skipping");
        return;
    };
    assert_eq!(counts.c, 4, "curves in the border of a 3-page sliced box");
}

#[test]
fn a_sliced_rounded_border_and_background_use_the_same_curves() {
    // The background was always rounded at the unbroken corners; with the border following it there are two curves per
    // end for each of the two, eight in all.
    let pdf = engine().render(&card("background:#eef")).expect("render");
    assert_eq!(page_count(&pdf), 3);
    let Some(counts) = count_ops(&pdf) else {
        eprintln!("qpdf not installed - skipping");
        return;
    };
    assert_eq!(
        counts.c, 8,
        "curves in border + background of a 3-page sliced box"
    );
}

/// The 8-bit grey rows of page `page` (1-based) of `pdf` at 72 dpi: `(width, height, pixels)`. `None` without pdftoppm.
fn grey_page(pdf: &[u8], page: usize) -> Option<(usize, usize, Vec<u8>)> {
    let dir = tempfile::tempdir().expect("tempdir");
    let src = dir.path().join("in.pdf");
    std::fs::write(&src, pdf).expect("write pdf");
    let out = dir.path().join("page");
    let status = match Command::new("pdftoppm")
        .args([
            "-r",
            "72",
            "-gray",
            "-f",
            &page.to_string(),
            "-l",
            &page.to_string(),
        ])
        .arg(&src)
        .arg(&out)
        .status()
    {
        Ok(s) => s,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return None,
        Err(e) => panic!("failed to execute pdftoppm: {e}"),
    };
    assert!(status.success(), "pdftoppm failed: {status:?}");
    let file = std::fs::read_dir(dir.path())
        .expect("read dir")
        .map(|e| e.expect("entry").path())
        .find(|p| p.extension().is_some_and(|e| e == "pgm"))
        .expect("pgm output");
    let bytes = std::fs::read(file).expect("read pgm");
    // P5 <w> <h> <max> <pixels>, whitespace separated header.
    let mut fields = Vec::new();
    let mut pos = 0;
    while fields.len() < 4 {
        while bytes[pos].is_ascii_whitespace() {
            pos += 1;
        }
        let start = pos;
        while !bytes[pos].is_ascii_whitespace() {
            pos += 1;
        }
        fields.push(String::from_utf8_lossy(&bytes[start..pos]).into_owned());
    }
    pos += 1;
    assert_eq!(fields[0], "P5");
    let (w, h): (usize, usize) = (fields[1].parse().unwrap(), fields[2].parse().unwrap());
    Some((w, h, bytes[pos..].to_vec()))
}

fn row_is_white(pixels: &[u8], width: usize, y: usize) -> bool {
    pixels[y * width..(y + 1) * width].iter().all(|&p| p >= 252)
}

#[test]
fn a_sliced_box_shadow_stops_at_the_open_edges() {
    // A4 at 72 dpi is 595 x 842 px, the margin is 20mm = 56.7 px: the frame of page 1 ends at y 785.3 and the frame of
    // page 2 starts at y 56.7. The shadow (`0 4px 12px`) reaches 16 px below and 8 px above a whole box; at a cut edge
    // the box goes on on the other page, so there is no shadow in the margin there.
    let pdf = engine()
        .render(&card("box-shadow:0 4px 12px rgba(0,0,0,.6)"))
        .expect("render");
    assert_eq!(page_count(&pdf), 3);
    let Some((w1, _, page1)) = grey_page(&pdf, 1) else {
        eprintln!("pdftoppm not installed - skipping");
        return;
    };
    for y in 787..842 {
        assert!(
            row_is_white(&page1, w1, y),
            "page 1, row {y}: shadow below the bottom edge of a box that goes on"
        );
    }
    let (w2, _, page2) = grey_page(&pdf, 2).expect("page 2");
    for y in 0..56 {
        assert!(
            row_is_white(&page2, w2, y),
            "page 2, row {y}: shadow above the top edge of a continuation"
        );
    }
    // The shadow beside the box stays, at full strength up to the cut: a pixel 4 px left of the box at 1/2 of the page.
    assert!(
        page2[400 * w2 + 52] < 250,
        "no shadow beside the box on page 2"
    );
    // ...and the first page keeps the shadow above its top edge.
    let top_rows_white = (30..55).all(|y| row_is_white(&page1, w1, y));
    assert!(!top_rows_white, "page 1 lost the shadow above its top edge");
}
