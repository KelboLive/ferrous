//! Custom browser docks: each one is a WebView2 child window laid exactly
//! over its dock's rectangle (Windows only).
//!
//! A native child window always draws above egui, so the page is hidden
//! whenever something egui draws should be on top of it: a menu, combo box,
//! dialog, floating dock window, or the drop guides while a dock is dragged.
//! The dock underneath shows a placeholder at those moments.

use eframe::egui;

/// Where a custom dock was drawn this frame.
#[cfg_attr(not(windows), allow(dead_code))]
pub struct WebFrame {
    pub id: u32,
    pub url: String,
    pub rect: egui::Rect,
    pub layer: egui::LayerId,
}

/// True when another egui layer is drawn on top of part of this dock.
#[cfg_attr(not(windows), allow(dead_code))]
pub fn covered(ctx: &egui::Context, f: &WebFrame) -> bool {
    ctx.memory(|m| {
        m.areas().visible_layer_ids().into_iter().any(|l| {
            if l == f.layer || l.order == egui::Order::Background {
                return false;
            }
            let Some(r) = m.area_rect(l.id) else { return false };
            let overlap = r.intersect(f.rect);
            if !overlap.is_positive() {
                return false;
            }
            match l.order.cmp(&f.layer.order) {
                std::cmp::Ordering::Greater => true,
                std::cmp::Ordering::Less => false,
                // Two windows: whichever is topmost where they overlap.
                std::cmp::Ordering::Equal => m.layer_id_at(overlap.center()) == Some(l),
            }
        })
    })
}

/// Opens a URL in the user's default browser.
pub fn open_external(url: &str) {
    #[cfg(windows)]
    {
        let _ = std::process::Command::new("explorer").arg(url).spawn();
    }
    #[cfg(not(windows))]
    {
        let _ = std::process::Command::new("xdg-open").arg(url).spawn();
    }
}

/// Makes a typed address loadable: "twitch.tv/x" becomes "https://twitch.tv/x".
pub fn normalize_url(url: &str) -> String {
    let u = url.trim();
    if u.is_empty() || u.contains("://") || u.starts_with("about:") || u.starts_with("data:") {
        u.to_string()
    } else {
        format!("https://{u}")
    }
}

#[cfg(windows)]
pub use imp::WebDocks;

#[cfg(windows)]
mod imp {
    use std::collections::HashMap;

    use eframe::egui;
    use wry::dpi::{PhysicalPosition, PhysicalSize};
    use wry::{NewWindowResponse, Rect, WebContext, WebView, WebViewBuilder};

    use super::{covered, open_external, WebFrame};

    struct View {
        webview: WebView,
        url: String,
        visible: bool,
        bounds: [i32; 4],
    }

    /// The live web views, by custom dock id.
    #[derive(Default)]
    pub struct WebDocks {
        views: HashMap<u32, View>,
        errors: HashMap<u32, String>,
        context: Option<WebContext>,
    }

    impl WebDocks {
        /// Why a dock's page couldn't be created, if it couldn't.
        pub fn error(&self, id: u32) -> Option<&str> {
            self.errors.get(&id).map(String::as_str)
        }

