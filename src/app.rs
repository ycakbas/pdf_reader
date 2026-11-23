use std::path::PathBuf;

use eframe::egui;

use crate::config::{Action, Config, DoubleClickAction, KeyBinding, WheelAction};
use crate::pdf::{OpenDocument, init_pdfium};
use crate::theme::{ResolvedTheme, all_themes};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum FitMode {
    Manual,
    FitWidth,
    FitPage,
}

struct CachedPage {
    page_index: usize,
    rendered_width_px: i32,
    theme_name: String,
    texture: egui::TextureHandle,
}

pub struct PdfReaderApp {
    config: Config,
    bindings: Vec<KeyBinding>,
    themes: Vec<ResolvedTheme>,
    active_theme_idx: usize,

    doc: Option<OpenDocument>,
    status_error: Option<String>,

    current_page: usize,
    page_size_pt: (f32, f32),

    zoom: f32,
    fit_mode: FitMode,
    pan: egui::Vec2,
    show_toolbar: bool,

    cached_page: Option<CachedPage>,
}

impl PdfReaderApp {
    pub fn new(
        _cc: &eframe::CreationContext<'_>,
        config: Config,
        initial_file: Option<PathBuf>,
        initial_page: Option<usize>,
    ) -> Self {
        let bindings = config.shortcuts.compile();
        let themes = all_themes(&config.themes);
        let active_theme_idx = themes
            .iter()
            .position(|t| t.name == config.theme)
            .unwrap_or(0);
        let show_toolbar = config.show_toolbar;

        let status_error = init_pdfium().err().map(|e| e.to_string());

        let mut app = Self {
            config,
            bindings,
            themes,
            active_theme_idx,
            doc: None,
            status_error,
            current_page: 0,
            page_size_pt: (612.0, 792.0),
            zoom: 1.0,
            fit_mode: FitMode::FitWidth,
            pan: egui::Vec2::ZERO,
            show_toolbar,
            cached_page: None,
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
                self.current_page = target_page;
                self.page_size_pt = doc.page_size_pt(target_page).unwrap_or((612.0, 792.0));
                self.doc = Some(doc);
                self.pan = egui::Vec2::ZERO;
                self.cached_page = None;
                self.status_error = None;
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

    fn goto_page(&mut self, index: usize) {
        let count = self.page_count();
        if count == 0 {
            return;
        }
        let clamped = index.min(count - 1);
        if clamped != self.current_page {
            self.current_page = clamped;
            self.pan = egui::Vec2::ZERO;
            self.cached_page = None;
            if let Some(doc) = &self.doc
                && let Ok(sz) = doc.page_size_pt(clamped)
            {
                self.page_size_pt = sz;
            }
        }
    }

    fn handle_action(&mut self, action: Action, ctx: &egui::Context) {
        match action {
            Action::NextPage => self.goto_page(self.current_page.saturating_add(1)),
            Action::PrevPage => self.goto_page(self.current_page.saturating_sub(1)),
            Action::FirstPage => self.goto_page(0),
            Action::LastPage => self.goto_page(self.page_count().saturating_sub(1)),
            Action::ZoomIn => {
                self.fit_mode = FitMode::Manual;
                self.zoom = (self.zoom * self.config.mouse.zoom_step).clamp(0.1, 10.0);
            }
            Action::ZoomOut => {
                self.fit_mode = FitMode::Manual;
                self.zoom = (self.zoom / self.config.mouse.zoom_step).clamp(0.1, 10.0);
            }
            Action::ZoomReset => {
                self.fit_mode = FitMode::Manual;
                self.zoom = 1.0;
                self.pan = egui::Vec2::ZERO;
            }
            Action::FitWidth => {
                self.fit_mode = FitMode::FitWidth;
                self.pan.x = 0.0;
            }
            Action::FitPage => {
                self.fit_mode = FitMode::FitPage;
                self.pan = egui::Vec2::ZERO;
            }
            Action::CycleTheme => {
                self.active_theme_idx = (self.active_theme_idx + 1) % self.themes.len();
                self.cached_page = None;
            }
            Action::ToggleToolbar => {
                self.show_toolbar = !self.show_toolbar;
            }
            Action::OpenFile => self.pick_and_open_file(),
            Action::Quit => {
                ctx.send_viewport_cmd(egui::ViewportCommand::Close);
            }
        }
    }

    fn process_shortcuts(&mut self, ctx: &egui::Context) {
        let mut triggered = Vec::new();
        ctx.input(|input| {
            for binding in &self.bindings {
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

    fn ensure_page_texture(&mut self, ctx: &egui::Context, target_width_px: i32) {
        let theme_name = self.active_theme().name.clone();
        let needs_render = match &self.cached_page {
            Some(c) => {
                c.page_index != self.current_page
                    || (c.rendered_width_px - target_width_px).abs() > 24
                    || c.theme_name != theme_name
            }
            None => true,
        };

        if !needs_render {
            return;
        }

        let Some(doc) = &self.doc else {
            return;
        };

        match doc.render_page(self.current_page, target_width_px) {
            Ok(mut rendered) => {
                self.active_theme().apply_to_rgba(&mut rendered.rgba);
                let color_image = egui::ColorImage::from_rgba_unmultiplied(
                    [rendered.width, rendered.height],
                    &rendered.rgba,
                );
                let texture = ctx.load_texture(
                    format!("pdf_page_{}_{}", self.current_page, theme_name),
                    color_image,
                    egui::TextureOptions::LINEAR,
                );
                self.cached_page = Some(CachedPage {
                    page_index: self.current_page,
                    rendered_width_px: target_width_px,
                    theme_name,
                    texture,
                });
                self.status_error = None;
            }
            Err(err) => {
                self.status_error = Some(err.to_string());
            }
        }
    }

    fn draw_toolbar(&mut self, ui: &mut egui::Ui) {
        ui.horizontal(|ui| {
            if ui.button("Open").clicked() {
                self.pick_and_open_file();
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

            if ui.button("-").clicked() {
                self.fit_mode = FitMode::Manual;
                self.zoom = (self.zoom / self.config.mouse.zoom_step).clamp(0.1, 10.0);
            }
            ui.label(format!("{:.0}%", self.zoom * 100.0));
            if ui.button("+").clicked() {
                self.fit_mode = FitMode::Manual;
                self.zoom = (self.zoom * self.config.mouse.zoom_step).clamp(0.1, 10.0);
            }
            if ui.button("Fit W").clicked() {
                self.fit_mode = FitMode::FitWidth;
                self.pan.x = 0.0;
            }
            if ui.button("Fit P").clicked() {
                self.fit_mode = FitMode::FitPage;
                self.pan = egui::Vec2::ZERO;
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
                            self.cached_page = None;
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

    fn draw_canvas(&mut self, ui: &mut egui::Ui) {
        let canvas_color = self.active_theme().canvas;
        let rect = ui.available_rect_before_wrap();
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

        if self.doc.is_none() {
            ui.painter().text(
                rect.center(),
                egui::Align2::CENTER_CENTER,
                "Drop a PDF here or press Ctrl+O to open",
                egui::FontId::proportional(18.0),
                egui::Color32::from_gray(180),
            );
            return;
        }

        // Update zoom based on fit mode
        let margin = 24.0;
        let avail_w = (rect.width() - margin * 2.0).max(64.0);
        let avail_h = (rect.height() - margin * 2.0).max(64.0);
        let (pw, ph) = self.page_size_pt;

        match self.fit_mode {
            FitMode::FitWidth => self.zoom = (avail_w / pw).clamp(0.1, 10.0),
            FitMode::FitPage => self.zoom = (avail_w / pw).min(avail_h / ph).clamp(0.1, 10.0),
            FitMode::Manual => {}
        }

        // Mouse drag pan
        let pan_btn = self.config.mouse.pan_button.to_egui();
        if response.dragged_by(pan_btn) {
            self.pan += response.drag_delta();
        }

        // Double-click action
        if response.double_clicked() {
            match self.config.mouse.double_click_action {
                DoubleClickAction::FitWidth => {
                    self.fit_mode = FitMode::FitWidth;
                    self.pan.x = 0.0;
                }
                DoubleClickAction::FitPage => {
                    self.fit_mode = FitMode::FitPage;
                    self.pan = egui::Vec2::ZERO;
                }
                DoubleClickAction::ToggleZoom => {
                    if self.fit_mode == FitMode::FitPage {
                        self.fit_mode = FitMode::FitWidth;
                    } else {
                        self.fit_mode = FitMode::FitPage;
                        self.pan = egui::Vec2::ZERO;
                    }
                }
                DoubleClickAction::None => {}
            }
        }

        // Mouse wheel action
        if response.hovered() {
            let (scroll_delta, modifiers) =
                ui.ctx().input(|i| (i.smooth_scroll_delta, i.modifiers));
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
                    WheelAction::Scroll => self.pan += delta,
                    WheelAction::HorizontalScroll => self.pan.x += delta.y + delta.x,
                    WheelAction::Zoom => {
                        self.fit_mode = FitMode::Manual;
                        let factor = if delta.y > 0.0 {
                            self.config.mouse.zoom_step
                        } else {
                            1.0 / self.config.mouse.zoom_step
                        };
                        self.zoom = (self.zoom * factor).clamp(0.1, 10.0);
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

        // Render page at HiDPI resolution
        let display_size = egui::vec2(pw * self.zoom, ph * self.zoom);
        let ppp = ui.ctx().pixels_per_point();
        let target_width_px = (display_size.x * ppp).round() as i32;
        self.ensure_page_texture(ui.ctx(), target_width_px);

        if let Some(cached) = &self.cached_page {
            let center = rect.center() + self.pan;
            let page_rect = egui::Rect::from_center_size(center, display_size);
            ui.painter().image(
                cached.texture.id(),
                page_rect,
                egui::Rect::from_min_max(egui::pos2(0.0, 0.0), egui::pos2(1.0, 1.0)),
                egui::Color32::WHITE,
            );
        }
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

        egui::CentralPanel::default()
            .frame(egui::Frame::NONE)
            .show(ui, |ui| {
                self.draw_canvas(ui);
            });
    }
}
