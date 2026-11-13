use eframe::egui::{self, Color32, CornerRadius, Vec2};

use crate::theme;

pub fn fmt_duration(d: std::time::Duration) -> String {
    let s = d.as_secs();
    format!("{:02}:{:02}:{:02}", s / 3600, (s / 60) % 60, s % 60)
}

pub fn pill(ui: &mut egui::Ui, text: &str, color: Color32) {
    egui::Frame::new()
        .fill(color.linear_multiply(0.18))
        .stroke(egui::Stroke::new(1.0, color))
        .corner_radius(CornerRadius::same(10))
        .inner_margin(egui::Margin::symmetric(8, 2))
        .show(ui, |ui| {
            ui.label(egui::RichText::new(text).color(color).strong().small());
        });
}

/// A card's title; omitted inside a dock, whose tab already names it.
pub fn dock_title(ui: &mut egui::Ui, text: &str) {
    if !crate::theme::is_in_dock() {
        section_title(ui, text);
    }
}

pub fn section_title(ui: &mut egui::Ui, text: &str) {
    ui.label(egui::RichText::new(text.to_uppercase()).small().strong().color(theme::muted()));
}

/// Big primary action button (Start Streaming / Start Recording).
pub fn action_button(ui: &mut egui::Ui, text: &str, active: bool) -> egui::Response {
    let size = Vec2::new(ui.available_width(), 38.0);
    let (rect, resp) = ui.allocate_exact_size(size, egui::Sense::click());
    let base = if active { theme::live() } else { theme::surface() };
    let fill = if resp.is_pointer_button_down_on() {
        base.gamma_multiply(0.8)
    } else if resp.hovered() {
        base.lerp_to_gamma(egui::Color32::WHITE, 0.12)
    } else {
        base
    };
    let p = ui.painter();
    // Glass themes: the idle button is an accent gradient, the live one red.
    let gradient = !active && theme::glassy();
    if gradient {
        let (a, b) = (theme::accent(), theme::accent2());
        let lift = |c: egui::Color32| if resp.is_pointer_button_down_on() { c.gamma_multiply(0.8) } else if resp.hovered() { c.lerp_to_gamma(egui::Color32::WHITE, 0.14) } else { c };
        theme::gradient_rect(p, rect.expand(1.0), lift(a).gamma_multiply(0.35), lift(b).gamma_multiply(0.35), 9.0);
        theme::gradient_rect(p, rect, lift(a), lift(b), 8.0);
    } else {
        p.rect_filled(rect, 6.0, fill);
    }
    if !active && !gradient {
        p.rect_stroke(rect, 6.0, egui::Stroke::new(1.0, theme::border()), egui::StrokeKind::Inside);
    }
    // Centred in both directions, and shrunk to fit if the button is narrow.
    let colour = if active {
        egui::Color32::WHITE
    } else if gradient {
        theme::on(theme::accent().lerp_to_gamma(theme::accent2(), 0.5))
    } else {
        theme::text()
    };
    let mut font = 15.0f32;
    let mut galley = p.layout_no_wrap(text.to_string(), egui::FontId::proportional(font), colour);
    let room = rect.width() - 16.0;
    if galley.size().x > room && room > 0.0 {
        font = (font * room / galley.size().x).max(9.0);
        galley = p.layout_no_wrap(text.to_string(), egui::FontId::proportional(font), colour);
    }
    p.galley(rect.center() - galley.size() / 2.0, galley, colour);
    if resp.hovered() {
        ui.ctx().set_cursor_icon(egui::CursorIcon::PointingHand);
    }
    tip(resp, text)
}


