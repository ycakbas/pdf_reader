use std::collections::HashMap;
use std::path::PathBuf;
use std::time::{Duration, Instant};

use eframe::egui;
use eframe::egui_wgpu;

use crate::config::{
    Action, Config, DefaultFitMode, DoubleClickAction, DragAction, KeyBinding, SideButtonAction,
    TabBarVisibility, WheelAction,
};
use crate::pdf::{
    BookmarkNode, HARD_MAX_RENDER_WIDTH_PX, LinkTarget, MIN_RENDER_WIDTH_PX, MatchRectPt,
    OpenDocument, PageImage, PageLink, SearchMatch, init_pdfium,
};
use crate::shader::{PdfShaderResources, paint_page_callback};
use crate::theme::{ResolvedTheme, all_themes};

const MAX_CACHED_PAGES: usize = 16;
const CANVAS_MARGIN: f32 = 24.0;
const ZOOM_SETTLE_DELAY: Duration = Duration::from_millis(120);

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum FitMode {
    Manual,
    FitWidth,
    FitPage,
}

impl From<DefaultFitMode> for FitMode {
    fn from(m: DefaultFitMode) -> Self {
        match m {
            DefaultFitMode::FitWidth => Self::FitWidth,
            DefaultFitMode::FitPage => Self::FitPage,
            DefaultFitMode::Manual => Self::Manual,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum DragMode {
    None,
    Pan,
    SelectText,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum SettingsTab {
    General,
    Mouse,
    Tabs,
}

struct TextSelection {
    page_index: usize,
    anchor_char: usize,
    head_char: usize,
    rects: Vec<MatchRectPt>,
    text: String,
}

struct ContextMenuState {
    pos: egui::Pos2,
    image: Option<(usize, PageImage)>,
    link: Option<LinkTarget>,
    has_text_selection: bool,
}

struct CachedPageEntry {
    rendered_width_px: i32,
    last_used_frame: u64,
    gpu_key: Option<u64>,
    cpu_texture: Option<(String, egui::TextureHandle)>,
}

struct PageCache {
    entries: HashMap<usize, CachedPageEntry>,
    frame_counter: u64,
}

impl PageCache {
    fn new() -> Self {
        Self {
            entries: HashMap::new(),
            frame_counter: 0,
        }
    }

    fn begin_frame(&mut self) {
        self.frame_counter = self.frame_counter.wrapping_add(1);
    }

    fn clear(&mut self, wgpu_state: Option<&egui_wgpu::RenderState>) {
        if let Some(rs) = wgpu_state {
            for entry in self.entries.values() {
                if let Some(key) = entry.gpu_key {
                    PdfShaderResources::remove_page(rs, key);
                }
            }
        }
        self.entries.clear();
    }

    fn has_page(&self, page_index: usize) -> bool {
        self.entries.contains_key(&page_index)
    }

    fn needs_render(
        &self,
        page_index: usize,
        clamped_target_width_px: i32,
        theme_name: &str,
        use_gpu: bool,
    ) -> bool {
        let Some(entry) = self.entries.get(&page_index) else {
            return true;
        };
        if (entry.rendered_width_px - clamped_target_width_px).abs() > 32 {
            return true;
        }
        if use_gpu {
            entry.gpu_key.is_none()
        } else {
            match &entry.cpu_texture {
                Some((cached_theme, _)) => cached_theme != theme_name,
                None => true,
            }
        }
    }

    fn touch(&mut self, page_index: usize) {
        if let Some(entry) = self.entries.get_mut(&page_index) {
            entry.last_used_frame = self.frame_counter;
        }
    }

    #[allow(clippy::too_many_arguments)]
    fn render_into_cache(
        &mut self,
        next_gpu_key: &mut u64,
        doc: &OpenDocument,
        page_index: usize,
        clamped_target_width_px: i32,
        max_width_px: i32,
        lcd_text: bool,
        theme: &ResolvedTheme,
        ctx: &egui::Context,
        wgpu_state: Option<&egui_wgpu::RenderState>,
    ) -> anyhow::Result<()> {
        let mut rendered =
            doc.render_page(page_index, clamped_target_width_px, max_width_px, lcd_text)?;

        if let Some(old) = self.entries.remove(&page_index)
            && let (Some(rs), Some(old_key)) = (wgpu_state, old.gpu_key)
        {
            PdfShaderResources::remove_page(rs, old_key);
        }

        let entry = if let Some(rs) = wgpu_state {
            let gpu_key = *next_gpu_key;
            *next_gpu_key = next_gpu_key.wrapping_add(1);
            PdfShaderResources::upload_page(
                rs,
                gpu_key,
                rendered.width as u32,
                rendered.height as u32,
                &rendered.rgba,
            );
            CachedPageEntry {
                rendered_width_px: clamped_target_width_px,
                last_used_frame: self.frame_counter,
                gpu_key: Some(gpu_key),
                cpu_texture: None,
            }
        } else {
            theme.apply_to_rgba(&mut rendered.rgba);
            let color_image = egui::ColorImage::from_rgba_unmultiplied(
                [rendered.width, rendered.height],
                &rendered.rgba,
            );
            let tex = ctx.load_texture(
                format!("pdf_page_{page_index}_{}", theme.name),
                color_image,
                egui::TextureOptions::LINEAR,
            );
            CachedPageEntry {
                rendered_width_px: clamped_target_width_px,
                last_used_frame: self.frame_counter,
                gpu_key: None,
                cpu_texture: Some((theme.name.clone(), tex)),
            }
        };

        self.entries.insert(page_index, entry);
        self.evict_lru(wgpu_state);
        Ok(())
    }

    fn evict_lru(&mut self, wgpu_state: Option<&egui_wgpu::RenderState>) {
        while self.entries.len() > MAX_CACHED_PAGES {
            let oldest = self
                .entries
                .iter()
                .min_by_key(|(_, e)| e.last_used_frame)
                .map(|(&idx, _)| idx);
            let Some(oldest_idx) = oldest else {
                break;
            };
            if let Some(removed) = self.entries.remove(&oldest_idx)
                && let (Some(rs), Some(key)) = (wgpu_state, removed.gpu_key)
            {
                PdfShaderResources::remove_page(rs, key);
            }
        }
    }
}

struct TabState {
    doc: OpenDocument,
    title: String,
    current_page: usize,
    nav_back_stack: Vec<usize>,
    nav_forward_stack: Vec<usize>,
    zoom: f32,
    fit_mode: FitMode,
    scroll_x: f32,
    scroll_y: f32,

    // Per-tab search state
    search_query: String,
    search_last_executed: String,
    search_matches: Vec<SearchMatch>,
    search_active_idx: usize,

    // Per-tab caches & selection
    links_cache: HashMap<usize, Vec<PageLink>>,
    images_cache: HashMap<usize, Vec<PageImage>>,
    selection: Option<TextSelection>,
    selected_image: Option<(usize, PageImage)>,
    thumb_cache: HashMap<usize, egui::TextureHandle>,
    last_thumb_synced_page: Option<usize>,
    cache: PageCache,
}

impl TabState {
    fn new(doc: OpenDocument, default_fit_mode: FitMode, initial_page: Option<usize>) -> Self {
        let title = doc
            .path
            .file_name()
            .and_then(|n| n.to_str())
            .unwrap_or("Untitled.pdf")
            .to_string();
        let target_page = initial_page
            .unwrap_or(0)
            .min(doc.page_count.saturating_sub(1));
        Self {
            doc,
            title,
            current_page: target_page,
            nav_back_stack: Vec::new(),
            nav_forward_stack: Vec::new(),
            zoom: 1.0,
            fit_mode: default_fit_mode,
            scroll_x: 0.0,
            scroll_y: 0.0,
            search_query: String::new(),
            search_last_executed: String::new(),
            search_matches: Vec::new(),
            search_active_idx: 0,
            links_cache: HashMap::new(),
            images_cache: HashMap::new(),
            selection: None,
            selected_image: None,
            thumb_cache: HashMap::new(),
            last_thumb_synced_page: None,
            cache: PageCache::new(),
        }
    }

    fn page_top_y(&self, page_index: usize, page_gap: f32) -> f32 {
        let mut y = CANVAS_MARGIN;
        for i in 0..page_index.min(self.doc.page_count) {
            y += self.doc.page_size_pt(i).1 * self.zoom + page_gap;
        }
        y
    }

    fn total_doc_height(&self, page_gap: f32) -> f32 {
        if self.doc.page_count == 0 {
            return 0.0;
        }
        let mut h = CANVAS_MARGIN * 2.0;
        for i in 0..self.doc.page_count {
            h += self.doc.page_size_pt(i).1 * self.zoom;
            if i + 1 < self.doc.page_count {
                h += page_gap;
            }
        }
        h
    }

    fn scroll_bounds(
        &self,
        canvas_size: egui::Vec2,
        continuous_scroll: bool,
        page_gap: f32,
    ) -> (f32, f32, f32, f32, f32) {
        let max_page_w_pt = if continuous_scroll {
            self.doc
                .page_sizes_pt
                .iter()
                .map(|(w, _)| *w)
                .fold(0.0_f32, f32::max)
        } else {
            self.doc.page_size_pt(self.current_page).0
        };

        let total_w = max_page_w_pt * self.zoom + CANVAS_MARGIN * 2.0;
        let max_x = (total_w - canvas_size.x).max(0.0) * 0.5;

        if continuous_scroll {
            let total_h = self.total_doc_height(page_gap);
            let max_y = (total_h - canvas_size.y).max(0.0);
            (max_x, 0.0, max_y, total_w, total_h)
        } else {
            let total_h = self.doc.page_size_pt(self.current_page).1 * self.zoom + CANVAS_MARGIN * 2.0;
            let max_y = (total_h - canvas_size.y).max(0.0) * 0.5;
            (max_x, -max_y, max_y, total_w, total_h)
        }
    }

    fn clamp_scroll(&mut self, canvas_size: egui::Vec2, continuous_scroll: bool, page_gap: f32) {
        let (max_x, min_y, max_y, _, _) =
            self.scroll_bounds(canvas_size, continuous_scroll, page_gap);
        self.scroll_x = self.scroll_x.clamp(-max_x, max_x);
        self.scroll_y = self.scroll_y.clamp(min_y, max_y);
    }

    fn goto_page_aligned(
        &mut self,
        index: usize,
        align_bottom: bool,
        canvas_size: egui::Vec2,
        continuous_scroll: bool,
        page_gap: f32,
    ) {
        if self.doc.page_count == 0 {
            return;
        }
        let clamped = index.min(self.doc.page_count - 1);
        self.current_page = clamped;
        if continuous_scroll {
            self.scroll_y = (self.page_top_y(clamped, page_gap) - CANVAS_MARGIN).max(0.0);
        } else {
            let (_, min_y, max_y, _, _) =
                self.scroll_bounds(canvas_size, continuous_scroll, page_gap);
            // In single-page mode: +max_y aligns page top with viewport top,
            // -max_y (min_y) aligns page bottom with viewport bottom,
            // and both are 0.0 (centered) when the page fits vertically.
            self.scroll_y = if align_bottom { min_y } else { max_y };
        }
        self.clamp_scroll(canvas_size, continuous_scroll, page_gap);
    }

    fn goto_page(
        &mut self,
        index: usize,
        canvas_size: egui::Vec2,
        continuous_scroll: bool,
        page_gap: f32,
    ) {
        self.goto_page_aligned(index, false, canvas_size, continuous_scroll, page_gap);
    }

    fn jump_via_link(
        &mut self,
        target_page: usize,
        canvas_size: egui::Vec2,
        continuous_scroll: bool,
        page_gap: f32,
    ) {
        if target_page != self.current_page {
            self.nav_back_stack.push(self.current_page);
            if self.nav_back_stack.len() > 64 {
                self.nav_back_stack.remove(0);
            }
            self.nav_forward_stack.clear();
        }
        self.goto_page(target_page, canvas_size, continuous_scroll, page_gap);
    }

    fn ensure_links_loaded(&mut self, page_index: usize) {
        if !self.links_cache.contains_key(&page_index) {
            let links = self.doc.page_links(page_index);
            self.links_cache.insert(page_index, links);
        }
    }

    fn ensure_images_loaded(&mut self, page_index: usize) {
        if !self.images_cache.contains_key(&page_index) {
            let images = self.doc.page_images(page_index);
            self.images_cache.insert(page_index, images);
        }
    }

    fn compute_visible_pages(
        &mut self,
        rect: egui::Rect,
        continuous_scroll: bool,
        page_gap: f32,
    ) -> Vec<(usize, egui::Rect)> {
        let mut visible_pages = Vec::new();
        let page_count = self.doc.page_count;
        if page_count == 0 {
            return visible_pages;
        }

        if continuous_scroll {
            let mut y = CANVAS_MARGIN;
            let view_top = self.scroll_y;
            let view_bottom = self.scroll_y + rect.height();
            let view_mid = self.scroll_y + rect.height() * 0.5;
            let mut center_page = self.current_page;

            for i in 0..page_count {
                let (pw, ph) = self.doc.page_size_pt(i);
                let disp_w = pw * self.zoom;
                let disp_h = ph * self.zoom;
                let page_bottom = y + disp_h;

                if y <= view_mid && view_mid <= page_bottom + page_gap {
                    center_page = i;
                }

                if page_bottom >= view_top && y <= view_bottom {
                    let screen_top = rect.min.y + (y - self.scroll_y);
                    let screen_left = rect.center().x + self.scroll_x - disp_w * 0.5;
                    let page_rect = egui::Rect::from_min_size(
                        egui::pos2(screen_left, screen_top),
                        egui::vec2(disp_w, disp_h),
                    );
                    visible_pages.push((i, page_rect));
                }

                y = page_bottom + page_gap;
                if y > view_bottom {
                    break;
                }
            }
            self.current_page = center_page;
        } else {
            let (pw, ph) = self.doc.page_size_pt(self.current_page);
            let disp_size = egui::vec2(pw * self.zoom, ph * self.zoom);
            let center = rect.center() + egui::vec2(self.scroll_x, self.scroll_y);
            visible_pages.push((
                self.current_page,
                egui::Rect::from_center_size(center, disp_size),
            ));
        }

        visible_pages
    }
}

pub struct PdfReaderApp {
    config: Config,
    config_path: Option<PathBuf>,
    bindings: Vec<KeyBinding>,
    themes: Vec<ResolvedTheme>,
    active_theme_idx: usize,
    wgpu_state: Option<egui_wgpu::RenderState>,
    next_gpu_key: u64,

    tabs: Vec<TabState>,
    active_tab: usize,
    status_error: Option<String>,

    zoom_settle_deadline: Option<Instant>,
    continuous_scroll: bool,
    last_canvas_size: egui::Vec2,

    show_toolbar: bool,
    show_toc: bool,
    show_thumbnails: bool,

    // Fast navigation state
    toolbar_page_input: String,
    goto_open: bool,
    goto_focus_requested: bool,
    goto_input: String,

    // Scrollbar drag & scroll acceleration state
    dragging_v_scrollbar: bool,
    dragging_h_scrollbar: bool,
    scroll_accel_boost: f32,
    last_scroll_time: Option<Instant>,
    last_scroll_dir: f32,
    wheel_flip_accum: f32,

    // Settings popup state
    settings_open: bool,
    settings_tab: SettingsTab,
    settings_status: Option<String>,

    // Search bar UI state
    search_open: bool,
    search_focus_requested: bool,
    search_match_case: bool,

    // Active drag & context menu state
    drag_mode: DragMode,
    drag_button: Option<egui::PointerButton>,
    pan_start_pointer: Option<egui::Pos2>,
    pan_start_scroll: egui::Vec2,
    context_menu: Option<ContextMenuState>,
    toast: Option<(String, Instant)>,
}

impl PdfReaderApp {
    pub fn new(
        cc: &eframe::CreationContext<'_>,
        config: Config,
        config_path: Option<PathBuf>,
        initial_files: Vec<PathBuf>,
        initial_page: Option<usize>,
    ) -> Self {
        let wgpu_state = cc.wgpu_render_state.clone();
        if let Some(rs) = &wgpu_state {
            PdfShaderResources::init(rs);
        }

        let bindings = config.shortcuts.compile();
        let themes = all_themes(&config.themes);
        let active_theme_idx = themes
            .iter()
            .position(|t| t.name == config.theme)
            .unwrap_or(0);
        let show_toolbar = config.show_toolbar;
        let continuous_scroll = config.continuous_scroll;
        let status_error = init_pdfium().err().map(|e| e.to_string());

        let mut app = Self {
            config,
            config_path,
            bindings,
            themes,
            active_theme_idx,
            wgpu_state,
            next_gpu_key: 1,
            tabs: Vec::new(),
            active_tab: 0,
            status_error,
            zoom_settle_deadline: None,
            continuous_scroll,
            last_canvas_size: egui::vec2(1000.0, 800.0),
            show_toolbar,
            show_toc: false,
            show_thumbnails: false,
            toolbar_page_input: String::new(),
            goto_open: false,
            goto_focus_requested: false,
            goto_input: String::new(),
            dragging_v_scrollbar: false,
            dragging_h_scrollbar: false,
            scroll_accel_boost: 1.0,
            last_scroll_time: None,
            last_scroll_dir: 0.0,
            wheel_flip_accum: 0.0,
            settings_open: false,
            settings_tab: SettingsTab::Mouse,
            settings_status: None,
            search_open: false,
            search_focus_requested: false,
            search_match_case: false,
            drag_mode: DragMode::None,
            drag_button: None,
            pan_start_pointer: None,
            pan_start_scroll: egui::Vec2::ZERO,
            context_menu: None,
            toast: None,
        };

        for (i, path) in initial_files.into_iter().enumerate() {
            let page = if i == 0 { initial_page } else { None };
            app.open_file(path, page);
        }
        if !app.tabs.is_empty() {
            app.active_tab = 0;
        }

        app
    }

    fn active_theme(&self) -> &ResolvedTheme {
        &self.themes[self.active_theme_idx]
    }

    fn current_tab(&self) -> Option<&TabState> {
        self.tabs.get(self.active_tab)
    }

    fn current_tab_mut(&mut self) -> Option<&mut TabState> {
        self.tabs.get_mut(self.active_tab)
    }

    fn page_count(&self) -> usize {
        self.current_tab().map_or(0, |t| t.doc.page_count)
    }

    fn clear_all_caches(&mut self) {
        let rs = self.wgpu_state.as_ref();
        for tab in &mut self.tabs {
            tab.cache.clear(rs);
        }
    }

    fn open_file(&mut self, path: PathBuf, initial_page: Option<usize>) {
        // Optionally focus an existing tab if this file is already open
        if self.config.tabs.reopen_existing_tab {
            let target_canon = std::fs::canonicalize(&path).ok();
            if let Some(existing_idx) = self.tabs.iter().position(|t| {
                if t.doc.path == path {
                    return true;
                }
                if let (Some(tc), Ok(ec)) = (&target_canon, std::fs::canonicalize(&t.doc.path)) {
                    return tc == &ec;
                }
                false
            }) {
                self.select_tab(existing_idx);
                if let Some(p) = initial_page {
                    self.goto_page(p);
                }
                return;
            }
        }

        match OpenDocument::open(&path) {
            Ok(doc) => {
                let default_fit = FitMode::from(self.config.default_fit_mode);
                let mut tab = TabState::new(doc, default_fit, initial_page);
                let target_page = tab.current_page;
                tab.goto_page(
                    target_page,
                    self.last_canvas_size,
                    self.continuous_scroll,
                    self.config.page_gap,
                );
                let new_idx = self.tabs.len();
                self.tabs.push(tab);
                if self.config.tabs.focus_new_tab || new_idx == 0 {
                    self.select_tab(new_idx);
                }
                self.status_error = None;
            }
            Err(err) => {
                self.status_error = Some(err.to_string());
            }
        }
    }

    fn select_tab(&mut self, index: usize) {
        if index < self.tabs.len() {
            self.active_tab = index;
            self.drag_mode = DragMode::None;
            self.drag_button = None;
            self.pan_start_pointer = None;
            self.dragging_v_scrollbar = false;
            self.dragging_h_scrollbar = false;
            self.tabs[index].last_thumb_synced_page = None;
        }
    }

    fn close_tab(&mut self, index: usize, ctx: &egui::Context) {
        if index >= self.tabs.len() {
            return;
        }
        let mut removed = self.tabs.remove(index);
        removed.cache.clear(self.wgpu_state.as_ref());

        if self.tabs.is_empty() {
            self.active_tab = 0;
            if self.config.tabs.close_window_on_last_tab {
                ctx.send_viewport_cmd(egui::ViewportCommand::Close);
            }
        } else if self.active_tab >= self.tabs.len() {
            self.active_tab = self.tabs.len() - 1;
            self.tabs[self.active_tab].last_thumb_synced_page = None;
        } else if index < self.active_tab {
            self.active_tab -= 1;
        } else {
            self.tabs[self.active_tab].last_thumb_synced_page = None;
        }

        self.drag_mode = DragMode::None;
        self.drag_button = None;
        self.pan_start_pointer = None;
    }

    fn pick_and_open_files(&mut self) {
        if let Some(paths) = rfd::FileDialog::new()
            .add_filter("PDF Documents", &["pdf", "PDF"])
            .pick_files()
        {
            for path in paths {
                self.open_file(path, None);
            }
        }
    }

    fn goto_page(&mut self, index: usize) {
        let canvas_size = self.last_canvas_size;
        let cont = self.continuous_scroll;
        let gap = self.config.page_gap;
        if let Some(tab) = self.current_tab_mut() {
            tab.goto_page(index, canvas_size, cont, gap);
        }
    }

    fn jump_via_link(&mut self, target_page: usize) {
        let canvas_size = self.last_canvas_size;
        let cont = self.continuous_scroll;
        let gap = self.config.page_gap;
        if let Some(tab) = self.current_tab_mut() {
            tab.jump_via_link(target_page, canvas_size, cont, gap);
        }
    }

    fn navigate_back(&mut self) {
        let canvas_size = self.last_canvas_size;
        let cont = self.continuous_scroll;
        let gap = self.config.page_gap;
        if let Some(tab) = self.current_tab_mut()
            && let Some(prev) = tab.nav_back_stack.pop()
        {
            tab.nav_forward_stack.push(tab.current_page);
            tab.goto_page(prev, canvas_size, cont, gap);
        }
    }

    fn navigate_forward(&mut self) {
        let canvas_size = self.last_canvas_size;
        let cont = self.continuous_scroll;
        let gap = self.config.page_gap;
        if let Some(tab) = self.current_tab_mut()
            && let Some(next) = tab.nav_forward_stack.pop()
        {
            tab.nav_back_stack.push(tab.current_page);
            tab.goto_page(next, canvas_size, cont, gap);
        }
    }

    fn zoom_at(&mut self, new_zoom: f32, anchor_in_canvas: egui::Vec2, canvas_size: egui::Vec2) {
        let cont = self.continuous_scroll;
        let gap = self.config.page_gap;
        let Some(tab) = self.current_tab_mut() else {
            return;
        };

        let old_zoom = tab.zoom;
        let clamped = new_zoom.clamp(0.1, 10.0);
        if (clamped - old_zoom).abs() < 1e-4 {
            return;
        }
        tab.fit_mode = FitMode::Manual;
        let ratio = clamped / old_zoom;

        let dx_from_center = anchor_in_canvas.x - canvas_size.x * 0.5;
        tab.scroll_x = dx_from_center - (dx_from_center - tab.scroll_x) * ratio;

        if cont {
            let doc_y = tab.scroll_y + anchor_in_canvas.y;
            tab.zoom = clamped;
            tab.scroll_y = doc_y * ratio - anchor_in_canvas.y;
        } else {
            let dy_from_center = anchor_in_canvas.y - canvas_size.y * 0.5;
            tab.scroll_y = dy_from_center - (dy_from_center - tab.scroll_y) * ratio;
            tab.zoom = clamped;
        }
        tab.clamp_scroll(canvas_size, cont, gap);
        self.zoom_settle_deadline = Some(Instant::now() + ZOOM_SETTLE_DELAY);
    }

    fn handle_side_button(&mut self, action: SideButtonAction) {
        let center = self.last_canvas_size * 0.5;
        let canvas_size = self.last_canvas_size;
        match action {
            SideButtonAction::NavigateBack => self.navigate_back(),
            SideButtonAction::NavigateForward => self.navigate_forward(),
            SideButtonAction::PrevPage => {
                if let Some(cur) = self.current_tab().map(|t| t.current_page) {
                    self.goto_page(cur.saturating_sub(1));
                }
            }
            SideButtonAction::NextPage => {
                if let Some(cur) = self.current_tab().map(|t| t.current_page) {
                    self.goto_page(cur.saturating_add(1));
                }
            }
            SideButtonAction::ZoomOut => {
                if let Some(z) = self.current_tab().map(|t| t.zoom) {
                    self.zoom_at(z / self.config.mouse.zoom_step, center, canvas_size);
                }
            }
            SideButtonAction::ZoomIn => {
                if let Some(z) = self.current_tab().map(|t| t.zoom) {
                    self.zoom_at(z * self.config.mouse.zoom_step, center, canvas_size);
                }
            }
            SideButtonAction::CycleTheme => {
                self.active_theme_idx = (self.active_theme_idx + 1) % self.themes.len();
                self.config.theme = self.active_theme().name.clone();
            }
            SideButtonAction::ToggleToc => {
                self.show_toc = !self.show_toc;
                if self.show_toc {
                    self.show_thumbnails = false;
                }
            }
            SideButtonAction::None => {}
        }
    }

    fn run_search(&mut self) {
        let match_case = self.search_match_case;
        let canvas_size = self.last_canvas_size;
        let cont = self.continuous_scroll;
        let gap = self.config.page_gap;
        let Some(tab) = self.current_tab_mut() else {
            return;
        };
        tab.search_last_executed = tab.search_query.clone();
        tab.search_active_idx = 0;
        tab.search_matches = tab.doc.search(&tab.search_query, match_case);
        if !tab.search_matches.is_empty() {
            scroll_tab_to_active_match(tab, canvas_size, cont, gap);
        }
    }

    fn step_search(&mut self, forward: bool) {
        let canvas_size = self.last_canvas_size;
        let cont = self.continuous_scroll;
        let gap = self.config.page_gap;
        let needs_run = match self.current_tab() {
            Some(tab) => tab.search_matches.is_empty() && !tab.search_query.trim().is_empty(),
            None => return,
        };
        if needs_run {
            self.run_search();
            return;
        }
        let Some(tab) = self.current_tab_mut() else {
            return;
        };
        if tab.search_matches.is_empty() {
            return;
        }
        let len = tab.search_matches.len();
        tab.search_active_idx = if forward {
            (tab.search_active_idx + 1) % len
        } else {
            (tab.search_active_idx + len - 1) % len
        };
        scroll_tab_to_active_match(tab, canvas_size, cont, gap);
    }

    fn handle_action(&mut self, action: Action, ctx: &egui::Context) {
        let center = self.last_canvas_size * 0.5;
        let canvas_size = self.last_canvas_size;

        match action {
            Action::NextPage => {
                if let Some(cur) = self.current_tab().map(|t| t.current_page) {
                    self.goto_page(cur.saturating_add(1));
                }
            }
            Action::PrevPage => {
                if let Some(cur) = self.current_tab().map(|t| t.current_page) {
                    self.goto_page(cur.saturating_sub(1));
                }
            }
            Action::FirstPage => self.goto_page(0),
            Action::LastPage => self.goto_page(self.page_count().saturating_sub(1)),
            Action::ZoomIn => {
                if let Some(z) = self.current_tab().map(|t| t.zoom) {
                    self.zoom_at(z * self.config.mouse.zoom_step, center, canvas_size);
                }
            }
            Action::ZoomOut => {
                if let Some(z) = self.current_tab().map(|t| t.zoom) {
                    self.zoom_at(z / self.config.mouse.zoom_step, center, canvas_size);
                }
            }
            Action::ZoomReset => {
                self.zoom_at(1.0, center, canvas_size);
                let cont = self.continuous_scroll;
                let gap = self.config.page_gap;
                if let Some(tab) = self.current_tab_mut() {
                    tab.scroll_x = 0.0;
                    tab.clamp_scroll(canvas_size, cont, gap);
                }
            }
            Action::FitWidth => {
                if let Some(tab) = self.current_tab_mut() {
                    tab.fit_mode = FitMode::FitWidth;
                    tab.scroll_x = 0.0;
                }
            }
            Action::FitPage => {
                let cont = self.continuous_scroll;
                if let Some(tab) = self.current_tab_mut() {
                    tab.fit_mode = FitMode::FitPage;
                    tab.scroll_x = 0.0;
                    if !cont {
                        tab.scroll_y = 0.0;
                    }
                }
            }
            Action::CycleTheme => {
                self.active_theme_idx = (self.active_theme_idx + 1) % self.themes.len();
                self.config.theme = self.active_theme().name.clone();
            }
            Action::ToggleToolbar => {
                self.show_toolbar = !self.show_toolbar;
            }
            Action::ToggleToc => {
                self.show_toc = !self.show_toc;
                if self.show_toc {
                    self.show_thumbnails = false;
                }
            }
            Action::ToggleThumbnails => {
                self.show_thumbnails = !self.show_thumbnails;
                if self.show_thumbnails {
                    self.show_toc = false;
                    if let Some(tab) = self.current_tab_mut() {
                        tab.last_thumb_synced_page = None;
                    }
                }
            }
            Action::ToggleContinuous => {
                self.continuous_scroll = !self.continuous_scroll;
                self.config.continuous_scroll = self.continuous_scroll;
                let cont = self.continuous_scroll;
                let gap = self.config.page_gap;
                for tab in &mut self.tabs {
                    let page = tab.current_page;
                    tab.goto_page(page, canvas_size, cont, gap);
                }
            }
            Action::GotoPage => {
                if let Some(cur) = self.current_tab().map(|t| t.current_page) {
                    self.goto_open = true;
                    self.goto_focus_requested = true;
                    self.goto_input = (cur + 1).to_string();
                }
            }
            Action::Find => {
                self.search_open = true;
                self.search_focus_requested = true;
            }
            Action::FindNext => self.step_search(true),
            Action::FindPrev => self.step_search(false),
            Action::OpenSettings => {
                self.settings_open = !self.settings_open;
                self.settings_status = None;
            }
            Action::OpenFile => self.pick_and_open_files(),
            Action::CloseTab => {
                if !self.tabs.is_empty() {
                    self.close_tab(self.active_tab, ctx);
                } else if self.config.tabs.close_window_on_last_tab {
                    ctx.send_viewport_cmd(egui::ViewportCommand::Close);
                }
            }
            Action::NextTab => {
                if self.tabs.len() > 1 {
                    let next = (self.active_tab + 1) % self.tabs.len();
                    self.select_tab(next);
                }
            }
            Action::PrevTab => {
                if self.tabs.len() > 1 {
                    let prev = (self.active_tab + self.tabs.len() - 1) % self.tabs.len();
                    self.select_tab(prev);
                }
            }
            Action::Quit => {
                ctx.send_viewport_cmd(egui::ViewportCommand::Close);
            }
        }
    }

    fn process_shortcuts(&mut self, ctx: &egui::Context) {
        let typing = ctx.egui_wants_keyboard_input();

        let (back_pressed, forward_pressed) = ctx.input(|i| {
            (
                i.pointer.button_pressed(egui::PointerButton::Extra1),
                i.pointer.button_pressed(egui::PointerButton::Extra2),
            )
        });
        if back_pressed {
            self.handle_side_button(self.config.mouse.back_button);
        }
        if forward_pressed {
            self.handle_side_button(self.config.mouse.forward_button);
        }

        // Copy selected text or selected image with Ctrl+C / Cmd+C when not typing in an input box
        if !typing && ctx.input(|i| i.modifiers.command && i.key_pressed(egui::Key::C)) {
            if let Some(tab) = self.current_tab() {
                if let Some(sel) = &tab.selection
                    && !sel.text.is_empty()
                {
                    ctx.copy_text(sel.text.clone());
                    self.toast = Some((
                        "Copied text".into(),
                        Instant::now() + Duration::from_secs(2),
                    ));
                    return;
                }
                if let Some((img_page, img)) = tab.selected_image.clone() {
                    self.copy_image_to_clipboard(ctx, img_page, &img);
                    return;
                }
            }
        }

        // Alt+Left / Alt+Right for link navigation history
        if !typing {
            if ctx.input(|i| i.modifiers.alt && i.key_pressed(egui::Key::ArrowLeft)) {
                self.navigate_back();
                return;
            }
            if ctx.input(|i| i.modifiers.alt && i.key_pressed(egui::Key::ArrowRight)) {
                self.navigate_forward();
                return;
            }
        }

        if ctx.input(|i| i.key_pressed(egui::Key::Escape)) {
            if self.context_menu.is_some() {
                self.context_menu = None;
                return;
            }
            if self.goto_open {
                self.goto_open = false;
                return;
            }
            if self.settings_open {
                self.settings_open = false;
                return;
            }
            if self.search_open {
                self.search_open = false;
                return;
            }
            if let Some(tab) = self.current_tab_mut()
                && (tab.selection.is_some() || tab.selected_image.is_some())
            {
                tab.selection = None;
                tab.selected_image = None;
                return;
            }
        }

        let mut triggered = Vec::new();
        ctx.input(|input| {
            for binding in &self.bindings {
                if typing
                    && !binding.modifiers.command
                    && !matches!(
                        binding.key,
                        egui::Key::F1 | egui::Key::F2 | egui::Key::F3 | egui::Key::F4 | egui::Key::F5
                    )
                {
                    continue;
                }

                let mods_match = input.modifiers.command == binding.modifiers.command
                    && input.modifiers.alt == binding.modifiers.alt
                    && (input.modifiers.shift == binding.modifiers.shift
                        || binding.key == egui::Key::Plus);
                if mods_match && input.key_pressed(binding.key) {
                    triggered.push(binding.action);
                }
            }
        });

        for action in triggered {
            self.handle_action(action, ctx);
        }
    }

    fn handle_dropped_files(&mut self, ctx: &egui::Context) {
        let dropped: Vec<PathBuf> = ctx.input(|i| {
            i.raw
                .dropped_files
                .iter()
                .map(|f| f.path().to_path_buf())
                .collect()
        });
        for path in dropped {
            self.open_file(path, None);
        }
    }

    fn should_show_tab_bar(&self) -> bool {
        match self.config.tabs.visibility {
            TabBarVisibility::Always => true,
            TabBarVisibility::MultipleOnly => self.tabs.len() > 1,
            TabBarVisibility::Never => false,
        }
    }

    fn draw_tab_bar(&mut self, ui: &mut egui::Ui, ctx: &egui::Context) {
        let mut select_idx: Option<usize> = None;
        let mut close_idx: Option<usize> = None;
        let mut open_new = false;
        let middle_close = self.config.tabs.middle_click_close;

        egui::ScrollArea::horizontal()
            .id_salt("tab_bar_scroll")
            .scroll_bar_visibility(egui::scroll_area::ScrollBarVisibility::AlwaysHidden)
            .show(ui, |ui| {
                ui.horizontal(|ui| {
                    ui.spacing_mut().item_spacing.x = 4.0;

                    for (idx, tab) in self.tabs.iter().enumerate() {
                        let is_active = idx == self.active_tab;
                        let bg = if is_active {
                            ui.visuals().selection.bg_fill
                        } else {
                            ui.visuals().faint_bg_color
                        };
                        let text_color = if is_active {
                            ui.visuals().strong_text_color()
                        } else {
                            ui.visuals().text_color()
                        };

                        let frame = egui::Frame::NONE
                            .fill(bg)
                            .corner_radius(4.0)
                            .inner_margin(egui::Margin::symmetric(8, 3));

                        let tab_resp = frame
                            .show(ui, |ui| {
                                ui.horizontal(|ui| {
                                    ui.spacing_mut().item_spacing.x = 6.0;
                                    let display_name = truncate_tab_title(&tab.title, 28);
                                    let label_resp = ui
                                        .add(
                                            egui::Label::new(
                                                egui::RichText::new(display_name)
                                                    .color(text_color),
                                            )
                                            .sense(egui::Sense::click()),
                                        )
                                        .on_hover_text(tab.doc.path.display().to_string());

                                    if label_resp.clicked() {
                                        select_idx = Some(idx);
                                    }
                                    if middle_close && label_resp.middle_clicked() {
                                        close_idx = Some(idx);
                                    }

                                    let close_btn = ui
                                        .add(
                                            egui::Button::new(
                                                egui::RichText::new("x").size(11.0).color(text_color),
                                            )
                                            .frame(false),
                                        )
                                        .on_hover_text("Close tab (Ctrl+W)");
                                    if close_btn.clicked() {
                                        close_idx = Some(idx);
                                    }
                                });
                            })
                            .response;

                        if middle_close && tab_resp.middle_clicked() {
                            close_idx = Some(idx);
                        }
                    }

                    if ui
                        .button("+")
                        .on_hover_text("Open PDF in new tab (Ctrl+O)")
                        .clicked()
                    {
                        open_new = true;
                    }
                });
            });

        if let Some(idx) = close_idx {
            self.close_tab(idx, ctx);
        } else if let Some(idx) = select_idx {
            self.select_tab(idx);
        }

        if open_new {
            self.pick_and_open_files();
        }
    }

    fn draw_toolbar(&mut self, ui: &mut egui::Ui) {
        ui.horizontal(|ui| {
            if ui.button("Open").clicked() {
                self.pick_and_open_files();
            }

            if ui
                .selectable_label(self.show_toc, "TOC")
                .on_hover_text("Toggle table of contents (Tab)")
                .clicked()
            {
                self.show_toc = !self.show_toc;
                if self.show_toc {
                    self.show_thumbnails = false;
                }
            }

            if ui
                .selectable_label(self.show_thumbnails, "Pages")
                .on_hover_text("Toggle page thumbnails sidebar (p)")
                .clicked()
            {
                self.show_thumbnails = !self.show_thumbnails;
                if self.show_thumbnails {
                    self.show_toc = false;
                    if let Some(tab) = self.current_tab_mut() {
                        tab.last_thumb_synced_page = None;
                    }
                }
            }

            if ui
                .selectable_label(self.search_open, "Find")
                .on_hover_text("Search text (Ctrl+F)")
                .clicked()
            {
                self.search_open = !self.search_open;
                if self.search_open {
                    self.search_focus_requested = true;
                }
            }

            let has_back = self
                .current_tab()
                .is_some_and(|t| !t.nav_back_stack.is_empty());
            let has_fwd = self
                .current_tab()
                .is_some_and(|t| !t.nav_forward_stack.is_empty());

            if has_back
                && ui
                    .button("Back")
                    .on_hover_text("Return to previous page (Alt+Left)")
                    .clicked()
            {
                self.navigate_back();
            }

            if has_fwd
                && ui
                    .button("Fwd")
                    .on_hover_text("Go forward in history (Alt+Right)")
                    .clicked()
            {
                self.navigate_forward();
            }

            ui.separator();

            if ui.button("<").clicked()
                && let Some(cur) = self.current_tab().map(|t| t.current_page)
            {
                self.goto_page(cur.saturating_sub(1));
            }
            let count = self.page_count();
            if count > 0 {
                let cur_page = self.current_tab().map_or(0, |t| t.current_page);
                let page_edit = ui.add(
                    egui::TextEdit::singleline(&mut self.toolbar_page_input).desired_width(38.0),
                );
                if !page_edit.has_focus() {
                    self.toolbar_page_input = (cur_page + 1).to_string();
                }
                if page_edit.lost_focus() && ui.input(|i| i.key_pressed(egui::Key::Enter)) {
                    self.jump_from_input(&self.toolbar_page_input.clone());
                }
                ui.label(format!("/ {count}"))
                    .on_hover_text("Type a page number or press Ctrl+G to jump");
            } else {
                ui.label("No document");
            }
            if ui.button(">").clicked()
                && let Some(cur) = self.current_tab().map(|t| t.current_page)
            {
                self.goto_page(cur.saturating_add(1));
            }

            ui.separator();

            let center = self.last_canvas_size * 0.5;
            let canvas_size = self.last_canvas_size;
            let cur_zoom = self.current_tab().map_or(1.0, |t| t.zoom);
            if ui.button("-").clicked() {
                self.zoom_at(cur_zoom / self.config.mouse.zoom_step, center, canvas_size);
            }
            ui.label(format!("{:.0}%", cur_zoom * 100.0));
            if ui.button("+").clicked() {
                self.zoom_at(cur_zoom * self.config.mouse.zoom_step, center, canvas_size);
            }
            if ui.button("Fit W").clicked()
                && let Some(tab) = self.current_tab_mut()
            {
                tab.fit_mode = FitMode::FitWidth;
                tab.scroll_x = 0.0;
            }
            if ui.button("Fit P").clicked()
                && let Some(tab) = self.current_tab_mut()
            {
                tab.fit_mode = FitMode::FitPage;
                tab.scroll_x = 0.0;
            }
            if ui
                .selectable_label(self.continuous_scroll, "Cont")
                .on_hover_text("Toggle continuous scroll (c)")
                .clicked()
            {
                self.continuous_scroll = !self.continuous_scroll;
                self.config.continuous_scroll = self.continuous_scroll;
                let cont = self.continuous_scroll;
                let gap = self.config.page_gap;
                for tab in &mut self.tabs {
                    let page = tab.current_page;
                    tab.goto_page(page, canvas_size, cont, gap);
                }
            }

            ui.separator();

            let current_theme = self.active_theme().name.clone();
            egui::ComboBox::from_id_salt("theme_picker")
                .selected_text(format!("Theme: {current_theme}"))
                .show_ui(ui, |ui| {
                    for (idx, theme) in self.themes.iter().enumerate() {
                        if ui
                            .selectable_label(idx == self.active_theme_idx, &theme.name)
                            .clicked()
                        {
                            self.active_theme_idx = idx;
                            self.config.theme = theme.name.clone();
                        }
                    }
                });

            ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                if ui
                    .selectable_label(self.settings_open, "Settings")
                    .on_hover_text("Configure mouse, tabs, and general settings (Ctrl+,)")
                    .clicked()
                {
                    self.settings_open = !self.settings_open;
                    self.settings_status = None;
                }

                if !self.should_show_tab_bar()
                    && let Some(tab) = self.current_tab()
                {
                    ui.weak(&tab.title);
                }
            });
        });
    }

    fn draw_settings_window(&mut self, ctx: &egui::Context) {
        if !self.settings_open {
            return;
        }

        let mut open = self.settings_open;
        let mut close_requested = false;
        egui::Window::new("Settings")
            .open(&mut open)
            .resizable(false)
            .collapsible(false)
            .default_width(450.0)
            .show(ctx, |ui| {
                ui.horizontal(|ui| {
                    ui.selectable_value(&mut self.settings_tab, SettingsTab::General, "General");
                    ui.selectable_value(&mut self.settings_tab, SettingsTab::Mouse, "Mouse");
                    ui.selectable_value(&mut self.settings_tab, SettingsTab::Tabs, "Tabs");
                });
                ui.separator();

                match self.settings_tab {
                    SettingsTab::General => {
                        egui::Grid::new("general_settings_grid")
                            .num_columns(2)
                            .spacing([16.0, 8.0])
                            .show(ui, |ui| {
                                ui.label("PDF Theme:");
                                let cur_name = self.active_theme().name.clone();
                                egui::ComboBox::from_id_salt("settings_theme")
                                    .selected_text(&cur_name)
                                    .show_ui(ui, |ui| {
                                        for (idx, theme) in self.themes.iter().enumerate() {
                                            if ui
                                                .selectable_label(
                                                    idx == self.active_theme_idx,
                                                    &theme.name,
                                                )
                                                .clicked()
                                            {
                                                self.active_theme_idx = idx;
                                                self.config.theme = theme.name.clone();
                                            }
                                        }
                                    });
                                ui.end_row();

                                ui.label("Default Fit Mode:");
                                egui::ComboBox::from_id_salt("settings_fit_mode")
                                    .selected_text(self.config.default_fit_mode.label())
                                    .show_ui(ui, |ui| {
                                        for mode in [
                                            DefaultFitMode::FitWidth,
                                            DefaultFitMode::FitPage,
                                            DefaultFitMode::Manual,
                                        ] {
                                            if ui
                                                .selectable_value(
                                                    &mut self.config.default_fit_mode,
                                                    mode,
                                                    mode.label(),
                                                )
                                                .clicked()
                                                && let Some(tab) = self.tabs.get_mut(self.active_tab)
                                            {
                                                tab.fit_mode = FitMode::from(mode);
                                            }
                                        }
                                    });
                                ui.end_row();

                                ui.label("Continuous Scroll:");
                                let prev_cont = self.config.continuous_scroll;
                                ui.checkbox(&mut self.config.continuous_scroll, "");
                                if self.config.continuous_scroll != prev_cont {
                                    self.continuous_scroll = self.config.continuous_scroll;
                                    let cont = self.continuous_scroll;
                                    let gap = self.config.page_gap;
                                    let sz = self.last_canvas_size;
                                    for tab in &mut self.tabs {
                                        let page = tab.current_page;
                                        tab.goto_page(page, sz, cont, gap);
                                    }
                                }
                                ui.end_row();

                                ui.label("Page Gap (px):");
                                ui.add(
                                    egui::Slider::new(&mut self.config.page_gap, 0.0..=48.0)
                                        .step_by(1.0),
                                );
                                ui.end_row();

                                ui.label("Show Toolbar on Startup:");
                                ui.checkbox(&mut self.config.show_toolbar, "");
                                ui.end_row();

                                ui.label("Show Scrollbars:");
                                ui.checkbox(&mut self.config.show_scrollbars, "");
                                ui.end_row();

                                ui.label("Render Scale (Sharpness):");
                                let prev_scale = self.config.render_scale;
                                ui.add(
                                    egui::Slider::new(&mut self.config.render_scale, 1.0..=3.0)
                                        .step_by(0.1)
                                        .fixed_decimals(1)
                                        .suffix("x"),
                                )
                                .on_hover_text(
                                    "Supersampling multiplier on top of display DPI (1.5x–2.0x for crisp text)",
                                );
                                if (self.config.render_scale - prev_scale).abs() > 1e-4 {
                                    self.clear_all_caches();
                                }
                                ui.end_row();

                                ui.label("Max Resolution (px):");
                                let prev_max_w = self.config.max_render_width;
                                ui.add(
                                    egui::Slider::new(
                                        &mut self.config.max_render_width,
                                        1536..=6144,
                                    )
                                    .step_by(256.0)
                                    .suffix(" px"),
                                )
                                .on_hover_text(
                                    "Maximum rasterized page width (higher = sharper when zoomed in, more VRAM)",
                                );
                                if self.config.max_render_width != prev_max_w {
                                    self.clear_all_caches();
                                }
                                ui.end_row();

                                ui.label("LCD Subpixel Text:");
                                let prev_lcd = self.config.lcd_text_rendering;
                                ui.checkbox(&mut self.config.lcd_text_rendering, "");
                                if self.config.lcd_text_rendering != prev_lcd {
                                    self.clear_all_caches();
                                }
                                ui.end_row();
                            });
                    }
                    SettingsTab::Mouse => {
                        egui::Grid::new("mouse_settings_grid")
                            .num_columns(2)
                            .spacing([16.0, 8.0])
                            .show(ui, |ui| {
                                drag_action_row(
                                    ui,
                                    "Left Drag:",
                                    "settings_left_drag",
                                    &mut self.config.mouse.left_drag,
                                );
                                drag_action_row(
                                    ui,
                                    "Middle Drag:",
                                    "settings_middle_drag",
                                    &mut self.config.mouse.middle_drag,
                                );
                                drag_action_row(
                                    ui,
                                    "Right Drag:",
                                    "settings_right_drag",
                                    &mut self.config.mouse.right_drag,
                                );
                                side_button_row(
                                    ui,
                                    "Mouse Back Button:",
                                    "settings_back_btn",
                                    &mut self.config.mouse.back_button,
                                );
                                side_button_row(
                                    ui,
                                    "Mouse Forward Button:",
                                    "settings_fwd_btn",
                                    &mut self.config.mouse.forward_button,
                                );
                                wheel_action_row(
                                    ui,
                                    "Mouse Wheel:",
                                    "settings_wheel",
                                    &mut self.config.mouse.wheel_action,
                                );
                                wheel_action_row(
                                    ui,
                                    "Ctrl + Wheel:",
                                    "settings_ctrl_wheel",
                                    &mut self.config.mouse.ctrl_wheel_action,
                                );
                                wheel_action_row(
                                    ui,
                                    "Shift + Wheel:",
                                    "settings_shift_wheel",
                                    &mut self.config.mouse.shift_wheel_action,
                                );

                                ui.label("Double Click:");
                                egui::ComboBox::from_id_salt("settings_dbl_click")
                                    .selected_text(self.config.mouse.double_click_action.label())
                                    .show_ui(ui, |ui| {
                                        for act in DoubleClickAction::ALL {
                                            ui.selectable_value(
                                                &mut self.config.mouse.double_click_action,
                                                act,
                                                act.label(),
                                            );
                                        }
                                    });
                                ui.end_row();

                                ui.label("Scroll Speed:");
                                ui.add(
                                    egui::Slider::new(
                                        &mut self.config.mouse.scroll_speed,
                                        0.2..=15.0,
                                    )
                                    .logarithmic(true)
                                    .fixed_decimals(2),
                                );
                                ui.end_row();

                                ui.label("Scroll Acceleration:");
                                ui.checkbox(&mut self.config.mouse.scroll_acceleration, "");
                                ui.end_row();

                                ui.add_enabled_ui(self.config.mouse.scroll_acceleration, |ui| {
                                    ui.label("Accel Strength:");
                                });
                                ui.add_enabled_ui(self.config.mouse.scroll_acceleration, |ui| {
                                    ui.add(
                                        egui::Slider::new(
                                            &mut self.config.mouse.scroll_accel_strength,
                                            0.2..=5.0,
                                        )
                                        .fixed_decimals(2),
                                    );
                                });
                                ui.end_row();

                                ui.add_enabled_ui(self.config.mouse.scroll_acceleration, |ui| {
                                    ui.label("Max Accel Multiplier:");
                                });
                                ui.add_enabled_ui(self.config.mouse.scroll_acceleration, |ui| {
                                    ui.add(
                                        egui::Slider::new(
                                            &mut self.config.mouse.scroll_accel_max,
                                            1.5..=20.0,
                                        )
                                        .logarithmic(true)
                                        .fixed_decimals(1)
                                        .suffix("x"),
                                    );
                                });
                                ui.end_row();

                                ui.label("Zoom Step:");
                                ui.add(
                                    egui::Slider::new(&mut self.config.mouse.zoom_step, 1.05..=1.5)
                                        .fixed_decimals(2),
                                );
                                ui.end_row();

                                ui.label("Invert Scroll:");
                                ui.checkbox(&mut self.config.mouse.invert_scroll, "");
                                ui.end_row();
                            });

                        ui.add_space(4.0);
                        ui.weak("Tip: Hold Space + Left-drag anytime to pan temporarily.");
                    }
                    SettingsTab::Tabs => {
                        egui::Grid::new("tabs_settings_grid")
                            .num_columns(2)
                            .spacing([16.0, 8.0])
                            .show(ui, |ui| {
                                ui.label("Tab Bar Visibility:");
                                egui::ComboBox::from_id_salt("settings_tab_vis")
                                    .selected_text(self.config.tabs.visibility.label())
                                    .show_ui(ui, |ui| {
                                        for vis in TabBarVisibility::ALL {
                                            ui.selectable_value(
                                                &mut self.config.tabs.visibility,
                                                vis,
                                                vis.label(),
                                            );
                                        }
                                    });
                                ui.end_row();

                                ui.label("Middle-Click Closes Tab:");
                                ui.checkbox(&mut self.config.tabs.middle_click_close, "");
                                ui.end_row();

                                ui.label("Focus New Tab on Open:");
                                ui.checkbox(&mut self.config.tabs.focus_new_tab, "");
                                ui.end_row();

                                ui.label("Switch to Existing Tab:");
                                ui.checkbox(&mut self.config.tabs.reopen_existing_tab, "")
                                    .on_hover_text(
                                        "If a PDF is already open, switch to its tab instead of opening a duplicate",
                                    );
                                ui.end_row();

                                ui.label("Close Window on Last Tab:");
                                ui.checkbox(&mut self.config.tabs.close_window_on_last_tab, "")
                                    .on_hover_text(
                                        "Quit the application when closing the last open tab",
                                    );
                                ui.end_row();
                            });

                        ui.add_space(4.0);
                        ui.weak("Shortcuts: Ctrl+O open, Ctrl+W close tab, Ctrl+Tab / Ctrl+Shift+Tab switch.");
                    }
                }

                ui.separator();
                ui.horizontal(|ui| {
                    if ui.button("Save to config.toml").clicked() {
                        match self.config.save(self.config_path.as_deref()) {
                            Ok(path) => {
                                self.settings_status = Some(format!("Saved to {}", path.display()));
                            }
                            Err(err) => {
                                self.settings_status = Some(format!("Save failed: {err}"));
                            }
                        }
                    }

                    if ui.button("Reset Defaults").clicked() {
                        let defaults = Config::default();
                        self.config.mouse = defaults.mouse;
                        self.config.tabs = defaults.tabs;
                        self.config.continuous_scroll = defaults.continuous_scroll;
                        self.config.page_gap = defaults.page_gap;
                        self.config.show_toolbar = defaults.show_toolbar;
                        self.config.show_scrollbars = defaults.show_scrollbars;
                        self.config.default_fit_mode = defaults.default_fit_mode;
                        self.config.render_scale = defaults.render_scale;
                        self.config.max_render_width = defaults.max_render_width;
                        self.config.lcd_text_rendering = defaults.lcd_text_rendering;
                        self.continuous_scroll = self.config.continuous_scroll;
                        self.clear_all_caches();
                        self.settings_status =
                            Some("Reset to defaults (click Save to persist)".into());
                    }

                    ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                        if ui.button("Close").clicked() {
                            close_requested = true;
                        }
                    });
                });

                if let Some(status) = &self.settings_status {
                    ui.add_space(4.0);
                    ui.weak(status);
                }
            });

        self.settings_open = open && !close_requested;
    }

