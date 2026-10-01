//! Draws decoded images in the preview column with the best method the terminal offers: kitty,
//! sixel or iTerm2 graphics, or else quadrant block characters that any terminal can show.

use std::{borrow::Cow, cell::RefCell, os::unix::ffi::OsStrExt, path::PathBuf, sync::Arc};

use crate::termquery::Answers;

use image::DynamicImage;
use image::imageops::FilterType;
use ratatui::{
    buffer::{Buffer, CellDiffOption},
    layout::{Rect, Size},
    style::Color,
    widgets::Widget,
};
use ratatui_image::{
    FontSize, Image, Resize,
    picker::{Picker, ProtocolType},
    protocol::Protocol,
};

/// A decoded image, shared between the preview cache and the screen.
#[derive(Clone)]
pub struct ImageData(pub Arc<DynamicImage>);

impl std::fmt::Debug for ImageData {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "ImageData({}x{})", self.0.width(), self.0.height())
    }
}

impl PartialEq for ImageData {
    fn eq(&self, other: &Self) -> bool {
        Arc::ptr_eq(&self.0, &other.0)
    }
}

impl Eq for ImageData {}

impl std::hash::Hash for ImageData {
    fn hash<H: std::hash::Hasher>(&self, state: &mut H) {
        Arc::as_ptr(&self.0).hash(state);
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Mode {
    /// Ask the terminal what it supports.
    Auto,
    Kitty,
    Sixel,
    Iterm2,
    /// Quadrant block characters: 2x2 pixels a cell, works in any terminal with a normal font.
    Blocks,
    /// Upper half blocks: 1x2 pixels a cell, for fonts without quadrant characters.
    Halfblocks,
    Off,
}

impl Mode {
    pub fn parse(text: &str) -> Option<Mode> {
        Some(match text {
            "auto" => Mode::Auto,
            "kitty" => Mode::Kitty,
            "sixel" => Mode::Sixel,
            "iterm2" => Mode::Iterm2,
            "blocks" => Mode::Blocks,
            "halfblocks" => Mode::Halfblocks,
            "off" => Mode::Off,
            _ => return None,
        })
    }
}

/// How pictures end up on screen.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Method {
    Graphics(ProtocolType),
    Blocks(BlockKind),
    Off,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BlockKind {
    Quadrants,
    Halves,
}

/// Picks the drawing method from the config, the terminal's answers and its environment variables.
/// Returns the method and a short account of why, for `:images`.
pub fn choose(
    mode: Mode,
    answers: Option<&Answers>,
    var: impl Fn(&str) -> Option<String>,
) -> (Method, String) {
    let forced = |method, what: &str| (method, format!("{what}, set in the config"));
    match mode {
        Mode::Off => return forced(Method::Off, "off"),
        Mode::Kitty => return forced(Method::Graphics(ProtocolType::Kitty), "kitty graphics"),
        Mode::Sixel => return forced(Method::Graphics(ProtocolType::Sixel), "sixel"),
        Mode::Iterm2 => return forced(Method::Graphics(ProtocolType::Iterm2), "iTerm2 images"),
        Mode::Blocks => return forced(Method::Blocks(BlockKind::Quadrants), "quadrant blocks"),
        Mode::Halfblocks => return forced(Method::Blocks(BlockKind::Halves), "half blocks"),
        Mode::Auto => {}
    }
    let get = |name: &str| var(name).unwrap_or_default();
    let term = get("TERM");
    let program = get("TERM_PROGRAM");
    let in_tmux = var("TMUX").is_some() || term.starts_with("tmux") || term.starts_with("screen");
    // The image library wraps its output for tmux only when TERM says tmux.
    let tmux_wrapped = term.starts_with("tmux") || program == "tmux";
    let blocks = |why: &str| {
        (
            Method::Blocks(BlockKind::Quadrants),
            format!("quadrant blocks, {why}"),
        )
    };
    if in_tmux && !tmux_wrapped {
        return blocks("because pictures cannot pass through this tmux or screen setup");
    }
    let empty = Answers::default();
    let answers = answers.unwrap_or(&empty);
    let name = answers.name.clone().unwrap_or_default().to_lowercase();
    let heard = if answers.complete {
        match &answers.name {
            Some(name) => format!("the terminal says it is {name}"),
            None => "the terminal answered".to_string(),
        }
    } else {
        "the terminal did not answer".to_string()
    };
    let graphics = |p, what: &str| (Method::Graphics(p), format!("{what}: {heard}"));
    let konsole = name.contains("konsole") || var("KONSOLE_VERSION").is_some();
    let wezterm = name.contains("wezterm") || program.contains("WezTerm");
    // iTerm2 sends LC_TERMINAL through SSH, where TERM_PROGRAM does not arrive. Both are inherited
    // by a terminal started from iTerm2, so a terminal that names itself is believed over them;
    // inside tmux the name is tmux's own.
    let named = !name.is_empty() && !name.starts_with("tmux");
    let iterm2 = name.contains("iterm2")
        || (!named && (program.contains("iTerm") || get("LC_TERMINAL") == "iTerm2"));
    // Konsole and WezTerm accept kitty graphics but not the unicode placeholders it is drawn with here.
    if konsole {
        return graphics(ProtocolType::Sixel, "sixel");
    }
    if wezterm || iterm2 || program.contains("mintty") {
        return graphics(ProtocolType::Iterm2, "iTerm2 images");
    }
    if answers.kitty || term == "xterm-kitty" || term == "xterm-ghostty" || program == "ghostty" {
        return graphics(ProtocolType::Kitty, "kitty graphics");
    }
    // Inside tmux the DA1 answer comes from tmux itself, not from the terminal the pictures would reach.
    if (answers.sixel && !in_tmux)
        || term.starts_with("foot")
        || term.starts_with("mlterm")
        || program.contains("contour")
    {
        return graphics(ProtocolType::Sixel, "sixel");
    }
    blocks(&format!("because no picture protocol was found ({heard})"))
}

/// Which image was drawn where, so the runtime can wipe leftovers of a graphics image.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Drawn {
    pub path: PathBuf,
    pub area: Rect,
}

/// A picture encoded for a size. Holding the image keeps its address, and so the key, unique.
pub type Key = (PathBuf, ImageData, Size);

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct BlockCell {
    ch: char,
    fg: (u8, u8, u8),
    bg: (u8, u8, u8),
}

pub enum Encoded {
    Graphics(Protocol),
    /// An iTerm2 inline image, a JPEG (or a PNG when it has transparency) that the terminal
    /// stretches over `size` cells.
    Inline {
        sequence: String,
        size: Size,
    },
    /// A kitty image: its pixels are uploaded once under `id`, then shown by placeholder characters.
    Kitty {
        id: u32,
        size: Size,
        upload: Upload,
    },
    Blocks {
        cells: Vec<BlockCell>,
        size: Size,
    },
}

/// How a kitty image's pixels reach the terminal.
pub enum Upload {
    /// Escape sequences carrying the pixels through the terminal connection, base64 encoded.
    Direct(String),
    /// Raw pixels handed over in a temporary file that the terminal reads and deletes, so only its
    /// name goes through the connection. For a terminal on the same machine.
    File { placement: String, pixels: Vec<u8> },
}

/// What encoding needs to know about the terminal.
#[derive(Clone)]
struct Encoder {
    method: Method,
    /// For sixel, the one protocol still encoded by the image library.
    picker: Option<Picker>,
    /// Pixels a cell is encoded with, fewer than it shows when pictures are sent small.
    font: FontSize,
    /// Wrap escape sequences so tmux passes them on.
    tmux: bool,
    /// Kitty can read pictures from temporary files.
    kitty_files: bool,
    /// Kitty inflates compressed pictures.
    kitty_zlib: bool,
}

/// Encoding a picture for the terminal, to be done off the main thread.
pub struct EncodeJob {
    key: Key,
    size: Size,
    encoder: Encoder,
}

impl EncodeJob {
    pub fn run(self) -> (Key, Option<Encoded>) {
        let encoded = encode(&self.encoder, &self.key.1.0, self.size);
        (self.key, encoded)
    }

    pub fn key(&self) -> Key {
        self.key.clone()
    }

