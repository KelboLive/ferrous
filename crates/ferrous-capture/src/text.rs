//! Text source, rasterised with fontdue.

use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use ferrous_core::scene::TextAlign;
use ferrous_core::VideoFrame;
use fontdue::layout::{CoordinateSystem, HorizontalAlign, Layout, LayoutSettings, TextStyle};
use fontdue::{Font, FontSettings};

use crate::Source;

/// Candidate default fonts, Windows first.
const DEFAULT_FONTS: &[&str] = &[
    r"C:\Windows\Fonts\segoeui.ttf",
    r"C:\Windows\Fonts\arial.ttf",
    "/usr/share/fonts/truetype/dejavu/DejaVuSans.ttf",
    "/usr/share/fonts/truetype/liberation/LiberationSans-Regular.ttf",
];

pub fn resolve_font(font: &str) -> Option<PathBuf> {
    let f = font.trim();
    if !f.is_empty() {
        let p = PathBuf::from(f);
        if p.is_file() {
            return Some(p);
        }
        // A bare file name: look in the system fonts folder.
        for dir in [r"C:\Windows\Fonts", "/usr/share/fonts/truetype"] {
            let c = Path::new(dir).join(f);
            if c.is_file() {
                return Some(c);
            }
        }
    }
    DEFAULT_FONTS.iter().map(PathBuf::from).find(|p| p.is_file())
}

pub struct TextStyleConfig {
    pub size: f32,
    pub bgra: [u8; 4],
    pub outline: ([u8; 4], u32),
    pub background: [u8; 4],
    pub align: TextAlign,
    pub wrap_width: u32,
}

/// Renders text to a tightly sized BGRA frame (straight alpha).
pub fn render_text(font: &Font, text: &str, st: &TextStyleConfig) -> VideoFrame {
    let size = st.size.clamp(4.0, 512.0);
    let pad = st.outline.1.min(32) as i32 + 2;
    let mut layout = Layout::new(CoordinateSystem::PositiveYDown);
    let max_width = (st.wrap_width > 0).then_some(st.wrap_width as f32);
    layout.reset(&LayoutSettings {
        max_width,
        horizontal_align: match st.align {
            TextAlign::Left => HorizontalAlign::Left,
            TextAlign::Center => HorizontalAlign::Center,
            TextAlign::Right => HorizontalAlign::Right,
        },
        ..LayoutSettings::default()
    });
    let text = if text.is_empty() { " " } else { text };
    layout.append(&[font], &TextStyle::new(text, size, 0));
    let glyphs = layout.glyphs();
    let content_w = max_width.unwrap_or_else(|| glyphs.iter().map(|g| g.x + g.width as f32).fold(0.0, f32::max));
    let w = (content_w.ceil() as i32 + pad * 2).max(1) as u32;
    let h = (layout.height().ceil() as i32 + pad * 2).max(1) as u32;

    // Coverage mask first, then colourise (outline under fill).
    let mut cov = vec![0u8; (w * h) as usize];
    for g in glyphs {
        if g.width == 0 || g.height == 0 {
            continue;
        }
        let (m, bitmap) = font.rasterize_config(g.key);
        let (ox, oy) = (g.x.round() as i32 + pad, g.y.round() as i32 + pad);
        for y in 0..m.height as i32 {
            for x in 0..m.width as i32 {
                let (cx, cy) = (ox + x, oy + y);
                if cx >= 0 && cy >= 0 && (cx as u32) < w && (cy as u32) < h {
                    let i = (cy as u32 * w + cx as u32) as usize;
                    cov[i] = cov[i].max(bitmap[(y as usize) * m.width + x as usize]);
                }
            }
        }
    }
    let outline_px = st.outline.1.min(32) as i32;
    let outline_cov: Vec<u8> = if outline_px > 0 {
        // Max-filter (dilate) the coverage by the outline width.
        let mut out = vec![0u8; cov.len()];
        let r2 = outline_px * outline_px;
        for y in 0..h as i32 {
            for x in 0..w as i32 {
                let mut m = 0u8;
                for dy in -outline_px..=outline_px {
                    for dx in -outline_px..=outline_px {
                        if dx * dx + dy * dy > r2 {
                            continue;
                        }
                        let (sx, sy) = (x + dx, y + dy);
                        if sx >= 0 && sy >= 0 && sx < w as i32 && sy < h as i32 {
                            m = m.max(cov[(sy as u32 * w + sx as u32) as usize]);
                        }
                    }
                }
                out[(y as u32 * w + x as u32) as usize] = m;
            }
        }
        out
    } else {
        Vec::new()
    };

    let over = |dst: [f32; 4], src: [u8; 4], a: f32| -> [f32; 4] {
        let sa = src[3] as f32 / 255.0 * a;
        let da = dst[3];
        let oa = sa + da * (1.0 - sa);
        if oa <= 0.0 {
            return [0.0; 4];
        }
        let mut o = [0.0; 4];
        for c in 0..3 {
            o[c] = (src[c] as f32 * sa + dst[c] * da * (1.0 - sa)) / oa;
        }
        o[3] = oa;
        o
    };
    let mut data = vec![0u8; (w * h * 4) as usize];
    for i in 0..(w * h) as usize {
        let mut px = [st.background[0] as f32, st.background[1] as f32, st.background[2] as f32, st.background[3] as f32 / 255.0];
        if outline_px > 0 && outline_cov[i] > 0 {
            px = over(px, st.outline.0, outline_cov[i] as f32 / 255.0);
        }
        if cov[i] > 0 {
            px = over(px, st.bgra, cov[i] as f32 / 255.0);
        }
        data[i * 4] = px[0] as u8;
        data[i * 4 + 1] = px[1] as u8;
        data[i * 4 + 2] = px[2] as u8;
        data[i * 4 + 3] = (px[3] * 255.0).round() as u8;
    }
    VideoFrame::new(w, h, data)
}

