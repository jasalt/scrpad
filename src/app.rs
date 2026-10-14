//! The whole window: header (title, pin, close), tab strip, editor and footer toolbar.
//! Everything is laid out in absolute rects inside the frameless window and painted by hand.

use std::path::PathBuf;
use std::sync::{Arc, mpsc};

use egui::{Align2, Color32, CursorIcon, Id, Pos2, Rect, ResizeDirection, Response, Sense, Stroke, Vec2, ViewportCommand};

use crate::clipboard;
use crate::crypto;
use crate::icons;
use crate::instance::Reply;
use crate::session::{Session, ToastAction, lock};
use crate::store::{FontStyle, Note, ReadMode, State};
use crate::theme::{self, ThemeKind, Tokens};
use zeroize::{Zeroize, Zeroizing};

const PAD: f32 = 12.0;
const HEADER_H: f32 = 36.0;
const TABS_H: f32 = 36.0;
const FOOTER_H: f32 = 42.0;
const RESIZE_EDGE: f32 = 6.0;

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum Popup {
    Font,
    More,
    Encrypt,
}

/// Keeps the pin in step with Hyprland, which (unlike winit on Wayland) can pin a window.
#[cfg(target_os = "linux")]
struct WmSync {
    hypr: crate::hyprland::Hyprland,
    /// Window state changes seen on the compositor side (SUPER+O, SUPER+T, moving, resizing).
    observed: mpsc::Receiver<crate::hyprland::WindowState>,
    /// The saved size and position still to be restored once the window floats at start-up.
    restore: bool,
    /// A pin state still to be sent, and until when to retry (the window may not be mapped yet).
    pending: Option<(bool, f64)>,
    /// Observations older than our own last change are stale until this time.
    ignore_until: f64,
    /// The pin to bring back after hiding, read by the `--toggle` listener thread.
    pinned: std::sync::Arc<std::sync::atomic::AtomicBool>,
}

/// Keeps the window out of screenshots, recordings and screen sharing while a hidden or
/// encrypted note is open, or always when the user asks. A secret is drawn only once that is in
/// effect: a viewport command acts after the frame that sends it.
#[derive(Default)]
struct CaptureGuard {
    /// What the window is set to; `None` until the first request went through.
    applied: Option<bool>,
    /// Until when to retry a failed request (on Hyprland the window may not be mapped yet).
    deadline: Option<f64>,
}

pub struct App {
    pub session: Session,
    capture: CaptureGuard,
    force_close: bool,
    fonts_ready: bool,
    applied_theme: Option<ThemeKind>,
    popup: Option<(Popup, Rect)>,
    /// The note being renamed, the edited title, and whether the field still needs to grab focus.
    renaming: Option<(u64, String, bool)>,
    /// The note whose tab is being dragged to a new place, and where on the tab it was grabbed.
    dragging: Option<(u64, f32)>,
    /// The active tab the strip last scrolled into view (scrolling only follows changes).
    scrolled_to: Option<u64>,
    encrypt_form: EncryptForm,
    /// The locked note the unlock field belongs to, what is typed in it, and an error to show.
    unlock_form: (u64, String, Option<&'static str>),
    /// Where the footer's lock button was last drawn (the Encrypt popup anchors to it).
    lock_button: Rect,
    /// The password typed into the approval dialog for a locked note, and an error to show there.
    approval_password: Zeroizing<String>,
    approval_error: Option<String>,
    #[cfg(target_os = "linux")]
    wm: Option<WmSync>,
    /// Takes a copied hidden or encrypted note off the clipboard again.
    pub clipboard: clipboard::Watch,
}

impl App {
    pub fn new(cc: &eframe::CreationContext<'_>, state: State, path: Option<PathBuf>, startup_error: Option<String>) -> Self {
        #[allow(unused_mut)]
        let mut app = Self::with_state_inner(&cc.egui_ctx, state, path, startup_error);
        #[cfg(target_os = "linux")]
        if let Some(hypr) = crate::hyprland::Hyprland::detect() {
            let (tx, observed) = mpsc::channel();
            let (watcher, ctx) = (hypr.clone(), cc.egui_ctx.clone());
            std::thread::spawn(move || {
                let mut last = None;
                loop {
                    std::thread::sleep(std::time::Duration::from_secs(1));
                    let Some(now) = watcher.window_state() else { continue };
                    if last != Some(now) {
                        last = Some(now);
                        if tx.send(now).is_err() {
                            break;
                        }
                        ctx.request_repaint();
                    }
                }
            });
            app.wm = Some(WmSync {
                hypr,
                observed,
                pending: Some((app.session.state.pinned, f64::NAN)),
                restore: true,
                ignore_until: 0.0,
                pinned: std::sync::Arc::new(app.session.state.pinned.into()),
            });
        }
        app
    }

    /// Carries out what another launch asks for (`scrpad`, `--toggle`, `--new`, `-`). A hidden
    /// window draws no frames, so showing happens right here; hiding goes through `draw`, which
    /// saves first and is running then, since only a focused window hides. A note is added by
    /// `draw` too, on the first frame the shown window draws. Script commands (`--append`,
    /// `--list`, `--read`) are answered right here as well, from the notes file, and leave the
    /// window as it is. `--write` and `--run` bring the window here and wait for the user's answer
    /// in `approval_dialog`.
    pub fn serve_commands(&self, ctx: &egui::Context, listener: interprocess::local_socket::Listener) {
        use crate::cli::Command;
        #[cfg(target_os = "linux")]
        let hypr = self.wm.as_ref().map(|wm| (wm.hypr.clone(), wm.pinned.clone()));
        let (ctx, hide, notes) = (ctx.clone(), self.session.hide_requests.0.clone(), self.session.note_requests.0.clone());
        let answer_script = self.session.script_server();
        let answer_approval = self.session.approval_server();
        // Brings the window here, wherever it is.
        let show = {
            let ctx = ctx.clone();
            #[cfg(target_os = "linux")]
            let hypr = hypr.clone();
            move || {
                #[cfg(target_os = "linux")]
                if let Some((hypr, pinned)) = &hypr {
                    hypr.show(pinned.load(std::sync::atomic::Ordering::Relaxed));
                    ctx.request_repaint();
                    return;
                }
                ctx.send_viewport_cmd(ViewportCommand::Minimized(false));
                ctx.send_viewport_cmd(ViewportCommand::Focus);
                ctx.request_repaint();
            }
        };
        let asks = self.session.asks();
        crate::instance::serve(listener, move |command| {
            if asks(&command) {
                // The question must be seen: bring the window here as `scrpad` does, then wait.
                show();
                return answer_approval(&command);
            }
            if command.is_script() {
                return answer_script(&command);
            }
            if let Command::Note(text) = &command {
                let _ = notes.send(text.clone());
            }
            #[cfg(target_os = "linux")]
            if let Some((hypr, _)) = &hypr {
                // Focused in a shown special workspace (SUPER+SHIFT+H) is still hidden: bring it here.
                if command == Command::Toggle && hypr.is_focused() && !hypr.window_state().is_some_and(|w| w.hidden) {
                    let _ = hide.send(());
                    ctx.request_repaint();
                } else {
                    show();
                }
                return Reply::Ok;
            }
            if command == Command::Toggle && ctx.input(|i| i.viewport().focused == Some(true)) {
                let _ = hide.send(());
                ctx.request_repaint();
            } else {
                show();
            }
            Reply::Ok
        });
    }

    /// `path: None` keeps everything in memory (tests).
    pub fn with_state(ctx: &egui::Context, state: State, path: Option<PathBuf>) -> Self {
        Self::with_state_inner(ctx, state, path, None)
    }

    fn with_state_inner(ctx: &egui::Context, state: State, path: Option<PathBuf>, startup_error: Option<String>) -> Self {
        egui_extras::install_image_loaders(ctx);
        let repaint = ctx.clone();
        Self {
            session: Session::new(state, path, startup_error, Arc::new(move |delay| repaint.request_repaint_after(delay))),
            capture: CaptureGuard::default(),
            force_close: false,
            fonts_ready: false,
            applied_theme: None,
            popup: None,
            renaming: None,
            dragging: None,
            scrolled_to: None,
            encrypt_form: Default::default(),
            unlock_form: Default::default(),
            lock_button: Rect::NOTHING,
            approval_password: Zeroizing::default(),
            approval_error: None,
            #[cfg(target_os = "linux")]
            wm: None,
            clipboard: clipboard::Watch::system(),
        }
    }

    fn tokens(&self) -> Tokens {
        Tokens::of(self.session.state.theme)
    }

    // ---- actions (also used by keyboard shortcuts and tests) -------------------------------

    pub fn close_note(&mut self, i: usize, now: f64) {
        self.session.close_note(i, now);
        self.renaming = None;
    }

    pub fn start_rename(&mut self) {
        if self.renaming.is_none() {
            self.renaming = Some((self.session.state.current().id, self.session.state.current().title.clone(), true));
        }
    }

    /// Steps away after `IDLE_SECS` without input, counting any input in this frame.
    fn check_idle(&mut self, ctx: &egui::Context, now: f64) {
        let input = ctx.input(|i| {
            i.events.iter().any(|e| {
                use egui::Event::*;
                matches!(e, Key { .. } | Text(_) | Paste(_) | Copy | Cut | PointerButton { .. } | PointerMoved(_) | MouseWheel { .. })
            })
        });
        self.session.check_idle(now, input);
    }

    /// Hides the window: Hyprland moves it to a special workspace, elsewhere it is minimized.
    /// Pending changes are saved first and in full (re-sealing on this thread): a hidden window
    /// draws no frames, so the debounced autosave would wait until it is shown again. For the
    /// same reason notes are locked and hidden again right here, not on a later frame.
    pub fn hide_window(&mut self, ctx: &egui::Context) {
        self.popup = None;
        self.session.step_away(ctx.input(|i| i.time));
        if self.session.dirty_since.is_some() && !self.session.save_now() {
            // Kept dirty and retried on the next frame, as after any failed save.
            ctx.request_repaint();
        }
        #[cfg(target_os = "linux")]
        if let Some(wm) = &mut self.wm {
            // The unpinned window seen in between is our doing, not the user's.
            wm.ignore_until = ctx.input(|i| i.time) + 1.5;
            if wm.hypr.hide() {
                return;
            }
        }
        ctx.send_viewport_cmd(ViewportCommand::Minimized(true));
    }