    fn jump_from_input(&mut self, raw: &str) {
        let count = self.page_count();
        if count == 0 {
            return;
        }
        let s = raw.trim();
        if let Some(pct_str) = s.strip_suffix('%') {
            if let Ok(pct) = pct_str.trim().parse::<f32>() {
                let frac = (pct / 100.0).clamp(0.0, 1.0);
                let target = ((count.saturating_sub(1)) as f32 * frac).round() as usize;
                self.jump_via_link(target);
            }
        } else if let Ok(page_1based) = s.parse::<usize>() {
            let target = page_1based.saturating_sub(1).min(count - 1);
            self.jump_via_link(target);
        }
    }

    fn draw_goto_window(&mut self, ctx: &egui::Context) {
        if !self.goto_open {
            return;
        }

        let count = self.page_count();
        let cur_page = self.current_tab().map_or(0, |t| t.current_page);
        let mut open = self.goto_open;
        let mut submit = false;
        let mut close_requested = false;

        egui::Window::new("Go to Page")
            .open(&mut open)
            .collapsible(false)
            .resizable(false)
            .anchor(egui::Align2::CENTER_TOP, egui::vec2(0.0, 64.0))
            .show(ctx, |ui| {
                ui.horizontal(|ui| {
                    ui.label(format!("Page (1–{count}) or %:"));
                    let resp = ui.add(
                        egui::TextEdit::singleline(&mut self.goto_input)
                            .desired_width(90.0)
                            .hint_text("e.g. 42 or 50%"),
                    );
                    if self.goto_focus_requested {
                        resp.request_focus();
                        self.goto_focus_requested = false;
                    }
                    if resp.lost_focus() && ui.input(|i| i.key_pressed(egui::Key::Enter)) {
                        submit = true;
                    }
                    if ui.button("Go").clicked() {
                        submit = true;
                    }
                });
                if count > 1 {
                    ui.add_space(4.0);
                    let mut p = cur_page + 1;
                    if ui
                        .add(egui::Slider::new(&mut p, 1..=count).text("Scrub"))
                        .changed()
                    {
                        self.goto_page(p - 1);
                        self.goto_input = p.to_string();
                    }
                }
                if submit {
                    self.jump_from_input(&self.goto_input.clone());
                    close_requested = true;
                }
            });

        self.goto_open = open && !close_requested;
    }

