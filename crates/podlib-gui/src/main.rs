//! podlib-gui — desktop front-end for iPod libraries (eframe/egui).
//!
//! Round 1: device panel (scan + sudo rw-mount) and safe eject. Round 2:
//! live library views (tracks/playlists), drag-and-drop, mouse actions.
//! See docs/tasks/2026-11-13-gui-connect.md for the round plan.

mod app;
mod device;
mod library;

use eframe::egui;

fn main() -> eframe::Result {
    let options = eframe::NativeOptions {
        viewport: egui::ViewportBuilder::default()
            .with_inner_size([1100.0, 700.0])
            // Native file drag-and-drop (files onto the window → add tracks).
            .with_drag_and_drop(true),
        ..Default::default()
    };
    eframe::run_native(
        "podlib",
        options,
        // Context is cloned once so worker threads can request repaints.
        Box::new(|cc| Ok(Box::new(app::PodlibApp::new(cc.egui_ctx.clone())))),
    )
}
