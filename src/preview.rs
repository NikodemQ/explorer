//! Turns a file into styled lines for the preview column. Reads are bounded, so a huge or hostile
//! file costs at most a fixed amount of memory and time.

use std::{
    fs::{self, File},
    io::{self, Read},
    path::Path,
    sync::OnceLock,
    time::SystemTime,
};

use std::sync::atomic::{AtomicU32, Ordering};

use chrono::{DateTime, Local};
use image::ImageDecoder;
use syntect::{
    easy::HighlightLines,
    highlighting::{Theme, ThemeSet},
    parsing::SyntaxSet,
    util::LinesWithEndings,
};

use crate::{
    imageview::ImageData,
    theme::{DIM, FG},
};

pub const HEAD_BYTES: usize = 64 * 1024;
pub const MAX_LINES: usize = 500;
pub const MAX_LINE_CHARS: usize = 1000;
const MAX_ARCHIVE_ENTRIES: usize = 500;
const MAX_ARCHIVE_STREAM: u64 = 256 * 1024 * 1024;
const HEX_BYTES: usize = 256;
const DIR_COLOR: Rgb = (0x5f, 0xb3, 0xff);

pub type Rgb = (u8, u8, u8);

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Span {
    pub text: String,
    pub color: Rgb,
    pub bold: bool,
    pub italic: bool,
}