    fn copy_image_to_clipboard(
        &mut self,
        ctx: &egui::Context,
        page_index: usize,
        img: &PageImage,
    ) {
        let Some(tab) = self.current_tab() else {
            return;
        };
        match tab.doc.extract_page_image_rgba(page_index, img.object_index) {
            Ok(rendered) => {
                let color_img = egui::ColorImage::from_rgba_unmultiplied(
                    [rendered.width, rendered.height],
                    &rendered.rgba,
                );
                ctx.copy_image(color_img);
                self.toast = Some((
                    format!("Copied image ({}x{})", rendered.width, rendered.height),
                    Instant::now() + Duration::from_secs(2),
                ));
            }
            Err(err) => {
                self.toast = Some((
                    format!("Copy failed: {err}"),
                    Instant::now() + Duration::from_secs(4),
                ));
            }
        }
    }

    fn export_image_dialog(&mut self, page_index: usize, img: &PageImage) {
        let default_name = format!("page_{}_image_{}.png", page_index + 1, img.object_index + 1);
        let Some(dest) = rfd::FileDialog::new()
            .set_file_name(&default_name)
            .add_filter("PNG Image", &["png"])
            .add_filter("JPEG Image", &["jpg", "jpeg"])
            .add_filter("WebP Image", &["webp"])
            .add_filter("BMP Image", &["bmp"])
            .save_file()
        else {
            return;
        };

        if let Some(tab) = self.current_tab() {
            match tab.doc.export_page_image(page_index, img.object_index, &dest) {
                Ok(()) => {
                    let name = dest
                        .file_name()
                        .and_then(|n| n.to_str())
                        .unwrap_or("image");
                    self.toast = Some((
                        format!("Exported {name}"),
                        Instant::now() + Duration::from_secs(3),
                    ));
                }
                Err(err) => {
                    self.toast = Some((
                        format!("Export failed: {err}"),
                        Instant::now() + Duration::from_secs(4),
                    ));
                }
            }
        }
    }

