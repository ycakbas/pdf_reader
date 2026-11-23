use std::collections::BTreeMap;
use std::fs;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use directories::ProjectDirs;
use eframe::egui;
use serde::{Deserialize, Serialize};

use crate::theme::ThemeSpec;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum DefaultFitMode {
    FitWidth,
    FitPage,
    Manual,
}

impl DefaultFitMode {
    pub fn label(self) -> &'static str {
        match self {
            Self::FitWidth => "Fit Width",
            Self::FitPage => "Fit Page",
            Self::Manual => "Manual (100%)",
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct Config {
    /// Active theme name (built-in or defined under `[themes.<name>]`).
    pub theme: String,
    /// Show the top status/toolbar on startup.
    pub show_toolbar: bool,
    /// Show overlay scrollbars on the PDF canvas.
    pub show_scrollbars: bool,
    /// Enable continuous vertical scrolling across pages.
    pub continuous_scroll: bool,
    /// Gap in logical pixels between pages in continuous scroll mode.
    pub page_gap: f32,
    /// Initial zoom fit mode when opening the viewer.
    pub default_fit_mode: DefaultFitMode,
    /// Supersampling multiplier applied on top of display DPI (`1.0` = native, `1.5–2.0` = sharper).
    pub render_scale: f32,
    /// Maximum rasterized page width in pixels (higher = sharper at high zoom, uses more VRAM/CPU).
    pub max_render_width: i32,
    /// Enable PDFium LCD subpixel text rendering for crisper text.
    pub lcd_text_rendering: bool,
    /// Mouse behavior settings.
    pub mouse: MouseConfig,
    /// Keyboard shortcut bindings.
    pub shortcuts: ShortcutConfig,
    /// User-defined PDF themes keyed by name.
    pub themes: BTreeMap<String, ThemeSpec>,
}

impl Default for Config {
    fn default() -> Self {
        Self {
            theme: "light".to_string(),
            show_toolbar: true,
            show_scrollbars: true,
            continuous_scroll: true,
            page_gap: 12.0,
            default_fit_mode: DefaultFitMode::FitWidth,
            render_scale: 1.5,
            max_render_width: 4096,
            lcd_text_rendering: true,
            mouse: MouseConfig::default(),
            shortcuts: ShortcutConfig::default(),
            themes: BTreeMap::new(),
        }
    }
}

impl Config {
    pub fn default_path() -> Option<PathBuf> {
        ProjectDirs::from("", "", "pdf_reader").map(|dirs| dirs.config_dir().join("config.toml"))
    }

    pub fn resolve_path(explicit_path: Option<&Path>) -> Option<PathBuf> {
        match explicit_path {
            Some(p) => Some(p.to_path_buf()),
            None => Self::default_path(),
        }
    }

    pub fn load_or_default(explicit_path: Option<&Path>) -> Result<Self> {
        let Some(path) = Self::resolve_path(explicit_path) else {
            return Ok(Self::default());
        };

        if !path.exists() {
            let cfg = Self::default();
            let _ = cfg.save(Some(&path));
            return Ok(cfg);
        }

        let raw = fs::read_to_string(&path)
            .with_context(|| format!("failed to read config at {}", path.display()))?;
        let cfg: Self = toml::from_str(&raw)
            .with_context(|| format!("failed to parse config at {}", path.display()))?;
        Ok(cfg)
    }

    pub fn save(&self, explicit_path: Option<&Path>) -> Result<PathBuf> {
        let path = Self::resolve_path(explicit_path)
            .context("could not determine config directory for this platform")?;
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent)
                .with_context(|| format!("failed to create config dir {}", parent.display()))?;
        }
        let serialized = toml::to_string_pretty(self).context("failed to serialize config")?;
        fs::write(&path, serialized)
            .with_context(|| format!("failed to write config at {}", path.display()))?;
        Ok(path)
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum DragAction {
    SelectText,
    Pan,
    None,
}

impl DragAction {
    pub const ALL: [Self; 3] = [Self::SelectText, Self::Pan, Self::None];

    pub fn label(self) -> &'static str {
        match self {
            Self::SelectText => "Select Text",
            Self::Pan => "Pan / Drag",
            Self::None => "Disabled",
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SideButtonAction {
    NavigateBack,
    NavigateForward,
    PrevPage,
    NextPage,
    ZoomOut,
    ZoomIn,
    CycleTheme,
    ToggleToc,
    None,
}

impl SideButtonAction {
    pub const ALL: [Self; 9] = [
        Self::NavigateBack,
        Self::NavigateForward,
        Self::PrevPage,
        Self::NextPage,
        Self::ZoomOut,
        Self::ZoomIn,
        Self::CycleTheme,
        Self::ToggleToc,
        Self::None,
    ];

    pub fn label(self) -> &'static str {
        match self {
            Self::NavigateBack => "Navigate Back (Link History)",
            Self::NavigateForward => "Navigate Forward (Link History)",
            Self::PrevPage => "Previous Page",
            Self::NextPage => "Next Page",
            Self::ZoomOut => "Zoom Out",
            Self::ZoomIn => "Zoom In",
            Self::CycleTheme => "Cycle Theme",
            Self::ToggleToc => "Toggle TOC",
            Self::None => "Disabled",
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum WheelAction {
    Scroll,
    HorizontalScroll,
    Zoom,
    Page,
    None,
}

impl WheelAction {
    pub const ALL: [Self; 5] = [
        Self::Scroll,
        Self::HorizontalScroll,
        Self::Zoom,
        Self::Page,
        Self::None,
    ];

    pub fn label(self) -> &'static str {
        match self {
            Self::Scroll => "Scroll",
            Self::HorizontalScroll => "Horizontal Scroll",
            Self::Zoom => "Zoom",
            Self::Page => "Change Page",
            Self::None => "Disabled",
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum DoubleClickAction {
    FitWidth,
    FitPage,
    ToggleZoom,
    None,
}

impl DoubleClickAction {
    pub const ALL: [Self; 4] = [
        Self::FitWidth,
        Self::FitPage,
        Self::ToggleZoom,
        Self::None,
    ];

    pub fn label(self) -> &'static str {
        match self {
            Self::FitWidth => "Fit Width",
            Self::FitPage => "Fit Page",
            Self::ToggleZoom => "Toggle Fit Width / Page",
            Self::None => "Disabled",
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct MouseConfig {
    pub left_drag: DragAction,
    pub middle_drag: DragAction,
    pub right_drag: DragAction,
    pub back_button: SideButtonAction,
    pub forward_button: SideButtonAction,
    pub wheel_action: WheelAction,
    pub ctrl_wheel_action: WheelAction,
    pub shift_wheel_action: WheelAction,
    pub double_click_action: DoubleClickAction,
    pub scroll_speed: f32,
    pub scroll_acceleration: bool,
    pub scroll_accel_strength: f32,
    pub scroll_accel_max: f32,
    pub zoom_step: f32,
    pub invert_scroll: bool,
}

impl Default for MouseConfig {
    fn default() -> Self {
        Self {
            left_drag: DragAction::SelectText,
            middle_drag: DragAction::Pan,
            right_drag: DragAction::Pan,
            back_button: SideButtonAction::NavigateBack,
            forward_button: SideButtonAction::NavigateForward,
            wheel_action: WheelAction::Scroll,
            ctrl_wheel_action: WheelAction::Zoom,
            shift_wheel_action: WheelAction::HorizontalScroll,
            double_click_action: DoubleClickAction::FitWidth,
            scroll_speed: 1.0,
            scroll_acceleration: false,
            scroll_accel_strength: 1.5,
            scroll_accel_max: 5.0,
            zoom_step: 1.15,
            invert_scroll: false,
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct ShortcutConfig {
    pub next_page: Vec<String>,
    pub prev_page: Vec<String>,
    pub first_page: Vec<String>,
    pub last_page: Vec<String>,
    pub zoom_in: Vec<String>,
    pub zoom_out: Vec<String>,
    pub zoom_reset: Vec<String>,
    pub fit_width: Vec<String>,
    pub fit_page: Vec<String>,
    pub cycle_theme: Vec<String>,
    pub toggle_toolbar: Vec<String>,
    pub toggle_toc: Vec<String>,
    pub toggle_thumbnails: Vec<String>,
    pub toggle_continuous: Vec<String>,
    pub goto_page: Vec<String>,
    pub find: Vec<String>,
    pub find_next: Vec<String>,
    pub find_prev: Vec<String>,
    pub open_settings: Vec<String>,
    pub open_file: Vec<String>,
    pub quit: Vec<String>,
}

impl Default for ShortcutConfig {
    fn default() -> Self {
        Self {
            next_page: vec!["j".into(), "PageDown".into(), "Right".into(), "Space".into()],
            prev_page: vec!["k".into(), "PageUp".into(), "Left".into()],
            first_page: vec!["Home".into(), "g".into()],
            last_page: vec!["End".into(), "Shift+G".into()],
            zoom_in: vec!["Ctrl+Equals".into(), "Plus".into()],
            zoom_out: vec!["Ctrl+Minus".into(), "Minus".into()],
            zoom_reset: vec!["Ctrl+0".into(), "0".into()],
            fit_width: vec!["w".into()],
            fit_page: vec!["f".into()],
            cycle_theme: vec!["t".into()],
            toggle_toolbar: vec!["b".into()],
            toggle_toc: vec!["Tab".into()],
            toggle_thumbnails: vec!["p".into()],
            toggle_continuous: vec!["c".into()],
            goto_page: vec!["Ctrl+G".into()],
            find: vec!["Ctrl+F".into(), "Slash".into()],
            find_next: vec!["n".into(), "F3".into()],
            find_prev: vec!["Shift+N".into(), "Shift+F3".into()],
            open_settings: vec!["Ctrl+Comma".into()],
            open_file: vec!["Ctrl+O".into()],
            quit: vec!["Ctrl+Q".into(), "q".into()],
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Action {
    NextPage,
    PrevPage,
    FirstPage,
    LastPage,
    ZoomIn,
    ZoomOut,
    ZoomReset,
    FitWidth,
    FitPage,
    CycleTheme,
    ToggleToolbar,
    ToggleToc,
    ToggleThumbnails,
    ToggleContinuous,
    GotoPage,
    Find,
    FindNext,
    FindPrev,
    OpenSettings,
    OpenFile,
    Quit,
}

#[derive(Debug, Clone)]
pub struct KeyBinding {
    pub modifiers: egui::Modifiers,
    pub key: egui::Key,
    pub action: Action,
}

impl ShortcutConfig {
    pub fn compile(&self) -> Vec<KeyBinding> {
        let mut out = Vec::new();
        let pairs: &[(&[String], Action)] = &[
            (&self.next_page, Action::NextPage),
            (&self.prev_page, Action::PrevPage),
            (&self.first_page, Action::FirstPage),
            (&self.last_page, Action::LastPage),
            (&self.zoom_in, Action::ZoomIn),
            (&self.zoom_out, Action::ZoomOut),
            (&self.zoom_reset, Action::ZoomReset),
            (&self.fit_width, Action::FitWidth),
            (&self.fit_page, Action::FitPage),
            (&self.cycle_theme, Action::CycleTheme),
            (&self.toggle_toolbar, Action::ToggleToolbar),
            (&self.toggle_toc, Action::ToggleToc),
            (&self.toggle_thumbnails, Action::ToggleThumbnails),
            (&self.toggle_continuous, Action::ToggleContinuous),
            (&self.goto_page, Action::GotoPage),
            (&self.find, Action::Find),
            (&self.find_next, Action::FindNext),
            (&self.find_prev, Action::FindPrev),
            (&self.open_settings, Action::OpenSettings),
            (&self.open_file, Action::OpenFile),
            (&self.quit, Action::Quit),
        ];

        for (specs, action) in pairs {
            for spec in *specs {
                if let Some((modifiers, key)) = parse_shortcut(spec) {
                    out.push(KeyBinding {
                        modifiers,
                        key,
                        action: *action,
                    });
                }
            }
        }
        out
    }
}

fn parse_shortcut(spec: &str) -> Option<(egui::Modifiers, egui::Key)> {
    let mut modifiers = egui::Modifiers::NONE;
    let mut key_part = None;

    for token in spec.split('+').map(str::trim).filter(|s| !s.is_empty()) {
        match token.to_ascii_lowercase().as_str() {
            "ctrl" | "control" | "cmd" | "command" => {
                modifiers.command = true;
            }
            "shift" => {
                modifiers.shift = true;
            }
            "alt" | "option" => {
                modifiers.alt = true;
            }
            _ => {
                key_part = Some(token);
            }
        }
    }

    let key = parse_key(key_part?)?;
    Some((modifiers, key))
}

fn parse_key(name: &str) -> Option<egui::Key> {
    use egui::Key;

    match name.to_ascii_lowercase().as_str() {
        "a" => Some(Key::A),
        "b" => Some(Key::B),
        "c" => Some(Key::C),
        "d" => Some(Key::D),
        "e" => Some(Key::E),
        "f" => Some(Key::F),
        "g" => Some(Key::G),
        "h" => Some(Key::H),
        "i" => Some(Key::I),
        "j" => Some(Key::J),
        "k" => Some(Key::K),
        "l" => Some(Key::L),
        "m" => Some(Key::M),
        "n" => Some(Key::N),
        "o" => Some(Key::O),
        "p" => Some(Key::P),
        "q" => Some(Key::Q),
        "r" => Some(Key::R),
        "s" => Some(Key::S),
        "t" => Some(Key::T),
        "u" => Some(Key::U),
        "v" => Some(Key::V),
        "w" => Some(Key::W),
        "x" => Some(Key::X),
        "y" => Some(Key::Y),
        "z" => Some(Key::Z),
        "0" => Some(Key::Num0),
        "1" => Some(Key::Num1),
        "2" => Some(Key::Num2),
        "3" => Some(Key::Num3),
        "4" => Some(Key::Num4),
        "5" => Some(Key::Num5),
        "6" => Some(Key::Num6),
        "7" => Some(Key::Num7),
        "8" => Some(Key::Num8),
        "9" => Some(Key::Num9),
        "space" => Some(Key::Space),
        "enter" | "return" => Some(Key::Enter),
        "escape" | "esc" => Some(Key::Escape),
        "tab" => Some(Key::Tab),
        "backspace" => Some(Key::Backspace),
        "home" => Some(Key::Home),
        "end" => Some(Key::End),
        "pageup" | "pgup" => Some(Key::PageUp),
        "pagedown" | "pgdn" => Some(Key::PageDown),
        "up" | "arrowup" => Some(Key::ArrowUp),
        "down" | "arrowdown" => Some(Key::ArrowDown),
        "left" | "arrowleft" => Some(Key::ArrowLeft),
        "right" | "arrowright" => Some(Key::ArrowRight),
        "plus" | "+" => Some(Key::Plus),
        "equals" | "=" => Some(Key::Equals),
        "minus" | "-" => Some(Key::Minus),
        "slash" | "/" => Some(Key::Slash),
        "comma" | "," => Some(Key::Comma),
        "f1" => Some(Key::F1),
        "f2" => Some(Key::F2),
        "f3" => Some(Key::F3),
        "f4" => Some(Key::F4),
        "f5" => Some(Key::F5),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn default_config_roundtrips_toml() {
        let cfg = Config::default();
        let serialized = toml::to_string_pretty(&cfg).expect("serialize default config");
        let parsed: Config = toml::from_str(&serialized).expect("deserialize default config");
        assert_eq!(parsed.theme, "light");
        assert_eq!(parsed.mouse.left_drag, DragAction::SelectText);
        assert_eq!(parsed.mouse.middle_drag, DragAction::Pan);
        assert_eq!(parsed.mouse.right_drag, DragAction::Pan);
        assert_eq!(parsed.mouse.back_button, SideButtonAction::NavigateBack);
        assert_eq!(
            parsed.mouse.forward_button,
            SideButtonAction::NavigateForward
        );
        assert!(!parsed.shortcuts.compile().is_empty());
    }

    #[test]
    fn shortcut_parser_handles_modifiers_and_keys() {
        let (mods, key) = parse_shortcut("Ctrl+Shift+G").expect("parse Ctrl+Shift+G");
        assert!(mods.command);
        assert!(mods.shift);
        assert!(!mods.alt);
        assert_eq!(key, egui::Key::G);
    }
}