    pub fn toggle_pin(&mut self, ctx: &egui::Context, now: f64) {
        self.session.state.pinned = !self.session.state.pinned;
        self.session.mark_dirty(now);
        #[cfg(target_os = "linux")]
        if let Some(wm) = &mut self.wm {
            wm.pending = Some((self.session.state.pinned, now + 2.0));
            return;
        }
        let level = if self.session.state.pinned { egui::WindowLevel::AlwaysOnTop } else { egui::WindowLevel::Normal };
        ctx.send_viewport_cmd(ViewportCommand::WindowLevel(level));
    }

    pub fn toggle_hide_from_capture(&mut self, now: f64) {
        self.session.state.hide_from_capture = !self.session.state.hide_from_capture;
        self.session.mark_dirty(now);
    }

    /// Whether this desktop lets the window stay out of screen capture: Windows and macOS do,
    /// on Linux only Hyprland (Wayland has no protocol for it, X11 lets anyone read the screen).
    #[cfg(target_os = "linux")]
    fn can_hide_from_capture(&self) -> bool {
        self.wm.is_some()
    }

    #[cfg(not(target_os = "linux"))]
    fn can_hide_from_capture(&self) -> bool {
        true
    }

    /// Asks the window system to keep the window out of screen capture, or not.
    fn set_capture_protection(&self, ctx: &egui::Context, on: bool) -> bool {
        #[cfg(target_os = "linux")]
        if let Some(wm) = &self.wm {
            return wm.hypr.set_no_screen_share(on);
        }
        // Windows and macOS; elsewhere winit does nothing with it.
        ctx.send_viewport_cmd(ViewportCommand::ContentProtected(on));
        true
    }

    /// Brings the window's capture protection in line with the option and the open note.
    /// Returns whether a secret may be drawn this frame: only once protection was in effect
    /// before it, or where the desktop offers none.
    fn guard_capture(&mut self, ctx: &egui::Context, now: f64) -> bool {
        let secret = self.session.open_note_is_secret();
        let want = self.session.state.hide_from_capture || secret;
        let protected = self.capture.applied == Some(true);
        if self.capture.applied != Some(want) {
            if self.set_capture_protection(ctx, want) {
                self.capture = CaptureGuard { applied: Some(want), deadline: None };
            } else if now < *self.capture.deadline.get_or_insert(now + 5.0) {
                ctx.request_repaint_after(std::time::Duration::from_millis(100));
            } else {
                // This compositor won't: the note shows, as on a desktop without the means.
                self.capture = CaptureGuard { applied: Some(want), deadline: None };
            }
        }
        if secret && !protected {
            ctx.request_repaint();
            return false;
        }
        true
    }

    pub fn copy(&mut self, ctx: &egui::Context, now: f64) {
        if self.session.showing_cipher()
            && let Some(cipher) = self.session.state.current().cipher.clone()
        {
            ctx.copy_text(cipher);
            self.clipboard.other_copied();
            self.session.toast(now, "Copied the encrypted text", None);
            return;
        }
        if self.session.state.current().is_locked() {
            self.session.toast(now, "Unlock the note first", None);
            return;
        }
        let note = self.session.state.current();
        if note.text.is_empty() {
            self.session.toast(now, "Nothing to copy", None);
            return;
        }
        let secret = note.hidden || note.is_encrypted();
        ctx.copy_text(note.text.clone());
        let msg = if !secret {
            self.clipboard.other_copied();
            "Copied to clipboard".to_owned()
        } else if self.clipboard.secret_copied(&self.session.state.current().text) {
            format!("Copied, clipboard clears in {} s", clipboard::CLEAR_AFTER.as_secs())
        } else {
            "Copied; the clipboard can't be cleared here".to_owned()
        };
        self.session.toast(now, msg, None);
    }

    // ---- encryption ---------------------------------------------------------------------------

    pub fn unlock_note(&mut self, password: &str, now: f64) -> Result<(), crypto::Error> {
        self.session.unlock_note(password, now)?;
        self.unlock_form = Default::default();
        Ok(())
    }

    /// The lock button and Ctrl+Shift+L: encrypt a plain note, lock an open one, unlock a locked one.
    fn lock_action(&mut self, anchor: Rect, now: f64) {
        let note = self.session.state.current();
        if !note.is_encrypted() {
            self.encrypt_form = EncryptForm { note: note.id, grab: true, ..Default::default() };
            self.toggle_popup(Popup::Encrypt, anchor);
        } else if note.is_locked() {
            self.session.show_cipher = None;
            self.session.focus_editor = true;
        } else {
            self.session.lock_note(now);
        }
    }

    /// Opens the native save dialog on a worker thread so the window keeps drawing.
    /// An encrypted note is saved as its encrypted message: its plaintext never goes to disk.
    pub fn export(&mut self) {
        self.session.seal_sync(self.session.state.active);
        let note = self.session.state.current();
        let title = note.title.clone();
        let contents = note.cipher.clone().unwrap_or_else(|| note.text.clone());
        let (tx, rx) = mpsc::channel();
        self.session.saves = Some(rx);
        let run = move || {
            let name = format!("{}.txt", sanitize(&title));
            let picked = rfd::FileDialog::new()
                .set_title("Save note")
                .set_file_name(name)
                .add_filter("Text", &["txt", "md"])
                .add_filter("All files", &["*"])
                .save_file();
            if let Some(path) = picked {
                let _ = tx.send(std::fs::write(&path, contents).map(|_| path).map_err(|e| e.to_string()));
            }
        };
        // AppKit dialogs must run on the main thread; elsewhere a worker keeps the window drawing.
        if cfg!(target_os = "macos") {
            run();
        } else {
            std::thread::spawn(run);
        }
    }

    fn shortcuts(&mut self, ctx: &egui::Context, now: f64) {
        use egui::{Key, KeyboardShortcut, Modifiers};
        let cmd = |key| KeyboardShortcut::new(Modifiers::COMMAND, key);
        let cmd_shift = |key| KeyboardShortcut::new(Modifiers::COMMAND | Modifiers::SHIFT, key);
        if ctx.input_mut(|i| i.consume_shortcut(&cmd(Key::N)) || i.consume_shortcut(&cmd(Key::T))) {
            self.session.add_note(now);
        }
        if ctx.input_mut(|i| i.consume_shortcut(&cmd(Key::W))) {
            self.close_note(self.session.state.active, now);
        }
        if ctx.input_mut(|i| i.consume_shortcut(&KeyboardShortcut::new(Modifiers::NONE, Key::F2))) {
            self.start_rename();
        }
        if ctx.input_mut(|i| i.consume_shortcut(&cmd(Key::S))) {
            self.export();
        }
        if ctx.input_mut(|i| i.consume_shortcut(&cmd_shift(Key::C))) {
            self.copy(ctx, now);
        }
        if ctx.input_mut(|i| i.consume_shortcut(&cmd_shift(Key::H))) {
            self.session.toggle_hidden(now);
        }
        // After Ctrl+Shift+H: consume_shortcut ignores an extra Shift.
        if ctx.input_mut(|i| i.consume_shortcut(&cmd(Key::H))) {
            self.hide_window(ctx);
        }
        if ctx.input_mut(|i| i.consume_shortcut(&cmd_shift(Key::L))) {
            self.lock_action(self.lock_button, now);
        }
        if ctx.input_mut(|i| i.consume_shortcut(&cmd_shift(Key::K))) {
            self.session.lock_all_now(now);
        }
        let n = self.session.state.notes.len();
        // Shift first: consume_shortcut ignores extra modifiers, so Ctrl+Tab would also match it.
        if ctx.input_mut(|i| i.consume_shortcut(&KeyboardShortcut::new(Modifiers::CTRL | Modifiers::SHIFT, Key::Tab))) {
            self.session.state.active = (self.session.state.active + n - 1) % n;
            self.session.focus_editor = true;
        }
        if ctx.input_mut(|i| i.consume_shortcut(&KeyboardShortcut::new(Modifiers::CTRL, Key::Tab))) {
            self.session.state.active = (self.session.state.active + 1) % n;
            self.session.focus_editor = true;
        }
        if ctx.input_mut(|i| i.consume_shortcut(&cmd_shift(Key::E))) {
            self.session.toggle_cipher_view(now);
        }
        if ctx.input(|i| i.key_pressed(Key::Escape)) {
            if self.popup.is_some() {
                self.popup = None;
            } else if self.session.showing_cipher() {
                self.session.toggle_cipher_view(now);
            }
        }
    }

    // ---- drawing ----------------------------------------------------------------------------

    fn window_frame(&self, ui: &egui::Ui, rect: Rect, t: &Tokens) {
        let p = ui.painter();
        // A soft vertical gradient: a mesh of two colours, clipped by the rounded body drawn on top of it.
        p.rect_filled(rect, t.radius, t.bg_bottom);
        let mut mesh = egui::Mesh::default();
        let fade =
            Rect::from_min_max(Pos2::new(rect.min.x + 1.0, rect.min.y + t.radius), Pos2::new(rect.max.x - 1.0, rect.max.y - t.radius));
        mesh.colored_vertex(fade.left_top(), t.bg_top);
        mesh.colored_vertex(fade.right_top(), t.bg_top);
        mesh.colored_vertex(fade.left_bottom(), t.bg_bottom);
        mesh.colored_vertex(fade.right_bottom(), t.bg_bottom);
        mesh.add_triangle(0, 1, 2);
        mesh.add_triangle(1, 2, 3);
        p.add(mesh);
        p.rect_filled(
            Rect::from_min_max(rect.min, Pos2::new(rect.max.x, rect.min.y + t.radius)),
            egui::CornerRadius { nw: t.radius as u8, ne: t.radius as u8, sw: 0, se: 0 },
            t.bg_top,
        );
        p.rect_stroke(rect, t.radius, Stroke::new(1.0, t.border), egui::StrokeKind::Inside);
    }

