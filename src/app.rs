use std::collections::HashMap;
use std::path::PathBuf;
use std::time::{Duration, Instant};

use eframe::egui;
use eframe::egui_wgpu;

use crate::config::{
    Action, Config, DefaultFitMode, DoubleClickAction, DragAction, KeyBinding, SideButtonAction,
    WheelAction,
};
use crate::pdf::{
    BookmarkNode, LinkTarget, MAX_RENDER_WIDTH_PX, MatchRectPt, OpenDocument, PageLink,
    SearchMatch, init_pdfium,
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
}

struct TextSelection {
    page_index: usize,
    anchor_char: usize,
    head_char: usize,
    rects: Vec<MatchRectPt>,
    text: String,
}

struct CachedPageEntry {
    rendered_width_px: i32,
    last_used_frame: u64,
    gpu_key: Option<u64>,
    cpu_texture: Option<(String, egui::TextureHandle)>,
}

struct PageCache {
    entries: HashMap<usize, CachedPageEntry>,
    next_gpu_key: u64,
    frame_counter: u64,
}

impl PageCache {
    fn new() -> Self {
        Self {
            entries: HashMap::new(),
            next_gpu_key: 1,
            frame_counter: 0,
        }
    }

    fn begin_frame(&mut self) {
        self.frame_counter = self.frame_counter.wrapping_add(1);
    }

