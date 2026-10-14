//! The terminal frontend (`scrpad --tui`): the same notes and `Session` as the window, drawn with
//! ratatui. `Tui` turns key events into session actions and draws frames; `term` runs it in the
//! real terminal. Tests drive `Tui` with synthetic events and ratatui's `TestBackend`.

mod editor;
mod term;

use std::collections::{HashMap, HashSet};

use base64::Engine;
use ratatui::Frame;
use ratatui::crossterm::event::{Event, KeyCode, KeyEvent, KeyEventKind, KeyModifiers};
use ratatui::layout::{Position, Rect};
use ratatui::style::{Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Clear, Paragraph, Wrap};
use zeroize::Zeroizing;

pub use term::run;

use crate::crypto;
use crate::session::{Session, ToastAction, lock};
use crate::store::ReadMode;
use editor::{Editor, Move};

/// The keys, as the help (F1) lists them and README.md documents them. Ctrl+Shift combinations
/// don't reach most terminals, Ctrl+H is Backspace in many, and Ctrl+Tab is taken: the window's
/// shortcuts lose their Shift here, and hiding moves to Alt+H.
pub const KEYS: &[(&str, &str)] = &[
    ("Ctrl+N, Ctrl+T", "New note"),
    ("Ctrl+W", "Close note (Undo: Alt+U)"),
    ("F2", "Rename note"),
    ("Alt+1…9", "Go to note 1…8, or the last"),
    ("Ctrl+PgUp/PgDn", "Previous / next note"),
    ("Alt+←/→", "Previous / next note"),
    ("Alt+H", "Hide or show the note"),
    ("Ctrl+L", "Encrypt, lock or unlock the note"),
    ("Ctrl+K", "Lock all encrypted notes"),
    ("Ctrl+E", "Show the encrypted text (Esc: back)"),
    ("Ctrl+Y", "Copy all (terminal clipboard)"),
    ("Ctrl+Z, Ctrl+R", "Undo, redo typing"),
    ("Alt+U", "Undo from the status line"),
    ("F10, Alt+M", "Menu: options and more"),
    ("F1", "These keys"),
    ("Ctrl+Q", "Save and quit"),
];

const PLACEHOLDER: &str = "Type anything. It saves by itself.";

/// The terminal's own colours, so a light or a dark terminal both look right.
mod style {
    use ratatui::style::{Color, Modifier, Style};
    pub const MUTED: Style = Style::new().fg(Color::DarkGray);
    pub const SECURE: Style = Style::new().fg(Color::Green);
    pub const ERROR: Style = Style::new().fg(Color::Red);
    pub const ACCENT: Style = Style::new().fg(Color::Cyan).add_modifier(Modifier::BOLD);
    pub const SELECTED: Style = Style::new().add_modifier(Modifier::REVERSED.union(Modifier::BOLD));
}

/// What a key or a menu item does.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum Action {
    New,
    Close,
    Rename,
    Go(usize),
    Previous,
    Next,
    Hide,
    Lock,
    LockAll,
    Cipher,
    RemoveEncryption,
    Clear,
    Copy,
    UndoToast,
    Undo,
    Redo,
    Help,
    Menu,
    AutoLock,
    AutoRehide,
    ScriptsRead(ReadMode),
    Quit,
    QuitWithoutSaving,
}

/// A box over the editor that takes the keys until it closes.
enum Modal {
    /// The password typed twice, as there is no way to recover it; `second` is the field typed in.
    Encrypt {
        note: u64,
        password: Zeroizing<String>,
        again: Zeroizing<String>,
        second: bool,
        error: Option<&'static str>,
    },
    Unlock {
        note: u64,
        password: Zeroizing<String>,
        error: Option<&'static str>,
    },
    Rename {
        note: u64,
        title: String,
    },
    Menu {
        selected: usize,
    },
    /// An agent's `--write` or `--run`, asked about in `session.approval`; the password is for
    /// a locked note.
    Approve {
        password: Zeroizing<String>,
        error: Option<String>,
    },
    Help,
}

pub struct Tui {
    pub session: Session,
    /// Per note: the cursor and the undo history of its text.
    editors: HashMap<u64, Editor>,
    modal: Option<Modal>,
    /// The locked note whose unlock box was closed with Esc: it opens again with Enter or Ctrl+L.
    dismissed: Option<u64>,
    /// Notes changed here since the last save, marked `*` in the tab bar.
    unsaved: HashSet<u64>,
    /// How far the encrypted text (Ctrl+E) is scrolled.
    cipher_scroll: u16,
    /// An OSC 52 sequence that puts text on the terminal's clipboard, for the loop to write out.
    clipboard: Option<Zeroizing<String>>,
    /// Whether a key or a paste arrived since the last tick, for the idle timeout.
    input: bool,
    /// The save error last drawn: the same error went to stderr, over the screen (see `term`).
    shown_error: Option<String>,
    redraw: bool,
    quit: bool,
    force_quit: bool,
}

impl Tui {
    pub fn new(session: Session) -> Self {
        Self {
            session,
            editors: HashMap::new(),
            modal: None,
            dismissed: None,
            unsaved: HashSet::new(),
            cipher_scroll: 0,
            clipboard: None,
            input: false,
            shown_error: None,
            redraw: false,
            quit: false,
            force_quit: false,
        }
    }

    /// Whether the user quit, with everything saved (or chose to quit without saving).
    pub fn quit_requested(&self) -> bool {
        self.quit
    }

    /// The OSC 52 sequence of a copy, once.
    pub fn take_clipboard(&mut self) -> Option<Zeroizing<String>> {
        self.clipboard.take()
    }

    /// Whether the screen may hold something not drawn by ratatui and must be drawn anew.
    pub fn take_redraw(&mut self) -> bool {
        std::mem::take(&mut self.redraw)
    }

