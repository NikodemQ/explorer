//! Decoding through macOS ImageIO, which scales a JPEG while decoding it and does so about twice as
//! fast as the decoders in Rust. It also reads HEIC, which they cannot.

use std::{ffi::c_void, ptr::null};

use image::{DynamicImage, RgbImage};

type Ref = *const c_void;

#[link(name = "CoreFoundation", kind = "framework")]
unsafe extern "C" {
    fn CFDataCreateWithBytesNoCopy(a: Ref, bytes: *const u8, len: isize, free: Ref) -> Ref;
    fn CFNumberCreate(a: Ref, kind: isize, value: *const c_void) -> Ref;
    fn CFDictionaryCreate(a: Ref, k: *const Ref, v: *const Ref, n: isize, kc: Ref, vc: Ref) -> Ref;
    fn CFRelease(r: Ref);
    static kCFBooleanTrue: Ref;
    static kCFAllocatorNull: Ref;
    static kCFTypeDictionaryKeyCallBacks: c_void;
    static kCFTypeDictionaryValueCallBacks: c_void;
}

#[link(name = "ImageIO", kind = "framework")]
unsafe extern "C" {
    fn CGImageSourceCreateWithData(data: Ref, options: Ref) -> Ref;
    fn CGImageSourceCreateThumbnailAtIndex(source: Ref, index: usize, options: Ref) -> Ref;
    static kCGImageSourceThumbnailMaxPixelSize: Ref;
    static kCGImageSourceCreateThumbnailFromImageAlways: Ref;
    static kCGImageSourceCreateThumbnailWithTransform: Ref;
    static kCGImageSourceShouldCacheImmediately: Ref;
}

#[link(name = "CoreGraphics", kind = "framework")]
unsafe extern "C" {
    fn CGImageGetWidth(image: Ref) -> usize;
    fn CGImageGetHeight(image: Ref) -> usize;
    fn CGColorSpaceCreateWithName(name: Ref) -> Ref;
    fn CGBitmapContextCreate(
        data: *mut c_void,
        width: usize,
        height: usize,
        bits: usize,
        row: usize,
        space: Ref,
        info: u32,
    ) -> Ref;
    fn CGContextDrawImage(context: Ref, rect: CGRect, image: Ref);
    fn CGImageRelease(image: Ref);
    static kCGColorSpaceSRGB: Ref;
}

/// Origin and size, as CoreGraphics lays it out.
#[repr(C)]
struct CGRect([f64; 4]);

const CF_NUMBER_SINT32: isize = 3;
/// Red, green, blue and an unused byte.
const ALPHA_NONE_SKIP_LAST: u32 = 5;

/// The JPEG or HEIC picture in `data`, upright and no longer than `longest` pixels on either side,
/// in sRGB.
pub fn thumbnail(data: &[u8], longest: u32) -> Option<DynamicImage> {
    // SAFETY: the bytes are lent, not copied, and outlive every object made from them, all of
    // which are checked for null and released before returning.
    unsafe {
        let bytes = CFDataCreateWithBytesNoCopy(
            null(),
            data.as_ptr(),
            data.len() as isize,
            kCFAllocatorNull,
        );
        if bytes.is_null() {
            return None;
        }
        let source = CGImageSourceCreateWithData(bytes, null());
        let image = from_source(source, longest);
        CFRelease(bytes);
        image
    }
}

/// The first use of ImageIO loads its codecs, about 150 ms. Doing that ahead keeps it off the first picture.
pub fn warm() {
    let mut jpeg = Vec::new();
    let tiny = DynamicImage::ImageRgb8(RgbImage::new(8, 8));
    if tiny
        .write_to(
            &mut std::io::Cursor::new(&mut jpeg),
            image::ImageFormat::Jpeg,
        )
        .is_ok()
    {
        thumbnail(&jpeg, 8);
    }
}

/// Takes ownership of `source`.
unsafe fn from_source(source: Ref, longest: u32) -> Option<DynamicImage> {
    if source.is_null() {
        return None;
    }
    unsafe {
        let longest = i32::try_from(longest.max(1)).unwrap_or(i32::MAX);
        let number = CFNumberCreate(null(), CF_NUMBER_SINT32, (&raw const longest).cast());
        let keys = [
            kCGImageSourceThumbnailMaxPixelSize,
            kCGImageSourceCreateThumbnailFromImageAlways,
            kCGImageSourceCreateThumbnailWithTransform,
            kCGImageSourceShouldCacheImmediately,
        ];
        let values = [number, kCFBooleanTrue, kCFBooleanTrue, kCFBooleanTrue];
        let options = CFDictionaryCreate(
            null(),
            keys.as_ptr(),
            values.as_ptr(),
            keys.len() as isize,
            (&raw const kCFTypeDictionaryKeyCallBacks).cast(),
            (&raw const kCFTypeDictionaryValueCallBacks).cast(),
        );
        let picture = CGImageSourceCreateThumbnailAtIndex(source, 0, options);
        CFRelease(options);
        CFRelease(number);
        CFRelease(source);
        if picture.is_null() {
            return None;
        }
        let (w, h) = (CGImageGetWidth(picture), CGImageGetHeight(picture));
        let mut pixels = vec![0u8; w * h * 4];
        let space = CGColorSpaceCreateWithName(kCGColorSpaceSRGB);
        let context = CGBitmapContextCreate(
            pixels.as_mut_ptr().cast(),
            w,
            h,
            8,
            w * 4,
            space,
            ALPHA_NONE_SKIP_LAST,
        );
        if !context.is_null() {
            CGContextDrawImage(context, CGRect([0.0, 0.0, w as f64, h as f64]), picture);
            CFRelease(context);
        }
        if !space.is_null() {
            CFRelease(space);
        }
        CGImageRelease(picture);
        if context.is_null() {
            return None;
        }
        let rgb: Vec<u8> = pixels
            .as_chunks::<4>()
            .0
            .iter()
            .flat_map(|&[r, g, b, _]| [r, g, b])
            .collect();
        RgbImage::from_raw(w as u32, h as u32, rgb).map(DynamicImage::ImageRgb8)
    }
}
