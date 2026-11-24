//! Right-click context menu for text fields (round 6.5).
//!
//! egui 0.37 ships no context menu on `TextEdit`; this module attaches
//! one to any text-field response: «Выделить всё», «Копировать»,
//! «Вырезать», «Вставить».

use crate::i18n::{Key, tr};
use eframe::egui;

/// Which items the menu carries.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum MenuKind {
    /// Everything: for regular text fields.
    Full,
    /// No «Копировать»/«Вырезать»: for password fields — egui blocks
    /// copying out of them anyway (`copy_if_not_password`), so offering
    /// the items would be dishonest UI.
    Secret,
}

/// One action of the context menu.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum MenuAction {
    SelectAll,
    Copy,
    Cut,
    Paste,
}

/// The menu items of `kind`, in display order (translated at call time —
/// the language is pinned at startup).
fn items(kind: MenuKind) -> Vec<(MenuAction, &'static str)> {
    match kind {
        MenuKind::Full => vec![
            (MenuAction::SelectAll, tr(Key::CtxSelectAll)),
            (MenuAction::Copy, tr(Key::CtxCopy)),
            (MenuAction::Cut, tr(Key::CtxCut)),
            (MenuAction::Paste, tr(Key::CtxPaste)),
        ],
        MenuKind::Secret => vec![
            (MenuAction::SelectAll, tr(Key::CtxSelectAll)),
            (MenuAction::Paste, tr(Key::CtxPaste)),
        ],
    }
}

/// The Ctrl-shortcut shown next to each item (renders as Cmd on macOS).
fn shortcut(action: MenuAction) -> egui::KeyboardShortcut {
    match action {
        MenuAction::SelectAll => {
            egui::KeyboardShortcut::new(egui::Modifiers::COMMAND, egui::Key::A)
        }
        MenuAction::Copy => egui::KeyboardShortcut::new(egui::Modifiers::COMMAND, egui::Key::C),
        MenuAction::Cut => egui::KeyboardShortcut::new(egui::Modifiers::COMMAND, egui::Key::X),
        MenuAction::Paste => egui::KeyboardShortcut::new(egui::Modifiers::COMMAND, egui::Key::V),
    }
}

/// Right-click context menu for a regular text field.
pub fn attach_text_menu(response: &egui::Response) {
    show_menu(response, MenuKind::Full);
}

/// Right-click context menu for a password field: no
/// «Копировать»/«Вырезать».
pub fn attach_secret_text_menu(response: &egui::Response) {
    show_menu(response, MenuKind::Secret);
}

fn show_menu(response: &egui::Response, kind: MenuKind) {
    // The field id is bound here, when the menu attaches to the field.
    let ctx = response.ctx.clone();
    let field_id = response.id;
    response.context_menu(|ui| {
        // A right-click does NOT focus a TextEdit; without focus the
        // viewport commands (and select-all) would land nowhere.
        ctx.memory_mut(|memory| memory.request_focus(field_id));
        menu_body(ui, &ctx, field_id, kind);
    });
}

fn menu_body(ui: &mut egui::Ui, ctx: &egui::Context, field_id: egui::Id, kind: MenuKind) {
    for (action, label) in items(kind) {
        let button = egui::Button::new(label).shortcut_text(ctx.format_shortcut(&shortcut(action)));
        if ui.add(button).clicked() {
            ui.close();
            run_action(ctx, field_id, action);
        }
    }
}