    /// Saves what is not saved yet, as on quitting (unless the user chose to quit without saving).
    pub fn finish(&mut self) {
        // A note from `scrpad --new` or `-` was answered ok: it is kept even if it never showed.
        while let Ok(text) = self.session.note_requests.1.try_recv() {
            self.session.add_note_with_text(text, 0.0);
        }
        // A sealing worker may still be running: its note is stale and gets sealed here.
        if !self.force_quit && (self.session.dirty_since.is_some() || self.session.state.notes.iter().any(|n| self.session.is_stale(n))) {
            let _ = self.session.save_now();
        }
    }

    // ---- what happens between frames, as `App::draw` does it ----------------------------------

    /// Takes in what happened outside the keys: ciphers sealed, the idle timeout, notes and
    /// appends from other launches, the autosave. Runs before every draw.
    pub fn tick(&mut self, now: f64) {
        self.session.poll_sealed(now);
        // Before anything is drawn, so text that should be locked by now never shows.
        self.session.check_idle(now, std::mem::take(&mut self.input));
        while let Ok(text) = self.session.note_requests.1.try_recv() {
            self.session.add_note_with_text(text, now);
        }
        let shared = self.session.scripts.clone();
        self.session.take_appends(&mut lock(&shared), Some(now));
        if self.session.toast.as_ref().is_some_and(|t| now > t.until) {
            self.session.toast = None;
        }
        self.session.poll_approvals(now);
        // The question comes first, over any other box; it closes when answered or given up on.
        match (&self.modal, self.session.approval.is_some()) {
            (Some(Modal::Approve { .. }), true) => {}
            (_, true) => self.modal = Some(Modal::Approve { password: Default::default(), error: None }),
            (Some(Modal::Approve { .. }), false) => self.modal = None,
            _ => {}
        }
        let current = self.session.state.current();
        let (id, locked) = (current.id, current.is_locked());
        match &self.modal {
            // The box belongs to another note now, or its note was unlocked: never act on whatever is active.
            Some(Modal::Encrypt { note, .. } | Modal::Rename { note, .. }) if *note != id => self.modal = None,
            Some(Modal::Unlock { note, .. }) if *note != id || !locked => self.modal = None,
            None if locked && self.dismissed != Some(id) && !self.session.showing_cipher() => {
                self.modal = Some(Modal::Unlock { note: id, password: Default::default(), error: None });
            }
            _ => {}
        }
        if self.dismissed.is_some_and(|d| d != id) {
            self.dismissed = None;
        }
        if self.session.show_cipher.is_some() && !self.session.showing_cipher() {
            self.session.show_cipher = None;
        }
        // Locking and closing drop a note's plaintext: its undo history holds copies of it.
        for id in self.session.forget.drain(..) {
            self.editors.remove(&id);
        }
        // Scripts see a note hidden or encrypted, or the read option changed, right away.
        lock(&self.session.scripts).track(&self.session.state);
        self.session.autosave_when_due(now);
        if self.session.dirty_since.is_none() {
            self.unsaved.clear();
        }
        if self.session.save_error != self.shown_error {
            self.shown_error = self.session.save_error.clone();
            self.redraw = true;
        }
    }

    // ---- input ----------------------------------------------------------------------------

    /// One terminal event, as the real loop and the tests deliver it.
    pub fn handle(&mut self, event: Event, now: f64) {
        match event {
            // Windows reports releases too.
            Event::Key(key) if key.kind != KeyEventKind::Release => {
                self.input = true;
                self.key(key, now);
            }
            Event::Paste(text) => {
                self.input = true;
                self.paste(&text, now);
            }
            Event::Resize(..) => self.redraw = true,
            _ => {}
        }
    }

    fn key(&mut self, key: KeyEvent, now: f64) {
        let action = binding(key);
        if action == Some(Action::Quit) {
            self.act(Action::Quit, now);
            return;
        }
        if let Some(modal) = self.modal.take() {
            self.modal_key(modal, key, action, now);
            return;
        }
        if let Some(action) = action {
            self.act(action, now);
        } else if key.modifiers.contains(KeyModifiers::CONTROL) && key.code == KeyCode::Char('c') {
            self.session.toast(now, "Copy all with Ctrl+Y, quit with Ctrl+Q", None);
        } else {
            self.edit_key(key, now);
        }
    }

