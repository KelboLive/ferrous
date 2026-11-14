# Roadmap progress log

This file tracks work through the roadmap in `README.md`, in priority order. Each entry records
what was built, how it was verified, and anything deferred (with the reason).

Verification environment: Linux build host with a software Vulkan driver (lavapipe) and FFmpeg 6.1.
Windows-only code paths (Windows Graphics Capture, DirectShow devices, WASAPI) can't run here; they're
called out as **Windows-untested** and are covered by the GitHub Actions Windows build.

**Summary (end of this pass):** all 8 roadmap priorities, the README's to-do list and 6 requested
additions are done. There are 126 automated tests (unit plus end-to-end through real FFmpeg,
sockets, Vulkan, Chrome, a real VST3 plugin and the real libobs), all passing, and the source is
about 24,000 lines of Rust. The whole workspace also type-checks for Windows from this Linux host.
Items marked *deferred* or *Windows-untested* below say why.

| # | Priority | Status |
|---|---|---|
| 1 | GPU compositor | done (zero-copy encode still deferred; see the to-do section) |
| 2 | Webcam and media sources | done |
| 3 | Filters | done (VST3 hosting added) |
| 4 | Studio mode and transitions | done |
| 5 | Replay buffer, virtual camera, multitrack, app audio, monitoring | done (virtual camera added, Windows-untested) |
| 6 | Hotkeys, text, browser source, nested scenes | done |
| 7 | Remote API and plugins | done (OBS plugin bridge built for sources) |
| 8 | Auto-config, profiles, OBS import | done |
| + | Ferrous Link (obs-teleport successor) | done |
| + | TikTok Kit | done |
| + | Toggleable docks, Stats dock, Performance Monitor | done |
| + | First-run setup benchmark, ingest ranking, layout persistence | done |
| + | Modular docking with snap-in-place | done |
| + | README to-do list + animated mixer | done (see the last section) |

---

## 1. GPU compositor: done

**Built**
- `ferrous-render/src/gpu.rs`: a wgpu compositor (Direct3D 12 or Vulkan on Windows) with one WGSL
  pipeline. Each layer is a textured quad with crop via UV, bilinear sampling, and opacity. It uses
  straight-alpha "over" blending into an opaque BGRA canvas, then reads the canvas back for the encoders.
- Source textures are cached by frame identity. Static sources (images, colors) upload once instead
  of every frame, and textures are freed when a source leaves the screen.
- `Renderer` enum in `ferrous-render` picks GPU or CPU. The engine uses GPU by default and falls back
  to CPU with a notification if no adapter exists. Settings > Video > Compositor can force CPU, and
  the status bar shows which compositor and adapter are active.

**Verified**
- `gpu_matches_cpu`: GPU output matches the CPU reference compositor on a gradient that's scaled up
  3×, plus overlapping semi-transparent layers. Mean per-channel difference is under 0.5, max is 8 or
  less (bilinear rounding). This ran on a real Vulkan adapter (lavapipe).
- The test also checks that textures are reused between frames (3 layers give 3 cached textures).
- `records_with_gpu_compositor`: a full recording through the GPU path. An extracted frame is
  identical to the CPU-path recording.

**Deferred**
- *Zero-copy hardware encoding* (handing the D3D11/D3D12 canvas texture straight to NVENC/AMF/QSV
  without CPU readback). This needs FFmpeg's `d3d11va` hwframes or the vendor SDKs linked in-process,
  and a Windows GPU to validate. The CPU readback works at 1080p60. 4K60 is the case that needs zero-copy.
- *Capture to GPU without a CPU copy*. Windows Graphics Capture already produces a D3D11 texture, but
  sharing it with the wgpu device needs D3D11-D3D12 interop. This can't be tested on this host.

---

## 2. Webcam and media sources: done

**Built**
- `ferrous-capture/src/ffsource.rs`: sources decoded by FFmpeg (one FFmpeg process per source).
  Frames arrive as BGRA on stdout. The source's audio arrives as 48 kHz float over loopback TCP,
  with the frame size taken from FFmpeg's own startup output.
  - **Video Capture Device**: webcams and capture cards via DirectShow (V4L2 elsewhere). Optional
    resolution and FPS, plus an optional paired audio device for capture-card audio. Dead devices
    are reopened automatically.
  - **Media Source**: any file or network URL FFmpeg can read (MP4/MKV/MOV/WebM, audio-only files,
    HLS, RTMP, SRT). Plays in real time, with loop and hardware decoding (`-hwaccel auto`) options.
  - Device listing parses `ffmpeg -list_devices` output in both the old and new formats.
- **The mixer now has dynamic channels.** Sources with audio get their own mixer channel (volume,
  mute, meter), which disappears when the source is removed. This also laid the groundwork for
  priorities 3 and 5: per-channel filter chains, a 6-track bitmask, and a monitoring mode.
- GUI: "Video Capture Device" and "Media Source" in the add menu, a properties editor for each, and
  the mixer lists source channels.

**Verified**
- `media_source_video_and_audio_reach_recording`: generates an MP4 with a test pattern and a 440 Hz
  tone, adds it as a media source, records 3 s, then checks the recording. Audio is present (mean
  -24 dB, not silent) and an extracted frame shows the clip composited at the right spot.
- Unit tests cover DirectShow listing (both FFmpeg output formats) and rawvideo size parsing.

**Windows-untested:** opening real DirectShow devices. The argument format follows FFmpeg's documented
`video=<name>:audio=<name>` syntax.

**Deferred:** native Media Foundation capture (lower latency than going through FFmpeg, and exposes
camera controls such as focus and exposure). The FFmpeg route covers the same devices today.

---

## 3. Filters: done (VST hosting deferred)

**Built**
- **Video filters** (`ferrous-render/src/filters.rs`) run on a source's frame before compositing,
  per scene item, in any order, and each can be switched on or off:
  - **Chroma key**: same maths as OBS's shader (CbCr distance, similarity, smoothness, spill
    desaturation). The 1–1000 slider ranges match OBS, so values carry over.
  - **Color correction**: gamma, contrast, brightness, saturation, hue shift, opacity (OBS ranges).
  - **Scroll**: horizontal and vertical with wrap-around.
  - **Apply LUT**: loads `.cube` 3D LUTs with trilinear lookup and an amount slider. A bad file
    shows an error on the source instead of breaking the scene.
- **Audio filters** (`ferrous-audio/src/filters.rs`) run per mixer channel, before the fader:
  - **Noise suppression** (RNNoise, via the pure-Rust `nnnoiseless` port).
  - **Noise gate** (open/close thresholds, attack, hold, release).
  - **Compressor** (ratio, threshold, attack, release, makeup).
  - **Limiter**.
  - **Gain**.
  - Defaults match OBS's.
- Settings are saved with the scene item (source filters) or in settings (Desktop/Mic filters).
- GUI: a reusable filter-chain editor (add, enable, reorder, remove), shown under source Properties
  and in each mixer channel's ⚙ window.

**Verified**
- Video filter tests:
  - The chroma key removes a real green-screen colour while a skin tone keeps full alpha.
  - Colour correction leaves the image unchanged at default settings, and brightness and
    desaturation behave as expected.
  - Scroll wraps correctly.
  - An identity LUT is lossless, an inverting LUT inverts, and malformed LUTs are rejected.
- `chroma_key_filter_applies_in_engine`: a keyed green layer reveals the red layer under it in the
  live engine output.
- Audio filter tests:
  - The gate fully silences a -40 dB signal and reopens for loud input.
  - A 4:1 compressor at -20 dB brings a full-scale sine to about -15 dB.
  - The limiter never exceeds its ceiling.
  - RNNoise cuts fan-like noise by more than 10 dB.

**Deferred**
- *VST 2/3 plugin hosting.* VST3 needs the SDK bindings plus a native plugin editor window, and real
  plugins to test against, so it belongs with the Windows test pass. VST2 is discontinued by
  Steinberg. The filter chain has a clear slot for it.
- *GPU versions of the filters.* They run on the CPU (rayon-parallel) on the source frame. Moving them
  into the wgpu compositor as shaders comes after zero-copy capture.

---

## 4. Studio mode and transitions: done

**Built**
- **Transitions** run in the engine whenever the program scene changes, from any trigger (click,
  Transition button, and later hotkeys or the remote API).
  - Cut, Fade, Fade to Color, Swipe (incoming slides over), Slide (incoming pushes the outgoing out)
    in four directions, and Stinger.
  - Durations are set in milliseconds, with smoothstep easing.
  - Stinger plays any video FFmpeg can decode over the switch (alpha-capable formats such as WebM
    VP9 work) and cuts the scenes underneath at a set cut point.
  - The transition settings are saved with the scene collection.
- **Studio mode** shows an editable Preview on the left, live Program on the right, and transition
  controls in the middle (Transition button, type, duration, type-specific options). Clicking a scene
  in studio mode loads it into Preview, and the live scene is marked ●. The engine renders the
  preview scene separately at 30 fps, so editing never touches the live output.
- **Also fixed:** in Auto mode, a *software* GPU adapter (llvmpipe, Microsoft WARP) is now rejected
  in favour of the faster CPU compositor. Without this, PCs with no real GPU got a slower render path.

**Verified**
- Transition tests:
  - A fade is fully A at 0, fully B at 1, and 50% at the midpoint.
  - A left swipe has the incoming scene on the right edge halfway through.
  - A slide moves the outgoing scene's edge by half the width at 50%.
  - Fade to colour hits the colour at the midpoint.
- `fade_transition_and_studio_preview` (live engine):
  - Studio preview renders scene 2 while program shows scene 1.
  - Switching program produces an intermediate value mid-fade, then the target.
- `stinger_transition_overlays_video`: a red stinger clip appears over the program during the switch.
- GUI screenshot shows Preview (empty "BRB") and Program (live scene) side by side with the controls.

**Deferred:** luma wipes (need a bundled wipe-image library), and per-scene transition overrides.
Both are small additions to `TransitionKind`.

---

## 5. Replay buffer, multitrack, application audio, monitoring: done (virtual camera deferred)

**Built**
- **Replay buffer**: a third output type that encodes into rolling 2-second MPEG-TS segments with a
  keyframe forced on every boundary. **Save Replay** joins the newest segments covering the
  configured length (5–600 s) into MKV/MP4/MOV without re-encoding, in the background, and saves it to
  the recordings folder. It has its own Start/Stop and Save buttons.
- **Multitrack audio**: the mixer produces 6 independent track mixes.
  - Every channel (Desktop, Mic, each source) has track checkboxes and is mixed only into the tracks
    it's assigned to.
  - Recordings and replays write any set of tracks as separate, titled audio streams ("Track 1"…).
  - Streaming picks one track, e.g. stream without music on Track 1 while recording music on Track 2.
- **Audio monitoring**: per channel, Off / Monitor only / Monitor and output, played through a
  selectable output device. Latency is capped at about 100 ms, and there's a warning when monitoring
  desktop audio would echo.
