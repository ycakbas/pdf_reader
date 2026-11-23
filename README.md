# pdf_reader

An ultra-fast, cross-platform (macOS, Windows, Linux) PDF reader built in **Rust** with **PDFium** and **egui/eframe**.

## Features
- **Fast startup & lazy rendering:** Opens directly using PDFium with an LRU GPU page cache and adjacent-page pre-rendering.
- **Continuous vertical scrolling:** Smooth multi-page scrolling (toggle single-page mode with `c`).
- **Zoom to cursor & pinch zoom:** Smooth exponential wheel zoom and trackpad pinch zoom anchored at the mouse pointer.
- **GPU-accelerated PDF theming (WGSL shader):** Zero-cost instant theme switching (`light`, `dark`, `oled`, `sepia`, `gruvbox_dark`, `solarized_dark`, or custom `[themes.<name>]` in `config.toml`) without re-rendering pages.
- **Text search & highlights:** `Ctrl+F` or `/` to search across the entire document with case-sensitivity toggle and match highlights (`Enter`/`n`/`F3` next, `Shift+Enter`/`Shift+N`/`Shift+F3` prev).
- **TOC / Bookmarks sidebar:** Press `Tab` (or click `TOC`) to browse the PDF outline and jump to sections.
- **Configurable mouse & keyboard controls:** Remap pan button, wheel / Ctrl+wheel / Shift+wheel actions, double-click action, and all shortcuts in `config.toml`.
- **Drag-and-drop** PDF opening + native file picker.

## Setup

### 1. Install Rust
```bash
curl --proto '=https' --tlsv1.2 -sSf https://sh.rustup.rs | sh
```
Then reload your shell (`source "$HOME/.cargo/env"`).

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
```

## Configuration
On first run, a default config file is loaded (and can be saved) at:
- **Linux:** `~/.config/pdf_reader/config.toml`
- **macOS:** `~/Library/Application Support/pdf_reader/config.toml`
- **Windows:** `%APPDATA%\pdf_reader\config.toml`

Override with `--config /path/to/config.toml`.
