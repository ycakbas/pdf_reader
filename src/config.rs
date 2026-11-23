use std::collections::BTreeMap;
use std::fs;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use directories::ProjectDirs;
use eframe::egui;
use serde::{Deserialize, Serialize};

use crate::theme::ThemeSpec;

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct Config {
    /// Active theme name (built-in or defined under `[themes.<name>]`).
    pub theme: String,
    /// Show the top status/toolbar on startup.
    pub show_toolbar: bool,
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

    pub fn load_or_default(explicit_path: Option<&Path>) -> Result<Self> {
        let path = match explicit_path {
            Some(p) => Some(p.to_path_buf()),
            None => Self::default_path(),
        };

        let Some(path) = path else {
            return Ok(Self::default());
        };

        if !path.exists() {
            let cfg = Self::default();
            // Best-effort write of default config so users can discover and edit it.
            if let Some(parent) = path.parent() {
                let _ = fs::create_dir_all(parent);
            }
            if let Ok(serialized) = toml::to_string_pretty(&cfg) {
                let _ = fs::write(&path, serialized);
            }
            return Ok(cfg);
        }

        let raw = fs::read_to_string(&path)
            .with_context(|| format!("failed to read config at {}", path.display()))?;
        let cfg: Self = toml::from_str(&raw)
            .with_context(|| format!("failed to parse config at {}", path.display()))?;
        Ok(cfg)
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum MouseButtonSpec {
    Left,
    Middle,
    Right,
}

impl MouseButtonSpec {
    pub fn to_egui(self) -> egui::PointerButton {
        match self {
            Self::Left => egui::PointerButton::Primary,
            Self::Middle => egui::PointerButton::Middle,
            Self::Right => egui::PointerButton::Secondary,
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

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum DoubleClickAction {
    FitWidth,
    FitPage,
    ToggleZoom,
    None,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct MouseConfig {
    pub pan_button: MouseButtonSpec,
    pub wheel_action: WheelAction,
    pub ctrl_wheel_action: WheelAction,
    pub shift_wheel_action: WheelAction,
    pub double_click_action: DoubleClickAction,
    pub scroll_speed: f32,
    pub zoom_step: f32,
    pub invert_scroll: bool,
}

impl Default for MouseConfig {
    fn default() -> Self {
        Self {
            pan_button: MouseButtonSpec::Left,
            wheel_action: WheelAction::Scroll,
            ctrl_wheel_action: WheelAction::Zoom,
            shift_wheel_action: WheelAction::HorizontalScroll,
            double_click_action: DoubleClickAction::FitWidth,
            scroll_speed: 1.0,
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
        assert_eq!(parsed.mouse.pan_button, MouseButtonSpec::Left);
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