- **Application Audio Capture** (Windows-only): new source type that captures a single app's sound
  via WASAPI process loopback (Windows 10 2004+), the mechanism OBS's equivalent uses. Apps are listed
  by the process names of open windows.

**Bug found and fixed while testing:** with more than one audio track, FFmpeg deadlocked. It reads
data from input N before it starts listening on input N+1, but the writer only sent data once every
track had connected. Each track now has its own writer thread.

**Verified**
- `multitrack_recording_routes_sources`: a tone source assigned to Track 2 only, recording Tracks 1 and 2.
  Track 1 measures -91 dB (silent) and Track 2 -24 dB (the tone).
- `replay_buffer_saves_clip`: 9 s of buffering with a 4 s setting gives a valid 4.1 s file.
- All 9 end-to-end engine tests pass together.

**Windows-untested:** application audio capture and monitoring playback (this host has no audio devices).

**Deferred: virtual camera.** A Windows virtual camera needs a registered DirectShow filter DLL, or on
Windows 11 a Media Foundation virtual camera COM source. Both are separate signed binaries that need
an installer and real webcam-consuming apps to validate. The planned approach is a small C++/Rust
`mfcamera` DLL fed from shared memory, the same design as OBS's `obs-virtualcam-module`.

---

## 6. Hotkeys, text, browser source, nested scenes: done

**Built**
- **Global hotkeys** (`app/src/hotkeys.rs`) use the system-wide hotkey API, so they work while a game
  has focus.
  - Actions: start/stop recording, streaming and replay buffer; save replay; switch to a named scene;
    studio-mode transition; show/hide a named source; mute/unmute a mixer channel.
  - Keys use friendly names ("Ctrl+Shift+R", "F9", "Alt+1"), with live ✔/✖ validation in
    Settings > Hotkeys. Conflicts with other apps are reported.
  - The app's buttons and the hotkeys now go through one `perform(action)` entry point, which the
    remote API (priority 7) reuses.
- **Text source** (fontdue rasteriser): any TTF/OTF by path or by file name in the Windows Fonts
  folder (default Segoe UI). Size, colour, outline (true dilated outline, 0–32 px), background colour,
  left/center/right alignment, wrap width, and **read from file** (re-read every second, for
  now-playing text, counters and the like).
- **Browser source**: renders any URL (alert boxes, chat widgets, TikFinity/StreamElements overlays)
  in headless Microsoft Edge, which ships with every Windows 10/11 PC, or Chrome, driven over the
  Chrome DevTools Protocol. The page background is transparent, like OBS, with configurable size and
  FPS and custom CSS injection. Frames arrive only when the page repaints, so static overlays cost
  almost nothing.
- **Nested scenes**: a scene can be added as a source in another scene, then scaled, cropped and
  filtered like any source. Nested scenes are composited onto a *transparent* canvas, so lower layers
  show through. Nesting depth is capped at 4, which also makes self-referencing scenes harmless.

**Verified**
- `renders_page_with_transparent_background`: a real Chrome instance renders a page. The red box
  appears at the right spot, and the empty page area has alpha 0.
- `renders_text_with_ink_and_outline`:
  - Text produces opaque glyph pixels.
  - Multi-line text is taller.
  - An outline adds dark pixels.
  - The wrap width constrains the frame.
- `text_source_renders` and `nested_scene_source` (live engine): the nested scene shows up scaled into
  the bottom-right quarter with black elsewhere, and a deliberate cycle (A contains B contains A)
  renders without hanging.
- Hotkey parsing accepts friendly combos and rejects invalid ones.
- All 11 engine end-to-end tests pass.

**Notes and deferred items**
- Browser source *audio* isn't routed to the mixer yet (the browser runs muted). It could be added by
  pointing Application Audio Capture at the browser's process. *Interaction* (clicking into the page)
  isn't implemented.
- Groups (several sources moved as one) aren't separate from nested scenes yet. A nested scene covers
  most uses.
