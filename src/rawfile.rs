//! Finds the JPEG a camera raw file carries for display. Developing the raw data itself takes
//! seconds; the camera already stored a finished picture of the same photo, often at full size.

use std::{fs::File, os::unix::fs::FileExt, path::Path};

/// A JPEG inside a file, and the orientation the file asks for when the JPEG does not say.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Embedded {
    pub offset: u64,
    pub len: u64,
    /// The EXIF orientation value, 1 to 8.
    pub orientation: Option<u16>,
}

const RAF_MAGIC: &[u8] = b"FUJIFILMCCD-RAW ";

/// Raw formats built on TIFF. Plain TIFF files are left to the image library.
const TIFF_RAWS: [&str; 10] = [
    "nef", "nrw", "arw", "sr2", "dng", "cr2", "pef", "srw", "erf", "3fr",
];

pub fn is_raf(head: &[u8]) -> bool {
    head.starts_with(RAF_MAGIC)
}

/// The JPEG to show for a camera raw: the smallest one that fills `target`, or else the largest.
/// `None` for files that are not camera raws, or carry no JPEG worth showing.
pub fn locate(path: &Path, head: &[u8], target: (u32, u32)) -> Option<Embedded> {
    let raf = is_raf(head);
    let extension = path
        .extension()
        .and_then(|e| e.to_str())
        .map(str::to_ascii_lowercase)
        .unwrap_or_default();
    if !raf && !TIFF_RAWS.contains(&extension.as_str()) {
        return None;
    }
    let file = File::open(path).ok()?;
    let candidates = if raf {
        // Fuji puts the offset and length of a full-size JPEG at a fixed place in the header.
        let at = |i: usize| {
            head.get(i..i + 4)
                .map(|b| u64::from(u32::from_be_bytes(b.try_into().unwrap())))
        };
        vec![(at(0x54)?, at(0x58)?, None)]
    } else {
        tiff_jpegs(&file)?
    };
    let mut sized: Vec<(Embedded, (u32, u32))> = candidates
        .into_iter()
        .filter_map(|(offset, len, orientation)| {
            // Cameras put a long EXIF block before the frame header that gives the size.
            let mut start = vec![0u8; len.min(256 * 1024) as usize];
            file.read_exact_at(&mut start, offset).ok()?;
            let size = displayable_size(&start)?;
            Some((
                Embedded {
                    offset,
                    len,
                    orientation,
                },
                size,
            ))
        })
        .collect();
    sized.sort_by_key(|(_, (w, h))| u64::from(*w) * u64::from(*h));
    let covers = |(w, h): (u32, u32)| w >= target.0 || h >= target.1;
    sized
        .iter()
        .find(|(_, size)| covers(*size))
        .or(sized.last())
        .map(|(embedded, _)| *embedded)
}

/// The size of a JPEG that an ordinary decoder can read: not the lossless kind raw data is kept in.
fn displayable_size(data: &[u8]) -> Option<(u32, u32)> {
    if !data.starts_with(b"\xFF\xD8\xFF") {
        return None;
    }
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
        // Baseline, extended and progressive frames. Lossless ones (C3, C7, CB, CF) hold raw data.
        if (0xC0..=0xC2).contains(&marker) {
            let h = u32::from(u16::from_be_bytes([data[at + 5], data[at + 6]]));
            let w = u32::from(u16::from_be_bytes([data[at + 7], data[at + 8]]));
            return (w > 0 && h > 0).then_some((w, h));
        }
        if (0xC3..=0xCF).contains(&marker) && !matches!(marker, 0xC4 | 0xC8 | 0xCC) {
            return None;
        }
        at += 2 + len;
    }
    None
}

