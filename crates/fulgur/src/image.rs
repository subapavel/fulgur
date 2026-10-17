//! ImageRender — renders images in PDF via Krilla's Image API.

use std::cell::RefCell;
use std::collections::BTreeMap;
use std::rc::Rc;
use std::sync::Arc;

/// Image format detected from data.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ImageFormat {
    Png,
    Jpeg,
    Gif,
}

impl ImageFormat {
    /// Create a krilla Image from raw data in this format.
    #[allow(clippy::result_unit_err)]
    pub fn to_krilla_image(self, data: krilla::Data) -> Result<krilla::image::Image, ()> {
        match self {
            ImageFormat::Png => krilla::image::Image::from_png(data, true).map_err(|_| ()),
            ImageFormat::Jpeg => krilla::image::Image::from_jpeg(data, true).map_err(|_| ()),
            ImageFormat::Gif => krilla::image::Image::from_gif(data, true).map_err(|_| ()),
        }
    }
}

/// Classification of an asset's bytes for rendering path selection.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AssetKind {
    /// Raster image (PNG/JPEG/GIF) → renders via `ImageRender`.
    Raster(ImageFormat),
    /// SVG vector image → renders via `SvgRender`.
    Svg,
    /// Unrecognized / unsupported.
    Unknown,
}

impl AssetKind {
    /// Classify raw asset bytes by sniffing the header.
    ///
    /// Accepts SVG via the `<svg` element start or `<?xml` prolog, tolerant
    /// of an optional leading UTF-8 BOM and ASCII whitespace. Falls through
    /// to `ImageRender::detect_format` for raster magic bytes.
    pub fn detect(data: &[u8]) -> AssetKind {
        if let Some(format) = ImageRender::detect_format(data) {
            return AssetKind::Raster(format);
        }
        let mut slice = data;
        // Strip optional UTF-8 BOM so `<svg` / `<?xml` detection works.
        if slice.starts_with(b"\xEF\xBB\xBF") {
            slice = &slice[3..];
        }
        // Skip ASCII whitespace that commonly precedes the SVG root.
        while let Some((first, rest)) = slice.split_first() {
            if first.is_ascii_whitespace() {
                slice = rest;
            } else {
                break;
            }
        }
        if slice.starts_with(b"<?xml") || slice.starts_with(b"<svg") {
            return AssetKind::Svg;
        }
        AssetKind::Unknown
    }
}

/// Largest PNG or GIF, in pixels, that [`validate`] accepts (a JPEG has its own limit, below): 128 Mi (134 Mpx). An A4 scan at 600 dpi is 35 Mpx,
/// an A3 one 70 Mpx, 8193 × 8192 is 67 Mpx. The limit is checked from the header before any pixel is
/// decoded, so a hostile header cannot make us allocate gigabytes (a 20000 × 20000 = 400 Mpx RGB PNG is
/// 1.2 GB of output buffer).
pub const MAX_IMAGE_PIXELS: u64 = 128 * 1024 * 1024;

/// Why an image is not drawn.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum ImageError {
    /// Not a PNG, JPEG, GIF or SVG.
    #[error("not a PNG, JPEG, GIF or SVG image")]
    Unsupported,
    /// The header declares more than [`MAX_IMAGE_PIXELS`].
    #[error("{w} x {h} pixels is more than the {MAX_IMAGE_PIXELS} pixel limit")]
    TooLarge { w: u32, h: u32 },
    /// The header or the pixel data does not decode.
    #[error("{0}")]
    Corrupt(String),
}

/// Largest JPEG, in pixels, that [`validate_detailed`] decodes. Krilla passes a JPEG through undecoded and a
/// damaged one is embedded anyway, so decoding it only serves to tell damage from a file nothing can read, and
/// is not worth its cost on a scan: 16 Mi pixels (a 4000 x 4000 photo) take about 100 ms and 48 MB; a
/// 72 Mpx scan would take 430 ms and 226 MB where Krilla needs none. Larger JPEGs get the header check only.
pub const MAX_JPEG_VALIDATION_PIXELS: u64 = 16 * 1024 * 1024;

fn check_size(w: u32, h: u32) -> Result<(), ImageError> {
    if u64::from(w) * u64::from(h) > MAX_IMAGE_PIXELS {
        Err(ImageError::TooLarge { w, h })
    } else {
        Ok(())
    }
}

/// What [`validate_detailed`] found out about an image that can be drawn.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Validation {
    pub kind: AssetKind,
    /// `Some(reason)` for a JPEG that is embedded although it is damaged: truncated, without an end marker
    /// or with stray bytes between header segments. Chrome and every viewer draw what is there and Krilla
    /// passes the bytes through, so it is not refused; an invoice with a cut-off logo is better than one
    /// with none. `None` for everything else.
    pub damaged: Option<String>,
}

/// Decodes `data` completely and says whether it can be drawn; see [`validate_detailed`].
pub fn validate(data: &[u8]) -> Result<AssetKind, ImageError> {
    validate_detailed(data).map(|v| v.kind)
}