    /// Gives the job up, so the picture can be asked for again later.
    pub fn abandon(self) -> (Key, Option<Encoded>) {
        (self.key, None)
    }
}

/// How much of each side of the room a sixel picture fills over a slow link: about a third of the bytes.
const REMOTE_SIXEL_SHARE: f64 = 0.6;

/// Encoded pictures kept, so going back to one is instant.
const ENCODED_CACHE: usize = 12;

/// iTerm2 decodes every picture it is sent, and it is sent again on every visit, so bytes cost
/// twice. At 80 a photo is about a quarter smaller than at 88 and looks the same at cell size.
const JPEG_QUALITY: u8 = 80;

impl Encoded {
    fn size(&self) -> Size {
        match self {
            Encoded::Graphics(protocol) => protocol.size(),
            Encoded::Inline { size, .. }
            | Encoded::Kitty { size, .. }
            | Encoded::Blocks { size, .. } => *size,
        }
    }
}

pub struct Painter {
    encoder: Encoder,
    /// The real cell size, which pictures are laid out with.
    font: FontSize,
    /// Share of the room a picture fills, below one when a slow link makes every pixel cost.
    shrink: f64,
    reason: String,
    cache: RefCell<std::collections::VecDeque<(Key, Arc<Encoded>)>>,
    requested: RefCell<std::collections::HashSet<Key>>,
    jobs: RefCell<Vec<EncodeJob>>,
    drawn: RefCell<Option<Drawn>>,
    /// Kitty images the terminal holds.
    uploaded: RefCell<std::collections::HashSet<u32>>,
    /// Kitty images dropped from the cache, to be deleted from the terminal with the next upload.
    freed: RefCell<Vec<u32>>,
    /// Cells the preview column had in the last frame.
    room: std::cell::Cell<Option<(u16, u16)>>,
    /// The cell size the window's pixel size gave at startup, and the one used then. Terminals that
    /// count their padding in disagree with their own answer, so later sizes are scaled alike.
    measured: Option<((u16, u16), FontSize)>,
}

impl Painter {
    /// `answers` are what the terminal said when asked at startup, if it was asked.
    pub fn new(mode: Mode, answers: Option<&Answers>) -> Painter {
        let (method, reason) = choose(mode, answers, |name| std::env::var(name).ok());
        let (w, h) = plausible_cell(&[answers.and_then(|a| a.cell), cell_size()]);
        let mut painter = Painter::build(method, FontSize::new(w, h), reason);
        painter.measured = cell_size().map(|cell| (cell, painter.font));
        let var = |name| std::env::var(name).unwrap_or_default();
        painter.encoder.tmux = var("TERM").starts_with("tmux") || var("TERM_PROGRAM") == "tmux";
        // iTerm2 answers the kitty question too, but is not drawn with kitty graphics here.
        let kitty = method == Method::Graphics(ProtocolType::Kitty);
        painter.encoder.kitty_files = kitty && answers.is_some_and(|a| a.kitty_files);
        painter.encoder.kitty_zlib = kitty && answers.is_some_and(|a| a.kitty_zlib);
        painter
    }

    /// Quadrant blocks, without asking the terminal anything. For tests and pipes.
    pub fn blocks() -> Painter {
        Painter::build(
            Method::Blocks(BlockKind::Quadrants),
            FontSize::new(10, 20),
            "quadrant blocks".into(),
        )
    }

    pub fn halfblocks() -> Painter {
        Painter::build(
            Method::Blocks(BlockKind::Halves),
            FontSize::new(10, 20),
            "half blocks".into(),
        )
    }

    pub fn off() -> Painter {
        Painter::build(Method::Off, FontSize::new(10, 20), "off".into())
    }

    /// Over a slow link the bytes of a picture are what the wait is made of. Kitty takes raw pixels
    /// and stretches them over the cells itself, so it is sent at half the resolution, a quarter
    /// of the bytes. Temporary files are of no use to a terminal on another machine.
    pub fn remote(mut self, remote: bool) -> Painter {
        if remote && self.encoder.method == Method::Graphics(ProtocolType::Kitty) {
            let font = self.font;
            self.encoder.font = FontSize::new((font.width / 2).max(1), (font.height / 2).max(1));
            self.encoder.kitty_files = false;
        }
        // Sixel pixels cannot be stretched by the terminal, so the picture itself has to be smaller.
        if remote && self.encoder.method == Method::Graphics(ProtocolType::Sixel) {
            self.shrink = REMOTE_SIXEL_SHARE;
        }
        self
    }

    fn build(method: Method, font: FontSize, reason: String) -> Painter {
        let picker = (method == Method::Graphics(ProtocolType::Sixel)).then(|| sixel_picker(font));
        Painter {
            encoder: Encoder {
                method,
                picker,
                font,
                tmux: false,
                kitty_files: false,
                kitty_zlib: false,
            },
            font,
            shrink: 1.0,
            reason,
            cache: RefCell::default(),
            requested: RefCell::default(),
            jobs: RefCell::default(),
            drawn: RefCell::new(None),
            uploaded: RefCell::default(),
            freed: RefCell::default(),
            room: std::cell::Cell::new(None),
            measured: None,
        }
    }

    pub fn enabled(&self) -> bool {
        self.encoder.method != Method::Off
    }

    /// How pictures are drawn and why, in a sentence for the footer.
    pub fn describe(&self) -> String {
        let files = if self.encoder.kitty_files {
            ", sent as files"
        } else {
            ""
        };
        format!(
            "pictures: {}{files}; cell {}x{} px",
            self.reason, self.font.width, self.font.height
        )
    }

    /// Whether images are drawn with escape sequences that text drawn over them may not erase.
    pub fn leaves_ghosts(&self) -> bool {
        matches!(
            self.encoder.method,
            Method::Graphics(ProtocolType::Sixel | ProtocolType::Iterm2)
        )
    }

    /// Whether writing a cell over a picture erases that piece of it, so leftovers are wiped by
    /// rewriting their cells rather than the whole screen. iTerm2 keeps pictures in its cell grid.
    pub fn overwriting_erases(&self) -> bool {
        self.encoder.method == Method::Graphics(ProtocolType::Iterm2)
    }

    /// The terminal may have dropped the kitty images it held, as on leaving the alternate screen
    /// for an editor, so each is uploaded again before it is next shown.
    pub fn forget_uploads(&self) {
        self.uploaded.borrow_mut().clear();
    }

    /// Follows the cell size after the window changed, as when the font is zoomed. Pictures are
    /// laid out and encoded again for the new size.
    pub fn follow_resize(&mut self) {
        self.rescale(cell_size());
    }

    /// Takes on the cell size `now` measured from the window, when it differs by more than the
    /// padding a resize alone moves it by. Says whether it did.
    fn rescale(&mut self, now: Option<(u16, u16)>) -> bool {
        let (Some((base, start)), Some(now)) = (self.measured, now) else {
            return false;
        };
        let scale = |cell: u16, now: u16, base: u16| {
            (u32::from(cell) * u32::from(now) / u32::from(base.max(1))) as u16
        };
        let cell = (
            scale(start.width, now.0, base.0),
            scale(start.height, now.1, base.1),
        );
        let moved = |new: u16, old: u16| f64::from(new.abs_diff(old)) > f64::from(old) * 0.08;
        if plausible_cell(&[Some(cell)]) != cell
            || !(moved(cell.0, self.font.width) || moved(cell.1, self.font.height))
        {
            return false;
        }
        let halved = self.encoder.font.width != self.font.width;
        self.font = FontSize::new(cell.0, cell.1);
        self.encoder.font = if halved {
            FontSize::new((cell.0 / 2).max(1), (cell.1 / 2).max(1))
        } else {
            self.font
        };
        if self.encoder.picker.is_some() {
            self.encoder.picker = Some(sixel_picker(self.font));
        }
        // Everything encoded for the old size goes, kitty images in the terminal included.
        for (_, encoded) in self.cache.get_mut().drain(..) {
            if let Encoded::Kitty { id, .. } = *encoded
                && self.uploaded.get_mut().remove(&id)
            {
                self.freed.get_mut().push(id);
            }
        }
        self.requested.get_mut().clear();
        true
    }

