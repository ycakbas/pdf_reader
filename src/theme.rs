use std::collections::BTreeMap;

use eframe::egui::Color32;
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ThemeSpec {
    /// Paper/page background hex color (e.g. "#1e1e2e").
    pub background: String,
    /// Ink/text foreground hex color (e.g. "#cdd6f4").
    pub foreground: String,
    /// Viewport surround canvas hex color.
    pub canvas: String,
    /// If true, remap page luminance between `background` and `foreground` (preserves hue).
    /// If false, render original PDF colors unmodified.
    #[serde(default = "default_true")]
    pub recolor: bool,
}

fn default_true() -> bool {
    true
}

#[derive(Debug, Clone)]
pub struct ResolvedTheme {
    pub name: String,
    pub background: [u8; 3],
    pub foreground: [u8; 3],
    pub canvas: Color32,
    pub recolor: bool,
}

impl ResolvedTheme {
    pub fn from_spec(name: impl Into<String>, spec: &ThemeSpec) -> Self {
        let bg = parse_hex_rgb(&spec.background).unwrap_or([255, 255, 255]);
        let fg = parse_hex_rgb(&spec.foreground).unwrap_or([0, 0, 0]);
        let canvas_rgb = parse_hex_rgb(&spec.canvas).unwrap_or([40, 40, 40]);

        Self {
            name: name.into(),
            background: bg,
            foreground: fg,
            canvas: Color32::from_rgb(canvas_rgb[0], canvas_rgb[1], canvas_rgb[2]),
            recolor: spec.recolor,
        }
    }

    /// Apply PDF theming in-place to an RGBA8 pixel buffer.
    ///
    /// Maps paper white (luma = 1.0) -> `background` and ink black (luma = 0.0) -> `foreground`,
    /// while preserving chromatic saturation for colored diagrams and highlights.
    pub fn apply_to_rgba(&self, rgba: &mut [u8]) {
        if !self.recolor {
            return;
        }

        let bg = [
            self.background[0] as f32 / 255.0,
            self.background[1] as f32 / 255.0,
            self.background[2] as f32 / 255.0,
        ];
        let fg = [
            self.foreground[0] as f32 / 255.0,
            self.foreground[1] as f32 / 255.0,
            self.foreground[2] as f32 / 255.0,
        ];

        for px in rgba.chunks_exact_mut(4) {
            let r = px[0] as f32 / 255.0;
            let g = px[1] as f32 / 255.0;
            let b = px[2] as f32 / 255.0;

            // Perceived luminance of source pixel
            let luma = 0.2126 * r + 0.7152 * g + 0.0722 * b;

            // Chromatic deviation from neutral gray (preserves colored highlights/charts)
            let dr = r - luma;
            let dg = g - luma;
            let db = b - luma;

            // Linearly interpolate between foreground (ink) and background (paper)
            let out_r = (fg[0] + luma * (bg[0] - fg[0]) + dr * 0.85).clamp(0.0, 1.0);
            let out_g = (fg[1] + luma * (bg[1] - fg[1]) + dg * 0.85).clamp(0.0, 1.0);
            let out_b = (fg[2] + luma * (bg[2] - fg[2]) + db * 0.85).clamp(0.0, 1.0);

            px[0] = (out_r * 255.0) as u8;
            px[1] = (out_g * 255.0) as u8;
            px[2] = (out_b * 255.0) as u8;
        }
    }
}

pub fn builtin_themes() -> BTreeMap<String, ThemeSpec> {
    let mut map = BTreeMap::new();
    map.insert(
        "light".into(),
        ThemeSpec {
            background: "#ffffff".into(),
            foreground: "#000000".into(),
            canvas: "#2b2b2b".into(),
            recolor: false,
        },
    );
    map.insert(
        "dark".into(),
        ThemeSpec {
            background: "#1e1e24".into(),
            foreground: "#e4e4ec".into(),
            canvas: "#121216".into(),
            recolor: true,
        },
    );
    map.insert(
        "oled".into(),
        ThemeSpec {
            background: "#000000".into(),
            foreground: "#d8d8d8".into(),
            canvas: "#000000".into(),
            recolor: true,
        },
    );
    map.insert(
        "sepia".into(),
        ThemeSpec {
            background: "#f4ecd8".into(),
            foreground: "#3b2f1e".into(),
            canvas: "#2c261e".into(),
            recolor: true,
        },
    );
    map.insert(
        "gruvbox_dark".into(),
        ThemeSpec {
            background: "#282828".into(),
            foreground: "#ebdbb2".into(),
            canvas: "#1d2021".into(),
            recolor: true,
        },
    );
    map.insert(
        "solarized_dark".into(),
        ThemeSpec {
            background: "#002b36".into(),
            foreground: "#93a1a1".into(),
            canvas: "#001f27".into(),
            recolor: true,
        },
    );
    map
}

pub fn all_themes(custom: &BTreeMap<String, ThemeSpec>) -> Vec<ResolvedTheme> {
    let mut merged = builtin_themes();
    for (k, v) in custom {
        merged.insert(k.clone(), v.clone());
    }
    merged
        .into_iter()
        .map(|(name, spec)| ResolvedTheme::from_spec(name, &spec))
        .collect()
}

fn parse_hex_rgb(hex: &str) -> Option<[u8; 3]> {
    let s = hex.trim().strip_prefix('#').unwrap_or(hex.trim());
    if s.len() != 6 {
        return None;
    }
    let r = u8::from_str_radix(&s[0..2], 16).ok()?;
    let g = u8::from_str_radix(&s[2..4], 16).ok()?;
    let b = u8::from_str_radix(&s[4..6], 16).ok()?;
    Some([r, g, b])
}
