use std::env;
use std::path::{Path, PathBuf};
use std::sync::OnceLock;

use anyhow::{Context, Result};
use pdfium_render::prelude::*;

pub const MAX_RENDER_WIDTH_PX: i32 = 3072;

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

/// Bounding box in page-local PDF points, normalized to top-left origin
/// (`x` rightwards, `y` downwards).
#[derive(Debug, Clone, Copy)]
pub struct MatchRectPt {
    pub left: f32,
    pub top: f32,
    pub right: f32,
    pub bottom: f32,
}

impl MatchRectPt {
    pub fn contains(&self, x: f32, y: f32) -> bool {
        x >= self.left && x <= self.right && y >= self.top && y <= self.bottom
    }
}

#[derive(Debug, Clone)]
pub struct SearchMatch {
    pub page_index: usize,
    pub rects: Vec<MatchRectPt>,
}

#[derive(Debug, Clone)]
pub enum LinkTarget {
    InternalPage(usize),
    ExternalUri(String),
}

#[derive(Debug, Clone)]
pub struct PageLink {
    pub rect: MatchRectPt,
    pub target: LinkTarget,
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

        let clamped_width = target_width_px.clamp(64, MAX_RENDER_WIDTH_PX);
        let render_config = PdfRenderConfig::new()
            .set_target_width(clamped_width)
            .set_maximum_height(4096);

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

    /// Extract all clickable links (internal destinations and external URIs) on a page.
    pub fn page_links(&self, page_index: usize) -> Vec<PageLink> {
        let Ok(page) = self.doc.pages().get(page_index as i32) else {
            return Vec::new();
        };
        let page_height = self.page_size_pt(page_index).1;
        let mut out = Vec::new();

        for link in page.links().iter() {
            let Ok(r) = link.rect() else {
                continue;
            };
            let rect = MatchRectPt {
                left: r.left().value,
                top: (page_height - r.top().value).max(0.0),
                right: r.right().value,
                bottom: (page_height - r.bottom().value).max(0.0),
            };

            let target = link
                .destination()
                .and_then(|d| d.page_index().ok())
                .and_then(|idx| usize::try_from(idx).ok())
                .map(LinkTarget::InternalPage)
                .or_else(|| {
                    link.action().and_then(|action| {
                        if let Some(uri) = action.as_uri_action().and_then(|u| u.uri().ok()) {
                            return Some(LinkTarget::ExternalUri(uri));
                        }
                        action
                            .as_local_destination_action()
                            .and_then(|l| l.destination().ok())
                            .and_then(|d| d.page_index().ok())
                            .and_then(|idx| usize::try_from(idx).ok())
                            .map(LinkTarget::InternalPage)
                    })
                });

            if let Some(target) = target {
                out.push(PageLink { rect, target });
            }
        }

        out
    }

    /// Find the character index near a page-local point (top-left origin, in PDF points).
    pub fn char_index_at(
        &self,
        page_index: usize,
        x_pt: f32,
        y_pt_top: f32,
        tolerance_pt: f32,
    ) -> Option<usize> {
        let page = self.doc.pages().get(page_index as i32).ok()?;
        let page_height = self.page_size_pt(page_index).1;
        let y_pt_pdf = page_height - y_pt_top;
        let text = page.text().ok()?;
        let chars = text.chars();
        let ch = chars.get_char_near_point(
            PdfPoints::new(x_pt),
            PdfPoints::new(y_pt_pdf),
            PdfPoints::new(tolerance_pt),
            PdfPoints::new(tolerance_pt),
        )?;
        Some(ch.index())
    }

    /// Compute selection highlight rectangles (top-left origin, PDF points) and extracted text
    /// for an inclusive character range `[a, b]` on `page_index`.
    pub fn selection_rects_and_text(
        &self,
        page_index: usize,
        a: usize,
        b: usize,
    ) -> (Vec<MatchRectPt>, String) {
        let Ok(page) = self.doc.pages().get(page_index as i32) else {
            return (Vec::new(), String::new());
        };
        let Ok(text) = page.text() else {
            return (Vec::new(), String::new());
        };
        let page_height = self.page_size_pt(page_index).1;
        let (lo, hi) = if a <= b { (a, b) } else { (b, a) };
        let count = hi - lo + 1;

        let mut rects = Vec::new();
        for seg in text.segments_subset(lo, count).iter() {
            let b = seg.bounds();
            rects.push(MatchRectPt {
                left: b.left().value,
                top: (page_height - b.top().value).max(0.0),
                right: b.right().value,
                bottom: (page_height - b.bottom().value).max(0.0),
            });
        }

        let chars = text.chars();
        let mut selected = String::with_capacity(count);
        for idx in lo..=hi {
            if let Ok(ch) = chars.get(idx)
                && let Some(c) = ch.unicode_char()
            {
                selected.push(c);
            }
        }

        (rects, selected)
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