impl Span {
    fn new(text: impl Into<String>, color: Rgb) -> Span {
        Span {
            text: text.into(),
            color,
            bold: false,
            italic: false,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct Line(pub Vec<Span>);

impl Line {
    fn plain(text: impl Into<String>) -> Line {
        Line(vec![Span::new(text, FG)])
    }

    fn dim(text: impl Into<String>) -> Line {
        Line(vec![Span::new(text, DIM)])
    }

    pub fn text(&self) -> String {
        self.0.iter().map(|s| s.text.as_str()).collect()
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Content {
    /// A decoded picture to draw above the lines, for image files.
    pub image: Option<ImageData>,
    pub lines: Vec<Line>,
    /// Show line numbers in a gutter (source text, not dumps).
    pub numbered: bool,
}

impl Content {
    /// Roughly how much memory this preview holds.
    pub fn weight(&self) -> usize {
        let picture = self.image.as_ref().map_or(0, |i| i.0.as_bytes().len());
        let text: usize = self
            .lines
            .iter()
            .flat_map(|l| &l.0)
            .map(|s| s.text.len() + 16)
            .sum();
        picture + text
    }

    fn message(text: &str) -> Content {
        Content {
            image: None,
            lines: vec![Line::dim(text)],
            numbered: false,
        }
    }
}

/// Text if there is no NUL and the bytes are UTF-8. When `cut` the buffer may end mid-character.
pub fn looks_like_text(head: &[u8], cut: bool) -> bool {
    if head.contains(&0) {
        return false;
    }
    match std::str::from_utf8(head) {
        Ok(_) => true,
        Err(e) => cut && e.error_len().is_none(),
    }
}

pub fn build(path: &Path) -> io::Result<Content> {
    let meta = fs::metadata(path)?;
    if !meta.is_file() {
        return Ok(Content::message("not a regular file, so it is not read"));
    }
    if let Some(kind) = archive_kind(path) {
        return archive_listing(path, kind, &meta);
    }
    let mut head = Vec::with_capacity(HEAD_BYTES.min(meta.len() as usize));
    File::open(path)?
        .take(HEAD_BYTES as u64)
        .read_to_end(&mut head)?;
    let cut = meta.len() > head.len() as u64;
    if looks_like_text(&head, cut) {
        return Ok(text_content(&head, cut, meta.len(), path));
    }
    let is_image = infer::get(&head).is_some_and(|k| k.mime_type().starts_with("image/"));
    if is_image && meta.len() <= MAX_IMAGE_BYTES {
        match decode_image(path) {
            Ok(image) => return Ok(image_content(path, &head, &meta, image)),
            Err(e) => {
                let mut content = binary_content(&head, cut, &meta);
                content
                    .lines
                    .insert(0, Line::dim(format!("cannot show the picture: {e}")));
                return Ok(content);
            }
        }
    }
    Ok(binary_content(&head, cut, &meta))
}

fn text_content(head: &[u8], cut: bool, total: u64, path: &Path) -> Content {
    let text = String::from_utf8_lossy(head);
    let text = text.trim_end_matches('\u{fffd}');
    let mut source: Vec<String> = text.lines().take(MAX_LINES + 1).map(sanitize).collect();
    let more_lines = source.len() > MAX_LINES;
    source.truncate(MAX_LINES);
    let mut lines = highlight(&source, path);
    if more_lines || cut {
        lines.push(Line::dim(format!(
            "… truncated, {} in total",
            human_size(total)
        )));
    }
    Content {
        image: None,
        lines,
        numbered: true,
    }
}

/// Tabs become spaces and control characters become a visible dot, so nothing can move the terminal cursor.
fn sanitize(line: &str) -> String {
    let mut out = String::new();
    for (count, c) in line.chars().enumerate() {
        if count >= MAX_LINE_CHARS {
            out.push('…');
            break;
        }
        match c {
            '\t' => out.push_str("    "),
            c if c.is_control() => out.push('·'),
            c => out.push(c),
        }
    }
    out
}

struct Highlighter {
    syntaxes: SyntaxSet,
    theme: Theme,
}

fn highlighter() -> &'static Highlighter {
    static CELL: OnceLock<Highlighter> = OnceLock::new();
    CELL.get_or_init(|| {
        let mut themes = ThemeSet::load_defaults().themes;
        Highlighter {
            syntaxes: SyntaxSet::load_defaults_newlines(),
            theme: themes.remove("base16-ocean.dark").expect("bundled theme"),
        }
    })
}

pub(crate) fn highlight(source: &[String], path: &Path) -> Vec<Line> {
    let plain = || {
        source
            .iter()
            .map(|l| Line::plain(l.clone()))
            .collect::<Vec<_>>()
    };
    let h = highlighter();
    let by_extension = path
        .extension()
        .and_then(|e| e.to_str())
        .and_then(|e| h.syntaxes.find_syntax_by_extension(e));
    let by_name = path
        .file_name()
        .and_then(|n| n.to_str())
        .and_then(|n| h.syntaxes.find_syntax_by_extension(n));
    let first = source.first().map(String::as_str).unwrap_or_default();
    let Some(syntax) = by_extension
        .or(by_name)
        .or_else(|| h.syntaxes.find_syntax_by_first_line(first))
    else {
        return plain();
    };
    let mut state = HighlightLines::new(syntax, &h.theme);
    let mut out = Vec::with_capacity(source.len());
    for raw in LinesWithEndings::from(&source.join("\n")) {
        let Ok(ranges) = state.highlight_line(raw, &h.syntaxes) else {
            return plain();
        };
        let spans = ranges
            .into_iter()
            .map(|(style, text)| Span {
                text: text.trim_end_matches('\n').to_string(),
                color: (style.foreground.r, style.foreground.g, style.foreground.b),
                bold: style
                    .font_style
                    .contains(syntect::highlighting::FontStyle::BOLD),
                italic: style
                    .font_style
                    .contains(syntect::highlighting::FontStyle::ITALIC),
            })
            .filter(|s| !s.text.is_empty())
            .collect();
        out.push(Line(spans));
    }
    out
}

pub const MAX_IMAGE_BYTES: u64 = 64 * 1024 * 1024;
/// Pictures are never kept larger than this on either side.
const MAX_IMAGE_SIDE: u32 = 2048;

static TARGET_WIDTH: AtomicU32 = AtomicU32::new(MAX_IMAGE_SIDE);
static TARGET_HEIGHT: AtomicU32 = AtomicU32::new(MAX_IMAGE_SIDE);

/// The most pixels a picture is ever shown with, from the terminal size and drawing method.
/// Pictures are decoded and shrunk to fit this, so a 40 megapixel photo costs no more than the screen.
pub fn set_image_target(width: u32, height: u32) {
    TARGET_WIDTH.store(width.clamp(64, MAX_IMAGE_SIDE), Ordering::Relaxed);
    TARGET_HEIGHT.store(height.clamp(64, MAX_IMAGE_SIDE), Ordering::Relaxed);
}

fn image_target() -> (u32, u32) {
    (
        TARGET_WIDTH.load(Ordering::Relaxed),
        TARGET_HEIGHT.load(Ordering::Relaxed),
    )
}

/// How much of a picture's header is read when looking for the camera's own preview. Cameras put
/// their previews in the first application segments, comfortably inside this.
const PREVIEW_SCAN_BYTES: u64 = 256 * 1024;

/// Whether a picture of this size can fill a `width` by `height` box without being enlarged.
/// Fitting keeps the shape, so covering the box in either direction is enough.
fn covers(size: (u32, u32), width: u32, height: u32) -> bool {
    size.0 >= width || size.1 >= height
}

/// Walks the segments of a JPEG up to the start of its image data. Everything before that is
/// header, and that is where cameras keep their previews.
fn header_end(data: &[u8]) -> usize {
    let mut at = 2usize;
    while at + 4 <= data.len() {
        if data[at] != 0xFF {
            at += 1;
            continue;
        }
        let marker = data[at + 1];
        // Padding, and markers that carry no length.
        if marker == 0xFF || marker == 0x01 || (0xD0..=0xD8).contains(&marker) {
            at += 2;
            continue;
        }
        // Start of scan: the compressed picture begins here.
        if marker == 0xDA {
            return at;
        }
        let len = usize::from(u16::from_be_bytes([data[at + 2], data[at + 3]]));
        if len < 2 {
            return at;
        }
        at += 2 + len;
    }
    data.len()
}

/// The width and height a JPEG declares in its frame header.
fn jpeg_size(data: &[u8]) -> Option<(u32, u32)> {
    let mut at = 2usize;
    while at + 9 <= data.len() {
        if data[at] != 0xFF {
            at += 1;
            continue;
        }
        let marker = data[at + 1];
        if marker == 0xFF || marker == 0x01 || (0xD0..=0xD8).contains(&marker) {
            at += 2;
            continue;
        }
        if marker == 0xDA {
            return None;
        }
        let len = usize::from(u16::from_be_bytes([data[at + 2], data[at + 3]]));
        if len < 2 {
            return None;
        }
        // The frame headers that carry the picture's size, baseline through progressive.
        if (0xC0..=0xCF).contains(&marker) && !matches!(marker, 0xC4 | 0xC8 | 0xCC) {
            let h = u32::from(u16::from_be_bytes([data[at + 5], data[at + 6]]));
            let w = u32::from(u16::from_be_bytes([data[at + 7], data[at + 8]]));
            return Some((w, h));
        }
        at += 2 + len;
    }
    None
}

/// The largest finished picture stored in a JPEG's header. Cameras keep a small thumbnail there and
/// usually a larger preview beside it, each a complete JPEG of the same photo.
fn embedded_preview(head: &[u8]) -> Option<&[u8]> {
    let end = header_end(head);
    let mut best: Option<(&[u8], u32)> = None;
    let mut at = 2usize;
    while at + 3 <= end {
        if head[at..at + 3] != *b"\xFF\xD8\xFF" {
            at += 1;
            continue;
        }
        // A preview that runs past the bytes read is no use, so stop rather than guess.
        let Some(stop) = head[at..end]
            .windows(2)
            .position(|w| w == b"\xFF\xD9")
            .map(|e| at + e + 2)
        else {
            break;
        };
        let candidate = &head[at..stop];
        if let Some((w, h)) = jpeg_size(candidate)
            && best.is_none_or(|(_, pixels)| w * h > pixels)
        {
            best = Some((candidate, w * h));
        }
        at = stop;
    }
    best.map(|(bytes, _)| bytes)
}

/// Reads a picture's header and decodes the preview the camera stored in it, with the orientation
/// the photo asks for. About a millisecond, where the real image data costs hundreds.
fn header_preview(path: &Path) -> Option<(Vec<u8>, image::DynamicImage)> {
    let mut head = Vec::with_capacity(PREVIEW_SCAN_BYTES as usize);
    File::open(path)
        .ok()?
        .take(PREVIEW_SCAN_BYTES)
        .read_to_end(&mut head)
        .ok()?;
    // Only JPEG carries these, and scanning anything else is wasted work.
    if !head.starts_with(b"\xFF\xD8\xFF") {
        return None;
    }
    let mut image = image::load_from_memory(embedded_preview(&head)?).ok()?;
    // The orientation flag describes the photo, and the preview is stored the same way round.
    if let Some(orientation) = image::ImageReader::new(io::Cursor::new(&head))
        .with_guessed_format()
        .ok()
        .and_then(|r| r.into_decoder().ok())
        .and_then(|mut d| d.orientation().ok())
    {
        image.apply_orientation(orientation);
    }
    Some((head, image))
}

/// The camera's own preview, shown at once while the real picture is still being decoded. `None`
/// when the file has no preview, or when it has one big enough that [`build`] is already instant.
pub fn build_quick(path: &Path) -> Option<Content> {
    let meta = fs::metadata(path).ok()?;
    if !meta.is_file() || meta.len() > MAX_IMAGE_BYTES {
        return None;
    }
    let (width, height) = image_target();
    let (head, image) = header_preview(path)?;
    if covers((image.width(), image.height()), width, height) {
        return None;
    }
    let image = ImageData(std::sync::Arc::new(image));
    Some(image_content(path, &head, &meta, image))
}

fn decode_image(path: &Path) -> Result<ImageData, String> {
    let (width, height) = image_target();
    // A preview the camera already made beats decoding forty megapixels, when it is big enough.
    if let Some((_, image)) = header_preview(path)
        && covers((image.width(), image.height()), width, height)
    {
        return Ok(ImageData(std::sync::Arc::new(
            image.thumbnail(width, height),
        )));
    }
    let reader = image::ImageReader::open(path)
        .and_then(|r| r.with_guessed_format())
        .map_err(|e| e.to_string())?;
    let is_jpeg = reader.format() == Some(image::ImageFormat::Jpeg);
    let mut decoder = reader.into_decoder().map_err(|e| e.to_string())?;
    let orientation = decoder
        .orientation()
        .unwrap_or(image::metadata::Orientation::NoTransforms);
    let scaled = if is_jpeg {
        decode_jpeg_scaled(path, width, height).ok()
    } else {
        None
    };
    let image = match scaled {
        Some(image) => image,
        None => {
            let mut limits = image::Limits::default();
            limits.max_image_width = Some(20_000);
            limits.max_image_height = Some(20_000);
            limits.max_alloc = Some(512 * 1024 * 1024);
            decoder.set_limits(limits).map_err(|e| e.to_string())?;
            image::DynamicImage::from_decoder(decoder).map_err(|e| e.to_string())?
        }
    };
    let mut image = if image.width() > width || image.height() > height {
        image.thumbnail(width, height)
    } else {
        image
    };
    image.apply_orientation(orientation);
    Ok(ImageData(std::sync::Arc::new(image)))
}

/// Decodes a JPEG straight at a reduced scale (1/2, 1/4 or 1/8), which skips most of the work
/// for big photos. `None`-worthy formats such as CMYK are left to the general decoder.
fn decode_jpeg_scaled(path: &Path, width: u32, height: u32) -> Result<image::DynamicImage, String> {
    use jpeg_decoder::PixelFormat;
    let file = std::io::BufReader::new(File::open(path).map_err(|e| e.to_string())?);
    let mut decoder = jpeg_decoder::Decoder::new(file);
    decoder.set_max_decoding_buffer_size(512 * 1024 * 1024);
    let clamp = |v: u32| v.min(u32::from(u16::MAX)) as u16;
    // The codec only divides by 2, 4 or 8 and picks the smallest result at least as big as asked.
    // Asking for two thirds lands within 2/3 to 4/3 of the target, which skips a costly shrink afterwards.
    let (w, h) = decoder
        .scale(clamp(width * 2 / 3), clamp(height * 2 / 3))
        .map_err(|e| e.to_string())?;
    let pixels = decoder.decode().map_err(|e| e.to_string())?;
    let format = decoder.info().ok_or("no image information")?.pixel_format;
    let (w, h) = (u32::from(w), u32::from(h));
    let image = match format {
        PixelFormat::RGB24 => {
            image::RgbImage::from_raw(w, h, pixels).map(image::DynamicImage::ImageRgb8)
        }
        PixelFormat::L8 => {
            image::GrayImage::from_raw(w, h, pixels).map(image::DynamicImage::ImageLuma8)
        }
        PixelFormat::L16 => {
            let high: Vec<u8> = pixels.as_chunks::<2>().0.iter().map(|c| c[0]).collect();
            image::GrayImage::from_raw(w, h, high).map(image::DynamicImage::ImageLuma8)
        }
        PixelFormat::CMYK32 => None,
    };
    image.ok_or_else(|| "unsupported pixel layout".to_string())
}

fn image_content(path: &Path, head: &[u8], meta: &fs::Metadata, image: ImageData) -> Content {
    let mut lines = vec![
        card("type", &describe_type(head)),
        card("size", &human_size(meta.len())),
    ];
    if let Ok(modified) = meta.modified() {
        lines.push(card("modified", &format_time(modified)));
    }
    lines.extend(
        photo_facts(path)
            .into_iter()
            .map(|(key, value)| card(key, &value)),
    );
    Content {
        image: Some(image),
        lines,
        numbered: false,
    }
}

/// What the camera recorded about a photo: which camera and lens, when, and how it was exposed.
/// Only the header is read, and a file without these simply has none.
fn photo_facts(path: &Path) -> Vec<(&'static str, String)> {
    use exif::{In, Tag, Value};
    let mut head = Vec::new();
    let read = File::open(path).and_then(|f| f.take(PREVIEW_SCAN_BYTES).read_to_end(&mut head));
    let Some(exif) = read.ok().and_then(|_| {
        exif::Reader::new()
            .read_from_container(&mut io::Cursor::new(head))
            .ok()
    }) else {
        return Vec::new();
    };
    let value = |tag| exif.get_field(tag, In::PRIMARY).map(|f| &f.value);
    let text = |tag| match value(tag)? {
        Value::Ascii(parts) => {
            let joined = parts
                .iter()
                .map(|p| {
                    String::from_utf8_lossy(p)
                        .trim_matches(['\0', ' '])
                        .to_string()
                })
                .filter(|p| !p.is_empty())
                .collect::<Vec<_>>()
                .join(" ");
            (!joined.is_empty()).then_some(joined)
        }
        _ => None,
    };
    let number = |tag| match value(tag)? {
        Value::Rational(r) => r.first().filter(|r| r.denom != 0).map(|r| r.to_f64()),
        other => other.get_uint(0).map(f64::from),
    };
    let mut facts = Vec::new();

    let make = text(Tag::Make);
    let model = text(Tag::Model);
    let camera = match (make, model) {
        // Most cameras repeat the maker in the model name.
        (Some(make), Some(model)) if model.to_lowercase().starts_with(&make.to_lowercase()) => {
            Some(model)
        }
        (Some(make), Some(model)) => Some(format!("{make} {model}")),
        (make, model) => make.or(model),
    };
    facts.extend(camera.map(|c| ("camera", c)));
    facts.extend(text(Tag::LensModel).map(|l| ("lens", l)));
    if let Some(taken) = exif.get_field(Tag::DateTimeOriginal, In::PRIMARY) {
        facts.push(("taken", taken.display_value().to_string()));
    }

    let mut exposure = Vec::new();
    if let Some(Value::Rational(r)) = value(Tag::ExposureTime)
        && let Some(t) = r.first().filter(|t| t.num != 0 && t.denom != 0)
    {
        exposure.push(if t.num >= t.denom {
            format!("{} s", short(t.to_f64()))
        } else {
            format!("1/{} s", (f64::from(t.denom) / f64::from(t.num)).round())
        });
    }
    if let Some(f) = number(Tag::FNumber).filter(|f| *f > 0.0) {
        exposure.push(format!("f/{}", short(f)));
    }
    if let Some(iso) = number(Tag::PhotographicSensitivity).filter(|i| *i > 0.0) {
        exposure.push(format!("ISO {iso}"));
    }
    if let Some(focal) = number(Tag::FocalLength).filter(|f| *f > 0.0) {
        let mut text = format!("{} mm", short(focal));
        if let Some(full) =
            number(Tag::FocalLengthIn35mmFilm).filter(|f| *f > 0.0 && (f - focal).abs() >= 1.0)
        {
            text.push_str(&format!(" ({full} mm eq.)"));
        }
        exposure.push(text);
    }
    if !exposure.is_empty() {
        facts.push(("exposure", exposure.join("  ")));
    }
    if value(Tag::Flash)
        .and_then(|v| v.get_uint(0))
        .is_some_and(|f| f & 1 == 1)
    {
        facts.push(("flash", "fired".to_string()));
    }

    let degrees = |tag, reference, negative: &str| {
        let Value::Rational(dms) = value(tag)? else {
            return None;
        };
        let [d, m, s] = dms.get(..3)? else {
            return None;
        };
        if [d, m, s].iter().any(|r| r.denom == 0) {
            return None;
        }
        let deg = d.to_f64() + m.to_f64() / 60.0 + s.to_f64() / 3600.0;
        let side = text(reference).unwrap_or_default();
        Some((deg, side.eq_ignore_ascii_case(negative)))
    };
    if let (Some((lat, south)), Some((lon, west))) = (
        degrees(Tag::GPSLatitude, Tag::GPSLatitudeRef, "S"),
        degrees(Tag::GPSLongitude, Tag::GPSLongitudeRef, "W"),
    ) {
        facts.push((
            "location",
            format!(
                "{lat:.5}° {}, {lon:.5}° {}",
                if south { "S" } else { "N" },
                if west { "W" } else { "E" }
            ),
        ));
    }
    facts
}

/// A number with at most one decimal, and none when it is whole: 2.8, 8, 0.5.
fn short(value: f64) -> String {
    let rounded = (value * 10.0).round() / 10.0;
    if rounded.fract() == 0.0 {
        format!("{rounded:.0}")
    } else {
        format!("{rounded:.1}")
    }
}

fn binary_content(head: &[u8], cut: bool, meta: &fs::Metadata) -> Content {
    let mut lines = vec![
        card("type", &describe_type(head)),
        card(
            "size",
            &format!("{} ({} bytes)", human_size(meta.len()), meta.len()),
        ),
    ];
    if let Ok(modified) = meta.modified() {
        lines.push(card("modified", &format_time(modified)));
    }
    lines.push(card(
        "mode",
        &format_mode(std::os::unix::fs::MetadataExt::mode(meta)),
    ));
    lines.push(Line::default());
    for (i, chunk) in head.chunks(16).take(HEX_BYTES / 16).enumerate() {
        lines.push(hex_line(i * 16, chunk));
    }
    if cut || head.len() > HEX_BYTES {
        lines.push(Line::dim(format!("… first {HEX_BYTES} bytes shown")));
    }
    Content {
        image: None,
        lines,
        numbered: false,
    }
}

fn card(key: &str, value: &str) -> Line {
    Line(vec![
        Span::new(format!("{key:<9}"), DIM),
        Span::new(value, FG),
    ])
}

fn describe_type(head: &[u8]) -> String {
    let mut parts = Vec::new();
    match infer::get(head) {
        Some(kind) => parts.push(format!("{} ({})", kind.mime_type(), kind.extension())),
        None => parts.push("unknown binary data".to_string()),
    }
    if let Ok(size) = imagesize::blob_size(head) {
        parts.push(format!("{}×{} px", size.width, size.height));
    }
    parts.join(", ")
}

fn hex_line(offset: usize, chunk: &[u8]) -> Line {
    let mut hex = String::new();
    for i in 0..16 {
        match chunk.get(i) {
            Some(b) => hex.push_str(&format!("{b:02x} ")),
            None => hex.push_str("   "),
        }
        if i == 7 {
            hex.push(' ');
        }
    }
    let ascii: String = chunk
        .iter()
        .map(|&b| {
            if (0x20..0x7f).contains(&b) {
                b as char
            } else {
                '.'
            }
        })
        .collect();
    Line(vec![
        Span::new(format!("{offset:08x}  "), DIM),
        Span::new(hex, FG),
        Span::new(format!(" |{ascii}|"), DIM),
    ])
}

#[derive(Clone, Copy)]
enum ArchiveKind {
    Zip,
    Tar,
    TarGz,
}

fn archive_kind(path: &Path) -> Option<ArchiveKind> {
    let name = path.file_name()?.to_str()?.to_lowercase();
    let ends = |exts: &[&str]| exts.iter().any(|e| name.ends_with(e));
    if ends(&[".zip", ".jar", ".apk", ".epub", ".whl"]) {
        Some(ArchiveKind::Zip)
    } else if ends(&[".tar.gz", ".tgz"]) {
        Some(ArchiveKind::TarGz)
    } else if ends(&[".tar"]) {
        Some(ArchiveKind::Tar)
    } else {
        None
    }
}

fn archive_listing(path: &Path, kind: ArchiveKind, meta: &fs::Metadata) -> io::Result<Content> {
    let mut items: Vec<(String, u64, bool)> = Vec::new();
    let mut total = 0usize;
    let file = File::open(path)?;
    match kind {
        ArchiveKind::Zip => {
            let mut zip = zip::ZipArchive::new(file).map_err(io::Error::other)?;
            total = zip.len();
            for i in 0..zip.len().min(MAX_ARCHIVE_ENTRIES) {
                let entry = zip.by_index_raw(i).map_err(io::Error::other)?;
                items.push((entry.name().to_string(), entry.size(), entry.is_dir()));
            }
        }
        ArchiveKind::Tar | ArchiveKind::TarGz => {
            let reader: Box<dyn Read> = match kind {
                ArchiveKind::TarGz => {
                    Box::new(flate2::read::GzDecoder::new(file).take(MAX_ARCHIVE_STREAM))
                }
                _ => Box::new(file.take(MAX_ARCHIVE_STREAM)),
            };
            let mut tar = tar::Archive::new(reader);
            for entry in tar.entries()? {
                let entry = entry?;
                total += 1;
                if items.len() < MAX_ARCHIVE_ENTRIES {
                    let name = entry.path()?.to_string_lossy().into_owned();
                    items.push((name, entry.size(), entry.header().entry_type().is_dir()));
                }
            }
        }
    }
    let mut lines = vec![Line::dim(format!(
        "{} archive, {} entries, {}",
        match kind {
            ArchiveKind::Zip => "zip",
            ArchiveKind::Tar => "tar",
            ArchiveKind::TarGz => "tar.gz",
        },
        total,
        human_size(meta.len())
    ))];
    lines.push(Line::default());
    for (name, size, is_dir) in &items {
        let name = sanitize(name);
        let (shown, color) = if *is_dir {
            (name, DIR_COLOR)
        } else {
            (name, FG)
        };
        let size = if *is_dir {
            String::new()
        } else {
            human_size(*size)
        };
        lines.push(Line(vec![
            Span::new(format!("{size:>8}  "), DIM),
            Span::new(shown, color),
        ]));
    }
    if total > items.len() {
        lines.push(Line::dim(format!("… and {} more", total - items.len())));
    }
    Ok(Content {
        image: None,
        lines,
        numbered: false,
    })
}

pub fn human_size(bytes: u64) -> String {
    const UNITS: [&str; 5] = ["B", "KiB", "MiB", "GiB", "TiB"];
    let mut size = bytes as f64;
    let mut unit = 0;
    while size >= 1024.0 && unit < UNITS.len() - 1 {
        size /= 1024.0;
        unit += 1;
    }
    if unit == 0 {
        format!("{bytes} B")
    } else {
        format!("{size:.1} {}", UNITS[unit])
    }
}

fn format_time(time: SystemTime) -> String {
    DateTime::<Local>::from(time)
        .format("%Y-%m-%d %H:%M")
        .to_string()
}

pub fn format_mode(mode: u32) -> String {
    let mut out = String::from(if mode & 0o170000 == 0o040000 {
        "d"
    } else {
        "-"
    });
    for shift in [6, 3, 0] {
        let bits = (mode >> shift) & 7;
        out.push(if bits & 4 != 0 { 'r' } else { '-' });
        out.push(if bits & 2 != 0 { 'w' } else { '-' });
        out.push(if bits & 1 != 0 { 'x' } else { '-' });
    }
    format!("{out} ({:04o})", mode & 0o7777)
}

#[cfg(test)]
mod tests {
    use std::{io::Write, process::Command};

    use super::*;

    fn file(name: &str, bytes: &[u8]) -> (tempfile::TempDir, std::path::PathBuf) {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join(name);
        fs::write(&path, bytes).unwrap();
        (tmp, path)
    }

    fn text(content: &Content) -> Vec<String> {
        content.lines.iter().map(Line::text).collect()
    }

    #[test]
    fn source_files_are_numbered_and_syntax_colored() {
        let (_t, path) = file("main.rs", b"fn main() {\n    let x = 1; // hi\n}\n");
        let content = build(&path).unwrap();
        assert!(content.numbered);
        assert_eq!(text(&content), ["fn main() {", "    let x = 1; // hi", "}"]);
        let colors: std::collections::HashSet<_> = content
            .lines
            .iter()
            .flat_map(|l| l.0.iter().map(|s| s.color))
            .collect();
        assert!(
            colors.len() >= 3,
            "keywords, numbers and comments get their own colors: {colors:?}"
        );
    }

    #[test]
    fn unknown_extensions_are_plain_text_in_the_foreground_color() {
        let (_t, path) = file("notes.zzz", b"just words\nand more\n");
        let content = build(&path).unwrap();
        assert_eq!(text(&content), ["just words", "and more"]);
        assert!(
            content
                .lines
                .iter()
                .flat_map(|l| &l.0)
                .all(|s| s.color != (0, 0, 0))
        );
    }

    #[test]
    fn shebang_lines_pick_the_syntax_for_extensionless_scripts() {
        let (_t, path) = file("run", b"#!/bin/sh\necho \"hi\"\n");
        let content = build(&path).unwrap();
        let colors: std::collections::HashSet<_> = content
            .lines
            .iter()
            .flat_map(|l| l.0.iter().map(|s| s.color))
            .collect();
        assert!(colors.len() >= 2, "{colors:?}");
    }

    #[test]
    fn tabs_and_control_characters_are_made_safe() {
        let (_t, path) = file("t.txt", b"a\tb\x1b[31mred\x07\r\n");
        let content = build(&path).unwrap();
        let lines = text(&content);
        assert_eq!(lines, ["a    b·[31mred·"]);
        assert!(!lines[0].contains('\x1b'));
    }

    #[test]
    fn long_files_are_cut_at_the_line_cap_with_a_note() {
        let body: String = (0..MAX_LINES + 50).map(|i| format!("line {i}\n")).collect();
        let (_t, path) = file("long.txt", body.as_bytes());
        let content = build(&path).unwrap();
        assert_eq!(content.lines.len(), MAX_LINES + 1);
        assert!(
            content
                .lines
                .last()
                .unwrap()
                .text()
                .starts_with("… truncated")
        );
        assert_eq!(
            content.lines[MAX_LINES - 1].text(),
            format!("line {}", MAX_LINES - 1)
        );
    }

    /// A flat JPEG of one colour, the size asked for.
    fn jpeg(width: u32, height: u32, color: [u8; 3]) -> Vec<u8> {
        let picture = image::RgbImage::from_pixel(width, height, image::Rgb(color));
        let mut bytes = Vec::new();
        image::DynamicImage::ImageRgb8(picture)
            .write_to(&mut io::Cursor::new(&mut bytes), image::ImageFormat::Jpeg)
            .unwrap();
        bytes
    }

    /// A photo with a smaller picture of itself in an APP1 segment, the way a camera stores its
    /// preview. The two are different colours here, so tests can tell which one was decoded.
    fn photo_with_preview() -> Vec<u8> {
        let (outer, inner) = (
            jpeg(1200, 900, [220, 20, 20]),
            jpeg(600, 450, [20, 20, 220]),
        );
        let mut out = outer[..2].to_vec();
        out.extend_from_slice(b"\xFF\xE1");
        out.extend_from_slice(&(u16::try_from(inner.len() + 2).unwrap()).to_be_bytes());
        out.extend_from_slice(&inner);
        out.extend_from_slice(&outer[2..]);
        out
    }

    /// The image target is process-wide, so the tests that set it take turns.
    fn with_target<T>(width: u32, height: u32, body: impl FnOnce() -> T) -> T {
        static LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());
        let _held = LOCK.lock().unwrap_or_else(|e| e.into_inner());
        set_image_target(width, height);
        let out = body();
        set_image_target(MAX_IMAGE_SIDE, MAX_IMAGE_SIDE);
        out
    }

    fn is_blue(image: &ImageData) -> bool {
        let p = image.0.to_rgb8();
        let [r, _, b] = p.get_pixel(p.width() / 2, p.height() / 2).0;
        b > r
    }

    #[test]
    fn a_cameras_own_preview_is_used_when_it_fills_the_column() {
        let (_t, path) = file("photo.jpg", &photo_with_preview());
        // The 600x450 preview covers a 400x300 column, so the 1200x900 photo is never decoded.
        let image = with_target(400, 300, || build(&path).unwrap().image.unwrap());
        assert!(is_blue(&image), "the embedded preview, not the photo");
        assert!(
            with_target(400, 300, || build_quick(&path)).is_none(),
            "nothing to refine when the preview already fills the column"
        );
    }

    #[test]
    fn a_preview_too_small_for_the_column_is_shown_first_and_then_replaced() {
        let (_t, path) = file("photo.jpg", &photo_with_preview());
        let quick = with_target(1000, 800, || build_quick(&path))
            .expect("a first paint from the header")
            .image
            .unwrap();
        assert!(is_blue(&quick), "the embedded preview");
        assert_eq!((quick.0.width(), quick.0.height()), (600, 450));
        let full = with_target(1000, 800, || build(&path).unwrap().image.unwrap());
        assert!(!is_blue(&full), "the real photo replaces it");
        assert_eq!((full.0.width(), full.0.height()), (1000, 750));
    }

    #[test]
    fn a_picture_without_an_embedded_preview_has_no_first_paint() {
        let (_t, path) = file("plain.jpg", &jpeg(1200, 900, [220, 20, 20]));
        assert!(with_target(1000, 800, || build_quick(&path)).is_none());
        let (_t, path) = file("plain.png", &png(64, 32));
        assert!(with_target(1000, 800, || build_quick(&path)).is_none());
    }

    #[test]
    fn a_header_that_is_not_a_picture_is_left_alone() {
        assert_eq!(
            header_end(b"\xFF\xD8\xFF"),
            3,
            "stops at the end of what it has"
        );
        assert!(embedded_preview(b"not a jpeg at all").is_none());
        // A length that would step backwards must not loop for ever.
        assert!(embedded_preview(b"\xFF\xD8\xFF\xE1\x00\x00rest").is_none());
    }

    #[test]
    fn huge_files_read_only_the_head() {
        let (_t, path) = file("big.txt", &vec![b'x'; HEAD_BYTES * 3]);
        let content = build(&path).unwrap();
        assert_eq!(content.lines.len(), 2, "one capped line plus the note");
        assert!(content.lines[0].text().chars().count() <= MAX_LINE_CHARS + 1);
        assert!(
            content.lines[1].text().contains("192.0 KiB"),
            "{:?}",
            content.lines[1].text()
        );
    }

    #[test]
    fn a_multibyte_character_cut_by_the_head_limit_is_not_binary() {
        let mut bytes = vec![b'a'; HEAD_BYTES - 1];
        bytes.extend("ż".as_bytes());
        bytes.extend(b"tail");
        let (_t, path) = file("cut.txt", &bytes);
        assert!(build(&path).unwrap().numbered, "still shown as text");
    }

    #[test]
    fn binary_files_get_a_metadata_card_and_a_hex_dump() {
        let mut data = b"\x7fELF\x02\x01\x01\0".to_vec();
        data.extend((0..56u8).collect::<Vec<_>>());
        let (_t, path) = file("prog", &data);
        let content = build(&path).unwrap();
        assert!(!content.numbered);
        let lines = text(&content);
        assert!(lines[0].starts_with("type"), "{lines:?}");
        assert!(lines[0].contains("elf"), "{lines:?}");
        assert!(lines[1].contains("64 bytes"), "{lines:?}");
        assert!(
            lines.iter().any(|l| l.starts_with("00000000  7f 45 4c 46")),
            "{lines:#?}"
        );
        assert!(lines.iter().any(|l| l.contains("|.ELF")), "{lines:#?}");
    }

    #[test]
    fn images_report_their_dimensions() {
        let mut png = b"\x89PNG\r\n\x1a\n\0\0\0\rIHDR".to_vec();
        png.extend([0, 0, 2, 128, 0, 0, 1, 224, 8, 6, 0, 0, 0]);
        let (_t, path) = file("pic.png", &png);
        let lines = text(&build(&path).unwrap());
        assert!(
            lines
                .iter()
                .any(|l| l.contains("image/png") && l.contains("640×480 px")),
            "{lines:?}"
        );
    }

    #[test]
    fn zip_archives_list_their_entries() {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("a.zip");
        let mut zip = zip::ZipWriter::new(File::create(&path).unwrap());
        let options = zip::write::SimpleFileOptions::default();
        zip.add_directory("docs/", options).unwrap();
        zip.start_file("docs/readme.txt", options).unwrap();
        zip.write_all(b"hello").unwrap();
        zip.finish().unwrap();
        let lines = text(&build(&path).unwrap());
        assert_eq!(
            lines[0],
            "zip archive, 2 entries, 300 B"
                .replace("300", &fs::metadata(&path).unwrap().len().to_string())
        );
        assert!(lines.iter().any(|l| l.ends_with("docs/")), "{lines:#?}");
        assert!(
            lines
                .iter()
                .any(|l| l.contains("5 B") && l.ends_with("docs/readme.txt")),
            "{lines:#?}"
        );
    }

    #[test]
    fn tar_and_tar_gz_archives_list_their_entries() {
        let tmp = tempfile::tempdir().unwrap();
        let mut builder = tar::Builder::new(Vec::new());
        let mut header = tar::Header::new_gnu();
        header.set_size(3);
        header.set_cksum();
        builder
            .append_data(&mut header, "dir/file.txt", &b"abc"[..])
            .unwrap();
        let tar_bytes = builder.into_inner().unwrap();
        let plain = tmp.path().join("x.tar");
        fs::write(&plain, &tar_bytes).unwrap();
        let mut gz = flate2::write::GzEncoder::new(Vec::new(), flate2::Compression::default());
        gz.write_all(&tar_bytes).unwrap();
        let zipped = tmp.path().join("x.tar.gz");
        fs::write(&zipped, gz.finish().unwrap()).unwrap();
        for (path, label) in [(plain, "tar archive"), (zipped, "tar.gz archive")] {
            let lines = text(&build(&path).unwrap());
            assert!(lines[0].starts_with(label), "{lines:?}");
            assert!(
                lines
                    .iter()
                    .any(|l| l.ends_with("dir/file.txt") && l.contains("3 B")),
                "{lines:#?}"
            );
        }
    }

    #[test]
    fn a_corrupt_archive_is_an_error_not_a_panic() {
        let (_t, path) = file("bad.zip", b"this is not a zip file at all");
        assert!(build(&path).is_err());
    }

    #[test]
    fn a_fifo_is_never_opened() {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("pipe");
        assert!(
            Command::new("mkfifo")
                .arg(&path)
                .status()
                .unwrap()
                .success()
        );
        let content = build(&path).unwrap();
        assert_eq!(text(&content), ["not a regular file, so it is not read"]);
    }

    #[test]
    fn symlinks_show_the_target_and_missing_files_error() {
        let (t, path) = file("real.txt", b"target text\n");
        let link = t.path().join("link");
        std::os::unix::fs::symlink(&path, &link).unwrap();
        assert_eq!(text(&build(&link).unwrap()), ["target text"]);
        assert!(build(&t.path().join("nope")).is_err());
    }

    #[test]
    fn empty_files_preview_as_nothing() {
        let (_t, path) = file("empty", b"");
        let content = build(&path).unwrap();
        assert!(content.lines.is_empty());
    }

    #[test]
    fn sizes_and_modes_are_human_readable() {
        assert_eq!(human_size(0), "0 B");
        assert_eq!(human_size(1023), "1023 B");
        assert_eq!(human_size(1536), "1.5 KiB");
        assert_eq!(human_size(5 * 1024 * 1024), "5.0 MiB");
        assert_eq!(format_mode(0o100644), "-rw-r--r-- (0644)");
        assert_eq!(format_mode(0o040755), "drwxr-xr-x (0755)");
    }

    fn png(w: u32, h: u32) -> Vec<u8> {
        let img = image::RgbImage::from_pixel(w, h, image::Rgb([10, 200, 30]));
        let mut out = std::io::Cursor::new(Vec::new());
        image::DynamicImage::ImageRgb8(img)
            .write_to(&mut out, image::ImageFormat::Png)
            .unwrap();
        out.into_inner()
    }

    #[test]
    fn pictures_are_decoded_for_display_with_a_short_card() {
        let (_t, path) = file("pic.png", &png(64, 32));
        let content = build(&path).unwrap();
        let image = content.image.clone().expect("decoded");
        assert_eq!((image.0.width(), image.0.height()), (64, 32));
        let lines = text(&content);
        assert!(
            lines[0].contains("image/png") && lines[0].contains("64×32"),
            "{lines:?}"
        );
        assert!(
            lines.iter().all(|l| !l.starts_with("00000000")),
            "no hex dump for pictures"
        );
    }

    #[test]
    fn huge_pictures_are_shrunk_after_decoding() {
        let (_t, path) = file("wide.png", &png(4096, 100));
        let image = build(&path).unwrap().image.unwrap();
        assert_eq!(image.0.width(), MAX_IMAGE_SIDE);
        assert!(image.0.height() <= 100);
    }

    #[test]
    fn a_broken_picture_falls_back_to_the_card_and_says_why() {
        let mut bytes = png(8, 8);
        bytes.truncate(40);
        let (_t, path) = file("broken.png", &bytes);
        let content = build(&path).unwrap();
        assert!(content.image.is_none());
        assert!(
            text(&content)[0].starts_with("cannot show the picture"),
            "{:?}",
            text(&content)
        );
    }

    fn png_with_exif(fields: &[exif::Field]) -> Vec<u8> {
        use image::ImageEncoder;
        let mut writer = exif::experimental::Writer::new();
        for field in fields {
            writer.push_field(field);
        }
        let mut tiff = std::io::Cursor::new(Vec::new());
        writer.write(&mut tiff, false).unwrap();
        let mut out = Vec::new();
        let mut encoder = image::codecs::png::PngEncoder::new(&mut out);
        encoder.set_exif_metadata(tiff.into_inner()).unwrap();
        let pixels = vec![128u8; 16 * 8 * 3];
        encoder
            .write_image(&pixels, 16, 8, image::ExtendedColorType::Rgb8)
            .unwrap();
        out
    }

    #[test]
    fn a_photo_card_says_which_camera_took_it_and_how() {
        use exif::{Field, In, Rational, Tag, Value};
        let field = |tag, value| Field {
            tag,
            ifd_num: In::PRIMARY,
            value,
        };
        let ascii = |s: &str| Value::Ascii(vec![s.as_bytes().to_vec()]);
        let rational = |num, denom| Value::Rational(vec![Rational { num, denom }]);
        let dms = |d, m, s| {
            Value::Rational(vec![
                Rational { num: d, denom: 1 },
                Rational { num: m, denom: 1 },
                Rational { num: s, denom: 100 },
            ])
        };
        let (_t, path) = file(
            "shot.png",
            &png_with_exif(&[
                field(Tag::Make, ascii("FUJIFILM")),
                field(Tag::Model, ascii("X-T5")),
                field(Tag::LensModel, ascii("XF35mmF1.4 R")),
                field(Tag::DateTimeOriginal, ascii("2026:05:01 18:04:09")),
                field(Tag::ExposureTime, rational(1, 250)),
                field(Tag::FNumber, rational(28, 10)),
                field(Tag::PhotographicSensitivity, Value::Short(vec![400])),
                field(Tag::FocalLength, rational(35, 1)),
                field(Tag::FocalLengthIn35mmFilm, Value::Short(vec![53])),
                field(Tag::Flash, Value::Short(vec![0x10])),
                field(Tag::GPSLatitudeRef, ascii("N")),
                field(Tag::GPSLatitude, dms(52, 13, 4700)),
                field(Tag::GPSLongitudeRef, ascii("W")),
                field(Tag::GPSLongitude, dms(21, 0, 3600)),
            ]),
        );
        let lines = text(&build(&path).unwrap());
        let row = |key: &str| {
            lines
                .iter()
                .find(|l| l.starts_with(key))
                .unwrap_or_else(|| panic!("no {key} in {lines:#?}"))
                .clone()
        };
        assert_eq!(row("camera"), "camera   FUJIFILM X-T5");
        assert_eq!(row("lens"), "lens     XF35mmF1.4 R");
        assert_eq!(row("taken"), "taken    2026-05-01 18:04:09");
        assert_eq!(
            row("exposure"),
            "exposure 1/250 s  f/2.8  ISO 400  35 mm (53 mm eq.)"
        );
        assert_eq!(row("location"), "location 52.22972° N, 21.01000° W");
        assert!(
            !lines.iter().any(|l| l.starts_with("flash")),
            "a flash that did not fire is not worth a line"
        );
    }

    #[test]
    fn a_picture_without_camera_data_keeps_the_short_card() {
        let (_t, path) = file("pic.png", &png(64, 32));
        let lines = text(&build(&path).unwrap());
        assert!(
            !lines.iter().any(|l| ["camera", "exposure", "taken"]
                .iter()
                .any(|k| l.starts_with(k))),
            "{lines:#?}"
        );
    }
}