    fn in_resize_band(&self, ui: &egui::Ui) -> bool {
        let window = ui.ctx().content_rect();
        ui.input(|i| i.pointer.press_origin()).is_some_and(|p| !window.shrink(RESIZE_EDGE).contains(p))
    }

    /// Lets the borderless window be resized from its edges and corners.
    fn resize_edges(&self, ui: &egui::Ui, rect: Rect) {
        let Some(pos) = ui.ctx().pointer_hover_pos() else { return };
        if !rect.contains(pos) {
            return;
        }
        let e = RESIZE_EDGE;
        let (l, r) = (pos.x < rect.left() + e, pos.x > rect.right() - e);
        let (t, b) = (pos.y < rect.top() + e, pos.y > rect.bottom() - e);
        let dir = match (l, r, t, b) {
            (true, _, true, _) => ResizeDirection::NorthWest,
            (_, true, true, _) => ResizeDirection::NorthEast,
            (true, _, _, true) => ResizeDirection::SouthWest,
            (_, true, _, true) => ResizeDirection::SouthEast,
            (true, ..) => ResizeDirection::West,
            (_, true, ..) => ResizeDirection::East,
            (_, _, true, _) => ResizeDirection::North,
            (_, _, _, true) => ResizeDirection::South,
            _ => return,
        };
        let icon = match dir {
            ResizeDirection::NorthWest | ResizeDirection::SouthEast => CursorIcon::ResizeNwSe,
            ResizeDirection::NorthEast | ResizeDirection::SouthWest => CursorIcon::ResizeNeSw,
            ResizeDirection::West | ResizeDirection::East => CursorIcon::ResizeHorizontal,
            _ => CursorIcon::ResizeVertical,
        };
        ui.ctx().set_cursor_icon(icon);
        if ui.input(|i| i.pointer.primary_pressed()) {
            ui.ctx().send_viewport_cmd(ViewportCommand::BeginResize(dir));
        }
    }

    fn header(&mut self, ui: &mut egui::Ui, rect: Rect, t: &Tokens, now: f64) {
        // Empty header space moves the window.
        let drag = ui.interact(rect, Id::new("drag-header"), Sense::click_and_drag());
        // A press in the resize band belongs to resize_edges, not to the window drag.
        if drag.drag_started() && !self.in_resize_band(ui) {
            ui.ctx().send_viewport_cmd(ViewportCommand::StartDrag);
        }
        ui.painter().text(
            Pos2::new(rect.left() + PAD + 2.0, rect.center().y + 2.0),
            Align2::LEFT_CENTER,
            "Scratchpad",
            theme::semibold(14.5),
            t.title,
        );
        let close = Rect::from_center_size(Pos2::new(rect.right() - PAD - 10.0, rect.center().y + 2.0), Vec2::splat(24.0));
        let resp = ui.interact(close, Id::new("close"), Sense::click()).on_hover_cursor(CursorIcon::PointingHand);
        resp.widget_info(|| egui::WidgetInfo::labeled(egui::WidgetType::Button, true, "Close window"));
        let fill = if resp.hovered() { t.text_muted } else { t.close_fill };
        ui.painter().circle_filled(close.center(), 9.5, fill);
        icons::paint(ui, close, "x", 12.0, t.close_glyph);
        if resp.on_hover_text("Close").clicked() && self.session.save_now() {
            ui.ctx().send_viewport_cmd(ViewportCommand::Close);
        }
        let pin = Rect::from_center_size(close.center() - Vec2::new(38.0, 0.0), Vec2::splat(28.0));
        let tint = if self.session.state.pinned { t.accent } else { t.icon_faint };
        let tip = if self.session.state.pinned { "Floating on top (click to unpin)" } else { "Float on top" };
        if icon_button(ui, pin, if self.session.state.pinned { "pin-filled" } else { "pin" }, 19.0, tint, t, "Float on top")
            .on_hover_text(tip)
            .clicked()
        {
            self.toggle_pin(ui.ctx(), now);
        }
        let capture = pin.translate(Vec2::new(-34.0, 0.0));
        let always = self.session.state.hide_from_capture;
        let hidden_now = always || self.session.open_note_is_secret();
        let icon = if hidden_now { "screen-share-off" } else { "screen-share" };
        let tint = if always { t.accent } else { t.icon_faint };
        let tip = if !self.can_hide_from_capture() {
            "Hide from screen capture (this desktop doesn't allow it)"
        } else if always {
            "Hidden from screenshots and screen sharing (click to show)"
        } else if hidden_now {
            "Hidden from screen capture while this note is open (click to hide always)"
        } else {
            "Hide from screenshots and screen sharing"
        };
        if icon_button(ui, capture, icon, 18.0, tint, t, "Hide from screen capture").on_hover_text(tip).clicked() {
            self.toggle_hide_from_capture(now);
        }
    }

    fn tabs(&mut self, ui: &mut egui::Ui, rect: Rect, t: &Tokens, now: f64) {
        let drag = ui.interact(rect, Id::new("drag-tabs"), Sense::click_and_drag());
        if drag.drag_started() && !self.in_resize_band(ui) {
            ui.ctx().send_viewport_cmd(ViewportCommand::StartDrag);
        }
        if drag.double_clicked() {
            self.session.add_note(now);
        }
        // Right side: add and more.
        let more = Rect::from_center_size(Pos2::new(rect.right() - PAD - 10.0, rect.center().y), Vec2::splat(28.0));
        let add = more.translate(Vec2::new(-34.0, 0.0));
        if icon_button(ui, add, "plus", 20.0, t.icon, t, "New note").on_hover_text("New note (Ctrl+N)").clicked() {
            self.session.add_note(now);
        }
        let more_resp = icon_button(ui, more, "ellipsis", 20.0, t.icon, t, "More").on_hover_text("More");
        if more_resp.clicked() {
            self.toggle_popup(Popup::More, more);
        }

        let strip = Rect::from_min_max(Pos2::new(rect.left() + PAD - 2.0, rect.top()), Pos2::new(add.left() - 6.0, rect.bottom()));
        let mut child = ui.new_child(egui::UiBuilder::new().max_rect(strip).layout(egui::Layout::left_to_right(egui::Align::Center)));
        child.set_clip_rect(strip.intersect(ui.clip_rect()));
        let mut switch_to = None;
        let mut close = None;
        let mut rects = Vec::with_capacity(self.session.state.notes.len());
        egui::ScrollArea::horizontal()
            .id_salt("tab-strip")
            .scroll_bar_visibility(egui::scroll_area::ScrollBarVisibility::AlwaysHidden)
            .show(&mut child, |ui| {
                ui.spacing_mut().item_spacing.x = 4.0;
                for i in 0..self.session.state.notes.len() {
                    match self.tab(ui, i, t, now, &mut rects) {
                        TabAction::None => {}
                        TabAction::Select => switch_to = Some(i),
                        TabAction::Close => close = Some(i),
                    }
                }
            });
        // Tabs that overflow fade out instead of being cut off at a hard edge.
        let fade = Rect::from_min_max(Pos2::new(strip.right() - 14.0, strip.top()), strip.right_bottom());
        let mut mesh = egui::Mesh::default();
        let bg = t.bg_top;
        mesh.colored_vertex(fade.left_top(), Color32::TRANSPARENT);
        mesh.colored_vertex(fade.right_top(), bg);
        mesh.colored_vertex(fade.left_bottom(), Color32::TRANSPARENT);
        mesh.colored_vertex(fade.right_bottom(), bg);
        mesh.add_triangle(0, 1, 2);
        mesh.add_triangle(1, 2, 3);
        ui.painter().add(mesh);
        if let Some(dragging) = self.dragging {
            self.drag_tab(ui.ctx(), dragging, &rects, strip, t, now);
        }
        if let Some(i) = switch_to
            && self.session.state.active != i
        {
            self.session.state.active = i;
            self.session.focus_editor = true;
            self.session.mark_dirty(now);
        }
        if let Some(i) = close {
            self.close_note(i, now);
        }
    }

    /// A dragged tab follows the pointer and trades places with a neighbour once the pointer
    /// passes that neighbour's middle; its place stays faintly marked. `rects` are where the
    /// tabs are drawn, in order.
    fn drag_tab(&mut self, ctx: &egui::Context, (id, grab): (u64, f32), rects: &[Rect], strip: Rect, t: &Tokens, now: f64) {
        let (pos, down) = ctx.input(|i| (i.pointer.latest_pos(), i.pointer.primary_down()));
        let from = self.session.state.notes.iter().position(|n| n.id == id).filter(|&i| i < rects.len());
        let (Some(from), Some(pos), true) = (from, pos, down) else {
            self.dragging = None;
            return;
        };
        ctx.set_cursor_icon(CursorIcon::Grabbing);
        let to = if rects.get(from + 1).is_some_and(|r| pos.x > r.center().x) {
            from + 1
        } else if from > 0 && pos.x < rects[from - 1].center().x {
            from - 1
        } else {
            from
        };
        if to != from {
            // One place per frame: the next frame lays the tabs out anew and goes on from there.
            self.session.state.move_note(from, to);
            self.session.mark_dirty(now);
            ctx.request_repaint();
            return;
        }
        let painter = ctx.layer_painter(egui::LayerId::new(egui::Order::Tooltip, Id::new("tab-drag"))).with_clip_rect(strip);
        let slot = rects[from];
        painter.rect_filled(slot, 7.0, t.bg_top);
        painter.rect_stroke(slot, 7.0, Stroke::new(1.0, t.divider), egui::StrokeKind::Inside);
        let ghost = Rect::from_min_size(Pos2::new(pos.x - grab, slot.top()), slot.size());
        painter.rect_filled(ghost.translate(Vec2::new(0.0, 2.0)), 7.0, Color32::from_black_alpha(40));
        painter.rect_filled(ghost, 7.0, t.tab_selected);
        painter.rect_stroke(ghost, 7.0, Stroke::new(1.0, t.border), egui::StrokeKind::Inside);
        let title = &self.session.state.notes[from].title;
        painter.text(ghost.left_center() + Vec2::new(10.0, 0.0), Align2::LEFT_CENTER, title, theme::semibold(14.0), t.text);
    }