    /// Cells the image takes when fitted into `available`, keeping its shape.
    /// Small pictures are enlarged at most twice, since every enlargement only makes bigger blocks.
    pub fn fitted_size(&self, image: &ImageData, available: Size) -> Option<Size> {
        if self.encoder.method == Method::Off || available.width == 0 || available.height == 0 {
            return None;
        }
        let (w, h) = (f64::from(image.0.width()), f64::from(image.0.height()));
        let (fw, fh) = (f64::from(self.font.width), f64::from(self.font.height));
        let scale = (f64::from(available.width) * fw / w)
            .min(f64::from(available.height) * fh / h)
            .min(2.0)
            * self.shrink;
        let cols = ((w * scale / fw).round() as u16).clamp(1, available.width);
        let rows = ((h * scale / fh).round() as u16).clamp(1, available.height);
        Some(Size::new(cols, rows))
    }

    /// Draws the image at the top left of `area`. A picture not encoded for this size yet is queued
    /// for the encoder thread and appears once it is ready; until then the last picture of the same
    /// file stays, so moving from the camera's preview to the photo never blanks the column.
    pub fn draw(&self, path: &std::path::Path, image: &ImageData, area: Rect, buf: &mut Buffer) {
        let Some(size) = self.fitted_size(image, area.as_size()) else {
            return;
        };
        let key: Key = (path.to_path_buf(), image.clone(), size);
        let found = {
            let cache = self.cache.borrow();
            let exact = cache.iter().find(|(k, _)| *k == key);
            if exact.is_none() && self.requested.borrow_mut().insert(key.clone()) {
                self.jobs.borrow_mut().push(EncodeJob {
                    key: key.clone(),
                    size,
                    encoder: self.encoder.clone(),
                });
            }
            exact
                .or_else(|| {
                    cache.iter().rev().find(|((p, _, s), _)| {
                        p == path && s.width <= area.width && s.height <= area.height
                    })
                })
                .map(|(_, e)| Arc::clone(e))
        };
        let Some(encoded) = found else {
            return;
        };
        let placed = Rect {
            width: encoded.size().width.min(area.width),
            height: encoded.size().height.min(area.height),
            ..area
        };
        match &*encoded {
            Encoded::Graphics(protocol) => Image::new(protocol).render(placed, buf),
            Encoded::Inline { sequence, .. } => place_sequence(sequence, placed, buf),
            Encoded::Kitty { id, upload, .. } => {
                let mut sequence = String::new();
                if self.uploaded.borrow_mut().insert(*id) {
                    for gone in self.freed.borrow_mut().drain(..) {
                        sequence.push_str(&kitty_command(
                            &format!("a=d,d=I,i={gone},q=2"),
                            "",
                            self.encoder.tmux,
                        ));
                    }
                    sequence.push_str(&upload.sequence(self.encoder.tmux));
                }
                place_kitty(*id, &sequence, placed, buf);
            }
            Encoded::Blocks { cells, size } => {
                for (i, cell) in cells.iter().enumerate() {
                    let (x, y) = (i as u16 % size.width, i as u16 / size.width);
                    if x < placed.width && y < placed.height {
                        buf[(placed.x + x, placed.y + y)]
                            .set_char(cell.ch)
                            .set_fg(Color::Rgb(cell.fg.0, cell.fg.1, cell.fg.2))
                            .set_bg(Color::Rgb(cell.bg.0, cell.bg.1, cell.bg.2));
                    }
                }
            }
        }
        *self.drawn.borrow_mut() = Some(Drawn {
            path: path.to_path_buf(),
            area: placed,
        });
    }

    pub fn take_jobs(&self) -> Vec<EncodeJob> {
        std::mem::take(&mut self.jobs.borrow_mut())
    }

    /// Keeps a finished encoding. `None` means the job was given up and may be asked for again.
    /// Encodings of an earlier picture of the same file, such as the camera's preview, go.
    pub fn store(&self, key: Key, encoded: Option<Encoded>) {
        self.requested.borrow_mut().remove(&key);
        let Some(encoded) = encoded else { return };
        let mut cache = self.cache.borrow_mut();
        let mut gone = Vec::new();
        cache.retain(|(k, e)| {
            let stale = *k == key || (k.0 == key.0 && k.1 != key.1);
            if stale {
                gone.push(Arc::clone(e));
            }
            !stale
        });
        cache.push_back((key, Arc::new(encoded)));
        while cache.len() > ENCODED_CACHE {
            gone.extend(cache.pop_front().map(|(_, e)| e));
        }
        for encoded in gone {
            if let Encoded::Kitty { id, .. } = *encoded
                && self.uploaded.borrow_mut().remove(&id)
            {
                self.freed.borrow_mut().push(id);
            }
        }
    }

    /// Runs the queued encodings on this thread. For tests and benchmarks.
    pub fn run_jobs_now(&self) {
        for job in self.take_jobs() {
            let (key, encoded) = job.run();
            self.store(key, encoded);
        }
    }

    /// The largest picture worth decoding for a preview column of this many cells.
    pub fn decode_target(&self, columns: u16, rows: u16) -> (u32, u32) {
        match self.encoder.method {
            Method::Graphics(_) => {
                let font = self.encoder.font;
                let px = |cells: u16, cell: u16| {
                    (f64::from(cells) * f64::from(cell) * self.shrink) as u32
                };
                (px(columns, font.width), px(rows, font.height))
            }
            // Two pixels a cell each way, and twice that so the smoothing filter has something to average.
            Method::Blocks(_) | Method::Off => (u32::from(columns) * 4, u32::from(rows) * 4),
        }
    }

    /// Records the cells the preview column has, which change with the folders beside it and the
    /// window size.
    pub fn note_room(&self, columns: u16, rows: u16) {
        self.room.set(Some((columns, rows)));
    }

    /// The pixels worth decoding pictures with for the room of the last frame, once per change.
    pub fn take_decode_target(&self) -> Option<(u32, u32)> {
        let (columns, rows) = self.room.take()?;
        Some(self.decode_target(columns, rows))
    }

    /// What the last frame showed, cleared for the next one.
    pub fn take_drawn(&self) -> Option<Drawn> {
        self.drawn.borrow_mut().take()
    }
}

fn encode(encoder: &Encoder, image: &DynamicImage, size: Size) -> Option<Encoded> {
    match encoder.method {
        Method::Off => None,
        Method::Graphics(ProtocolType::Iterm2) => encode_inline(encoder, image, size),
        Method::Graphics(ProtocolType::Kitty) => Some(encode_kitty(encoder, image, size)),
        Method::Graphics(_) => {
            let smooth = Resize::Scale(Some(FilterType::CatmullRom));
            encoder
                .picker
                .as_ref()?
                .new_protocol(image.clone(), size, smooth)
                .ok()
                .map(Encoded::Graphics)
        }
        Method::Blocks(kind) => Some(Encoded::Blocks {
            cells: encode_blocks(image, size, kind),
            size,
        }),
    }
}

/// The picture with no more pixels than `size` cells hold. Within a cell of that, the terminal's
/// own stretching does as well as resampling here, so it is left as it is.
fn at_most_cells(image: &DynamicImage, size: Size, font: FontSize) -> Cow<'_, DynamicImage> {
    let (fw, fh) = (u32::from(font.width), u32::from(font.height));
    let (max_w, max_h) = (u32::from(size.width) * fw, u32::from(size.height) * fh);
    if image.width() > max_w + fw || image.height() > max_h + fh {
        Cow::Owned(crate::preview::shrink(image.clone(), max_w, max_h))
    } else {
        Cow::Borrowed(image)
    }
}

fn has_transparency(image: &DynamicImage) -> bool {
    match image {
        DynamicImage::ImageRgba8(i) => i.pixels().any(|p| p.0[3] < 255),
        DynamicImage::ImageLumaA8(i) => i.pixels().any(|p| p.0[1] < 255),
        other => other.color().has_alpha() && other.to_rgba8().pixels().any(|p| p.0[3] < 255),
    }
}

