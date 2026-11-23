use std::env;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use pdfium_render::prelude::*;

pub struct RenderedPage {
    pub width: usize,
    pub height: usize,
    pub rgba: Vec<u8>,
}

pub struct PdfEngine {
    pdfium: Pdfium,
}

impl PdfEngine {
    /// Locate and bind to `libpdfium` using a fast, cross-platform search order:
    /// 1. `PDFIUM_LIB_DIR` environment variable
    /// 2. Directory containing the current executable
    /// 3. `./lib`
    /// 4. `./`
    /// 5. System library search path
    pub fn init() -> Result<Self> {
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

        for dir in &search_dirs {
            let candidate = Pdfium::pdfium_platform_library_name_at_path(dir);
            if let Ok(bindings) = Pdfium::bind_to_library(&candidate) {
                return Ok(Self {
                    pdfium: Pdfium::new(bindings),
                });
            }
        }

        let bindings = Pdfium::bind_to_system_library().context(
            "Could not load libpdfium. Run ./scripts/fetch_pdfium.sh or set PDFIUM_LIB_DIR.",
        )?;
        Ok(Self {
            pdfium: Pdfium::new(bindings),
        })
    }

    pub fn open<'a>(&'a self, path: &Path) -> Result<OpenDocument<'a>> {
        let doc = self
            .pdfium
            .load_pdf_from_file(path, None)
            .with_context(|| format!("Failed to open PDF: {}", path.display()))?;
        let page_count = doc.pages().len() as usize;
        Ok(OpenDocument {
            path: path.to_path_buf(),
            page_count,
            doc,
        })
    }
}

pub struct OpenDocument<'a> {
    pub path: PathBuf,
    pub page_count: usize,
    doc: PdfDocument<'a>,
}

impl<'a> OpenDocument<'a> {
    /// Returns page size in PDF points (width, height).
    pub fn page_size_pt(&self, page_index: usize) -> Result<(f32, f32)> {
        let page = self
            .doc
            .pages()
            .get(page_index as u16)
            .with_context(|| format!("Invalid page index {page_index}"))?;
        Ok((page.width().value, page.height().value))
    }

    /// Render a single page to RGBA8 at the requested target pixel width.
    pub fn render_page(&self, page_index: usize, target_width_px: i32) -> Result<RenderedPage> {
        let page = self
            .doc
            .pages()
            .get(page_index as u16)
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