    fn tab(&mut self, ui: &mut egui::Ui, i: usize, t: &Tokens, now: f64, rects: &mut Vec<Rect>) -> TabAction {
        let selected = i == self.session.state.active;
        let h = 28.0;
        // Inline rename.
        if let Some((id, buf, grab)) = &mut self.renaming
            && *id == self.session.state.notes[i].id
        {
            let edit = egui::TextEdit::singleline(buf)
                .font(theme::semibold(14.0))
                .desired_width(110.0)
                .margin(egui::Margin::symmetric(8, 5))
                .background_color(t.tab_selected);
            let resp = ui.add(edit);
            rects.push(resp.rect);
            if std::mem::take(grab) {
                resp.request_focus();
            }
            let commit = resp.lost_focus() || ui.input(|i| i.key_pressed(egui::Key::Enter));
            if ui.input(|i| i.key_pressed(egui::Key::Escape)) {
                self.renaming = None;
            } else if commit {
                let title = buf.trim().to_owned();
                if !title.is_empty() {
                    self.session.state.notes[i].title = title;
                }
                self.renaming = None;
                self.session.focus_editor = true;
                self.session.mark_dirty(now);
            }
            return TabAction::None;
        }

        let title = self.session.state.notes[i].title.clone();
        let font = if selected { theme::semibold(14.0) } else { theme::regular(14.0) };
        let color = if selected { t.text } else { t.text_muted };
        let galley = ui.painter().layout_no_wrap(title, font, color);
        let x_w = if selected { 20.0 } else { 0.0 };
        let lock_w = if self.session.state.notes[i].is_encrypted() { 16.0 } else { 0.0 };
        let warn_w = if self.session.findings(self.session.state.notes[i].id).is_empty() { 0.0 } else { 16.0 };
        let size = Vec2::new(galley.size().x + 20.0 + x_w + lock_w + warn_w, h);
        // Dragging sideways reorders; a click still selects and a double-click renames. The id
        // follows the note, so a drag carries on while the tab changes places.
        let (rect, _) = ui.allocate_exact_size(size, Sense::hover());
        let resp = ui.interact(rect, Id::new(("tab", self.session.state.notes[i].id)), Sense::click_and_drag());
        rects.push(rect);
        if resp.drag_started()
            && let Some(pos) = resp.interact_pointer_pos()
        {
            self.dragging = Some((self.session.state.notes[i].id, pos.x - rect.left()));
        }
        let resp = resp.on_hover_cursor(CursorIcon::PointingHand);
        resp.widget_info(|| {
            egui::WidgetInfo::selected(egui::WidgetType::SelectableLabel, true, selected, &self.session.state.notes[i].title)
        });
        if selected {
            ui.painter().rect_filled(rect, 7.0, t.tab_selected);
        } else if resp.hovered() {
            ui.painter().rect_filled(rect, 7.0, t.hover);
        }
        if lock_w > 0.0 {
            let name = if self.session.state.notes[i].is_locked() { "lock" } else { "lock-open" };
            icons::paint(
                ui,
                Rect::from_center_size(Pos2::new(rect.left() + 16.0, rect.center().y), Vec2::splat(13.0)),
                name,
                13.0,
                t.secure,
            );
        }
        if warn_w > 0.0 {
            let mark = Rect::from_center_size(Pos2::new(rect.left() + 16.0 + lock_w, rect.center().y), Vec2::splat(14.0));
            icons::paint(ui, mark, "triangle-alert", 13.0, t.error);
            let findings = self.session.findings(self.session.state.notes[i].id);
            let mut tip = String::from("May hold instructions for an AI:");
            for f in findings.iter().take(10) {
                tip.push_str(&format!("\nLine {}: {}", f.line, f.kind.label()));
            }
            if findings.len() > 10 {
                tip.push_str(&format!("\n…and {} more", findings.len() - 10));
            }
            let warn = ui.interact(mark, Id::new(("tab-warn", self.session.state.notes[i].id)), Sense::hover());
            warn.widget_info(|| egui::WidgetInfo::labeled(egui::WidgetType::Label, true, "Warnings"));
            warn.on_hover_text(tip);
        }
        ui.painter().galley(Pos2::new(rect.left() + 10.0 + lock_w + warn_w, rect.center().y - galley.size().y / 2.0), galley, color);
        let id = self.session.state.notes[i].id;
        if selected && self.scrolled_to != Some(id) {
            self.scrolled_to = Some(id);
            ui.scroll_to_rect(rect, None);
        }
        let mut action = TabAction::None;
        if selected {
            let x_rect = Rect::from_center_size(Pos2::new(rect.right() - 16.0, rect.center().y), Vec2::splat(18.0));
            let x = ui.interact(x_rect, Id::new(("tab-x", i)), Sense::click()).on_hover_cursor(CursorIcon::PointingHand);
            x.widget_info(|| egui::WidgetInfo::labeled(egui::WidgetType::Button, true, "Close note"));
            if x.hovered() {
                ui.painter().rect_filled(x_rect, 4.0, t.hover);
            }
            icons::paint(ui, x_rect, "x", 13.0, if x.hovered() { t.text } else { t.text_muted });
            if x.on_hover_text("Close note (Ctrl+W)").clicked() {
                return TabAction::Close;
            }
        }
        if resp.double_clicked() {
            self.renaming = Some((self.session.state.notes[i].id, self.session.state.notes[i].title.clone(), true));
        } else if resp.clicked() {
            action = TabAction::Select;
        } else if resp.middle_clicked() {
            action = TabAction::Close;
        }
        resp.on_hover_text("Double-click to rename (F2)");
        action
    }

    fn editor(&mut self, ui: &mut egui::Ui, rect: Rect, t: &Tokens, now: f64) {
        if self.session.showing_cipher() {
            self.cipher_view(ui, rect, t);
            return;
        }
        if self.session.state.current().is_locked() {
            self.unlock_view(ui, rect, t, now);
            return;
        }
        let font = match self.session.state.font {
            FontStyle::Sans => egui::FontId::proportional(self.session.state.font_size),
            FontStyle::Mono => egui::FontId::monospace(self.session.state.font_size - 1.0),
        };
        let mut child = ui.new_child(egui::UiBuilder::new().max_rect(rect).layout(egui::Layout::top_down(egui::Align::Min)));
        child.set_clip_rect(rect.intersect(ui.clip_rect()));
        let hidden = self.session.state.current().hidden;
        let focus = std::mem::take(&mut self.session.focus_editor);
        let placeholder = egui::RichText::new("Type anything. It saves by itself.").color(t.placeholder).font(font.clone());
        let mut changed = false;
        egui::ScrollArea::vertical().id_salt(("editor", self.session.state.current().id)).auto_shrink(false).show(&mut child, |ui| {
            let note = &mut self.session.state.notes[self.session.state.active];
            let mut mask = |ui: &egui::Ui, text: &dyn egui::TextBuffer, wrap: f32| {
                // Hidden notes keep their line structure but show only dots; the text itself is untouched.
                let shown: String = text.as_str().chars().map(|c| if c == '\n' { '\n' } else { '•' }).collect();
                let mut job = egui::text::LayoutJob::simple(shown, font.clone(), t.text, wrap);
                job.wrap.max_width = wrap;
                ui.fonts_mut(|f| f.layout_job(job))
            };
            let mut edit = egui::TextEdit::multiline(&mut note.text)
                .id(Id::new(("editor", note.id)))
                .frame(egui::Frame::NONE)
                .font(font.clone())
                .text_color(t.text)
                .hint_text(placeholder)
                .desired_width(f32::INFINITY)
                .min_size(Vec2::new(rect.width(), rect.height()))
                .margin(egui::Margin::symmetric(0, 0))
                .lock_focus(true);
            if hidden {
                edit = edit.layouter(&mut mask);
            }
            let resp = edit.show(ui).response;
            if focus {
                resp.request_focus();
            }
            changed = resp.changed();
        });
        if changed {
            self.session.mark_dirty(now);
        }
        if hidden && self.session.state.current().text.is_empty() {
            ui.painter().text(Pos2::new(rect.right(), rect.top()), Align2::RIGHT_TOP, "hidden", theme::regular(11.0), t.placeholder);
        }
    }

    /// The note as it is saved: the SimpleTextCrypt message, selectable but read-only.
    fn cipher_view(&mut self, ui: &mut egui::Ui, rect: Rect, t: &Tokens) {
        let cipher = self.session.state.current().cipher.clone().unwrap_or_default();
        let label = Rect::from_min_max(rect.min, Pos2::new(rect.right(), rect.top() + 20.0));
        let icon = Rect::from_min_size(label.min, Vec2::splat(16.0));
        icons::paint(ui, icon, "lock", 14.0, t.secure);
        ui.painter().text(
            Pos2::new(icon.right() + 6.0, icon.center().y),
            Align2::LEFT_CENTER,
            "Encrypted, as saved to disk · Esc to go back",
            theme::regular(12.0),
            t.secure,
        );
        let body = Rect::from_min_max(Pos2::new(rect.left(), label.bottom() + 6.0), rect.max);
        let mut child = ui.new_child(egui::UiBuilder::new().max_rect(body).layout(egui::Layout::top_down(egui::Align::Min)));
        child.set_clip_rect(body.intersect(ui.clip_rect()));
        egui::ScrollArea::vertical().id_salt("cipher-view").auto_shrink(false).show(&mut child, |ui| {
            let mut shown = cipher.as_str();
            let edit = egui::TextEdit::multiline(&mut shown)
                .id(Id::new("cipher-view"))
                .frame(egui::Frame::NONE)
                .font(egui::FontId::monospace((self.session.state.font_size - 2.0).max(10.0)))
                .text_color(t.text_muted)
                .desired_width(f32::INFINITY)
                .margin(egui::Margin::symmetric(0, 0));
            let resp = ui.add(edit);
            resp.widget_info(|| egui::WidgetInfo::labeled(egui::WidgetType::TextEdit, true, "Encrypted text"));
        });
    }

