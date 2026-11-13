# Changelog

Newest first. Each entry lists what changed for users; the detail and test evidence live in
[`docs/PROGRESS.md`](docs/PROGRESS.md).

## v0.4 (unreleased)
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
