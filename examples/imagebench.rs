//! Times each stage of showing a large picture. Run: cargo run --release --example imagebench -- [path]
use std::{path::PathBuf, time::Instant};

fn main() {
    let path = std::env::args_os()
        .nth(1)
        .map(PathBuf::from)
        .unwrap_or_else(|| {
            let path = std::env::temp_dir().join("tx-bench-40mp-noisy.jpg");
            if !path.exists() {
                let (w, h) = (7728u32, 5152u32);
                let mut seed = 1u32;
                let img = image::RgbImage::from_fn(w, h, |x, y| {
                    seed ^= seed << 13;
                    seed ^= seed >> 17;
                    seed ^= seed << 5;
                    let n = (seed & 63) as u8;
                    image::Rgb([
                        (x * 190 / w) as u8 + n,
                        (y * 190 / h) as u8 + n,
                        ((x ^ y) & 0x7f) as u8 + n,
                    ])
                });
                img.save(&path).unwrap();
            }
            path
        });
    println!(
        "file: {} ({} MB)",
        path.display(),
        std::fs::metadata(&path).unwrap().len() / 1_000_000
    );
    let t = Instant::now();
    match image::open(&path) {
        Ok(full) => println!(
            "before: full decode alone: {:?} ({}x{})",
            t.elapsed(),
            full.width(),
            full.height()
        ),
        Err(e) => println!("before: the image library cannot read it: {e}"),
    }
    // A 220x55 terminal with the folders taking half the width, as by default.
    let screen = (220u16, 55u16);
    println!(
        "{}",
        tx::imageview::Painter::new(tx::imageview::Mode::Sixel, None).describe()
    );
    for (name, painter) in [
        ("blocks", tx::imageview::Painter::blocks()),
        (
            "sixel",
            tx::imageview::Painter::new(tx::imageview::Mode::Sixel, None),
        ),
        (
            "kitty",
            tx::imageview::Painter::new(tx::imageview::Mode::Kitty, None),
        ),
        (
            "iterm2",
            tx::imageview::Painter::new(tx::imageview::Mode::Iterm2, None),
        ),
    ] {
        let (w, h) = painter.decode_target(screen.0 / 2, screen.1 - 2);
        tx::preview::set_image_target(w, h);
        // What a photo's own header preview costs, for the first paint.
        let t = Instant::now();
        match tx::preview::build_quick(&path) {
            Some(content) => {
                let quick = content.image.unwrap();
                println!(
                    "{name}: first paint from the file header: {:?} ({}x{})",
                    t.elapsed(),
                    quick.0.width(),
                    quick.0.height()
                );
            }
            None => println!("{name}: no header preview needed for the first paint"),
        }
        let t = Instant::now();
        let content = tx::preview::build(&path).unwrap();
        let image = content.image.unwrap();
        println!(
            "{name}: decode for a {}x{} cell screen: {:?}, kept {}x{}",
            screen.0,
            screen.1,
            t.elapsed(),
            image.0.width(),
            image.0.height()
        );
        let area = ratatui::layout::Rect::new(0, 0, 110, 50);
        let mut buf = ratatui::buffer::Buffer::empty(area);
        let t = Instant::now();
        painter.draw(&path, &image, area, &mut buf);
        println!("{name}: first draw on the main thread: {:?}", t.elapsed());
        let t = Instant::now();
        painter.run_jobs_now();
        println!("{name}: encoding, now on its own thread: {:?}", t.elapsed());
        let t = Instant::now();
        painter.draw(&path, &image, area, &mut buf);
        println!("{name}: drawing it again from the cache: {:?}", t.elapsed());
        let bytes: usize = buf.content().iter().map(|c| c.symbol().len()).sum();
        println!("{name}: bytes sent to the terminal: {} KB", bytes / 1000);
    }
}
