# GPUI and gpui-component for a cross-platform photo editor

Research date: 2026-08-16. All claims cite primary sources (official docs, source code, first-party READMEs). Investigated for: a Linux/Windows/macOS desktop photo editor with a contact-sheet thumbnail grid (thousands of thumbnails), a single-image editor view with crop overlay, and forms for metadata templates.

---

## 1. GPUI (Zed's UI framework)

### 1.1 What it is, where it lives, license

- GPUI is "a hybrid immediate and retained mode, GPU accelerated, UI framework for Rust" from the creators of the Zed editor. It lives in the Zed monorepo at `crates/gpui` (there is **no** separate `zed-industries/gpui` repo — that URL 404s). Official site: <https://www.gpui.rs>. Source: [gpui README](https://github.com/zed-industries/zed/blob/main/crates/gpui/README.md), <https://www.gpui.rs>.
- **License: Apache-2.0** — verified in the crate manifest (`license = "Apache-2.0"`, `publish = true`). Source: [crates/gpui/Cargo.toml](https://github.com/zed-industries/zed/blob/main/crates/gpui/Cargo.toml).
- Zed Industries also publishes it on crates.io as [`gpui`](https://crates.io/crates/gpui) (latest **0.2.2**, published 2025-10-22; 0.1.0 was a 2022 placeholder, real publishing resumed 0.2.0 on 2025-10-09). Source: <https://crates.io/api/v1/crates/gpui/versions>.

### 1.2 Maturity

- **Pre-1.0 with explicit API-churn warning**: "GPUI is still in active development as we work on the Zed code editor, and is still pre-1.0. There will often be breaking changes between versions. You'll also need to use the latest version of stable Rust." Source: [gpui README](https://github.com/zed-industries/zed/blob/main/crates/gpui/README.md).
- The framework is "tied to Zed": "for the near future gpui is tied to Zed, so contributions will need to be made there and kept in sync with it." Source: <https://www.gpui.rs> (footer).
- Documentation is thin by design: "Currently, the best way to learn about these APIs is to read the Zed source code or drop a question in the Zed Discord." Source: [gpui README](https://github.com/zed-industries/zed/blob/main/crates/gpui/README.md). Available docs: README, crate-root docs ([gpui.rs source file](https://github.com/zed-industries/zed/blob/main/crates/gpui/src/gpui.rs)), [contexts.md](https://github.com/zed-industries/zed/blob/main/crates/gpui/docs/contexts.md), [key_dispatch.md](https://github.com/zed-industries/zed/blob/main/crates/gpui/docs/key_dispatch.md), plus ~20 runnable examples linked from <https://www.gpui.rs>.
- Development is extremely active (commits to `crates/gpui` within the last 1–2 days of this research). Source: [commit history for crates/gpui](https://github.com/zed-industries/zed/commits/main/crates/gpui).

### 1.3 Platform support (Linux / Windows / macOS)

All three desktop platforms are supported — Zed itself ships on all of them. Source: [Zed README](https://github.com/zed-industries/zed#readme) ("On macOS, Linux, and Windows you can download Zed directly…").

The platform stack was recently split into per-OS crates behind a facade crate `gpui_platform`. Per the [gpui README](https://github.com/zed-industries/zed/blob/main/crates/gpui/README.md) and [crates/gpui_platform/Cargo.toml](https://github.com/zed-industries/zed/blob/main/crates/gpui_platform/Cargo.toml):

| Platform | Windowing | Text | Renderer (GPU) |
|---|---|---|---|
| macOS | AppKit | Core Text (+ optional `font-kit`) | **Metal** (`metal`, `objc2-metal`; `gpui_apple` crate with `shaders.metal`) |
| Windows | **Win32** ("no features required") | **DirectWrite** | **Direct3D 11 + DirectComposition + DXGI** (`directx_renderer.rs`, `directx_devices.rs`, `shaders.hlsl` in `gpui_windows`) |
| Linux/FreeBSD | **Wayland and/or X11** (cargo features; xkbcommon + XIM input method on X11; xdg-desktop-portals via ashpd) | cosmic-text + swash (+ optional font-kit) | **wgpu 29 with `Backends::VULKAN | Backends::GL`**, WGSL shaders (`gpui_wgpu` crate) |

Sources: [gpui README platform notes](https://github.com/zed-industries/zed/blob/main/crates/gpui/README.md), [gpui_linux/Cargo.toml](https://github.com/zed-industries/zed/blob/main/crates/gpui_linux/Cargo.toml), [gpui_windows/Cargo.toml](https://github.com/zed-industries/zed/blob/main/crates/gpui_windows/Cargo.toml), [gpui_windows/src/directx_renderer.rs](https://github.com/zed-industries/zed/blob/main/crates/gpui_windows/src/directx_renderer.rs) (imports `Direct3D11`, `DirectComposition`, `DirectWrite`), [gpui_wgpu/src/wgpu_context.rs](https://github.com/zed-industries/zed/blob/main/crates/gpui_wgpu/src/wgpu_context.rs) (`backends: wgpu::Backends::VULKAN | wgpu::Backends::GL`, `PowerPreference::HighPerformance`).

Important historical note: **the blade renderer is gone from current main.** Older GPUI (including the published crates.io 0.2.2) used [blade-graphics](https://github.com/kvark/blade) on Linux/Windows; current `main` has no blade dependency in the workspace and uses wgpu (Linux/WASM) and a custom D3D11 renderer (Windows). Sources: [Zed workspace Cargo.toml](https://github.com/zed-industries/zed/blob/main/Cargo.toml) (wgpu = 29.0.4, no blade entry), [docs.rs gpui 0.2.2 deps](https://docs.rs/gpui/latest/gpui/struct.RenderImage.html) (still lists optional `blade-graphics`), [gpui_wgpu crate](https://github.com/zed-industries/zed/tree/main/crates/gpui_wgpu) (`shaders.wgsl`, `shaders_storage.wgsl`, `shaders_webgl.wgsl`).

There is also a WASM target (`gpui_web`, WebGPU/WebGL backends selected at runtime). Source: [gpui_wgpu/src/wgpu_context.rs](https://github.com/zed-industries/zed/blob/main/crates/gpui_wgpu/src/wgpu_context.rs) (`BROWSER_WEBGPU | GL`).

### 1.4 Setting up a project

From the [gpui README](https://github.com/zed-industries/zed/blob/main/crates/gpui/README.md) — GPUI is now crates.io-installable:

```toml
gpui = { version = "*" }
gpui_platform = { version = "*", features = ["font-kit", "wayland", "x11"] }
```

- Everything starts with `gpui_platform::application().run(|cx| ...)`, then `cx.open_window(WindowOptions { .. }, |window, cx| ..)`. Render trees are built by implementing `Render`. Source: [gpui README](https://github.com/zed-industries/zed/blob/main/crates/gpui/README.md), [hello_world example](https://github.com/zed-industries/zed/blob/main/crates/gpui/examples/hello_world.rs).
- Linux needs at least one of the `wayland` / `x11` features on `gpui_platform`; Windows needs no features; macOS needs `font-kit` for real glyphs. Source: [gpui README](https://github.com/zed-industries/zed/blob/main/crates/gpui/README.md).
- macOS build needs Xcode + command-line tools (for Metal). Source: [gpui README](https://github.com/zed-industries/zed/blob/main/crates/gpui/README.md).

### 1.5 Rendering model

- Hybrid immediate/retained: each frame GPUI calls `render()` on the root view, producing a tree of *elements*; elements are laid out with [Taffy](https://github.com/DioxusLabs/taffy) (pinned `=0.13.0`) with a Tailwind-style styling API (`div().flex().gap_3()...`), then rasterized on the GPU. A low-level imperative `Element` API exists for custom rendering (used for "efficient views into large lists, custom layouting for a code editor"). Source: [gpui README](https://github.com/zed-industries/zed/blob/main/crates/gpui/README.md), [Cargo.toml](https://github.com/zed-industries/zed/blob/main/crates/gpui/Cargo.toml).
- The scene graph is a fixed set of GPU primitives: `Quad`, `Underline`, `Shadow`, `MonochromeSprite`, `SubpixelSprite`, `PolychromeSprite`, `PaintSurface`, `Path` (tessellated via [lyon](https://github.com/nical/lyon)). Glyphs/images go through texture atlases ([etagere](https://github.com/nical/etagere) allocator; `metal_atlas.rs`, `directx_atlas.rs`, `wgpu_atlas.rs`). Sources: [scene.rs](https://github.com/zed-industries/zed/blob/main/crates/gpui/src/scene.rs), [Cargo.toml](https://github.com/zed-industries/zed/blob/main/crates/gpui/Cargo.toml), [gpui_apple/src/metal_atlas.rs](https://github.com/zed-industries/zed/blob/main/crates/gpui_apple/src/metal_atlas.rs).
- 4x MSAA is used for path rendering on Metal and D3D11. Sources: [metal_renderer.rs](https://github.com/zed-industries/zed/blob/main/crates/gpui_apple/src/metal_renderer.rs) ("Use 4x MSAA, all devices support it"), [directx_renderer.rs](https://github.com/zed-industries/zed/blob/main/crates/gpui_windows/src/directx_renderer.rs) ("guaranteed to be supported by DirectX 11").

### 1.6 How images/textures are drawn

This is the core question for a photo app. Findings:

- **`img()` element**: `img(source)` accepts an `ImageSource`, with sizing, `ObjectFit` (contain/cover/…), auto width/height. Sources: [elements/img.rs](https://github.com/zed-industries/zed/blob/main/crates/gpui/src/elements/img.rs), [image example](https://github.com/zed-industries/zed/blob/main/crates/gpui/examples/image/image.rs).
- **`ImageSource` variants** ([img.rs](https://github.com/zed-industries/zed/blob/main/crates/gpui/src/elements/img.rs)):
  - `Resource(Resource)` — a URI, file path, or embedded asset; the example loads a local JPEG **with EXIF orientation applied**, a remote `https://` image (via the app's HTTP client), and an SVG asset. Source: [image example](https://github.com/zed-industries/zed/blob/main/crates/gpui/examples/image/image.rs).
  - `Render(Arc<RenderImage>)` — an app-provided, already-decoded image.
  - `Image(Arc<Image>)` — cached image data.
  - `Custom(...)` — a closure `(Window, App) -> Option<Result<Arc<RenderImage>, ImageCacheError>>`, i.e. **a fully custom async-ish loader hook** (used for thumbnails generated on the fly).
- **`RenderImage`** is "a cached and processed image, **in BGRA format**", constructed from `image::animation::Frame`s (the [`image`](https://crates.io/crates/image) crate), supports multiple frames + per-frame delay (GIF animation — see the [gif_viewer example](https://github.com/zed-industries/zed/blob/main/crates/gpui/examples/gif_viewer.rs)). Sources: [docs.rs RenderImage](https://docs.rs/gpui/latest/gpui/struct.RenderImage.html) (source: `assets.rs`), [img.rs](https://github.com/zed-industries/zed/blob/main/crates/gpui/src/elements/img.rs) (GifDecoder/WebPDecoder imports).
- There is a built-in **image cache with LRU behavior and loading states** (`ImageCache`, `AnyImageCache`, `ImageCacheError`, 200 ms loading-delay indicator). Sources: [img.rs](https://github.com/zed-industries/zed/blob/main/crates/gpui/src/elements/img.rs), [elements/image_cache.rs](https://github.com/zed-industries/zed/blob/main/crates/gpui/src/elements/image_cache.rs).
- **Imperative painting**: `Window::paint_image(bounds, ..., data: Arc<RenderImage>, ...)` paints pixel data directly; sibling methods: `paint_quad`, `paint_path` (gradients/patterns), `paint_underline`, `paint_strikethrough`, `paint_glyph`, `paint_emoji`, `paint_svg`, `paint_surface`, `paint_drop_shadows`/`paint_inset_shadows`, `paint_layer`. Source: [window.rs](https://github.com/zed-industries/zed/blob/main/crates/gpui/src/window.rs) (grep `pub fn paint_`).
- **Reference implementation**: Zed itself ships an image viewer built on GPUI — `crates/image_viewer` — which holds an `Arc<RenderImage>`, renders `img(image)`, and handles pinch-zoom/scroll (uses `PinchEvent`). This is essentially a minimal single-image editor view. Source: [crates/image_viewer/src/image_viewer.rs](https://github.com/zed-industries/zed/blob/main/crates/image_viewer/src/image_viewer.rs).

### 1.7 Custom painting and shaders

- **Custom painting: yes.** Two levels: (a) the `canvas(prepaint, paint)` element — "meant for accessing the low level paint API without defining a whole custom element" ([canvas.rs](https://github.com/zed-industries/zed/blob/main/crates/gpui/src/elements/canvas.rs)); (b) implementing the `Element` trait directly with full control of `request_layout`/`prepaint`/`paint` ([README](https://github.com/zed-industries/zed/blob/main/crates/gpui/README.md)). A crop overlay (rect + rules-of-thirds + handles) is straightforward with `paint_path`/`paint_quad` in a canvas or custom element; gradient and pattern fills exist (`linear_gradient`, `pattern_slash` — see the [pattern example](https://github.com/zed-industries/zed/blob/main/crates/gpui/examples/pattern.rs)).
- **Custom shaders: no public API.** The shader set is fixed per backend (`shaders.metal`, `shaders.hlsl`, `shaders*.wgsl`); the `Scene` primitive enum is closed. The `runtime_shaders` cargo feature (macOS only, enabled by gpui-component) merely compiles the framework's own Metal shader source at runtime instead of embedding a precompiled `.metallib` — it is a development convenience, not an app-facing shader hook. Sources: [gpui_platform/Cargo.toml](https://github.com/zed-industries/zed/blob/main/crates/gpui_platform/Cargo.toml), [metal_renderer.rs](https://github.com/zed-industries/zed/blob/main/crates/gpui_apple/src/metal_renderer.rs) (`#[cfg(feature = "runtime_shaders")] ... new_library_with_source`).
- **GPU-surface interop**: a `surface()` element + `Window::paint_surface` exists for displaying external GPU-produced frames, but `SurfaceSource` currently only accepts a macOS `CVPixelBuffer` (CoreVideo) — i.e. **macOS-only**. Sources: [elements/surface.rs](https://github.com/zed-industries/zed/blob/main/crates/gpui/src/elements/surface.rs), [window.rs](https://github.com/zed-industries/zed/blob/main/crates/gpui/src/window.rs#L4491).
- Consequence for a photo editor: GPU filters/lens corrections cannot be expressed as app-provided shaders; heavy pixel ops must run CPU-side (or via an external GPU pipeline, which currently has no import path except macOS surfaces). Displaying the result is easy (`img()` with a fresh `RenderImage`).

### 1.8 Input / keyboard handling

- GPUI is "designed for keyboard-first interactivity": app-defined **actions** (unit or payload structs via `actions!`/`#[gpui::action]`), **key contexts** on element subtrees, and **JSON keymaps** binding keystrokes to actions per context (exactly how Zed does configurable shortcuts). Source: [key_dispatch.md](https://github.com/zed-industries/zed/blob/main/crates/gpui/docs/key_dispatch.md).
- Mouse: click/drag/scroll listeners on elements (`InteractiveElement`), plus scroll-wheel, mouse-pressure ([mouse_pressure example](https://github.com/zed-industries/zed/blob/main/crates/gpui/examples/mouse_pressure.rs)), pinch/gesture support (`gestures.rs`, `PinchEvent` used by Zed's image viewer). Sources: [gpui.rs examples list](https://www.gpui.rs), [image_viewer.rs](https://github.com/zed-industries/zed/blob/main/crates/image_viewer/src/image_viewer.rs).
- Text input: basic `input.rs` example in gpui; serious editors (Zed's editor, gpui-component's `Input`/`Textarea`/code `Editor`) are built on top. IME on X11 via zed's `xim` fork; Wayland text-input protocols; Windows/Linux/macOS each have platform text stacks (DirectWrite / cosmic-text / Core Text). Sources: [input example](https://github.com/zed-industries/zed/blob/main/crates/gpui/examples/input.rs), [gpui_linux/Cargo.toml](https://github.com/zed-industries/zed/blob/main/crates/gpui_linux/Cargo.toml).
- Accessibility via [AccessKit](https://accesskit.dev/) integration (`accesskit` dep, `a11y` example). Source: [Cargo.toml](https://github.com/zed-industries/zed/blob/main/crates/gpui/Cargo.toml), [a11y example](https://github.com/zed-industries/zed/blob/main/crates/gpui/examples/a11y.rs).
- A `#[gpui::test]` macro + `TestAppContext` provides simulation of common platform input for tests. Source: [gpui README](https://github.com/zed-industries/zed/blob/main/crates/gpui/README.md).

### 1.9 Virtual list performance (contact-sheet grid)

- **`uniform_list`** — "provides lazy rendering for a set of items that are of uniform height… will only render the visible subset of items" and skips the full Taffy layout system ("measures the first element and then lays out all remaining elements in a line based on that measurement. This is much faster"). Source: [uniform_list.rs](https://github.com/zed-industries/zed/blob/main/crates/gpui/src/elements/uniform_list.rs), [uniform_list example](https://github.com/zed-industries/zed/blob/main/crates/gpui/examples/uniform_list.rs).
- **`list`** — the general virtualized list for *variable*-height items (used for Zed's editor; 111 KB implementation). Source: [elements/list.rs](https://github.com/zed-industries/zed/blob/main/crates/gpui/src/elements/list.rs).
- There is **no built-in virtualized 2-D grid**; a contact sheet is built by making each `uniform_list` row render N thumbnails (row chunking — each row is uniform height), or by writing a custom `Element`. Zed's own UI does this pattern with lists; gpui-component's `VirtualList`/`Scrollable`/`DataTable` extend it. Sources: [uniform_list.rs](https://github.com/zed-industries/zed/blob/main/crates/gpui/src/elements/uniform_list.rs), [gpui-component virtual_list.rs](https://github.com/longbridge/gpui-component/blob/main/crates/ui/src/virtual_list.rs).
- Zed renders 200K+ line files through gpui lists at 120 fps; gpui-component markets "120 FPS rendering… dense interfaces stay smooth" and a table that handles "hundreds of thousands of rows". Sources: <https://longbridge.github.io/gpui-component/>, [gpui-component README](https://github.com/longbridge/gpui-component#features).

---

## 2. gpui-component (Longbridge)

### 2.1 Overview, license, maintenance

- "Rust GUI components for building fantastic cross-platform desktop application by using GPUI" — **60+ components**, shadcn/ui-inspired, with themes, sizes, i18n, Markdown/HTML rendering, charts, a code editor with LSP, dock layout, and "High Performance: Virtualized Table and List components". Source: [README](https://github.com/longbridge/gpui-component#features).
- **License: Apache-2.0** (`LICENSE-APACHE` file; README license section; Cargo.toml `license = "Apache-2.0"`). Sources: [repo files](https://github.com/longbridge/gpui-component), [crates/ui/Cargo.toml](https://github.com/longbridge/gpui-component/blob/main/crates/ui/Cargo.toml).
- **Maintenance: very active.** ~12.8k stars, 778 forks, 2,063 commits, 86 open issues; commits landed on the day of this research (2026-08-16). Published on crates.io as [`gpui-component`](https://crates.io/crates/gpui-component) — latest release **0.5.1** (2026-02-05), repo workspace currently at 0.5.2; release cadence 0.3.0 (2025-10-24) → 0.4.x (Nov 2025) → 0.5.0 (2025-12-08) → 0.5.1. Sources: [repo page](https://github.com/longbridge/gpui-component), <https://crates.io/api/v1/crates/gpui-component/versions>, [releases](https://github.com/longbridge/gpui-component/releases).
- Backed by Longbridge (a fintech), who build their production desktop app with it (see §3).

### 2.2 Component inventory

Full catalog (from the component sources at [crates/ui/src](https://github.com/longbridge/gpui-component/tree/main/crates/ui/src) and the docs index <https://longbridge.github.io/gpui-component/llms.txt>):

- **Layout/app shell**: `Root` (required window root), `TitleBar`, `Sidebar`, `StatusBar`, dock system (`DockArea`, `TabPanel`, resizable/tiling layouts, serializable), `Tabs`, `Resizable` panel groups, `Popover`/anchored overlays, `Scrollable`/`Scrollbar`.
- **Data display**: `Table` + `DataTable` ("virtual scrolling, sorting, filtering, and column management"; "fixed and resizable columns… cell selection across hundreds of thousands of rows"), `List` (sections, search, selection, infinite scroll), `VirtualList`, `DescriptionList`, `Tree`, `Breadcrumb`, `Avatar`, `Badge`, `Tag`, `Image` (loading states + fallbacks), `Skeleton`, `Progress`, `Spinner`, `Chart`/`Plot` (line, bar, area, pie, radar, candlestick, sankey), `Rating`, `Stepper`.
- **Forms** (relevant for metadata templates): **`Form`** — "flexible form container with support for field layout, validation, and multi-column layouts" (+ `Form::Field`), `Input` (validation, masking), `Textarea`, `NumberInput`, `Select`, `Combobox` (autocomplete), `Checkbox`, `Radio`/`RadioGroup`, `Switch`, `Slider`, `ColorPicker`, `DatePicker`/`Calendar`/`TimePicker`, `DropdownButton`, `FocusTrap`.
- **Overlays/feedback**: `Dialog` (modal), `AlertDialog`, `Sheet` (edge panel), `Menu` (context/popup menus, submenus), `NativeMenu` (AppKit NSMenu / Win32 menus), `Notification`/`Toast`, `Tooltip`, `HoverCard`, `Alert`, `Clipboard`.
- **Text/content**: `Label`, `Kbd`, `Link`, `TextView` (Markdown + HTML rendering), code `Editor` (rope-backed, "up to 200K lines", Tree-sitter highlighting, LSP diagnostics/completion/hover), `TextView` selection support.
- Two-layer architecture: styled `gpui-component` on top of a new unstyled behavior layer **`gpui-base`** ("focus, overlay positioning, virtualization and accessibility" — for teams wanting to own the visual design). Sources: [README](https://github.com/longbridge/gpui-component#ecosystem-architecture), <https://longbridge.github.io/gpui-component/base/>.

### 2.3 Version compatibility with gpui

- gpui-component **tracks GPUI via git, not crates.io**: workspace declares `gpui = { version = "0.2.2", git = "https://github.com/zed-industries/zed" }` plus `gpui_platform = { git = …, features = ["font-kit", "x11", "wayland", "runtime_shaders"] }`. The README's "Usage" likewise says `gpui = { git = "https://github.com/zed-industries/zed" }`. Sources: [workspace Cargo.toml](https://github.com/longbridge/gpui-component/blob/main/Cargo.toml), [README](https://github.com/longbridge/gpui-component#usage).
- Consequence: the crates.io `gpui` 0.2.2 (Oct 2025) lags Zed `main` (which has since split platform crates and swapped blade → wgpu/D3D11). Apps should consume both via git revisions that are known to work together (i.e., let gpui-component's pin drive yours), and expect periodic breakage when bumping. Sources: [gpui versions on crates.io](https://crates.io/api/v1/crates/gpui/versions), [Zed workspace Cargo.toml](https://github.com/zed-industries/zed/blob/main/Cargo.toml).

### 2.4 Examples & storybook

- `cargo run` launches a desktop **gallery/storybook** (`story` crate) showcasing all components; highlighted examples: `editor` (code editor + LSP), `dock`, `markdown`, `html`; standalone example crates: `hello_world`, `system_monitor` (real-time charts), `window_title`. A **WASM web gallery** also runs the components in a browser. Source: [README development section](https://github.com/longbridge/gpui-component#development).
- Docs website with per-component pages and llms.txt: <https://longbridge.github.io/gpui-component/>.

---

## 3. Production apps built with GPUI / gpui-component

1. **Zed** — the flagship GPUI app; a shipping commercial-quality editor on macOS, Linux, and Windows (plus collab features). Sources: <https://www.gpui.rs> ("Today, it's Zed's UI framework"), [Zed README](https://github.com/zed-industries/zed#readme).
2. **Longbridge Pro** (desktop trading app) — "the first application built using GPUI Component." Source: [gpui-component README showcase](https://github.com/longbridge/gpui-component#showcase) (links <https://longbridge.com/desktop>).
3. Beyond those, the ecosystem is young; gpui-component's own gallery/story apps demonstrate the rest. No first-party evidence of other large production apps was found in the sources surveyed.

---

## 4. Risks

- **API churn (high)**: pre-1.0, "there will often be breaking changes between versions"; and a real, ongoing mid-flight refactor (platform crates split, blade → wgpu/D3D11) means crates.io 0.2.2 ≠ current main. Pin git revisions; budget for periodic migration. Sources: [gpui README](https://github.com/zed-industries/zed/blob/main/crates/gpui/README.md), [crates.io gpui versions](https://crates.io/crates/gpui).
- **Docs gap (medium)**: official guidance is "read the Zed source code or ask in Discord"; gpui has only a README + 2 explainer docs + examples. gpui-component's docs site is much better but still "Simple" by its own comparison table. Sources: [gpui README](https://github.com/zed-industries/zed/blob/main/crates/gpui/README.md), [gpui-component README comparison](https://github.com/longbridge/gpui-component#compare-to-others).
- **Upstream coupling (medium)**: gpui evolves for Zed's needs ("for the near future gpui is tied to Zed"); your app is a passenger. Source: <https://www.gpui.rs>.
- **Licensing (low)**: both GPUI and gpui-component are Apache-2.0 — commercial-friendly, no LGPL/Qt-style concerns. Sources: [gpui Cargo.toml](https://github.com/zed-industries/zed/blob/main/crates/gpui/Cargo.toml), [gpui-component Cargo.toml](https://github.com/longbridge/gpui-component/blob/main/crates/ui/Cargo.toml).
- **Renderer requirements (low–medium)**: macOS needs Metal (all Macs since 2015); Windows needs D3D11 + DirectComposition (extremely broad; an escape hatch `GPUI_DISABLE_DIRECT_COMPOSITION` exists for odd setups/VMs/RDP); Linux needs Vulkan or GL via wgpu plus Wayland and/or X11 — Linux GPU driver variance is the largest support surface, and the new wgpu renderer is younger than the old blade path (NVIDIA-on-Wayland edge cases are special-cased in code). Sources: [README](https://github.com/zed-industries/zed/blob/main/crates/gpui/README.md), [directx_renderer.rs](https://github.com/zed-industries/zed/blob/main/crates/gpui_windows/src/directx_renderer.rs), [wgpu_context.rs](https://github.com/zed-industries/zed/blob/main/crates/gpui_wgpu/src/wgpu_context.rs).
- **No app-facing custom shaders (high for photo editing)**: filters/adjustments can't run as GPUI shaders; the scene primitive set is closed and GPU import (`paint_surface`) is macOS-only today. Pixel processing must be CPU-side (rayon/image crate) or a separate GPU stack with no display interop. Sources: [scene.rs](https://github.com/zed-industries/zed/blob/main/crates/gpui/src/scene.rs), [surface.rs](https://github.com/zed-industries/zed/blob/main/crates/gpui/src/elements/surface.rs).
- **No virtualized grid primitive (low)**: contact sheet = `uniform_list` of chunked rows or a custom element; well-trodden (Zed, DataTable do similar) but bespoke. Source: [uniform_list.rs](https://github.com/zed-industries/zed/blob/main/crates/gpui/src/elements/uniform_list.rs).
- **Bus factor/second-source (low–medium)**: gpui-component is effectively one company's project (very active, 12.8k stars, Apache-2.0, forkable; the new `gpui-base` layer reduces lock-in).

---

## Verdict

**Suitable, with eyes open.** GPUI is a genuinely fast, GPU-accelerated, cross-platform (Metal / D3D11 / wgpu-Vulkan) Rust UI framework that is production-proven by Zed on all three target OSes, and gpui-component supplies every widget the app shell needs — virtualized `DataTable`/`List`, dock layout, modal `Dialog`/`Sheet`, dropdowns/`Select`/`Combobox`, and a validated multi-column `Form` that maps directly onto metadata-template editing.

For the photo-editor specifics:

- **Thumbnail grid (thousands of thumbnails)**: good fit. `uniform_list` renders only visible items and bypasses the heavy layout engine; `RenderImage` (BGRA `image` frames) plus the `img()` cache or a `Custom` loader hook is exactly the shape of a thumbnail pipeline, and Zed's `image_viewer` crate is a working reference for the single-image view (including pinch/zoom/pan).
- **Single-image editor view with crop overlay**: good fit. `canvas()` or a custom `Element` with `paint_image` + `paint_path`/`paint_quad` (gradients, shadows, 4x MSAA paths) covers crop rectangles, thirds guides, and drag handles; mouse/pinch/scroll handling is first-class.
- **Forms for metadata templates**: solved off-the-shelf by gpui-component's `Form` + inputs.
- **Main caveats**: no app-defined GPU shaders and no cross-platform GPU-texture interop, so adjustment/filtering must be CPU-side (or an out-of-band GPU pipeline) — acceptable for organize/crop/metadata workflows, a real constraint if you want realtime GPU filters; pre-1.0 churn means pinning git revisions and periodic migration; sparse GPUI docs mean learning from Zed's source; and the Linux wgpu renderer stack is the youngest of the three backends. If those trade-offs are acceptable, GPUI + gpui-component is a strong, modern, Apache-2.0 choice; the closest risk-adjusted alternatives (egui, iced) trade away either polish or the component ecosystem.