    fn clear(&mut self, wgpu_state: Option<&egui_wgpu::RenderState>) {
        self.entries.clear();
        if let Some(rs) = wgpu_state {
            PdfShaderResources::clear_pages(rs);
        }
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

    fn render_into_cache(
        &mut self,
        doc: &OpenDocument,
        page_index: usize,
        clamped_target_width_px: i32,
        theme: &ResolvedTheme,
        ctx: &egui::Context,
        wgpu_state: Option<&egui_wgpu::RenderState>,
    ) -> anyhow::Result<()> {
        let mut rendered = doc.render_page(page_index, clamped_target_width_px)?;

        if let Some(old) = self.entries.remove(&page_index)
            && let (Some(rs), Some(old_key)) = (wgpu_state, old.gpu_key)
        {
            PdfShaderResources::remove_page(rs, old_key);
        }

        let entry = if let Some(rs) = wgpu_state {
            let gpu_key = self.next_gpu_key;
            self.next_gpu_key = self.next_gpu_key.wrapping_add(1);
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

pub struct PdfReaderApp {
    config: Config,
    config_path: Option<PathBuf>,
    bindings: Vec<KeyBinding>,
    themes: Vec<ResolvedTheme>,
    active_theme_idx: usize,
    wgpu_state: Option<egui_wgpu::RenderState>,

    doc: Option<OpenDocument>,
    status_error: Option<String>,

    current_page: usize,
    nav_back_stack: Vec<usize>,
    nav_forward_stack: Vec<usize>,
    zoom: f32,
    zoom_settle_deadline: Option<Instant>,
    fit_mode: FitMode,
    continuous_scroll: bool,
    scroll_x: f32,
    scroll_y: f32,
    last_canvas_size: egui::Vec2,

    show_toolbar: bool,
    show_toc: bool,
    show_thumbnails: bool,

    // Fast navigation state
    toolbar_page_input: String,
    goto_open: bool,
    goto_focus_requested: bool,
    goto_input: String,
    thumb_cache: HashMap<usize, egui::TextureHandle>,
    last_thumb_synced_page: Option<usize>,

    // Scrollbar drag state (survives pointer leaving window)
    dragging_v_scrollbar: bool,
    dragging_h_scrollbar: bool,

    // Settings popup state
    settings_open: bool,
    settings_tab: SettingsTab,
    settings_status: Option<String>,

    // Search state
    search_open: bool,
    search_focus_requested: bool,
    search_query: String,
    search_last_executed: String,
    search_match_case: bool,
    search_matches: Vec<SearchMatch>,
    search_active_idx: usize,

    // Interaction state (links & text selection)
    links_cache: HashMap<usize, Vec<PageLink>>,
    drag_mode: DragMode,
    drag_button: Option<egui::PointerButton>,
    pan_start_pointer: Option<egui::Pos2>,
    pan_start_scroll: egui::Vec2,
    selection: Option<TextSelection>,

    cache: PageCache,
}

impl PdfReaderApp {
    pub fn new(
        cc: &eframe::CreationContext<'_>,
        config: Config,
        config_path: Option<PathBuf>,
        initial_file: Option<PathBuf>,
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
        let fit_mode = FitMode::from(config.default_fit_mode);
        let status_error = init_pdfium().err().map(|e| e.to_string());

        let mut app = Self {
            config,
            config_path,
            bindings,
            themes,
            active_theme_idx,
            wgpu_state,
            doc: None,
            status_error,
            current_page: 0,
            nav_back_stack: Vec::new(),
            nav_forward_stack: Vec::new(),
            zoom: 1.0,
            zoom_settle_deadline: None,
            fit_mode,
            continuous_scroll,
            scroll_x: 0.0,
            scroll_y: 0.0,
            last_canvas_size: egui::vec2(1000.0, 800.0),
            show_toolbar,
            show_toc: false,
            show_thumbnails: false,
            toolbar_page_input: String::new(),
            goto_open: false,
            goto_focus_requested: false,
            goto_input: String::new(),
            thumb_cache: HashMap::new(),
            last_thumb_synced_page: None,
            dragging_v_scrollbar: false,
            dragging_h_scrollbar: false,
            settings_open: false,
            settings_tab: SettingsTab::Mouse,
            settings_status: None,
            search_open: false,
            search_focus_requested: false,
            search_query: String::new(),
            search_last_executed: String::new(),
            search_match_case: false,
            search_matches: Vec::new(),
            search_active_idx: 0,
            links_cache: HashMap::new(),
            drag_mode: DragMode::None,
            drag_button: None,
            pan_start_pointer: None,
            pan_start_scroll: egui::Vec2::ZERO,
            selection: None,
            cache: PageCache::new(),
        };

        if let Some(path) = initial_file {
            app.open_file(path, initial_page);
        }

        app
    }

    fn active_theme(&self) -> &ResolvedTheme {
        &self.themes[self.active_theme_idx]
    }

    fn page_count(&self) -> usize {
        self.doc.as_ref().map_or(0, |d| d.page_count)
    }

    fn open_file(&mut self, path: PathBuf, initial_page: Option<usize>) {
        match OpenDocument::open(&path) {
            Ok(doc) => {
                let target_page = initial_page
                    .unwrap_or(0)
                    .min(doc.page_count.saturating_sub(1));
                self.doc = Some(doc);
                self.cache.clear(self.wgpu_state.as_ref());
                self.thumb_cache.clear();
                self.last_thumb_synced_page = None;
                self.links_cache.clear();
                self.nav_back_stack.clear();
                self.nav_forward_stack.clear();
                self.selection = None;
                self.drag_mode = DragMode::None;
                self.drag_button = None;
                self.pan_start_pointer = None;
                self.search_matches.clear();
                self.search_last_executed.clear();
                self.search_active_idx = 0;
                self.scroll_x = 0.0;
                self.scroll_y = 0.0;
                self.status_error = None;
                self.goto_page(target_page);
            }
            Err(err) => {
                self.status_error = Some(err.to_string());
            }
        }
    }

    fn pick_and_open_file(&mut self) {
        if let Some(path) = rfd::FileDialog::new()
            .add_filter("PDF Documents", &["pdf", "PDF"])
            .pick_file()
        {
            self.open_file(path, None);
        }
    }

    fn page_top_y(&self, page_index: usize) -> f32 {
        let Some(doc) = &self.doc else {
            return CANVAS_MARGIN;
        };
        let mut y = CANVAS_MARGIN;
        for i in 0..page_index.min(doc.page_count) {
            y += doc.page_size_pt(i).1 * self.zoom + self.config.page_gap;
        }
        y
    }

    fn total_doc_height(&self) -> f32 {
        let Some(doc) = &self.doc else {
            return 0.0;
        };
        if doc.page_count == 0 {
            return 0.0;
        }
        let mut h = CANVAS_MARGIN * 2.0;
        for i in 0..doc.page_count {
            h += doc.page_size_pt(i).1 * self.zoom;
            if i + 1 < doc.page_count {
                h += self.config.page_gap;
            }
        }
        h
    }

    /// Returns `(max_scroll_x, min_scroll_y, max_scroll_y, total_w, total_h)` for the current document.
    fn scroll_bounds(&self, canvas_size: egui::Vec2) -> (f32, f32, f32, f32, f32) {
        let Some(doc) = &self.doc else {
            return (0.0, 0.0, 0.0, canvas_size.x, canvas_size.y);
        };

        let max_page_w_pt = if self.continuous_scroll {
            doc.page_sizes_pt
                .iter()
                .map(|(w, _)| *w)
                .fold(0.0_f32, f32::max)
        } else {
            doc.page_size_pt(self.current_page).0
        };

        let total_w = max_page_w_pt * self.zoom + CANVAS_MARGIN * 2.0;
        let max_x = (total_w - canvas_size.x).max(0.0) * 0.5;

        if self.continuous_scroll {
            let total_h = self.total_doc_height();
            let max_y = (total_h - canvas_size.y).max(0.0);
            (max_x, 0.0, max_y, total_w, total_h)
        } else {
            let total_h = doc.page_size_pt(self.current_page).1 * self.zoom + CANVAS_MARGIN * 2.0;
            let max_y = (total_h - canvas_size.y).max(0.0) * 0.5;
            (max_x, -max_y, max_y, total_w, total_h)
        }
    }

    fn clamp_scroll(&mut self, canvas_size: egui::Vec2) {
        let (max_x, min_y, max_y, _, _) = self.scroll_bounds(canvas_size);
        self.scroll_x = self.scroll_x.clamp(-max_x, max_x);
        self.scroll_y = self.scroll_y.clamp(min_y, max_y);
    }

    fn goto_page(&mut self, index: usize) {
        let count = self.page_count();
        if count == 0 {
            return;
        }
        let clamped = index.min(count - 1);
        self.current_page = clamped;
        if self.continuous_scroll {
            self.scroll_y = (self.page_top_y(clamped) - CANVAS_MARGIN).max(0.0);
        } else {
            self.scroll_x = 0.0;
            self.scroll_y = 0.0;
        }
        self.clamp_scroll(self.last_canvas_size);
    }

    fn jump_via_link(&mut self, target_page: usize) {
        if target_page != self.current_page {
            self.nav_back_stack.push(self.current_page);
            if self.nav_back_stack.len() > 64 {
                self.nav_back_stack.remove(0);
            }
            self.nav_forward_stack.clear();
        }
        self.goto_page(target_page);
    }

    fn navigate_back(&mut self) {
        if let Some(prev) = self.nav_back_stack.pop() {
            self.nav_forward_stack.push(self.current_page);
            self.goto_page(prev);
        }
    }

    fn navigate_forward(&mut self) {
        if let Some(next) = self.nav_forward_stack.pop() {
            self.nav_back_stack.push(self.current_page);
            self.goto_page(next);
        }
    }

    fn zoom_at(&mut self, new_zoom: f32, anchor_in_canvas: egui::Vec2, canvas_size: egui::Vec2) {
        let old_zoom = self.zoom;
        let clamped = new_zoom.clamp(0.1, 10.0);
        if (clamped - old_zoom).abs() < 1e-4 {
            return;
        }
        self.fit_mode = FitMode::Manual;
        self.zoom_settle_deadline = Some(Instant::now() + ZOOM_SETTLE_DELAY);
        let ratio = clamped / old_zoom;

        let dx_from_center = anchor_in_canvas.x - canvas_size.x * 0.5;
        self.scroll_x = dx_from_center - (dx_from_center - self.scroll_x) * ratio;

        if self.continuous_scroll {
            let doc_y = self.scroll_y + anchor_in_canvas.y;
            self.zoom = clamped;
            self.scroll_y = doc_y * ratio - anchor_in_canvas.y;
        } else {
            let dy_from_center = anchor_in_canvas.y - canvas_size.y * 0.5;
            self.scroll_y = dy_from_center - (dy_from_center - self.scroll_y) * ratio;
            self.zoom = clamped;
        }
        self.clamp_scroll(canvas_size);
    }

    fn handle_side_button(&mut self, action: SideButtonAction) {
        let center = self.last_canvas_size * 0.5;
        let canvas_size = self.last_canvas_size;
        match action {
            SideButtonAction::NavigateBack => self.navigate_back(),
            SideButtonAction::NavigateForward => self.navigate_forward(),
            SideButtonAction::PrevPage => self.goto_page(self.current_page.saturating_sub(1)),
            SideButtonAction::NextPage => self.goto_page(self.current_page.saturating_add(1)),
            SideButtonAction::ZoomOut => {
                self.zoom_at(self.zoom / self.config.mouse.zoom_step, center, canvas_size);
            }
            SideButtonAction::ZoomIn => {
                self.zoom_at(self.zoom * self.config.mouse.zoom_step, center, canvas_size);
            }
            SideButtonAction::CycleTheme => {
                self.active_theme_idx = (self.active_theme_idx + 1) % self.themes.len();
                self.config.theme = self.active_theme().name.clone();
            }
            SideButtonAction::ToggleToc => {
                self.show_toc = !self.show_toc;
            }
            SideButtonAction::None => {}
        }
    }

    fn run_search(&mut self) {
        self.search_last_executed = self.search_query.clone();
        self.search_active_idx = 0;
        if let Some(doc) = &self.doc {
            self.search_matches = doc.search(&self.search_query, self.search_match_case);
            if !self.search_matches.is_empty() {
                self.scroll_to_active_match();
            }
        } else {
            self.search_matches.clear();
        }
    }

    fn step_search(&mut self, forward: bool) {
        if self.search_matches.is_empty() {
            if !self.search_query.trim().is_empty() {
                self.run_search();
            }
            return;
        }
        let len = self.search_matches.len();
        self.search_active_idx = if forward {
            (self.search_active_idx + 1) % len
        } else {
            (self.search_active_idx + len - 1) % len
        };
        self.scroll_to_active_match();
    }

    fn scroll_to_active_match(&mut self) {
        let Some(m) = self.search_matches.get(self.search_active_idx).cloned() else {
            return;
        };
        self.current_page = m.page_index;
        if self.continuous_scroll {
            let page_y = self.page_top_y(m.page_index);
            let match_y_in_page = m.rects.first().map_or(0.0, |r| r.top * self.zoom);
            let target_y = page_y + match_y_in_page - self.last_canvas_size.y * 0.35;
            self.scroll_y = target_y.max(0.0);
        } else {
            self.scroll_x = 0.0;
            self.scroll_y = 0.0;
        }
        self.clamp_scroll(self.last_canvas_size);
    }

    fn handle_action(&mut self, action: Action, ctx: &egui::Context) {
        let center = self.last_canvas_size * 0.5;
        let canvas_size = self.last_canvas_size;

        match action {
            Action::NextPage => self.goto_page(self.current_page.saturating_add(1)),
            Action::PrevPage => self.goto_page(self.current_page.saturating_sub(1)),
            Action::FirstPage => self.goto_page(0),
            Action::LastPage => self.goto_page(self.page_count().saturating_sub(1)),
            Action::ZoomIn => {
                let target = self.zoom * self.config.mouse.zoom_step;
                self.zoom_at(target, center, canvas_size);
            }
            Action::ZoomOut => {
                let target = self.zoom / self.config.mouse.zoom_step;
                self.zoom_at(target, center, canvas_size);
            }
            Action::ZoomReset => {
                self.zoom_at(1.0, center, canvas_size);
                self.scroll_x = 0.0;
                self.clamp_scroll(canvas_size);
            }
            Action::FitWidth => {
                self.fit_mode = FitMode::FitWidth;
                self.scroll_x = 0.0;
            }
            Action::FitPage => {
                self.fit_mode = FitMode::FitPage;
                self.scroll_x = 0.0;
                if !self.continuous_scroll {
                    self.scroll_y = 0.0;
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
                    self.last_thumb_synced_page = None;
                }
            }
            Action::ToggleContinuous => {
                let page = self.current_page;
                self.continuous_scroll = !self.continuous_scroll;
                self.config.continuous_scroll = self.continuous_scroll;
                self.goto_page(page);
            }
            Action::GotoPage => {
                self.goto_open = true;
                self.goto_focus_requested = true;
                self.goto_input = (self.current_page + 1).to_string();
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
            Action::OpenFile => self.pick_and_open_file(),
            Action::Quit => {
                ctx.send_viewport_cmd(egui::ViewportCommand::Close);
            }
        }
    }

    fn process_shortcuts(&mut self, ctx: &egui::Context) {
        let typing = ctx.egui_wants_keyboard_input();

        // Mouse side buttons (Back / Forward) work anywhere in the window
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

        // Copy selected text with Ctrl+C / Cmd+C when not typing in an input box
        if !typing
            && let Some(sel) = &self.selection
            && !sel.text.is_empty()
            && ctx.input(|i| i.modifiers.command && i.key_pressed(egui::Key::C))
        {
            ctx.copy_text(sel.text.clone());
            return;
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
            if self.selection.is_some() {
                self.selection = None;
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
        let dropped: Option<PathBuf> = ctx.input(|i| {
            i.raw
                .dropped_files
                .first()
                .map(|f| f.path().to_path_buf())
        });
        if let Some(path) = dropped {
            self.open_file(path, None);
        }
    }

    fn ensure_links_loaded(&mut self, page_index: usize) {
        if self.links_cache.contains_key(&page_index) {
            return;
        }
        if let Some(doc) = &self.doc {
            let links = doc.page_links(page_index);
            self.links_cache.insert(page_index, links);
        }
    }

    fn draw_toolbar(&mut self, ui: &mut egui::Ui) {
        ui.horizontal(|ui| {
            if ui.button("Open").clicked() {
                self.pick_and_open_file();
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
                    self.last_thumb_synced_page = None;
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

            if !self.nav_back_stack.is_empty()
                && ui
                    .button("Back")
                    .on_hover_text("Return to previous page (Alt+Left)")
                    .clicked()
            {
                self.navigate_back();
            }

            if !self.nav_forward_stack.is_empty()
                && ui
                    .button("Fwd")
                    .on_hover_text("Go forward in history (Alt+Right)")
                    .clicked()
            {
                self.navigate_forward();
            }

            ui.separator();

            if ui.button("<").clicked() {
                self.goto_page(self.current_page.saturating_sub(1));
            }
            let count = self.page_count();
            if count > 0 {
                let page_edit = ui.add(
                    egui::TextEdit::singleline(&mut self.toolbar_page_input).desired_width(38.0),
                );
                if !page_edit.has_focus() {
                    self.toolbar_page_input = (self.current_page + 1).to_string();
                }
                if page_edit.lost_focus() && ui.input(|i| i.key_pressed(egui::Key::Enter)) {
                    self.jump_from_input(&self.toolbar_page_input.clone());
                }
                ui.label(format!("/ {count}"))
                    .on_hover_text("Type a page number or press Ctrl+G to jump");
            } else {
                ui.label("No document");
            }
            if ui.button(">").clicked() {
                self.goto_page(self.current_page.saturating_add(1));
            }

            ui.separator();

            let center = self.last_canvas_size * 0.5;
            let canvas_size = self.last_canvas_size;
            if ui.button("-").clicked() {
                self.zoom_at(self.zoom / self.config.mouse.zoom_step, center, canvas_size);
            }
            ui.label(format!("{:.0}%", self.zoom * 100.0));
            if ui.button("+").clicked() {
                self.zoom_at(self.zoom * self.config.mouse.zoom_step, center, canvas_size);
            }
            if ui.button("Fit W").clicked() {
                self.fit_mode = FitMode::FitWidth;
                self.scroll_x = 0.0;
            }
            if ui.button("Fit P").clicked() {
                self.fit_mode = FitMode::FitPage;
                self.scroll_x = 0.0;
            }
            if ui
                .selectable_label(self.continuous_scroll, "Cont")
                .on_hover_text("Toggle continuous scroll (c)")
                .clicked()
            {
                let page = self.current_page;
                self.continuous_scroll = !self.continuous_scroll;
                self.config.continuous_scroll = self.continuous_scroll;
                self.goto_page(page);
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
                    .on_hover_text("Configure mouse and general settings (Ctrl+,)")
                    .clicked()
                {
                    self.settings_open = !self.settings_open;
                    self.settings_status = None;
                }

                if let Some(doc) = &self.doc
                    && let Some(name) = doc.path.file_name().and_then(|n| n.to_str())
                {
                    ui.weak(name);
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
            .default_width(440.0)
            .show(ctx, |ui| {
                ui.horizontal(|ui| {
                    ui.selectable_value(&mut self.settings_tab, SettingsTab::General, "General");
                    ui.selectable_value(&mut self.settings_tab, SettingsTab::Mouse, "Mouse");
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
                                            {
                                                self.fit_mode = FitMode::from(mode);
                                            }
                                        }
                                    });
                                ui.end_row();

                                ui.label("Continuous Scroll:");
                                let prev_cont = self.config.continuous_scroll;
                                ui.checkbox(&mut self.config.continuous_scroll, "");
                                if self.config.continuous_scroll != prev_cont {
                                    let page = self.current_page;
                                    self.continuous_scroll = self.config.continuous_scroll;
                                    self.goto_page(page);
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
                                        0.2..=4.0,
                                    )
                                    .fixed_decimals(2),
                                );
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
                        self.config.continuous_scroll = defaults.continuous_scroll;
                        self.config.page_gap = defaults.page_gap;
                        self.config.show_toolbar = defaults.show_toolbar;
                        self.config.show_scrollbars = defaults.show_scrollbars;
                        self.config.default_fit_mode = defaults.default_fit_mode;
                        self.continuous_scroll = self.config.continuous_scroll;
                        self.fit_mode = FitMode::from(self.config.default_fit_mode);
                        self.settings_status = Some("Reset to defaults (click Save to persist)".into());
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

    /// Parse either a 1-based page number (`"42"`) or a percentage (`"50%"`) and jump there.
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
                    let mut p = self.current_page + 1;
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

    fn draw_thumbnails_panel(&mut self, ui: &mut egui::Ui) {
        ui.heading("Pages");
        ui.separator();

        let count = self.page_count();
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
            .id_salt("thumb_scroll")
            .auto_shrink([false, false]);

        // Auto-scroll thumbnail sidebar to keep the highlighted page visible whenever current_page changes
        if self.last_thumb_synced_page != Some(self.current_page) {
            let avail_h = ui.available_height().max(row_h);
            let target_y =
                (self.current_page as f32 * row_h - avail_h * 0.5 + row_h * 0.5).max(0.0);
            scroll_area = scroll_area.vertical_scroll_offset(target_y);
            self.last_thumb_synced_page = Some(self.current_page);
        }

        scroll_area.show_rows(ui, row_h, count, |ui, row_range| {
                for page_idx in row_range {
                    // Lazily render at most 2 visible thumbnails per frame so opening stays instant
                    if !self.thumb_cache.contains_key(&page_idx) && rendered_this_frame < 2 {
                        if let Some(doc) = &self.doc
                            && let Ok(rendered) = doc.render_page(page_idx, 160)
                        {
                            let color_img = egui::ColorImage::from_rgba_unmultiplied(
                                [rendered.width, rendered.height],
                                &rendered.rgba,
                            );
                            let tex = ctx.load_texture(
                                format!("pdf_thumb_{page_idx}"),
                                color_img,
                                egui::TextureOptions::LINEAR,
                            );
                            self.thumb_cache.insert(page_idx, tex);
                            rendered_this_frame += 1;
                            ctx.request_repaint();
                        }
                    }

                    let is_current = page_idx == self.current_page;
                    let (pw, ph) = self
                        .doc
                        .as_ref()
                        .map_or((612.0, 792.0), |d| d.page_size_pt(page_idx));
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
                                if let Some(tex) = self.thumb_cache.get(&page_idx) {
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

                        let label_resp = ui.selectable_label(is_current, format!("{}", page_idx + 1));
                        if label_resp.clicked() {
                            jump_to = Some(page_idx);
                        }
                    });
                }
            });

        if let Some(page) = jump_to {
            self.goto_page(page);
        }
    }

    /// Draw overlay vertical and horizontal scrollbars on the canvas edges.
    /// Returns `true` if a scrollbar is currently hovered or being dragged.
    fn draw_scrollbars(&mut self, ui: &mut egui::Ui, rect: egui::Rect) -> bool {
        if !self.config.show_scrollbars || self.doc.is_none() {
            return false;
        }

        let (max_x, min_y, max_y, total_w, total_h) = self.scroll_bounds(rect.size());
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

        // Vertical scrollbar (right edge)
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
                    self.scroll_y = min_y + t * (max_y - min_y);
                    self.clamp_scroll(rect.size());
                }

                let t = ((self.scroll_y - min_y) / (max_y - min_y).max(1.0)).clamp(0.0, 1.0);
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

                // Floating page indicator badge while dragging the vertical scrollbar
                if is_dragging {
                    let badge_text = format!("Page {} / {}", self.current_page + 1, self.page_count());
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
                    painter.galley(badge_rect.min + egui::vec2(7.0, 4.0), galley, egui::Color32::WHITE);
                }
            }
        }

        // Horizontal scrollbar (bottom edge)
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

                // Note: positive scroll_x shifts page right (revealing left side), so map t=0 -> +max_x, t=1 -> -max_x
                if (is_dragging || resp.clicked())
                    && let Some(pos) = resp.interact_pointer_pos().or(latest_pos)
                {
                    let t = ((pos.x - track_rect.min.x - thumb_w * 0.5) / travel).clamp(0.0, 1.0);
                    self.scroll_x = max_x - t * (2.0 * max_x);
                    self.clamp_scroll(rect.size());
                }

                let t = ((max_x - self.scroll_x) / (2.0 * max_x).max(1.0)).clamp(0.0, 1.0);
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

    fn draw_search_bar(&mut self, ui: &mut egui::Ui) {
        ui.horizontal(|ui| {
            ui.label("Find:");
            let resp = ui.add(
                egui::TextEdit::singleline(&mut self.search_query)
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
                if self.search_query != self.search_last_executed {
                    self.run_search();
                } else {
                    self.step_search(!shift_held);
                }
                resp.request_focus();
            }

            if ui.button("Search").clicked() {
                self.run_search();
            }
            if ui.button("Prev").clicked() {
                self.step_search(false);
            }
            if ui.button("Next").clicked() {
                self.step_search(true);
            }

            let prev_case = self.search_match_case;
            ui.checkbox(&mut self.search_match_case, "Aa")
                .on_hover_text("Match case");
            if self.search_match_case != prev_case && !self.search_query.trim().is_empty() {
                self.run_search();
            }

            if !self.search_matches.is_empty() {
                ui.label(format!(
                    "{} / {}",
                    self.search_active_idx + 1,
                    self.search_matches.len()
                ));
            } else if !self.search_last_executed.trim().is_empty() {
                ui.weak("No matches");
            }

            ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                if ui.button("x").clicked() {
                    self.search_open = false;
                }
            });
        });
    }

    fn draw_toc_panel(&mut self, ui: &mut egui::Ui) {
        ui.heading("Contents");
        ui.separator();

        let bookmarks = match &self.doc {
            Some(doc) => doc.bookmarks.clone(),
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

    fn compute_visible_pages(&mut self, rect: egui::Rect) -> Vec<(usize, egui::Rect)> {
        let mut visible_pages = Vec::new();
        let page_count = self.page_count();
        if page_count == 0 {
            return visible_pages;
        }

        if self.continuous_scroll {
            let mut y = CANVAS_MARGIN;
            let view_top = self.scroll_y;
            let view_bottom = self.scroll_y + rect.height();
            let view_mid = self.scroll_y + rect.height() * 0.5;
            let mut center_page = self.current_page;

            for i in 0..page_count {
                let (pw, ph) = self.doc.as_ref().unwrap().page_size_pt(i);
                let disp_w = pw * self.zoom;
                let disp_h = ph * self.zoom;
                let page_bottom = y + disp_h;

                if y <= view_mid && view_mid <= page_bottom + self.config.page_gap {
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

                y = page_bottom + self.config.page_gap;
                if y > view_bottom {
                    break;
                }
            }
            self.current_page = center_page;
        } else {
            let (pw, ph) = self.doc.as_ref().unwrap().page_size_pt(self.current_page);
            let disp_size = egui::vec2(pw * self.zoom, ph * self.zoom);
            let center = rect.center() + egui::vec2(self.scroll_x, self.scroll_y);
            visible_pages.push((
                self.current_page,
                egui::Rect::from_center_size(center, disp_size),
            ));
        }

        visible_pages
    }

    fn draw_canvas(&mut self, ui: &mut egui::Ui) {
        self.cache.begin_frame();
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

        let Some(doc) = &self.doc else {
            ui.painter().text(
                rect.center(),
                egui::Align2::CENTER_CENTER,
                "Drop a PDF here or press Ctrl+O to open",
                egui::FontId::proportional(18.0),
                egui::Color32::from_gray(180),
            );
            return;
        };
        if doc.page_count == 0 {
            return;
        }

        // Fit mode recalculation using current page's dimensions
        let avail_w = (rect.width() - CANVAS_MARGIN * 2.0).max(64.0);
        let avail_h = (rect.height() - CANVAS_MARGIN * 2.0).max(64.0);
        let (cur_pw, cur_ph) = doc.page_size_pt(self.current_page);

        match self.fit_mode {
            FitMode::FitWidth => {
                let target_zoom = (avail_w / cur_pw).clamp(0.1, 10.0);
                if (target_zoom - self.zoom).abs() > 1e-3 {
                    let ratio = target_zoom / self.zoom;
                    self.zoom = target_zoom;
                    if self.continuous_scroll {
                        self.scroll_y *= ratio;
                    }
                }
            }
            FitMode::FitPage => {
                let target_zoom = (avail_w / cur_pw).min(avail_h / cur_ph).clamp(0.1, 10.0);
                if (target_zoom - self.zoom).abs() > 1e-3 {
                    let ratio = target_zoom / self.zoom;
                    self.zoom = target_zoom;
                    if self.continuous_scroll {
                        self.scroll_y *= ratio;
                    }
                }
            }
            FitMode::Manual => {}
        }
        self.clamp_scroll(rect.size());

        // Check if pointer is currently over/dragging a scrollbar so canvas clicks/drags don't steal it
        let hover_pos = ui.ctx().input(|i| i.pointer.hover_pos());
        let latest_pos = ui.ctx().input(|i| i.pointer.latest_pos());
        let on_scrollbar = self.dragging_v_scrollbar
            || self.dragging_h_scrollbar
            || (self.config.show_scrollbars
                && hover_pos
                    .is_some_and(|p| p.x >= rect.max.x - 16.0 || p.y >= rect.max.y - 16.0));

        let response = ui.allocate_rect(rect, egui::Sense::click_and_drag());

        let visible_pages = self.compute_visible_pages(rect);
        for &(page_idx, _) in &visible_pages {
            self.ensure_links_loaded(page_idx);
        }

        // Hit-test pointer against links and text on visible pages
        let hovered_page_pt: Option<(usize, f32, f32, egui::Rect)> = if on_scrollbar {
            None
        } else {
            hover_pos.and_then(|pos| {
                visible_pages.iter().find_map(|&(page_idx, page_rect)| {
                    if page_rect.contains(pos) {
                        let x_pt = (pos.x - page_rect.min.x) / self.zoom;
                        let y_pt = (pos.y - page_rect.min.y) / self.zoom;
                        Some((page_idx, x_pt, y_pt, page_rect))
                    } else {
                        None
                    }
                })
            })
        };

        let hovered_link: Option<(usize, PageLink, egui::Rect)> =
            hovered_page_pt.and_then(|(page_idx, x_pt, y_pt, page_rect)| {
                self.links_cache.get(&page_idx)?.iter().find_map(|link| {
                    if link.rect.contains(x_pt, y_pt) {
                        Some((page_idx, link.clone(), page_rect))
                    } else {
                        None
                    }
                })
            });

        // Update cursor icon & link tooltip
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
            } else if let Some((page_idx, x_pt, y_pt, _)) = hovered_page_pt
                && let Some(doc) = &self.doc
                && doc.char_index_at(page_idx, x_pt, y_pt, 6.0, 6.0).is_some()
            {
                ui.ctx().set_cursor_icon(egui::CursorIcon::Text);
            }
        }

        // Click handling: open link or clear selection
        if response.clicked() && !on_scrollbar {
            if let Some((_, link, _)) = &hovered_link {
                match &link.target {
                    LinkTarget::InternalPage(p) => self.jump_via_link(*p),
                    LinkTarget::ExternalUri(uri) => {
                        ui.ctx().open_url(egui::OpenUrl::new_tab(uri.clone()));
                    }
                }
            } else {
                self.selection = None;
            }
        }

        // Drag start: dispatch based on configured per-button DragAction (Space + Left always pans)
        let space_held = ui.ctx().input(|i| i.key_down(egui::Key::Space));
        if response.drag_started() && !on_scrollbar {
            let (started_btn, drag_action) = if response.drag_started_by(egui::PointerButton::Primary)
            {
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
                    self.drag_mode = DragMode::Pan;
                    self.drag_button = started_btn;
                    self.pan_start_pointer = ui
                        .ctx()
                        .input(|i| i.pointer.press_origin())
                        .or(latest_pos);
                    self.pan_start_scroll = egui::vec2(self.scroll_x, self.scroll_y);
                }
                DragAction::SelectText => {
                    // Use press origin if available so selection starts right where the user pressed
                    let press_pos = ui
                        .ctx()
                        .input(|i| i.pointer.press_origin())
                        .or(latest_pos);
                    let start_pt = press_pos.and_then(|pos| {
                        visible_pages.iter().find_map(|&(page_idx, page_rect)| {
                            if page_rect.contains(pos) {
                                let x_pt = (pos.x - page_rect.min.x) / self.zoom;
                                let y_pt = (pos.y - page_rect.min.y) / self.zoom;
                                Some((page_idx, x_pt, y_pt))
                            } else {
                                None
                            }
                        })
                    });

                    if let Some((page_idx, x_pt, y_pt)) = start_pt
                        && let Some(doc) = &self.doc
                        && let Some(char_idx) = doc
                            .char_index_at(page_idx, x_pt, y_pt, 16.0, 16.0)
                            .or_else(|| doc.char_index_at(page_idx, x_pt, y_pt, 80.0, 24.0))
                    {
                        let (rects, text) =
                            doc.selection_rects_and_text(page_idx, char_idx, char_idx);
                        self.selection = Some(TextSelection {
                            page_index: page_idx,
                            anchor_char: char_idx,
                            head_char: char_idx,
                            rects,
                            text,
                        });
                        self.drag_mode = DragMode::SelectText;
                        self.drag_button = started_btn;
                    } else {
                        self.selection = None;
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

        // End drag only when the initiating mouse button is actually released
        // (keeps drag alive even if the cursor leaves the window!)
        if let Some(btn) = self.drag_button
            && !ui.ctx().input(|i| i.pointer.button_down(btn))
        {
            self.drag_mode = DragMode::None;
            self.drag_button = None;
            self.pan_start_pointer = None;
        }

        // Active drag update (anchor-based pan prevents any slip when cursor leaves/re-enters window)
        match self.drag_mode {
            DragMode::Pan => {
                if let (Some(start_pos), Some(pos)) = (self.pan_start_pointer, latest_pos) {
                    let total = pos - start_pos;
                    let desired_x = self.pan_start_scroll.x + total.x;
                    let desired_y = if self.continuous_scroll {
                        self.pan_start_scroll.y - total.y
                    } else {
                        self.pan_start_scroll.y + total.y
                    };

                    self.scroll_x = desired_x;
                    self.scroll_y = desired_y;
                    self.clamp_scroll(rect.size());

                    // Keep anchor synced if we hit a scroll boundary so reversing has zero dead-zone
                    if (self.scroll_x - desired_x).abs() > 0.5
                        || (self.scroll_y - desired_y).abs() > 0.5
                    {
                        self.pan_start_pointer = Some(pos);
                        self.pan_start_scroll = egui::vec2(self.scroll_x, self.scroll_y);
                    }
                }
            }
            DragMode::SelectText => {
                if let (Some(sel), Some(pos), Some(doc)) =
                    (&mut self.selection, latest_pos, &self.doc)
                    && let Some(&(_, page_rect)) =
                        visible_pages.iter().find(|&&(idx, _)| idx == sel.page_index)
                {
                    let x_pt = ((pos.x - page_rect.min.x) / self.zoom)
                        .clamp(0.0, page_rect.width() / self.zoom);
                    let y_pt = ((pos.y - page_rect.min.y) / self.zoom)
                        .clamp(0.0, page_rect.height() / self.zoom);
                    let found = doc
                        .char_index_at(sel.page_index, x_pt, y_pt, 16.0, 16.0)
                        .or_else(|| doc.char_index_at(sel.page_index, x_pt, y_pt, 120.0, 24.0));
                    if let Some(head) = found
                        && head != sel.head_char
                    {
                        sel.head_char = head;
                        let (rects, text) =
                            doc.selection_rects_and_text(sel.page_index, sel.anchor_char, head);
                        sel.rects = rects;
                        sel.text = text;
                    }
                }
            }
            DragMode::None => {}
        }

        // Double-click action
        if response.double_clicked() && hovered_link.is_none() {
            match self.config.mouse.double_click_action {
                DoubleClickAction::FitWidth => {
                    self.fit_mode = FitMode::FitWidth;
                    self.scroll_x = 0.0;
                }
                DoubleClickAction::FitPage => {
                    self.fit_mode = FitMode::FitPage;
                    self.scroll_x = 0.0;
                }
                DoubleClickAction::ToggleZoom => {
                    if self.fit_mode == FitMode::FitPage {
                        self.fit_mode = FitMode::FitWidth;
                    } else {
                        self.fit_mode = FitMode::FitPage;
                        self.scroll_x = 0.0;
                    }
                }
                DoubleClickAction::None => {}
            }
            self.clamp_scroll(rect.size());
        }

        // Mouse wheel + trackpad pinch zoom (anchored at mouse cursor!)
        if ui.rect_contains_pointer(rect) {
            let (scroll_delta, pinch_delta, modifiers, pointer_pos) = ui.ctx().input(|i| {
                (
                    i.smooth_scroll_delta,
                    i.zoom_delta(),
                    i.modifiers,
                    i.pointer.hover_pos(),
                )
            });

            let anchor = pointer_pos
                .map(|p| p - rect.min)
                .unwrap_or(rect.size() * 0.5);

            if (pinch_delta - 1.0).abs() > 1e-3 {
                self.zoom_at(self.zoom * pinch_delta, anchor, rect.size());
            }

            if scroll_delta != egui::Vec2::ZERO {
                let sign = if self.config.mouse.invert_scroll {
                    -1.0
                } else {
                    1.0
                };
                let delta = scroll_delta * sign * self.config.mouse.scroll_speed;

                let wheel_action = if modifiers.command {
                    self.config.mouse.ctrl_wheel_action
                } else if modifiers.shift {
                    self.config.mouse.shift_wheel_action
                } else {
                    self.config.mouse.wheel_action
                };

                match wheel_action {
                    WheelAction::Scroll => {
                        let (_, min_y, max_y, _, _) = self.scroll_bounds(rect.size());
                        if !self.continuous_scroll && (max_y - min_y) <= 1.0 {
                            // In single-page mode when page fits vertically, wheel flips pages
                            if delta.y < -4.0 {
                                self.goto_page(self.current_page.saturating_add(1));
                            } else if delta.y > 4.0 {
                                self.goto_page(self.current_page.saturating_sub(1));
                            }
                        } else {
                            self.scroll_x += delta.x;
                            if self.continuous_scroll {
                                self.scroll_y -= delta.y;
                            } else {
                                self.scroll_y += delta.y;
                            }
                            self.clamp_scroll(rect.size());
                        }
                    }
                    WheelAction::HorizontalScroll => {
                        self.scroll_x += delta.y + delta.x;
                        self.clamp_scroll(rect.size());
                    }
                    WheelAction::Zoom => {
                        let factor = (delta.y * 0.004).exp();
                        self.zoom_at(self.zoom * factor, anchor, rect.size());
                    }
                    WheelAction::Page => {
                        if delta.y < -4.0 {
                            self.goto_page(self.current_page.saturating_add(1));
                        } else if delta.y > 4.0 {
                            self.goto_page(self.current_page.saturating_sub(1));
                        }
                    }
                    WheelAction::None => {}
                }
            }
        }

        // Recompute visible pages after scroll/zoom updates
        let visible_pages = self.compute_visible_pages(rect);

        // Debounce high-res re-render while actively zooming
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

        // Render visible pages into LRU cache
        let ppp = ui.ctx().pixels_per_point();
        let use_gpu = self.wgpu_state.is_some();
        let theme = self.active_theme().clone();

        for &(page_idx, page_rect) in &visible_pages {
            let clamped_target_w =
                ((page_rect.width() * ppp).round() as i32).clamp(64, MAX_RENDER_WIDTH_PX);

            let should_render = if is_zooming && self.cache.has_page(page_idx) {
                false
            } else {
                self.cache
                    .needs_render(page_idx, clamped_target_w, &theme.name, use_gpu)
            };

            if should_render {
                if let Some(doc) = &self.doc
                    && let Err(err) = self.cache.render_into_cache(
                        doc,
                        page_idx,
                        clamped_target_w,
                        &theme,
                        ui.ctx(),
                        self.wgpu_state.as_ref(),
                    )
                {
                    self.status_error = Some(err.to_string());
                }
            } else {
                self.cache.touch(page_idx);
            }
        }

        // Paint visible pages, link hover highlight, text selection, and search highlights
        let painter = ui.painter_at(rect);
        for &(page_idx, page_rect) in &visible_pages {
            if let Some(entry) = self.cache.entries.get(&page_idx) {
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

            // Subtle hover highlight for links
            if let Some((hl_page, ref link, _)) = hovered_link
                && hl_page == page_idx
            {
                let lr = egui::Rect::from_min_max(
                    egui::pos2(
                        page_rect.min.x + link.rect.left * self.zoom,
                        page_rect.min.y + link.rect.top * self.zoom,
                    ),
                    egui::pos2(
                        page_rect.min.x + link.rect.right * self.zoom,
                        page_rect.min.y + link.rect.bottom * self.zoom,
                    ),
                );
                painter.rect_filled(
                    lr,
                    2.0,
                    egui::Color32::from_rgba_unmultiplied(80, 160, 255, 45),
                );
            }

            // Text selection highlight
            if let Some(sel) = &self.selection
                && sel.page_index == page_idx
            {
                let sel_fill = egui::Color32::from_rgba_unmultiplied(70, 140, 255, 95);
                for r in &sel.rects {
                    let sr = egui::Rect::from_min_max(
                        egui::pos2(
                            page_rect.min.x + r.left * self.zoom,
                            page_rect.min.y + r.top * self.zoom,
                        ),
                        egui::pos2(
                            page_rect.min.x + r.right * self.zoom,
                            page_rect.min.y + r.bottom * self.zoom,
                        ),
                    );
                    painter.rect_filled(sr, 1.0, sel_fill);
                }
            }

            // Search match highlights
            if !self.search_matches.is_empty() {
                for (match_idx, m) in self.search_matches.iter().enumerate() {
                    if m.page_index != page_idx {
                        continue;
                    }
                    let is_active = match_idx == self.search_active_idx;
                    let fill = if is_active {
                        egui::Color32::from_rgba_unmultiplied(255, 140, 0, 115)
                    } else {
                        egui::Color32::from_rgba_unmultiplied(255, 230, 0, 85)
                    };
                    for r in &m.rects {
                        let hl = egui::Rect::from_min_max(
                            egui::pos2(
                                page_rect.min.x + r.left * self.zoom,
                                page_rect.min.y + r.top * self.zoom,
                            ),
                            egui::pos2(
                                page_rect.min.x + r.right * self.zoom,
                                page_rect.min.y + r.bottom * self.zoom,
                            ),
                        );
                        painter.rect_filled(hl, 2.0, fill);
                    }
                }
            }
        }

        // Draw overlay vertical & horizontal scrollbars on top of the canvas
        let _ = self.draw_scrollbars(ui, rect);

        // Pre-render at most 1 adjacent page when idle
        if !is_zooming
            && let (Some(doc), Some(&(first_vis, _)), Some(&(last_vis, _))) =
                (&self.doc, visible_pages.first(), visible_pages.last())
        {
            let candidates = [
                (last_vis + 1 < doc.page_count).then_some(last_vis + 1),
                first_vis.checked_sub(1),
            ];
            for adj in candidates.into_iter().flatten() {
                let adj_w = ((doc.page_size_pt(adj).0 * self.zoom * ppp).round() as i32)
                    .clamp(64, MAX_RENDER_WIDTH_PX);
                if self
                    .cache
                    .needs_render(adj, adj_w, &theme.name, use_gpu)
                {
                    let _ = self.cache.render_into_cache(
                        doc,
                        adj,
                        adj_w,
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
    }
}