/// Decodes `data` and says whether it can be drawn, and whether it is damaged.
///
/// Krilla reads only the header of a PNG or GIF when an image is created and decodes the pixels in
/// `Document::finish()`, where a failure aborts the whole document; a JPEG is passed through after a
/// header check without ever being decoded. So a PNG or GIF is refused when it does not decode, with the
/// decoders and calls Krilla uses (`png`, `gif`) and the pixel count checked against [`MAX_IMAGE_PIXELS`]
/// from the header first. A JPEG is refused only when `zune-jpeg` cannot decode it at all, even leniently
/// (a garbage header, an unsupported coding); one that decodes only leniently is accepted and reported in
/// [`Validation::damaged`] (`zune-jpeg`'s strict mode is what notices truncation). A JPEG over
/// [`MAX_JPEG_VALIDATION_PIXELS`] is checked by its header only: nothing of Krilla's decodes it. SVG is parsed with `usvg` and no font
/// database.
///
/// Costs one extra decode of the image.
pub fn validate_detailed(data: &[u8]) -> Result<Validation, ImageError> {
    let kind = AssetKind::detect(data);
    let ok = |damaged| Ok(Validation { kind, damaged });
    match kind {
        AssetKind::Unknown => Err(ImageError::Unsupported),
        AssetKind::Svg => usvg::Tree::from_data(data, &usvg::Options::default())
            .map_err(|e| ImageError::Corrupt(e.to_string()))
            .and_then(|_| ok(None)),
        AssetKind::Raster(ImageFormat::Png) => validate_png(data).and_then(|()| ok(None)),
        AssetKind::Raster(ImageFormat::Jpeg) => validate_jpeg(data).and_then(ok),
        AssetKind::Raster(ImageFormat::Gif) => validate_gif(data).and_then(|()| ok(None)),
    }
}

fn validate_png(data: &[u8]) -> Result<(), ImageError> {
    let mut decoder = png::Decoder::new(std::io::Cursor::new(data));
    // The transformation Krilla applies (`PNG_TRANSFORMATIONS`).
    decoder.set_transformations(png::Transformations::EXPAND);
    let mut reader = decoder
        .read_info()
        .map_err(|e| ImageError::Corrupt(e.to_string()))?;
    let (w, h) = (reader.info().width, reader.info().height);
    check_size(w, h)?;
    let mut pixels = vec![0; reader.output_buffer_size()];
    reader
        .next_frame(&mut pixels)
        .map_err(|e| ImageError::Corrupt(e.to_string()))?;
    Ok(())
}

/// `Ok(Some(reason))` when the JPEG decodes leniently but not strictly.
fn validate_jpeg(data: &[u8]) -> Result<Option<String>, ImageError> {
    use zune_jpeg::zune_core::options::DecoderOptions;
    let corrupt = |e: zune_jpeg::errors::DecodeErrors| ImageError::Corrupt(e.to_string());
    let decoder =
        |options| zune_jpeg::JpegDecoder::new_with_options(std::io::Cursor::new(data), options);
    // The header, leniently, as Krilla reads it.
    let mut headers = decoder(DecoderOptions::default());
    headers.decode_headers().map_err(corrupt)?;
    let (w, h) = headers
        .dimensions()
        .ok_or_else(|| ImageError::Corrupt("failed to read image dimensions".into()))?;
    // Our own scan gives the natural size to layout; a JPEG it cannot read the way the decoder does has
    // no box and is not drawn unless CSS gives both sizes, which must not go unsaid.
    let scanned = ImageRender::decode_dimensions(data, ImageFormat::Jpeg);
    if let Some(note) = header_size_note(scanned, (w, h)) {
        return Ok(Some(note));
    }
    if u64::from(w as u32) * u64::from(h as u32) > MAX_JPEG_VALIDATION_PIXELS {
        // Krilla passes a JPEG through undecoded: the header was all it asked for.
        return Ok(None);
    }
    let strict = decoder(DecoderOptions::default().set_strict_mode(true))
        .decode()
        .map(|_| ());
    match strict {
        Ok(()) => Ok(None),
        Err(strict_error) => {
            decoder(DecoderOptions::default())
                .decode()
                .map_err(corrupt)?;
            Ok(Some(strict_error.to_string().trim_matches('"').to_string()))
        }
    }
}

/// A reason when the size our header scan found (`scanned`, [`ImageRender::decode_dimensions`]) is not the
/// one the decoder read (`decoded`, `zune-jpeg`'s `dimensions()`), `None` when they agree.
fn header_size_note(scanned: Option<(u32, u32)>, decoded: (usize, usize)) -> Option<String> {
    let (w, h) = decoded;
    if scanned.is_some_and(|(sw, sh)| sw as usize == w && sh as usize == h) {
        return None;
    }
    let found = match scanned {
        Some((sw, sh)) => format!("{sw} \u{d7} {sh}"),
        None => "none".to_string(),
    };
    Some(format!(
        "the size in the header could not be read (scan: {found}; decoder: {w} \u{d7} {h}); \
         it is drawn only where CSS gives both sizes"
    ))
}

fn validate_gif(data: &[u8]) -> Result<(), ImageError> {
    let corrupt = |e: gif::DecodingError| ImageError::Corrupt(e.to_string());
    let mut options = gif::DecodeOptions::new();
    options.set_color_output(gif::ColorOutput::RGBA);
    let mut decoder = options.read_info(data).map_err(corrupt)?;
    check_size(u32::from(decoder.width()), u32::from(decoder.height()))?;
    decoder
        .read_next_frame()
        .map_err(corrupt)?
        .ok_or_else(|| ImageError::Corrupt("GIF image seems to be empty".into()))?;
    Ok(())
}