    fn act(&mut self, action: Action, now: f64) {
        let n = self.session.state.notes.len();
        let active = self.session.state.active;
        match action {
            Action::New => self.session.add_note(now),
            Action::Close => self.session.close_note(active, now),
            Action::Rename => {
                let note = self.session.state.current();
                self.modal = Some(Modal::Rename { note: note.id, title: note.title.clone() });
            }
            Action::Go(i) => self.switch_to(i.min(n - 1), now),
            Action::Previous => self.switch_to((active + n - 1) % n, now),
            Action::Next => self.switch_to((active + 1) % n, now),
            Action::Hide => {
                self.session.toggle_hidden(now);
                self.unsaved.insert(self.session.state.current().id);
            }
            Action::Lock => {
                let note = self.session.state.current();
                if !note.is_encrypted() {
                    let note = note.id;
                    self.modal =
                        Some(Modal::Encrypt { note, password: Default::default(), again: Default::default(), second: false, error: None });
                } else if note.is_locked() {
                    self.session.show_cipher = None;
                    self.dismissed = None;
                } else {
                    self.session.lock_note(now);
                }
            }
            Action::LockAll => self.session.lock_all_now(now),
            Action::Cipher => {
                self.cipher_scroll = 0;
                self.session.toggle_cipher_view(now);
            }
            Action::RemoveEncryption => self.session.remove_encryption(now),
            Action::Clear => {
                self.unsaved.insert(self.session.state.current().id);
                self.session.clear(now);
            }
            Action::Copy => self.copy(now),
            Action::UndoToast => {
                if self.session.toast.as_ref().is_some_and(|t| matches!(t.action, Some(ToastAction::Undo(_))))
                    && let Some(ToastAction::Undo(undo)) = self.session.toast.take().and_then(|t| t.action)
                {
                    self.session.undo(undo, now);
                    self.unsaved.insert(self.session.state.current().id);
                }
            }
            Action::Undo | Action::Redo => {
                let note = &mut self.session.state.notes[active];
                if note.is_locked() || self.session.show_cipher.is_some() {
                    return;
                }
                let editor = self.editors.entry(note.id).or_insert_with(|| Editor::new(&note.text));
                let changed = if action == Action::Undo { editor.undo(&mut note.text) } else { editor.redo(&mut note.text) };
                if changed {
                    self.edited(now);
                }
            }
            Action::Help => self.modal = Some(Modal::Help),
            Action::Menu => self.modal = Some(Modal::Menu { selected: 0 }),
            Action::AutoLock => self.toggle_option(now, |s| &mut s.auto_lock),
            Action::AutoRehide => self.toggle_option(now, |s| &mut s.auto_rehide),
            Action::ScriptsRead(mode) => {
                self.session.set_reading(mode, now);
            }
            Action::Quit => {
                // The launch asking gets a clear "denied", not "closed".
                self.session.deny(now);
                // Stays when saving fails: the error says why, and the menu offers to quit anyway.
                if self.session.save_now() {
                    self.quit = true;
                }
            }
            Action::QuitWithoutSaving => {
                self.session.dirty_since = None;
                self.force_quit = true;
                self.quit = true;
            }
        }
    }

    fn switch_to(&mut self, i: usize, now: f64) {
        if self.session.state.active != i {
            self.session.state.active = i;
            self.session.mark_dirty(now);
        }
    }

    fn toggle_option(&mut self, now: f64, option: fn(&mut crate::store::State) -> &mut bool) {
        let value = option(&mut self.session.state);
        *value = !*value;
        self.session.mark_dirty(now);
    }

    /// The text of the current note changed here.
    fn edited(&mut self, now: f64) {
        self.session.mark_dirty(now);
        self.unsaved.insert(self.session.state.current().id);
    }

    /// Copies the note (or the encrypted text on show) through OSC 52, which works over SSH too.
    /// The terminal's clipboard can't be read back, so unlike the window's it is never cleared:
    /// clearing it blindly could wipe what was copied since.
    fn copy(&mut self, now: f64) {
        if self.session.showing_cipher()
            && let Some(cipher) = &self.session.state.current().cipher
        {
            self.clipboard = Some(osc52(cipher));
            self.session.toast(now, "Copied the encrypted text", None);
            return;
        }
        let note = self.session.state.current();
        let msg = if note.is_locked() {
            "Unlock the note first"
        } else if note.text.is_empty() {
            "Nothing to copy"
        } else {
            self.clipboard = Some(osc52(&note.text));
            if note.hidden || note.is_encrypted() { "Copied; the terminal's clipboard won't be cleared" } else { "Copied to clipboard" }
        };
        self.session.toast(now, msg, None);
    }

    /// Keys for the editor, the encrypted text and the locked note.
    fn edit_key(&mut self, key: KeyEvent, now: f64) {
        let ctrl = key.modifiers.contains(KeyModifiers::CONTROL);
        let alt = key.modifiers.contains(KeyModifiers::ALT);
        if self.session.showing_cipher() {
            let page = 10;
            self.cipher_scroll = match key.code {
                KeyCode::Esc => {
                    self.session.toggle_cipher_view(now);
                    0
                }
                KeyCode::Up => self.cipher_scroll.saturating_sub(1),
                KeyCode::Down => self.cipher_scroll.saturating_add(1),
                KeyCode::PageUp => self.cipher_scroll.saturating_sub(page),
                KeyCode::PageDown => self.cipher_scroll.saturating_add(page),
                KeyCode::Home => 0,
                _ => self.cipher_scroll,
            };
            return;
        }
        let active = self.session.state.active;
        let note = &mut self.session.state.notes[active];
        if note.is_locked() {
            if key.code == KeyCode::Enter {
                self.dismissed = None;
            }
            return;
        }
        let editor = self.editors.entry(note.id).or_insert_with(|| Editor::new(&note.text));
        let text = &mut note.text;
        let to = match key.code {
            KeyCode::Left if ctrl => Move::WordLeft,
            KeyCode::Right if ctrl => Move::WordRight,
            KeyCode::Left => Move::Left,
            KeyCode::Right => Move::Right,
            KeyCode::Up => Move::Up(1),
            KeyCode::Down => Move::Down(1),
            KeyCode::PageUp => Move::Up(0),
            KeyCode::PageDown => Move::Down(0),
            KeyCode::Home if ctrl => Move::Top,
            KeyCode::End if ctrl => Move::Bottom,
            KeyCode::Home => Move::Home,
            KeyCode::End => Move::End,
            code => {
                let changed = match code {
                    // AltGr arrives as Ctrl+Alt on Windows: those are characters too.
                    KeyCode::Char(c) if ctrl == alt => editor.insert(text, c.encode_utf8(&mut [0; 4])),
                    KeyCode::Enter => editor.insert(text, "\n"),
                    KeyCode::Tab => editor.insert(text, "\t"),
                    KeyCode::Backspace => editor.backspace(text),
                    KeyCode::Delete => editor.delete(text),
                    _ => false,
                };
                if changed {
                    self.edited(now);
                }
                return;
            }
        };
        editor.move_to(text, to);
    }