- Hotkey *registration* is Windows-untested (this host's test run has no desktop session). Parsing
  and dispatch are tested.

---

## 7. Remote API and plugins: done (OBS plugin bridge designed, not built)

**Built**
- **obs-websocket 5.x-compatible server** (`crates/ferrous-remote`). It implements the real
  protocol: Hello/Identify handshake, the `obswebsocket.json` subprotocol, SHA-256
  challenge/response authentication, requests, request batches (with `Sleep`), event subscriptions,
  and the standard status and close codes.
  - **36 request types**, including:
    - Scenes and studio mode: scene list, program/preview scene get and set, studio mode, transition.
    - Scene items: list, id lookup, enable/disable.
    - Inputs: list, settings (text, URL, file, which alert tools change), mute, volume.
    - Outputs: record, stream and replay buffer start/stop/toggle/status, save replay.
    - Stats and custom events.
  - **Events** for program-scene changes, record and stream state, and studio mode.
  - Settings > Advanced has enable, port (default 4455) and password, with a warning when no
    password is set.
- **Native plugin system**: a stable C ABI (`crates/ferrous-plugin-api`, C header in
  `docs/PLUGINS.md`) for video sources, video filters and audio filters.
  - The host (`crates/ferrous-plugins`) loads DLLs from the plugins folders, checks the ABI version,
    validates callbacks, and keeps libraries loaded while instances exist.
  - Plugins show up in the Add Source and Add Filter menus, with JSON settings.
  - A complete example plugin is in `plugins/example`.
- **Settings robustness fix:** settings files with missing fields (older versions, hand edits)
  now merge with defaults instead of being discarded entirely. This was found while testing the
  remote server: a partial file silently reset everything.
- **OBS plugin compatibility:** a written design for hosting the real libobs in a crash-isolated
  helper process (`docs/OBS_PLUGIN_BRIDGE.md`), covering what will and won't work and the risks.

**Verified**
- `ferrous-remote` tests:
  - The auth algorithm matches an independent computation.
  - A real WebSocket client completes the handshake with authentication.
  - Requests return correct data and status codes (100/204/300).
  - Batches run in order.
  - Events arrive.
  - A wrong password is rejected with close code 4009.
- **Live app test:** a Python `websocket-client` script, written the same way as third-party OBS
  tools, drove the running GUI:
  - Listed 3 scenes, switched program to "BRB" (and received `CurrentProgramSceneChanged`), and
    listed scene items with OBS input kinds.
  - Got a 600 error for a missing scene.
  - Started a recording (`RecordStateChanged` event with the file path), checked its status,
    stopped it (got the output path back), and toggled mic mute.
- `loads_and_runs_example_plugin`: builds the example DLL, loads it, then runs the gradient source,
  the invert filter (pixel values check out) and the gain filter (-6 dB = 0.501×). Kind mismatches
  and unknown ids are rejected.

**Not built:** the OBS plugin bridge itself. It needs an OBS install and libobs SDK headers on Windows.

---

## 8. Auto-config, profiles and scene collections, OBS import: done

**Built**
- **Profiles and scene collections** (`ferrous-core/src/library.rs`), as in OBS: any number of named
  settings profiles and scene collections, with New, Duplicate, Rename and Remove (the last one is
  kept) from the **Profile** and **Scene Collection** menus.
  - Switching saves the one you're leaving.
  - Names are made safe as Windows file names and kept unique.
  - Existing single-file installs migrate automatically to "Default". This was checked on the test
    profile: the scenes and the current scene survived the migration.
- **Import from OBS Studio** (Scene Collection → Import from OBS Studio…): the file dialog opens in
  OBS's own scenes folder.
  - **Scenes and layout:** scenes in OBS order, the current scene, nested scenes, and groups (as
    nested scenes). Position, scale, crop, visibility and locks are kept. OBS alignment and bounding
    boxes (all 7 bounds modes) are resolved exactly once each source's real size is known.
  - **Sources:** display capture, window capture, game capture (window mode), image, colour,
    media (local file or URL), video capture device (with custom resolution), text
    (GDI+/FreeType, with font, colour, opacity, outline, background, alignment, wrap and
    read-from-file), browser (URL, local file, size, FPS, CSS) and application audio.
  - **Filters:** chroma key, colour correction, scroll, LUT, noise suppression, noise gate,
    compressor, limiter and gain.
  - **Audio and transitions:** each source's audio track assignment and monitoring mode, plus the
    default transition and its duration.
  - **Report:** anything without an equivalent (rotation, third-party sources and filters, game
    capture in fullscreen mode) is listed in an "Imported from OBS" window, never silently dropped.
- **Auto-Configuration Wizard** (Tools menu). Choose streaming or recording, 30 or 60 fps, your
  upload speed, and a platform bitrate limit.
  - It picks an encoder from what actually passed the test on this PC: NVENC > AMF > QSV > x264.
    For streaming it uses H.264 (universal ingest support); for recording it prefers HEVC/AV1.
  - It then picks the highest output resolution and frame rate the bitrate budget supports (75% of
    upload, capped). Software encoders stay at or below 1080p for live use. It drops to 30 fps
    before going below 720p.
  - Recording uses native resolution with constant quality.
  - You review the result, with the reasoning shown, before applying.

**Verified**
- `imports_obs_collection`, with a realistic OBS JSON fixture of 2 scenes and 7 sources:
  - Scene order, current scene and transition carry over.
  - Display capture keeps its monitor index, no-cursor setting, crop, scale and lock.
  - The webcam keeps its name parsed from the device id, custom 1280×720, chroma key and RNNoise,
    and tracks 1+2 with monitoring.
  - A bottom-right-aligned 480×270 scale-inner bounding box resolves to exactly (1420, 790) at
    0.375×.
  - The GDI+ text keeps its yellow ABGR colour, Arial → arial.ttf, size 48 and 3 px outline.
  - The nested scene and colour source come through.
  - The unknown source, unknown filter and rotation are all reported.
- Alignment maths: top-left is unchanged, and a centred item is offset by half its size.
- `migrate_list_rename_remove`:
  - The old settings migrate to "Default" and keep their values.
  - Name collisions get " 2".
  - Unsafe characters are sanitised.
  - The last profile can't be removed.
  - The active selection persists.
- Auto-config tests:
  - Encoder choice (NVIDIA streaming → h264_nvenc, recording → hevc_nvenc; AMD recording → av1_amf;
    CPU-only recording stays on x264).
  - 1440p60 at an 8 Mbps cap → 1080p60.
  - 4 Mbps upload → 720p and at most 3 Mbps.
  - A 4K CPU-only PC stays at or below 1080p.
- GUI screenshots: the new Profile, Scene Collection and Tools menus, and the wizard showing a
  recommendation with its reasoning.

**Deferred:** an automatic upload-bandwidth test against the platform's ingest server. It needs
platform-specific test modes (e.g. Twitch's `?bandwidthtest=true`) and a real stream key, so the
wizard asks for a speed-test number instead.

---

## Added: Ferrous Link (obs-teleport successor): done

Sends video and audio between Ferrous instances on a LAN, for example from a gaming PC to a
streaming PC (`crates/ferrous-link`).

**Built**
- **Discovery:** senders send a UDP multicast beacon every second (plus a localhost copy, so two
  instances on one PC also find each other). Receivers list senders by name, with resolution, codec
  and a PIN indicator. Manual `host:port` works too.
- **Three codecs, chosen per link:**
  - **JPEG:** the frame is split into 8 strips encoded and decoded in parallel, for the lowest
    latency. With **adaptive quality** on, the sender lowers quality (down to 35) when a receiver
    falls behind and climbs back after 3 clean seconds.
  - **Lossless + alpha:** XOR against the previous frame, then LZ4, so static areas cost almost
    nothing. Pixels arrive exactly, transparency included, which makes it ideal for overlays.
  - **Hardware (lowest bandwidth):** any FFmpeg encoder (NVENC/AMF/QSV/x264), tuned for low
    latency (no B-frames, 1 s keyframes), sent as MPEG-TS and decoded on the receiver.
- **What to send:** program output, any scene, or any single source (unaffected by its layout).
- **Per-link settings:** resolution (scaled on the sender), frame rate cap, and which audio track
  goes along (16-bit PCM, 48 kHz).
- **Pairing PIN with encryption:** the PIN derives a ChaCha20-Poly1305 key (salted per session),
  and every message is authenticated. A wrong or missing PIN is refused with a clear message.
- **Robustness:**
  - Each receiver has its own bounded queue, so a slow receiver loses frames without stalling the
    others.
  - Hardware-codec receivers that fall behind are reconnected cleanly at the next keyframe.
  - Receivers reconnect automatically.
  - Nothing is encoded while nobody is watching.
- **Stats:**
  - Sender: receivers connected, frames sent, dropped and skipped, current JPEG quality, Mbps.
  - Receiver: connection, codec, fps, Mbps, latency (clock-offset corrected via ping/pong) and RTT,
    shown live in the source's properties.
- **UI:** a Settings → Ferrous Link tab (any number of outputs), and an "Ferrous Link" source with a
  picker of discovered senders. Link audio becomes a mixer channel.

**Verified**, all over real TCP sockets:
- **JPEG with PIN:** a frame arrives with correct colour and audio arrives intact (0.5 → 0.5). On
  loopback: latency 1 ms, RTT 1 ms, about 2.5 Mbps at 320×180.
- **Wrong and missing PIN:** both refused, with "needs a PIN" for the missing one.
- **Lossless:** the received frame is byte-identical, alpha included.
- **Hardware codec:** a libx264 stream decodes with colours within tolerance.
- **Discovery:** the sender is found by name and connected to by name.
- **Adaptive quality:** a receiver that stops reading causes 136 dropped frames for that receiver,
  and JPEG quality backs off from 95 to 47.
- **Two engines:** engine A sends its program (lossless, with PIN), and engine B shows it as a
  source.
- Unit tests: AEAD round trip and counter sync, JPEG error bounds and compression, lossless
  exactness and delta size, scaling.

**Not yet:** network auto-discovery across subnets/VLANs (multicast TTL is 1 by design; use an
address). A future option is SRT transport for links over the internet.

---

## Added: TikTok Kit: done

**Built**
- **Vertical canvas (9:16).** A second canvas (default 1080×1920; 720×1280 preset) renders next to
  the main one on every frame.
  - It follows the program scene, so switching scenes switches both, but each scene has its own
    vertical layout.
  - Until an item is customised, it shows the main composition scaled to the vertical width
    (letterboxed), so nothing looks broken. Dragging or resizing an item in the vertical pane gives
    it its own placement. A 📱 toggle per source shows or hides it on the vertical canvas only.
  - Scene transitions apply to both canvases (stinger only on the main one).
- **Multistreaming.** Any number of extra destinations, each with its own canvas (main or
  vertical), output resolution, encoder, bitrate and audio track.
  - Start Streaming starts all of them (the main server can be left empty to stream only to these).
  - Each reconnects independently (10 attempts), and per-destination status is exposed.
- **TikTok LIVE preset** (Settings → Stream → "＋ TikTok LIVE (vertical)"): vertical canvas (turned on
  if needed), 1080×1920, CBR H.264 at 6000 kbps with no B-frames and 2 s keyframes. It uses the best
  encoder that passed the test on this PC.
- **Per-session keys.** TikTok issues a new server URL and key for each LIVE. Destinations marked
  "ask each time" open a **Go live** prompt that explains where to find them. The values are used for
  that session only and never saved, and streaming refuses to start without them.
- **Vertical replay clips.** With the vertical canvas on, the replay buffer can also keep a 9:16
  buffer, so Save Replay writes a second "(vertical)" clip ready to post (MP4 is now the default
  replay format).
- **Alert tools** such as TikFinity connect through the obs-websocket-compatible server (priority 7)
  and show overlays through the browser source (priority 6). No unofficial TikTok connection is
  built into Ferrous.

**Verified**
- `vertical_destination_with_session_key`, with a local FFmpeg RTMP listener standing in for the
  platform:
  - The main server is empty and a TikTok destination is set to "ask each time" on a 360×640
    vertical canvas.
  - Starting without the session key is refused. Starting with the key goes live.
  - The vertical preview is portrait and letterboxed (black at the top).
  - The destination reports live, and the received stream is exactly 360×640.
- GUI screenshots: the vertical pane next to the main preview, and the Go live prompt.

---

## Added: toggleable docks, Stats dock, Performance Monitor: done

**Built**
- **Every dock is toggleable** from **View → Docks**: Scenes, Sources, Controls, Audio Mixer,
  Properties, Stats and Performance Monitor.
  - Each one can be shown, hidden, or **floated** as its own movable, resizable window.
  - Empty side panels collapse. Stats and Performance sit in a resizable bottom panel, side by side.
  - Visibility and float state are saved per profile, and **Reset dock layout** restores the
    defaults.
- **Stats dock**, expanded beyond OBS's. **⚙ Choose stats** has a checkbox for every stat, saved
  per profile:
  - **Rendering:** FPS against target, average frame time against the frame budget, worst frame per
    second, frames missed, which compositor, and GPU compositing time.
  - **System:** Ferrous CPU (of all cores and of one core), Ferrous RAM, system CPU, system RAM, GPU
    usage and VRAM.
  - **Storage:** free space on the recordings drive, and recording time left at the current bitrate.
  - **Outputs table:** status, time, bitrate, encoder-skipped frames (%), and encoder speed for the
    stream, every extra destination, recording and replay buffer.
  - **Audio:** buffer depth per mixer channel.
  - **Network:** Ferrous Link outputs (receivers, Mbps, JPEG quality, drops) and remote-API clients.
  - Values are colour-coded for warning and critical levels.
- **Performance Monitor dock**, per source, with smoothed readouts and 60-second graphs:
  - **Capture + filters** and **compositing** milliseconds per frame, measured directly in the
    render loop and smoothed.
  - The source's **helper process** (FFmpeg decoder, headless browser): CPU, GPU and RAM.
  - **GPU:** the compositing pass is timed on the GPU with timestamp queries and shared across
    sources by covered area (shown as "est."). On the CPU compositor this reads "CPU path".
  - Rows for every encoder process (stream, recording, replay, destinations), Ferrous's total CPU and
    the system GPU, each with graphs.
  - Sort by cost and a current-scene-only filter.
  - The dock states how GPU attribution works, and that Windows screen capture's own GPU work isn't
    split per source.
- **New `ferrous-sysmon` crate:** system, Ferrous and per-process CPU and RAM via sysinfo, sampled at
  1 Hz. On Windows, GPU utilisation and dedicated VRAM, system-wide and per process, come from the
  PDH "GPU Engine" and "GPU Process Memory" counters that Task Manager uses (busiest engine type,
  summed per process).

**Verified**
- `per_source_performance_is_measured` (live engine):
  - The test pattern source reports capture and compositing cost.
  - The media source reports its FFmpeg process, which is sampled (about 2% CPU, 58 MB).
- `samples_own_process`: Ferrous's own CPU (during a busy loop), memory and disk space are read.
- `gpu_instance_parsing_and_aggregation`: Task Manager-style maths (busiest engine; per-process
  maximum over engine types).
- GPU timestamp queries work on a real Vulkan adapter (0.53 ms pass measured).
- GUI screenshots: the Stats and Performance docks live (with graphs), and the View → Docks menu
  with float toggles.

**Windows-untested:** the PDH GPU counters. Linux has no equivalent here, so GPU shows "n/a".

---

## Added: first-run setup benchmark, ingest ranking, layout persistence: done

**Built**
- **Setup & Benchmark** (`crates/ferrous-bench`, `app/src/setup.rs`) opens automatically on first
  launch, and later from Tools → Setup & Benchmark.
  - **Encoders:** a real timed FFmpeg encode with the best hardware H.264, the best hardware
    HEVC/AV1, and x264, at each candidate (1440p60, 1080p60/30, 720p60/30, up to your display
    size). Each run reports the frame rate actually sustained, and a setting counts as real time
    only with 10% headroom.
  - **Compositor:** renders a busy 3-layer scene with changing content at each resolution, on the
    compositor you'll use.
  - **Upload:** an 8-second timed upload of incompressible data to Cloudflare's public speed-test
    endpoint.
  - **Ingest servers:**
    - **Twitch:** fetches Twitch's public ingest list and ranks every server by median TCP connect
      latency and jitter. With a stream key, it runs Twitch's bandwidth-test mode (`?bandwidthtest=true`,
      which doesn't go live) on the 3 closest and re-ranks by sustained throughput.
    - **YouTube:** tests primary and backup. The backup is never auto-selected, because it exists
      for redundancy.
    - **Custom:** ranks any list of RTMP/SRT URLs.
    - **TikTok:** issues one server per session, so the wizard explains there's nothing to rank.
  - **Recommendations:** separate **streaming** and **recording** recommendations. Each walks down
    from the best setting until the chosen encoder *and* the compositor (under 60% of the frame
    budget) keep up, and lists the measured reasons.
  - **Apply:** apply streaming, recording, or both; choose any ranked ingest or **Use the fastest**.
    Recording can now run at a different resolution from the stream (new `record.output_size`), so
    streaming at 1080p and recording at native resolution works.
- **Layout persistence:** window size and position, floating dock positions and sizes, and
  side and bottom panel sizes are saved by eframe's persistence (`app.ron` in the user data folder,
  auto-saved every 10 s and on exit). Dock visibility is saved in the profile.

**Verified**
- `bench_encoder` and `bench_compositor` on real hardware (x264 640×360: 275 fps; CPU compositor
  timed).
- `parse_ffmpeg_speed`.
- `upload_bench_against_local_server`: a chunked upload measured against a local HTTP sink.
- `parses_twitch_list` against the documented response format.
- `ranks_by_latency_and_never_picks_backup`: a live server, a closed port, and a backup. The live one
  wins, and the closed one reports an error.
- `stream_test_measures_throughput`: a 3 Mbps CBR push to a local RTMP listener measures 3000 kbps.
- `recommendation_respects_measured_capacity`:
  - NVENC H.264 fails 1440p60, so streaming is 1080p60.
  - Recording uses HEVC at 1440p60.
- `slow_compositor_lowers_resolution`: 14 ms at 1080p means 1080p30, not 60.
- **GUI run on a fresh profile:** setup opens automatically, and "Just recording" benchmarks 4
  configurations. The VM's slow compositor correctly brings the recommendation down to 720p30.
  `app.ron` contains the window geometry, floating areas and panel rectangles.

**Can't run here:** the real upload test and Twitch ingest fetch, because this host has no general
internet access. Both are covered by local-server tests of the same code paths.

## Added: modular docking with snap-in-place: done

Every card is now its own dock: **Preview**, **Vertical Canvas**, **Scenes**, **Sources**,
**Controls**, **Audio Mixer**, **Properties**, **Stats** and **Performance Monitor**. The fixed
left/right/bottom panels are gone; the main window is one dock area (`app/src/layout.rs`, built on
`egui_stack` 0.25, the maintained continuation of `egui_dock`).

- **Move:** drag any dock by its tab.
- **Snap:** while dragging, the region the dock will take lights up in the Ferrous accent colour:
  - near an edge of a panel (outer 30%) → the panel splits and the dock takes that half (left,
    right, above, below);
  - over the middle of a panel or its tab strip → it joins that panel as a tab, at the pointer's
    position in the strip.
  - **Snap guides** (View menu) switch between *Zones* (above) and *Compass*, a Visual Studio-style
    set of five drop buttons.
- **Resize:** drag the gaps between docks; they highlight on hover and while dragging.
- **Float:** drag a tab out, or use *float* in View → Docks. Floating docks are ordinary windows that
  can hold several tabs and can be dragged back in and snapped.
- **Close/reopen:** the × on a tab or the View → Docks checkbox. A reopened dock goes back into the
  panel it was closed from if that panel still exists; otherwise it opens as a new column (a new row
  for Stats/Performance; beside the preview for the vertical canvas). The preview can be moved but
  not closed.
- **Lock layout** (View menu): no dragging or closing.
- **Presets:** *Ferrous* (default), *Classic* (OBS-style row of docks below the preview), *Analytics*
  (Stats and Performance under the preview), *Vertical / TikTok* (9:16 canvas beside the preview).
- **Saved layouts:** save the current arrangement under a name, recall or delete it.
- **Persistence:** the whole tree (splits, sizes, tabs, floating windows) is saved in the profile
  (`ui.layout`, as RON because panels not yet laid out have infinite rects that JSON can't hold) and
  restored on start. Each profile has its own layout. Layouts from earlier versions are migrated:
  their show/float switches are applied to the default layout.
- Card titles inside docks are dropped (the tab names the dock); the Properties tab shows the
  source type and the Vertical tab the canvas size. The vertical dock offers a one-click
  *Turn on 1080×1920* when the vertical canvas is off.

**Verified**
- Unit tests (`layout::tests`, 7): presets contain every expected dock once; layout round-trips
  through settings including a floating window; close-and-reopen returns to the same panel; float
  and dock back; preview can't be closed and is restored if missing; legacy switches honoured;
  a corrupt saved layout falls back to the default.
- GUI under Xvfb, driven with real mouse drags (screenshots 12–19): edge snap with zone preview,
  drop result, tabbing two docks, floating Stats then snapping it back into the left column,
  Compass guides, View menu, Classic and Vertical presets, enabling the vertical canvas from its
  dock. The layout survived a restart unchanged.

## README to-do list, plus the animated mixer: done

Worked through the README's "What's next" list in order, plus the request for OBS-style animated
mixer sliders.

**New: Windows type-checking here.** The Rust standard library's source was fetched (sparse
checkout of `rust-lang/rust` at the toolchain's exact commit), so this Linux host can now run
`cargo check -Zbuild-std --target x86_64-pc-windows-gnu --workspace --all-targets`. Before, the
Windows-only code was only checked by CI. The whole workspace passes, and the camera DLL even
cross-links with MinGW, with its four COM exports in place. This caught three Windows-only bugs
that would otherwise have reached CI. "Windows-untested" below now means type-checked but not run.

### Audio mixer: OBS-style animated meters and faders (`app/src/mixer.rs`)
- **Meters:** stereo bars on a -60..0 dBFS scale with OBS's green/yellow/red zones (-20 and -9 dB),
  and a dB scale under them.
  - Peaks rise instantly and fall at OBS's "fast" rate, 20.53 dB/s.
  - The brighter inner section is RMS ("magnitude") with 300 ms integration.
  - A peak-hold line waits 1.5 s, then falls.
  - A clip light latches for 2 s and pulses.
  - Muting fades the meter to grey.
- **Levels:** the mixer thread accumulates per-side peak and sum of squares between UI frames
  (`LevelTap`), so a 60 Hz UI never misses a peak from the 100 Hz mixer.
- **Fader:** uses OBS's cubic curve, scaled to +6 dB, so unity sits about 79% along, with a 0 dB
  notch.
  - The handle glides (70 ms) to values set elsewhere (hotkeys, remote API, reset).
  - It grows under the pointer.
  - Drag, Ctrl-drag for fine control, scroll for 0.5 dB steps, double-click for 0 dB.
- **Layout:** right-click a strip for mute, reset, filters, or switching to OBS's **vertical
  layout** (strips side by side).
- **Persistence:** fader and mute now persist, per source in the scene collection and for
  desktop/mic in the profile, from any origin (mixer, hotkey or remote API). Earlier, source
  volumes reset on restart. OBS imports bring `volume` and `muted` along.

### Rotation in transforms
- `Transform.rotation`: degrees clockwise about the box's centre.
- **CPU compositor:** inverse-maps every pixel in the rotated bounds, samples bilinearly, and
  antialiases edges by pixel coverage.
- **GPU compositor:** rotates the quad in the vertex shader. Rotated quads grow by a pixel, and the
  fragment shader computes the same coverage. Unrotated output is unchanged, and still matches the
  CPU path exactly.
- **Canvas:** hit testing, outline and resize handle all follow the rotation. A **rotate knob**
  above the box rotates with 3° detents at right angles; Shift snaps to 15°.
- **Menus and import:**
  - The Properties panel has a rotation field and ±90° buttons.
  - Edit → Transform has rotate 90° CW/CCW, 180°, reset, fit and centre. Fit to screen fits the
    rotated bounds.
  - OBS import converts OBS's rotation, which pivots on the item's position, to centre rotation
    (no longer reported as unsupported).
- **Verified:** corner, bounds, contains and local round trips; OBS pivot conversion; a 90° bar;
  45° edges partly covered; GPU matches CPU inside the edges (≤ 8 levels); dragging the knob in the
  GUI (screenshot 22).

### Luma wipes
- New `Luma` transition (OBS's semantics: progress × (1 + softness), blend band = softness, invert).
- **27 built-in wipes generated procedurally:** OBS's names (linear, barn door, blinds, box,
  iris, square, circles, clock, fan, burst, spiral, checkerboard, zigzag, sinus9, cloud, plus
  dissolve). Each is generated at the canvas size and cached, so nothing has to ship as image
  files.
- **Custom wipe images** (PNG/JPG/BMP/WebP) are scaled to the canvas.
- **Picker:** a grid of thumbnails, plus softness and invert.
- **OBS import** maps `wipe_transition` (built-in names, absolute custom paths), along with swipe,
  slide, fade-to-colour and stinger, and their settings. Earlier, every OBS transition other than
  Cut became Fade.
- **Verified:** every wipe starts on A, ends on B and is 20–80% done at the midpoint; direction
  and invert; softness width; a wipe from an image file; the OBS import mapping. A contact sheet
  rendered with the real code is screenshot 24.

### GPU encoders and the encode path
- **New vendor-neutral GPU encoders:** `h264/hevc/av1_d3d12va` (DirectX 12 Video, FFmpeg 7.1–8.1)
  and `h264/hevc/av1_vulkan` (Vulkan Video).
  - FFmpeg gets a hardware device (`-init_hw_device`) and an upload step (`hwupload`), and the
    capability probe tests them the same way.
  - Auto-config ranks them after NVENC/AMF/QSV and before the CPU. They're the only hardware
    option on, for example, Snapdragon laptops.
- **Encoder-input conversion in Ferrous** (`ferrous-output/src/convert.rs`): the output thread scales
  the canvas and converts it to NV12 (hardware) or I420 (software).
  - Uses BT.709 limited range, a separable triangle filter widened for downscales, and all cores.
  - The pipe to FFmpeg then carries 1.5 bytes per pixel instead of 4. A 4K60 canvas drops from
    2.0 GB/s to 0.75 GB/s, which matters because a Windows pipe tops out around there.
  - **Auto** (default) uses it above 1080p60 or on 8+ core CPUs; Settings → Video can force either
    way.
- **Verified:**
  - Reference colours within 1 code value.
  - NV12 and I420 carry identical samples.
  - Downscaled checkerboards average to grey.
  - End to end through FFmpeg, an Ferrous-converted recording decodes to 34.7 dB PSNR against
    FFmpeg's Lanczos path.
  - Speed on this 2-core VM: 1080p 4.5 ms per frame, 4K 24 ms, 4K→1080p 38 ms. FFmpeg's
    single-threaded SIMD swscale takes about 2.8 ms, 12.8 ms and 17.8 ms of CPU for the same jobs.
    That's why Auto only switches on where the pipe, not the CPU, is the limit.
- **Deferred: true zero-copy** (the canvas texture handed to NVENC/AMF/QSV/D3D12 without CPU
  readback). FFmpeg runs as a separate process, so frames must cross in system memory. Zero-copy
  needs an in-process encoder (libavcodec linked in, or the vendor SDKs) sharing the wgpu device's
  D3D12 resource, and a Windows GPU to build it against.

### VST3 hosting (`crates/ferrous-vst3`)
- **Lifecycle:** loads `.vst3` bundles (or legacy single files) from the standard folders. It
  creates the component, processor and edit controller and connects them. It transfers state,
  negotiates stereo (falling back to mono), sets up 48 kHz 32-bit processing and activates.
- **Processing:** runs on the mixer's 480-frame blocks; longer buffers are split.
- **Host side:** application context, messages and attribute lists, memory streams, parameter
  change queues, and a component handler that receives edits from the plugin's own editor.
- **Parameters:** Ferrous shows a generic editor: sliders, toggles and stepped values, with the
  plugin's own value text and a search box for long lists.
- **State** (component and controller) is saved with the filter and restored on load. Pending
  parameter changes are flushed with a zero-sample block first, as the VST3 API provides.
- **The plugin's own editor** opens in a Win32 window on its own thread, with resize support
  (Windows-untested).
- **Scanning and the filter chain:**
  - Scanning runs each bundle in a helper process (`ferrous --scan-vst3`, 20 s timeout) and
    is cached, so a crashing plugin can't take Ferrous down.
  - A filter chain now keeps running instances when other filters change, or when only a VST3's
    saved state changes. Before, any edit restarted every filter.
- **Test fixture:** `plugins/vst3-test-gain`, the vst3-rs example plugin (MIT/Apache) extended with
  a second parameter and state.
- **Verified:**
  - Scan, then load, then process (unity, then -6 dB from a host change, phase invert, odd block
    sizes).
  - State into a fresh instance, for both processor and controller.
  - The chain reuses a live instance and restores from saved config.
  - In the GUI: added from the menu, gain moved, meter dropped (screenshot 25).

### Virtual camera (Windows 11)
- **`ferrous-vcam`:** a seqlock'd, triple-buffered shared-memory frame transport (1080p30 NV12), plus
  camera control (`MFCreateVirtualCamera` with a session lifetime, and regsvr32 install via UAC).
- **`ferrous-vcam-source`:** the camera itself, a Media Foundation source DLL that Windows 11's
  Frame Server loads.
  - Built on the windows-rs `implement` macro: an activator (`IMFActivate`), the media source
    (`IMFMediaSourceEx`, `IMFGetService`, `IKsControl`) and a video stream (`IMFMediaStream2`,
    `IKsControl`), following Microsoft's VCamSample.
  - The DLL creates the `Global\` shared region with an Everyone/low-integrity DACL, because only
    a service may create global objects. Ferrous opens it once an app starts the camera, and retries
    until then.
  - An idle card shows while Ferrous isn't sending.
- **Engine and UI:** the engine converts program frames to 1080p NV12 on a worker thread at 30 fps.
  The UI adds **Start/Stop Virtual Camera** in Controls (showing "in use" when an app has it open)
  and **Tools → Install Virtual Camera**. CI bundles `ferrous_vcam.dll`.
- **Verified:**
  - Transport: newest frame wins, and no torn frames under a concurrent writer.
  - End to end on Linux with a file-backed region standing in for the camera: it opens late, Ferrous
    connects, and red arrives as BT.709 Y 63 / U 102 / V 240.
  - The DLL cross-compiles for Windows with the right exports.
  - **Windows-untested:** the Frame Server loading the DLL and apps seeing the camera.

### OBS plugin bridge (`crates/ferrous-obs`, `tools/obs-host`)
- **Built as designed, for sources:** `ferrous-obs-host` loads the real libobs from the user's OBS
  install at run time.
  - Its structs are declared to match libobs 30–32, checked against the libobs 30 headers; other
    major versions are refused.
  - It starts libobs headless and loads the plugins, then runs one plugin source.
  - It talks JSON over stdin/stdout: list types, create, properties, update, button click.
  - It streams video (BGRA) and audio (48 kHz float) over a loopback socket.
- **Video with alpha:** OBS's canvas output is opaque, so the host instead renders the source
  alone onto a transparent texture in libobs's render callback (`gs_texrender`). It reads that
  back one frame behind through a staging surface, and un-premultiplies. Text and overlays keep
  their transparency.
- **UI-only plugins** (docks, menus) abort without OBS's main window. Plugins that reference
  `obs_frontend_get_main_window` are skipped. Every other plugin is first loaded, including
  post-load, in a throwaway child process, and the results are cached.
- **In Ferrous:**
  - **Add Source → OBS plugins** lists the installed types (deprecated versions hidden).
  - Properties are a native form built from the plugin's own `obs_properties`: numbers, sliders,
    text, paths, lists, colours, fonts, groups, buttons.
  - Edits reach the running plugin without restarting it, through a new `Source::update` hook.
  - Hosts start in the background, so the render loop never waits.
  - The remote API reports the real OBS kind. **OBS scene imports now bring third-party plugin
    sources across as OBS plugin sources** with their settings, instead of skipping them.
- **Verified against real libobs 30.0.2 on this host** (Ubuntu's OBS, OpenGL on llvmpipe):
  - All 15 source types and 29 filter types list.
  - The 4 UI plugins are skipped.
  - OBS's Color Source shows red in Ferrous's preview, then blue after a live settings change with
    the same host process (`obs_plugin_e2e`).
  - In the GUI, OBS's FreeType text source renders with a transparent background over Ferrous's
    scene (screenshots 26 and 27).
- **Windows-untested:** the same host on `obs.dll` with D3D11.
- **Not built yet: OBS plugin filters** through the bridge. They need a frame-injector source type
  registered in the host. Ferrous's own filters, VST3 and native plugins cover audio and video
  filtering meanwhile.

## Optimization and dead-code pass: done

Measured on the 2-core Linux VM: dev profile (optimized), app under Xvfb with llvmpipe (software
GL), so the UI's GPU cost is exaggerated compared with a real GPU. CPU is the share of one core.

| Scenario | Before | After |
|---|---|---|
| Idle app, nothing visible on canvas: whole process | ~173% | ~46% |
| Idle: render thread | ~44% | ~0.2% |
| Static scene (colour source): render thread | ~44% | ~0.2% |
| Live 1080p test pattern: render thread | ~94% | ~32% |
| UI repaints, static scene | 60 fps (fixed timer) | ~9 fps (mixer's idle meter rate) |
| UI repaints, live scene | 21 fps (starved) | 29 fps (full preview rate) |
| CPU compositor, 1 opaque full-canvas layer | 3.67 ms | 0.53 ms |
| CPU compositor, full layer + picture-in-picture | 4.22 ms | 1.46 ms |
| CPU compositor, 640x360 scaled to 1080p | ~9.0 ms | ~8.2 ms |

Compositor output is byte-identical before and after (checksum over all four cases). The
CPU-vs-GPU parity tests still pass.

### Runtime waste removed

- **Composition memo** (`Renderer`). When every layer has the same frame buffer and the same
  transform as a recent composition, its output is returned as-is. Static scenes then cost
  nothing per tick: no redraw, and no blocking GPU readback on the GPU path. A live scene
  replaces its own memo entry rather than piling up stale frames, so memory stays bounded.
  Nested scenes are memoised too.
- **Unchanged buffers skip downstream work.** The UI previews (program, vertical, studio), the
  virtual camera's NV12 conversion, and each output's BGRA→YUV conversion all compare the
  frame's buffer pointer with the last one, and reuse their result when it hasn't changed.
- **Filter-chain memo.** A chain with no time-dependent filters (no scroll, no plugin filters)
  over an unchanged input returns its last result instead of re-filtering every pixel.
- **CPU compositor fast paths.**
  - An opaque, unscaled, full-canvas bottom layer becomes the canvas directly, passed through
    when it's the only layer, copied otherwise. This replaces fill-then-overdraw.
  - Unscaled opaque rows are copied rather than blended.
  - Bilinear sampling does one bounds check per pixel instead of one per channel.
  - Transparent canvases skip the redundant background fill.
- **GPU readback** appends rows into a fresh buffer instead of zero-filling 8 MB and then
  overwriting it.
- **Scenes are shared by `Arc`.** Previously the render loop deep-cloned the whole scene
  collection every frame, and the program scene a second time. It now copies a pointer, and the
  engine swaps in a new `Arc` only when the UI's scenes actually change. Sources are diffed
  against the scenes only on change, plus a 250 ms pass for dead or failed sources. That also
  stops every source's audio-filter lock being taken 60 times a second.
- **UI repaint on demand.**
  - The engine wakes the UI when it publishes a new preview.
  - Global hotkeys wake it through their own channel.
  - The fixed 60 fps timer is now a 250 ms tick for clocks and status text.
  - The mixer's animation is capped at 60 fps; it was unthrottled.
  - Previews are opaque, so they're uploaded with the premultiplied constructor, skipping a
    per-pixel alpha conversion.
- **Audio mixer** only mixes and clamps the output tracks that someone subscribes to (track 1
  always, for the master meters), not all six, every 10 ms.
- **VST3.**
  - One parameter-changes COM object is reused and cleared each block, instead of a new one
    allocated on the audio thread every block.
  - Bus channel counts are queried once, instead of a `getBusArrangement` COM call per bus per
    block.

### Dead code and hygiene

- **Removed:**
  - Never-called functions: `Transform::fit_width`, `ferrous_render::layer_rect`,
    `layout::surface_of`, `AudioEngine::subscribe`/`set_monitor_sink`, and the output stats'
    `recent_log`/`convert_ms`/`record_convert`. That also drops their per-frame timing and the
    log ring buffer, now a single last line.
  - The unread `AudioEngine::monitor` field and the unused `State::Failed` payload.
  - Six unused dependencies (cargo-machete, then checked by hand): `log` and `serde_json` from
    ferrous-vst3, `log` from ferrous-vcam and ferrous-bench, `log` and `serde` from ferrous-encode.
- **Clippy:** clean on Linux (`--all-targets`) and on the Windows target. The one exception is a
  `thread_local` lint that is a false positive, because the initialiser is already `const`.
- **`ferrous-vcam-source`'s library is now `ferrous_vcam_source`.** It previously shared the
  `ferrous_vcam` output name with the ferrous-vcam crate. The CI renames the DLL to `ferrous_vcam.dll`
  when packaging.
- **New: Tools → Uninstall Virtual Camera… (Windows).** It unregisters the camera's COM server.

## Added: custom browser docks, Ferrous Link discovery fixes, UI fixes: done

**Custom browser docks** (View → Docks → Custom Browser Docks…), Ferrous's version of OBS's
feature of the same name.
- **What they are:** a named dock showing a live, interactive web page, such as Twitch or
  YouTube chat, the Twitch activity feed, or StreamElements/Streamlabs docks. Presets fill in
  the common URLs.
- **They behave like built-in docks:** snap, tab, float, close and reopen from View → Docks,
  and they're saved in the layout. Each has a stable id, so renaming a dock or changing its
  URL keeps its place.
- **Windows:**
  - Each dock is a WebView2 child window (via `wry`) placed over the dock's rect every frame.
  - Pages hide whenever an egui layer is on top of them: menus, combo boxes, dialogs,
    floating docks, and the drop guides while dragging. Native windows always draw above egui,
    so this is what keeps those visible. `webdock::covered` is unit-tested.
  - Inactive tabs keep their page, hidden, so chat history and sign-ins survive.
  - The profile lives in `%APPDATA%\Ferrous\webview`.
  - Pop-ups open in the normal browser.
- **Packaging:**
  - The exe now imports `WebView2Loader.dll`, shipped next to it.
  - It also embeds an application manifest: Common Controls v6 (needed by WebView2 hosting),
    Windows 10/11 compatibility, and long paths.
- **Other platforms:** the dock shows the URL with an "Open in browser" button.

**Ferrous Link discovery**
- **Beacons:** they now go out on every network adapter, as multicast plus a subnet broadcast.
  Previously Windows used only its default adapter, which a VPN or a Hyper-V/WSL/VMware adapter
  often is. The receiver joins the group on every adapter, rechecking every 10 s.
- **Addresses:**
  - A sender's last address is remembered, so a saved link reconnects by name.
  - A display name ("Gaming PC") is no longer looked up as a hostname.
  - "Connection refused" and "unreachable" now come with advice.
- **Settings:** Settings → Ferrous Link lists, for each running output, this PC's addresses with
  the real port, adapter names, and virtual adapters marked. Click an address to copy it.

**UI fixes**
- **Missing glyphs:** five characters the bundled fonts lack drew as boxes. These were the
  fullwidth ＋/－ on the add/remove buttons, 🗔, 🧩 and 🧱. Every non-ASCII character in the UI
  was checked against the fonts.
- **Replay buttons:** they share the Controls dock's width, or stack when it's narrow, instead
  of overflowing.
- **Mixer:** channel names truncate in narrow mixers instead of being overprinted by
  "no device".
- **Windows log file:** release builds log to `%APPDATA%\Ferrous\ferrous.log`,
  because there's no console.

## Ferrous Link performance pass: done

The symptoms were that it struggled to hold 60 fps, latency built up, and it disconnected at
higher settings. Measured with `cargo run --release -p ferrous-link --example link_bench`: 1080p60,
textured, noisy, moving frames (harder to compress than games), sender and receiver over
loopback sharing this VM's 2 cores. On two real PCs each side gets its own cores.

| Codec | Before | After |
|---|---|---|
| JPEG q85 | 19 fps, ~354 ms latency | 38 fps, ~58 ms |
| JPEG q95 | 16 fps, ~550 ms | 35 fps, ~62 ms |
| JPEG q85, 720p60 | n/a | 60 fps, ~11 ms |
| Lossless | 53 fps | 59 fps |
| H.264 (x264) 20 Mbps | 3.5 fps, **4 disconnects** in 8 s | encoder-bound (≈42 fps on 2 cores), 0 disconnects |
| H.264 (x264) 50 Mbps | 0 fps, **4 disconnects** | encoder-bound (≈29 fps on 2 cores), 0 disconnects |
| Static scene, JPEG 1080p60 | ~330 Mbps | 60 fps at 1.1 Mbps |

JPEG per frame at 1080p (`--example codec_stages`, 2 cores): encode went from 31.5 to 13.4 ms.

**Fixes**
- **Disconnects:** each receiver had a 6-message queue, and a full queue disconnected
  hardware-codec receivers. A high-bitrate keyframe is more than 6 network chunks. Ordered
  traffic (stream, audio, pongs) now has a 48 MB byte budget; only a receiver that has stopped
  reading entirely is dropped.
- **Latency:** video now goes through a one-frame slot per receiver. A new frame replaces one
  that hasn't been sent, so a receiver that falls behind gets the newest picture instead of a
  backlog. The receiver does the same before decoding.
- **Receiver threading:** the receiver reads the socket on one thread and decodes on another.
  It used to stop reading while decoding, which the sender saw as a slow receiver.
- **JPEG codec:**
  - Encoding uses a SIMD encoder (`jpeg-encoder`, AVX2) straight from BGRA. It used to go
    through the `image` crate, plus a per-pixel BGRA→RGB copy.
  - Decoding uses `zune-jpeg` directly into each strip's rows of the output frame.
  - Strips: one per core, at least 8, aligned to the 16-row MCU.
  - Chroma is 4:2:0, the same as the stream it ends up in.
- **Lossless:**
  - XOR and LZ4 now run in 16 parallel chunks.
  - A receiver that misses a delta is resynced with a keyframe. Before, it showed corruption
    for up to 120 frames.
- **Repeats:** an unchanged picture (a static or memoised scene) is sent as a 9-byte `REPEAT`
  instead of being re-encoded and re-sent. Newly joined receivers always get a full frame, and
  a repeat never displaces a pending frame.
- **Sockets:**
  - 4 MB send and receive buffers.
  - Each message goes out as one buffered write; before, it was a separate 4-byte packet per
    message with no-delay on.
  - Plaintext bodies are written without a copy.
- **H.264 receiver:** `-fps_mode passthrough`, so FFmpeg no longer invents duplicate frames
  (it reported 77 fps from a 60 fps link, wasting receiver CPU).
- **Settings:** each codec shows a bandwidth guide.
- **Protocol version 2** (chunked lossless and `REPEAT`). Both PCs need this build; a mismatch
  says so clearly.

**Tests:** `static_scene_repeats_and_late_joiners_get_the_picture` (both codecs; lossless exact).
The adaptive-quality test now uses fresh frames, as live capture does.

## Smooth preview, and a preview on/off switch: done

**Why it was choppy** (measured with `cargo run --release -p ferrous-engine --example preview_cadence`,
moving 1080p test pattern):
- **30 fps cap:** the preview was capped at 30 fps, so a 60 fps canvas showed every other frame.
- **Timer throttle:** the cap was a timer, "at least 33 ms since the last preview". Against a
  33.3 ms frame period, any frame that came in a little early was skipped, leaving 50–68 ms holes.
- **Nearest-neighbour downscale:** fine detail shimmered as it moved.

| Canvas | Before | After |
|---|---|---|
| 60 fps | 28 fps preview, gaps 35 ms (sd 5.1), max 50 ms | **59.8 fps**, 16.7 ms (sd 1.1), 0% uneven |
| 30 fps | 25 fps, gaps 39.5 ms (sd 12.8), max 68 ms, 19% uneven | **29.8 fps**, 33.3 ms (sd 1.1), 0% uneven |

**Changes**
- **Engine cadence:** previews go out every N-th rendered frame, counted rather than timed.
  The default is every frame.
- **Size:** the preview is rendered at the pixel width it is shown at, as reported by the UI
  and rounded to 64 px, instead of a fixed 1280. A small dock costs less, and a large one is
  sharper.
- **Downscale:** box-filtered, averaging up to 4×4 samples per pixel, instead of
  nearest-neighbour.
- **No preview when nobody sees it:** while the window is minimised, or no preview dock is on
  screen, no preview is made.
- **Enable preview:**
  - In View → Docks and in a new right-click menu on the preview, as in OBS.
  - Off shows a placeholder with an "Enable preview" button. Streaming, recording and other
    outputs are unaffected.
  - On the live 1080p60 scene, the whole app went from about 175% to about 58% of a core
    (render thread about 72% → 24%).
- **Preview rate:** Match canvas, 30 or 15 fps. Lower rates use even frame steps.
- **Saved:** both settings are kept with the layout (`UiSettings::preview_enabled`,
  `preview_max_fps`).

**Test fix:** the VST3 host test built and copied the plugin from two parallel tests into
one path, so one test could load a half-copied library ("file too short"). The plugin is now
built once per test binary and moved into place only once it's fully copied.

## Sources list and right-click menu, OBS-style: done

- **Aligned rows:**
  - Each Sources row is laid out like OBS's: the name left-aligned, truncated if long, then the
    eye and lock toggles (and 📱 when the vertical canvas is on) in fixed 24 px slots on the
    right.
  - Before, the eye and lock were content-sized buttons before the name. Their glyphs differ
    in width (👁/◌, 🔒/🔓), so they shifted between rows and states, and the name was a
    centred button.
  - Hidden sources are dimmed; sources with errors show in the warning colour.
- **Right-click menu:** in the Sources list, or on the canvas, where it first selects the
  source under the pointer, as in OBS. It offers:
  - Properties and Filters.
  - Rename (F2) and Remove.
  - Order: Move Up/Down/to Top/to Bottom.
  - Transform: Reset, Fit, Stretch, Center to Screen.
  - Hide/Show and Lock/Unlock.
  - On the canvas, the preview options follow.
- **Properties window:** "Properties for '<name>'" holds the source's own settings and
  Opacity; position, size, crop and rotation are edited on the canvas (see below).
  The **Filters** window holds its video and audio filters. Double-click opens Properties,
  as in OBS; rename moves to the menu and F2.
- **Properties dock:**
  - Removed from every layout preset.
  - Closed once in existing saved layouts, by the one-time update `UiSettings::layout_rev = 1`.
  - Still available under View → Docks, and re-opening it sticks.
  - Both behaviours are unit-tested.

## Transform on the canvas instead of number fields: done

Properties no longer has Position, Scale, Crop or Rotation fields. As in OBS, the source itself
is edited on the canvas:
- **Move:** drag the source. It snaps to the canvas edges and centre; hold Alt for no snapping.
- **Resize, eight handles:**
  - Corners keep the aspect ratio; hold Shift for free.
  - Edges stretch one side.
  - The opposite side stays put, rotated sources included.
- **Crop:** hold Alt and drag any handle to crop that side, or drag outwards to uncrop. The
  picture stays in place.
- **Rotate:** with the knob above the top edge (Shift: 15° steps), or right-click → Transform
  → Rotate 90°.
- **Right-click → Transform:** Reset, Fit/Stretch/Center to Screen, Rotate 90° either way,
  Remove Crop.
- **Cursors:** they show what a handle will do (resize directions, or a crosshair while Alt is
  held).
- **Opacity** stays as a slider in Properties, since it has no canvas gesture.
- **Geometry:** `Transform::drag_handle` in ferrous-core, unit-tested for corners, edges, Shift,
  crop and uncrop clamping, and rotated boxes.
- **Fix:** canvas drags hit-test where the button went down, not where egui reports the drag
  start a few pixels later, so a quick grab of a handle no longer selects whatever is under it.

## Render pipeline: frame times and pacing: done

The render loop now records every frame's start-to-start interval, total work and a six-stage
breakdown (`Engine::frame_times()`, the last 600 frames). These are measured with
`cargo run --release -p ferrous-engine --example render_bench -- 10 cpu|gpu light|heavy`.
- **light:** 1080p60 canvas with a moving full-screen source, a scaled 640×360 overlay, text,
  and the preview on.
- **heavy:** light plus a 1080×1920 vertical canvas and a 720p JPEG Ferrous Link output with a
  receiver.

This VM has 2 cores, so "heavy" is overloaded here; a real PC has room to spare.

| Scenario | Before | After |
|---|---|---|
| CPU light: frame interval | 16.67 ms (sd 0.18, max 18.8) | 16.67 ms (**sd 0.06**, max 17.2) |
| CPU light: work per frame | mean 8.67, p99 14.4 ms | **mean 3.45, p99 6.2 ms** |
| CPU heavy: frame interval | 26.4 ms (sd 5.7, max 73) | **17.3 ms** (sd 4.1, max 41) |
| CPU heavy: late frames (>25 ms) / lagged in 10 s | 308 / 206 | **29 / 18** |
| GPU (llvmpipe, software) light: max frame | 179 ms | 31–92 ms (noisy: the rasterizer shares the 2 cores) |

**Changes**
- **Previews on their own thread:**
  - The render thread hands the frame over; a worker downscales it.
  - Each preview kind has a "newest wins" slot.
  - The worker wakes the UI.
  - This was the largest single cost: 3.7 ms per frame at 60 fps in "light", 12.7 ms in
    "heavy".
- **A compositor thread pool of its own:**
  - The render thread joins a dedicated rayon pool (`use_current_thread`).
  - Ferrous Link's JPEG encoder, filters and preview work used to queue on the same global pool,
    so a frame could wait mid-composite for unrelated work. This caused the 53 ms compose
    spikes under load.
- **Thread priority:** on Windows the render thread and its pool run at above-normal
  priority, so encoders, previews and background work yield to rendering.
- **Frame buffer pool:**
  - `ferrous_core::FramePool` recycles 8 MB frame buffers once every consumer has dropped them.
    It's used for compositor output (CPU path and GPU readback) and for nested scenes.
  - On Windows a fresh large allocation is new pages from the OS, faulted in and zeroed on
    first touch.
  - Tested: a recycled buffer never shows the previous frame, opaque or transparent.
- **GPU source textures are reused:** frames no longer on screen return their textures to a
  spare list, and new frames of the same size upload into one. Before, a texture was created
  and destroyed for every new frame of every live source.
- **Windows capture (WGC):** one copy per frame, row by row from the mapped texture into a
  recycled buffer. Before, it was a scratch copy for row padding, then `to_vec()` into a fresh
  allocation.
- **Checked and left alone:** Rust's `thread::sleep` on Windows already uses high-resolution
  waitable timers (Windows 10 1803+), so frame scheduling isn't limited by the 15.6 ms
  system tick.

## Crash reporting: done

- **The previous session's log is kept** as `%APPDATA%\Ferrous\ferrous.previous.log`.
  Before, each launch replaced the log, so a crash's log was gone as soon as the app was
  reopened.
- **Panic hook:** every internal error (Rust panic), on any thread, is written to the log and
  to `crash-<UTC time>.txt` beside it, with the thread, the message, the source location and a
  backtrace. If it's the UI thread (the app is closing), a dialog says so and gives the
  report's path. Before, release builds have no console, so a panic closed the window without
  a trace.
- **Ferrous Link handshake:** a receiver that hangs up mid-handshake (another Ferrous version on
  the other PC) is now explained in the log as a version mismatch, and logged once a minute
  per address instead of every 2 s.

## Ferrous Link 3: rebuilt on obs-teleport's design: done

The problem: the link didn't hold 60 fps on a wired network. The user's link was the Hardware
codec at 1440p60, which:
- piped 14.7 MB of BGRA per frame (about 885 MB/s) into FFmpeg;
- converted it with FFmpeg's single-threaded scaler before NVENC;
- decoded H.264 in software on the receiver;
- converted back with single-threaded `swscale`;
- piped another 885 MB/s back.

That's two single-threaded 1440p conversions per frame.

**Reference: obs-teleport.** It is JPEG only (libjpeg-turbo). It encodes each frame whole on
its own worker, with several frames in flight, and sends them in order. The receiver decodes on
parallel workers, reorders, and lets OBS's async-source timing play frames at their
timestamps. It reuses no code (GPL); the design is reimplemented here.

**JPEG codec (recommended on wired networks)**
- **Codec:** libjpeg-turbo 3.1, built statically from source with SIMD required, via the
  `turbojpeg` crate, cmake and nasm; it cross-compiles for windows-gnu. It goes straight from
  BGRA with 4:2:0 chroma, and decodes straight to BGRA into recycled buffers.
  - Per frame, on one core of this VM: 1080p encode 9.6 ms, decode 13.8 ms; 1440p encode
    17.3 ms, decode 24 ms (noisy test content; real footage is smaller and faster).
  - The previous encoder took 31 ms for 1440p on both cores.
- **Sender:**
  - A dispatcher hands whole frames to 2–8 encoder workers (half the cores).
  - An orderer broadcasts them by sequence number; REPEATs go through the orderer too.
  - Each receiver has a 3-frame ordered queue (about 50 ms), instead of the old one-frame
    "newest wins" slot, so a momentary hiccup no longer costs a frame.
- **Receiver:**
  - The socket thread only reads and decrypts.
  - 2–8 decode workers (one for lossless, whose deltas need order).
  - A presenter reorders results and gives each frame a due time: sender timestamp, plus the
    smallest transit seen in a sliding 2–4 s window, plus an adaptive jitter delay. The delay
    starts at one frame, grows half a frame when more than 1% (or 3) of frames in a second
    are late, shrinks 1 ms per second after 5 clean seconds, and is capped at 150 ms.
  - `latest_frame()` (once per render tick) shows the newest due frame. This is the OBS
    async-source model.
  - Latency now includes the jitter delay, so it reports capture to on-screen.

**Hardware codec (for Wi-Fi)**
- **Sender:** Ferrous converts and scales to NV12 on all cores (the recorder's converter), so the
  pipe carries 1.5 bytes per pixel instead of 4, with no FFmpeg input conversion. NVENC, QSV
  and AMF take NV12 directly.
- **Receiver:** `-hwaccel auto` (GPU decode where available) and NV12 output (2.6× less pipe
  traffic), converted to BGRA on all cores in Ferrous (BT.709 limited range, unit-tested against
  the encoder's converter).

**Protocol 3:** microsecond timestamps; whole-frame JPEG payloads. A version mismatch now says
which PC to update.

**Measured over loopback on this 2-core VM** (sender and receiver share the cores). The
benchmark (`--example link_bench`) now also counts how many 60 Hz "render ticks" on the
receiver showed a new picture.

| JPEG q85, 1080p60 | Before | After |
|---|---|---|
| Received | 38 fps | **59.7 fps** |
| Sender skips/drops | 22 / 0 per s | **0 / 0** |
| New picture per 60 Hz tick | not measured | 94% |
| Latency | ~58 ms | ~55 ms (now including the jitter buffer) |

1440p60 needs about 41 ms of CPU per frame for both ends together, more than 2 cores provide.
Two real PCs each run their own side. The Hardware path here is limited by software x264,
standing in for NVENC.

- **Packaging:** licence notices for libjpeg-turbo are in `licenses/`.
- **Dependencies:** `jpeg-encoder`, `zune-jpeg` and `image` were removed from ferrous-link.

## OBS-informed optimizations: done

Sources: OBS's knowledge base and forum guides, and libobs's `obs-video.c`
(the developer docs site wasn't reachable). Each item below is something OBS
does that Ferrous didn't.

- **GPU readback ring** (`ferrous-render/src/gpu.rs`). The compositor used to
  submit a frame and wait for the GPU to finish before reading it. Now two
  readback slots alternate: the frame submitted this tick is read next tick
  (OBS's staging-surface scheme), so the render thread never blocks on the
  GPU. Costs one frame of latency (16 ms at 60 fps, 7 ms at 144), and can be
  switched off in Settings > Video > GPU readback. Only the program output
  is pipelined; studio mode, transitions and Ferrous Link scene targets read
  back immediately. The memo records each result under the inputs it was
  actually drawn from, so static scenes still cost nothing.
- **GPU NV12 conversion.** After compositing, two small passes (luma, and
  chroma sampled at each 2x2 block's centre) write BT.709 limited-range NV12.
  `VideoFrame::nv12` carries it; the output thread and Ferrous Link's
  hardware path use it instead of converting on the CPU when the output is
  the canvas size. "Encoder input: Auto" now picks Ferrous conversion whenever
  the GPU provides NV12. Scaled outputs, I420 (x264) and the CPU renderer
  keep the CPU converter.
- **Sources that aren't shown stop.** Only the visible items of the program
  scene, the studio preview scene, a transition's outgoing scene, nested
  scenes and Ferrous Link targets run. Anything else stops after a 10 s grace
  period (so switching back is instant) and starts when shown. Right-click >
  "Keep running when hidden" exempts an item (`SceneItem::keep_running`).
- **Capture rate follows the canvas.** Windows capture asks for frames no
  faster than ~1.3x the canvas rate (`MinimumUpdateInterval`, Windows 11
  22H2+): a 144 Hz monitor no longer costs 144 copies a second for a 60 fps
  canvas. At 144 fps and above the limit changes nothing, so high refresh
  rates stay smooth. Not run on Windows yet.
- **GPU scheduling priority** (Settings > Video, off by default): the same
  `D3DKMTSetProcessSchedulingPriorityClass` call OBS makes (realtime when
  running as administrator, else high).
- Browser sources already default to 30 fps like OBS, and now sleep with the
  rest.

Tests: GPU NV12 against a reference conversion, pipelined ordering and
equality with the synchronous path, the renderer's memo with pipelining on,
which items count as shown, and the grace period.

## Post-delivery fixes (not yet in a build)
- Vertical-canvas right-click menu acts on the vertical layout; rename commits on Enter / cancels on Esc; resize keeps aspect on every handle (Shift = free stretch).
- Hover tooltips: `widgets::hint` table + `row_label` / `tip`, applied across Settings and the action buttons.
- Ferrous Link: hardware (MPEG-TS) streams now start receivers on PAT + PMT + keyframe (`ts.rs` scanner; `Client.stream_wait`), so switching encoders no longer leaves the receiver blank. Receiver decoder probes minimally and reconnects if bytes arrive but no picture does for 5 s. A restarted sender waits briefly for its previous port so receivers find it again. Test: `receiver_follows_encoder_switches`.
- Link sharpness: JPEG is now 4:4:4 (was 4:2:0), default quality 92; the hardware path uses VBR instead of CBR.

## Background removal and themes (not yet in a build)
- **Background Removal filter** (video filter, per source): PP-HumanSeg (Apache-2.0, OpenCV zoo) via `tract` in the new `ferrous-matte` crate; pure Rust, no extra DLLs. The model runs on its own thread (`ferrous-render/src/matte.rs`) on a 192x192 downscale at <= `max_fps` (default 20), with temporal smoothing; the filter itself only upsamples the mask and scales alpha (rayon), memoised per frame/mask. A static source isn't re-segmented. Tests: a real photo (skipped when /tmp/messi.jpg is absent), an empty frame, mask maths, worker lifecycle.
- **Themes** (`app/src/theme.rs`): constants became functions over a palette table (Ferrous, AMOLED Black, Neon Noir, Aurora Glass, Synthwave Horizon, Sakura Night, Nebula, Vapor Sunset, Thunderhead). Settings → Advanced → Appearance picks theme and glass level (Off / Light / Full) with live preview. Glass = translucent panels over a painted backdrop (glows, stars, grid, rain, dots), sheen and bright edge; gradient primary buttons. It is an approximation: the backdrop is soft by construction, there is no true per-panel blur.
- Start Streaming / Start Recording buttons are drawn by hand (centred, shrink to fit).
- **Living themes**: new **Rainfall** theme (and Thunderhead) have weather (`app/src/fx.rs`): rain streaks land on the dock panels (rectangles recorded each frame from the dock viewer), splash, and sometimes leave a bead that crawls down the glass with a wet trail (stop-and-go) and runs off the bottom as a drop. Thunderhead adds lightning flashes. The video preview gets splashes but no drips. Other themes drift their glows; Aurora/Nebula stars twinkle; Synthwave's grid scrolls; Sakura drops petals. Settings → Appearance → Animated backgrounds: Off / Reduced (30 fps, half the rain) / Full. Animation repaints continuously, so Off is fully static.

## Rename and optimization pass

- **Project renamed to Ferrous** (was Ember Studio). Crates are `ferrous-*`, the binary is `ferrous`, the config folder is `%APPDATA%\Ferrous`. An existing `%APPDATA%\EmberStudio` folder is moved over automatically on first start. Kept stable on purpose: the `ember` theme id, the `ember_cpu`/`ember_mem` stat ids, and the Link discovery field, so saved profiles and mixed-version Link senders keep working. The old `Ember` conversion-mode value still loads (serde alias). Plugin ABI constants are now `FERROUS_*`, so plugins must be rebuilt.
- **Realistic rain**: drops fall only above exposed docks, splash on the top edge, pool into beads that merge, creep to the side and drip off. Nothing is drawn over the canvas or other docks.
- **Optimization**: animation and weather simulation pause when the window is unfocused or minimized; per-source filter chains run in parallel; the GPU NV12 pass runs only when an encoder needs it; Link hardware encoding stops while no receiver is connected; theme sync does no per-frame allocation.
- Workspace clippy (`-D warnings`) and the full test suite pass.

## Source Clone

- New source type **Source Clone** (`SourceConfig::Clone { source }`): shows another item's live frame with its own position, crop and filters, without capturing again. Add it from the Sources "+" menu; pick the original (any scene) in Properties.
- Engine: `item_frame` resolves the original's newest raw frame; the original is kept running while a visible clone needs it, even from another scene. A clone of a clone, or of itself, shows nothing (no cycles). Audio is not duplicated.
- Test: `a_clone_shares_its_originals_picture_and_keeps_it_running`.

## Track Matte transitions (Nerd or Die style packs)

- New transition **Track Matte** (`TransitionKind::Matte { path, layout, invert }`): a video's brightness decides where the incoming scene shows (white = new, black = old). Layouts: matte only, picture | matte (side by side), picture over matte. The picture half, when present, is drawn on top. Runs for the transition's duration; until the first video frame arrives the outgoing scene is held.
- Works with matte packs such as Nerd or Die's free Matte pack and OBS track-matte stingers; OBS imports map `track_matte_enabled` (layout names matched loosely, best effort).
- No third-party transition files are bundled (Nerd or Die states no license terms).
- Not yet done: per-source show/hide transitions (OBS 27 "source visibility transitions").

## Add Source window and Game Capture

- **Add Source window** (`app/src/add_source.rs`), modelled on OBS 32.1's: a list of source types on the left (plus Plugins and OBS plugins), and on the right a "Create new" tile and tiles for sources of that type already in your scenes. "Add existing" adds one again as a live clone (nothing is captured twice). "Recently Created" page, optional name, "Make source visible" checkbox, Esc/Close. It replaces the old "+" menu.
- **Game Capture** (`SourceConfig::GameCapture { program, capture_cursor }`): finds the full-screen or borderless game in front (or a named program) and follows it across restarts. Uses Windows Graphics Capture at the canvas frame rate, with no code injected into the game, so anti-cheat is unaffected. A true injected hook (OBS-style) was deliberately not built. OBS `game_capture` sources now import as Game Capture.
- Windows target type-checks clean; behaviour not yet seen on a real game.

## Undo/redo, audio sync, per-scene transitions, projector and multiview

- **Undo/redo** (`app/src/history.rs`): snapshots of the scene collection taken once an edit settles (mouse released, text field left), so a drag or slider is one step. Ctrl+Z, Ctrl+Shift+Z / Ctrl+Y, and ↶ ↷ buttons in the Sources dock. 100 steps. Which scene is live isn't an edit. Loading another collection clears the history. Tests in `history.rs`.
- **Audio sync offset**: per mixer channel, 0 to 5000 ms (Audio properties window; sources, Desktop and Mic). `DelayLine` in `ferrous-audio` holds a channel back in 10 ms chunks before its filters. Saved per source (`audio_sync_ms`) and per device. OBS import maps `sync` (ns, positive only). Tests: `delay_tests`.
- **Per-scene transition**: right-click a scene, "Own transition". The scene going live brings its own transition (`Scene.transition`), shown with ⇄ in the list.
- **Program projector** (View menu): its own window; drag to another monitor, double-click for full screen, Esc leaves it. The preview keeps rendering while the main window is minimized if it's open.
- **Multiview** (View menu): every scene as a small live picture (engine renders one scene per 100 ms, only while open), program outlined red, studio preview green, click to switch. It starts the sources of scenes that aren't live while open.
- **HDR10 output, phase 1 (SDR sources → HDR10).** Per-output `hdr` toggle (Settings → Output → encoder section, with an SDR-white level, default 203 nits). Only for HEVC/AV1 encoders (not H.264, not Media Foundation HEVC). The CPU converter gained `YuvLayout::P010` (hardware) and `I420P10` (software): sRGB → linear → BT.709→BT.2020 primaries → SDR white at the chosen nits → PQ (ST 2084, table-driven) → 10-bit limited-range BT.2020 Y'CbCr. FFmpeg gets `p010le`/`yuv420p10le` input, bt2020/smpte2084/bt2020nc tags, and `-profile:v main10` on NVENC/QSV HEVC. HDR forces Ferrous-side conversion (FFmpeg's scaler can't do the mapping) and skips the GPU NV12 pass. Tests: PQ reference points, white/black/neutral-chroma codes for both layouts, scaled conversion, argument tags.
- Not done (HDR phases 2–3): true HDR capture (WGC float scRGB from HDR monitors), a float canvas through the compositor, HDR mastering metadata (MaxCLL/mastering display), and tone-mapped preview for SDR displays. Not verified on a real HDR player or GPU encoder yet.
- Checked: workspace clippy and 157 tests pass, Windows target type-checks. The two new windows haven't been seen running.

## Encoder quality pass
- NVENC: two-pass rate control (`-multipass qres`, `fullres` on p6/p7), H.264 High profile (FFmpeg defaults to Main), HEVC/AV1 reference B-frames (`-b_ref_mode middle` with 2+ B-frames), AV1 10-bit internal precision for 8-bit input (`-highbitdepth 1`), and for Quality/VBR a 20-frame lookahead with temporal AQ. CBR streaming stays lookahead-free for latency. `-tune uhq` deliberately not used (reported crashes at end of file).
- Quick Sync: adaptive I/B placement (+ `-b_strategy 1` with B-frames) for H.264/HEVC, H.264 High profile.
- AMF: H.264 High profile, VBAQ on H.264/HEVC.
- SVT-AV1: `tune=0` (visual-quality tune) in every rate mode.
- Not changed: x264/x265/libaom, D3D12, Vulkan, Media Foundation (already sensible or minimal knobs). AMF has no CRF-like mode in FFmpeg, so Quality stays constant-QP.
- Unverified on real GPUs: these are options FFmpeg documents, but an older FFmpeg build or GPU may reject one; use the custom-options box or tell me which encoder fails and I'll gate it.

## Recording presets and benchmark
- Settings → Recording gains a Preset chooser and a "Presets & benchmark…" window. Built-in presets (Max quality, Balanced, Light, Small files) are generated for the encoders that passed the test; your own are saved with the profile (`recording_presets`). A preset holds encoder, quality, container, audio and recording size.
- Benchmark (`ferrous-bench::compare`): renders lossless test footage once (built-in fractal+test-card scene, or your own clip), encodes it with each setting one at a time, and measures encode speed, bitrate and quality (SSIM/PSNR; VMAF when the FFmpeg build has libvmaf). Flags ★ best quality, ◆ best value (within a barely visible difference of the best, but the lightest), ⚡ lightest, among settings that keep up with 25% headroom. Options: equal-bitrate mode, sweep of each encoder's slow/middle/fast preset, cancel. Results have Use / Save as preset.
- Tested end to end with real FFmpeg/libx264 here; the window itself and GPU encoders are not yet verified visually/on hardware.

## GPU filters, GPU-resident capture, GPU figure, frame cap, audit pass
- **GPU filters** (`ferrous-render/src/gpufx.rs`): chroma key, colour correction, scroll, LUT and the background-removal mask as render passes before the composite; compiled lazily on first use. CPU twins remain and tests compare both.
- **GPU-resident capture** (`ferrous-capture/src/gpu_ring.rs`, `VideoSettings.gpu_capture`, off by default): the Windows Graphics Capture callback copies each frame GPU-to-GPU into a ring of 4 shared D3D11 textures and publishes a `SharedFrame`; the DX12 compositor opens the NT handle (adapter LUID checked) and caches it per slot. Only top-level scenes and layers without plugin or background-removal filters take this path; everything else, any error, or a different GPU falls back to the CPU frame, which keeps being produced for a short while after it was last needed. Not yet measured on real hardware.
- **Sharper downscaling**: area-averaged sampling for layers shrunk more than ~1.25:1 on both compositors.
- **System GPU figure**: `ferrous-sysmon/src/nvml.rs` loads `nvml.dll` dynamically (5 s after start) and reports card-level utilisation; the Windows "GPU Engine" counters are only a fallback because they were seen reading 50%+ on an idle app that RTSS showed at 5%. Task Manager's per-process figure shows the same inflation for a bare eframe window, so it is a counter artefact, not Ferrous.
- **Window frame rate** (`ui.max_fps`, default 60, clamped 15–480, 0 = unlimited): the UI thread sleeps to the frame interval; video frame rate is separate.
- **Startup stability**: wgpu on Windows is limited to Vulkan and DX12 unless `WGPU_BACKEND` is set; DX12 is tried first only when GPU capture is on; stage logging while the compositor starts; `native-crash.txt` from an unhandled-exception filter. The crash was intermittent and the OpenGL enumeration is the suspected cause, so it needs more launches to call it closed.
- **Audit pass** (static review, no real-GPU timings): persistent dynamic-offset uniform buffer instead of one buffer per layer per frame; capture wait sleeps after 50 spins; audio scratch buffers pre-sized; bounded engine-event, remote-client and remote-call channels; `convert` skips a wrong-sized frame instead of panicking; poisoned-lock recovery in transitions; `saturating_sub` on scene indices; bulk byte-to-float conversion in app-audio capture.
- Checked on Linux: workspace clippy with `-D warnings`, render/engine/output/remote/audio/capture/sysmon tests (compositor tests on a software Vulkan driver), Windows target type-checks. Screenshots in the README were retaken from this build.
- Still to do: measure GPU capture on and off at 1080p/1440p/4K on a real PC, move the background-removal mask readback to the GPU so those layers can stay GPU-resident, and retire the CPU render path except as a fallback once the GPU path is proven.