/// What [`krilla_image`] knows about the images of one render, keyed by the address of the bundle's
/// `Arc` (the cache holds the `Arc`, so an address is never reused). Lookup only: nothing iterates it,
/// so it cannot influence the order of anything in the PDF.
#[derive(Clone, Default)]
pub struct ImageCache(Rc<RefCell<BTreeMap<(usize, u8), CachedImage>>>);

/// The bytes kept alive for their address, and the image if it passed [`validate`].
type CachedImage = (Arc<Vec<u8>>, Option<krilla::image::Image>);

/// The Krilla image for `data`, or `None` when the image cannot be drawn.
///
/// The one place a raster image reaches Krilla. An image that fails [`validate`] is not drawn (and
/// logged at debug level) instead of failing `Document::finish()` for the whole document; the layout
/// box it occupies is unaffected. `cache` makes the validation and the creation of the Krilla image
/// happen once per bundled image, however often or on however many pages it is drawn.
pub fn krilla_image(
    cache: &ImageCache,
    data: &Arc<Vec<u8>>,
    format: ImageFormat,
) -> Option<krilla::image::Image> {
    let key = (Arc::as_ptr(data) as usize, format as u8);
    if let Some((_, image)) = cache.0.borrow().get(&key) {
        return image.clone();
    }
    let image = match validate(data) {
        Ok(_) => format
            .to_krilla_image(Arc::clone(data).into())
            .map_err(|()| log::debug!("image not drawn: Krilla refused it"))
            .ok(),
        Err(e) => {
            log::debug!("image not drawn: {e}");
            None
        }
    };
    cache
        .0
        .borrow_mut()
        .insert(key, (Arc::clone(data), image.clone()));
    image
}

/// An image element that renders an image at its computed size.
#[derive(Clone)]
pub struct ImageRender {
    pub image_data: Arc<Vec<u8>>,
    pub format: ImageFormat,
    pub width: f32,
    pub height: f32,
    pub opacity: f32,
    pub visible: bool,
    /// fulgur-r6we (Phase 3.2.a): DOM NodeId for `slice_for_page`
    /// geometry lookup.
    pub node_id: Option<usize>,
}

impl ImageRender {
    pub fn new(data: Arc<Vec<u8>>, format: ImageFormat, width: f32, height: f32) -> Self {
        Self {
            image_data: data,
            format,
            width,
            height,
            opacity: 1.0,
            visible: true,
            node_id: None,
        }
    }

    pub fn with_node_id(mut self, node_id: Option<usize>) -> Self {
        self.node_id = node_id;
        self
    }

    /// Decode image dimensions (width, height) from header bytes.
    /// Returns None if the data is too short or malformed.
    pub fn decode_dimensions(data: &[u8], format: ImageFormat) -> Option<(u32, u32)> {
        match format {
            ImageFormat::Png => {
                // PNG IHDR: bytes 16..20 = width (BE u32), 20..24 = height (BE u32)
                if data.len() < 24 {
                    return None;
                }
                let w = u32::from_be_bytes([data[16], data[17], data[18], data[19]]);
                let h = u32::from_be_bytes([data[20], data[21], data[22], data[23]]);
                Some((w, h))
            }
            ImageFormat::Gif => {
                // GIF header: bytes 6..8 = width (LE u16), 8..10 = height (LE u16)
                if data.len() < 10 {
                    return None;
                }
                let w = u16::from_le_bytes([data[6], data[7]]) as u32;
                let h = u16::from_le_bytes([data[8], data[9]]) as u32;
                Some((w, h))
            }
            ImageFormat::Jpeg => {
                // JPEG: scan for SOF0..SOF15 markers (0xC0..0xCF, excluding 0xC4 DHT and 0xCC DAC)
                // SOF structure: FF Cx, 2-byte length, 1 byte precision, 2 bytes height (BE), 2 bytes width (BE)
                if data.len() < 2 {
                    return None;
                }
                let mut i = 2; // skip SOI (FF D8)
                while i + 1 < data.len() {
                    if data[i] != 0xFF {
                        i += 1;
                        continue;
                    }
                    // T.81 §B.1.1.2: a marker may be preceded by any number of 0xFF fill bytes.
                    // The scan ends at the first SOF, so it never meets entropy-coded data.
                    while i + 1 < data.len() && data[i + 1] == 0xFF {
                        i += 1;
                    }
                    if i + 1 >= data.len() {
                        return None;
                    }
                    let marker = data[i + 1];
                    i += 2;
                    if (0xC0..=0xCF).contains(&marker) && marker != 0xC4 && marker != 0xCC {
                        if i + 7 > data.len() {
                            return None;
                        }
                        let h = u16::from_be_bytes([data[i + 3], data[i + 4]]) as u32;
                        let w = u16::from_be_bytes([data[i + 5], data[i + 6]]) as u32;
                        return Some((w, h));
                    }
                    if i + 1 >= data.len() {
                        return None;
                    }
                    let seg_len = u16::from_be_bytes([data[i], data[i + 1]]) as usize;
                    if seg_len < 2 {
                        return None;
                    }
                    i += seg_len;
                }
                None
            }
        }
    }