    /// Pasted text goes where typing would; a box's one-line field takes its first line.
    fn paste(&mut self, pasted: &str, now: f64) {
        match &mut self.modal {
            Some(Modal::Encrypt { password, again, second, .. }) => {
                field_paste(if *second { again } else { password }, pasted);
            }
            Some(Modal::Unlock { password, .. }) => field_paste(password, pasted),
            Some(Modal::Approve { password, .. }) => {
                if self.session.approval_needs_password() {
                    field_paste(password, pasted);
                }
            }
            Some(Modal::Rename { title, .. }) => field_paste(title, pasted),
            Some(_) => {}
            None if self.session.showing_cipher() => {}
            None => {
                let note = &mut self.session.state.notes[self.session.state.active];
                if note.is_locked() {
                    return;
                }
                let editor = self.editors.entry(note.id).or_insert_with(|| Editor::new(&note.text));
                if editor.insert(&mut note.text, &pasted.replace("\r\n", "\n").replace('\r', "\n")) {
                    self.edited(now);
                }
            }
        }
    }

    /// Keys for the question about an agent's request. Nothing answers it before the type-ahead
    /// guard is over, and no key reaches the editor while it is open.
    fn approve_key(&mut self, password: &mut Zeroizing<String>, error: &mut Option<String>, key: KeyEvent, now: f64) {
        let ready = self.session.approval_ready(now);
        let needs = self.session.approval_needs_password();
        match key.code {
            KeyCode::Esc if ready => self.session.deny(now),
            KeyCode::Char('n') if ready && !needs => self.session.deny(now),
            KeyCode::Char('y') if ready && !needs => {
                // Only an error is possible with a password, and there is none here.
                let _ = self.session.allow(None, now);
            }
            KeyCode::Enter if ready && needs => {
                let typed = std::mem::take(password);
                if let Err(e) = self.session.allow(Some(&typed), now) {
                    *error = Some(e);
                }
            }
            _ if needs => field_key(password, key),
            _ => {}
        }
    }

    fn modal_key(&mut self, mut modal: Modal, key: KeyEvent, action: Option<Action>, now: f64) {
        if let Modal::Approve { password, error } = &mut modal {
            self.approve_key(password, error, key, now);
            // Answered: the box is gone; otherwise it stays as it is.
            if self.session.approval.is_some() {
                self.modal = Some(modal);
            }
            return;
        }
        if key.code == KeyCode::Esc {
            if let Modal::Unlock { note, .. } = modal {
                self.dismissed = Some(note);
            }
            return;
        }
        match &mut modal {
            Modal::Encrypt { password, again, second, error, note } => match key.code {
                KeyCode::Tab | KeyCode::BackTab | KeyCode::Up | KeyCode::Down => *second = !*second,
                KeyCode::Enter if !*second => *second = true,
                KeyCode::Enter => {
                    let note = *note;
                    if note != self.session.state.current().id {
                        // The box belongs to another note: never encrypt whatever is active now.
                        return;
                    } else if password.is_empty() {
                        *error = Some("Type a password");
                        *second = false;
                    } else if password != again {
                        *error = Some("The passwords don't match");
                    } else {
                        let password = std::mem::take(password);
                        again.clear();
                        match self.session.encrypt_note(&password, now) {
                            Ok(()) => return,
                            Err(e) => {
                                *error = Some(if e == crypto::Error::EmptyPassword { "Type a password" } else { "Encryption failed" })
                            }
                        }
                    }
                }
                _ => field_key(if *second { again } else { password }, key),
            },
            Modal::Unlock { password, error, note } => {
                if action == Some(Action::Cipher) {
                    self.dismissed = Some(*note);
                    self.act(Action::Cipher, now);
                    return;
                }
                if key.code == KeyCode::Enter {
                    let typed = std::mem::take(password);
                    match self.session.unlock_note(&typed, now) {
                        Ok(()) => return,
                        Err(e) => *error = Some(if e == crypto::Error::EmptyPassword { "Type the password" } else { "Wrong password" }),
                    }
                } else {
                    field_key(password, key);
                }
            }
            Modal::Rename { title, note } => {
                if key.code == KeyCode::Enter {
                    let title = title.trim();
                    if let Some(n) = self.session.state.notes.iter_mut().find(|n| n.id == *note)
                        && !title.is_empty()
                    {
                        n.title = title.to_owned();
                        self.session.mark_dirty(now);
                        self.unsaved.insert(*note);
                    }
                    return;
                }
                field_key(title, key);
            }
            Modal::Menu { selected } => {
                let items = self.menu();
                match key.code {
                    KeyCode::Up => *selected = (*selected + items.len() - 1) % items.len(),
                    KeyCode::Down => *selected = (*selected + 1) % items.len(),
                    KeyCode::Enter => {
                        let item = items[(*selected).min(items.len() - 1)];
                        self.act(item.action, now);
                        // Options stay open to see them change; anything else is done.
                        if item.checked.is_none() {
                            return;
                        }
                    }
                    _ if action == Some(Action::Menu) => return,
                    _ => {}
                }
            }
            Modal::Approve { .. } => unreachable!("handled above"),
            Modal::Help => return,
        }
        // An action from inside may have opened another box (none does today): that one wins.
        if self.modal.is_none() {
            self.modal = Some(modal);
        }
    }