    fn draw_context_menu(&mut self, ctx: &egui::Context) {
        let Some(menu) = self.context_menu.as_ref() else {
            return;
        };

        let pos = menu.pos;
        let image_opt = menu.image.clone();
        let link_opt = menu.link.clone();
        let has_text = menu.has_text_selection;

        let mut close_menu = false;
        let mut do_copy_text = false;
        let mut do_copy_image: Option<(usize, PageImage)> = None;
        let mut do_export_image: Option<(usize, PageImage)> = None;
        let mut do_open_link: Option<LinkTarget> = None;
        let mut do_copy_url: Option<String> = None;

        let area_resp = egui::Area::new(egui::Id::unique("pdf_context_menu"))
            .fixed_pos(pos)
            .order(egui::Order::Foreground)
            .show(ctx, |ui| {
                egui::Frame::popup(ui.style()).show(ui, |ui| {
                    ui.set_min_width(175.0);

                    if has_text && ui.button("Copy Text (Ctrl+C)").clicked() {
                        do_copy_text = true;
                        close_menu = true;
                    }

                    if let Some((img_page, img)) = image_opt {
                        if ui.button("Copy Image (Ctrl+C)").clicked() {
                            do_copy_image = Some((img_page, img.clone()));
                            close_menu = true;
                        }
                        let label = if img.pixel_width > 0 && img.pixel_height > 0 {
                            format!("Export Image ({}x{})...", img.pixel_width, img.pixel_height)
                        } else {
                            "Export Image...".to_string()
                        };
                        if ui.button(label).clicked() {
                            do_export_image = Some((img_page, img));
                            close_menu = true;
                        }
                    }

                    if let Some(link) = link_opt {
                        match &link {
                            LinkTarget::InternalPage(p) => {
                                if ui.button(format!("Go to Page {}", p + 1)).clicked() {
                                    do_open_link = Some(link);
                                    close_menu = true;
                                }
                            }
                            LinkTarget::ExternalUri(uri) => {
                                if ui.button("Open Link in Browser").clicked() {
                                    do_open_link = Some(link.clone());
                                    close_menu = true;
                                }
                                if ui.button("Copy Link URL").clicked() {
                                    do_copy_url = Some(uri.clone());
                                    close_menu = true;
                                }
                            }
                        }
                    }
                });
            });

        // Close if user clicked anywhere outside the menu
        if ctx.input(|i| i.pointer.any_pressed())
            && let Some(ptr) = ctx.input(|i| i.pointer.interact_pos())
            && !area_resp.response.rect.contains(ptr)
        {
            close_menu = true;
        }

        if close_menu {
            self.context_menu = None;
        }
        if do_copy_text
            && let Some(tab) = self.current_tab()
            && let Some(sel) = &tab.selection
        {
            ctx.copy_text(sel.text.clone());
            self.toast = Some((
                "Copied text".into(),
                Instant::now() + Duration::from_secs(2),
            ));
        }
        if let Some((p, img)) = do_copy_image {
            self.copy_image_to_clipboard(ctx, p, &img);
        }
        if let Some((p, img)) = do_export_image {
            self.export_image_dialog(p, &img);
        }
        if let Some(link) = do_open_link {
            match link {
                LinkTarget::InternalPage(p) => self.jump_via_link(p),
                LinkTarget::ExternalUri(uri) => {
                    ctx.open_url(egui::OpenUrl::new_tab(uri));
                }
            }
        }
        if let Some(uri) = do_copy_url {
            ctx.copy_text(uri);
            self.toast = Some((
                "Copied link URL".into(),
                Instant::now() + Duration::from_secs(2),
            ));
        }
    }

