# pdf_reader

An ultra-fast, cross-platform (**macOS, Windows, Linux**) PDF reader built in **Rust** with **PDFium** and **egui/eframe** (`wgpu`).

## Features
- **Ultra-fast startup & async rendering:** Opens documents in ~1–2ms using PDFium's lightweight page-size API, with a dedicated background render worker and LRU GPU page cache so scrolling and zooming stay smooth at 60/120fps.
- **GPU-accelerated PDF theming (WGSL shader):** Instant 0ms theme switching (`light`, `dark`, `oled`, `sepia`, `gruvbox_dark`, `solarized_dark`, or custom `[themes.<name>]` in `config.toml`) without re-rendering pages.
- **Multi-file tabs:** Open multiple PDFs via CLI, `Ctrl+O`, drag-and-drop, or `+` on the tab bar. Per-tab reading state and configurable tab behavior (`Settings → Tabs`).
- **Continuous & Single-Page modes:** Smooth custom scroll integrator (with configurable scroll speed and acceleration) in continuous mode, or centered single-page mode (`c`) with exact 1-tick page flipping and edge-scroll transitions.
- **Zoom & Resolution controls:** Smooth zoom toward cursor, trackpad pinch zoom, configurable render scale (`1.0x–3.0x`), max resolution, and LCD subpixel text rendering (`Settings → General`).
- **Text & Image selection:** Drag-select text, double-click for word, triple-click for line (`Ctrl+C` to copy); click/right-click embedded images to **Copy Image** or **Export Image** (PNG, JPEG, WebP, BMP).
- **Clickable links & navigation history:** External URLs open in your default browser; internal PDF links jump directly with Back/Forward history (`Alt+Left` / `Alt+Right` or Mouse Back/Forward buttons).
- **Fast navigation:** Overlay vertical & horizontal scrollbars, editable toolbar page box, `Ctrl+G` Go-to-Page popup (supports page numbers or `%`), virtualized **Pages** thumbnail sidebar (`p`), **TOC** outline sidebar (`Tab`), and full-document text search (`Ctrl+F` / `/`).
- **Configurable controls:** Settings popup (`Ctrl+,`) to customize mouse buttons (Left/Middle/Right drag, Mouse Back/Forward, Wheel/Ctrl+Wheel/Shift+Wheel, double-click), general/resolution options, and tab behavior, persisted to `config.toml`.

## Setup

### 1. Install Rust
```bash
curl --proto '=https' --tlsv1.2 -sSf https://sh.rustup.rs | sh
source "$HOME/.cargo/env"
```

### 2. Fetch PDFium binary
Run the helper script (Linux / macOS) to download the prebuilt library from [bblanchon/pdfium-binaries](https://github.com/bblanchon/pdfium-binaries) into `./lib/`:
```bash
./scripts/fetch_pdfium.sh
```
On Windows, download `pdfium-win-x64.tgz` from `bblanchon/pdfium-binaries` releases and place `pdfium.dll` next to the executable or in `./lib/`.

You can also point to a custom library directory via `PDFIUM_LIB_DIR=/path/to/dir`.

### 3. Build & Run
```bash
cargo run --release -- path/to/file.pdf
# Or open multiple files in tabs:
cargo run --release -- a.pdf b.pdf
```

## Configuration
A `config.toml` file is automatically created and saved at:
- **Linux:** `~/.config/pdf_reader/config.toml`
- **macOS:** `~/Library/Application Support/pdf_reader/config.toml`
- **Windows:** `%APPDATA%\pdf_reader\config.toml`

Override with `--config /path/to/config.toml`.

## Credits & Third-Party Licenses

This project is built on top of the following open-source projects:

| Project | Purpose | License |
| :--- | :--- | :--- |
| [PDFium](https://pdfium.googlesource.com/pdfium/) (Google / Chromium) | PDF parsing, text/link/image extraction & rendering engine | Apache-2.0 / BSD-3-Clause |
| [pdfium-binaries](https://github.com/bblanchon/pdfium-binaries) (Benoit Blanchon) | Prebuilt cross-platform PDFium binaries | Apache-2.0 |
| [pdfium-render](https://github.com/ajrcarey/pdfium-render) | Rust bindings to PDFium | MIT OR Apache-2.0 |
| [egui & eframe](https://github.com/emilk/egui) | Cross-platform immediate-mode GUI framework | MIT OR Apache-2.0 |
| [wgpu](https://github.com/gfx-rs/wgpu) | Cross-platform GPU rendering (Metal / DX12 / Vulkan) & WGSL shaders | MIT OR Apache-2.0 |
| [clap](https://github.com/clap-rs/clap) | Command-line argument parser | MIT OR Apache-2.0 |
| [serde](https://github.com/serde-rs/serde) & [toml](https://github.com/toml-rs/toml) | Configuration serialization/deserialization | MIT OR Apache-2.0 |
| [directories](https://github.com/soc/directories-rs) | Platform-standard config directory resolution | MIT OR Apache-2.0 |
| [rfd](https://github.com/PolyMeilex/rfd) | Native file open/save dialogs | MIT |
| [anyhow](https://github.com/dtolnay/anyhow) | Error handling | MIT OR Apache-2.0 |

*(Note: If you distribute prebuilt binaries bundled with `libpdfium`, include PDFium's `LICENSE` file alongside the distributed binary.)*

## License

Dual-licensed under either **MIT** ([LICENSE-MIT](LICENSE-MIT)) or **Apache-2.0**, at your option.