        /// Places, shows or hides every dock's page to match this frame.
        /// `configured` are the ids that still exist; others are destroyed.
        pub fn sync(&mut self, ctx: &egui::Context, window: &eframe::Frame, frames: Vec<WebFrame>, configured: &[u32]) {
            self.views.retain(|id, _| configured.contains(id));
            self.errors.retain(|id, _| configured.contains(id));
            let ppp = ctx.pixels_per_point();
            let mut drawn = Vec::new();
            for f in frames {
                drawn.push(f.id);
                let r = f.rect;
                let bounds = [
                    (r.min.x * ppp).round() as i32,
                    (r.min.y * ppp).round() as i32,
                    (r.width() * ppp).round().max(1.0) as i32,
                    (r.height() * ppp).round().max(1.0) as i32,
                ];
                let rect = Rect {
                    position: PhysicalPosition::new(bounds[0], bounds[1]).into(),
                    size: PhysicalSize::new(bounds[2] as u32, bounds[3] as u32).into(),
                };
                let show = r.width() > 8.0 && r.height() > 8.0 && !covered(ctx, &f);
                if !self.views.contains_key(&f.id) {
                    if self.errors.contains_key(&f.id) || f.url.is_empty() {
                        continue;
                    }
                    // One profile folder for all docks, beside the settings, so
                    // sign-ins (e.g. to chat) persist and nothing is written
                    // next to the exe (which may be read-only).
                    let context = self
                        .context
                        .get_or_insert_with(|| WebContext::new(Some(ferrous_core::paths::config_dir().join("webview"))));
                    let built = WebViewBuilder::new_with_web_context(context)
                        .with_url(&f.url)
                        .with_bounds(rect)
                        .with_visible(show)
                        .with_background_color((0x14, 0x15, 0x1a, 255))
                        // Pop-ups (sign-in, links) go to the real browser.
                        .with_new_window_req_handler(|url, _| {
                            open_external(&url);
                            NewWindowResponse::Deny
                        })
                        .build_as_child(window);
                    match built {
                        Ok(webview) => {
                            self.views.insert(f.id, View { webview, url: f.url.clone(), visible: show, bounds });
                        }
                        Err(e) => {
                            log::warn!("custom dock {}: {e}", f.id);
                            self.errors.insert(
                                f.id,
                                format!("Couldn't create the page: {e}. Custom docks need the Microsoft Edge WebView2 Runtime."),
                            );
                        }
                    }
                    continue;
                }
                let v = self.views.get_mut(&f.id).unwrap();
                if v.url != f.url {
                    v.url = f.url.clone();
                    let _ = v.webview.load_url(&f.url);
                }
                if v.bounds != bounds {
                    v.bounds = bounds;
                    let _ = v.webview.set_bounds(rect);
                }
                if v.visible != show {
                    v.visible = show;
                    let _ = v.webview.set_visible(show);
                }
            }
            // Docks not drawn this frame (another tab is in front, or closed)
            // keep their page, hidden, so chat history and sign-ins survive.
            for (id, v) in self.views.iter_mut() {
                if !drawn.contains(id) && v.visible {
                    v.visible = false;
                    let _ = v.webview.set_visible(false);
                }
            }
        }
    }
}

/// Elsewhere there is no embedded browser; docks offer to open the page.
#[cfg(not(windows))]
#[derive(Default)]
pub struct WebDocks;

#[cfg(not(windows))]
impl WebDocks {
    pub fn error(&self, _id: u32) -> Option<&str> {
        None
    }

    pub fn sync(&mut self, _ctx: &egui::Context, _window: &eframe::Frame, _frames: Vec<WebFrame>, _configured: &[u32]) {}
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn urls_get_a_scheme() {
        assert_eq!(normalize_url("twitch.tv/popout/me/chat"), "https://twitch.tv/popout/me/chat");
        assert_eq!(normalize_url(" http://localhost:8080/x "), "http://localhost:8080/x");
        assert_eq!(normalize_url(""), "");
    }

    /// Runs two frames (layer visibility is tracked across frames) with an
    /// optional popup-like area at `popup`, and reports whether the dock is covered.
    fn covered_with(popup: Option<egui::Rect>) -> bool {
        let ctx = egui::Context::default();
        let dock = egui::Rect::from_min_size(egui::pos2(100.0, 100.0), egui::vec2(300.0, 200.0));
        let mut result = false;
        for _ in 0..2 {
            let input = egui::RawInput {
                screen_rect: Some(egui::Rect::from_min_size(egui::Pos2::ZERO, egui::vec2(800.0, 600.0))),
                ..Default::default()
            };
            let mut out = ctx.run_ui(input, |ui| {
                if let Some(r) = popup {
                    egui::Area::new(egui::Id::new("popup")).order(egui::Order::Foreground).fixed_pos(r.min).show(ui.ctx(), |ui| {
                        ui.allocate_space(r.size());
                    });
                }
                let f = WebFrame { id: 1, url: String::new(), rect: dock, layer: egui::LayerId::background() };
                result = covered(ui.ctx(), &f);
            });
            out.textures_delta.clear(); // no renderer in tests
        }
        result
    }

    #[test]
    fn pages_hide_only_under_overlapping_layers() {
        assert!(!covered_with(None), "nothing on top");
        let over = egui::Rect::from_min_size(egui::pos2(150.0, 150.0), egui::vec2(80.0, 60.0));
        assert!(covered_with(Some(over)), "a menu over the dock hides the page");
        let beside = egui::Rect::from_min_size(egui::pos2(500.0, 100.0), egui::vec2(80.0, 60.0));
        assert!(!covered_with(Some(beside)), "a menu elsewhere doesn't");
    }
}