    fn draw_toast(&mut self, ctx: &egui::Context) {
        let Some((msg, deadline)) = &self.toast else {
            return;
        };
        let now = Instant::now();
        if now >= *deadline {
            self.toast = None;
            return;
        }
        ctx.request_repaint_after(*deadline - now);

        egui::Area::new(egui::Id::unique("pdf_toast"))
            .anchor(egui::Align2::RIGHT_BOTTOM, egui::vec2(-20.0, -20.0))
            .order(egui::Order::Tooltip)
            .interactable(false)
            .show(ctx, |ui| {
                egui::Frame::NONE
                    .fill(egui::Color32::from_rgba_unmultiplied(24, 24, 30, 230))
                    .corner_radius(6.0)
                    .inner_margin(egui::Margin::symmetric(12, 7))
                    .show(ui, |ui| {
                        ui.label(egui::RichText::new(msg).color(egui::Color32::WHITE));
                    });
            });
    }

    fn draw_thumbnails_panel(&mut self, ui: &mut egui::Ui) {
        ui.heading("Pages");
        ui.separator();

        let Some(tab) = self.tabs.get_mut(self.active_tab) else {
            ui.weak("No document open.");
            return;
        };
        let count = tab.doc.page_count;
        if count == 0 {
            ui.weak("No document open.");
            return;
        }

        let thumb_w = 136.0_f32;
        let row_h = 196.0_f32;
        let mut jump_to: Option<usize> = None;
        let mut rendered_this_frame = 0_usize;
        let ctx = ui.ctx().clone();

        let mut scroll_area = egui::ScrollArea::vertical()
            .id_salt(("thumb_scroll", self.active_tab))
            .auto_shrink([false, false]);

        if tab.last_thumb_synced_page != Some(tab.current_page) {
            let avail_h = ui.available_height().max(row_h);
            let target_y =
                (tab.current_page as f32 * row_h - avail_h * 0.5 + row_h * 0.5).max(0.0);
            scroll_area = scroll_area.vertical_scroll_offset(target_y);
            tab.last_thumb_synced_page = Some(tab.current_page);
        }

        scroll_area.show_rows(ui, row_h, count, |ui, row_range| {
            for page_idx in row_range {
                if !tab.thumb_cache.contains_key(&page_idx) && rendered_this_frame < 2 {
                    if let Ok(rendered) = tab.doc.render_page(page_idx, 160, 512, false) {
                        let color_img = egui::ColorImage::from_rgba_unmultiplied(
                            [rendered.width, rendered.height],
                            &rendered.rgba,
                        );
                        let tex = ctx.load_texture(
                            format!("pdf_thumb_{}_{page_idx}", self.active_tab),
                            color_img,
                            egui::TextureOptions::LINEAR,
                        );
                        tab.thumb_cache.insert(page_idx, tex);
                        rendered_this_frame += 1;
                        ctx.request_repaint();
                    }
                }

                let is_current = page_idx == tab.current_page;
                let (pw, ph) = tab.doc.page_size_pt(page_idx);
                let aspect = (ph / pw.max(1.0)).clamp(0.4, 2.0);
                let img_h = (thumb_w * aspect).min(row_h - 28.0);
                let img_w = img_h / aspect;

                ui.vertical_centered(|ui| {
                    let frame = egui::Frame::NONE
                        .stroke(if is_current {
                            egui::Stroke::new(2.0, egui::Color32::from_rgb(80, 160, 255))
                        } else {
                            egui::Stroke::new(1.0, egui::Color32::from_gray(70))
                        })
                        .inner_margin(2.0);

                    let resp = frame
                        .show(ui, |ui| {
                            let (img_rect, resp) = ui.allocate_exact_size(
                                egui::vec2(img_w, img_h),
                                egui::Sense::click(),
                            );
                            if let Some(tex) = tab.thumb_cache.get(&page_idx) {
                                ui.painter().image(
                                    tex.id(),
                                    img_rect,
                                    egui::Rect::from_min_max(
                                        egui::pos2(0.0, 0.0),
                                        egui::pos2(1.0, 1.0),
                                    ),
                                    egui::Color32::WHITE,
                                );
                            } else {
                                ui.painter().rect_filled(
                                    img_rect,
                                    2.0,
                                    egui::Color32::from_gray(50),
                                );
                            }
                            resp
                        })
                        .inner;

                    if resp.clicked() {
                        jump_to = Some(page_idx);
                    }

                    if ui
                        .selectable_label(is_current, format!("{}", page_idx + 1))
                        .clicked()
                    {
                        jump_to = Some(page_idx);
                    }
                });
            }
        });

        if let Some(page) = jump_to {
            self.goto_page(page);
        }
    }