    /// A locked note shows only a password field: its text is not in memory at all.
    fn unlock_view(&mut self, ui: &mut egui::Ui, rect: Rect, t: &Tokens, now: f64) {
        let id = self.session.state.current().id;
        if self.unlock_form.0 != id {
            self.unlock_form = (id, String::new(), None);
        }
        let focus = std::mem::take(&mut self.session.focus_editor);
        let col = Rect::from_center_size(rect.center() - Vec2::new(0.0, 10.0), Vec2::new(rect.width().min(240.0), 120.0));
        let mut child = ui.new_child(egui::UiBuilder::new().max_rect(col).layout(egui::Layout::top_down(egui::Align::Center)));
        child.set_clip_rect(rect.intersect(ui.clip_rect()));
        let ui = &mut child;
        let (icon, _) = ui.allocate_exact_size(Vec2::splat(26.0), Sense::hover());
        icons::paint(ui, icon, "lock", 24.0, t.secure);
        ui.add_space(4.0);
        ui.label(egui::RichText::new("Encrypted note").font(theme::semibold(14.0)).color(t.text));
        ui.add_space(8.0);
        let mut submit = false;
        ui.horizontal(|ui| {
            let field_w = (ui.available_width() - 76.0).max(80.0);
            let edit = egui::TextEdit::singleline(&mut self.unlock_form.1)
                .id(Id::new("unlock-password"))
                .password(true)
                .hint_text("Password")
                .background_color(t.bg_bottom)
                .desired_width(field_w)
                .margin(egui::Margin::symmetric(8, 5));
            let resp = ui.add(edit);
            resp.widget_info(|| egui::WidgetInfo::labeled(egui::WidgetType::TextEdit, true, "Password"));
            if focus {
                resp.request_focus();
            }
            if resp.lost_focus() && ui.input(|i| i.key_pressed(egui::Key::Enter)) {
                submit = true;
            }
            if ui.add(egui::Button::new(egui::RichText::new("Unlock").font(theme::semibold(13.0)))).clicked() {
                submit = true;
            }
        });
        if let Some(err) = self.unlock_form.2 {
            ui.add_space(4.0);
            ui.label(egui::RichText::new(err).font(theme::regular(12.5)).color(t.error));
        }
        ui.add_space(6.0);
        let link =
            egui::Button::new(egui::RichText::new("Show encrypted text").font(theme::regular(12.5)).color(t.text_muted)).frame(false);
        if ui.add(link).on_hover_text("Ctrl+Shift+E").clicked() {
            self.session.toggle_cipher_view(now);
        }
        if submit {
            let password = std::mem::take(&mut self.unlock_form.1);
            if let Err(e) = self.unlock_note(&password, now) {
                self.unlock_form.2 = Some(if e == crypto::Error::EmptyPassword { "Type the password" } else { "Wrong password" });
                self.session.focus_editor = true;
            }
        }
    }

    fn footer(&mut self, ui: &mut egui::Ui, rect: Rect, t: &Tokens, now: f64) {
        let y = rect.center().y;
        let size = Vec2::splat(32.0);
        let mut x = rect.left() + PAD + 10.0;
        // "Aa" is text, not an icon glyph: it reads the same everywhere since the font is embedded.
        let aa = Rect::from_center_size(Pos2::new(x + 2.0, y), Vec2::new(34.0, 32.0));
        let resp = ui.interact(aa, Id::new("font"), Sense::click()).on_hover_cursor(CursorIcon::PointingHand);
        resp.widget_info(|| egui::WidgetInfo::labeled(egui::WidgetType::Button, true, "Font"));
        if resp.hovered() || self.popup.is_some_and(|(p, _)| p == Popup::Font) {
            ui.painter().rect_filled(aa, 7.0, t.hover);
        }
        ui.painter().text(aa.center(), Align2::CENTER_CENTER, "Aa", theme::regular(16.0), t.text_muted);
        if resp.on_hover_text("Font").clicked() {
            self.toggle_popup(Popup::Font, aa);
        }
        x += 40.0;
        let hidden = self.session.state.current().hidden;
        let eye = Rect::from_center_size(Pos2::new(x, y), size);
        let (name, tip) = match (hidden, self.session.state.auto_rehide) {
            (true, true) => ("eye-off", "Show note until the window hides or sits idle (Ctrl+Shift+H)"),
            (true, false) => ("eye-off", "Show note (Ctrl+Shift+H)"),
            (false, _) => ("eye", "Hide note (Ctrl+Shift+H)"),
        };
        let tint = if hidden { t.accent } else { t.icon_faint };
        if icon_button(ui, eye, name, 20.0, tint, t, "Hide note").on_hover_text(tip).clicked() {
            self.session.toggle_hidden(now);
        }
        x += 38.0;
        // Encryption gets its own icon and colour: unlike hiding, it changes what reaches the disk.
        let lock = Rect::from_center_size(Pos2::new(x, y), size);
        self.lock_button = lock;
        let note = self.session.state.current();
        let (name, tint, tip) = match (note.is_encrypted(), note.is_locked()) {
            (false, _) => ("lock-open", t.icon_faint, "Encrypt note… (Ctrl+Shift+L)"),
            (true, false) if self.session.save_error.is_some() => ("lock", t.error, "Encrypted in memory; changes are not saved"),
            (true, false) => ("lock", t.secure, "Encrypted; saved only encrypted. Click to lock now (Ctrl+Shift+L)"),
            (true, true) => ("lock", t.secure, "Locked: type the password to unlock"),
        };
        let open = self.popup.is_some_and(|(p, _)| p == Popup::Encrypt);
        let resp = icon_button(ui, lock, name, 19.0, if open { t.secure } else { tint }, t, "Encryption");
        if resp.on_hover_text(tip).clicked() {
            self.lock_action(lock, now);
        }
        x += 38.0;
        let copy = Rect::from_center_size(Pos2::new(x, y), size);
        if icon_button(ui, copy, "files", 19.0, t.icon_faint, t, "Copy").on_hover_text("Copy all (Ctrl+Shift+C)").clicked() {
            self.copy(ui.ctx(), now);
        }
        x += 38.0;
        let save = Rect::from_center_size(Pos2::new(x, y), size);
        if icon_button(ui, save, "download", 19.0, t.icon_faint, t, "Save to file").on_hover_text("Save to file… (Ctrl+S)").clicked() {
            self.export();
        }
        let trash = Rect::from_center_size(Pos2::new(rect.right() - PAD - 10.0, y), size);
        if icon_button(ui, trash, "trash-2", 19.0, t.icon_faint, t, "Clear note").on_hover_text("Clear note").clicked() {
            self.session.clear(now);
        }
    }

    fn toggle_popup(&mut self, kind: Popup, anchor: Rect) {
        self.popup = match self.popup {
            Some((k, _)) if k == kind => None,
            _ => Some((kind, anchor)),
        };
    }

    fn popups(&mut self, ctx: &egui::Context, window: Rect, t: &Tokens, now: f64) {
        let Some((kind, anchor)) = self.popup else { return };
        let (pos, pivot) = match kind {
            Popup::Font | Popup::Encrypt => (anchor.left_top() - Vec2::new(0.0, 4.0), Align2::LEFT_BOTTOM),
            Popup::More => (anchor.right_bottom() + Vec2::new(0.0, 4.0), Align2::RIGHT_TOP),
        };
        let frame = egui::Frame::new()
            .fill(t.popup)
            .stroke(Stroke::new(1.0, t.border))
            .corner_radius(8)
            .inner_margin(egui::Margin::same(5))
            .shadow(egui::Shadow { offset: [0, 4], blur: 14, spread: 0, color: Color32::from_black_alpha(70) });
        let area = egui::Area::new(Id::new("popup"))
            .order(egui::Order::Foreground)
            .fixed_pos(pos)
            .pivot(pivot)
            .constrain_to(window.shrink(4.0))
            .show(ctx, |ui| {
                frame.show(ui, |ui| {
                    ui.set_min_width(150.0);
                    ui.spacing_mut().item_spacing.y = 1.0;
                    match kind {
                        Popup::Font => self.font_menu(ui, t, now),
                        // Scrolls in a window too short for it (the minimum height is 170).
                        Popup::More => {
                            egui::ScrollArea::vertical().max_height(window.height() - 20.0).show(ui, |ui| self.more_menu(ui, t, now));
                        }
                        Popup::Encrypt => self.encrypt_menu(ui, t, now),
                    }
                });
            });
        let clicked_outside = ctx.input(|i| i.pointer.any_pressed())
            && ctx.pointer_interact_pos().is_some_and(|p| !area.response.rect.contains(p) && !anchor.contains(p));
        if clicked_outside {
            self.popup = None;
        }
    }