/// An iTerm2 inline image. The library sends a PNG in absolute pixels, about a megabyte for a photo
/// that fills the column, where a JPEG of the same picture is a tenth of that. Cell counts for the
/// size let the terminal do the scaling, so the picture is never sent bigger than it is. Only a
/// picture with see-through parts stays a PNG, which iTerm2 shows over its background.
fn encode_inline(encoder: &Encoder, image: &DynamicImage, size: Size) -> Option<Encoded> {
    let image = at_most_cells(image, size, encoder.font);
    let mut data = Vec::new();
    if has_transparency(&image) {
        use image::{ImageEncoder, codecs::png};
        let mut rgba = image.to_rgba8();
        // Colour hidden under full transparency is noise to the compressor: a third of the bytes.
        for pixel in rgba.pixels_mut().filter(|p| p.0[3] == 0) {
            pixel.0 = [0; 4];
        }
        png::PngEncoder::new_with_quality(
            &mut data,
            png::CompressionType::Fast,
            png::FilterType::Adaptive,
        )
        .write_image(
            &rgba,
            rgba.width(),
            rgba.height(),
            image::ExtendedColorType::Rgba8,
        )
        .ok()?;
    } else {
        let mut jpeg = image::codecs::jpeg::JpegEncoder::new_with_quality(&mut data, JPEG_QUALITY);
        match image.as_luma8() {
            Some(grey) => jpeg.encode_image(grey),
            None => jpeg.encode_image(&image.to_rgb8()),
        }
        .ok()?;
    }
    let (start, escape, end) = tmux_wrapping(encoder.tmux);
    let mut sequence = String::with_capacity(data.len() * 4 / 3 + 200);
    sequence.push_str(start);
    // Blank the cells first, the way the library does, so nothing shows through at the edges.
    for _ in 0..size.height {
        sequence.push_str(&format!("{escape}[{}X{escape}[1B", size.width));
    }
    sequence.push_str(&format!("{escape}[{}A", size.height));
    sequence.push_str(&format!(
        "{escape}]1337;File=inline=1;size={};width={};height={};preserveAspectRatio=0;doNotMoveCursor=1:",
        data.len(),
        size.width,
        size.height
    ));
    base64_simd::STANDARD.encode_append(&data, &mut sequence);
    sequence.push_str(&format!("\x07{end}"));
    Some(Encoded::Inline { sequence, size })
}

/// What starts an escape sequence, what stands for an escape inside it, and what ends it: tmux
/// passes sequences on to the terminal only wrapped in its own.
fn tmux_wrapping(tmux: bool) -> (&'static str, &'static str, &'static str) {
    if tmux {
        ("\x1bPtmux;", "\x1b\x1b", "\x1b\\")
    } else {
        ("", "\x1b", "")
    }
}

/// One kitty graphics command.
fn kitty_command(control: &str, payload: &str, tmux: bool) -> String {
    let (start, escape, end) = tmux_wrapping(tmux);
    format!("{start}{escape}_G{control};{payload}{escape}\\{end}")
}

/// Kitty image ids, unique within this program and unlikely to meet another's. They stay below
/// 2^24 so the whole id fits in the placeholders' colour.
fn next_kitty_id() -> u32 {
    static NEXT: std::sync::atomic::AtomicU32 = std::sync::atomic::AtomicU32::new(0);
    let n = NEXT.fetch_add(1, std::sync::atomic::Ordering::Relaxed) & 0xFFFF;
    (std::process::id() % 255 + 1) << 16 | n
}

/// A kitty image at the size it was decoded, three bytes a pixel. The placement names the cells,
/// and kitty fits the pixels into them on the GPU, so nothing is resampled or padded here.
fn encode_kitty(encoder: &Encoder, image: &DynamicImage, size: Size) -> Encoded {
    let image = at_most_cells(image, size, encoder.font);
    let (width, height) = (image.width(), image.height());
    let (pixels, format) = if image.color().has_alpha() {
        (image.to_rgba8().into_raw(), 32)
    } else {
        match &*image {
            DynamicImage::ImageRgb8(rgb) => (rgb.as_raw().clone(), 24),
            other => (other.to_rgb8().into_raw(), 24),
        }
    };
    let id = next_kitty_id();
    let placement = format!(
        "a=T,U=1,i={id},f={format},s={width},v={height},c={},r={},q=2",
        size.width, size.height
    );
    let upload = if encoder.kitty_files {
        Upload::File { placement, pixels }
    } else if encoder.kitty_zlib {
        // Through the connection every byte counts: a photo deflates to about half, quickly.
        use std::io::Write as _;
        let mut zlib = flate2::write::ZlibEncoder::new(Vec::new(), flate2::Compression::fast());
        let deflated = zlib.write_all(&pixels).and_then(|()| zlib.finish());
        match deflated {
            Ok(deflated) => Upload::Direct(kitty_direct(
                &deflated,
                &format!("{placement},o=z"),
                encoder.tmux,
            )),
            Err(_) => Upload::Direct(kitty_direct(&pixels, &placement, encoder.tmux)),
        }
    } else {
        Upload::Direct(kitty_direct(&pixels, &placement, encoder.tmux))
    };
    Encoded::Kitty { id, size, upload }
}

/// Pixels sent through the connection in base64 pieces of 4096 bytes, the most kitty takes in one.
/// `placement` is the control data of an upload that also makes the virtual placement the
/// placeholders show.
fn kitty_direct(pixels: &[u8], placement: &str, tmux: bool) -> String {
    const CHUNK: usize = 4096 / 4 * 3;
    let count = pixels.len().div_ceil(CHUNK).max(1);
    let mut out = String::with_capacity(pixels.len() * 4 / 3 + count * 40);
    let mut payload = String::with_capacity(4096);
    for (i, chunk) in pixels.chunks(CHUNK).enumerate() {
        let more = u8::from(i + 1 < count);
        let control = if i == 0 {
            format!("{placement},m={more}")
        } else {
            format!("m={more},q=2")
        };
        payload.clear();
        base64_simd::STANDARD.encode_append(chunk, &mut payload);
        out.push_str(&kitty_command(&control, &payload, tmux));
    }
    out
}

impl Upload {
    /// The escape sequences that hand the pixels to the terminal. A file is written now, since the
    /// terminal deletes it once read and each upload needs its own.
    fn sequence(&self, tmux: bool) -> String {
        match self {
            Upload::Direct(sequence) => sequence.clone(),
            Upload::File { placement, pixels } => {
                static FILES: std::sync::atomic::AtomicU32 = std::sync::atomic::AtomicU32::new(0);
                let n = FILES.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
                let path = std::env::temp_dir().join(format!(
                    "tty-graphics-protocol-tx-{}-{n}",
                    std::process::id()
                ));
                if std::fs::write(&path, pixels).is_err() {
                    return String::new();
                }
                let name = base64_simd::STANDARD.encode_to_string(path.as_os_str().as_bytes());
                kitty_command(&format!("{placement},t=t"), &name, tmux)
            }
        }
    }
}

/// Puts a terminal escape sequence in the top left cell and keeps the rest of the area from being
/// written over it.
fn place_sequence(sequence: &str, area: Rect, buf: &mut Buffer) {
    for y in area.top()..area.bottom() {
        for x in area.left()..area.right() {
            let Some(cell) = buf.cell_mut((x, y)) else {
                continue;
            };
            if (x, y) == (area.left(), area.top()) {
                let one = std::num::NonZeroU16::MIN;
                cell.set_symbol(sequence)
                    .set_diff_option(CellDiffOption::ForcedWidth(one));
            } else {
                cell.set_diff_option(CellDiffOption::Skip);
            }
        }
    }
}

/// The character kitty replaces with a piece of an image.
const PLACEHOLDER: char = '\u{10EEEE}';

/// Fills `area` with kitty placeholders for image `id`, whose colour names the image. The first
/// cell of each row says which row and column it starts; kitty counts on from there. `upload`, when
/// not empty, goes out first, in the top left cell.
fn place_kitty(id: u32, upload: &str, area: Rect, buf: &mut Buffer) {
    let [_, r, g, b] = id.to_be_bytes();
    let one = CellDiffOption::ForcedWidth(std::num::NonZeroU16::MIN);
    let mut symbol = String::new();
    for (row, y) in (area.top()..area.bottom()).enumerate() {
        for x in area.left()..area.right() {
            let Some(cell) = buf.cell_mut((x, y)) else {
                continue;
            };
            symbol.clear();
            if (x, y) == (area.left(), area.top()) {
                symbol.push_str(upload);
            }
            symbol.push(PLACEHOLDER);
            if x == area.left() {
                symbol.extend([diacritic(row), diacritic(0)]);
            }
            cell.set_symbol(&symbol)
                .set_fg(Color::Rgb(r, g, b))
                .set_diff_option(one);
        }
    }
}

/// The combining mark that numbers a row or column of kitty placeholders.
fn diacritic(n: usize) -> char {
    DIACRITICS.chars().nth(n).unwrap_or('\u{305}')
}

