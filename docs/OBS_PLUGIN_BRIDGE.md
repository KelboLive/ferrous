# Design: running OBS plugins in Ferrous

Status: **built for sources** (`crates/ferrous-obs`, `tools/obs-host`). It has been tested against
the real libobs 30 on Linux, and type-checked but not run for Windows (`obs.dll`). Two things
differ from the plan below:
- Video is read back from a transparent render target in libobs's render callback, so alpha
  survives. The plan was the raw video callback, which loses alpha.
- UI-only plugins are skipped automatically.

Plugin *filters* (step 3) are not built yet. See `docs/PROGRESS.md`.

## The problem

OBS plugins are DLLs that link against `obs.dll` (libobs) and call its C API. That API has about
1,000 functions, including OBS's graphics subsystem (`gs_*`, HLSL-like `.effect` shaders). Plugins
register types with `obs_register_source(struct obs_source_info*)`, and libobs then calls back
into them for every frame. Plugins that add UI (docks, menus) also use Qt and the
`obs-frontend-api`, which Ferrous doesn't have.

## Options considered

| Approach | Compatibility | Cost | Verdict |
|---|---|---|---|
| Re-implement libobs (a fake `obs.dll`) | Low: anything using `gs_*` effects breaks | Very high, crash-prone | Rejected |
| **Host real libobs** in a helper process | High for sources, filters and audio | Moderate | **Chosen** |
| Host libobs in-process | High | Moderate, but a plugin crash takes Ferrous down | Rejected |

## Chosen design: `ferrous-obs-host.exe`

A small helper process ships next to Ferrous (or uses the user's OBS install when allowed by its
GPL license). It loads the real `obs.dll`, initialises libobs headless, loads
`obs-plugins\64bit\*.dll`, and serves Ferrous over a local pipe.

1. **Startup.** It calls `obs_startup`, then `obs_reset_video` (the canvas = the requested plugin
   source size, `GS_DEVICE_DIRECT3D_11`), `obs_reset_audio` (48 kHz stereo), then
   `obs_add_module_path` and `obs_load_all_modules`. Afterwards it lists source types with
   `obs_enum_input_types` and filters with `obs_enum_filter_types`, plus their display names.
2. **OBS Plugin Source** (new Ferrous source type). The host creates a private scene containing
   `obs_source_create(type_id, name, settings, NULL)`, makes that scene the output on channel 0,
   and receives finished frames through `obs_add_raw_video_callback` in BGRA. Frames are copied into
   a shared-memory ring (one per source) and picked up by Ferrous's existing `Source` trait. Audio
   comes from `obs_add_raw_audio_callback` into an Ferrous mixer channel.
3. **OBS Plugin Filter.** For a filter, the host creates an `obs_source` of a small "frame
   injector" type (registered by the host itself) that feeds Ferrous's frame in. It attaches the
   plugin filter with `obs_source_filter_add` and reads the filtered output the same way. This adds
   one frame of latency.
4. **Settings.** Plugins describe their settings with `obs_properties_t`. The host walks them
   (`obs_properties_first` / `obs_property_next`, type, name, description, limits, list items) and
   sends a JSON schema. Ferrous renders it with egui (number sliders, text, lists, colours, paths),
   so plugin settings get a native-looking editor without Qt.
5. **Isolation.** If the host crashes, Ferrous shows the source as offline and restarts the host,
   the same way dead capture devices are handled now.

## What will and won't work

- **Will work:** most input sources and video/audio filters, including shader-based ones (they
  run on libobs's own D3D11 device). Examples: Move Transition's filters, StreamFX-style effects,
  NVIDIA/background-removal filters, VST (via OBS's own VST filter), Advanced Masks.
- **Won't work:** plugins whose value is in OBS's Qt UI (docks, custom dialogs, the
  `obs-frontend-api` scene-switching automation), and outputs/services that replace OBS's streaming
  pipeline. For automation, Ferrous's obs-websocket server is the path. Tools like Advanced Scene
  Switcher's external equivalents and Streamer.bot drive Ferrous through it.

## Risks

- libobs structs (`obs_video_info`, `video_data`) are ABI-sensitive across OBS versions, so the
  host must be built against the matching OBS SDK headers. Each OBS major version needs its own
  host build.
- Licensing: libobs is GPL-2.0 and Ferrous is GPL-2.0-or-later, so bundling is compatible.
- GPU sharing: the host renders on its own D3D11 device, and frames cross processes through shared
  memory (copy). Later, a shared D3D11 texture handle could make this zero-copy.
