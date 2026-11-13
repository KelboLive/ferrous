<p align="center">
  <img src="docs/icon.png" width="128" alt="Ferrous icon">
</p>

# Ferrous

![Ferrous](docs/screenshot.png)

**A lean, fast recording and streaming studio for Windows, written in Rust: a modern successor to OBS Studio.**

## The goal

OBS is the standard, but it spends more CPU, GPU and memory than it needs to, and it makes you hunt
through plugins for things people use every day. Ferrous has one aim: **capture, composite, encode,
record and stream with as little overhead as possible, and still give you the cleanest, smoothest picture
your hardware can produce.** In practice that means:

- **Low resource use first.** A multi-threaded GPU compositor with a CPU fallback, finished NV12 handed
  straight to the encoder, memoised static scenes, and no work done for windows or previews you aren't
  looking at. Every feature is judged against its cost.
- **Picture quality you can measure.** Every encoder is tuned (two-pass, lookahead, adaptive
  quantisation, 10-bit internals), HDR10 output is built in, and a built-in benchmark compares
  settings by speed *and* quality (SSIM/VMAF) so you pick settings from evidence, not guesses.
- **The things OBS needs plugins for, built in.** Source clone, undo/redo, track-matte transitions, vertical
  canvas for TikTok/Shorts, Ferrous Link (a LAN video/audio link between PCs), virtual camera, VST3, and
  safe game capture that never injects into a game.
- **Familiar, not foreign.** obs-websocket 5 compatible (Stream Deck, Touch Portal and Streamer.bot work),
  OBS scene-collection import, and an OBS-style Add Source dialog.

> **Status: v0.4, pre-release.** Built and tested on Linux with a Windows cross-build; much of the Windows-only
> code (virtual camera, game capture, VST3 editors, the D3D12/Vulkan encoders, HDR playback) has not yet been
> run on real hardware. Progress, test evidence and deferred items are in [`docs/PROGRESS.md`](docs/PROGRESS.md).

## Highlights

| | |
|---|---|
| **Recording presets and a speed/quality benchmark** | Save named recording presets (built-in *Max quality*, *Balanced*, *Light*, *Small files* are generated for your encoders). The benchmark encodes the same footage with each setting, then reports speed, bitrate and quality (SSIM/PSNR, or VMAF if your FFmpeg has it) and flags the best quality, best value and lightest settings that still keep up in real time. |
| **HDR10 output** | SDR sources mapped to 10-bit BT.2020/PQ for HEVC and AV1 (adjustable SDR white level). True HDR capture is planned. |
| **Tuned encoders** | NVENC two-pass and lookahead, H.264 High profile, AV1 10-bit internals, Quick Sync adaptive I/B, AMF VBAQ, SVT-AV1 visual-quality tune. |
| **Safe Game Capture** | Follows your game with Windows Graphics Capture: no DLL injection, so it is anti-cheat safe. |
| **Source Clone and Track Matte** | Reuse a live source without a second capture; stinger-style transitions with a track matte. |
| **Add Source dialog** | OBS 32-style source picker with categories, existing sources and recently used. |
| **Undo/redo, per-scene transitions, projector and multiview windows, audio sync offset** | Everyday tools that are plugins or workarounds in OBS. |

## Features