/// From kitty's rowcolumn-diacritics.txt: the marks for 0, 1, 2 and on.
const DIACRITICS: &str = concat!(
    "\u{305}\u{30D}\u{30E}\u{310}\u{312}\u{33D}\u{33E}\u{33F}\u{346}\u{34A}\u{34B}\u{34C}",
    "\u{350}\u{351}\u{352}\u{357}\u{35B}\u{363}\u{364}\u{365}\u{366}\u{367}\u{368}\u{369}",
    "\u{36A}\u{36B}\u{36C}\u{36D}\u{36E}\u{36F}\u{483}\u{484}\u{485}\u{486}\u{487}\u{592}",
    "\u{593}\u{594}\u{595}\u{597}\u{598}\u{599}\u{59C}\u{59D}\u{59E}\u{59F}\u{5A0}\u{5A1}",
    "\u{5A8}\u{5A9}\u{5AB}\u{5AC}\u{5AF}\u{5C4}\u{610}\u{611}\u{612}\u{613}\u{614}\u{615}",
    "\u{616}\u{617}\u{657}\u{658}\u{659}\u{65A}\u{65B}\u{65D}\u{65E}\u{6D6}\u{6D7}\u{6D8}",
    "\u{6D9}\u{6DA}\u{6DB}\u{6DC}\u{6DF}\u{6E0}\u{6E1}\u{6E2}\u{6E4}\u{6E7}\u{6E8}\u{6EB}",
    "\u{6EC}\u{730}\u{732}\u{733}\u{735}\u{736}\u{73A}\u{73D}\u{73F}\u{740}\u{741}\u{743}",
    "\u{745}\u{747}\u{749}\u{74A}\u{7EB}\u{7EC}\u{7ED}\u{7EE}\u{7EF}\u{7F0}\u{7F1}\u{7F3}",
    "\u{816}\u{817}\u{818}\u{819}\u{81B}\u{81C}\u{81D}\u{81E}\u{81F}\u{820}\u{821}\u{822}",
    "\u{823}\u{825}\u{826}\u{827}\u{829}\u{82A}\u{82B}\u{82C}\u{82D}\u{951}\u{953}\u{954}",
    "\u{F82}\u{F83}\u{F86}\u{F87}\u{135D}\u{135E}\u{135F}\u{17DD}\u{193A}\u{1A17}\u{1A75}",
    "\u{1A76}\u{1A77}\u{1A78}\u{1A79}\u{1A7A}\u{1A7B}\u{1A7C}\u{1B6B}\u{1B6D}\u{1B6E}\u{1B6F}",
    "\u{1B70}\u{1B71}\u{1B72}\u{1B73}\u{1CD0}\u{1CD1}\u{1CD2}\u{1CDA}\u{1CDB}\u{1CE0}\u{1DC0}",
    "\u{1DC1}\u{1DC3}\u{1DC4}\u{1DC5}\u{1DC6}\u{1DC7}\u{1DC8}\u{1DC9}\u{1DCB}\u{1DCC}\u{1DD1}",
    "\u{1DD2}\u{1DD3}\u{1DD4}\u{1DD5}\u{1DD6}\u{1DD7}\u{1DD8}\u{1DD9}\u{1DDA}\u{1DDB}\u{1DDC}",
    "\u{1DDD}\u{1DDE}\u{1DDF}\u{1DE0}\u{1DE1}\u{1DE2}\u{1DE3}\u{1DE4}\u{1DE5}\u{1DE6}\u{1DFE}",
    "\u{20D0}\u{20D1}\u{20D4}\u{20D5}\u{20D6}\u{20D7}\u{20DB}\u{20DC}\u{20E1}\u{20E7}\u{20E9}",
    "\u{20F0}\u{2CEF}\u{2CF0}\u{2CF1}\u{2DE0}\u{2DE1}\u{2DE2}\u{2DE3}\u{2DE4}\u{2DE5}\u{2DE6}",
    "\u{2DE7}\u{2DE8}\u{2DE9}\u{2DEA}\u{2DEB}\u{2DEC}\u{2DED}\u{2DEE}\u{2DEF}\u{2DF0}\u{2DF1}",
    "\u{2DF2}\u{2DF3}\u{2DF4}\u{2DF5}\u{2DF6}\u{2DF7}\u{2DF8}\u{2DF9}\u{2DFA}\u{2DFB}\u{2DFC}",
    "\u{2DFD}\u{2DFE}\u{2DFF}\u{A66F}\u{A67C}\u{A67D}\u{A6F0}\u{A6F1}\u{A8E0}\u{A8E1}\u{A8E2}",
    "\u{A8E3}\u{A8E4}\u{A8E5}\u{A8E6}\u{A8E7}\u{A8E8}\u{A8E9}\u{A8EA}\u{A8EB}\u{A8EC}\u{A8ED}",
    "\u{A8EE}\u{A8EF}\u{A8F0}\u{A8F1}\u{AAB0}\u{AAB2}\u{AAB3}\u{AAB7}\u{AAB8}\u{AABE}\u{AABF}",
    "\u{AAC1}\u{FE20}\u{FE21}\u{FE22}\u{FE23}\u{FE24}\u{FE25}\u{FE26}\u{10A0F}\u{10A38}",
    "\u{1D185}\u{1D186}\u{1D187}\u{1D188}\u{1D189}\u{1D1AA}\u{1D1AB}\u{1D1AC}\u{1D1AD}",
    "\u{1D242}\u{1D243}\u{1D244}",
);

/// Quadrant characters indexed by which quarters use the foreground colour:
/// 1 top left, 2 top right, 4 bottom left, 8 bottom right.
const QUADRANTS: [char; 16] = [
    ' ', '▘', '▝', '▀', '▖', '▌', '▞', '▛', '▗', '▚', '▐', '▜', '▄', '▙', '▟', '█',
];

type Rgb = (u8, u8, u8);

fn mean(pixels: &[Rgb]) -> Rgb {
    let n = pixels.len().max(1) as u32;
    let sum = pixels.iter().fold((0u32, 0u32, 0u32), |s, p| {
        (
            s.0 + u32::from(p.0),
            s.1 + u32::from(p.1),
            s.2 + u32::from(p.2),
        )
    });
    ((sum.0 / n) as u8, (sum.1 / n) as u8, (sum.2 / n) as u8)
}

fn error(pixels: &[Rgb], color: Rgb) -> u32 {
    pixels
        .iter()
        .map(|p| {
            let d = |a: u8, b: u8| (i32::from(a) - i32::from(b)).pow(2) as u32;
            d(p.0, color.0) + d(p.1, color.1) + d(p.2, color.2)
        })
        .sum()
}

/// Splits the four pixels of a cell into the two colour groups that match them best, the way
/// chafa picks block symbols. Returns the character and its foreground and background colours.
fn best_quadrant(pixels: [Rgb; 4]) -> BlockCell {
    let mut best = BlockCell {
        ch: ' ',
        fg: mean(&pixels),
        bg: mean(&pixels),
    };
    let mut best_error = error(&pixels, best.bg);
    // A mask and its complement split the pixels the same way, so the masks up to 7 cover every split.
    for (mask, &ch) in QUADRANTS.iter().enumerate().take(8).skip(1) {
        let (fg, bg): (Vec<Rgb>, Vec<Rgb>) = (0..4).map(|i| (mask >> i & 1 == 1, pixels[i])).fold(
            (Vec::new(), Vec::new()),
            |(mut f, mut b), (on, p)| {
                if on {
                    f.push(p)
                } else {
                    b.push(p)
                }
                (f, b)
            },
        );
        let (fg_color, bg_color) = (mean(&fg), mean(&bg));
        let total = error(&fg, fg_color) + error(&bg, bg_color);
        if total < best_error {
            best_error = total;
            best = BlockCell {
                ch,
                fg: fg_color,
                bg: bg_color,
            };
        }
    }
    best
}