    fn font_menu(&mut self, ui: &mut egui::Ui, t: &Tokens, now: f64) {
        for (style, label) in [(FontStyle::Sans, "Sans – Inter"), (FontStyle::Mono, "Mono – JetBrains Mono")] {
            if menu_item(ui, t, label, "", self.session.state.font == style).clicked() {
                self.session.state.font = style;
                self.session.mark_dirty(now);
            }
        }
        separator(ui, t);
        ui.horizontal(|ui| {
            ui.add_space(6.0);
            ui.label(
                egui::RichText::new(format!("Size {:.0}", self.session.state.font_size)).color(t.text_muted).font(theme::regular(13.0)),
            );
            ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                for (label, delta) in [("A+", 1.0), ("A−", -1.0)] {
                    let b = egui::Button::new(egui::RichText::new(label).font(theme::semibold(13.0)).color(t.text)).frame(false);
                    if ui.add(b).clicked() {
                        self.session.state.font_size = (self.session.state.font_size + delta).clamp(10.0, 32.0);
                        self.session.mark_dirty(now);
                    }
                }
            });
        });
    }

    /// Sets a password for the current note: typed twice, as there is no way to recover it.
    fn encrypt_menu(&mut self, ui: &mut egui::Ui, t: &Tokens, now: f64) {
        ui.set_width(220.0);
        ui.spacing_mut().item_spacing.y = 6.0;
        ui.horizontal(|ui| {
            ui.add_space(4.0);
            let (icon, _) = ui.allocate_exact_size(Vec2::splat(16.0), Sense::hover());
            icons::paint(ui, icon, "lock", 15.0, t.secure);
            ui.label(egui::RichText::new("Encrypt note").font(theme::semibold(13.5)).color(t.text));
        });
        ui.label(
            egui::RichText::new("The text is saved to disk only encrypted. The password is not stored and cannot be recovered.")
                .font(theme::regular(12.0))
                .color(t.text_muted),
        );
        let EncryptForm { password, again, error, grab, .. } = &mut self.encrypt_form;
        let mut submit = false;
        let mut next = false;
        for (buf, label) in [(password, "Password"), (again, "Repeat password")] {
            let edit = egui::TextEdit::singleline(buf)
                .id(Id::new(("encrypt-password", label)))
                .password(true)
                .hint_text(label)
                .background_color(t.bg_bottom)
                .desired_width(f32::INFINITY)
                .margin(egui::Margin::symmetric(8, 5));
            let resp = ui.add(edit);
            resp.widget_info(|| egui::WidgetInfo::labeled(egui::WidgetType::TextEdit, true, label));
            if label == "Password" && std::mem::take(grab) || label == "Repeat password" && std::mem::take(&mut next) {
                resp.request_focus();
            }
            // Enter in the first field moves on to the second; in the second it submits.
            if resp.lost_focus() && ui.input(|i| i.key_pressed(egui::Key::Enter)) {
                if label == "Password" { next = true } else { submit = true }
            }
        }
        if let Some(e) = error {
            ui.label(egui::RichText::new(*e).font(theme::regular(12.5)).color(t.error));
        }
        ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
            if ui.add(egui::Button::new(egui::RichText::new("Encrypt").font(theme::semibold(13.0)))).clicked() {
                submit = true;
            }
        });
        if submit {
            let form = &mut self.encrypt_form;
            if form.note != self.session.state.current().id {
                // The popup belongs to another note: never encrypt whatever is active now.
                self.popup = None;
            } else if form.password.is_empty() {
                form.error = Some("Type a password");
            } else if form.password != form.again {
                form.error = Some("The passwords don't match");
            } else {
                let password = std::mem::take(&mut form.password);
                form.again.clear();
                match self.session.encrypt_note(&password, now) {
                    Ok(()) => {
                        self.popup = None;
                        self.session.focus_editor = true;
                    }
                    Err(e) => {
                        self.encrypt_form.error =
                            Some(if e == crypto::Error::EmptyPassword { "Type a password" } else { "Encryption failed" })
                    }
                }
            }
        }
    }

    fn more_menu(&mut self, ui: &mut egui::Ui, t: &Tokens, now: f64) {
        if menu_item(ui, t, "Rename note", "F2", false).clicked() {
            self.start_rename();
            self.popup = None;
        }
        if menu_item(ui, t, "Close note", "Ctrl+W", false).clicked() {
            self.close_note(self.session.state.active, now);
            self.popup = None;
        }
        let note = self.session.state.current();
        if note.is_encrypted() && menu_item(ui, t, "Show encrypted text", "Ctrl+Shift+E", self.session.showing_cipher()).clicked() {
            self.session.toggle_cipher_view(now);
            self.popup = None;
        }
        let note = self.session.state.current();
        if !note.is_encrypted() {
            if menu_item(ui, t, "Encrypt note…", "Ctrl+Shift+L", false).clicked() {
                self.encrypt_form = EncryptForm { note: note.id, grab: true, ..Default::default() };
                self.popup = Some((Popup::Encrypt, self.lock_button));
            }
        } else if !note.is_locked() {
            if menu_item(ui, t, "Lock note", "Ctrl+Shift+L", false).clicked() {
                self.session.lock_note(now);
                self.popup = None;
            }
            if menu_item(ui, t, "Remove encryption", "", false).clicked() {
                self.session.remove_encryption(now);
                self.popup = None;
            }
        }
        if self.session.state.notes.iter().any(Note::is_encrypted) && menu_item(ui, t, "Lock all", "Ctrl+Shift+K", false).clicked() {
            self.session.lock_all_now(now);
            self.popup = None;
        }
        separator(ui, t);
        let other = match self.session.state.theme {
            ThemeKind::Dark => (ThemeKind::Light, "Light theme"),
            ThemeKind::Light => (ThemeKind::Dark, "Dark theme"),
        };
        if menu_item(ui, t, other.1, "", false).clicked() {
            self.session.state.theme = other.0;
            self.session.mark_dirty(now);
            self.popup = None;
        }
        if menu_item(ui, t, "Float on top", "", self.session.state.pinned).clicked() {
            self.toggle_pin(ui.ctx(), now);
        }
        if menu_item(ui, t, "Hide from screen capture", "", self.session.state.hide_from_capture).clicked() {
            self.toggle_hide_from_capture(now);
        }
        if menu_item(ui, t, "Hide window", "Ctrl+H", false).clicked() {
            self.hide_window(ui.ctx());
        }
        separator(ui, t);
        // On hiding the window and after 5 minutes without input.
        if menu_item(ui, t, "Lock encrypted notes automatically", "", self.session.state.auto_lock).clicked() {
            self.session.state.auto_lock = !self.session.state.auto_lock;
            self.session.mark_dirty(now);
        }
        if menu_item(ui, t, "Hide notes again automatically", "", self.session.state.auto_rehide).clicked() {
            self.session.state.auto_rehide = !self.session.state.auto_rehide;
            self.session.mark_dirty(now);
        }
        // `scrpad --list` and `--read`. Hidden and encrypted notes stay out whatever this says.
        ui.add_space(2.0);
        ui.label(egui::RichText::new("Let scripts read notes").font(theme::regular(12.0)).color(t.text_muted));
        for mode in [ReadMode::Off, ReadMode::Ask, ReadMode::Always] {
            if menu_item(ui, t, mode.label(), "", self.session.state.script_reading == mode).clicked() {
                self.session.state.script_reading = mode;
                self.session.mark_dirty(now);
            }
        }
        separator(ui, t);
        let checking = self.session.update_check.is_some();
        if menu_item(ui, t, if checking { "Checking for updates…" } else { "Check for updates…" }, "", false).clicked() && !checking {
            self.session.check_for_updates(now);
            self.popup = None;
        }
        if menu_item(ui, t, "Quit", "", false).clicked() && self.session.save_now() {
            ui.ctx().send_viewport_cmd(ViewportCommand::Close);
        }
        if self.session.save_error.is_some() && menu_item(ui, t, "Quit without saving", "", false).clicked() {
            self.session.dirty_since = None;
            self.force_close = true;
            ui.ctx().send_viewport_cmd(ViewportCommand::Close);
        }
    }

    fn toast_ui(&mut self, ui: &mut egui::Ui, above: Rect, t: &Tokens, now: f64) {
        let Some(toast) = &self.session.toast else { return };
        if now > toast.until {
            self.session.toast = None;
            return;
        }
        ui.ctx().request_repaint_after(std::time::Duration::from_secs_f64(toast.until - now));
        let font = theme::regular(13.0);
        let label = toast.action.as_ref().map(ToastAction::label);
        let action_label = label.map(|l| ui.painter().layout_no_wrap(l.into(), theme::semibold(13.0), t.accent));
        let action_w = action_label.as_ref().map_or(0.0, |g| g.size().x + 14.0);
        // A long message wraps instead of running out of the window.
        let wrap = (above.width() - 48.0 - action_w).max(100.0);
        let text = ui.painter().layout(toast.text.clone(), font.clone(), t.text, wrap);
        let w = text.size().x + 24.0 + action_w;
        let h = (text.size().y + 12.0).max(28.0);
        let rect = Rect::from_center_size(Pos2::new(above.center().x, above.bottom() - 8.0 - h / 2.0), Vec2::new(w, h));
        let read_out = ui.interact(rect, Id::new("toast"), Sense::hover());
        read_out.widget_info(|| egui::WidgetInfo::labeled(egui::WidgetType::Label, true, &toast.text));
        let p = ui.painter();
        p.rect_filled(rect.translate(Vec2::new(0.0, 2.0)), 14.0, Color32::from_black_alpha(40));
        p.rect_filled(rect, 14.0, t.popup);
        p.rect_stroke(rect, 14.0, Stroke::new(1.0, t.border), egui::StrokeKind::Inside);
        p.galley(Pos2::new(rect.left() + 12.0, rect.center().y - text.size().y / 2.0), text, t.text);
        if let (Some(g), Some(label)) = (action_label, label) {
            let r = Rect::from_min_size(Pos2::new(rect.right() - 12.0 - g.size().x, rect.center().y - g.size().y / 2.0), g.size());
            ui.painter().galley(r.min, g, t.accent);
            let resp = ui.interact(r.expand(4.0), Id::new("toast-action"), Sense::click()).on_hover_cursor(CursorIcon::PointingHand);
            resp.widget_info(|| egui::WidgetInfo::labeled(egui::WidgetType::Button, true, label));
            if resp.clicked() {
                match self.session.toast.take().and_then(|t| t.action) {
                    Some(ToastAction::Undo(undo)) => self.session.undo(undo, now),
                    Some(ToastAction::Open(url)) => ui.ctx().open_url(egui::OpenUrl::new_tab(url)),
                    None => {}
                }
            }
        }
    }

    fn save_error_ui(&self, ui: &mut egui::Ui, above: Rect, t: &Tokens) {
        let Some(error) = &self.session.save_error else { return };
        let suffix = if self.session.save_blocked {
            " Export your notes, then use More → Quit without saving."
        } else {
            " Retrying automatically."
        };
        let galley = ui.painter().layout(format!("{error}{suffix}"), theme::regular(12.5), t.error, (above.width() - 40.0).max(160.0));
        let rect = Rect::from_center_size(
            Pos2::new(above.center().x, above.bottom() - galley.size().y / 2.0 - 10.0),
            galley.size() + Vec2::new(24.0, 14.0),
        );
        ui.painter().rect_filled(rect, 8.0, t.popup);
        ui.painter().rect_stroke(rect, 8.0, Stroke::new(1.0, t.error), egui::StrokeKind::Inside);
        ui.painter().galley(rect.min + Vec2::new(12.0, 7.0), galley, t.error);
    }

    /// Sends a pending pin state to Hyprland and picks up pinning done with its own keys.
    #[cfg(target_os = "linux")]
    fn sync_wm(&mut self, ctx: &egui::Context, now: f64) {
        let Some(wm) = &mut self.wm else { return };
        wm.pinned.store(self.session.state.pinned, std::sync::atomic::Ordering::Relaxed);
        if let Some((pinned, deadline)) = wm.pending {
            // The start-up request gets its deadline once the clock runs.
            let deadline = if deadline.is_nan() { now + 5.0 } else { deadline };
            wm.pending = Some((pinned, deadline));
            if wm.hypr.set_pinned(pinned) {
                wm.pending = None;
                wm.ignore_until = now + 1.5;
                if std::mem::take(&mut wm.restore) {
                    wm.hypr.place(self.session.state.window_size, self.session.state.window_pos);
                }
            } else if now < deadline {
                // Not mapped yet right after start: try again shortly.
                ctx.request_repaint_after(std::time::Duration::from_millis(100));
            } else {
                wm.pending = None;
            }
        }
        while let Ok(win) = wm.observed.try_recv() {
            // Hiding unpins the window; the pin comes back with it.
            if wm.pending.is_some() || now < wm.ignore_until || win.hidden {
                continue;
            }
            if win.pinned != self.session.state.pinned {
                self.session.state.pinned = win.pinned;
                self.session.dirty_since.get_or_insert(now);
            }
            // Only a floating window's geometry is worth remembering; a tiled one is the layout's.
            if win.floating {
                let size = Some([win.size[0] as f32, win.size[1] as f32]);
                let pos = Some([win.at[0] as f32, win.at[1] as f32]);
                if (size, pos) != (self.session.state.window_size, self.session.state.window_pos) {
                    self.session.state.window_size = size;
                    self.session.state.window_pos = pos;
                    self.session.dirty_since.get_or_insert(now);
                }
            }
        }
    }

    /// Remembers the window's size (and position where the platform reports it) for next start.
    fn track_geometry(&mut self, ctx: &egui::Context, now: f64) {
        #[cfg(target_os = "linux")]
        if self.wm.is_some() {
            return; // Hyprland reports it (and only for floating windows): see sync_wm.
        }
        let (inner, outer, odd) = ctx.input(|i| {
            let v = i.viewport();
            (v.inner_rect, v.outer_rect, v.minimized == Some(true) || v.maximized == Some(true) || v.fullscreen == Some(true))
        });
        let Some(inner) = inner.filter(|_| !odd) else { return };
        let size = Some([inner.width().round(), inner.height().round()]);
        // Wayland never reports a real position.
        let pos = outer.filter(|_| std::env::var_os("WAYLAND_DISPLAY").is_none()).map(|r| [r.min.x.round(), r.min.y.round()]);
        if size != self.session.state.window_size || (pos.is_some() && pos != self.session.state.window_pos) {
            self.session.state.window_size = size;
            self.session.state.window_pos = pos.or(self.session.state.window_pos);
            self.session.mark_dirty(now);
        }
    }

    /// Draws one frame into `ui` (the full window). Shared by eframe and the headless tests.
    pub fn draw(&mut self, ui: &mut egui::Ui) {
        let ctx = ui.ctx().clone();
        let now = ctx.input(|i| i.time);
        let t = self.tokens();
        if self.applied_theme != Some(t.kind) {
            t.apply(&ctx);
            self.applied_theme = Some(t.kind);
        }
        // Fonts given to set_fonts only exist from the next frame: named families would panic now.
        if !self.fonts_ready {
            egui_extras::install_image_loaders(&ctx);
            theme::install_fonts(&ctx);
            self.fonts_ready = true;
            ctx.request_repaint();
            return;
        }
        self.session.poll_saves(now);
        self.session.poll_update_check(now);
        self.session.poll_sealed(now);
        self.check_idle(&ctx, now);
        if self.session.saves.is_some() {
            ctx.request_repaint_after(std::time::Duration::from_millis(200));
        }
        // The approval dialog is modal: the editor's shortcuts wait until it is answered.
        if self.session.approval.is_none() {
            self.shortcuts(&ctx, now);
        }
        if self.session.hide_requests.1.try_recv().is_ok() {
            self.hide_window(&ctx);
        }
        while let Ok(text) = self.session.note_requests.1.try_recv() {
            self.session.add_note_with_text(text, now);
        }
        let shared = self.session.scripts.clone();
        self.session.take_appends(&mut lock(&shared), Some(now));
        self.session.poll_approvals(now);
        #[cfg(target_os = "linux")]
        self.sync_wm(&ctx, now);
        self.track_geometry(&ctx, now);
        let may_show_secret = self.guard_capture(&ctx, now);

        let window = ctx.content_rect();
        self.window_frame(ui, window, &t);
        let header = Rect::from_min_size(window.min, Vec2::new(window.width(), HEADER_H));
        let tabs = Rect::from_min_size(Pos2::new(window.left(), header.bottom()), Vec2::new(window.width(), TABS_H));
        let footer = Rect::from_min_max(Pos2::new(window.left(), window.bottom() - FOOTER_H), window.max);
        let divider_y = tabs.bottom() + 3.0;
        ui.painter().hline(window.x_range().shrink(1.0), divider_y, Stroke::new(1.0, t.divider));
        let editor =
            Rect::from_min_max(Pos2::new(window.left() + PAD + 4.0, divider_y + 10.0), Pos2::new(window.right() - PAD, footer.top()));

        self.header(ui, header, &t, now);
        self.tabs(ui, tabs, &t, now);
        if may_show_secret {
            self.editor(ui, editor, &t, now);
        } else {
            // Still laid out, so the editor keeps its focus, but nothing shows for this frame.
            let mut veiled = ui.new_child(egui::UiBuilder::new().max_rect(window));
            veiled.set_opacity(0.0);
            self.editor(&mut veiled, editor, &t, now);
        }
        self.footer(ui, footer, &t, now);
        let message_area = Rect::from_min_max(editor.min, Pos2::new(window.right(), footer.top()));
        self.toast_ui(ui, message_area, &t, now);
        if self.session.save_error.is_some() {
            // Above the toast, so an Undo stays reachable while saving fails.
            let above = if self.session.toast.is_some() { message_area.with_max_y(message_area.bottom() - 36.0) } else { message_area };
            self.save_error_ui(ui, above, &t);
        }
        self.popups(&ctx, window, &t, now);
        self.resize_edges(ui, window);
        if self.popup.is_some_and(|(p, _)| p == Popup::Encrypt) && self.encrypt_form.note != self.session.state.current().id {
            self.popup = None;
        }
        if !self.popup.is_some_and(|(p, _)| p == Popup::Encrypt) && self.encrypt_form != EncryptForm::default() {
            // A typed password does not linger once the popup is gone.
            self.encrypt_form = EncryptForm::default();
        }
        if self.session.show_cipher.is_some() && !self.session.showing_cipher() {
            self.session.show_cipher = None;
        }
        for id in self.session.forget.drain(..) {
            ctx.data_mut(|d| d.remove::<egui::text_edit::TextEditState>(Id::new(("editor", id))));
        }
        // Scripts see a note hidden or encrypted, or the read option changed, right away.
        lock(&self.session.scripts).track(&self.session.state);

        self.session.autosave_when_due(now);
        self.approval_dialog(&ctx, &t, now);
    }

    /// An agent's `--write` or `--run`, waiting for the user. No default button, and the buttons
    /// wake up only after `APPROVAL_GRACE`, so typing that was meant for the editor can't allow it.
    fn approval_dialog(&mut self, ctx: &egui::Context, t: &Tokens, now: f64) {
        use crate::session::Ask;
        let Some(approval) = &self.session.approval else {
            self.approval_password.zeroize();
            self.approval_error = None;
            return;
        };
        let ready = self.session.approval_ready(now);
        if !ready {
            ctx.request_repaint_after(std::time::Duration::from_millis(100));
        }
        let needs_password = self.session.approval_needs_password();
        let mut answer = None;
        let frame = egui::Frame::new()
            .fill(t.popup)
            .stroke(Stroke::new(1.0, t.border))
            .corner_radius(10)
            .inner_margin(egui::Margin::same(16))
            .shadow(egui::Shadow { offset: [0, 6], blur: 20, spread: 0, color: Color32::from_black_alpha(90) });
        let widest = if matches!(approval.ask, Ask::Read { .. }) { 440.0 } else { 320.0 };
        let width = (ctx.content_rect().width() - 64.0).clamp(200.0, widest);
        let modal = egui::Modal::new(Id::new("approval")).frame(frame).backdrop_color(Color32::from_black_alpha(110));
        modal.show(ctx, |ui| {
            ui.set_width(width);
            ui.spacing_mut().item_spacing.y = 6.0;
            let note = |text: &str, color| egui::RichText::new(text).font(theme::regular(12.5)).color(color);
            match &approval.ask {
                Ask::Write { title, path, exists, git_ignored } => {
                    ui.label(egui::RichText::new(format!("Write “{title}” to a file?")).font(theme::semibold(14.0)).color(t.text));
                    ui.label(note(&path.display().to_string(), t.text_muted));
                    if *exists {
                        ui.label(note("Replaces the file that is there.", t.error));
                    }
                    if *git_ignored == Some(false) {
                        ui.label(note("Git does not ignore this file: it could be committed.", t.error));
                    }
                }
                Ask::Run { title, argv } => {
                    ui.label(egui::RichText::new(format!("Give “{title}” to a command?")).font(theme::semibold(14.0)).color(t.text));
                    let font = egui::FontId::monospace(12.5);
                    ui.label(egui::RichText::new(crate::handoff::shown_command(argv)).font(font).color(t.text_muted));
                    ui.label(note("Its KEY=VALUE lines become the command's environment.", t.text_muted));
                }
                Ask::Read { title, text, findings, .. } => {
                    ui.label(egui::RichText::new(format!("Let the agent read “{title}”?")).font(theme::semibold(14.0)).color(t.text));
                    ui.label(note("It may take this text as your instructions.", t.text_muted));
                    read_preview(ui, t, text, findings);
                    if !findings.is_empty() {
                        egui::Frame::new()
                            .fill(t.error.gamma_multiply(0.12))
                            .stroke(Stroke::new(1.0, t.error))
                            .corner_radius(6)
                            .inner_margin(egui::Margin::same(8))
                            .show(ui, |ui| {
                                ui.set_width(ui.available_width());
                                ui.spacing_mut().item_spacing.y = 2.0;
                                ui.label(note("Looks like instructions for an AI:", t.error).strong());
                                for f in findings.iter().take(8) {
                                    ui.label(note(&format!("Line {}: {}", f.line, f.kind.label()), t.text));
                                }
                                if findings.len() > 8 {
                                    ui.label(note(&format!("…and {} more", findings.len() - 8), t.text_muted));
                                }
                            });
                    }
                }
            }
            if needs_password {
                ui.add_space(4.0);
                let edit = egui::TextEdit::singleline(&mut *self.approval_password)
                    .password(true)
                    .hint_text("Password of the note")
                    .background_color(t.bg_bottom)
                    .desired_width(f32::INFINITY)
                    .margin(egui::Margin::symmetric(8, 5));
                let resp = ui.add(edit);
                resp.widget_info(|| egui::WidgetInfo::labeled(egui::WidgetType::TextEdit, true, "Password of the note"));
            }
            if let Some(e) = &self.approval_error {
                ui.label(note(e, t.error));
            }
            ui.add_space(8.0);
            ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                let button =
                    |text: &str| egui::Button::new(egui::RichText::new(text).font(theme::semibold(13.0))).min_size(Vec2::new(72.0, 28.0));
                // Right to left: the answer that hands the note over sits rightmost.
                let allow = if matches!(approval.ask, Ask::Write { exists: true, .. }) { "Replace" } else { "Allow" };
                // Only a real click answers: not Space or Enter on a button that Tab focused while
                // the user kept typing, nor an accessibility "click" another program can send.
                let clicked = |resp: Response| resp.clicked_by(egui::PointerButton::Primary);
                if clicked(ui.add_enabled(ready, button(allow))) {
                    answer = Some(true);
                }
                if clicked(ui.add_enabled(ready, button("Deny"))) {
                    answer = Some(false);
                }
            });
        });
        if ready && ctx.input(|i| i.key_pressed(egui::Key::Escape)) {
            answer = Some(false);
        }
        match answer {
            Some(true) => {
                let password = needs_password.then_some(self.approval_password.as_str());
                match self.session.allow(password, now) {
                    Ok(()) => {
                        self.approval_password.zeroize();
                        self.approval_error = None;
                    }
                    Err(e) => self.approval_error = Some(e),
                }
            }
            Some(false) => self.session.deny(now),
            None => {}
        }
    }
}