    /// The menu's items as things are now, like the window's `…` menu.
    fn menu(&self) -> Vec<MenuItem> {
        let s = &self.session;
        let note = s.state.current();
        let mut items = vec![MenuItem::new("Rename note", "F2", Action::Rename), MenuItem::new("Close note", "Ctrl+W", Action::Close)];
        let hide = if note.hidden { "Show note" } else { "Hide note" };
        items.push(MenuItem::new(hide, "Alt+H", Action::Hide));
        if note.is_encrypted() {
            let label = if s.showing_cipher() { "Show the text" } else { "Show encrypted text" };
            items.push(MenuItem::new(label, "Ctrl+E", Action::Cipher));
        }
        if !note.is_encrypted() {
            items.push(MenuItem::new("Encrypt note…", "Ctrl+L", Action::Lock));
        } else if note.is_locked() {
            items.push(MenuItem::new("Unlock note…", "Ctrl+L", Action::Lock));
        } else {
            items.push(MenuItem::new("Lock note", "Ctrl+L", Action::Lock));
            items.push(MenuItem::new("Remove encryption", "", Action::RemoveEncryption));
        }
        if s.state.notes.iter().any(crate::store::Note::is_encrypted) {
            items.push(MenuItem::new("Lock all", "Ctrl+K", Action::LockAll));
        }
        items.push(MenuItem::new("Copy all", "Ctrl+Y", Action::Copy));
        items.push(MenuItem::new("Clear note", "", Action::Clear));
        // On quitting and after 5 minutes without input.
        items.push(MenuItem::option("Lock encrypted notes automatically", Action::AutoLock, s.state.auto_lock));
        items.push(MenuItem::option("Hide notes again automatically", Action::AutoRehide, s.state.auto_rehide));
        // `scrpad --list` and `--read`. Hidden and encrypted notes stay out whatever this says.
        for (label, mode) in [
            ("Let scripts read notes: off", ReadMode::Off),
            ("Let scripts read notes: ask each time", ReadMode::Ask),
            ("Let scripts read notes: always", ReadMode::Always),
        ] {
            items.push(MenuItem::option(label, Action::ScriptsRead(mode), s.state.script_reading == mode));
        }
        items.push(MenuItem::new("Keys", "F1", Action::Help));
        items.push(MenuItem::new("Quit", "Ctrl+Q", Action::Quit));
        if s.save_error.is_some() {
            items.push(MenuItem::new("Quit without saving", "", Action::QuitWithoutSaving));
        }
        items
    }

    // ---- drawing ----------------------------------------------------------------------------

    /// Draws the whole screen: tab bar, the note, the status line, and a box over them if one is open.
    pub fn draw(&mut self, frame: &mut Frame) {
        let area = frame.area();
        if area.width < 24 || area.height < 6 {
            frame.render_widget(Paragraph::new("Make the terminal larger").style(style::MUTED), area);
            return;
        }
        let error = self.session.save_error.as_ref().map(|error| {
            let suffix = if self.session.save_blocked {
                " Copy your notes elsewhere, then F10 → Quit without saving."
            } else {
                " Retrying automatically."
            };
            format!("{error}{suffix}")
        });
        let error_h = error.as_ref().map_or(0, |e| editor::wrap(e, usize::from(area.width - 2)).len().min(3) as u16);
        let row = |y: u16, h: u16| Rect::new(area.x, y, area.width, h);
        let tabs = row(area.y, 1);
        let body_top = area.y + 2;
        let footer = row(area.bottom() - 1, 1);
        let body = Rect::new(area.x + 1, body_top, area.width - 2, footer.y - 1 - error_h - body_top);
        self.tab_bar(frame, tabs);
        let divider = "─".repeat(usize::from(area.width));
        frame.render_widget(Line::styled(divider.as_str(), style::MUTED), row(area.y + 1, 1));
        frame.render_widget(Line::styled(divider.as_str(), style::MUTED), row(footer.y - 1, 1));
        if let Some(error) = error {
            let at = Rect::new(area.x + 1, footer.y - 1 - error_h, area.width - 2, error_h);
            frame.render_widget(Paragraph::new(error).style(style::ERROR).wrap(Wrap { trim: true }), at);
        }
        let cursor = self.body(frame, body);
        self.status(frame, footer);
        match self.modal_box(frame, area) {
            Some(at) => frame.set_cursor_position(at),
            None if self.modal.is_none() => {
                if let Some(at) = cursor {
                    frame.set_cursor_position(at);
                }
            }
            None => {}
        }
    }

    fn tab_bar(&self, frame: &mut Frame, area: Rect) {
        let state = &self.session.state;
        let labels: Vec<(String, Style)> = state
            .notes
            .iter()
            .enumerate()
            .map(|(i, note)| {
                let mut label = format!(" {}", truncate(&note.title, 24));
                if note.is_locked() {
                    label.push_str(" [locked]");
                } else if note.is_encrypted() {
                    label.push_str(" [unlocked]");
                }
                if self.unsaved.contains(&note.id) {
                    label.push_str(" *");
                }
                if !self.session.findings(note.id).is_empty() {
                    label.push_str(" [!]");
                }
                label.push(' ');
                let style = if i == state.active {
                    style::SELECTED
                } else if note.is_encrypted() {
                    style::SECURE
                } else {
                    Style::new()
                };
                (label, style)
            })
            .collect();
        let width = |s: &str| Line::raw(s).width();
        let head = " scrpad ─";
        let tail = " + ";
        let help = "[?] ";
        let room = usize::from(area.width).saturating_sub(width(head) + width(tail) + width(help) + 2);
        // Scrolled so the active tab shows; the ones cut off are marked with ….
        let mut first = 0;
        while first < state.active && labels[first..=state.active].iter().map(|(l, _)| width(l) + 1).sum::<usize>() > room {
            first += 1;
        }
        let mut spans = vec![Span::styled(head, style::MUTED)];
        if first > 0 {
            spans.push(Span::styled("…", style::MUTED));
        }
        let mut used = 0;
        for (i, (label, style)) in labels.iter().enumerate().skip(first) {
            if used + width(label) + 1 > room && i > state.active {
                spans.push(Span::styled("…", style::MUTED));
                break;
            }
            used += width(label) + 1;
            if i > first {
                spans.push(Span::styled("│", style::MUTED));
            }
            spans.push(Span::styled(label.as_str(), *style));
        }
        spans.push(Span::styled("│", style::MUTED));
        spans.push(Span::styled(tail, style::MUTED));
        frame.render_widget(Line::from(spans), area);
        frame.render_widget(Line::styled(help, style::MUTED).right_aligned(), area);
    }