/// Draws a picture with block characters: resized with a smoothing filter to the pixels the cells
/// can show, transparent parts laid over the page background.
fn encode_blocks(image: &DynamicImage, size: Size, kind: BlockKind) -> Vec<BlockCell> {
    let (cols, rows) = (u32::from(size.width), u32::from(size.height));
    let across = if kind == BlockKind::Quadrants { 2 } else { 1 };
    let pixels = image
        .resize_exact(cols * across, rows * 2, FilterType::CatmullRom)
        .to_rgba8();
    let background = crate::theme::BG;
    let at = |x: u32, y: u32| -> Rgb {
        let p = pixels.get_pixel(x, y).0;
        let a = u32::from(p[3]);
        let over = |c: u8, b: u8| ((u32::from(c) * a + u32::from(b) * (255 - a)) / 255) as u8;
        (
            over(p[0], background.0),
            over(p[1], background.1),
            over(p[2], background.2),
        )
    };
    let mut cells = Vec::with_capacity((cols * rows) as usize);
    for y in 0..rows {
        for x in 0..cols {
            let cell = match kind {
                BlockKind::Quadrants => best_quadrant([
                    at(2 * x, 2 * y),
                    at(2 * x + 1, 2 * y),
                    at(2 * x, 2 * y + 1),
                    at(2 * x + 1, 2 * y + 1),
                ]),
                BlockKind::Halves => {
                    let (top, bottom) = (at(x, 2 * y), at(x, 2 * y + 1));
                    BlockCell {
                        ch: if top == bottom { ' ' } else { '▀' },
                        fg: top,
                        bg: bottom,
                    }
                }
            };
            cells.push(cell);
        }
    }
    cells
}

fn sixel_picker(font: FontSize) -> Picker {
    // Deprecated in favour of the library's own terminal query, which leaves a reader thread
    // behind when the terminal is silent. This program asks by itself instead.
    #[allow(deprecated)]
    let mut picker = Picker::from_fontsize(font);
    picker.set_protocol_type(ProtocolType::Sixel);
    picker
}

/// Pixel size of one terminal cell, from the size the terminal reports for its window.
fn cell_size() -> Option<(u16, u16)> {
    let w = ratatui::crossterm::terminal::window_size().ok()?;
    (w.columns > 0 && w.rows > 0).then(|| (w.width / w.columns, w.height / w.rows))
}

const DEFAULT_CELL: (u16, u16) = (10, 20);

/// The first cell size that looks like a real font. Some terminals report pixel sizes that make a
/// cell a few pixels tall, which would shrink pictures to a blur.
fn plausible_cell(candidates: &[Option<(u16, u16)>]) -> (u16, u16) {
    candidates
        .iter()
        .flatten()
        .copied()
        .find(|&(w, h)| (4..=200).contains(&w) && (8..=400).contains(&h))
        .unwrap_or(DEFAULT_CELL)
}

#[cfg(test)]
mod tests {
    use image::{Rgba, RgbaImage};

    use super::*;

    fn picture(w: u32, h: u32) -> ImageData {
        let mut img = RgbaImage::new(w, h);
        for (x, _, px) in img.enumerate_pixels_mut() {
            *px = if x < w / 2 {
                Rgba([255, 0, 0, 255])
            } else {
                Rgba([0, 0, 255, 255])
            };
        }
        ImageData(Arc::new(DynamicImage::ImageRgba8(img)))
    }

    #[test]
    fn modes_parse_from_config_words() {
        assert_eq!(Mode::parse("auto"), Some(Mode::Auto));
        assert_eq!(Mode::parse("sixel"), Some(Mode::Sixel));
        assert_eq!(Mode::parse("off"), Some(Mode::Off));
        assert_eq!(Mode::parse("svga"), None);
    }

    #[test]
    fn halfblocks_draw_the_image_colours_into_cells() {
        let painter = Painter::halfblocks();
        let image = picture(40, 40);
        let mut buf = Buffer::empty(Rect::new(0, 0, 30, 20));
        painter.draw(
            std::path::Path::new("/p.png"),
            &image,
            Rect::new(2, 1, 20, 10),
            &mut buf,
        );
        painter.run_jobs_now();
        painter.draw(
            std::path::Path::new("/p.png"),
            &image,
            Rect::new(2, 1, 20, 10),
            &mut buf,
        );
        let drawn = painter
            .take_drawn()
            .expect("something was drawn once encoded");
        assert_eq!((drawn.area.x, drawn.area.y), (2, 1));
        assert!(drawn.area.width <= 20 && drawn.area.height <= 10);
        let left = &buf[(drawn.area.x, drawn.area.y)];
        let right = &buf[(drawn.area.right() - 1, drawn.area.y)];
        assert_ne!(left.fg, right.fg, "red half and blue half differ");
        assert_eq!(buf[(0, 0)].symbol(), " ", "nothing outside the area");
        assert!(painter.take_drawn().is_none(), "the record is per frame");
    }

    #[test]
    fn a_fine_checkerboard_shrinks_to_grey_instead_of_random_black_and_white() {
        let board = image::RgbImage::from_fn(1000, 1000, |x, y| {
            if (x + y) % 2 == 0 {
                image::Rgb([0, 0, 0])
            } else {
                image::Rgb([255, 255, 255])
            }
        });
        let image = ImageData(Arc::new(DynamicImage::ImageRgb8(board)));
        let painter = Painter::halfblocks();
        let mut buf = Buffer::empty(Rect::new(0, 0, 20, 10));
        painter.draw(
            std::path::Path::new("/board.png"),
            &image,
            buf.area,
            &mut buf,
        );
        painter.run_jobs_now();
        painter.draw(
            std::path::Path::new("/board.png"),
            &image,
            buf.area,
            &mut buf,
        );
        let area = painter.take_drawn().unwrap().area;
        for y in area.top()..area.bottom() {
            for x in area.left()..area.right() {
                for color in [buf[(x, y)].fg, buf[(x, y)].bg] {
                    let ratatui::style::Color::Rgb(r, _, _) = color else {
                        panic!("expected rgb, got {color:?}")
                    };
                    assert!((60..=195).contains(&r), "cell {x},{y} is {r}, not grey");
                }
            }
        }
    }

    #[test]
    fn a_picture_fills_the_room_keeping_its_shape_and_icons_grow_at_most_twice() {
        let painter = Painter::halfblocks();
        let big = painter
            .fitted_size(&picture(2000, 1000), Size::new(40, 40))
            .unwrap();
        assert_eq!(big.width, 40);
        assert!(big.height < 40, "keeps the 2:1 shape: {big:?}");
        let icon = painter
            .fitted_size(&picture(40, 40), Size::new(100, 100))
            .unwrap();
        assert!(
            icon.width > 4,
            "a 40 px icon is 4 cells wide at 10 px a cell, so it grows: {icon:?}"
        );
        assert!(icon.width <= 8, "but no more than twice: {icon:?}");
    }

    fn env(pairs: &'static [(&'static str, &'static str)]) -> impl Fn(&str) -> Option<String> {
        move |k| {
            pairs
                .iter()
                .find(|(n, _)| *n == k)
                .map(|(_, v)| v.to_string())
        }
    }

    fn answers(kitty: bool, sixel: bool, name: Option<&str>) -> Answers {
        Answers {
            kitty,
            kitty_files: false,
            kitty_zlib: false,
            sixel,
            name: name.map(String::from),
            cell: None,
            complete: true,
        }
    }

    fn method(
        mode: Mode,
        a: Option<&Answers>,
        vars: &'static [(&'static str, &'static str)],
    ) -> Method {
        choose(mode, a, env(vars)).0
    }

    #[test]
    fn the_terminals_answers_pick_the_protocol() {
        use ProtocolType::*;
        let xterm = &[("TERM", "xterm-256color")];
        assert_eq!(
            method(
                Mode::Auto,
                Some(&answers(true, false, Some("kitty(0.35)"))),
                xterm
            ),
            Method::Graphics(Kitty)
        );
        assert_eq!(
            method(
                Mode::Auto,
                Some(&answers(false, true, Some("XTerm(390)"))),
                xterm
            ),
            Method::Graphics(Sixel)
        );
        assert_eq!(
            method(
                Mode::Auto,
                Some(&answers(true, true, Some("WezTerm 2024"))),
                xterm
            ),
            Method::Graphics(Iterm2),
            "WezTerm lacks kitty placeholders"
        );
        assert_eq!(
            method(
                Mode::Auto,
                Some(&answers(true, true, Some("Konsole 23.08"))),
                xterm
            ),
            Method::Graphics(Sixel),
            "so does Konsole"
        );
        assert_eq!(
            method(
                Mode::Auto,
                Some(&answers(false, false, Some("VTE(7600)"))),
                xterm
            ),
            Method::Blocks(BlockKind::Quadrants)
        );
    }

