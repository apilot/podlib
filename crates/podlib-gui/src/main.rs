//! podlib-gui — desktop front-end for iPod libraries (eframe/egui).
//!
//! Round 1: device panel (scan + sudo rw-mount) and safe eject. Round 2:
//! live library views (tracks/playlists), drag-and-drop, mouse actions.
//! See docs/tasks/2026-11-13-gui-connect.md for the round plan.

mod app;
mod device;
mod library;
mod text_menu;

use eframe::egui;

/// Which winit backend the GUI runs under (round 6.4).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum BackendChoice {
    /// X11 — native or via XWayland (DND-capable; see main()).
    X11,
    /// Native Wayland — file drops silently lost (see main()).
    NativeWayland,
}

/// Pure decision behind `PODLIB_WAYLAND` (round 6.4): only the exact
/// values `1`/`true` fire the opt-out; unset, empty and any other value
/// (surrounding whitespace included) keep the X11 default.
fn select_backend(podlib_wayland: Option<&str>) -> BackendChoice {
    match podlib_wayland {
        Some("1") | Some("true") => BackendChoice::NativeWayland,
        _ => BackendChoice::X11,
    }
}

fn main() -> eframe::Result {
    // WHY: winit 0.30 has no native Wayland file drag-and-drop (winit#1881 —
    // hover/drop exist only in the X11 backend) yet silently picks Wayland
    // whenever WAYLAND_DISPLAY/WAYLAND_SOCKET is set, losing every drop on
    // Wayland sessions. eframe 0.37 exposes no backend-forcing option without
    // a direct `winit` dependency — the `event_loop_builder` hook would need
    // `EventLoopBuilderExtX11` from the winit crate — so X11 is forced by
    // dropping those two env vars: winit 0.30.13 treats them as "not set"
    // even when empty and then falls back to DISPLAY, i.e. X11/XWayland
    // (the same selection verified live end-to-end).
    match select_backend(std::env::var("PODLIB_WAYLAND").ok().as_deref()) {
        BackendChoice::NativeWayland => {
            eprintln!(
                "[backend] PODLIB_WAYLAND=1: Wayland backend requested — drag-and-drop will be unavailable (winit 0.30 implements file drag-and-drop on X11 only)"
            );
        }
        BackendChoice::X11 => {
            std::env::remove_var("WAYLAND_DISPLAY");
            std::env::remove_var("WAYLAND_SOCKET");
        }
    }
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn wayland_opt_out_requires_exact_1_or_true() {
        // Only exact opt-out values fire — see main() for the winit#1881
        // rationale; select_backend's doc covers the value grammar.
        for not_set in [None, Some(""), Some("0"), Some("yes"), Some("1 "), Some(" true")] {
            assert_eq!(select_backend(not_set), BackendChoice::X11);
        }
        assert_eq!(select_backend(Some("1")), BackendChoice::NativeWayland);
        assert_eq!(select_backend(Some("true")), BackendChoice::NativeWayland);
    }
}