    /// The note, its encrypted text, or what a locked note shows; where the cursor goes.
    fn body(&mut self, frame: &mut Frame, area: Rect) -> Option<Position> {
        if area.height == 0 {
            return None;
        }
        let note = &self.session.state.notes[self.session.state.active];
        if self.session.showing_cipher() {
            let label = Line::styled("Encrypted, as saved to disk · Esc to go back", style::SECURE);
            frame.render_widget(label, Rect { height: 1, ..area });
            let text = Rect { y: area.y + 1, height: area.height - 1, ..area };
            let cipher = Paragraph::new(note.cipher.as_deref().unwrap_or_default())
                .style(style::MUTED)
                .wrap(Wrap { trim: false })
                .scroll((self.cipher_scroll, 0));
            frame.render_widget(cipher, text);
            return None;
        }
        if note.is_locked() {
            let lines = vec![
                Line::styled("[locked] Encrypted note", style::SECURE.add_modifier(Modifier::BOLD)),
                Line::styled("Enter to unlock · Ctrl+E shows the encrypted text", style::MUTED),
            ];
            let y = area.y + area.height.saturating_sub(2) / 2;
            frame.render_widget(Paragraph::new(lines).centered(), Rect { y, height: 2.min(area.height), ..area });
            return None;
        }
        let editor = self.editors.entry(note.id).or_insert_with(|| Editor::new(&note.text));
        let at = editor.draw(&note.text, note.hidden, area, frame.buffer_mut(), Style::new());
        if note.text.is_empty() {
            let hint =
                if note.hidden { Line::styled("hidden", style::MUTED).right_aligned() } else { Line::styled(PLACEHOLDER, style::MUTED) };
            frame.render_widget(hint, Rect { height: 1, ..area });
        }
        Some(at)
    }

    /// The bottom line: a toast with its Undo, or the keys that matter for this note.
    fn status(&self, frame: &mut Frame, area: Rect) {
        let line = if let Some(toast) = &self.session.toast {
            let mut spans = vec![Span::raw(" "), Span::raw(toast.text.as_str())];
            if let Some(ToastAction::Undo(_)) = &toast.action {
                spans.extend([Span::styled(" · ", style::MUTED), Span::styled("Alt+U Undo", style::ACCENT)]);
            }
            Line::from(spans)
        } else {
            let note = self.session.state.current();
            let found = self.session.findings(note.id);
            let lock = match (note.is_encrypted(), note.is_locked()) {
                (false, _) => "encrypt",
                (true, false) => "lock",
                (true, true) => "unlock",
            };
            let hide = if note.hidden { "show" } else { "hide" };
            let keys = [("^N", "new"), ("M-h", hide), ("^L", lock), ("^Y", "copy"), ("^Q", "quit"), ("F10", "menu"), ("F1", "keys")];
            let mut spans = vec![];
            let mut used = 0;
            if let Some(first) = found.first() {
                let more = if found.len() > 1 { format!(" (+{} more)", found.len() - 1) } else { String::new() };
                let warning = format!(" [!] line {}: {}{more} ·", first.line, first.kind.label());
                used += warning.chars().count();
                spans.push(Span::styled(warning, style::ERROR));
            }
            // Whole hints only: one cut in half helps no one.
            for (key, what) in keys {
                used += key.len() + what.len() + 3;
                if used > usize::from(area.width) {
                    break;
                }
                spans.extend([Span::styled(format!(" {key} "), style::ACCENT), Span::styled(what, style::MUTED), Span::raw(" ")]);
            }
            Line::from(spans)
        };
        frame.render_widget(line, area);
    }

