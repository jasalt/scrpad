//! What scripts and AI agents may do with the notes: `scrpad --append`, `--list` and `--read`.
//! Appending is always allowed and never replaces or removes anything. Reading is off until the
//! user sets “Let scripts read notes” to ask each time or always, and never reaches a hidden or
//! an encrypted note.
//!
//! The notes file is where scripts meet the notes. With no window running, `answer` loads it,
//! changes it and saves it. A running window has the same done by the thread that answers its
//! socket, under the lock the window saves under (`Window`): that way a script gets its answer
//! even while the window is hidden and draws no frames, and an append is on disk at once.

use std::collections::HashSet;
use std::path::Path;

use crate::cli::Command;
use crate::instance::Reply;
use crate::store::{Note, ReadMode, State};

pub const NOBODY_TO_ASK: &str = "scrpad is not open to ask you; open it (the window, or scrpad --tui) and try again";
pub const READING_ASKS: &str = "scrpad asks you before each read now; try again";
pub const READING_OFF: &str = "reading notes is off; turn on “Let scripts read notes” in the … menu of scrpad";

/// What a running window shares with the thread that answers scripts. The window holds the lock
/// while it saves, and adds the waiting appends to its notes first, so an append on disk is never
/// overwritten by a save that lacks it.
#[derive(Default)]
pub struct Window {
    /// Appends (title, text) saved to the notes file, still to be added to the window's notes.
    pub appends: Vec<(String, String)>,
    /// Where the window is ahead of its last save: the read option as it is now, and the notes
    /// (by id) hidden or encrypted now.
    pub reading: ReadMode,
    pub hidden: HashSet<u64>,
    pub encrypted: HashSet<u64>,
    /// The encrypted notes unlocked now: only the window has their password, so an append to
    /// one goes to the window, which adds it and saves it sealed.
    pub unlocked: HashSet<u64>,
    /// The window could not load the notes file and saves nothing, so scripts don't either.
    pub blocked: bool,
}

impl Window {
    /// Catches up with the window's notes and options.
    pub fn track(&mut self, state: &State) {
        self.reading = state.script_reading;
        self.hidden.clear();
        self.hidden.extend(state.notes.iter().filter(|n| n.hidden).map(|n| n.id));
        self.encrypted.clear();
        self.encrypted.extend(state.notes.iter().filter(|n| n.is_encrypted()).map(|n| n.id));
        self.unlocked.clear();
        self.unlocked.extend(state.notes.iter().filter(|n| n.is_encrypted() && !n.is_locked()).map(|n| n.id));
    }
}

/// Carries out `--append`, `--list` or `--read` on the notes file at `path`, as `window` sees
/// it when one runs. Appending keeps the file's rules: it is saved atomically, an unreadable one
/// is moved aside first, and encrypted notes stay exactly as their cipher (they are never
/// unlocked here).
pub fn answer(path: Option<&Path>, command: &Command, window: Option<&mut Window>) -> Reply {
    carry_out(path, command, window).unwrap_or_else(Reply::Error)
}

fn carry_out(path: Option<&Path>, command: &Command, mut window: Option<&mut Window>) -> Result<Reply, String> {
    // `--write` and `--run` wait for the user's answer, and only a running scrpad can ask.
    if command.needs_approval() {
        return Err(NOBODY_TO_ASK.into());
    }
    let path = path.ok_or("scrpad has no notes file here (no config directory)")?;
    if window.as_ref().is_some_and(|w| w.blocked) {
        return Err("scrpad could not load its notes file and saves nothing until it is restarted".into());
    }
    let mut state = State::load(path).map_err(|e| e.to_string())?;
    // A read that must be asked about is the running window's to answer.
    if matches!(command, Command::Read(_)) {
        match reading(&state, window.as_deref()) {
            ReadMode::Ask if window.is_none() => return Err(NOBODY_TO_ASK.into()),
            ReadMode::Ask => return Err(READING_ASKS.into()),
            _ => {}
        }
    }
    match command {
        Command::Append { title, text } => {
            let target = state.notes.iter().find(|n| n.title == *title);
            if let (Some(w), Some(note)) = (window.as_deref_mut(), target) {
                // Never into the file: the window adds it and saves the note sealed.
                if w.unlocked.contains(&note.id) {
                    w.appends.push((title.clone(), text.clone()));
                    return Ok(Reply::Ok);
                }
                // Encrypted in the window, though not saved that way yet.
                if w.encrypted.contains(&note.id) {
                    return Err(locked(title));
                }
            }
            append(&mut state, title, text)?;
            state.save(path).map_err(|e| format!("could not save {}: {e}", path.display()))?;
            if let Some(w) = window {
                w.appends.push((title.clone(), text.clone()));
            }
            Ok(Reply::Ok)
        }
        Command::List => list(&state, window.as_deref()).map(Reply::Text),
        Command::Read(title) => read(&state, title, window.as_deref()).map(|text| Reply::Text(text.to_owned())),
        _ => Err("not a command for scripts".into()),
    }
}

