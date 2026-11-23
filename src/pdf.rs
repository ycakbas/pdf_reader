use std::env;
use std::path::{Path, PathBuf};
use std::sync::OnceLock;

use anyhow::{Context, Result};
use pdfium_render::prelude::*;

static PDFIUM: OnceLock<Pdfium> = OnceLock::new();

pub struct RenderedPage {
    pub width: usize,
    pub height: usize,
    pub rgba: Vec<u8>,
}

/// Locate and bind to `libpdfium` once per process using a cross-platform search order:
/// 1. `PDFIUM_LIB_DIR` environment variable
/// 2. Directory containing the current executable (and `<exe_dir>/lib`)
/// 3. `./lib`
/// 4. `./`
/// 5. System library search path
pub fn init_pdfium() -> Result<&'static Pdfium> {
    if let Some(pdfium) = PDFIUM.get() {
        return Ok(pdfium);
    }

    let mut search_dirs: Vec<PathBuf> = Vec::new();

    if let Ok(dir) = env::var("PDFIUM_LIB_DIR") {
        search_dirs.push(PathBuf::from(dir));
    }
    if let Ok(exe) = env::current_exe()
        && let Some(parent) = exe.parent()
    {
        search_dirs.push(parent.to_path_buf());
        search_dirs.push(parent.join("lib"));
    }
    search_dirs.push(PathBuf::from("./lib"));
    search_dirs.push(PathBuf::from("."));

    let mut bindings = None;
    for dir in &search_dirs {
        let candidate = Pdfium::pdfium_platform_library_name_at_path(dir);
        if let Ok(b) = Pdfium::bind_to_library(&candidate) {
            bindings = Some(b);
            break;
        }
    }

    let bindings = match bindings {
        Some(b) => b,
        None => Pdfium::bind_to_system_library().context(
            "Could not load libpdfium. Run ./scripts/fetch_pdfium.sh or set PDFIUM_LIB_DIR.",
        )?,
    };

    let _ = PDFIUM.set(Pdfium::new(bindings));
    Ok(PDFIUM.get().expect("PDFIUM was just initialized"))
}

pub struct OpenDocument {
    pub path: PathBuf,
    pub page_count: usize,
    doc: PdfDocument<'static>,
}

impl OpenDocument {
    pub fn open(path: &Path) -> Result<Self> {
        let pdfium = init_pdfium()?;
        let doc = pdfium
            .load_pdf_from_file(path, None)
            .with_context(|| format!("Failed to open PDF: {}", path.display()))?;
        let page_count = doc.pages().len() as usize;
        Ok(Self {
            path: path.to_path_buf(),
            page_count,
            doc,
        })
    }

    /// Returns page size in PDF points (width, height).
    pub fn page_size_pt(&self, page_index: usize) -> Result<(f32, f32)> {
        let page = self
            .doc
            .pages()
            .get(page_index as i32)
            .with_context(|| format!("Invalid page index {page_index}"))?;
        Ok((page.width().value, page.height().value))
    }

    /// Render a single page to RGBA8 at the requested target pixel width.
    pub fn render_page(&self, page_index: usize, target_width_px: i32) -> Result<RenderedPage> {
        let page = self
            .doc
            .pages()
            .get(page_index as i32)
            .with_context(|| format!("Invalid page index {page_index}"))?;

        let clamped_width = target_width_px.clamp(64, 8192);
        let render_config = PdfRenderConfig::new()
            .set_target_width(clamped_width)
            .set_maximum_height(8192);

        let bitmap = page
            .render_with_config(&render_config)
            .with_context(|| format!("Failed to render page {}", page_index + 1))?;

        let width = bitmap.width() as usize;
        let height = bitmap.height() as usize;
        let rgba = bitmap.as_rgba_bytes();

        Ok(RenderedPage {
            width,
            height,
            rgba,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn loads_pdfium_library() {
        let pdfium = init_pdfium().expect("libpdfium should load from ./lib or system");
        let doc = pdfium
            .create_new_pdf()
            .expect("should create in-memory PDF via PDFium");
        assert_eq!(doc.pages().len(), 0);
    }
}