    #[test]
    fn without_answers_the_environment_decides_and_blocks_are_the_fallback() {
        use ProtocolType::*;
        assert_eq!(
            method(Mode::Auto, None, &[("TERM", "xterm-kitty")]),
            Method::Graphics(Kitty)
        );
        assert_eq!(
            method(Mode::Auto, None, &[("TERM", "foot")]),
            Method::Graphics(Sixel)
        );
        assert_eq!(
            method(Mode::Auto, None, &[("TERM_PROGRAM", "iTerm.app")]),
            Method::Graphics(Iterm2)
        );
        assert_eq!(
            method(Mode::Auto, None, &[("TERM", "xterm-256color")]),
            Method::Blocks(BlockKind::Quadrants)
        );
    }

    #[test]
    fn kitty_started_from_iterm2_is_kitty_despite_the_inherited_variables() {
        let iterm2_env = &[("TERM", "xterm-kitty"), ("TERM_PROGRAM", "iTerm.app")];
        assert_eq!(
            method(
                Mode::Auto,
                Some(&answers(true, false, Some("kitty(0.49.1)"))),
                iterm2_env
            ),
            Method::Graphics(ProtocolType::Kitty)
        );
        assert_eq!(
            method(
                Mode::Auto,
                Some(&answers(false, false, Some("tmux 3.4"))),
                &[("TERM", "tmux-256color"), ("TERM_PROGRAM", "iTerm.app")]
            ),
            Method::Graphics(ProtocolType::Iterm2),
            "inside tmux the variables are all there is"
        );
    }

    #[test]
    fn iterm2_is_recognised_over_ssh_by_lc_terminal() {
        assert_eq!(
            method(
                Mode::Auto,
                None,
                &[("TERM", "xterm-256color"), ("LC_TERMINAL", "iTerm2")]
            ),
            Method::Graphics(ProtocolType::Iterm2)
        );
    }

    #[test]
    fn tmux_passes_graphics_on_only_when_the_library_can_wrap_them() {
        use ProtocolType::*;
        let kitty = answers(true, false, Some("kitty(0.35)"));
        assert_eq!(
            method(
                Mode::Auto,
                Some(&kitty),
                &[("TMUX", "/tmp/t"), ("TERM", "tmux-256color")]
            ),
            Method::Graphics(Kitty)
        );
        assert_eq!(
            method(
                Mode::Auto,
                Some(&kitty),
                &[("TMUX", "/tmp/t"), ("TERM", "screen-256color")]
            ),
            Method::Blocks(BlockKind::Quadrants)
        );
    }

    #[test]
    fn inside_tmux_its_own_sixel_answer_is_not_trusted() {
        let tmux_says_sixel = answers(false, true, None);
        assert_eq!(
            method(
                Mode::Auto,
                Some(&tmux_says_sixel),
                &[("TMUX", "/tmp/t"), ("TERM", "tmux-256color")]
            ),
            Method::Blocks(BlockKind::Quadrants)
        );
    }

    #[test]
    fn the_config_overrides_detection_and_the_reason_says_why() {
        let (m, why) = choose(Mode::Sixel, None, env(&[("TERM", "xterm")]));
        assert_eq!(m, Method::Graphics(ProtocolType::Sixel));
        assert!(why.contains("set in the config"), "{why}");
        let (_, why) = choose(Mode::Auto, None, env(&[("TERM", "xterm")]));
        assert!(why.contains("did not answer"), "{why}");
        let (_, why) = choose(
            Mode::Auto,
            Some(&answers(false, true, Some("foot(1.16)"))),
            env(&[]),
        );
        assert!(why.contains("foot(1.16)"), "{why}");
    }

    #[test]
    fn a_cell_splits_into_the_two_colours_that_fit_its_quarters_best() {
        let (r, b) = ((250, 0, 0), (0, 0, 250));
        let cell = best_quadrant([r, b, r, b]);
        assert!(matches!(cell.ch, '▌' | '▐'), "a vertical split: {cell:?}");
        let cell = best_quadrant([r, r, b, b]);
        assert!(matches!(cell.ch, '▀' | '▄'), "a horizontal split: {cell:?}");
        let cell = best_quadrant([r, b, b, b]);
        assert_eq!(cell.ch, '▘');
        assert_eq!((cell.fg, cell.bg), (r, b));
        let flat = best_quadrant([r, r, r, r]);
        assert_eq!((flat.ch, flat.bg), (' ', r));
    }

    #[test]
    fn quadrants_show_twice_the_detail_of_half_blocks_across() {
        let stripes = image::RgbImage::from_fn(40, 40, |x, _| {
            if (x / 10) % 2 == 0 {
                image::Rgb([255, 255, 255])
            } else {
                image::Rgb([0, 0, 0])
            }
        });
        let image = DynamicImage::ImageRgb8(stripes);
        let quads = encode_blocks(&image, Size::new(2, 1), BlockKind::Quadrants);
        assert!(
            quads.iter().all(|c| matches!(c.ch, '▌' | '▐')),
            "each cell holds a white and a black stripe: {quads:?}"
        );
        let halves = encode_blocks(&image, Size::new(2, 1), BlockKind::Halves);
        assert!(
            halves.iter().all(|c| c.ch == ' '),
            "half blocks cannot split a cell sideways"
        );
    }

    #[test]
    fn transparent_pixels_take_the_page_background() {
        let clear = DynamicImage::ImageRgba8(RgbaImage::from_pixel(4, 4, Rgba([255, 255, 255, 0])));
        let cells = encode_blocks(&clear, Size::new(1, 1), BlockKind::Quadrants);
        assert_eq!(cells[0].bg, crate::theme::BG);
    }

    #[test]
    fn switched_off_draws_nothing() {
        let painter = Painter::off();
        let mut buf = Buffer::empty(Rect::new(0, 0, 10, 10));
        painter.draw(
            std::path::Path::new("/p.png"),
            &picture(8, 8),
            buf.area,
            &mut buf,
        );
        assert!(painter.take_drawn().is_none());
        assert!(!painter.enabled());
        assert!(
            painter.describe().starts_with("pictures: off"),
            "{}",
            painter.describe()
        );
    }

    #[test]
    fn a_zoomed_font_resizes_pictures_and_padding_alone_does_not() {
        let mut painter = graphics(ProtocolType::Kitty);
        // Like Ghostty: the window says 16x37 with its padding, the terminal says 16x35.
        painter.font = FontSize::new(16, 35);
        painter.encoder.font = painter.font;
        painter.measured = Some(((16, 37), painter.font));
        let image = picture(400, 200);
        let mut buf = Buffer::empty(Rect::new(0, 0, 40, 20));
        painter.draw(std::path::Path::new("/p.png"), &image, buf.area, &mut buf);
        painter.run_jobs_now();
        assert!(
            !painter.rescale(Some((16, 38))),
            "a resize moves the padding a little"
        );
        assert_eq!(painter.cache.borrow().len(), 1);
        assert!(painter.rescale(Some((20, 46))), "zoomed in");
        assert_eq!((painter.font.width, painter.font.height), (20, 43));
        assert!(
            painter.cache.borrow().is_empty(),
            "encoded again for the new cells"
        );
        assert!(
            !graphics(ProtocolType::Kitty).rescale(Some((20, 46))),
            "nothing to go by"
        );
    }

    #[test]
    fn implausible_cell_sizes_are_ignored() {
        assert_eq!(
            plausible_cell(&[Some((9, 18)), Some((2, 6))]),
            (9, 18),
            "the terminal's own answer wins"
        );
        assert_eq!(
            plausible_cell(&[None, Some((2, 6))]),
            DEFAULT_CELL,
            "a cell 6 px tall is not a font"
        );
        assert_eq!(plausible_cell(&[Some((0, 0)), Some((8, 17))]), (8, 17));
        assert_eq!(plausible_cell(&[None, None]), DEFAULT_CELL);
    }

    #[test]
    fn only_sixel_and_iterm2_need_a_repaint_to_clear() {
        assert!(!Painter::blocks().leaves_ghosts());
        assert!(!Painter::off().leaves_ghosts());
        let sixel = Painter::build(
            Method::Graphics(ProtocolType::Sixel),
            FontSize::new(10, 20),
            String::new(),
        );
        assert!(sixel.leaves_ghosts());
    }

    fn graphics(protocol: ProtocolType) -> Painter {
        Painter::build(
            Method::Graphics(protocol),
            FontSize::new(10, 20),
            String::new(),
        )
    }