| Area | What's included |
|---|---|
| **Rendering** | GPU compositor (wgpu: D3D12/Vulkan) with CPU fallback, studio mode, transitions (cut, fade, fade-to-colour, swipe, slide, stinger, **27 luma wipes** or your own wipe image), nested scenes, **source clone** (reuse a live source with its own crop and filters, no second capture), **rotation** (antialiased, with an on-canvas rotate handle) |
| **Vertical canvas** | Second 9:16 canvas with its own per-scene layout, for TikTok, Shorts and Reels, alongside the main canvas |
| **Sources** | Display, window, **game capture** (follows your game, no injection) and **application audio** capture; webcams and capture cards; media files and URLs; images; colour; **text** (from file too); **browser** (headless Edge, transparent); **Ferrous Link**; plugin sources; **OBS plugin sources** (run on your OBS install's libobs); test pattern |
| **Filters** | Chroma key, colour correction, scroll, LUT; RNNoise noise suppression, gate, compressor, limiter, gain; **VST3 effects** (generic editor, plus the plugin's own editor window); plugin filters |
| **Audio** | WASAPI desktop and mic plus per-source channels, 6 tracks, monitoring; **OBS-style animated mixer** (stereo peak/RMS meters with peak hold and clip lights, gliding faders, horizontal or vertical layout); volume and mute saved per source |
| **Encoders** | NVENC, AMF, Quick Sync (H.264/HEVC/AV1), **DirectX 12 Video and Vulkan Video** (H.264/HEVC/AV1 on any GPU whose driver supports them), Media Foundation, x264, x265, SVT-AV1, AOM. Every encoder is test-encoded before it's offered. The canvas can be converted to the encoder's YUV format in Ferrous, multi-threaded, before it reaches FFmpeg (62% less data per frame) |
| **Outputs** | Recording (MKV, MP4, fragmented MP4, MOV, TS, FLV; multitrack; separate recording resolution), streaming (RTMP/RTMPS, SRT, WHIP) with auto-reconnect, **multistream destinations**, **replay buffer** (plus a vertical clip), **virtual camera** (Windows 11) |
| **TikTok** | TikTok LIVE preset, per-session key prompt, vertical canvas output, vertical replay clips |
| **Ferrous Link** | obs-teleport successor: LAN discovery on every network adapter, JPEG, lossless+alpha or hardware codecs, adaptive quality, PIN with encryption, live stats |
| **Control** | **Undo/redo** (Ctrl+Z), per-scene transitions, program projector and multiview windows, Global hotkeys, **obs-websocket 5 compatible** API (Stream Deck, Touch Portal, Streamer.bot, TikFinity) |
| **Extensibility** | Native C-ABI plugin system (`docs/PLUGINS.md`); **OBS plugin bridge** for OBS plugin sources (`docs/OBS_PLUGIN_BRIDGE.md`); VST3 |
| **Presets** | **Recording presets** (built-in and your own) and a **speed/quality benchmark** that ranks settings for your PC |
| **Setup** | First-run **benchmark** (encoders, compositor, upload, ingest-server ranking) with separate streaming and recording recommendations; quick auto-config wizard |
| **Organisation** | Profiles, scene collections, **OBS scene-collection import** |
| **Ease of use** | **Simple mode** by default (switch to Advanced any time), empty-state tiles and scene templates, a **pre-flight check** before recording or going live, plain-language errors with fix buttons, Undo toasts, a **Ctrl+K command palette**, a first-run tour and troubleshooting page; accessibility: interface zoom, high-contrast theme, colour-blind meters, reduce motion |
| **UI** | Fully modular docks: every card (preview, scenes, sources, mixer, controls, properties, stats, performance, vertical canvas) can be dragged and snapped beside, above, below or into any other dock with a live snap preview, resized, floated, closed and reopened; layout presets, named saved layouts and a layout lock; **custom browser docks** (chat, alerts, activity feeds as live pages, like OBS); **Stats** dock (choose your stats); **Performance Monitor** (per-source CPU/GPU with graphs); layout remembered between sessions |

## Screenshots

All taken from the current build, on a software-rendered virtual display (so the preview is black and there are no audio devices). Everything shown is the real interface.

| | |
|---|---|
| ![Simple mode](docs/screenshots/02-simple-mode.png) **Simple mode** (the default): scenes, sources, preview, mixer and two big buttons, plus a three-step tour | ![Empty scene](docs/screenshots/03-empty-scene-tiles.png) An empty scene offers big tiles instead of a blank list, and removing something offers Undo |
| ![Pre-flight](docs/screenshots/04-preflight-check.png) **Pre-flight check**: problems in plain words with one-click fixes | ![Templates](docs/screenshots/05-scene-templates.png) Scene templates: Screen, Just chatting, Gameplay, Tutorial, Starting soon |
| ![Palette](docs/screenshots/06-command-palette.png) **Ctrl+K** finds any action, scene, source type or setting | ![Advanced](docs/screenshots/10-advanced-mode.png) **Advanced mode**: every dock and menu, one click away |
| ![General](docs/screenshots/07-settings-general.png) Settings → General: themes and accessibility (interface size, high contrast, colour-blind meters, reduce motion) | ![Recording](docs/screenshots/08-settings-recording.png) Settings → Recording: quality profiles and presets |
| ![Stream](docs/screenshots/09-settings-stream.png) Settings → Stream, with a live "GB per hour / Mbps needed" hint | ![Source menu](docs/screenshots/12-source-context-menu.png) Source right-click menu |
| ![Scene menu](docs/screenshots/13-scene-context-menu.png) Scene right-click menu | ![Setup](docs/screenshots/01-first-run-setup.png) First-run setup and benchmark |

## Architecture

```
app/                 egui GUI: docks, settings, setup benchmark, wizards, remote-API bridge
crates/
  ferrous-core         scene model, settings, profiles/collections, OBS import, filter configs
  ferrous-capture      sources: WGC, DirectShow/media (FFmpeg), text, browser (CDP), app audio, Link, plugins
  ferrous-render       GPU (wgpu) and CPU compositors, video filters, transitions
  ferrous-audio        WASAPI capture, mixer (6 tracks), audio filters, monitoring
  ferrous-encode       encoder catalog, hardware probe, FFmpeg args, auto-config
  ferrous-output       FFmpeg outputs: record, stream, replay segments, multitrack
  ferrous-engine       render clock, source cache, outputs, destinations, links, perf accounting
  ferrous-link         Ferrous Link sender/receiver, codecs, discovery, encryption
  ferrous-remote       obs-websocket 5 protocol server
  ferrous-plugin-api   stable C ABI for plugins
  ferrous-plugins      plugin loader/host
  ferrous-sysmon       system, process and Windows GPU counters
  ferrous-bench        setup benchmark and ingest ranking
  ferrous-vst3         VST3 host (load, process, parameters, state, editor window)
  ferrous-vcam         virtual camera frame transport and Windows camera control
  ferrous-vcam-source  the camera's Media Foundation source (COM DLL loaded by Windows' Frame Server)
  ferrous-obs          OBS plugin bridge client and protocol
tools/obs-host       ferrous-obs-host: runs OBS plugin sources on the real libobs
plugins/example      example plugin (gradient source, invert filter, gain filter)
plugins/vst3-test-gain  tiny VST3 effect used to test the VST3 host
```

Encoding goes through FFmpeg, the same way OBS uses libavcodec for most of its outputs, which gives
one stable interface to every hardware and software encoder.

## Building

### Option A: GitHub Actions (no local setup)

Push this folder to a GitHub repository. The included workflow (`.github/workflows/windows.yml`)
runs the tests, builds a release `.exe`, bundles FFmpeg (a GPL build with every encoder above) and
uploads `ferrous-windows-x64` as a downloadable artifact.

### Option B: locally on Windows 10 (1903+) or 11

1. Install Rust: <https://rustup.rs> (pick the default MSVC toolchain and install the Visual Studio
   C++ Build Tools when prompted).
2. Get FFmpeg: download `ffmpeg-master-latest-win64-gpl.zip` from
   <https://github.com/BtbN/FFmpeg-Builds/releases>. Put `ffmpeg.exe` next to `ferrous.exe`
   or on your `PATH`, or set its path in Settings > Advanced.
3. Build and run:
   ```
   cargo run --release -p ferrous
   ```
4. Run the tests (the end-to-end recording test needs FFmpeg on `PATH`):
   ```
   cargo test --workspace --release
   ```

Profiles and scene collections are saved to `%APPDATA%\Ferrous`, and plugins go in `%APPDATA%\Ferrous\plugins`. Recordings go to your Videos folder by default.

## What's next

The previous to-do list is done (see "README to-do list" in `docs/PROGRESS.md`). What remains:

- **Run the Windows-only parts on real hardware.** The virtual camera source, VST3 editor windows,
  the D3D12/Vulkan encoders and the OBS host on `obs.dll` all compile for Windows, but haven't run
  on a Windows machine yet.
- **True zero-copy encoding** (the canvas texture straight to the hardware encoder without a trip
  through system memory). It needs the encoder in-process rather than through FFmpeg's pipe.
- **OBS plugin filters** through the bridge (sources work today).

## License

GPL-2.0-or-later. This project contains no OBS code. The bundled FFmpeg GPL build is GPL-licensed.