    /// Draws the open box, if any; where its cursor goes.
    fn modal_box(&self, frame: &mut Frame, area: Rect) -> Option<Position> {
        let modal = self.modal.as_ref()?;
        let w = area.width.saturating_sub(4).min(if matches!(modal, Modal::Help) { 66 } else { 56 });
        let inner_w = usize::from(w.saturating_sub(4));
        let mut lines: Vec<Line> = Vec::new();
        // The line of the field typed in, and how far in.
        let mut field: Option<(usize, usize)> = None;
        // The line to keep in view when the box is taller than the terminal.
        let mut keep = 0;
        let title;
        match modal {
            Modal::Encrypt { password, again, second, error, .. } => {
                title = " Encrypt note ";
                let about = "The text is saved to disk only encrypted. The password is not stored and cannot be recovered.";
                lines.extend(editor::wrap(about, inner_w).into_iter().map(|l| Line::styled(l, style::MUTED)));
                lines.push(Line::raw(""));
                for (label, value, active) in [("Password        ", password, !*second), ("Repeat password ", again, *second)] {
                    let dots = masked(value, inner_w.saturating_sub(label.len()));
                    if active {
                        field = Some((lines.len(), label.len() + dots.chars().count()));
                    }
                    let label_style = if active { Style::new().add_modifier(Modifier::BOLD) } else { style::MUTED };
                    lines.push(Line::from(vec![Span::styled(label, label_style), Span::raw(dots)]));
                }
                if let Some(e) = error {
                    lines.push(Line::styled(*e, style::ERROR));
                }
                lines.push(Line::raw(""));
                let enter = if *second { "Enter encrypt" } else { "Enter next" };
                lines.push(Line::styled(format!("{enter} · Tab switch · Esc cancel"), style::MUTED));
            }
            Modal::Unlock { password, error, .. } => {
                title = " Encrypted note ";
                let label = "Password ";
                let dots = masked(password, inner_w.saturating_sub(label.len()));
                field = Some((0, label.len() + dots.chars().count()));
                lines.push(Line::from(vec![Span::styled(label, Style::new().add_modifier(Modifier::BOLD)), Span::raw(dots)]));
                if let Some(e) = error {
                    lines.push(Line::styled(*e, style::ERROR));
                }
                lines.push(Line::raw(""));
                lines.push(Line::styled("Enter unlock · Ctrl+E encrypted text · Esc close", style::MUTED));
            }
            Modal::Rename { title: typed, .. } => {
                title = " Rename note ";
                let shown = tail(typed, inner_w.saturating_sub(1));
                field = Some((0, Line::raw(shown).width()));
                lines.push(Line::raw(shown));
                lines.push(Line::raw(""));
                lines.push(Line::styled("Enter rename · Esc cancel", style::MUTED));
            }
            Modal::Menu { selected } => {
                title = " Menu ";
                let items = self.menu();
                let selected = (*selected).min(items.len() - 1);
                keep = selected;
                for (i, item) in items.iter().enumerate() {
                    let check = match item.checked {
                        Some(true) => "[x] ",
                        Some(false) => "[ ] ",
                        None => "",
                    };
                    let label = format!("{check}{}", item.label);
                    let pad = inner_w.saturating_sub(Line::raw(label.as_str()).width() + item.keys.len());
                    let text = format!("{label}{}{}", " ".repeat(pad), item.keys);
                    lines.push(Line::styled(text, if i == selected { style::SELECTED } else { Style::new() }));
                }
            }
            Modal::Approve { password, error } => {
                use crate::session::Ask;
                title = " Allow? ";
                let Some(approval) = &self.session.approval else { return None };
                let wrap = |text: &str, style: Style| -> Vec<Line<'static>> {
                    editor::wrap(text, inner_w).into_iter().map(|l| Line::styled(l.to_owned(), style)).collect()
                };
                match &approval.ask {
                    Ask::Write { title, target: crate::handoff::Target { path, exists, .. }, git_ignored } => {
                        lines.extend(wrap(&format!("Write “{title}” to a file?"), Style::new().add_modifier(Modifier::BOLD)));
                        lines.extend(wrap(&path.display().to_string(), style::MUTED));
                        if *exists {
                            lines.extend(wrap("Replaces the file that is there.", style::ERROR));
                        }
                        if *git_ignored == Some(false) {
                            lines.extend(wrap("Git does not ignore this file: it could be committed.", style::ERROR));
                        }
                    }
                    Ask::Run { title, argv } => {
                        lines.extend(wrap(&format!("Give “{title}” to a command?"), Style::new().add_modifier(Modifier::BOLD)));
                        lines.extend(wrap(&crate::handoff::shown_command(argv), style::MUTED));
                        lines.extend(wrap("Its KEY=VALUE lines become the command's environment.", style::MUTED));
                    }
                    Ask::Read { title, text, findings, .. } => {
                        lines.extend(wrap(&format!("Let the agent read “{title}”?"), Style::new().add_modifier(Modifier::BOLD)));
                        let size = crate::session::size_label(text.len());
                        lines.extend(wrap(&format!("{size}. It may take this text as your instructions."), Style::new()));
                        lines.push(Line::raw(""));
                        // Hidden characters shown escaped; a few lines, the rest only counted.
                        let (shown, _) = crate::scan::shown_text(text, 400, 32 << 10);
                        let mut rows = 0;
                        let mut started = 0;
                        for (n, line) in shown.lines().enumerate() {
                            for part in editor::wrap(line, inner_w.saturating_sub(5)) {
                                if rows < 6 {
                                    let flagged = findings.iter().any(|f| f.line == n + 1);
                                    lines.push(Line::from(vec![
                                        Span::styled(format!("{:>3} ", n + 1), style::MUTED),
                                        Span::styled(part.to_owned(), if flagged { style::ERROR } else { Style::new() }),
                                    ]));
                                    rows += 1;
                                    started = n + 1;
                                }
                            }
                        }
                        // What the agent gets beyond the lines shown, however far the preview cut.
                        let total = text.lines().count();
                        let left = crate::scan::LeftOut {
                            lines: total.saturating_sub(started),
                            bytes: text.split_inclusive('\n').skip(started).map(str::len).sum(),
                        };
                        if left.lines > 0 {
                            lines.extend(wrap(&crate::session::left_out_line(left), style::MUTED));
                        }
                        if !findings.is_empty() {
                            lines.push(Line::raw(""));
                            lines.extend(wrap("Looks like instructions for an AI:", style::ERROR));
                            for f in findings.iter().take(6) {
                                lines.extend(wrap(&format!("line {}: {}", f.line, f.kind.label()), style::ERROR));
                            }
                            if findings.len() > 6 {
                                lines.extend(wrap(&format!("… and {} more", findings.len() - 6), style::ERROR));
                            }
                        }
                    }
                }
                let needs = self.session.approval_needs_password();
                if needs {
                    lines.push(Line::raw(""));
                    let label = "Password ";
                    let dots = masked(password, inner_w.saturating_sub(label.len()));
                    field = Some((lines.len(), label.len() + dots.chars().count()));
                    lines.push(Line::from(vec![Span::styled(label, Style::new().add_modifier(Modifier::BOLD)), Span::raw(dots)]));
                }
                if let Some(e) = error {
                    lines.push(Line::styled(e.as_str(), style::ERROR));
                }
                lines.push(Line::raw(""));
                lines.push(Line::styled(if needs { "Enter allow · Esc deny" } else { "y allow · n/Esc deny" }, style::MUTED));
            }
            Modal::Help => {
                title = " Keys ";
                let key_w = KEYS.iter().map(|(k, _)| k.chars().count()).max().unwrap_or(0) + 2;
                for (keys, what) in KEYS {
                    let pad = " ".repeat(key_w - keys.chars().count());
                    lines.push(Line::from(vec![Span::styled(format!("{keys}{pad}"), style::ACCENT), Span::raw(*what)]));
                }
                lines.push(Line::raw(""));
                lines.push(Line::styled("Typing, arrows, Home/End, Ctrl+←/→ (words) edit as usual. Any key closes.", style::MUTED));
            }
        }
        let h = (lines.len() as u16 + 2).min(area.height);
        let rect = Rect::new(area.x + (area.width - w) / 2, area.y + area.height.saturating_sub(h) / 2, w, h);
        let border = if matches!(modal, Modal::Encrypt { .. } | Modal::Unlock { .. } | Modal::Approve { .. }) {
            style::SECURE
        } else {
            Style::new()
        };
        frame.render_widget(Clear, rect);
        let block = Block::bordered().title(title).border_style(border).padding(ratatui::widgets::Padding::horizontal(1));
        let inner = block.inner(rect);
        let scroll = keep.saturating_sub(usize::from(inner.height).saturating_sub(1)) as u16;
        frame.render_widget(Paragraph::new(lines).block(block).scroll((scroll, 0)), rect);
        let (line, col) = field?;
        Some(Position::new(inner.x + (col as u16).min(inner.width.saturating_sub(1)), inner.y + line as u16))
    }
}

