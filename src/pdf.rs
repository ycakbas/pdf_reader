use std::env;
use std::path::{Path, PathBuf};
use std::sync::OnceLock;

use anyhow::{Context, Result};
use pdfium_render::prelude::*;

pub const MIN_RENDER_WIDTH_PX: i32 = 64;
pub const HARD_MAX_RENDER_WIDTH_PX: i32 = 8192;

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

#[derive(Debug, Clone)]
pub struct PageImage {
    pub object_index: usize,
    pub rect: MatchRectPt,
    pub pixel_width: u32,
    pub pixel_height: u32,
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
    pub fn render_page(
        &self,
        page_index: usize,
        target_width_px: i32,
        max_width_px: i32,
        lcd_text: bool,
    ) -> Result<RenderedPage> {
        let page = self
            .doc
            .pages()
            .get(page_index as i32)
            .with_context(|| format!("Invalid page index {page_index}"))?;

        let max_w = max_width_px.clamp(512, HARD_MAX_RENDER_WIDTH_PX);
        let max_h = ((max_w as f32) * 1.6) as i32;
        let clamped_width = target_width_px.clamp(MIN_RENDER_WIDTH_PX, max_w);
        let render_config = PdfRenderConfig::new()
            .set_target_width(clamped_width)
            .set_maximum_height(max_h)
            .use_lcd_text_rendering(lcd_text)
            .set_text_smoothing(true)
            .set_image_smoothing(true)
            .set_path_smoothing(true);

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
        tolerance_x_pt: f32,
        tolerance_y_pt: f32,
    ) -> Option<usize> {
        let page = self.doc.pages().get(page_index as i32).ok()?;
        let page_height = self.page_size_pt(page_index).1;
        let y_pt_pdf = page_height - y_pt_top;
        let text = page.text().ok()?;
        let chars = text.chars();
        // NOTE: pdfium-render's parameter order is (x, tolerance_x, y, tolerance_y).
        let ch = chars.get_char_near_point(
            PdfPoints::new(x_pt),
            PdfPoints::new(tolerance_x_pt),
            PdfPoints::new(y_pt_pdf),
            PdfPoints::new(tolerance_y_pt),
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

    /// Expand a character index to the surrounding word range `(lo, hi)` (inclusive).
    pub fn word_range_at(&self, page_index: usize, char_idx: usize) -> Option<(usize, usize)> {
        let page = self.doc.pages().get(page_index as i32).ok()?;
        let text = page.text().ok()?;
        let chars = text.chars();
        let len = chars.len();
        if char_idx >= len {
            return None;
        }

        let target_class = char_word_class(chars.get(char_idx).ok()?.unicode_char()?);
        let mut lo = char_idx;
        while lo > 0 {
            let Ok(ch) = chars.get(lo - 1) else {
                break;
            };
            let Some(c) = ch.unicode_char() else {
                break;
            };
            if char_word_class(c) != target_class {
                break;
            }
            lo -= 1;
        }

        let mut hi = char_idx;
        while hi + 1 < len {
            let Ok(ch) = chars.get(hi + 1) else {
                break;
            };
            let Some(c) = ch.unicode_char() else {
                break;
            };
            if char_word_class(c) != target_class {
                break;
            }
            hi += 1;
        }

        Some((lo, hi))
    }

    /// Expand a character index to the surrounding visual line range `(lo, hi)` (inclusive).
    pub fn line_range_at(&self, page_index: usize, char_idx: usize) -> Option<(usize, usize)> {
        let page = self.doc.pages().get(page_index as i32).ok()?;
        let text = page.text().ok()?;
        let chars = text.chars();
        let len = chars.len();
        if char_idx >= len {
            return None;
        }

        let anchor = chars.get(char_idx).ok()?;
        let anchor_y = anchor.origin_y().ok()?.value;
        let line_tol = (anchor.scaled_font_size().value * 0.65).max(5.0);

        let same_line = |idx: usize| -> bool {
            let Ok(ch) = chars.get(idx) else {
                return false;
            };
            if let Some(c) = ch.unicode_char()
                && (c == '\n' || c == '\r')
            {
                return false;
            }
            if let Ok(y) = ch.origin_y() {
                (y.value - anchor_y).abs() <= line_tol
            } else {
                false
            }
        };

        let mut lo = char_idx;
        while lo > 0 && same_line(lo - 1) {
            lo -= 1;
        }

        let mut hi = char_idx;
        while hi + 1 < len && same_line(hi + 1) {
            hi += 1;
        }

        Some((lo, hi))
    }

    /// Extract all image objects on a page with their top-left-origin bounding boxes.
    pub fn page_images(&self, page_index: usize) -> Vec<PageImage> {
        let Ok(page) = self.doc.pages().get(page_index as i32) else {
            return Vec::new();
        };
        let page_height = self.page_size_pt(page_index).1;
        let mut out = Vec::new();

        for (object_index, obj) in page.objects().iter().enumerate() {
            let Some(img) = obj.as_image_object() else {
                continue;
            };
            let Ok(b) = obj.bounds() else {
                continue;
            };
            let rect = MatchRectPt {
                left: b.left().value,
                top: (page_height - b.top().value).max(0.0),
                right: b.right().value,
                bottom: (page_height - b.bottom().value).max(0.0),
            };
            let pixel_width = img.width().map(|p| p.max(0) as u32).unwrap_or(0);
            let pixel_height = img.height().map(|p| p.max(0) as u32).unwrap_or(0);

            out.push(PageImage {
                object_index,
                rect,
                pixel_width,
                pixel_height,
            });
        }

        out
    }

    /// Decode a specific image object from a page into RGBA8 pixels (for clipboard copy, etc.).
    pub fn extract_page_image_rgba(
        &self,
        page_index: usize,
        object_index: usize,
    ) -> Result<RenderedPage> {
        let page = self
            .doc
            .pages()
            .get(page_index as i32)
            .with_context(|| format!("Invalid page index {page_index}"))?;
        let obj = page
            .objects()
            .get(object_index)
            .with_context(|| format!("Invalid object index {object_index}"))?;
        let img_obj = obj
            .as_image_object()
            .context("Selected PDF object is not an image")?;

        let dynamic = img_obj
            .get_processed_image(&self.doc)
            .or_else(|_| img_obj.get_raw_image())
            .context("Failed to decode PDF image object")?;

        let rgba_img = dynamic.into_rgba8();
        let width = rgba_img.width() as usize;
        let height = rgba_img.height() as usize;
        Ok(RenderedPage {
            width,
            height,
            rgba: rgba_img.into_raw(),
        })
    }

    /// Export a specific image object from a page to `dest` (format inferred from file extension).
    pub fn export_page_image(
        &self,
        page_index: usize,
        object_index: usize,
        dest: &Path,
    ) -> Result<()> {
        let page = self
            .doc
            .pages()
            .get(page_index as i32)
            .with_context(|| format!("Invalid page index {page_index}"))?;
        let obj = page
            .objects()
            .get(object_index)
            .with_context(|| format!("Invalid object index {object_index}"))?;
        let img_obj = obj
            .as_image_object()
            .context("Selected PDF object is not an image")?;

        let dynamic = img_obj
            .get_processed_image(&self.doc)
            .or_else(|_| img_obj.get_raw_image())
            .context("Failed to decode PDF image object")?;

        dynamic
            .save(dest)
            .with_context(|| format!("Failed to save image to {}", dest.display()))?;
        Ok(())
    }
}

fn char_word_class(c: char) -> u8 {
    if c.is_alphanumeric() || c == '_' || c == '\'' {
        0
    } else if c.is_whitespace() {
        1
    } else {
        2
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