    fn sent(painter: &Painter, image: &ImageData, size: Size) -> Encoded {
        let (_, encoded) = EncodeJob {
            key: (PathBuf::new(), image.clone(), size),
            size,
            encoder: painter.encoder.clone(),
        }
        .run();
        encoded.expect("encoded")
    }

    fn inline_data(sequence: &str) -> Vec<u8> {
        let data = sequence
            .rsplit_once(':')
            .unwrap()
            .1
            .trim_end_matches('\x07');
        base64_simd::STANDARD.decode_to_vec(data).unwrap()
    }

    #[test]
    fn iterm2_gets_a_small_jpeg_stretched_over_the_cells_by_the_terminal() {
        let size = Size::new(20, 5);
        let Encoded::Inline { sequence, .. } = sent(
            &graphics(ProtocolType::Iterm2),
            &ImageData(Arc::new(DynamicImage::ImageRgb8(image::RgbImage::new(
                800, 400,
            )))),
            size,
        ) else {
            panic!("an inline image");
        };
        assert!(sequence.contains("width=20;height=5;"), "{sequence:.200}");
        let jpeg = inline_data(&sequence);
        assert_eq!(
            image::guess_format(&jpeg).unwrap(),
            image::ImageFormat::Jpeg
        );
        let shown = image::load_from_memory(&jpeg).unwrap();
        assert!(
            shown.width() <= 200 && shown.height() <= 100,
            "never more pixels than the cells hold: {}x{}",
            shown.width(),
            shown.height()
        );
    }

    #[test]
    fn only_a_picture_with_see_through_parts_stays_a_png() {
        let iterm2 = graphics(ProtocolType::Iterm2);
        let format = |image: DynamicImage| {
            let Encoded::Inline { sequence, .. } =
                sent(&iterm2, &ImageData(Arc::new(image)), Size::new(4, 1))
            else {
                panic!("an inline image");
            };
            image::guess_format(&inline_data(&sequence)).unwrap()
        };
        let mut clear = RgbaImage::from_pixel(40, 20, Rgba([255, 0, 0, 255]));
        clear.put_pixel(3, 3, Rgba([0, 0, 0, 0]));
        assert_eq!(
            format(DynamicImage::ImageRgba8(clear)),
            image::ImageFormat::Png
        );
        let opaque = RgbaImage::from_pixel(40, 20, Rgba([255, 0, 0, 255]));
        assert_eq!(
            format(DynamicImage::ImageRgba8(opaque)),
            image::ImageFormat::Jpeg,
            "a screenshot with an alpha channel it does not use"
        );
    }

    #[test]
    fn kitty_gets_the_pixels_as_decoded_and_scales_them_over_the_cells_itself() {
        let painter = graphics(ProtocolType::Kitty);
        let image = ImageData(Arc::new(DynamicImage::ImageRgb8(image::RgbImage::new(
            300, 200,
        ))));
        let Encoded::Kitty { id, upload, .. } = sent(&painter, &image, Size::new(60, 20)) else {
            panic!("a kitty image");
        };
        let Upload::Direct(sequence) = upload else {
            panic!("sent through the connection");
        };
        assert!(
            sequence.starts_with(&format!(
                "\x1b_Ga=T,U=1,i={id},f=24,s=300,v=200,c=60,r=20,q=2,m=1;"
            )),
            "three bytes a pixel, no enlarging here: {sequence:.80}"
        );
        let base64: usize = sequence
            .split("\x1b_G")
            .filter_map(|c| c.split_once(';'))
            .map(|(_, d)| d.trim_end_matches("\x1b\\").len())
            .sum();
        assert_eq!(base64, (300 * 200 * 3usize).div_ceil(3) * 4);
    }

    #[test]
    fn kitty_through_a_connection_gets_the_pixels_deflated_when_it_can_inflate_them() {
        let mut painter = graphics(ProtocolType::Kitty);
        painter.encoder.kitty_zlib = true;
        let flat = ImageData(Arc::new(DynamicImage::ImageRgb8(image::RgbImage::new(
            300, 200,
        ))));
        let Encoded::Kitty {
            upload: Upload::Direct(sequence),
            ..
        } = sent(&painter, &flat, Size::new(60, 20))
        else {
            panic!("sent through the connection");
        };
        assert!(sequence.contains(",o=z,m=0;"), "{sequence:.120}");
        assert!(
            sequence.len() < 10_000,
            "240 KB of base64 raw, almost nothing deflated"
        );
    }

    #[test]
    fn a_kitty_picture_is_uploaded_once_and_again_after_the_terminal_may_have_lost_it() {
        let painter = graphics(ProtocolType::Kitty);
        let image = picture(40, 20);
        let mut buf = Buffer::empty(Rect::new(0, 0, 20, 10));
        let path = std::path::Path::new("/p.png");
        let draw = |buf: &mut Buffer| {
            painter.draw(path, &image, buf.area, buf);
            buf[(0, 0)].symbol().len()
        };
        draw(&mut buf);
        painter.run_jobs_now();
        assert!(draw(&mut buf) > 100, "the first frame carries the pixels");
        assert!(draw(&mut buf) < 20, "later ones only the placeholder");
        painter.forget_uploads();
        assert!(draw(&mut buf) > 100, "uploaded again");
    }

    #[test]
    fn kitty_on_this_machine_reads_the_pixels_from_a_temporary_file() {
        let mut painter = graphics(ProtocolType::Kitty);
        painter.encoder.kitty_files = true;
        let image = picture(40, 20);
        let mut buf = Buffer::empty(Rect::new(0, 0, 20, 10));
        let path = std::path::Path::new("/p.png");
        painter.draw(path, &image, buf.area, &mut buf);
        painter.run_jobs_now();
        painter.draw(path, &image, buf.area, &mut buf);
        let first = buf[(0, 0)].symbol().to_string();
        assert!(first.contains(",t=t;"), "{first:?}");
        let name = first
            .split_once(";")
            .unwrap()
            .1
            .split_once('\x1b')
            .unwrap()
            .0;
        let file = String::from_utf8(base64_simd::STANDARD.decode_to_vec(name).unwrap()).unwrap();
        assert!(
            file.contains("tty-graphics-protocol"),
            "kitty insists on it: {file}"
        );
        assert_eq!(
            std::fs::read(&file).unwrap().len(),
            40 * 20 * 4,
            "RGBA, it has alpha"
        );
        std::fs::remove_file(file).unwrap();
        assert!(
            !painter.remote(true).encoder.kitty_files,
            "no files across a network"
        );
    }

    #[test]
    fn the_last_picture_of_a_file_stays_until_the_better_one_is_encoded() {
        let painter = graphics(ProtocolType::Iterm2);
        let path = std::path::Path::new("/photo.jpg");
        let (quick, full) = (picture(80, 40), picture(160, 80));
        let mut buf = Buffer::empty(Rect::new(0, 0, 40, 20));
        painter.draw(path, &quick, buf.area, &mut buf);
        painter.run_jobs_now();
        painter.draw(path, &quick, buf.area, &mut buf);
        assert!(painter.take_drawn().is_some());
        painter.draw(path, &full, buf.area, &mut buf);
        assert!(
            painter.take_drawn().is_some(),
            "the camera's preview is shown while the photo is encoded"
        );
        painter.run_jobs_now();
        assert_eq!(painter.cache.borrow().len(), 1, "and dropped once it is");
    }

    #[test]
    fn kitty_is_sent_at_half_resolution_over_a_slow_link_and_full_otherwise() {
        let local = graphics(ProtocolType::Kitty);
        let remote = graphics(ProtocolType::Kitty).remote(true);
        assert_eq!(local.decode_target(100, 50), (1000, 1000));
        assert_eq!(remote.decode_target(100, 50), (500, 500));
        assert_eq!(
            remote.describe(),
            local.describe(),
            "the layout keeps the real cell size"
        );
        let sixel = graphics(ProtocolType::Sixel).remote(true);
        assert_eq!(
            sixel.decode_target(100, 50),
            (600, 600),
            "sixel is drawn smaller"
        );
        let fitted = sixel
            .fitted_size(&picture(1000, 1000), Size::new(100, 50))
            .unwrap();
        assert_eq!(fitted, Size::new(60, 30), "60% of the room, in cells");
    }
}
