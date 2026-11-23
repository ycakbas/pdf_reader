mod app;
mod config;
mod pdf;
mod shader;
mod theme;

use std::path::PathBuf;

use anyhow::Result;
use clap::Parser;
use eframe::egui;

#[derive(Debug, Parser)]
#[command(name = "pdf_reader", about = "Ultra-fast cross-platform PDF reader")]
struct Cli {
    /// PDF file(s) to open on startup
    files: Vec<PathBuf>,

    /// Path to a custom config.toml file
    #[arg(short, long)]
    config: Option<PathBuf>,

    /// Initial 1-based page number to jump to (applied to first file)
    #[arg(short, long)]
    page: Option<usize>,

    /// Override active PDF theme (e.g. light, dark, sepia, gruvbox_dark, solarized_dark, oled)
    #[arg(short, long)]
    theme: Option<String>,
}

fn main() -> Result<()> {
    let cli = Cli::parse();
    let mut cfg = config::Config::load_or_default(cli.config.as_deref())?;

    if let Some(theme_name) = cli.theme {
        cfg.theme = theme_name;
    }

    let initial_files = cli.files;
    let initial_page = cli.page.map(|p| p.saturating_sub(1));
    let config_path = cli.config;

    let native_options = eframe::NativeOptions {
        viewport: egui::ViewportBuilder::default()
            .with_title("pdf_reader")
            .with_inner_size([1100.0, 850.0])
            .with_drag_and_drop(true),
        ..Default::default()
    };

    eframe::run_native(
        "pdf_reader",
        native_options,
        Box::new(move |cc| {
            Ok(Box::new(app::PdfReaderApp::new(
                cc,
                cfg,
                config_path,
                initial_files,
                initial_page,
            )))
        }),
    )
    .map_err(|err| anyhow::anyhow!("eframe error: {err}"))
}
