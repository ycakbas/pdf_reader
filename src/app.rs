use std::collections::HashMap;
use std::path::PathBuf;

use eframe::egui;
use eframe::egui_wgpu;

use crate::config::{Action, Config, DoubleClickAction, KeyBinding, WheelAction};
use crate::pdf::{BookmarkNode, OpenDocument, SearchMatch, init_pdfium};
use crate::shader::{PdfShaderResources, paint_page_callback};
use crate::theme::{ResolvedTheme, all_themes};

const MAX_CACHED_PAGES: usize = 16;
const CANVAS_MARGIN: f32 = 24.0;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum FitMode {
    Manual,
    FitWidth,
    FitPage,
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

    fn needs_render(&self, page_index: usize, target_width_px: i32, theme_name: &str, use_gpu: bool) -> bool {
        let Some(entry) = self.entries.get(&page_index) else {
            return true;
        };
        if (entry.rendered_width_px - target_width_px).abs() > 24 {
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
        target_width_px: i32,
        theme: &ResolvedTheme,
        ctx: &egui::Context,
        wgpu_state: Option<&egui_wgpu::RenderState>,
    ) -> anyhow::Result<()> {
        let mut rendered = doc.render_page(page_index, target_width_px)?;

        // Remove any previous GPU texture for this page before replacing it.
        if let Some(old) = self.entries.remove(&page_index)
            && let (Some(rs), Some(old_key)) = (wgpu_state, old.gpu_key)
        {
            PdfShaderResources::remove_page(rs, old_key);
        }

        let entry = if let Some(rs) = wgpu_state {
            let gpu_key = self.next_gpu_key;
            self.next_gpu_key = self.next_gpu_key.wrapping_add(1);
            // Upload raw RGBA once; the WGSL fragment shader applies theming on the GPU.
            PdfShaderResources::upload_page(
                rs,
                gpu_key,
                rendered.width as u32,
                rendered.height as u32,
                &rendered.rgba,
            );
            CachedPageEntry {
                rendered_width_px: target_width_px,
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
                rendered_width_px: target_width_px,
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
    bindings: Vec<KeyBinding>,
    themes: Vec<ResolvedTheme>,
    active_theme_idx: usize,
    wgpu_state: Option<egui_wgpu::RenderState>,

    doc: Option<OpenDocument>,
    status_error: Option<String>,

    current_page: usize,
    zoom: f32,
    fit_mode: FitMode,
    continuous_scroll: bool,
    /// Horizontal pan offset relative to centered page(s) in logical pixels.
    scroll_x: f32,
    /// Vertical scroll offset from top of document in logical pixels (continuous mode),
    /// or vertical pan offset from center (single-page mode).
    scroll_y: f32,
    last_canvas_size: egui::Vec2,

    show_toolbar: bool,
    show_toc: bool,

    // Search state
    search_open: bool,
    search_focus_requested: bool,
    search_query: String,
    search_last_executed: String,
    search_match_case: bool,
    search_matches: Vec<SearchMatch>,
    search_active_idx: usize,

    cache: PageCache,
}

impl PdfReaderApp {
    pub fn new(
        cc: &eframe::CreationContext<'_>,
        config: Config,
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
        let status_error = init_pdfium().err().map(|e| e.to_string());

        let mut app = Self {
            config,
            bindings,
            themes,
            active_theme_idx,
            wgpu_state,
            doc: None,
            status_error,
            current_page: 0,
            zoom: 1.0,
            fit_mode: FitMode::FitWidth,
            continuous_scroll,
            scroll_x: 0.0,
            scroll_y: 0.0,
            last_canvas_size: egui::vec2(1000.0, 800.0),
            show_toolbar,
            show_toc: false,
            search_open: false,
            search_focus_requested: false,
            search_query: String::new(),
            search_last_executed: String::new(),
            search_match_case: false,
            search_matches: Vec::new(),
            search_active_idx: 0,
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

    /// Y-offset (in logical pixels) of the top of `page_index` in continuous scroll mode.
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

    /// Total document height (in logical pixels) in continuous scroll mode.
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

    fn clamp_scroll(&mut self, viewport_height: f32) {
        if self.continuous_scroll {
            let max_y = (self.total_doc_height() - viewport_height).max(0.0);
            self.scroll_y = self.scroll_y.clamp(0.0, max_y);
        }
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
            self.clamp_scroll(self.last_canvas_size.y);
        } else {
            self.scroll_x = 0.0;
            self.scroll_y = 0.0;
        }
    }

    /// Zoom to `new_zoom` while keeping the document point under `anchor_in_canvas` fixed.
    fn zoom_at(&mut self, new_zoom: f32, anchor_in_canvas: egui::Vec2, canvas_size: egui::Vec2) {
        let old_zoom = self.zoom;
        let clamped = new_zoom.clamp(0.1, 10.0);
        if (clamped - old_zoom).abs() < 1e-4 {
            return;
        }
        self.fit_mode = FitMode::Manual;
        let ratio = clamped / old_zoom;

        // Horizontal: center of canvas + scroll_x is page center.
        let dx_from_center = anchor_in_canvas.x - canvas_size.x * 0.5;
        self.scroll_x = dx_from_center - (dx_from_center - self.scroll_x) * ratio;

        if self.continuous_scroll {
            // Document Y under cursor before zoom -> scale by ratio -> solve for new scroll_y.
            let doc_y = self.scroll_y + anchor_in_canvas.y;
            self.zoom = clamped;
            self.scroll_y = doc_y * ratio - anchor_in_canvas.y;
            self.clamp_scroll(canvas_size.y);
        } else {
            let dy_from_center = anchor_in_canvas.y - canvas_size.y * 0.5;
            self.scroll_y = dy_from_center - (dy_from_center - self.scroll_y) * ratio;
            self.zoom = clamped;
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
            self.clamp_scroll(self.last_canvas_size.y);
        } else {
            self.scroll_x = 0.0;
            self.scroll_y = 0.0;
        }
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
                // GPU shader recolors live without re-rendering! Only CPU fallback needs refresh.
            }
            Action::ToggleToolbar => {
                self.show_toolbar = !self.show_toolbar;
            }
            Action::ToggleToc => {
                self.show_toc = !self.show_toc;
            }
            Action::ToggleContinuous => {
                let page = self.current_page;
                self.continuous_scroll = !self.continuous_scroll;
                self.goto_page(page);
            }
            Action::Find => {
                self.search_open = true;
                self.search_focus_requested = true;
            }
            Action::FindNext => self.step_search(true),
            Action::FindPrev => self.step_search(false),
            Action::OpenFile => self.pick_and_open_file(),
            Action::Quit => {
                ctx.send_viewport_cmd(egui::ViewportCommand::Close);
            }
        }
    }

    fn process_shortcuts(&mut self, ctx: &egui::Context) {
        let typing = ctx.egui_wants_keyboard_input();

        if self.search_open && ctx.input(|i| i.key_pressed(egui::Key::Escape)) {
            self.search_open = false;
            return;
        }

        let mut triggered = Vec::new();
        ctx.input(|input| {
            for binding in &self.bindings {
                // While typing in search box, only allow shortcuts with Ctrl/Cmd or F-keys.
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

            ui.separator();

            if ui.button("<").clicked() {
                self.goto_page(self.current_page.saturating_sub(1));
            }
            let count = self.page_count();
            if count > 0 {
                ui.label(format!("{} / {count}", self.current_page + 1));
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
                        }
                    }
                });

            if let Some(doc) = &self.doc
                && let Some(name) = doc.path.file_name().and_then(|n| n.to_str())
            {
                ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                    ui.weak(name);
                });
            }
        });
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
            self.goto_page(page);
        }
    }

    fn draw_canvas(&mut self, ui: &mut egui::Ui) {
        self.cache.begin_frame();
        let canvas_color = self.active_theme().canvas;
        let rect = ui.available_rect_before_wrap();
        self.last_canvas_size = rect.size();
        ui.painter().rect_filled(rect, 0.0, canvas_color);

        let response = ui.allocate_rect(rect, egui::Sense::click_and_drag());

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
                        self.clamp_scroll(rect.height());
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
                        self.clamp_scroll(rect.height());
                    }
                }
            }
            FitMode::Manual => {}
        }

        // Mouse drag pan
        let pan_btn = self.config.mouse.pan_button.to_egui();
        if response.dragged_by(pan_btn) {
            let delta = response.drag_delta();
            self.scroll_x += delta.x;
            if self.continuous_scroll {
                self.scroll_y -= delta.y;
                self.clamp_scroll(rect.height());
            } else {
                self.scroll_y += delta.y;
            }
        }

        // Double-click action
        if response.double_clicked() {
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
        }

        // Mouse wheel + trackpad pinch zoom (anchored at mouse cursor!)
        if response.hovered() {
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

            // Trackpad pinch-to-zoom
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
                        self.scroll_x += delta.x;
                        if self.continuous_scroll {
                            self.scroll_y -= delta.y;
                            self.clamp_scroll(rect.height());
                        } else {
                            self.scroll_y += delta.y;
                        }
                    }
                    WheelAction::HorizontalScroll => {
                        self.scroll_x += delta.y + delta.x;
                    }
                    WheelAction::Zoom => {
                        // Smooth exponential zoom proportional to wheel delta
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

        // Compute visible page rectangles
        let mut visible_pages: Vec<(usize, egui::Rect)> = Vec::new();
        let page_count = self.page_count();
        let mut center_page = self.current_page;

        if self.continuous_scroll {
            let mut y = CANVAS_MARGIN;
            let view_top = self.scroll_y;
            let view_bottom = self.scroll_y + rect.height();
            let view_mid = self.scroll_y + rect.height() * 0.5;

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

        // Render visible pages into LRU cache (and pre-render 1 adjacent page lazily)
        let ppp = ui.ctx().pixels_per_point();
        let use_gpu = self.wgpu_state.is_some();
        let theme = self.active_theme().clone();

        for &(page_idx, page_rect) in &visible_pages {
            let target_w = (page_rect.width() * ppp).round() as i32;
            if self
                .cache
                .needs_render(page_idx, target_w, &theme.name, use_gpu)
            {
                if let Some(doc) = &self.doc
                    && let Err(err) = self.cache.render_into_cache(
                        doc,
                        page_idx,
                        target_w,
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

        // Paint visible pages + search highlights
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

            // Draw search match highlights on this page
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

        // Pre-render at most 1 adjacent page (next or prev) per frame so scrolling stays instant
        if let (Some(doc), Some(&(first_vis, _)), Some(&(last_vis, _))) =
            (&self.doc, visible_pages.first(), visible_pages.last())
        {
            let candidates = [
                (last_vis + 1 < doc.page_count).then_some(last_vis + 1),
                first_vis.checked_sub(1),
            ];
            for adj in candidates.into_iter().flatten() {
                let adj_w = (doc.page_size_pt(adj).0 * self.zoom * ppp).round() as i32;
                if self.cache.needs_render(adj, adj_w, &theme.name, use_gpu) {
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
        }

        egui::CentralPanel::default()
            .frame(egui::Frame::NONE)
            .show(ui, |ui| {
                self.draw_canvas(ui);
            });
    }
}