/// The note's text as the agent would get it, with line numbers; hidden characters shown
/// escaped and flagged lines in the error colour.
fn read_preview(ui: &mut egui::Ui, t: &Tokens, text: &str, findings: &[crate::scan::Finding]) {
    let (shown, left_out) = crate::scan::shown_text(text, 400, 32 << 10);
    let font = egui::FontId::monospace(12.0);
    let mut job = egui::text::LayoutJob::default();
    job.wrap.max_width = ui.available_width() - 24.0;
    let format = |color| egui::TextFormat { font_id: font.clone(), color, ..Default::default() };
    let digits = shown.split('\n').count().to_string().len();
    for (i, line) in shown.split('\n').enumerate() {
        let flagged = findings.iter().any(|f| f.line == i + 1);
        job.append(&format!("{:>digits$}  ", i + 1), 0.0, format(t.text_muted));
        job.append(line, 0.0, format(if flagged { t.error } else { t.text }));
        job.append("\n", 0.0, format(t.text));
    }
    if left_out > 0 {
        job.append(&format!("… {left_out} more lines"), 0.0, format(t.text_muted));
    }
    egui::Frame::new().fill(t.bg_bottom).corner_radius(6).inner_margin(egui::Margin::same(8)).show(ui, |ui| {
        ui.set_width(ui.available_width());
        egui::ScrollArea::vertical().max_height(180.0).auto_shrink([false, true]).show(ui, |ui| {
            ui.add(egui::Label::new(job).selectable(false));
        });
    });
}