    fn draw_search_bar(&mut self, ui: &mut egui::Ui) {
        let mut do_search = false;
        let mut do_step: Option<bool> = None;
        let mut close = false;

        ui.horizontal(|ui| {
            ui.label("Find:");
            let Some(tab) = self.tabs.get_mut(self.active_tab) else {
                ui.weak("No document open.");
                return;
            };

            let resp = ui.add(
                egui::TextEdit::singleline(&mut tab.search_query)
                    .desired_width(240.0)
                    .hint_text("Search in document..."),
            );
            if self.search_focus_requested {
                resp.request_focus();
                self.search_focus_requested = false;
            }

            let enter_pressed = resp.lost_focus() && ui.input(|i| i.key_pressed(egui::Key::Enter));
            let shift_held = ui.input(|i| i.modifiers.shift);
            if enter_pressed {
                if tab.search_query != tab.search_last_executed {
                    do_search = true;
                } else {
                    do_step = Some(!shift_held);
                }
                resp.request_focus();
            }

            if ui.button("Search").clicked() {
                do_search = true;
            }
            if ui.button("Prev").clicked() {
                do_step = Some(false);
            }
            if ui.button("Next").clicked() {
                do_step = Some(true);
            }

            let prev_case = self.search_match_case;
            ui.checkbox(&mut self.search_match_case, "Aa")
                .on_hover_text("Match case");
            if self.search_match_case != prev_case && !tab.search_query.trim().is_empty() {
                do_search = true;
            }

            if !tab.search_matches.is_empty() {
                ui.label(format!(
                    "{} / {}",
                    tab.search_active_idx + 1,
                    tab.search_matches.len()
                ));
            } else if !tab.search_last_executed.trim().is_empty() {
                ui.weak("No matches");
            }

            ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                if ui.button("x").clicked() {
                    close = true;
                }
            });
        });

        if close {
            self.search_open = false;
        }
        if do_search {
            self.run_search();
        } else if let Some(fwd) = do_step {
            self.step_search(fwd);
        }
    }

    fn draw_toc_panel(&mut self, ui: &mut egui::Ui) {
        ui.heading("Contents");
        ui.separator();

        let bookmarks = match self.current_tab() {
            Some(tab) => tab.doc.bookmarks.clone(),
            None => Vec::new(),
        };

        if bookmarks.is_empty() {
            ui.weak("No bookmarks in this PDF.");
            return;
        }

        let mut jump_to: Option<usize> = None;
        egui::ScrollArea::vertical().show(ui, |ui| {
            for (i, node) in bookmarks.iter().enumerate() {
                draw_bookmark_node(ui, node, i, &mut jump_to);
            }
        });

        if let Some(page) = jump_to {
            self.jump_via_link(page);
        }
    }

    fn draw_scrollbars(&mut self, ui: &mut egui::Ui, rect: egui::Rect) -> bool {
        if !self.config.show_scrollbars || self.tabs.is_empty() {
            return false;
        }

        let cont = self.continuous_scroll;
        let gap = self.config.page_gap;
        let (max_x, min_y, max_y, total_w, total_h) =
            self.tabs[self.active_tab].scroll_bounds(rect.size(), cont, gap);
        let show_v = (max_y - min_y) > 1.0;
        let show_h = max_x > 1.0;
        let bar_thickness = 12.0_f32;
        let pad = 2.0_f32;
        let mut active = false;

        let (primary_down, latest_pos) =
            ui.ctx().input(|i| (i.pointer.primary_down(), i.pointer.latest_pos()));
        if !primary_down {
            self.dragging_v_scrollbar = false;
            self.dragging_h_scrollbar = false;
        }

        if show_v {
            let track_bottom = if show_h {
                rect.max.y - bar_thickness - pad
            } else {
                rect.max.y - pad
            };
            let track_rect = egui::Rect::from_min_max(
                egui::pos2(rect.max.x - bar_thickness - pad, rect.min.y + pad),
                egui::pos2(rect.max.x - pad, track_bottom),
            );

            if track_rect.height() > 32.0 {
                let id = ui.make_persistent_id("pdf_v_scrollbar");
                let resp = ui.interact(track_rect, id, egui::Sense::click_and_drag());
                if resp.drag_started_by(egui::PointerButton::Primary) {
                    self.dragging_v_scrollbar = true;
                }
                let is_dragging = self.dragging_v_scrollbar || resp.dragged();
                let is_hot = resp.hovered() || is_dragging;
                if is_hot {
                    active = true;
                }

                let visible_frac = (rect.height() / total_h.max(1.0)).clamp(0.05, 1.0);
                let thumb_h = (track_rect.height() * visible_frac).max(28.0);
                let travel = (track_rect.height() - thumb_h).max(1.0);

                if (is_dragging || resp.clicked())
                    && let Some(pos) = resp.interact_pointer_pos().or(latest_pos)
                {
                    let t = ((pos.y - track_rect.min.y - thumb_h * 0.5) / travel).clamp(0.0, 1.0);
                    let tab = &mut self.tabs[self.active_tab];
                    tab.scroll_y = if cont {
                        min_y + t * (max_y - min_y)
                    } else {
                        max_y - t * (max_y - min_y)
                    };
                    tab.clamp_scroll(rect.size(), cont, gap);
                }

                let tab = &self.tabs[self.active_tab];
                let t = if cont {
                    ((tab.scroll_y - min_y) / (max_y - min_y).max(1.0)).clamp(0.0, 1.0)
                } else {
                    ((max_y - tab.scroll_y) / (max_y - min_y).max(1.0)).clamp(0.0, 1.0)
                };
                let thumb_y = track_rect.min.y + t * travel;
                let thumb_rect = egui::Rect::from_min_size(
                    egui::pos2(track_rect.min.x + 1.0, thumb_y),
                    egui::vec2(track_rect.width() - 2.0, thumb_h),
                );

                let painter = ui.painter_at(rect);
                let track_bg = if is_hot {
                    egui::Color32::from_rgba_unmultiplied(20, 20, 24, 120)
                } else {
                    egui::Color32::from_rgba_unmultiplied(20, 20, 24, 55)
                };
                let thumb_bg = if is_dragging {
                    egui::Color32::from_rgba_unmultiplied(220, 220, 230, 230)
                } else if resp.hovered() {
                    egui::Color32::from_rgba_unmultiplied(195, 195, 205, 200)
                } else {
                    egui::Color32::from_rgba_unmultiplied(160, 160, 170, 135)
                };

                painter.rect_filled(track_rect, 4.0, track_bg);
                painter.rect_filled(thumb_rect, 4.0, thumb_bg);

                if is_dragging {
                    let badge_text =
                        format!("Page {} / {}", tab.current_page + 1, tab.doc.page_count);
                    let badge_pos = egui::pos2(track_rect.min.x - 10.0, thumb_rect.center().y);
                    let galley = painter.layout_no_wrap(
                        badge_text,
                        egui::FontId::proportional(13.0),
                        egui::Color32::WHITE,
                    );
                    let badge_size = galley.size() + egui::vec2(14.0, 8.0);
                    let badge_rect = egui::Rect::from_min_size(
                        egui::pos2(badge_pos.x - badge_size.x, badge_pos.y - badge_size.y * 0.5),
                        badge_size,
                    );
                    painter.rect_filled(
                        badge_rect,
                        4.0,
                        egui::Color32::from_rgba_unmultiplied(24, 24, 30, 225),
                    );
                    painter.galley(
                        badge_rect.min + egui::vec2(7.0, 4.0),
                        galley,
                        egui::Color32::WHITE,
                    );
                }
            }
        }

        if show_h {
            let track_right = if show_v {
                rect.max.x - bar_thickness - pad * 2.0
            } else {
                rect.max.x - pad
            };
            let track_rect = egui::Rect::from_min_max(
                egui::pos2(rect.min.x + pad, rect.max.y - bar_thickness - pad),
                egui::pos2(track_right, rect.max.y - pad),
            );

            if track_rect.width() > 32.0 {
                let id = ui.make_persistent_id("pdf_h_scrollbar");
                let resp = ui.interact(track_rect, id, egui::Sense::click_and_drag());
                if resp.drag_started_by(egui::PointerButton::Primary) {
                    self.dragging_h_scrollbar = true;
                }
                let is_dragging = self.dragging_h_scrollbar || resp.dragged();
                let is_hot = resp.hovered() || is_dragging;
                if is_hot {
                    active = true;
                }

                let visible_frac = (rect.width() / total_w.max(1.0)).clamp(0.05, 1.0);
                let thumb_w = (track_rect.width() * visible_frac).max(28.0);
                let travel = (track_rect.width() - thumb_w).max(1.0);

                if (is_dragging || resp.clicked())
                    && let Some(pos) = resp.interact_pointer_pos().or(latest_pos)
                {
                    let t = ((pos.x - track_rect.min.x - thumb_w * 0.5) / travel).clamp(0.0, 1.0);
                    let tab = &mut self.tabs[self.active_tab];
                    tab.scroll_x = max_x - t * (2.0 * max_x);
                    tab.clamp_scroll(rect.size(), cont, gap);
                }

                let tab = &self.tabs[self.active_tab];
                let t = ((max_x - tab.scroll_x) / (2.0 * max_x).max(1.0)).clamp(0.0, 1.0);
                let thumb_x = track_rect.min.x + t * travel;
                let thumb_rect = egui::Rect::from_min_size(
                    egui::pos2(thumb_x, track_rect.min.y + 1.0),
                    egui::vec2(thumb_w, track_rect.height() - 2.0),
                );

                let painter = ui.painter_at(rect);
                let track_bg = if is_hot {
                    egui::Color32::from_rgba_unmultiplied(20, 20, 24, 120)
                } else {
                    egui::Color32::from_rgba_unmultiplied(20, 20, 24, 55)
                };
                let thumb_bg = if is_dragging {
                    egui::Color32::from_rgba_unmultiplied(220, 220, 230, 230)
                } else if resp.hovered() {
                    egui::Color32::from_rgba_unmultiplied(195, 195, 205, 200)
                } else {
                    egui::Color32::from_rgba_unmultiplied(160, 160, 170, 135)
                };

                painter.rect_filled(track_rect, 4.0, track_bg);
                painter.rect_filled(thumb_rect, 4.0, thumb_bg);
            }
        }

        active
    }

    fn draw_canvas(&mut self, ui: &mut egui::Ui) {
        let canvas_color = self.active_theme().canvas;
        let rect = ui.available_rect_before_wrap();
        self.last_canvas_size = rect.size();
        ui.painter().rect_filled(rect, 0.0, canvas_color);

        if let Some(err) = &self.status_error {
            ui.painter().text(
                rect.center(),
                egui::Align2::CENTER_CENTER,
                err,
                egui::FontId::proportional(16.0),
                egui::Color32::LIGHT_RED,
            );
            return;
        }

        if self.tabs.is_empty() {
            ui.painter().text(
                rect.center(),
                egui::Align2::CENTER_CENTER,
                "Drop PDF file(s) here or press Ctrl+O to open",
                egui::FontId::proportional(18.0),
                egui::Color32::from_gray(180),
            );
            return;
        }

        let cont = self.continuous_scroll;
        let gap = self.config.page_gap;
        self.tabs[self.active_tab].cache.begin_frame();

        if self.tabs[self.active_tab].doc.page_count == 0 {
            return;
        }

        // Fit mode recalculation using current page's dimensions
        let avail_w = (rect.width() - CANVAS_MARGIN * 2.0).max(64.0);
        let avail_h = (rect.height() - CANVAS_MARGIN * 2.0).max(64.0);
        {
            let tab = &mut self.tabs[self.active_tab];
            let (cur_pw, cur_ph) = tab.doc.page_size_pt(tab.current_page);
            match tab.fit_mode {
                FitMode::FitWidth => {
                    let target_zoom = (avail_w / cur_pw).clamp(0.1, 10.0);
                    if (target_zoom - tab.zoom).abs() > 1e-3 {
                        let ratio = target_zoom / tab.zoom;
                        tab.zoom = target_zoom;
                        if cont {
                            tab.scroll_y *= ratio;
                        }
                    }
                }
                FitMode::FitPage => {
                    let target_zoom = (avail_w / cur_pw).min(avail_h / cur_ph).clamp(0.1, 10.0);
                    if (target_zoom - tab.zoom).abs() > 1e-3 {
                        let ratio = target_zoom / tab.zoom;
                        tab.zoom = target_zoom;
                        if cont {
                            tab.scroll_y *= ratio;
                        }
                    }
                }
                FitMode::Manual => {}
            }
            tab.clamp_scroll(rect.size(), cont, gap);
        }

        let hover_pos = ui.ctx().input(|i| i.pointer.hover_pos());
        let latest_pos = ui.ctx().input(|i| i.pointer.latest_pos());
        let on_scrollbar = self.dragging_v_scrollbar
            || self.dragging_h_scrollbar
            || (self.config.show_scrollbars
                && hover_pos
                    .is_some_and(|p| p.x >= rect.max.x - 16.0 || p.y >= rect.max.y - 16.0));

        let response = ui.allocate_rect(rect, egui::Sense::click_and_drag());

        let visible_pages = self.tabs[self.active_tab].compute_visible_pages(rect, cont, gap);
        for &(page_idx, _) in &visible_pages {
            self.tabs[self.active_tab].ensure_links_loaded(page_idx);
            self.tabs[self.active_tab].ensure_images_loaded(page_idx);
        }

        let zoom_now = self.tabs[self.active_tab].zoom;
        let hovered_page_pt: Option<(usize, f32, f32, egui::Rect)> = if on_scrollbar {
            None
        } else {
            hover_pos.and_then(|pos| {
                visible_pages.iter().find_map(|&(page_idx, page_rect)| {
                    if page_rect.contains(pos) {
                        let x_pt = (pos.x - page_rect.min.x) / zoom_now;
                        let y_pt = (pos.y - page_rect.min.y) / zoom_now;
                        Some((page_idx, x_pt, y_pt, page_rect))
                    } else {
                        None
                    }
                })
            })
        };

        let hovered_link: Option<(usize, PageLink, egui::Rect)> =
            hovered_page_pt.and_then(|(page_idx, x_pt, y_pt, page_rect)| {
                self.tabs[self.active_tab]
                    .links_cache
                    .get(&page_idx)?
                    .iter()
                    .find_map(|link| {
                        if link.rect.contains(x_pt, y_pt) {
                            Some((page_idx, link.clone(), page_rect))
                        } else {
                            None
                        }
                    })
            });

        // Pick the smallest (innermost) image containing the pointer so overlapping background images don't shadow foreground ones
        let hovered_image: Option<(usize, PageImage, egui::Rect)> =
            hovered_page_pt.and_then(|(page_idx, x_pt, y_pt, page_rect)| {
                self.tabs[self.active_tab]
                    .images_cache
                    .get(&page_idx)?
                    .iter()
                    .filter(|img| img.rect.contains(x_pt, y_pt))
                    .min_by(|a, b| {
                        let area_a = (a.rect.right - a.rect.left) * (a.rect.bottom - a.rect.top);
                        let area_b = (b.rect.right - b.rect.left) * (b.rect.bottom - b.rect.top);
                        area_a.total_cmp(&area_b)
                    })
                    .map(|img| (page_idx, img.clone(), page_rect))
            });

        let hovered_char: Option<(usize, usize)> = hovered_page_pt.and_then(|(page_idx, x_pt, y_pt, _)| {
            self.tabs[self.active_tab]
                .doc
                .char_index_at(page_idx, x_pt, y_pt, 6.0, 6.0)
                .map(|c| (page_idx, c))
        });

        // Cursor icon & link tooltip
        if response.hovered() {
            if let Some((_, ref link, _)) = hovered_link {
                ui.ctx().set_cursor_icon(egui::CursorIcon::PointingHand);
                let tip = match &link.target {
                    LinkTarget::InternalPage(p) => format!("Go to page {}", p + 1),
                    LinkTarget::ExternalUri(uri) => uri.clone(),
                };
                response.clone().on_hover_text(tip);
            } else if self.drag_mode == DragMode::Pan {
                ui.ctx().set_cursor_icon(egui::CursorIcon::Grabbing);
            } else if hovered_char.is_some() {
                ui.ctx().set_cursor_icon(egui::CursorIcon::Text);
            } else if hovered_image.is_some() {
                ui.ctx().set_cursor_icon(egui::CursorIcon::PointingHand);
            }
        }

        // Triple-click: select entire visual line of text
        let mut handled_multiclick = false;
        if response.triple_clicked() && !on_scrollbar {
            self.context_menu = None;
            if let Some((page_idx, x_pt, y_pt, _)) = hovered_page_pt {
                let tab = &mut self.tabs[self.active_tab];
                if let Some(ch) = tab.doc.char_index_at(page_idx, x_pt, y_pt, 12.0, 12.0)
                    && let Some((lo, hi)) = tab.doc.line_range_at(page_idx, ch)
                {
                    let (rects, text) = tab.doc.selection_rects_and_text(page_idx, lo, hi);
                    tab.selection = Some(TextSelection {
                        page_index: page_idx,
                        anchor_char: lo,
                        head_char: hi,
                        rects,
                        text,
                    });
                    tab.selected_image = None;
                    handled_multiclick = true;
                }
            }
        }

        // Double-click: select word if over text, select image if over image, else run double_click_action
        if response.double_clicked() && !handled_multiclick && !on_scrollbar {
            self.context_menu = None;
            let mut selected_something = false;

            if let Some((page_idx, x_pt, y_pt, _)) = hovered_page_pt {
                let tab = &mut self.tabs[self.active_tab];
                if let Some(ch) = tab.doc.char_index_at(page_idx, x_pt, y_pt, 10.0, 10.0)
                    && let Some((lo, hi)) = tab.doc.word_range_at(page_idx, ch)
                {
                    let (rects, text) = tab.doc.selection_rects_and_text(page_idx, lo, hi);
                    tab.selection = Some(TextSelection {
                        page_index: page_idx,
                        anchor_char: lo,
                        head_char: hi,
                        rects,
                        text,
                    });
                    tab.selected_image = None;
                    selected_something = true;
                }
            }

            if !selected_something
                && let Some((img_page, ref img, _)) = hovered_image
            {
                let tab = &mut self.tabs[self.active_tab];
                tab.selected_image = Some((img_page, img.clone()));
                tab.selection = None;
                selected_something = true;
            }

            if !selected_something && hovered_link.is_none() {
                let tab = &mut self.tabs[self.active_tab];
                match self.config.mouse.double_click_action {
                    DoubleClickAction::FitWidth => {
                        tab.fit_mode = FitMode::FitWidth;
                        tab.scroll_x = 0.0;
                    }
                    DoubleClickAction::FitPage => {
                        tab.fit_mode = FitMode::FitPage;
                        tab.scroll_x = 0.0;
                    }
                    DoubleClickAction::ToggleZoom => {
                        if tab.fit_mode == FitMode::FitPage {
                            tab.fit_mode = FitMode::FitWidth;
                        } else {
                            tab.fit_mode = FitMode::FitPage;
                            tab.scroll_x = 0.0;
                        }
                    }
                    DoubleClickAction::None => {}
                }
                tab.clamp_scroll(rect.size(), cont, gap);
            }
            handled_multiclick = true;
        }

        // Single-click handling: open link, select image, or clear selection
        if response.clicked() && !handled_multiclick && !on_scrollbar {
            self.context_menu = None;
            if let Some((_, link, _)) = &hovered_link {
                match &link.target {
                    LinkTarget::InternalPage(p) => self.jump_via_link(*p),
                    LinkTarget::ExternalUri(uri) => {
                        ui.ctx().open_url(egui::OpenUrl::new_tab(uri.clone()));
                    }
                }
            } else if hovered_char.is_none()
                && let Some((img_page, ref img, _)) = hovered_image
            {
                let tab = &mut self.tabs[self.active_tab];
                tab.selected_image = Some((img_page, img.clone()));
                tab.selection = None;
            } else {
                let tab = &mut self.tabs[self.active_tab];
                tab.selection = None;
                tab.selected_image = None;
            }
        }

        // Right-click (non-drag) context menu for image export, copying text, and links
        if response.secondary_clicked() && !on_scrollbar {
            if let Some(pos) = hover_pos.or(latest_pos) {
                let image_target = hovered_image.as_ref().map(|(p, img, _)| (*p, img.clone()));
                if let Some((img_page, ref img)) = image_target {
                    self.tabs[self.active_tab].selected_image = Some((img_page, img.clone()));
                }
                let link_target = hovered_link.as_ref().map(|(_, l, _)| l.target.clone());
                let has_text = self.tabs[self.active_tab]
                    .selection
                    .as_ref()
                    .is_some_and(|s| !s.text.is_empty());

                if image_target.is_some() || link_target.is_some() || has_text {
                    self.context_menu = Some(ContextMenuState {
                        pos,
                        image: image_target,
                        link: link_target,
                        has_text_selection: has_text,
                    });
                } else {
                    self.context_menu = None;
                }
            }
        }

        // Drag start
        let space_held = ui.ctx().input(|i| i.key_down(egui::Key::Space));
        if response.drag_started() && !on_scrollbar {
            self.context_menu = None;
            let (started_btn, drag_action) =
                if response.drag_started_by(egui::PointerButton::Primary) {
                    let act = if space_held {
                        DragAction::Pan
                    } else {
                        self.config.mouse.left_drag
                    };
                    (Some(egui::PointerButton::Primary), act)
                } else if response.drag_started_by(egui::PointerButton::Middle) {
                    (Some(egui::PointerButton::Middle), self.config.mouse.middle_drag)
                } else if response.drag_started_by(egui::PointerButton::Secondary) {
                    (Some(egui::PointerButton::Secondary), self.config.mouse.right_drag)
                } else {
                    (None, DragAction::None)
                };

            match drag_action {
                DragAction::Pan => {
                    let tab = &self.tabs[self.active_tab];
                    self.drag_mode = DragMode::Pan;
                    self.drag_button = started_btn;
                    self.pan_start_pointer = ui
                        .ctx()
                        .input(|i| i.pointer.press_origin())
                        .or(latest_pos);
                    self.pan_start_scroll = egui::vec2(tab.scroll_x, tab.scroll_y);
                }
                DragAction::SelectText => {
                    let press_pos = ui
                        .ctx()
                        .input(|i| i.pointer.press_origin())
                        .or(latest_pos);
                    let tab = &mut self.tabs[self.active_tab];
                    let start_pt = press_pos.and_then(|pos| {
                        visible_pages.iter().find_map(|&(page_idx, page_rect)| {
                            if page_rect.contains(pos) {
                                let x_pt = (pos.x - page_rect.min.x) / tab.zoom;
                                let y_pt = (pos.y - page_rect.min.y) / tab.zoom;
                                Some((page_idx, x_pt, y_pt))
                            } else {
                                None
                            }
                        })
                    });

                    if let Some((page_idx, x_pt, y_pt)) = start_pt
                        && let Some(char_idx) = tab
                            .doc
                            .char_index_at(page_idx, x_pt, y_pt, 16.0, 16.0)
                            .or_else(|| tab.doc.char_index_at(page_idx, x_pt, y_pt, 80.0, 24.0))
                    {
                        let (rects, text) =
                            tab.doc
                                .selection_rects_and_text(page_idx, char_idx, char_idx);
                        tab.selection = Some(TextSelection {
                            page_index: page_idx,
                            anchor_char: char_idx,
                            head_char: char_idx,
                            rects,
                            text,
                        });
                        tab.selected_image = None;
                        self.drag_mode = DragMode::SelectText;
                        self.drag_button = started_btn;
                    } else {
                        tab.selection = None;
                        self.drag_mode = DragMode::None;
                        self.drag_button = None;
                    }
                }
                DragAction::None => {
                    self.drag_mode = DragMode::None;
                    self.drag_button = None;
                }
            }
        }

        if let Some(btn) = self.drag_button
            && !ui.ctx().input(|i| i.pointer.button_down(btn))
        {
            self.drag_mode = DragMode::None;
            self.drag_button = None;
            self.pan_start_pointer = None;
        }

        // Active drag update
        match self.drag_mode {
            DragMode::Pan => {
                if let (Some(start_pos), Some(pos)) = (self.pan_start_pointer, latest_pos) {
                    let total = pos - start_pos;
                    let desired_x = self.pan_start_scroll.x + total.x;
                    let desired_y = if cont {
                        self.pan_start_scroll.y - total.y
                    } else {
                        self.pan_start_scroll.y + total.y
                    };

                    let tab = &mut self.tabs[self.active_tab];
                    tab.scroll_x = desired_x;
                    tab.scroll_y = desired_y;
                    tab.clamp_scroll(rect.size(), cont, gap);

                    if (tab.scroll_x - desired_x).abs() > 0.5
                        || (tab.scroll_y - desired_y).abs() > 0.5
                    {
                        self.pan_start_pointer = Some(pos);
                        self.pan_start_scroll = egui::vec2(tab.scroll_x, tab.scroll_y);
                    }
                }
            }
            DragMode::SelectText => {
                let tab = &mut self.tabs[self.active_tab];
                if let (Some(sel), Some(pos)) = (&mut tab.selection, latest_pos)
                    && let Some(&(_, page_rect)) =
                        visible_pages.iter().find(|&&(idx, _)| idx == sel.page_index)
                {
                    let x_pt = ((pos.x - page_rect.min.x) / tab.zoom)
                        .clamp(0.0, page_rect.width() / tab.zoom);
                    let y_pt = ((pos.y - page_rect.min.y) / tab.zoom)
                        .clamp(0.0, page_rect.height() / tab.zoom);
                    let found = tab
                        .doc
                        .char_index_at(sel.page_index, x_pt, y_pt, 16.0, 16.0)
                        .or_else(|| {
                            tab.doc
                                .char_index_at(sel.page_index, x_pt, y_pt, 120.0, 24.0)
                        });
                    if let Some(head) = found
                        && head != sel.head_char
                    {
                        sel.head_char = head;
                        let (rects, text) = tab.doc.selection_rects_and_text(
                            sel.page_index,
                            sel.anchor_char,
                            head,
                        );
                        sel.rects = rects;
                        sel.text = text;
                    }
                }
            }
            DragMode::None => {}
        }

        // Mouse wheel + trackpad pinch zoom
        if ui.rect_contains_pointer(rect) {
            let (scroll_delta, pinch_delta, modifiers, pointer_pos, raw_wheel_events) =
                ui.ctx().input(|i| {
                    let wheel_events: Vec<(egui::MouseWheelUnit, egui::Vec2)> = i
                        .events
                        .iter()
                        .filter_map(|ev| match ev {
                            egui::Event::MouseWheel { unit, delta, .. } => Some((*unit, *delta)),
                            _ => None,
                        })
                        .collect();
                    (
                        i.smooth_scroll_delta,
                        i.zoom_delta(),
                        i.modifiers,
                        i.pointer.hover_pos(),
                        wheel_events,
                    )
                });

            let sign = if self.config.mouse.invert_scroll {
                -1.0
            } else {
                1.0
            };

            // Compute exact discrete page-flip steps from raw MouseWheel events
            // (avoids egui's smooth_scroll_delta tail triggering a second page flip!)
            let mut wheel_page_steps: i32 = 0;
            for &(unit, raw_d) in &raw_wheel_events {
                let dy = raw_d.y * sign;
                if dy == 0.0 {
                    continue;
                }
                match unit {
                    egui::MouseWheelUnit::Line | egui::MouseWheelUnit::Page => {
                        self.wheel_flip_accum = 0.0;
                        if dy < 0.0 {
                            wheel_page_steps += 1;
                        } else {
                            wheel_page_steps -= 1;
                        }
                    }
                    egui::MouseWheelUnit::Point => {
                        self.wheel_flip_accum += dy;
                        if self.wheel_flip_accum <= -45.0 {
                            wheel_page_steps += 1;
                            self.wheel_flip_accum = 0.0;
                        } else if self.wheel_flip_accum >= 45.0 {
                            wheel_page_steps -= 1;
                            self.wheel_flip_accum = 0.0;
                        }
                    }
                }
            }

            let anchor = pointer_pos
                .map(|p| p - rect.min)
                .unwrap_or(rect.size() * 0.5);

            if (pinch_delta - 1.0).abs() > 1e-3 {
                let z = self.tabs[self.active_tab].zoom;
                self.zoom_at(z * pinch_delta, anchor, rect.size());
            }

            if scroll_delta != egui::Vec2::ZERO || wheel_page_steps != 0 {
                let wheel_action = if modifiers.command {
                    self.config.mouse.ctrl_wheel_action
                } else if modifiers.shift {
                    self.config.mouse.shift_wheel_action
                } else {
                    self.config.mouse.wheel_action
                };

                let is_scroll_action = matches!(
                    wheel_action,
                    WheelAction::Scroll | WheelAction::HorizontalScroll
                );

                let accel_mult = if is_scroll_action
                    && self.config.mouse.scroll_acceleration
                    && scroll_delta != egui::Vec2::ZERO
                {
                    let now = Instant::now();
                    let dir = if scroll_delta.y.abs() >= scroll_delta.x.abs() {
                        scroll_delta.y.signum()
                    } else {
                        scroll_delta.x.signum()
                    };
                    let dt = self
                        .last_scroll_time
                        .map(|t| now.duration_since(t).as_secs_f32())
                        .unwrap_or(1.0);

                    if dt > 0.35 || (dir != 0.0 && dir * self.last_scroll_dir < 0.0) {
                        self.scroll_accel_boost = 1.0;
                    } else {
                        let decay = (-dt * 3.5).exp();
                        let retained = 1.0 + (self.scroll_accel_boost - 1.0) * decay;
                        let impulse = 0.22 * self.config.mouse.scroll_accel_strength;
                        let max_mult = self.config.mouse.scroll_accel_max.max(1.0);
                        self.scroll_accel_boost = (retained + impulse).clamp(1.0, max_mult);
                    }
                    self.last_scroll_time = Some(now);
                    if dir != 0.0 {
                        self.last_scroll_dir = dir;
                    }
                    self.scroll_accel_boost
                } else {
                    self.scroll_accel_boost = 1.0;
                    1.0
                };

                let delta = scroll_delta * sign * self.config.mouse.scroll_speed * accel_mult;

                match wheel_action {
                    WheelAction::Scroll => {
                        let (_, min_y, max_y, _, _) =
                            self.tabs[self.active_tab].scroll_bounds(rect.size(), cont, gap);
                        if !cont {
                            let page_count = self.tabs[self.active_tab].doc.page_count;
                            let cur = self.tabs[self.active_tab].current_page;

                            if (max_y - min_y) <= 1.0 {
                                // Page fits vertically in the middle: 1 raw wheel tick = 1 page
                                if wheel_page_steps > 0 && cur + 1 < page_count {
                                    let next = (cur + wheel_page_steps as usize).min(page_count - 1);
                                    self.tabs[self.active_tab].goto_page_aligned(
                                        next,
                                        false,
                                        rect.size(),
                                        cont,
                                        gap,
                                    );
                                } else if wheel_page_steps < 0 && cur > 0 {
                                    let prev = cur.saturating_sub((-wheel_page_steps) as usize);
                                    self.tabs[self.active_tab].goto_page_aligned(
                                        prev,
                                        true,
                                        rect.size(),
                                        cont,
                                        gap,
                                    );
                                }
                            } else {
                                // Page is taller than viewport: scroll within page, then flip on a raw tick past top/bottom
                                let tab = &mut self.tabs[self.active_tab];
                                let at_bottom = tab.scroll_y <= min_y + 1.0;
                                let at_top = tab.scroll_y >= max_y - 1.0;

                                if wheel_page_steps > 0 && at_bottom && cur + 1 < page_count {
                                    tab.goto_page_aligned(
                                        cur + 1,
                                        false,
                                        rect.size(),
                                        cont,
                                        gap,
                                    );
                                } else if wheel_page_steps < 0 && at_top && cur > 0 {
                                    tab.goto_page_aligned(
                                        cur - 1,
                                        true,
                                        rect.size(),
                                        cont,
                                        gap,
                                    );
                                } else if scroll_delta != egui::Vec2::ZERO {
                                    tab.scroll_x += delta.x;
                                    tab.scroll_y += delta.y;
                                    tab.clamp_scroll(rect.size(), cont, gap);
                                }
                            }
                        } else if scroll_delta != egui::Vec2::ZERO {
                            let tab = &mut self.tabs[self.active_tab];
                            tab.scroll_x += delta.x;
                            tab.scroll_y -= delta.y;
                            tab.clamp_scroll(rect.size(), cont, gap);
                        }
                    }
                    WheelAction::HorizontalScroll => {
                        let tab = &mut self.tabs[self.active_tab];
                        tab.scroll_x += delta.y + delta.x;
                        tab.clamp_scroll(rect.size(), cont, gap);
                    }
                    WheelAction::Zoom => {
                        let z = self.tabs[self.active_tab].zoom;
                        let factor = (delta.y * 0.004).exp();
                        self.zoom_at(z * factor, anchor, rect.size());
                    }
                    WheelAction::Page => {
                        let cur = self.tabs[self.active_tab].current_page;
                        if wheel_page_steps > 0 {
                            self.goto_page(cur.saturating_add(wheel_page_steps as usize));
                        } else if wheel_page_steps < 0 {
                            self.goto_page(cur.saturating_sub((-wheel_page_steps) as usize));
                        }
                    }
                    WheelAction::None => {}
                }
            }
        }

        // Recompute visible pages after scroll/zoom updates
        let visible_pages = self.tabs[self.active_tab].compute_visible_pages(rect, cont, gap);

        let now = Instant::now();
        let is_zooming = match self.zoom_settle_deadline {
            Some(deadline) if now < deadline => {
                ui.ctx().request_repaint_after(deadline - now);
                true
            }
            Some(_) => {
                self.zoom_settle_deadline = None;
                false
            }
            None => false,
        };

        let ppp = ui.ctx().pixels_per_point();
        let render_scale = self.config.render_scale.clamp(0.5, 4.0);
        let max_render_w = self
            .config
            .max_render_width
            .clamp(512, HARD_MAX_RENDER_WIDTH_PX);
        let lcd_text = self.config.lcd_text_rendering;
        let use_gpu = self.wgpu_state.is_some();
        let theme = self.active_theme().clone();

        for &(page_idx, page_rect) in &visible_pages {
            let clamped_target_w = ((page_rect.width() * ppp * render_scale).round() as i32)
                .clamp(MIN_RENDER_WIDTH_PX, max_render_w);

            let tab = &mut self.tabs[self.active_tab];
            let should_render = if is_zooming && tab.cache.has_page(page_idx) {
                false
            } else {
                tab.cache
                    .needs_render(page_idx, clamped_target_w, &theme.name, use_gpu)
            };

            if should_render {
                if let Err(err) = tab.cache.render_into_cache(
                    &mut self.next_gpu_key,
                    &tab.doc,
                    page_idx,
                    clamped_target_w,
                    max_render_w,
                    lcd_text,
                    &theme,
                    ui.ctx(),
                    self.wgpu_state.as_ref(),
                ) {
                    self.status_error = Some(err.to_string());
                }
            } else {
                tab.cache.touch(page_idx);
            }
        }

        // Paint visible pages, links, selection, and search highlights
        let painter = ui.painter_at(rect);
        let tab = &self.tabs[self.active_tab];
        for &(page_idx, page_rect) in &visible_pages {
            if let Some(entry) = tab.cache.entries.get(&page_idx) {
                if let Some(gpu_key) = entry.gpu_key {
                    if let Some(cb) = paint_page_callback(page_rect, rect, gpu_key, &theme) {
                        painter.add(cb);
                    }
                } else if let Some((_, tex)) = &entry.cpu_texture {
                    painter.image(
                        tex.id(),
                        page_rect,
                        egui::Rect::from_min_max(egui::pos2(0.0, 0.0), egui::pos2(1.0, 1.0)),
                        egui::Color32::WHITE,
                    );
                }
            }

            if let Some((hl_page, ref link, _)) = hovered_link
                && hl_page == page_idx
            {
                let lr = egui::Rect::from_min_max(
                    egui::pos2(
                        page_rect.min.x + link.rect.left * tab.zoom,
                        page_rect.min.y + link.rect.top * tab.zoom,
                    ),
                    egui::pos2(
                        page_rect.min.x + link.rect.right * tab.zoom,
                        page_rect.min.y + link.rect.bottom * tab.zoom,
                    ),
                );
                painter.rect_filled(
                    lr,
                    2.0,
                    egui::Color32::from_rgba_unmultiplied(80, 160, 255, 45),
                );
            }

            if let Some((img_page, ref img)) = tab.selected_image
                && img_page == page_idx
            {
                let ir = egui::Rect::from_min_max(
                    egui::pos2(
                        page_rect.min.x + img.rect.left * tab.zoom,
                        page_rect.min.y + img.rect.top * tab.zoom,
                    ),
                    egui::pos2(
                        page_rect.min.x + img.rect.right * tab.zoom,
                        page_rect.min.y + img.rect.bottom * tab.zoom,
                    ),
                );
                painter.rect_filled(
                    ir,
                    2.0,
                    egui::Color32::from_rgba_unmultiplied(70, 140, 255, 45),
                );
                painter.rect_stroke(
                    ir,
                    2.0,
                    egui::Stroke::new(2.0, egui::Color32::from_rgb(70, 140, 255)),
                    egui::StrokeKind::Outside,
                );
            }

            if let Some(sel) = &tab.selection
                && sel.page_index == page_idx
            {
                let sel_fill = egui::Color32::from_rgba_unmultiplied(70, 140, 255, 95);
                for r in &sel.rects {
                    let sr = egui::Rect::from_min_max(
                        egui::pos2(
                            page_rect.min.x + r.left * tab.zoom,
                            page_rect.min.y + r.top * tab.zoom,
                        ),
                        egui::pos2(
                            page_rect.min.x + r.right * tab.zoom,
                            page_rect.min.y + r.bottom * tab.zoom,
                        ),
                    );
                    painter.rect_filled(sr, 1.0, sel_fill);
                }
            }

            if !tab.search_matches.is_empty() {
                for (match_idx, m) in tab.search_matches.iter().enumerate() {
                    if m.page_index != page_idx {
                        continue;
                    }
                    let is_active = match_idx == tab.search_active_idx;
                    let fill = if is_active {
                        egui::Color32::from_rgba_unmultiplied(255, 140, 0, 115)
                    } else {
                        egui::Color32::from_rgba_unmultiplied(255, 230, 0, 85)
                    };
                    for r in &m.rects {
                        let hl = egui::Rect::from_min_max(
                            egui::pos2(
                                page_rect.min.x + r.left * tab.zoom,
                                page_rect.min.y + r.top * tab.zoom,
                            ),
                            egui::pos2(
                                page_rect.min.x + r.right * tab.zoom,
                                page_rect.min.y + r.bottom * tab.zoom,
                            ),
                        );
                        painter.rect_filled(hl, 2.0, fill);
                    }
                }
            }
        }

        let _ = self.draw_scrollbars(ui, rect);

        // Pre-render at most 1 adjacent page when idle
        if !is_zooming
            && let (Some(&(first_vis, _)), Some(&(last_vis, _))) =
                (visible_pages.first(), visible_pages.last())
        {
            let tab = &mut self.tabs[self.active_tab];
            let candidates = [
                (last_vis + 1 < tab.doc.page_count).then_some(last_vis + 1),
                first_vis.checked_sub(1),
            ];
            for adj in candidates.into_iter().flatten() {
                let adj_w = ((tab.doc.page_size_pt(adj).0 * tab.zoom * ppp * render_scale).round()
                    as i32)
                    .clamp(MIN_RENDER_WIDTH_PX, max_render_w);
                if tab.cache.needs_render(adj, adj_w, &theme.name, use_gpu) {
                    let _ = tab.cache.render_into_cache(
                        &mut self.next_gpu_key,
                        &tab.doc,
                        adj,
                        adj_w,
                        max_render_w,
                        lcd_text,
                        &theme,
                        ui.ctx(),
                        self.wgpu_state.as_ref(),
                    );
                    break;
                }
            }
        }
    }
}

