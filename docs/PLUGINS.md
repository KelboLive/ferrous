# Writing Ferrous plugins

A plugin is a DLL that exports one function, `ferrous_plugin_module`, returning a static table of
plugins. A DLL can provide any number of plugins of three kinds: **video sources**, **video
filters** and **audio filters**. Put plugin DLLs in `%APPDATA%\Ferrous\plugins` or in a
`plugins` folder next to `ferrous.exe`. They're loaded at startup and appear in the
**Add Source → Plugins** menu and in every **Add filter** menu.

A complete working example (Rust) is in `plugins/example`. It contains a gradient source, an invert
filter and a gain filter. In Rust, depend on the `ferrous-plugin-api` crate. In C or C++, use this
header:

```c
// ferrous_plugin.h: Ferrous plugin ABI v1
#include <stdint.h>

#define FERROUS_PLUGIN_ABI_VERSION 1
#define FERROUS_KIND_VIDEO_SOURCE 1
#define FERROUS_KIND_VIDEO_FILTER 2
#define FERROUS_KIND_AUDIO_FILTER 3

typedef struct {
    uint32_t width, height;
    uint32_t stride;   /* bytes per row */
    uint8_t *data;     /* BGRA, straight alpha */
} FerrousFrame;

typedef struct {
    const char *id;    /* unique, e.g. "acme.blur" */
    const char *name;  /* shown in menus */
    uint32_t kind;
    void *(*create)(const char *settings_json);
    void  (*destroy)(void *instance);
    void  (*video_size)(void *instance, uint32_t *w, uint32_t *h);         /* sources */
    void  (*video_render)(void *instance, FerrousFrame *frame, uint64_t ns); /* sources, video filters */
    void  (*audio_process)(void *instance, float *stereo, uint32_t frames); /* audio filters, 48 kHz */
} FerrousPluginDesc;

typedef struct {
    uint32_t abi_version;
    uint32_t count;
    const FerrousPluginDesc *plugins;
} FerrousPluginModule;

__declspec(dllexport) const FerrousPluginModule *ferrous_plugin_module(void);
```

## Contract

- **Settings** are a JSON object string. The user edits them in the source or filter properties.
  When they change, the old instance is destroyed and a new one is created.
- **Threading:** an instance is used from one thread at a time. Different instances may run in
  parallel.
- **Video sources:** `video_size` is called before every `video_render`, so the size may change
  from frame to frame (maximum 8192×8192).
- **Video filters:** modify the frame in place. They receive the source's frame before compositing.
- **Audio filters:** process interleaved stereo float at 48 kHz in place, usually 480 frames
  (10 ms) per call.
- **Never let a panic or C++ exception escape** a callback.
- The ABI version is checked on load. Incompatible plugins are skipped with a message in the app.
