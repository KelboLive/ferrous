//! The Ferrous icon: the window/taskbar icon and the small logo in the top bar.
//! Both are raw RGBA rendered from the same artwork as `windows/ferrous.ico`
//! (which Windows shows for the .exe itself), so nothing is decoded at start-up.

use eframe::egui;

const ICON_128: &[u8] = include_bytes!("../assets/icon-128.rgba");
const LOGO_48: &[u8] = include_bytes!("../assets/logo-48.rgba");

/// Window and taskbar icon.
pub fn window_icon() -> egui::IconData {
    egui::IconData { rgba: ICON_128.to_vec(), width: 128, height: 128 }
}

/// The logo as an egui texture, for the top bar.
pub fn logo_texture(ctx: &egui::Context) -> egui::TextureHandle {
    let image = egui::ColorImage::from_rgba_unmultiplied([48, 48], LOGO_48);
    ctx.load_texture("ferrous-logo", image, egui::TextureOptions::LINEAR)
}

#[cfg(test)]
mod tests {
    #[test]
    fn embedded_icons_have_the_right_size() {
        assert_eq!(super::ICON_128.len(), 128 * 128 * 4);
        assert_eq!(super::LOGO_48.len(), 48 * 48 * 4);
        // Rounded corners are transparent.
        assert_eq!(super::ICON_128[3], 0);
    }
}