fn locked(title: &str) -> String {
    format!("“{title}” is encrypted and locked; unlock it in scrpad to add to it")
}

/// Adds `text` as a new paragraph to the first note titled `title` in tab order, or to a new
/// note with that title after the others. A locked note is refused, and no other is made; only
/// a window unlocks one, and it seals what was added.
pub fn append(state: &mut State, title: &str, text: &str) -> Result<(), String> {
    match state.notes.iter().position(|n| n.title == title) {
        Some(i) if state.notes[i].is_locked() => return Err(locked(title)),
        Some(i) => {
            let note = &mut state.notes[i].text;
            // One blank line between paragraphs, however the note ends.
            let gap = if note.is_empty() || note.ends_with("\n\n") {
                ""
            } else if note.ends_with('\n') {
                "\n"
            } else {
                "\n\n"
            };
            note.push_str(gap);
            note.push_str(text);
        }
        None => {
            let mut note = Note::new(title.to_owned());
            note.id = state.fresh_id();
            note.text = text.to_owned();
            state.notes.push(note);
        }
    }
    Ok(())
}

/// Scripts never read a hidden or an encrypted note, whatever the option says.
fn readable(note: &Note, window: Option<&Window>) -> bool {
    !note.hidden && !note.is_encrypted() && window.is_none_or(|w| !w.hidden.contains(&note.id) && !w.encrypted.contains(&note.id))
}

/// The reading option as it is now: the window's when one runs, else the notes file's.
fn reading(state: &State, window: Option<&Window>) -> ReadMode {
    window.map_or(state.script_reading, |w| w.reading)
}

/// Both `--list` and `--read` are refused with reading off.
fn may_read(state: &State, window: Option<&Window>) -> Result<(), String> {
    if reading(state, window) == ReadMode::Off { Err(READING_OFF.into()) } else { Ok(()) }
}

/// Whether the running scrpad must ask the user before carrying out `command`: always for
/// `--write` and `--run`, and for `--read` while reading is set to ask each time.
pub fn asks(command: &Command, window: &Window) -> bool {
    command.needs_approval() || matches!(command, Command::Read(_)) && window.reading == ReadMode::Ask
}

/// The titles of the readable notes, one per line, in tab order.
pub fn list(state: &State, window: Option<&Window>) -> Result<String, String> {
    may_read(state, window)?;
    Ok(state.notes.iter().filter(|n| readable(n, window)).map(|n| format!("{}\n", n.title)).collect())
}

