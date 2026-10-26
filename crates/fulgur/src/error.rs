#[derive(Debug, thiserror::Error)]
pub enum Error {
    #[error("HTML parse error: {0}")]
    HtmlParse(String),

    #[error("Layout error: {0}")]
    Layout(String),

    #[error("PDF generation error: {0}")]
    PdfGeneration(String),

    #[error("IO error: {0}")]
    Io(#[from] std::io::Error),

    #[error("Asset error: {0}")]
    Asset(String),

    #[error("Template error: {0}")]
    Template(String),

    #[error("WOFF decode error: {0}")]
    WoffDecode(String),

    #[error("Unsupported font format: {0}")]
    UnsupportedFontFormat(String),

    /// Text for which no glyph run was produced (`EngineBuilder::strict_text`): `<element> "excerpt"` each,
    /// at most 20 in the message. A character the chosen font has no glyph for (`.notdef`) still gets a run
    /// and is not reported; nor is text in `@page` margin boxes.
    #[error("text without a font face: {}", join_dropped(.0))]
    TextWithoutFont(Vec<crate::convert::DroppedText>),

    /// Raster images inside SVGs that cannot be drawn (`EngineBuilder::strict_images`), in document order: the SVG,
    /// the image and why, at most 20 in the message. A top-level `<img>` with such an image is refused by the
    /// caller's own check instead; without strict mode these images are left out of their SVG with a warning.
    #[error("images inside SVGs that cannot be drawn: {}", join_svg_images(.0))]
    SvgImages(Vec<crate::svg_images::DroppedSvgImage>),

    /// SVG text whose `font-family` list matches no bundled face (`EngineBuilder::strict_text`), in document order:
    /// the SVG and the list as written, at most 20 in the message. Without strict mode that text is drawn in the
    /// default family (the serif mapping of the asset bundle) with a warning.
    #[error("SVG text with no bundled font: {}", join_svg_fonts(.0))]
    SvgFontFallback(Vec<crate::svg_images::SvgFontFallback>),

    #[error("{0}")]
    Other(String),
}

/// Entries of `Error::TextWithoutFont` named in its message; a document can have thousands.
const MAX_DROPPED_IN_MESSAGE: usize = 20;

fn join_dropped(list: &[crate::convert::DroppedText]) -> String {
    let mut out = list
        .iter()
        .take(MAX_DROPPED_IN_MESSAGE)
        .map(|d| d.to_string())
        .collect::<Vec<_>>()
        .join("; ");
    if list.len() > MAX_DROPPED_IN_MESSAGE {
        out.push_str(&format!(
            "; ... and {} more",
            list.len() - MAX_DROPPED_IN_MESSAGE
        ));
    }
    out
}

fn join_svg_images(list: &[crate::svg_images::DroppedSvgImage]) -> String {
    let mut out = list
        .iter()
        .take(MAX_DROPPED_IN_MESSAGE)
        .map(|d| d.to_string())
        .collect::<Vec<_>>()
        .join("; ");
    if list.len() > MAX_DROPPED_IN_MESSAGE {
        out.push_str(&format!(
            "; ... and {} more",
            list.len() - MAX_DROPPED_IN_MESSAGE
        ));
    }
    out
}

fn join_svg_fonts(list: &[crate::svg_images::SvgFontFallback]) -> String {
    let mut out = list
        .iter()
        .take(MAX_DROPPED_IN_MESSAGE)
        .map(|d| d.to_string())
        .collect::<Vec<_>>()
        .join("; ");
    if list.len() > MAX_DROPPED_IN_MESSAGE {
        out.push_str(&format!(
            "; ... and {} more",
            list.len() - MAX_DROPPED_IN_MESSAGE
        ));
    }
    out
}

impl From<minijinja::Error> for Error {
    fn from(e: minijinja::Error) -> Self {
        Error::Template(e.to_string())
    }
}

pub type Result<T> = std::result::Result<T, Error>;

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn text_without_font_message_names_at_most_twenty_entries() {
        let many: Vec<_> = (0..1000)
            .map(|i| crate::convert::DroppedText {
                element: "p".into(),
                excerpt: format!("t{i}"),
            })
            .collect();
        let msg = Error::TextWithoutFont(many).to_string();
        assert!(msg.contains("t19") && !msg.contains("t20\""), "{msg}");
        assert!(msg.ends_with("... and 980 more"), "{msg}");
        assert!(msg.len() < 1000, "{} bytes", msg.len());
        let few = Error::TextWithoutFont(vec![crate::convert::DroppedText {
            element: "p".into(),
            excerpt: "x".into(),
        }]);
        assert_eq!(few.to_string(), "text without a font face: <p> \"x\"");
    }

    #[test]
    fn test_template_error_display() {
        let err = Error::Template("syntax error at line 3".into());
        assert!(err.to_string().contains("syntax error at line 3"));
    }
}