fn scroll_tab_to_active_match(
    tab: &mut TabState,
    canvas_size: egui::Vec2,
    continuous_scroll: bool,
    page_gap: f32,
) {
    let Some(m) = tab.search_matches.get(tab.search_active_idx).cloned() else {
        return;
    };
    tab.current_page = m.page_index;
    if continuous_scroll {
        let page_y = tab.page_top_y(m.page_index, page_gap);
        let match_y_in_page = m.rects.first().map_or(0.0, |r| r.top * tab.zoom);
        let target_y = page_y + match_y_in_page - canvas_size.y * 0.35;
        tab.scroll_y = target_y.max(0.0);
    } else {
        tab.scroll_x = 0.0;
        tab.scroll_y = 0.0;
    }
    tab.clamp_scroll(canvas_size, continuous_scroll, page_gap);
}

fn truncate_tab_title(title: &str, max_chars: usize) -> String {
    let count = title.chars().count();
    if count <= max_chars {
        title.to_string()
    } else {
        let mut s: String = title.chars().take(max_chars.saturating_sub(1)).collect();
        s.push('…');
        s
    }
}

fn drag_action_row(ui: &mut egui::Ui, label: &str, id: &str, value: &mut DragAction) {
    ui.label(label);
    egui::ComboBox::from_id_salt(id)
        .selected_text(value.label())
        .show_ui(ui, |ui| {
            for act in DragAction::ALL {
                ui.selectable_value(value, act, act.label());
            }
        });
    ui.end_row();
}

