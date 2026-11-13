//! Writes a contact sheet of luma wipes at 40% progress:
//! `cargo run -p ferrous-render --example luma_demo -- out.png`
use ferrous_core::{scene::TransitionKind, VideoFrame};

fn main() {
    let out = std::env::args().nth(1).unwrap_or_else(|| "luma_demo.png".into());
    let (w, h) = (192u32, 108u32);
    let mut a = vec![0u8; (w * h * 4) as usize];
    let mut b = a.clone();
    for y in 0..h {
        for x in 0..w {
            let i = ((y * w + x) * 4) as usize;
            a[i..i + 4].copy_from_slice(&[90, 40, 30, 255]);
            let band = (x * 7 / w) as u8;
            b[i..i + 4].copy_from_slice(&[40 + band * 30, 120 + band * 15, 255 - band * 30, 255]);
        }
    }
    let (a, b) = (VideoFrame::new(w, h, a), VideoFrame::new(w, h, b));
    let wipes = ferrous_core::import_obs::LUMA_WIPES;
    let cols = 6u32;
    let rows = (wipes.len() as u32).div_ceil(cols);
    let mut sheet = image::RgbImage::new(cols * (w + 6), rows * (h + 6));
    for (n, wipe) in wipes.iter().enumerate() {
        let k = TransitionKind::Luma { wipe: wipe.to_string(), image: None, softness: 0.1, invert: false };
        let f = ferrous_render::transitions::apply(&k, &a, &b, 0.4);
        let (ox, oy) = ((n as u32 % cols) * (w + 6), (n as u32 / cols) * (h + 6));
        for y in 0..h {
            for x in 0..w {
                let i = ((y * w + x) * 4) as usize;
                sheet.put_pixel(ox + x, oy + y, image::Rgb([f.data[i + 2], f.data[i + 1], f.data[i]]));
            }
        }
    }
    sheet.save(&out).unwrap();
    println!("wrote {out}");
}
