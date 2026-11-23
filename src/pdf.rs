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

#[derive(Debug, Clone)]
pub struct BookmarkNode {
    pub title: String,
    pub page_index: Option<usize>,
    pub children: Vec<BookmarkNode>,
}

/// Match bounding box in page-local PDF points, normalized to top-left origin
/// (`x` rightwards, `y` downwards).
#[derive(Debug, Clone, Copy)]
pub struct MatchRectPt {
    pub left: f32,
    pub top: f32,
    pub right: f32,
    pub bottom: f32,
}

#[derive(Debug, Clone)]
pub struct SearchMatch {
    pub page_index: usize,
    pub rects: Vec<MatchRectPt>,
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
    pub page_sizes_pt: Vec<(f32, f32)>,
    pub bookmarks: Vec<BookmarkNode>,
    doc: PdfDocument<'static>,
}

impl OpenDocument {
    pub fn open(path: &Path) -> Result<Self> {
        let pdfium = init_pdfium()?;
        let doc = pdfium
            .load_pdf_from_file(path, None)
            .with_context(|| format!("Failed to open PDF: {}", path.display()))?;
        let pages = doc.pages();
        let page_count = pages.len() as usize;

        let mut page_sizes_pt = Vec::with_capacity(page_count);
        for i in 0..page_count {
            let size = pages
                .get(i as i32)
                .map(|p| (p.width().value, p.height().value))
                .unwrap_or((612.0, 792.0));
            page_sizes_pt.push(size);
        }

        let bookmarks = extract_bookmarks(&doc);

        Ok(Self {
            path: path.to_path_buf(),
            page_count,
            page_sizes_pt,
            bookmarks,
            doc,
        })
    }

    /// Returns page size in PDF points (width, height).
    pub fn page_size_pt(&self, page_index: usize) -> (f32, f32) {
        self.page_sizes_pt
            .get(page_index)
            .copied()
            .unwrap_or((612.0, 792.0))
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

    /// Search all pages for `query` and return matches with top-left-origin bounding boxes.
    pub fn search(&self, query: &str, match_case: bool) -> Vec<SearchMatch> {
        let trimmed = query.trim();
        if trimmed.is_empty() {
            return Vec::new();
        }

        let options = PdfSearchOptions::new().match_case(match_case);
        let mut results = Vec::new();

        for page_index in 0..self.page_count {
            let Ok(page) = self.doc.pages().get(page_index as i32) else {
                continue;
            };
            let page_height = self.page_size_pt(page_index).1;
            let Ok(text) = page.text() else {
                continue;
            };
            let Ok(search) = text.search(trimmed, &options) else {
                continue;
            };

            while let Some(segments) = search.find_next() {
                let mut rects = Vec::new();
                for seg in segments.iter() {
                    let b = seg.bounds();
                    // PDF coordinates have bottom-left origin; flip Y to top-left origin.
                    rects.push(MatchRectPt {
                        left: b.left().value,
                        top: (page_height - b.top().value).max(0.0),
                        right: b.right().value,
                        bottom: (page_height - b.bottom().value).max(0.0),
                    });
                }
                if !rects.is_empty() {
                    results.push(SearchMatch { page_index, rects });
                }
            }
        }

        results
    }
}

fn extract_bookmarks(doc: &PdfDocument<'_>) -> Vec<BookmarkNode> {
    let mut roots = Vec::new();
    let mut current = doc.bookmarks().root();
    while let Some(bm) = current {
        roots.push(build_bookmark_node(&bm));
        current = bm.next_sibling();
    }
    roots
}

fn build_bookmark_node(bm: &PdfBookmark<'_>) -> BookmarkNode {
    let title = bm.title().unwrap_or_else(|| "(Untitled)".to_string());
    let page_index = bm
        .destination()
        .and_then(|d| d.page_index().ok())
        .or_else(|| {
            bm.action().and_then(|a| {
                a.as_local_destination_action()
                    .and_then(|l| l.destination().ok())
                    .and_then(|d| d.page_index().ok())
            })
        })
        .and_then(|idx| usize::try_from(idx).ok());

    let mut children = Vec::new();
    let mut child = bm.first_child();
    while let Some(c) = child {
        children.push(build_bookmark_node(&c));
        child = c.next_sibling();
    }

    BookmarkNode {
        title,
        page_index,
        children,
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