/// What a setting or button does, shown when hovering its label. Keyed by
/// the label text, so the same wording gets the same explanation everywhere.
pub fn hint(label: &str) -> Option<&'static str> {
    Some(match label.trim_end_matches([':', '…']).trim() {
        "Service" => "The streaming service. Picking one fills in its ingest server for you.",
        "Server" => "Where the stream is sent: an rtmp://, rtmps://, srt:// or WHIP (https://) address. Leave the main server empty to stream only to the additional destinations.",
        "Stream key" => "The secret key from your streaming platform. Anyone with it can stream to your channel, so don't share it.",
        "Audio bitrate" => "Audio quality in kilobits per second. 128 to 160 is typical; higher uses more upload bandwidth.",
        "Audio track" => "Which mixer track (1 to 6) this output carries. Choose each source's tracks in its audio settings in the mixer.",
        "Audio tracks" => "The mixer tracks saved in the recording, one audio stream each. Several tracks let you edit game, voice and music separately later.",
        "Audio" => "Audio settings for this output.",
        "B-frames" => "Frames predicted from both neighbours. They improve quality for the same bitrate and add a little delay. Use 0 for the lowest latency or the most compatibility.",
        "Base (canvas) resolution" => "The size of the canvas you build scenes on. Sources are placed in this space.",
        "Output (scaled) resolution" => "The size that is encoded and sent. A smaller size than the canvas reduces encoder load and the bitrate you need.",
        "Bitrate" => "Video data per second. Higher is sharper but needs more upload speed or disk space. Keep streams under about 80% of your upload speed.",
        "Codec" => "The video format. H.264 works everywhere; HEVC and AV1 are more efficient but not accepted by every platform or player.",
        "Compositor" => "Where scenes are drawn. The GPU is faster; the CPU works on any machine. Auto uses the GPU when a real one is available.",
        "Custom FFmpeg options" => "Extra FFmpeg arguments added to the encoder. For advanced use; wrong options can stop the output from starting.",
        "Desktop audio" => "The playback device whose sound is captured as desktop audio.",
        "Microphone" => "The input device captured as your microphone.",
        "Monitoring device" => "Where monitored audio plays (usually your headphones), so you can hear a source without sending it to the stream.",
        "Encoder" => "What compresses the video. Hardware encoders (NVENC, QSV, AMF) use your graphics card and barely touch the CPU. x264 runs on the CPU and gets more quality per bit.",
        "Encoder input" => "Where video is converted to the encoder's colour format. Ferrous does it on all CPU cores, or the GPU when available; FFmpeg does it on one core. Auto picks the best for your setup.",
        "FFmpeg path" => "The ffmpeg.exe used for encoding. The bundled copy is used when this is empty.",
        "File name" => "Naming pattern for recordings. Date and time codes such as %CCYY-%MM-%DD are filled in.",
        "Folder" => "Where recordings and replay clips are saved.",
        "Format" => "The container for recordings. MKV survives a crash; MP4 is the most compatible but can be lost if Ferrous closes unexpectedly (fragmented MP4 is safer).",
        "Frame rate" => "Frames per second for the canvas and everything encoded. 60 looks smoother in games but costs more of everything.",
        "GPU priority" => "Asks Windows to schedule Ferrous's GPU work ahead of other programs, which helps when a game uses the whole GPU.",
        "GPU readback" => "Reads each GPU frame back one frame later so the program never waits for the GPU. Smoother, with one frame more delay.",
        "Keep the last" => "How many seconds the replay buffer holds. Save Replay writes this much of the past to a file.",
        "Keyframe interval" => "Seconds between full keyframes. Most platforms want 2. Shorter means viewers join faster and costs some bitrate.",
        "Preset" => "Speed against quality for the encoder. Slower presets compress better and use more of the encoder.",
        "Rate control" => "How bitrate is managed. CBR holds it steady (best for streaming); VBR, CQP and CRF aim for a steady quality instead (good for recording).",
        "Quality" => "Picture quality. For quality-based modes a lower number is better and makes bigger files; for Ferrous Link JPEG a higher number is better and uses more bandwidth.",
        "Vertical canvas" => "A second 9:16 canvas with its own layout, for TikTok, Shorts and Reels, which can be streamed or recorded alongside the main one.",
        "Output" => "The size sent to this destination. 0 means the canvas size.",
        "Send" => "What this Ferrous Link output shares with the other PC: the program output, one scene or one source.",
        "Size / FPS" => "Resolution and frame rate sent over Ferrous Link. 0 uses the source's own size and the canvas frame rate.",
        "Pairing PIN" => "Optional code receivers must enter. With a PIN the link is encrypted; without one anyone on your network can watch.",
        "Port" => "The network port this service listens on. Allow it through the Windows firewall for other devices to connect.",
        "Receivers use" => "The addresses to type into Ferrous Link sources on the other PC (they are found automatically on most networks).",
        "Password" => "Required by remote control clients before they can connect.",
        "Key color" => "The colour to make transparent. Pick the colour of your green or blue screen.",
        "Opacity" => "How see-through the source is. 100% is solid.",
        "Outline" => "Draws an outline around the text so it stays readable on any background.",
        "Background" => "A solid colour behind the text.",
        "Font" => "The typeface and size of the text.",
        "Size" => "How large the source is drawn.",
        "Color" => "The colour used for this source.",
        // Buttons and checkboxes
        "Start Streaming" | "Stop Streaming" => "Goes live to the main server and every enabled destination, or ends the stream.",
        "Start Recording" | "Stop Recording" => "Saves the program to a file in the recordings folder.",
        "Start Replay Buffer" | "Stop Replay Buffer" => "Keeps the last few seconds in memory so you can save a replay of something that just happened.",
        "💾 Save Replay" => "Writes the replay buffer to a file now.",
        "Studio Mode" | "Exit Studio Mode" => "Edit the next scene in a separate preview, then send it live with Transition.",
        "Transition ▶" => "Switches the live program to the scene being edited, using the selected transition.",
        "Enable preview" => "Draws the program in the preview. Turning it off saves CPU and GPU; streaming and recording are unaffected.",
        "Enable remote control server" => "Lets phones and other apps control Ferrous over your network. Set a password first.",
        "Current scene only" => "Shows only this scene's sources in the Performance monitor instead of every source.",
        "🔒 Lock layout" => "Stops docks from being dragged, resized or closed by accident.",
        "Adapt to network" => "Lowers quality automatically when the link can't keep up and raises it again when it recovers.",
        "Send audio from" => "Sends a mixer track's audio to the receiving PC along with the video.",
        "Read from file" => "Takes the text from a file and updates it whenever the file changes.",
        "Show" => "Reveals the hidden value.",
        "Duplicate" => "Makes a copy with the same settings.",
        "Rename" => "Changes the name shown in the lists.",
        "Fit to screen" => "Scales the source to fit inside the canvas, keeping its shape.",
        "Center on screen" => "Moves the source to the middle of the canvas without resizing it.",
        "Reset rotation" => "Turns the source back upright.",
        _ => return None,
    })
}

/// A label that explains itself when hovered.
pub fn row_label(ui: &mut egui::Ui, text: &str) -> egui::Response {
    tip(ui.label(text), text)
}

/// Adds the standard hover text for `label` to a widget's response.
pub fn tip(r: egui::Response, label: &str) -> egui::Response {
    match hint(label) {
        Some(t) => r.on_hover_text(t),
        None => r,
    }
}