fn run_action(ctx: &egui::Context, field_id: egui::Id, action: MenuAction) {
    // Clicking a menu item gives IT the focus, and every action must land
    // in the field — so the field reclaims focus first. It is rendered
    // in the very next pass (the menu is closed by then), which also
    // keeps the reclaimed focus from being dropped (egui discards the
    // focus of a widget that skips the pass right after gaining it).
    ctx.memory_mut(|memory| memory.request_focus(field_id));
    match action {
        // eframe translates these into real Copy/Cut/Paste input events
        // for the focused widget on the next frame.
        MenuAction::Copy => ctx.send_viewport_cmd(egui::ViewportCommand::RequestCopy),
        MenuAction::Cut => ctx.send_viewport_cmd(egui::ViewportCommand::RequestCut),
        MenuAction::Paste => ctx.send_viewport_cmd(egui::ViewportCommand::RequestPaste),
        // A synthetic key event pushed into the input would be dropped
        // before the next pass (`begin_pass` rebuilds the event list
        // from the raw input), so select-all writes the TextEdit
        // selection directly; on the next pass the focused TextEdit
        // clamps the over-large range onto its text. The stored
        // selection survives only while the field keeps focus: every
        // unfocused pass collapses it to a bare cursor.
        MenuAction::SelectAll => {
            if let Some(mut state) = egui::TextEdit::load_state(ctx, field_id) {
                state.cursor.set_char_range(Some(egui::text::CCursorRange::two(
                    egui::text::CCursor::new(0),
                    egui::text::CCursor::new(usize::MAX),
                )));
                egui::TextEdit::store_state(ctx, field_id, state);
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const SCREEN: egui::Vec2 = egui::vec2(1000.0, 600.0);

    fn raw_input(events: Vec<egui::Event>) -> egui::RawInput {
        egui::RawInput {
            screen_rect: Some(egui::Rect::from_min_size(egui::Pos2::ZERO, SCREEN)),
            events,
            ..Default::default()
        }
    }

    fn secondary(pos: egui::Pos2, pressed: bool) -> egui::Event {
        egui::Event::PointerButton {
            pos,
            button: egui::PointerButton::Secondary,
            pressed,
            modifiers: egui::Modifiers::default(),
        }
    }

    /// One pass with the field + menu; drops the font textures the
    /// pass queued (same as the app.rs modal tests do).
    fn run_frame(ctx: &egui::Context, input: egui::RawInput, frame: impl FnMut(&mut egui::Ui)) {
        let mut output = ctx.run_ui(input, frame);
        output.textures_delta.clear();
    }

    // ---- menu structure ----

    #[test]
    fn full_menu_offers_select_all_copy_cut_paste_in_order() {
        assert_eq!(
            items(MenuKind::Full),
            vec![
                (MenuAction::SelectAll, tr(Key::CtxSelectAll)),
                (MenuAction::Copy, tr(Key::CtxCopy)),
                (MenuAction::Cut, tr(Key::CtxCut)),
                (MenuAction::Paste, tr(Key::CtxPaste)),
            ],
            "the full menu carries all four items, in order"
        );
    }

    #[test]
    fn secret_menu_omits_copy_and_cut() {
        assert_eq!(
            items(MenuKind::Secret),
            vec![
                (MenuAction::SelectAll, tr(Key::CtxSelectAll)),
                (MenuAction::Paste, tr(Key::CtxPaste)),
            ],
            "a password field gets no copy/cut item — egui blocks \
             copying out of password fields anyway"
        );
    }

    // ---- item actions ----

    #[test]
    fn copy_cut_paste_send_matching_viewport_commands() {
        let field_id = egui::Id::unique("viewport-cmd-test");
        for (action, expected) in [
            (MenuAction::Copy, egui::ViewportCommand::RequestCopy),
            (MenuAction::Cut, egui::ViewportCommand::RequestCut),
            (MenuAction::Paste, egui::ViewportCommand::RequestPaste),
        ] {
            let ctx = egui::Context::default();
            let mut output = ctx.run_ui(raw_input(Vec::new()), |_| {
                run_action(&ctx, field_id, action);
            });
            assert!(
                output
                    .viewport_output
                    .values()
                    .any(|viewport| viewport.commands.contains(&expected)),
                "{expected:?} must be requested"
            );
            output.textures_delta.clear();
        }
    }

    #[test]
    fn select_all_selects_the_whole_text_of_the_focused_field() {
        let ctx = egui::Context::default();
        let mut text = String::from("привет");
        let mut field_id = egui::Id::NULL;
        run_frame(&ctx, raw_input(Vec::new()), |ui| {
            let response = ui.add(egui::TextEdit::singleline(&mut text));
            field_id = response.id;
        });
        // run_action re-focuses the field itself (a clicked menu item
        // holds the focus otherwise).
        run_frame(&ctx, raw_input(Vec::new()), |_| {
            run_action(&ctx, field_id, MenuAction::SelectAll);
        });
        // Next pass: the focused TextEdit clamps the stored range onto
        // its text and stores the settled selection back.
        run_frame(&ctx, raw_input(Vec::new()), |ui| {
            ui.add(egui::TextEdit::singleline(&mut text));
        });
        let state = egui::TextEdit::load_state(&ctx, field_id)
            .expect("TextEdit stores its state every pass");
        let range = state
            .cursor
            .char_range()
            .expect("select-all must leave a selection");
        let sorted = range.as_sorted_char_range();
        assert_eq!(sorted.start.0, 0, "selection starts at the beginning");
        assert_eq!(
            sorted.end.0,
            text.chars().count(),
            "selection covers the whole text"
        );
    }

    // ---- menu opens on right-click ----

    #[test]
    fn right_click_opens_the_menu_on_an_attached_field() {
        let ctx = egui::Context::default();
        let mut text = String::from("abc");
        let mut field_rect = egui::Rect::NOTHING;
        run_frame(&ctx, raw_input(Vec::new()), |ui| {
            let response = ui.add(egui::TextEdit::singleline(&mut text));
            attach_text_menu(&response);
            field_rect = response.rect;
        });
        let center = field_rect.center();
        run_frame(&ctx, raw_input(vec![secondary(center, true)]), |ui| {
            let response = ui.add(egui::TextEdit::singleline(&mut text));
            attach_text_menu(&response);
        });
        let mut opened = false;
        let mut field_id = egui::Id::NULL;
        run_frame(&ctx, raw_input(vec![secondary(center, false)]), |ui| {
            let response = ui.add(egui::TextEdit::singleline(&mut text));
            attach_text_menu(&response);
            opened = response.context_menu_opened();
            field_id = response.id;
        });
        assert!(opened, "right-click must open the menu");
        assert_eq!(
            ctx.memory(|memory| memory.focused()),
            Some(field_id),
            "the menu focuses the field so its commands land in it"
        );
    }

    #[test]
    fn right_click_opens_the_secret_menu_on_a_password_field() {
        let ctx = egui::Context::default();
        let mut password = String::from("sudo");
        let mut field_rect = egui::Rect::NOTHING;
        run_frame(&ctx, raw_input(Vec::new()), |ui| {
            let response = ui.add(
                egui::TextEdit::singleline(&mut password)
                    .password(true),
            );
            attach_secret_text_menu(&response);
            field_rect = response.rect;
        });
        let center = field_rect.center();
        run_frame(&ctx, raw_input(vec![secondary(center, true)]), |ui| {
            let response = ui.add(
                egui::TextEdit::singleline(&mut password)
                    .password(true),
            );
            attach_secret_text_menu(&response);
        });
        let mut opened = false;
        run_frame(&ctx, raw_input(vec![secondary(center, false)]), |ui| {
            let response = ui.add(
                egui::TextEdit::singleline(&mut password)
                    .password(true),
            );
            attach_secret_text_menu(&response);
            opened = response.context_menu_opened();
        });
        assert!(opened, "right-click must open the secret menu too");
    }
}