/// A menu row: what it does, its key, and its state when it is an option.
#[derive(Clone, Copy)]
struct MenuItem {
    label: &'static str,
    keys: &'static str,
    action: Action,
    checked: Option<bool>,
}

impl MenuItem {
    fn new(label: &'static str, keys: &'static str, action: Action) -> Self {
        Self { label, keys, action, checked: None }
    }

    fn option(label: &'static str, action: Action, on: bool) -> Self {
        Self { label, keys: "", action, checked: Some(on) }
    }
}

/// The action a key is bound to, wherever the focus is. Ctrl+letter arrives in lower case, but
/// some terminals send it in upper case with Shift.
fn binding(key: KeyEvent) -> Option<Action> {
    let ctrl = key.modifiers.contains(KeyModifiers::CONTROL);
    let alt = key.modifiers.contains(KeyModifiers::ALT);
    Some(match key.code {
        KeyCode::Char(c) if ctrl && !alt => match c.to_ascii_lowercase() {
            'q' => Action::Quit,
            'n' | 't' => Action::New,
            'w' => Action::Close,
            'l' => Action::Lock,
            'k' => Action::LockAll,
            'e' => Action::Cipher,
            'y' => Action::Copy,
            'z' => Action::Undo,
            'r' => Action::Redo,
            _ => return None,
        },
        KeyCode::Char(c) if alt && !ctrl => match c.to_ascii_lowercase() {
            'h' => Action::Hide,
            'u' => Action::UndoToast,
            'm' => Action::Menu,
            '9' => Action::Go(usize::MAX),
            d @ '1'..='8' => Action::Go(usize::from(d as u8 - b'1')),
            _ => return None,
        },
        KeyCode::PageUp if ctrl => Action::Previous,
        KeyCode::PageDown if ctrl => Action::Next,
        KeyCode::Left if alt => Action::Previous,
        KeyCode::Right if alt => Action::Next,
        KeyCode::F(1) => Action::Help,
        KeyCode::F(2) => Action::Rename,
        KeyCode::F(10) => Action::Menu,
        _ => return None,
    })
}

/// Typing in a one-line field: characters, Backspace, and Ctrl+U to clear it.
fn field_key(field: &mut String, key: KeyEvent) {
    let ctrl = key.modifiers.contains(KeyModifiers::CONTROL);
    let alt = key.modifiers.contains(KeyModifiers::ALT);
    match key.code {
        KeyCode::Char('u') if ctrl && !alt => field.clear(),
        KeyCode::Char(c) if ctrl == alt && !c.is_control() => field.push(c),
        KeyCode::Backspace => {
            field.pop();
        }
        _ => {}
    }
}

fn field_paste(field: &mut String, pasted: &str) {
    field.extend(pasted.lines().next().unwrap_or_default().chars().filter(|c| !c.is_control()));
}

/// A password as dots, one per character, at most `max` of them.
fn masked(password: &str, max: usize) -> String {
    "•".repeat(password.chars().count().min(max))
}

fn truncate(title: &str, max: usize) -> String {
    if title.chars().count() <= max { title.to_owned() } else { title.chars().take(max - 1).chain(['…']).collect() }
}

/// The end of a typed line that fits `max` columns, so the cursor at its end shows.
fn tail(text: &str, max: usize) -> &str {
    let mut start = 0;
    while Line::raw(&text[start..]).width() > max {
        start += text[start..].chars().next().map_or(1, char::len_utf8);
    }
    &text[start..]
}

/// The escape sequence that asks the terminal to put `text` on the system clipboard (OSC 52).
/// Built in one allocation of the final size, so no stray copy of the text is left behind.
fn osc52(text: &str) -> Zeroizing<String> {
    let mut seq = Zeroizing::new(String::with_capacity(8 + text.len().div_ceil(3) * 4));
    seq.push_str("\x1b]52;c;");
    base64::engine::general_purpose::STANDARD.encode_string(text, &mut seq);
    seq.push('\x07');
    seq
}

impl Drop for Tui {
    fn drop(&mut self) {
        self.finish();
    }
}