/// Every JPEG a TIFF-based raw points to, from its directories and their sub-directories, with the
/// orientation of the first directory.
fn tiff_jpegs(file: &File) -> Option<Vec<(u64, u64, Option<u16>)>> {
    let mut header = [0u8; 8];
    file.read_exact_at(&mut header, 0).ok()?;
    let little = match &header[..4] {
        b"II*\0" => true,
        b"MM\0*" => false,
        _ => return None,
    };
    let u16_at = |b: &[u8]| {
        let b = [b[0], b[1]];
        if little {
            u16::from_le_bytes(b)
        } else {
            u16::from_be_bytes(b)
        }
    };
    let u32_at = |b: &[u8]| {
        let b = [b[0], b[1], b[2], b[3]];
        if little {
            u32::from_le_bytes(b)
        } else {
            u32::from_be_bytes(b)
        }
    };
    let mut queue = vec![u64::from(u32_at(&header[4..]))];
    let mut seen = Vec::new();
    let mut found = Vec::new();
    let mut orientation = None;
    // A hostile file could chain directories in a loop or without end.
    while let Some(ifd) = queue.pop() {
        if ifd == 0 || seen.contains(&ifd) || seen.len() >= 32 {
            continue;
        }
        seen.push(ifd);
        let mut count = [0u8; 2];
        file.read_exact_at(&mut count, ifd).ok()?;
        let count = usize::from(u16_at(&count)).min(512);
        let mut entries = vec![0u8; count * 12 + 4];
        file.read_exact_at(&mut entries, ifd + 2).ok()?;
        let (mut jpeg, mut jpeg_len, mut strip, mut strip_len, mut compression) =
            (None, None, None, None, 0);
        for entry in entries[..count * 12].as_chunks::<12>().0 {
            let (tag, kind, n) = (u16_at(entry), u16_at(&entry[2..]), u32_at(&entry[4..]));
            // A single short or long, stored in the entry itself.
            let value = match kind {
                3 => u32::from(u16_at(&entry[8..])),
                _ => u32_at(&entry[8..]),
            };
            let one = n == 1;
            match tag {
                0x103 => compression = value,
                0x112 if one && seen.len() == 1 => orientation = Some(value as u16),
                0x111 if one => strip = Some(value),
                0x117 if one => strip_len = Some(value),
                0x201 => jpeg = Some(value),
                0x202 => jpeg_len = Some(value),
                0x14A if one => queue.push(u64::from(value)),
                0x14A => {
                    let mut offsets = vec![0u8; (n.min(16) * 4) as usize];
                    if file.read_exact_at(&mut offsets, u64::from(value)).is_ok() {
                        queue.extend(
                            offsets
                                .as_chunks::<4>()
                                .0
                                .iter()
                                .map(|o| u64::from(u32_at(o))),
                        );
                    }
                }
                _ => {}
            }
        }
        if let (Some(at), Some(len)) = (jpeg, jpeg_len) {
            found.push((u64::from(at), u64::from(len)));
        }
        // Old-style and new-style JPEG compression of the whole directory in one strip.
        if let (6 | 7, Some(at), Some(len)) = (compression, strip, strip_len) {
            found.push((u64::from(at), u64::from(len)));
        }
        queue.push(u64::from(u32_at(&entries[count * 12..])));
    }
    Some(
        found
            .into_iter()
            .filter(|(_, len)| *len > 0)
            .map(|(at, len)| (at, len, orientation))
            .collect(),
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    fn jpeg(width: u32, height: u32) -> Vec<u8> {
        let mut bytes = Vec::new();
        image::DynamicImage::ImageRgb8(image::RgbImage::new(width, height))
            .write_to(
                &mut std::io::Cursor::new(&mut bytes),
                image::ImageFormat::Jpeg,
            )
            .unwrap();
        bytes
    }

    /// A little-endian TIFF the way a Nikon lays it out: a thumbnail in the first directory and the
    /// full-size JPEG in a sub-directory, with the orientation in the first.
    fn nef(thumb: &[u8], full: &[u8], orientation: u16) -> Vec<u8> {
        let entry = |tag: u16, kind: u16, value: u32| {
            let mut e = Vec::new();
            e.extend(tag.to_le_bytes());
            e.extend(kind.to_le_bytes());
            e.extend(1u32.to_le_bytes());
            e.extend(value.to_le_bytes());
            e
        };
        let ifd0 = 8u32;
        let ifd0_len = 2 + 4 * 12 + 4;
        let sub = ifd0 + ifd0_len;
        let sub_len = 2 + 2 * 12 + 4;
        let thumb_at = sub + sub_len;
        let full_at = thumb_at + thumb.len() as u32;
        let mut out = b"II*\0".to_vec();
        out.extend(ifd0.to_le_bytes());
        out.extend(4u16.to_le_bytes());
        out.extend(entry(0x112, 3, u32::from(orientation)));
        out.extend(entry(0x14A, 4, sub));
        out.extend(entry(0x201, 4, thumb_at));
        out.extend(entry(0x202, 4, thumb.len() as u32));
        out.extend(0u32.to_le_bytes());
        out.extend(2u16.to_le_bytes());
        out.extend(entry(0x201, 4, full_at));
        out.extend(entry(0x202, 4, full.len() as u32));
        out.extend(0u32.to_le_bytes());
        out.extend(thumb);
        out.extend(full);
        out
    }

    fn file(name: &str, bytes: &[u8]) -> (crate::testdir::TestDir, std::path::PathBuf) {
        let dir = crate::testdir::tempdir();
        let path = dir.path().join(name);
        std::fs::write(&path, bytes).unwrap();
        (dir, path)
    }

    #[test]
    fn a_nef_offers_its_full_size_jpeg_and_its_orientation() {
        let (thumb, full) = (jpeg(160, 120), jpeg(1200, 800));
        let (_d, path) = file("a.NEF", &nef(&thumb, &full, 6));
        let head = std::fs::read(&path).unwrap();
        let found = locate(&path, &head, (1000, 1000)).expect("a picture");
        assert_eq!(found.len, full.len() as u64, "the big one");
        assert_eq!(found.orientation, Some(6));
        let small = locate(&path, &head, (100, 100)).unwrap();
        assert_eq!(
            small.len,
            thumb.len() as u64,
            "the smallest that fills the room"
        );
        let (_d, tif) = file("a.tif", &nef(&thumb, &full, 1));
        assert!(
            locate(&tif, &head, (100, 100)).is_none(),
            "plain TIFF is not a raw"
        );
    }

    #[test]
    fn a_nef_is_shown_from_its_jpeg_turned_the_way_the_raw_asks() {
        let (_d, path) = file("turned.nef", &nef(&jpeg(160, 120), &jpeg(1200, 800), 6));
        let content = crate::preview::build(&path).unwrap();
        let image = content.image.expect("a picture, not a hex dump");
        assert!(
            image.0.height() > image.0.width() && image.0.height() > 120,
            "the full-size JPEG, upright: {}x{}",
            image.0.width(),
            image.0.height()
        );
    }

    #[test]
    fn a_raf_points_at_its_jpeg_from_the_header() {
        let full = jpeg(600, 400);
        let mut raf = RAF_MAGIC.to_vec();
        raf.resize(0x94, 0);
        raf[0x54..0x58].copy_from_slice(&0x94u32.to_be_bytes());
        raf[0x58..0x5C].copy_from_slice(&(full.len() as u32).to_be_bytes());
        raf.extend(&full);
        raf.extend([7u8; 1000]);
        let (_d, path) = file("a.RAF", &raf);
        let found = locate(&path, &raf, (1000, 1000)).unwrap();
        assert_eq!((found.offset, found.len), (0x94, full.len() as u64));
    }

    #[test]
    fn lossless_raw_data_and_looping_directories_are_refused() {
        let mut lossless = jpeg(10, 10);
        let sof = lossless.windows(2).position(|w| w == [0xFF, 0xC0]).unwrap();
        lossless[sof + 1] = 0xC3;
        assert!(displayable_size(&lossless).is_none());
        let mut looped = b"II*\0\x08\0\0\0\x00\x00\x08\0\0\0".to_vec();
        looped.resize(64, 0);
        let (_d, path) = file("loop.dng", &looped);
        assert_eq!(locate(&path, &looped, (1, 1)), None);
    }
}
