//! podlib-gui — desktop front-end for iPod libraries (eframe/egui).
//!
//! Round 1: device panel (scan + sudo rw-mount) and safe eject. See
//! docs/tasks/2026-11-13-gui-connect.md for the round plan.

mod app;
mod device;

use eframe::egui;

fn main() -> eframe::Result {
    let options = eframe::NativeOptions {
        viewport: egui::ViewportBuilder::default().with_inner_size([900.0, 600.0]),
        ..Default::default()
    };
    eframe::run_native(
        "podlib",
        options,
        // Context is cloned once so worker threads can request repaints.
        Box::new(|cc| Ok(Box::new(app::PodlibApp::new(cc.egui_ctx.clone())))),
    )
}
