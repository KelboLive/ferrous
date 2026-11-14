# Changelog

Newest first. Each entry lists what changed for users; the detail and test evidence live in
[`docs/PROGRESS.md`](docs/PROGRESS.md).

## v0.4 (unreleased)
- **Video filters on the GPU**: chroma key, colour correction, scroll, LUTs and the background-removal mask now run as GPU passes in the compositor instead of per-pixel CPU loops, with CPU twins kept as the fallback (tests prove both give the same picture).
- **GPU-resident capture (experimental, off by default; Settings → Video → GPU capture)**: Windows display, window and game capture can hand each frame to the GPU renderer as a shared texture, skipping the copy to system memory. It falls back to the normal CPU copy by itself for plugin filters, background removal, nested scenes, a different GPU, or any error.
- **Sharper downscaling**: layers shrunk more than ~1.25:1 (a 4K display capture on a 1080p canvas, a big camera in a corner) are now area-averaged instead of bilinear-sampled, on both the GPU and CPU compositors, so fine lines and text no longer shimmer or vanish. 1:1 and enlarged layers are unchanged and cost nothing extra.
- **A GPU figure you can rely on**: the Performance Monitor's system GPU percentage now comes from the card's own utilisation counter (the one RTSS and the NVIDIA overlay read), with the Windows counters as a fallback. Windows' per-engine counters can read far too high, and they no longer drive the headline figure. CPU totals say whether they are "of all cores" or "of one core".
- **Window frame rate** (Settings → General): Ferrous's own window redraws at most 30, 60 (default), 120, 144 or 240 times a second, or unlimited. It never changes your video's frame rate; lower values use less GPU.
- **Startup stability**: the GPU compositor only considers Vulkan and DirectX 12 (the OpenGL probe could crash on some PCs), GPU filter shaders compile the first time a filter is used, and a native crash writes `native-crash.txt` (exception code and module) next to your settings.
- **Efficiency and robustness pass**: one persistent GPU buffer for layer parameters instead of a new buffer per layer per frame; the capture thread sleeps instead of spinning while the GPU finishes a copy; audio buffers are sized up front; event, remote-control and client queues are bounded so a stalled reader can't grow memory; a frame of the wrong size is skipped instead of panicking; a poisoned lock no longer stops transitions.
- **Security and robustness pass**: the remote-control server no longer lets a client skip the password with Reidentify; Ferrous Link survives malformed or hostile senders (empty/short messages, absurd picture sizes), encrypts each connection under its own key, and no longer lets a silent peer block new receivers; Stop no longer hangs when FFmpeg stops reading (it is killed after 15 s).
- **Debug pass**: damaged settings, scene and library files are repaired or kept as `.bad` instead of failing; 14 corrupt-profile launches (truncated, empty, binary, zero/huge fps, bad UI scale or layout) start cleanly; Settings tabs and the scene/source button rows now wrap in narrow windows so nothing is unreachable.
- **Recording presets and a speed/quality benchmark** (Settings → Recording).
- **HDR10 output** for HEVC and AV1 (SDR sources mapped to BT.2020/PQ, adjustable white level).
- **Encoder quality pass**: NVENC two-pass and lookahead, H.264 High profile, AV1 10-bit internals, Quick Sync
  adaptive I/B, AMF VBAQ, SVT-AV1 visual-quality tune.
- **New icon** (Fe 26 glass) in the exe, taskbar and top bar.
- Safe Game Capture, Source Clone, Track Matte transitions, OBS-style Add Source dialog, undo/redo,
  audio sync offset, per-scene transitions, projector and multiview windows.
- Renamed from Ember Studio to Ferrous (settings migrate automatically).
- Leaner build: trimmed eframe and env_logger default features.