/// The text of the first readable note titled `title`, in tab order.
pub fn read<'a>(state: &'a State, title: &str, window: Option<&Window>) -> Result<&'a str, String> {
    may_read(state, window)?;
    state
        .notes
        .iter()
        .find(|n| n.title == title && readable(n, window))
        .map(|n| n.text.as_str())
        .ok_or_else(|| format!("no readable note is titled “{title}”"))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;
    use std::sync::atomic::{AtomicU64, Ordering};

    fn temp_notes(name: &str) -> (PathBuf, PathBuf) {
        static SEQUENCE: AtomicU64 = AtomicU64::new(0);
        let n = SEQUENCE.fetch_add(1, Ordering::Relaxed);
        let dir = std::env::temp_dir().join(format!("scrpad-scripts-{name}-{}-{n}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        (dir.join("notes.json"), dir)
    }

    fn note(title: &str, text: &str) -> Note {
        Note { text: text.into(), ..Note::new(title.into()) }
    }

    fn state(notes: Vec<Note>) -> State {
        let mut s = State { notes, script_reading: ReadMode::Always, ..State::default() };
        s.fix_up();
        s
    }

    fn titles(s: &State) -> Vec<&str> {
        s.notes.iter().map(|n| n.title.as_str()).collect()
    }

    fn append_cmd(title: &str, text: &str) -> Command {
        Command::Append { title: title.into(), text: text.into() }
    }

    #[test]
    fn handoff_needs_a_running_scrpad() {
        let (path, _) = temp_notes("handoff");
        let cmd = Command::Run { title: ".env".into(), argv: vec!["true".into()] };
        assert_eq!(answer(Some(&path), &cmd, None), Reply::Error(NOBODY_TO_ASK.into()));
        assert!(!path.exists(), "nothing was written");
    }

    #[test]
    fn appending_adds_a_paragraph_to_the_first_note_with_that_exact_title() {
        let mut s =
            state(vec![note("agent", "lower"), note("Agent", "first"), note("Agent", "second"), note("Empty", ""), note("Ends", "line\n")]);
        append(&mut s, "Agent", "cargo test").unwrap();
        append(&mut s, "Empty", "one").unwrap();
        append(&mut s, "Ends", "two").unwrap();
        assert_eq!(s.notes[1].text, "first\n\ncargo test");
        assert_eq!(s.notes[2].text, "second");
        assert_eq!(s.notes[0].text, "lower", "titles match exactly");
        assert_eq!(s.notes[3].text, "one");
        assert_eq!(s.notes[4].text, "line\n\ntwo");
        append(&mut s, "Ends", "three").unwrap();
        assert_eq!(s.notes[4].text, "line\n\ntwo\n\nthree");
        // A missing title makes a note after the others, without touching the active one.
        let active = s.active;
        append(&mut s, "Paths", "/etc/hosts").unwrap();
        assert_eq!(titles(&s), ["agent", "Agent", "Agent", "Empty", "Ends", "Paths"]);
        assert_eq!((s.notes[5].text.as_str(), s.active), ("/etc/hosts", active));
        let ids: HashSet<_> = s.notes.iter().map(|n| n.id).collect();
        assert_eq!(ids.len(), s.notes.len());
    }

    #[test]
    fn a_locked_note_is_refused_and_not_duplicated() {
        let mut s = state(vec![note("Keys", "sk-live"), note("Other", "")]);
        s.notes[0].encrypt("pw").unwrap();
        let cipher = s.notes[0].cipher.clone();
        s.notes[0].lock();
        let e = append(&mut s, "Keys", "more").unwrap_err();
        assert!(e.contains("locked") && e.contains("unlock it"), "{e}");
        assert_eq!(titles(&s), ["Keys", "Other"]);
        assert_eq!(s.notes[0].cipher, cipher);
    }

    #[test]
    fn an_unlocked_encrypted_note_takes_the_append_and_stays_encrypted() {
        // Only the window, which has the password, holds an unlocked note.
        let mut s = state(vec![note("Keys", "A=1"), note("Other", "")]);
        s.notes[0].encrypt("pw").unwrap();
        append(&mut s, "Keys", "B=").unwrap();
        assert_eq!(titles(&s), ["Keys", "Other"]);
        assert_eq!(s.notes[0].text, "A=1\n\nB=");
        assert!(s.notes[0].is_encrypted() && !s.notes[0].is_locked());
    }

    #[test]
    fn hidden_and_encrypted_notes_are_never_listed_or_read() {
        let mut s = state(vec![note("Todo", "ship it"), note("Key", "sk-live"), note("Vault", "secret"), note("Todo", "second")]);
        s.notes[1].hidden = true;
        s.notes[2].encrypt("pw").unwrap();
        assert_eq!(list(&s, None).unwrap(), "Todo\nTodo\n");
        assert_eq!(read(&s, "Todo", None).unwrap(), "ship it");
        assert!(read(&s, "Key", None).unwrap_err().contains("no readable note"));
        // Unlocked or not.
        assert!(read(&s, "Vault", None).is_err());
        s.notes[2].lock();
        assert!(read(&s, "Vault", None).is_err());
        // The first readable one in tab order.
        s.notes[0].hidden = true;
        assert_eq!(read(&s, "Todo", None).unwrap(), "second");
        // A running window knows of notes hidden since its last save.
        let mut window = Window::default();
        window.track(&s);
        s.notes[3].hidden = false;
        let mut shown = s.clone();
        shown.notes[0].hidden = false;
        assert_eq!(list(&shown, Some(&window)).unwrap(), "Todo\n");
        assert_eq!(read(&shown, "Todo", Some(&window)).unwrap(), "second");
    }

    #[test]
    fn reading_is_refused_with_the_option_off() {
        let mut s = state(vec![note("Todo", "ship it")]);
        s.script_reading = ReadMode::Off;
        assert_eq!(list(&s, None).unwrap_err(), READING_OFF);
        assert_eq!(read(&s, "Todo", None).unwrap_err(), READING_OFF);
        assert!(READING_OFF.contains("Let scripts read notes"));
        // A running window's option as it is now wins over the last save.
        let mut window = Window::default();
        window.track(&s);
        s.script_reading = ReadMode::Always;
        assert_eq!(read(&s, "Todo", Some(&window)).unwrap_err(), READING_OFF);
        window.reading = ReadMode::Always;
        s.script_reading = ReadMode::Off;
        assert_eq!(read(&s, "Todo", Some(&window)).unwrap(), "ship it");
        // Appending needs no option.
        assert!(append(&mut s, "Todo", "more").is_ok());
    }

    #[test]
    fn asking_lists_the_titles_and_leaves_reads_to_the_window() {
        let (path, dir) = temp_notes("ask");
        let mut s = state(vec![note("Todo", "ship it"), note("Key", "sk-live")]);
        s.notes[1].hidden = true;
        s.script_reading = ReadMode::Ask;
        s.save(&path).unwrap();
        let read_todo = Command::Read("Todo".into());
        // No window: the list works, and nobody can ask about the read.
        assert_eq!(answer(Some(&path), &Command::List, None), Reply::Text("Todo\n".into()));
        assert_eq!(answer(Some(&path), &read_todo, None), Reply::Error(NOBODY_TO_ASK.into()));
        // A window whose option is Ask answers the same; it asks before the script path is reached.
        let mut window = Window::default();
        window.track(&s);
        assert_eq!(answer(Some(&path), &Command::List, Some(&mut window)), Reply::Text("Todo\n".into()));
        assert!(asks(&read_todo, &window));
        // Mode changed to Ask a moment ago, after the window decided not to ask.
        window.reading = ReadMode::Ask;
        assert_eq!(answer(Some(&path), &read_todo, Some(&mut window)), Reply::Error(READING_ASKS.into()));
        // Off refuses both.
        window.reading = ReadMode::Off;
        assert_eq!(answer(Some(&path), &Command::List, Some(&mut window)), Reply::Error(READING_OFF.into()));
        assert_eq!(answer(Some(&path), &read_todo, Some(&mut window)), Reply::Error(READING_OFF.into()));
        s.script_reading = ReadMode::Off;
        s.save(&path).unwrap();
        assert_eq!(answer(Some(&path), &Command::List, None), Reply::Error(READING_OFF.into()));
        assert_eq!(answer(Some(&path), &read_todo, None), Reply::Error(READING_OFF.into()));
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn only_a_read_in_ask_mode_and_the_handoffs_are_asked_about() {
        let mut window = Window::default();
        let read = Command::Read("a".into());
        let write = Command::Write { title: "a".into(), dir: "/".into() };
        let run = Command::Run { title: "a".into(), argv: vec!["x".into()] };
        for (mode, reads) in [(ReadMode::Off, false), (ReadMode::Ask, true), (ReadMode::Always, false)] {
            window.reading = mode;
            assert_eq!(asks(&read, &window), reads, "{mode:?}");
            assert!(asks(&write, &window) && asks(&run, &window));
            assert!(!asks(&Command::List, &window) && !asks(&append_cmd("a", "b"), &window));
        }
    }

    #[test]
    fn appending_with_no_window_saves_the_file_and_keeps_ciphers() {
        let (path, dir) = temp_notes("file");
        let mut s = state(vec![note("Agent", "first"), note("Vault", "the launch codes")]);
        s.notes[1].encrypt("hunter2").unwrap();
        s.save(&path).unwrap();
        let cipher = s.notes[1].cipher.clone();

        assert_eq!(answer(Some(&path), &append_cmd("Agent", "cargo test"), None), Reply::Ok);
        assert_eq!(answer(Some(&path), &append_cmd("New", "ls"), None), Reply::Ok);
        let Reply::Error(e) = answer(Some(&path), &append_cmd("Vault", "more"), None) else { panic!("appended to an encrypted note") };
        assert!(e.contains("encrypted"));

        let json = std::fs::read_to_string(&path).unwrap();
        assert!(!json.contains("launch codes") && !json.contains("hunter2"), "{json}");
        let loaded = State::load(&path).unwrap();
        assert_eq!(titles(&loaded), ["Agent", "Vault", "New"]);
        assert_eq!(loaded.notes[0].text, "first\n\ncargo test");
        assert_eq!(loaded.notes[2].text, "ls");
        assert!(loaded.notes[1].is_locked());
        assert_eq!(loaded.notes[1].cipher, cipher, "the cipher is kept exactly");
        // Saved with the file's own rules: private.
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            assert_eq!(std::fs::metadata(&path).unwrap().permissions().mode() & 0o777, 0o600);
        }
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn an_unreadable_file_is_moved_aside_not_overwritten() {
        let (path, dir) = temp_notes("corrupt");
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(&path, "{ not json").unwrap();
        assert_eq!(answer(Some(&path), &append_cmd("Agent", "x"), None), Reply::Ok);
        assert_eq!(std::fs::read_to_string(dir.join("notes.json.bak")).unwrap(), "{ not json");
        assert_eq!(State::load(&path).unwrap().notes.last().unwrap().text, "x");
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn the_file_answers_reads_as_its_option_says() {
        let (path, dir) = temp_notes("read");
        let mut s = state(vec![note("Todo", "ship it"), note("Key", "sk-live")]);
        s.notes[1].hidden = true;
        s.script_reading = ReadMode::Off;
        s.save(&path).unwrap();
        assert_eq!(answer(Some(&path), &Command::List, None), Reply::Error(READING_OFF.into()));
        s.script_reading = ReadMode::Always;
        s.save(&path).unwrap();
        assert_eq!(answer(Some(&path), &Command::List, None), Reply::Text("Todo\n".into()));
        assert_eq!(answer(Some(&path), &Command::Read("Todo".into()), None), Reply::Text("ship it".into()));
        assert!(matches!(answer(Some(&path), &Command::Read("Key".into()), None), Reply::Error(_)));
        // No file yet: nothing to read, and the option is off.
        let (missing, _) = temp_notes("missing");
        assert_eq!(answer(Some(&missing), &Command::List, None), Reply::Error(READING_OFF.into()));
        assert!(matches!(answer(None, &Command::List, None), Reply::Error(_)));
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn a_running_window_gets_the_append_and_has_the_last_word() {
        let (path, dir) = temp_notes("window");
        let mut s = state(vec![note("Agent", ""), note("Vault", "secret")]);
        s.save(&path).unwrap();
        // Encrypted in the window since the last save.
        s.notes[1].encrypt("pw").unwrap();
        let mut window = Window::default();
        window.track(&s);
        assert_eq!(answer(Some(&path), &append_cmd("Agent", "pwd"), Some(&mut window)), Reply::Ok);
        assert_eq!(window.appends, [("Agent".to_owned(), "pwd".to_owned())]);
        assert_eq!(State::load(&path).unwrap().notes[0].text, "pwd");
        window.blocked = true;
        assert!(matches!(answer(Some(&path), &append_cmd("Agent", "x"), Some(&mut window)), Reply::Error(_)));
        assert_eq!(State::load(&path).unwrap().notes[0].text, "pwd");
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn an_unlocked_encrypted_note_gets_the_append_only_through_the_window() {
        let (path, dir) = temp_notes("unlocked");
        let mut s = state(vec![note("Agent", ""), note(".env", "A=1"), note("Vault", "secret")]);
        s.notes[1].encrypt("pw").unwrap();
        s.notes[2].encrypt("pw").unwrap();
        s.save(&path).unwrap();
        let before = std::fs::read_to_string(&path).unwrap();
        // The window has .env unlocked and Vault locked.
        s.notes[2].lock();
        let mut window = Window::default();
        window.track(&s);

        assert_eq!(answer(Some(&path), &append_cmd(".env", "B=2"), Some(&mut window)), Reply::Ok);
        assert_eq!(window.appends, [(".env".to_owned(), "B=2".to_owned())]);
        // The window adds it and saves it sealed; the file is not touched here.
        assert_eq!(std::fs::read_to_string(&path).unwrap(), before);

        let Reply::Error(e) = answer(Some(&path), &append_cmd("Vault", "x"), Some(&mut window)) else {
            panic!("appended to a locked note")
        };
        assert!(e.contains("locked"), "{e}");
        assert_eq!(window.appends.len(), 1);
        assert_eq!(std::fs::read_to_string(&path).unwrap(), before);

        // No window: nothing could seal it.
        let Reply::Error(e) = answer(Some(&path), &append_cmd(".env", "C=3"), None) else { panic!("appended with no window") };
        assert!(e.contains("locked"), "{e}");
        assert_eq!(std::fs::read_to_string(&path).unwrap(), before);
        let _ = std::fs::remove_dir_all(dir);
    }
}