fn side_button_row(ui: &mut egui::Ui, label: &str, id: &str, value: &mut SideButtonAction) {
    ui.label(label);
    egui::ComboBox::from_id_salt(id)
        .selected_text(value.label())
        .show_ui(ui, |ui| {
            for act in SideButtonAction::ALL {
                ui.selectable_value(value, act, act.label());
            }
        });
    ui.end_row();
}

fn wheel_action_row(ui: &mut egui::Ui, label: &str, id: &str, value: &mut WheelAction) {
    ui.label(label);
    egui::ComboBox::from_id_salt(id)
        .selected_text(value.label())
        .show_ui(ui, |ui| {
            for act in WheelAction::ALL {
                ui.selectable_value(value, act, act.label());
            }
        });
    ui.end_row();
}

fn draw_bookmark_node(
    ui: &mut egui::Ui,
    node: &BookmarkNode,
    idx: usize,
    jump_to: &mut Option<usize>,
) {
    if node.children.is_empty() {
        let label = match node.page_index {
            Some(p) => format!("{}  (p.{})", node.title, p + 1),
            None => node.title.clone(),
        };
        if ui.selectable_label(false, label).clicked()
            && let Some(p) = node.page_index
        {
            *jump_to = Some(p);
        }
    } else {
        egui::CollapsingHeader::new(&node.title)
            .id_salt(("bm", idx, &node.title))
            .default_open(false)
            .show(ui, |ui| {
                if let Some(p) = node.page_index
                    && ui
                        .selectable_label(false, format!("Go to p.{}", p + 1))
                        .clicked()
                {
                    *jump_to = Some(p);
                }
                for (ci, child) in node.children.iter().enumerate() {
                    draw_bookmark_node(ui, child, ci, jump_to);
                }
            });
    }
}

impl eframe::App for PdfReaderApp {
    fn ui(&mut self, ui: &mut egui::Ui, _frame: &mut eframe::Frame) {
        let ctx = ui.ctx().clone();
        self.handle_dropped_files(&ctx);
        self.process_shortcuts(&ctx);

        if self.show_toolbar {
            egui::Panel::top("toolbar").show(ui, |ui| {
                self.draw_toolbar(ui);
            });
        }

        if self.should_show_tab_bar() {
            egui::Panel::top("tab_bar").show(ui, |ui| {
                self.draw_tab_bar(ui, &ctx);
            });
        }

        if self.search_open {
            egui::Panel::top("search_bar").show(ui, |ui| {
                self.draw_search_bar(ui);
            });
        }

        if self.show_toc {
            egui::Panel::left("toc_panel")
                .resizable(true)
                .default_size(240.0)
                .show(ui, |ui| {
                    self.draw_toc_panel(ui);
                });
        } else if self.show_thumbnails {
            egui::Panel::left("thumb_panel")
                .resizable(true)
                .default_size(175.0)
                .show(ui, |ui| {
                    self.draw_thumbnails_panel(ui);
                });
        }

        egui::CentralPanel::default()
            .frame(egui::Frame::NONE)
            .show(ui, |ui| {
                self.draw_canvas(ui);
            });

        self.draw_goto_window(&ctx);
        self.draw_settings_window(&ctx);
        self.draw_context_menu(&ctx);
        self.draw_toast(&ctx);
    }
}