    /// Detect image format from data magic bytes.
    pub fn detect_format(data: &[u8]) -> Option<ImageFormat> {
        if data.starts_with(b"\x89PNG") {
            Some(ImageFormat::Png)
        } else if data.starts_with(b"\xFF\xD8\xFF") {
            Some(ImageFormat::Jpeg)
        } else if data.starts_with(b"GIF") {
            Some(ImageFormat::Gif)
        } else {
            None
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // 1x1 red PNG
    const MINIMAL_PNG: &[u8] = &[
        0x89, 0x50, 0x4E, 0x47, 0x0D, 0x0A, 0x1A, 0x0A, 0x00, 0x00, 0x00, 0x0D, 0x49, 0x48, 0x44,
        0x52, 0x00, 0x00, 0x00, 0x01, 0x00, 0x00, 0x00, 0x01, 0x08, 0x02, 0x00, 0x00, 0x00, 0x90,
        0x77, 0x53, 0xDE, 0x00, 0x00, 0x00, 0x0C, 0x49, 0x44, 0x41, 0x54, 0x78, 0x9C, 0x63, 0xF8,
        0xCF, 0xC0, 0x00, 0x00, 0x03, 0x01, 0x01, 0x00, 0xC9, 0xFE, 0x92, 0xEF, 0x00, 0x00, 0x00,
        0x00, 0x49, 0x45, 0x4E, 0x44, 0xAE, 0x42, 0x60, 0x82,
    ];

    // 1x1 white GIF89a
    const MINIMAL_GIF: &[u8] = &[
        0x47, 0x49, 0x46, 0x38, 0x39, 0x61, 0x01, 0x00, 0x01, 0x00, 0x00, 0x00, 0x00, 0x3B,
    ];

    #[test]
    fn test_png_dimensions() {
        let dims = ImageRender::decode_dimensions(MINIMAL_PNG, ImageFormat::Png);
        assert_eq!(dims, Some((1, 1)));
    }

    #[test]
    fn jpeg_decode_dimensions_skips_fill_bytes() {
        // T.81 §B.1.1.2: any marker may be preceded by any number of 0xFF fill bytes. A synthetic header,
        // fill bytes before APP0 and before SOF0: 40 wide, 30 high.
        let jpeg = [
            &[0xFF, 0xD8][..],
            &[0xFF, 0xFF, 0xFF, 0xE0, 0x00, 0x04, 0x00, 0x00],
            &[
                0xFF, 0xFF, 0xFF, 0xC0, 0x00, 0x0B, 0x08, 0x00, 0x1E, 0x00, 0x28, 0x01, 0x01, 0x11,
                0x00,
            ],
        ]
        .concat();
        assert_eq!(
            ImageRender::decode_dimensions(&jpeg, ImageFormat::Jpeg),
            Some((40, 30))
        );
        // Without them: the same answer.
        let plain = [&jpeg[..2], &jpeg[4..10], &jpeg[12..]].concat();
        assert_eq!(
            ImageRender::decode_dimensions(&plain, ImageFormat::Jpeg),
            Some((40, 30))
        );
    }

    #[test]
    fn test_gif_dimensions() {
        let dims = ImageRender::decode_dimensions(MINIMAL_GIF, ImageFormat::Gif);
        assert_eq!(dims, Some((1, 1)));
    }

    // Minimal 1x1 white JPEG (SOF0 baseline)
    const MINIMAL_JPEG: &[u8] = &[
        0xFF, 0xD8, // SOI
        0xFF, 0xE0, // APP0 (JFIF)
        0x00, 0x10, // length = 16
        0x4A, 0x46, 0x49, 0x46, 0x00, // "JFIF\0"
        0x01, 0x01, // version
        0x00, // units
        0x00, 0x01, // X density
        0x00, 0x01, // Y density
        0x00, 0x00, // thumbnail
        0xFF, 0xC0, // SOF0
        0x00, 0x0B, // length = 11
        0x08, // precision = 8
        0x00, 0x01, // height = 1
        0x00, 0x01, // width = 1
        0x01, // components = 1
        0x01, 0x11, 0x00, // component 1
        0xFF, 0xD9, // EOI
    ];

    #[test]
    fn test_jpeg_dimensions() {
        let dims = ImageRender::decode_dimensions(MINIMAL_JPEG, ImageFormat::Jpeg);
        assert_eq!(dims, Some((1, 1)));
    }

    #[test]
    fn test_truncated_data_returns_none() {
        let dims = ImageRender::decode_dimensions(&[0x89, 0x50], ImageFormat::Png);
        assert_eq!(dims, None);
    }

    #[test]
    fn test_detect_asset_kind_png() {
        assert!(matches!(
            AssetKind::detect(MINIMAL_PNG),
            AssetKind::Raster(ImageFormat::Png)
        ));
    }

    #[test]
    fn test_detect_asset_kind_jpeg() {
        assert!(matches!(
            AssetKind::detect(MINIMAL_JPEG),
            AssetKind::Raster(ImageFormat::Jpeg)
        ));
    }

    #[test]
    fn test_detect_asset_kind_gif() {
        assert!(matches!(
            AssetKind::detect(MINIMAL_GIF),
            AssetKind::Raster(ImageFormat::Gif)
        ));
    }

    #[test]
    fn test_detect_asset_kind_svg_tag() {
        let svg = br#"<svg xmlns="http://www.w3.org/2000/svg"></svg>"#;
        assert!(matches!(AssetKind::detect(svg), AssetKind::Svg));
    }

    #[test]
    fn test_detect_asset_kind_svg_xml_prolog() {
        let svg = br#"<?xml version="1.0"?><svg xmlns="http://www.w3.org/2000/svg"></svg>"#;
        assert!(matches!(AssetKind::detect(svg), AssetKind::Svg));
    }

    #[test]
    fn test_detect_asset_kind_svg_with_utf8_bom() {
        let svg = b"\xEF\xBB\xBF<svg xmlns=\"http://www.w3.org/2000/svg\"></svg>";
        assert!(matches!(AssetKind::detect(svg), AssetKind::Svg));
    }

    #[test]
    fn test_detect_asset_kind_empty() {
        assert!(matches!(AssetKind::detect(&[]), AssetKind::Unknown));
    }

    #[test]
    fn test_detect_asset_kind_unknown() {
        assert!(matches!(
            AssetKind::detect(b"not an image"),
            AssetKind::Unknown
        ));
    }

    #[test]
    fn image_render_new_initialises_defaults_and_with_node_id_overrides() {
        let data = Arc::new(MINIMAL_PNG.to_vec());
        let img = ImageRender::new(data.clone(), ImageFormat::Png, 100.0, 50.0);
        assert_eq!(img.width, 100.0);
        assert_eq!(img.height, 50.0);
        assert_eq!(img.opacity, 1.0);
        assert!(img.visible);
        assert!(img.node_id.is_none());
        let img2 = img.with_node_id(Some(42));
        assert_eq!(img2.node_id, Some(42));
        let img3 = img2.with_node_id(None);
        assert!(img3.node_id.is_none());
    }

    // --- to_krilla_image: cover the Jpeg and Gif match arms ---

    #[test]
    fn to_krilla_image_jpeg_invalid_returns_err() {
        // Exercises the Jpeg arm and the map_err closure.
        let data: krilla::Data = b"not a jpeg".to_vec().into();
        assert!(ImageFormat::Jpeg.to_krilla_image(data).is_err());
    }

    #[test]
    fn to_krilla_image_gif_invalid_returns_err() {
        // Exercises the Gif arm and the map_err closure.
        let data: krilla::Data = b"not a gif".to_vec().into();
        assert!(ImageFormat::Gif.to_krilla_image(data).is_err());
    }

    #[test]
    fn to_krilla_image_png_invalid_returns_err() {
        // Exercises the Png arm error path.
        let data: krilla::Data = b"not a png".to_vec().into();
        assert!(ImageFormat::Png.to_krilla_image(data).is_err());
    }

    // --- JPEG decode_dimensions edge cases ---

    #[test]
    fn jpeg_decode_dimensions_too_short_returns_none() {
        // Hits the `data.len() < 2` early-return.
        assert_eq!(ImageRender::decode_dimensions(&[], ImageFormat::Jpeg), None);
        assert_eq!(
            ImageRender::decode_dimensions(&[0xFF], ImageFormat::Jpeg),
            None
        );
    }

    #[test]
    fn jpeg_decode_dimensions_non_0xff_byte_skipped() {
        // A garbage non-0xFF byte at index 2 is skipped (i += 1), then a valid
        // SOF0 marker is found and dimensions are read correctly.
        let data: &[u8] = &[
            0xFF, 0xD8, // SOI
            0xAA, // non-0xFF garbage — triggers `i += 1; continue`
            0xFF, 0xC0, // SOF0
            0x00, 0x0B, // segment length = 11
            0x08, // sample precision
            0x00, 0x02, // height = 2
            0x00, 0x03, // width = 3
            0x01, // components
        ];
        assert_eq!(
            ImageRender::decode_dimensions(data, ImageFormat::Jpeg),
            Some((3, 2))
        );
    }

    #[test]
    fn jpeg_decode_dimensions_truncated_at_sof_returns_none() {
        // SOF0 marker is found but fewer than 7 payload bytes remain.
        let data: &[u8] = &[
            0xFF, 0xD8, // SOI (i = 2)
            0xFF, 0xC0, // SOF0 (i → 4)
            0x00, // only 1 byte left; need i+7=11, have 5 → None
        ];
        assert_eq!(
            ImageRender::decode_dimensions(data, ImageFormat::Jpeg),
            None
        );
    }

    #[test]
    fn jpeg_decode_dimensions_truncated_before_seg_len_returns_none() {
        // Non-SOF marker at end of data; no bytes remain for the segment length.
        // Hits the `i + 1 >= data.len()` early-return.
        let data: &[u8] = &[
            0xFF, 0xD8, // SOI (i = 2)
            0xFF, 0xE0, // APP0 (i → 4); need data[4..6] for seg_len but len = 4
        ];
        assert_eq!(
            ImageRender::decode_dimensions(data, ImageFormat::Jpeg),
            None
        );
    }

    #[test]
    fn jpeg_decode_dimensions_seg_len_too_small_returns_none() {
        // Segment length field value < 2 — hits the `seg_len < 2` guard.
        let data: &[u8] = &[
            0xFF, 0xD8, // SOI (i = 2)
            0xFF, 0xE0, // APP0 (i → 4)
            0x00, 0x01, // seg_len = 1 < 2 → None
        ];
        assert_eq!(
            ImageRender::decode_dimensions(data, ImageFormat::Jpeg),
            None
        );
    }

    #[test]
    fn jpeg_decode_dimensions_no_sof_marker_returns_none() {
        // A complete non-SOF segment is consumed; the while loop exits naturally
        // without having found a SOF marker, so the function returns None.
        let data: &[u8] = &[
            0xFF, 0xD8, // SOI (i = 2)
            0xFF, 0xE0, // APP0 (i → 4)
            0x00, 0x04, // seg_len = 4 (i → 4 + 4 = 8)
            0x00,
            0x00, // APP0 payload (2 bytes)
                  // i = 8; 8+1 = 9 >= data.len()=8 → loop exits → None
        ];
        assert_eq!(
            ImageRender::decode_dimensions(data, ImageFormat::Jpeg),
            None
        );
    }

    // --- GIF edge case ---

    #[test]
    fn gif_decode_dimensions_too_short_returns_none() {
        // data.len() < 10 hits the early-return guard.
        assert_eq!(
            ImageRender::decode_dimensions(b"GIF89a\x01\x00\x01", ImageFormat::Gif),
            None
        );
    }

    // --- AssetKind::detect whitespace-skipping path ---

    #[test]
    fn detect_svg_with_leading_whitespace() {
        // ASCII whitespace before `<svg` exercises the `slice = rest` branch in
        // the whitespace-skipping loop inside AssetKind::detect.
        let svg = b"  \n\t<svg xmlns=\"http://www.w3.org/2000/svg\"></svg>";
        assert!(matches!(AssetKind::detect(svg), AssetKind::Svg));
    }

    // --- validate / krilla_image (F32a) ---

    const JPEG: &[u8] = include_bytes!("../tests/fixtures/images/gradient64.jpg");
    const JPEG_PROGRESSIVE: &[u8] =
        include_bytes!("../tests/fixtures/images/gradient64-progressive.jpg");
    const JPEG_GRAY: &[u8] = include_bytes!("../tests/fixtures/images/gradient64-gray.jpg");
    const JPEG_CMYK: &[u8] = include_bytes!("../tests/fixtures/images/gradient64-cmyk.jpg");
    const JPEG_SUBSAMPLED: &[u8] =
        include_bytes!("../tests/fixtures/images/gradient64-subsampled.jpg");
    const GIF: &[u8] = include_bytes!("../tests/fixtures/images/gradient32.gif");

    fn png(w: u32, h: u32) -> Vec<u8> {
        let img = ::image::RgbImage::from_fn(w, h, |x, y| ::image::Rgb([x as u8, y as u8, 7]));
        let mut out = Vec::new();
        img.write_to(
            &mut std::io::Cursor::new(&mut out),
            ::image::ImageFormat::Png,
        )
        .unwrap();
        out
    }

    /// `png` with its first IDAT chunk's payload cut to `keep` bytes (CRC recomputed): a well-formed
    /// container with too few pixels.
    fn png_with_short_idat(png: &[u8], keep: usize) -> Vec<u8> {
        let mut out = png[..8].to_vec();
        let mut i = 8;
        let mut cut = false;
        while i + 12 <= png.len() {
            let len = u32::from_be_bytes(png[i..i + 4].try_into().unwrap()) as usize;
            let kind = &png[i + 4..i + 8];
            let data = &png[i + 8..i + 8 + len];
            if kind == b"IDAT" && !cut {
                cut = true;
                let data = &data[..keep.min(len)];
                out.extend_from_slice(&(data.len() as u32).to_be_bytes());
                let mut body = kind.to_vec();
                body.extend_from_slice(data);
                out.extend_from_slice(&body);
                out.extend_from_slice(&crc32(&body).to_be_bytes());
            } else if kind != b"IDAT" {
                out.extend_from_slice(&png[i..i + 12 + len]);
            }
            i += 12 + len;
        }
        out
    }

    fn crc32(data: &[u8]) -> u32 {
        let mut crc = !0u32;
        for &byte in data {
            crc ^= u32::from(byte);
            for _ in 0..8 {
                crc = if crc & 1 != 0 {
                    (crc >> 1) ^ 0xEDB8_8320
                } else {
                    crc >> 1
                };
            }
        }
        !crc
    }

    fn is_corrupt(r: Result<AssetKind, ImageError>) -> bool {
        matches!(r, Err(ImageError::Corrupt(_)))
    }

    #[test]
    fn validate_accepts_what_krilla_draws() {
        for (what, data, format) in [
            ("png", png(40, 30), ImageFormat::Png),
            ("jpeg", JPEG.to_vec(), ImageFormat::Jpeg),
            (
                "progressive jpeg",
                JPEG_PROGRESSIVE.to_vec(),
                ImageFormat::Jpeg,
            ),
            ("gray jpeg", JPEG_GRAY.to_vec(), ImageFormat::Jpeg),
            ("cmyk jpeg", JPEG_CMYK.to_vec(), ImageFormat::Jpeg),
            (
                "subsampled jpeg",
                JPEG_SUBSAMPLED.to_vec(),
                ImageFormat::Jpeg,
            ),
            ("gif", GIF.to_vec(), ImageFormat::Gif),
        ] {
            assert_eq!(validate(&data), Ok(AssetKind::Raster(format)), "{what}");
            let data = Arc::new(data);
            assert!(
                krilla_image(&ImageCache::default(), &data, format).is_some(),
                "{what}"
            );
        }
    }

    #[test]
    fn validate_accepts_svg_and_rejects_broken_svg() {
        let svg = br#"<svg xmlns="http://www.w3.org/2000/svg" width="10" height="10"><rect width="5" height="5"/></svg>"#;
        assert_eq!(validate(svg), Ok(AssetKind::Svg));
        assert!(is_corrupt(validate(
            br#"<svg xmlns="http://www.w3.org/2000/svg" width="100"><rect"#
        )));
    }

    #[test]
    fn validate_rejects_truncated_pngs() {
        let full = png(64, 64);
        assert_eq!(validate(&full), Ok(AssetKind::Raster(ImageFormat::Png)));
        // The file ends inside the pixel data.
        assert!(is_corrupt(validate(&full[..full.len() / 2])));
        // Well-formed chunks, half the pixel data.
        let idat = png_with_short_idat(&full, usize::MAX);
        assert_eq!(validate(&idat), Ok(AssetKind::Raster(ImageFormat::Png)));
        assert!(is_corrupt(validate(&png_with_short_idat(&full, 50))));
        // Only the header.
        assert!(is_corrupt(validate(&full[..40])));
    }

    #[test]
    fn validate_rejects_truncated_gif_and_empty_gif() {
        // Krilla decodes a GIF at `finish()` and fails the document, so a truncated one is refused (a
        // JPEG is not: see `damaged_jpegs_are_passed_through_like_krilla_does`).
        assert!(is_corrupt(validate(&GIF[..GIF.len() / 2])));
        assert!(is_corrupt(validate(MINIMAL_GIF)));
    }

    #[test]
    fn validate_refuses_huge_headers_before_decoding() {
        // 20000 x 20000 RGB would be 1.2 GB of output; the IDAT holds a few bytes.
        let huge = png_with_short_idat(&png(16, 16), usize::MAX);
        let mut huge = huge;
        huge[16..20].copy_from_slice(&20000u32.to_be_bytes());
        huge[20..24].copy_from_slice(&20000u32.to_be_bytes());
        // The IHDR CRC no longer matches; `png` checks it, so recompute.
        let crc = crc32(&huge[12..29]);
        huge[29..33].copy_from_slice(&crc.to_be_bytes());
        assert_eq!(
            validate(&huge),
            Err(ImageError::TooLarge { w: 20000, h: 20000 })
        );
        // A JPEG is not subject to the limit: see `a_jpeg_above_the_pixel_cap_is_checked_by_header_only`.
        let mut jpeg = JPEG.to_vec();
        let sof = jpeg.windows(2).position(|w| w == [0xff, 0xc0]).unwrap();
        // Wider than zune-jpeg decodes at all: refused as well (Krilla's header check says the same).
        jpeg[sof + 5..sof + 9].copy_from_slice(&[0xea, 0x60, 0xea, 0x60]);
        assert!(validate(&jpeg).is_err());
    }

    #[test]
    fn the_pixel_limit_admits_a_600_dpi_a3_scan() {
        // 69.6 Mpx; 8193 x 8192 is what main embedded and the 64 Mi limit refused.
        assert_eq!(check_size(8193, 8192), Ok(()));
        assert_eq!(check_size(7016, 9933), Ok(()));
        assert_eq!(check_size(11585, 11585), Ok(()));
        assert!(check_size(11586, 11586).is_err());
    }

    #[test]
    fn validate_rejects_garbage_and_empty() {
        assert_eq!(
            validate(b"not an image at all"),
            Err(ImageError::Unsupported)
        );
        assert_eq!(validate(b""), Err(ImageError::Unsupported));
    }

    #[test]
    fn krilla_image_skips_what_does_not_validate_and_remembers_the_verdict() {
        let cache = ImageCache::default();
        let full = png(64, 64);
        let bad = Arc::new(full[..full.len() / 2].to_vec());
        let good = Arc::new(full);
        for _ in 0..2 {
            assert!(krilla_image(&cache, &bad, ImageFormat::Png).is_none());
            assert!(krilla_image(&cache, &good, ImageFormat::Png).is_some());
        }
        assert_eq!(
            cache.0.borrow().len(),
            2,
            "one entry per image, not per draw"
        );
    }

    /// `data` with `n` zero bytes between its first (APP0) segment and the next marker.
    fn with_stray_bytes(data: &[u8], n: usize) -> Vec<u8> {
        let app0_end = 2 + 2 + usize::from(u16::from_be_bytes([data[4], data[5]]));
        let mut out = data[..app0_end].to_vec();
        out.extend(std::iter::repeat_n(0u8, n));
        out.extend_from_slice(&data[app0_end..]);
        out
    }

    #[test]
    fn damaged_jpegs_are_passed_through_like_krilla_does() {
        // Krilla never decodes a JPEG; Chrome and viewers draw what is there. Only a JPEG that no
        // decoder can read at all is refused; one that decodes non-strictly is accepted.
        let cut = |data: &[u8], keep: f32| {
            let sos = data.windows(2).position(|w| w == [0xff, 0xda]).unwrap();
            data[..sos + ((data.len() - sos) as f32 * keep) as usize].to_vec()
        };
        for (what, data) in [
            ("stray header bytes", with_stray_bytes(JPEG, 3)),
            ("stray bytes before APP", with_stray_bytes(JPEG, 6)),
            ("cmyk with stray bytes", with_stray_bytes(JPEG_CMYK, 40)),
            ("truncated at 99 %", cut(JPEG, 0.99)),
            ("truncated at 50 %", cut(JPEG, 0.5)),
            ("progressive truncated", cut(JPEG_PROGRESSIVE, 0.6)),
            ("no EOI", JPEG[..JPEG.len() - 2].to_vec()),
        ] {
            assert_eq!(
                validate(&data),
                Ok(AssetKind::Raster(ImageFormat::Jpeg)),
                "{what}"
            );
            let data = Arc::new(data);
            assert!(
                krilla_image(&ImageCache::default(), &data, ImageFormat::Jpeg).is_some(),
                "{what}"
            );
        }
    }

    #[test]
    fn jpegs_that_do_not_decode_at_all_are_refused() {
        assert!(is_corrupt(validate(&JPEG[..10])));
        let mut broken = JPEG.to_vec();
        broken[2..30].fill(0x11);
        assert!(validate(&broken).is_err());
    }

    #[test]
    fn a_large_jpeg_is_checked_by_header_only() {
        // Krilla passes a JPEG through undecoded, so main embedded any JPEG whose header parses; decoding
        // 72 Mpx to learn whether it is damaged costs 430 ms and 226 MB. Over
        // MAX_JPEG_VALIDATION_PIXELS only the header is read. The probe is a JPEG cut in its scan, which a
        // decode reports as damaged, with the size in its header changed.
        let sos = JPEG.windows(2).position(|w| w == [0xff, 0xda]).unwrap();
        let cut = &JPEG[..sos + (JPEG.len() - sos) / 2];
        let sof = cut.windows(2).position(|w| w == [0xff, 0xc0]).unwrap();
        let claim = |w: u16, h: u16| {
            let mut jpeg = cut.to_vec();
            jpeg[sof + 5..sof + 7].copy_from_slice(&h.to_be_bytes());
            jpeg[sof + 7..sof + 9].copy_from_slice(&w.to_be_bytes());
            validate_detailed(&jpeg).unwrap().damaged
        };
        assert!(claim(64, 64).is_some(), "within the limit: decoded");
        assert!(claim(4000, 4000).is_some(), "16 Mpx is the limit itself");
        assert_eq!(claim(5000, 5000), None, "over it: header only");
        assert_eq!(claim(12000, 12000), None);
        // The PNG / GIF limit stays 128 Mi: see `validate_refuses_huge_headers_before_decoding`.
        const { assert!(MAX_IMAGE_PIXELS > MAX_JPEG_VALIDATION_PIXELS) };
    }

    #[test]
    fn header_size_note_is_silent_when_the_scan_and_the_decoder_agree() {
        assert_eq!(header_size_note(Some((64, 48)), (64, 48)), None);
    }

    #[test]
    fn header_size_note_names_both_sizes_when_they_differ_or_the_scan_failed() {
        let wrong = header_size_note(Some((64, 64)), (64, 48)).unwrap();
        assert!(
            wrong.contains("64 \u{d7} 64") && wrong.contains("64 \u{d7} 48"),
            "{wrong}"
        );
        let none = header_size_note(None, (64, 48)).unwrap();
        assert!(
            none.contains("none") && none.contains("decoder: 64 \u{d7} 48"),
            "{none}"
        );
        assert!(none.contains("both sizes"), "{none}");
    }

    #[test]
    fn jpeg_with_fill_bytes_is_valid_and_not_damaged() {
        let sof = JPEG.windows(2).position(|w| w == [0xff, 0xdb]).unwrap();
        let mut filled = JPEG.to_vec();
        filled.splice(sof..sof, [0xFF; 7]);
        let v = validate_detailed(&filled).unwrap();
        assert_eq!(v.kind, AssetKind::Raster(ImageFormat::Jpeg));
        assert_eq!(v.damaged, None);
    }

    #[test]
    fn validate_detailed_says_when_a_jpeg_is_embedded_although_damaged() {
        let intact = validate_detailed(JPEG).unwrap();
        assert_eq!(intact.damaged, None);
        let sos = JPEG.windows(2).position(|w| w == [0xff, 0xda]).unwrap();
        let cut = validate_detailed(&JPEG[..sos + (JPEG.len() - sos) / 2]).unwrap();
        assert_eq!(cut.kind, AssetKind::Raster(ImageFormat::Jpeg));
        assert!(cut.damaged.is_some_and(|why| !why.is_empty()));
        let stray = validate_detailed(&with_stray_bytes(JPEG, 3)).unwrap();
        assert!(stray.damaged.is_some_and(|why| why.contains("Extra bytes")));
    }
}