/// The Encrypt popup: the note it was opened for, the password typed twice, an error to show,
/// and whether the first field still needs to grab focus.
#[derive(Default, PartialEq)]
struct EncryptForm {
    note: u64,
    password: String,
    again: String,
    error: Option<&'static str>,
    grab: bool,
}

enum TabAction {
    None,
    Select,
    Close,
}

/// A square icon button: transparent until hovered.
fn icon_button(ui: &mut egui::Ui, rect: Rect, icon: &str, size: f32, tint: Color32, t: &Tokens, label: &str) -> Response {
    let resp = ui.interact(rect, Id::new(("icon-btn", label)), Sense::click()).on_hover_cursor(CursorIcon::PointingHand);
    resp.widget_info(|| egui::WidgetInfo::labeled(egui::WidgetType::Button, true, label));
    if resp.hovered() {
        ui.painter().rect_filled(rect, 7.0, t.hover);
    }
    let shift = if resp.is_pointer_button_down_on() { Vec2::new(0.0, 1.0) } else { Vec2::ZERO };
    icons::paint(ui, rect.translate(shift), icon, size, tint);
    resp
}

/// A popup-menu row; `keys` is its keyboard shortcut, shown right-aligned and muted ("" for none).
fn menu_item(ui: &mut egui::Ui, t: &Tokens, label: &str, keys: &str, checked: bool) -> Response {
    let font = theme::regular(13.5);
    let label_w = ui.painter().layout_no_wrap(label.into(), font.clone(), t.text).size().x;
    let keys = (!keys.is_empty()).then(|| ui.painter().layout_no_wrap(keys.into(), theme::regular(12.5), t.text_muted));
    let check_w = if checked { 16.0 } else { 0.0 };
    let min_w = 8.0 + label_w + keys.as_ref().map_or(0.0, |k| 24.0 + k.size().x) + check_w + 8.0;
    let (rect, resp) = ui.allocate_exact_size(Vec2::new(ui.available_width().max(150.0).max(min_w), 24.0), Sense::click());
    resp.widget_info(|| egui::WidgetInfo::selected(egui::WidgetType::Button, true, checked, label));
    if resp.hovered() {
        ui.painter().rect_filled(rect, 5.0, t.hover);
    }
    ui.painter().text(Pos2::new(rect.left() + 8.0, rect.center().y), Align2::LEFT_CENTER, label, font, t.text);
    if let Some(keys) = keys {
        let pos = Pos2::new(rect.right() - 8.0 - check_w - keys.size().x, rect.center().y - keys.size().y / 2.0);
        ui.painter().galley(pos, keys, t.text_muted);
    }
    if checked {
        ui.painter().circle_filled(Pos2::new(rect.right() - 12.0, rect.center().y), 3.5, t.accent);
    }
    resp
}

fn separator(ui: &mut egui::Ui, t: &Tokens) {
    let (rect, _) = ui.allocate_exact_size(Vec2::new(ui.available_width(), 7.0), Sense::hover());
    ui.painter().hline(rect.x_range().shrink(4.0), rect.center().y, Stroke::new(1.0, t.divider));
}

/// A file name from a note title: no path separators or reserved characters.
fn sanitize(title: &str) -> String {
    let s: String = title.chars().map(|c| if r#"/\:*?"<>|"#.contains(c) || c.is_control() { '_' } else { c }).collect();
    if s.trim().is_empty() { "note".into() } else { s.trim().to_owned() }
}

impl eframe::App for App {
    fn clear_color(&self, _visuals: &egui::Visuals) -> [f32; 4] {
        [0.0; 4]
    }

    fn logic(&mut self, ctx: &egui::Context, _frame: &mut eframe::Frame) {
        if ctx.input(|i| i.viewport().close_requested()) {
            // The launch asking gets a clear "denied", not "closed".
            self.session.deny(ctx.input(|i| i.time));
            if !self.force_close && !self.session.save_now() {
                ctx.send_viewport_cmd(ViewportCommand::CancelClose);
            }
        }
    }

    fn ui(&mut self, ui: &mut egui::Ui, _frame: &mut eframe::Frame) {
        self.draw(ui);
    }
}

impl Drop for App {
    fn drop(&mut self) {
        // A sealing worker may still be running: its note is stale and gets sealed here.
        if !self.force_close && (self.session.dirty_since.is_some() || self.session.state.notes.iter().any(|n| self.session.is_stale(n))) {
            let _ = self.session.save_now();
        }
        // A secret copied in the last 30 s does not outlive the app on the clipboard.
        self.clipboard.clear_now(std::time::Duration::from_millis(500));
    }
}

#[cfg(test)]
mod tests {
    use super::sanitize;

    #[test]
    fn sanitizes_file_names() {
        assert_eq!(sanitize("API keys / prod"), "API keys _ prod");
        assert_eq!(sanitize("  "), "note");
    }
}
