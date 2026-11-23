# pdf_reader

An ultra-fast, cross-platform (macOS, Windows, Linux) PDF reader built in **Rust** with **PDFium** and **egui/eframe**.

## Features (MVP)
- **Fast startup & lazy rendering:** Opens directly to the first page using PDFium, rendering only visible pages at exact HiDPI resolution.
- **PDF theming:** Built-in themes (`light`, `dark`, `sepia`, `gruvbox_dark`, `solarized_dark`, `oled`) plus custom `[themes.<name>]` definitions in config.
- **Configurable mouse controls:** Customize pan button, wheel / Ctrl+wheel / Shift+wheel actions (`scroll`, `zoom`, `page`), scroll speed, and zoom step.
- **Configurable keyboard shortcuts:** Remap navigation, zoom, fit modes, theming, and UI toggles in `config.toml`.
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
