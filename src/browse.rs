//! `ibackup browse` — a window for looking inside a finished backup.
//!
//! The backup folder is opened read-only: `Manifest.db` is indexed into memory
//! (see [`model`]) and file contents are read straight out of the blob folders
//! for previews, or copied out by an explicit "Extract" action.

mod app;
mod model;
mod preview;

use std::path::Path;
use std::path::PathBuf;
use std::process::Command;

use anyhow::Result;

pub fn run(initial: Option<PathBuf>) -> Result<()> {
    crate::util::set_ui_mode(true);
    let native = eframe::NativeOptions {
        persist_window: false,
        viewport: eframe::egui::ViewportBuilder::default()
            .with_min_inner_size([980.0, 620.0])
            .with_maximized(true)
            .with_title("iPhone Backup Explorer")
            .with_icon(crate::ui::window_icon()),
        ..Default::default()
    };
    let result = eframe::run_native(
        "ibackup-explorer",
        native,
        Box::new(move |cc| {
            Ok(Box::new(app::ExploreApp::new(cc, initial)) as Box<dyn eframe::App>)
        }),
    )
    .map_err(|e| anyhow::anyhow!("GUI: {e}"));
    crate::util::set_ui_mode(false);
    result
}

/// Hand a path to the operating system's "open with" handler.
///
/// Used for videos/audio that the explorer does not decode itself, and for
/// revealing an extraction folder.
pub(crate) fn open_in_system(path: &Path) -> std::io::Result<()> {
    #[cfg(target_os = "windows")]
    {
        Command::new("cmd")
            .args(["/C", "start", ""])
            .arg(path)
            .spawn()
            .map(|_| ())
    }
    #[cfg(target_os = "macos")]
    {
        Command::new("open").arg(path).spawn().map(|_| ())
    }
    #[cfg(all(unix, not(target_os = "macos")))]
    {
        Command::new("xdg-open").arg(path).spawn().map(|_| ())
    }
}