pub struct TextSource {
    font: Font,
    style: TextStyleConfig,
    text: String,
    from_file: Option<PathBuf>,
    last_check: Instant,
    frame: VideoFrame,
}

impl TextSource {
    pub fn new(text: &str, from_file: Option<PathBuf>, font: &str, style: TextStyleConfig) -> anyhow::Result<Self> {
        let path = resolve_font(font).ok_or_else(|| anyhow::anyhow!("font \"{font}\" not found"))?;
        let bytes = std::fs::read(&path)?;
        let font = Font::from_bytes(bytes, FontSettings::default()).map_err(|e| anyhow::anyhow!("bad font {}: {e}", path.display()))?;
        let text = match &from_file {
            Some(p) => std::fs::read_to_string(p).unwrap_or_default(),
            None => text.to_string(),
        };
        let frame = render_text(&font, &text, &style);
        Ok(Self { font, style, text, from_file, last_check: Instant::now(), frame })
    }
}

impl Source for TextSource {
    fn latest_frame(&mut self) -> Option<VideoFrame> {
        if let Some(p) = &self.from_file {
            if self.last_check.elapsed() >= Duration::from_secs(1) {
                self.last_check = Instant::now();
                if let Ok(t) = std::fs::read_to_string(p) {
                    if t != self.text {
                        self.text = t;
                        self.frame = render_text(&self.font, &self.text, &self.style);
                    }
                }
            }
        }
        Some(self.frame.clone())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn style() -> TextStyleConfig {
        TextStyleConfig { size: 48.0, bgra: [255, 255, 255, 255], outline: ([0, 0, 0, 255], 0), background: [0; 4], align: TextAlign::Left, wrap_width: 0 }
    }

    #[test]
    fn renders_text_with_ink_and_outline() {
        let Some(path) = resolve_font("") else {
            eprintln!("no system font; skipping");
            return;
        };
        let font = Font::from_bytes(std::fs::read(path).unwrap(), FontSettings::default()).unwrap();
        let f = render_text(&font, "Hello", &style());
        assert!(f.width > 80 && f.height > 30, "{f:?}");
        let ink = f.data.chunks_exact(4).filter(|p| p[3] > 200 && p[0] > 200).count();
        assert!(ink > 200, "only {ink} opaque white pixels");
        // Two lines are taller than one.
        let two = render_text(&font, "Hello\nWorld", &style());
        assert!(two.height > f.height + 30);
        // An outline adds black opaque pixels.
        let mut st = style();
        st.outline = ([0, 0, 0, 255], 3);
        let o = render_text(&font, "Hello", &st);
        let black = o.data.chunks_exact(4).filter(|p| p[3] > 200 && p[0] < 30).count();
        assert!(black > 100);
        // Wrapping narrows the frame.
        let mut st = style();
        st.wrap_width = 120;
        let wrapped = render_text(&font, "Hello there wide world", &st);
        assert!(wrapped.width <= 120 + 8 && wrapped.height > f.height);
    }
}
