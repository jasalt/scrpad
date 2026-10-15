//! The notes and window preferences, saved as JSON in the user's config directory.

use std::collections::BTreeMap;
use std::ffi::{OsStr, OsString};
use std::fs::OpenOptions;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};

use serde::{Deserialize, Serialize};

use zeroize::Zeroize;

use crate::crypto;
use crate::theme::ThemeKind;

static TEMP_FILE_SEQUENCE: AtomicU64 = AtomicU64::new(0);

#[derive(Debug)]
pub struct LoadError {
    path: PathBuf,
    reason: String,
    backup_path: PathBuf,
    backup_error: std::io::Error,
}

impl std::fmt::Display for LoadError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "could not load {} ({}), and could not move it to {}: {}",
            self.path.display(),
            self.reason,
            self.backup_path.display(),
            self.backup_error
        )
    }
}

impl std::error::Error for LoadError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        Some(&self.backup_error)
    }
}

#[derive(Clone, Debug, Deserialize, PartialEq)]
pub struct Note {
    /// Stable identity across tab moves (editor undo history and undo toasts key on it).
    #[serde(default)]
    pub id: u64,
    pub title: String,
    #[serde(default)]
    pub text: String,
    /// Masked on screen (API keys and the like); the text itself is unchanged.
    #[serde(default)]
    pub hidden: bool,
    /// The text as a SimpleTextCrypt message. Set: the note is encrypted and `text` never reaches the disk.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cipher: Option<String>,
    /// Runtime only: the password of an unlocked encrypted note.
    #[serde(skip)]
    pub password: Option<String>,
}

/// What a note looks like on disk: the plaintext of an encrypted note is written as "".
#[derive(Serialize)]
struct NoteOnDisk<'a> {
    id: u64,
    title: &'a str,
    text: &'a str,
    hidden: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    cipher: Option<&'a str>,
}

impl Serialize for Note {
    fn serialize<S: serde::Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        NoteOnDisk {
            id: self.id,
            title: &self.title,
            text: if self.is_encrypted() { "" } else { &self.text },
            hidden: self.hidden,
            cipher: self.cipher.as_deref(),
        }
        .serialize(serializer)
    }
}

impl Note {
    pub fn new(title: String) -> Self {
        Self { id: 0, title, text: String::new(), hidden: false, cipher: None, password: None }
    }

    pub fn is_encrypted(&self) -> bool {
        self.cipher.is_some()
    }

    /// Encrypted and no password yet: `text` is empty.
    pub fn is_locked(&self) -> bool {
        self.is_encrypted() && self.password.is_none()
    }

    /// Encrypts the text with `password` (also changes the password of an encrypted note).
    pub fn encrypt(&mut self, password: &str) -> Result<(), crypto::Error> {
        self.cipher = Some(crypto::encrypt(password, &self.text)?);
        self.password = Some(password.to_owned());
        Ok(())
    }

    /// Re-encrypts the current text with the stored password; a no-op unless unlocked and encrypted.
    pub fn reseal(&mut self) -> Result<(), crypto::Error> {
        if let (Some(_), Some(password)) = (&self.cipher, &self.password) {
            self.cipher = Some(crypto::encrypt(password, &self.text)?);
        }
        Ok(())
    }

    /// Decrypts the cipher into `text` and keeps the password for `reseal`.
    pub fn unlock(&mut self, password: &str) -> Result<(), crypto::Error> {
        let cipher = self.cipher.as_deref().ok_or(crypto::Error::InvalidFormat)?;
        self.text = crypto::decrypt(password, cipher)?;
        self.password = Some(password.to_owned());
        Ok(())
    }

    /// Wipes the text and password from memory. `reseal` first: edits since the last one are lost.
    pub fn lock(&mut self) {
        if self.is_encrypted() {
            self.text.zeroize();
            self.password.zeroize();
            self.password = None;
        }
    }

    /// Drops the encryption and keeps the text; a no-op on a locked note (its text is not there).
    pub fn remove_encryption(&mut self) {
        if !self.is_locked() {
            self.cipher = None;
            self.password.zeroize();
            self.password = None;
        }
    }
}

#[derive(Clone, Copy, Debug, Serialize, Deserialize, PartialEq, Eq, Default)]
pub enum FontStyle {
    #[default]
    Sans,
    Mono,
}

/// Whether `scrpad --list` and `--read` may read the notes that are neither hidden nor encrypted.
#[derive(Clone, Copy, Debug, Serialize, Deserialize, PartialEq, Eq, Default)]
#[serde(rename_all = "lowercase")]
pub enum ReadMode {
    /// `--list` works; each `--read` waits for the user to allow it in a running scrpad.
    Ask,
    /// Both work with no question.
    Always,
    /// Both are refused. Also what any unknown value loads as.
    #[default]
    #[serde(other)]
    Off,
}

impl ReadMode {
    pub fn label(self) -> &'static str {
        match self {
            ReadMode::Off => "Off",
            ReadMode::Ask => "Ask each time",
            ReadMode::Always => "Always",
        }
    }
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
#[serde(default)]
pub struct State {
    pub notes: Vec<Note>,
    pub active: usize,
    pub pinned: bool,
    pub theme: ThemeKind,
    pub font: FontStyle,
    pub font_size: f32,
    /// Last floating window size and position in logical points (position is not restorable
    /// everywhere: Wayland lets only the compositor place windows).
    pub window_size: Option<[f32; 2]>,
    pub window_pos: Option<[f32; 2]>,
    /// Ids are never reused: editor state (undo history included) is keyed on them, and a new
    /// note must not inherit what a closed, possibly encrypted, note held.
    pub next_id: u64,
    /// Lock every unlocked encrypted note when the window hides or sits idle.
    pub auto_lock: bool,
    /// Hide again, on the same occasions, the hidden notes revealed since.
    pub auto_rehide: bool,
    /// Warn about orders aimed at an AI, hidden characters and the like. Off: no scan runs.
    pub warnings: bool,
    /// What `scrpad --list` and `--read` may do. Off unless the user chooses otherwise: what a
    /// script reads may end up with an AI provider.
    pub script_reading: ReadMode,
    /// Keep the whole window out of screenshots, recordings and screen sharing, not only
    /// while a hidden or encrypted note is open (the GUI does that regardless).
    pub hide_from_capture: bool,
    /// Per note title: the variable names (never values) of the last `--run` the user allowed,
    /// for the dialog to point out new ones. Plain and hidden notes only: an encrypted note's
    /// names stay in the session, out of the file.
    #[serde(skip_serializing_if = "BTreeMap::is_empty")]
    pub run_names: BTreeMap<String, Vec<String>>,
}

impl Default for State {
    fn default() -> Self {
        let mut state = Self {
            notes: vec![Note::new("Scratchpad 1".into())],
            active: 0,
            pinned: true,
            theme: ThemeKind::Dark,
            font: FontStyle::Sans,
            font_size: 14.5,
            window_size: None,
            window_pos: None,
            next_id: 1,
            auto_lock: true,
            auto_rehide: true,
            warnings: true,
            script_reading: ReadMode::Off,
            hide_from_capture: false,
            run_names: BTreeMap::new(),
        };
        state.fix_up();
        state
    }
}

impl State {
    pub fn default_path() -> Option<PathBuf> {
        dirs::config_dir().map(|d| d.join("scrpad").join("notes.json"))
    }

    /// A file that exists but can't be read is moved to a fresh backup name. If it cannot be
    /// preserved, loading fails so the caller cannot accidentally overwrite the original.
    pub fn load(path: &Path) -> Result<Self, LoadError> {
        let mut state = match std::fs::read_to_string(path) {
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => State::default(),
            read => match read.map_err(|e| e.to_string()).and_then(|s| serde_json::from_str(&s).map_err(|e| e.to_string())) {
                Ok(state) => state,
                Err(reason) => {
                    let backup_path = next_backup_path(path);
                    std::fs::rename(path, &backup_path).map_err(|backup_error| LoadError {
                        path: path.to_owned(),
                        reason: reason.clone(),
                        backup_path: backup_path.clone(),
                        backup_error,
                    })?;
                    eprintln!("scrpad: could not load {} ({reason}); moved it to {}", path.display(), backup_path.display());
                    State::default()
                }
            },
        };
        state.fix_up();
        Ok(state)
    }

    /// Writes atomically (temp file + rename) so a crash never leaves half a file.
    pub fn save(&self, path: &Path) -> std::io::Result<()> {
        if let Some(dir) = path.parent() {
            std::fs::create_dir_all(dir)?;
        }
        let json = serde_json::to_vec_pretty(self)?;
        let mut tmp = create_unique_temp(path)?;
        tmp.file_mut().write_all(&json)?;
        tmp.file_mut().flush()?;
        tmp.file_mut().sync_all()?;
        tmp.close();
        std::fs::rename(tmp.path(), path)?;
        tmp.persisted();
        sync_parent(path)
    }

    /// Keeps the `--run` names only of titles a note still has and no encrypted note has: they
    /// reach the file for a plain or hidden note, whatever renames, closing or encrypting did.
    pub fn prune_run_names(&mut self) {
        let notes = &self.notes;
        self.run_names.retain(|title, _| {
            let mut titled = notes.iter().filter(|n| n.title == *title).peekable();
            titled.peek().is_some() && !titled.any(Note::is_encrypted)
        });
    }

    pub fn fix_up(&mut self) {
        if self.notes.is_empty() {
            self.notes.push(Note::new("Scratchpad 1".into()));
        }
        self.active = self.active.min(self.notes.len() - 1);
        // An encrypted note without a password is locked: whatever text it came with goes.
        for note in self.notes.iter_mut().filter(|n| n.is_locked()) {
            note.text.clear();
        }
        // Give every note a unique non-zero id (older files and new notes start at 0).
        let mut seen = std::collections::HashSet::new();
        for i in 0..self.notes.len() {
            if self.notes[i].id == 0 || !seen.insert(self.notes[i].id) {
                self.notes[i].id = self.fresh_id();
                seen.insert(self.notes[i].id);
            }
        }
        self.font_size = self.font_size.clamp(10.0, 32.0);
        // A size from a broken file or a huge tiled window must not come back as the float size.
        if self.window_size.is_some_and(|[w, h]| !(200.0..=4000.0).contains(&w) || !(150.0..=4000.0).contains(&h)) {
            self.window_size = None;
        }
    }

    pub fn fresh_id(&mut self) -> u64 {
        let id = self.next_id.max(self.notes.iter().map(|n| n.id).max().unwrap_or(0) + 1);
        self.next_id = id + 1;
        id
    }

    /// "Scratchpad N" with the smallest N not already used.
    pub fn next_title(&self) -> String {
        (1..).map(|n| format!("Scratchpad {n}")).find(|t| !self.notes.iter().any(|n| &n.title == t)).unwrap()
    }

    pub fn add_note(&mut self) {
        let mut note = Note::new(self.next_title());
        note.id = self.fresh_id();
        self.notes.push(note);
        self.active = self.notes.len() - 1;
    }

    pub fn close_note(&mut self, i: usize) {
        if i >= self.notes.len() {
            return;
        }
        self.notes.remove(i);
        if self.notes.is_empty() {
            self.notes.push(Note::new("Scratchpad 1".into()));
            self.fix_up();
        }
        if self.active > i || self.active >= self.notes.len() {
            self.active = self.active.saturating_sub(1);
        }
    }

    /// Moves note `from` to index `to` (tab dragging); the active note stays the active one.
    pub fn move_note(&mut self, from: usize, to: usize) {
        if from == to || from >= self.notes.len() || to >= self.notes.len() {
            return;
        }
        let active = self.notes[self.active].id;
        let note = self.notes.remove(from);
        self.notes.insert(to, note);
        self.active = self.notes.iter().position(|n| n.id == active).unwrap_or(0);
    }

    pub fn current(&self) -> &Note {
        &self.notes[self.active]
    }

    pub fn current_mut(&mut self) -> &mut Note {
        &mut self.notes[self.active]
    }
}

pub(crate) fn next_backup_path(path: &Path) -> PathBuf {
    for suffix in 0_u64.. {
        let ending = if suffix == 0 { ".bak".to_owned() } else { format!(".bak.{suffix}") };
        let mut name = path.file_name().unwrap_or_default().to_os_string();
        name.push(ending);
        let candidate = path.with_file_name(name);
        if !candidate.exists() {
            return candidate;
        }
    }
    unreachable!()
}

pub(crate) struct PendingTemp {
    path: Option<PathBuf>,
    file: Option<std::fs::File>,
}

impl PendingTemp {
    pub(crate) fn path(&self) -> &Path {
        self.path.as_deref().expect("pending temp has a path")
    }
    pub(crate) fn file_mut(&mut self) -> &mut std::fs::File {
        self.file.as_mut().expect("pending temp is open")
    }
    pub(crate) fn close(&mut self) {
        self.file = None;
    }
    pub(crate) fn persisted(&mut self) {
        self.path = None;
    }
}

impl Drop for PendingTemp {
    fn drop(&mut self) {
        if let Some(path) = &self.path {
            let _ = std::fs::remove_file(path);
        }
    }
}

/// A name for a temp file next to the file `name`, unused by this process before.
pub(crate) fn temp_name(name: &OsStr) -> OsString {
    let sequence = TEMP_FILE_SEQUENCE.fetch_add(1, Ordering::Relaxed);
    let mut name = name.to_os_string();
    name.push(format!(".tmp.{}.{sequence}", std::process::id()));
    name
}

pub(crate) fn create_unique_temp(path: &Path) -> std::io::Result<PendingTemp> {
    loop {
        let candidate = path.with_file_name(temp_name(path.file_name().unwrap_or_default()));
        let mut options = OpenOptions::new();
        options.write(true).create_new(true);
        #[cfg(unix)]
        {
            use std::os::unix::fs::OpenOptionsExt;
            options.mode(0o600);
        }
        match options.open(&candidate) {
            Ok(file) => return Ok(PendingTemp { path: Some(candidate), file: Some(file) }),
            Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => continue,
            Err(error) => return Err(error),
        }
    }
}

#[cfg(unix)]
pub(crate) fn sync_parent(path: &Path) -> std::io::Result<()> {
    match path.parent().filter(|parent| !parent.as_os_str().is_empty()) {
        Some(parent) => ignore_unsupported_sync(std::fs::File::open(parent).and_then(|dir| dir.sync_all())),
        None => Ok(()),
    }
}

/// The rename already happened: a file system that cannot fsync a directory (some FUSE and
/// network mounts) must not make every save look failed.
#[cfg(unix)]
pub(crate) fn ignore_unsupported_sync(result: std::io::Result<()>) -> std::io::Result<()> {
    match result {
        Err(e) if matches!(e.kind(), std::io::ErrorKind::Unsupported | std::io::ErrorKind::InvalidInput) => Ok(()),
        result => result,
    }
}

#[cfg(not(unix))]
pub(crate) fn sync_parent(_path: &Path) -> std::io::Result<()> {
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn test_dir(name: &str) -> PathBuf {
        let sequence = TEMP_FILE_SEQUENCE.fetch_add(1, Ordering::Relaxed);
        std::env::temp_dir().join(format!("scratchpad-{name}-{}-{sequence}", std::process::id()))
    }

    #[test]
    fn titles_fill_gaps_and_closing_keeps_one() {
        let mut s = State::default();
        s.add_note();
        s.add_note();
        assert_eq!(s.notes.iter().map(|n| n.title.as_str()).collect::<Vec<_>>(), ["Scratchpad 1", "Scratchpad 2", "Scratchpad 3"]);
        s.close_note(1);
        assert_eq!(s.active, 1);
        assert_eq!(s.next_title(), "Scratchpad 2");
        s.close_note(0);
        s.close_note(0);
        assert_eq!(s.notes.len(), 1);
        assert_eq!(s.active, 0);
    }

    #[test]
    fn moving_a_note_keeps_the_active_one() {
        let mut s = State::default();
        s.add_note();
        s.add_note();
        s.active = 1;
        s.move_note(0, 2);
        assert_eq!(s.notes.iter().map(|n| n.title.as_str()).collect::<Vec<_>>(), ["Scratchpad 2", "Scratchpad 3", "Scratchpad 1"]);
        assert_eq!(s.current().title, "Scratchpad 2");
        s.move_note(0, 1);
        assert_eq!(s.current().title, "Scratchpad 2");
        assert_eq!(s.active, 1);
        s.move_note(1, 7);
        assert_eq!(s.active, 1);
    }

    #[test]
    fn ids_are_unique_and_corrupt_files_are_kept() {
        let mut s = State::default();
        s.add_note();
        s.add_note();
        let ids: std::collections::HashSet<_> = s.notes.iter().map(|n| n.id).collect();
        assert_eq!(ids.len(), 3);
        assert!(!ids.contains(&0));

        let dir = std::env::temp_dir().join(format!("scratchpad-corrupt-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("notes.json");
        std::fs::write(&path, "{ not json").unwrap();
        assert_eq!(State::load(&path).unwrap().notes.len(), 1);
        assert_eq!(std::fs::read_to_string(dir.join("notes.json.bak")).unwrap(), "{ not json");
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn round_trips_through_json() {
        let dir = std::env::temp_dir().join(format!("scratchpad-store-{}", std::process::id()));
        let path = dir.join("notes.json");
        let mut s = State::default();
        s.current_mut().text = "sk-secret".into();
        s.current_mut().hidden = true;
        s.save(&path).unwrap();
        assert_eq!(State::load(&path).unwrap(), s);
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn encrypted_notes_never_write_plaintext() {
        let dir = std::env::temp_dir().join(format!("scratchpad-crypt-{}", std::process::id()));
        let path = dir.join("notes.json");
        let mut s = State::default();
        s.current_mut().text = "the launch codes".into();
        s.current_mut().encrypt("hunter2").unwrap();
        assert!(!s.current().is_locked());
        s.current_mut().text.push_str(" v2");
        s.current_mut().reseal().unwrap();
        s.save(&path).unwrap();
        let json = std::fs::read_to_string(&path).unwrap();
        assert!(!json.contains("launch codes") && !json.contains("hunter2"), "{json}");
        assert!(json.contains("\"cipher\""));

        let mut loaded = State::load(&path).unwrap();
        let note = loaded.current_mut();
        assert!(note.is_locked() && note.text.is_empty());
        assert_eq!(note.unlock("wrong"), Err(crypto::Error::DecryptFailed));
        assert!(note.is_locked());
        note.unlock("hunter2").unwrap();
        assert_eq!(note.text, "the launch codes v2");
        note.lock();
        assert!(note.is_locked() && note.text.is_empty());
        note.remove_encryption();
        assert!(note.is_encrypted(), "a locked note keeps its cipher");
        note.unlock("hunter2").unwrap();
        note.remove_encryption();
        assert!(!note.is_encrypted() && note.password.is_none());
        assert_eq!(note.text, "the launch codes v2");
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn old_files_load_and_stray_plaintext_is_dropped() {
        let s: State =
            serde_json::from_str(r#"{"notes":[{"title":"a","text":"plain"},{"title":"b","text":"leak","cipher":"x"}]}"#).unwrap();
        let mut s = s;
        s.fix_up();
        assert_eq!(s.notes[0].text, "plain");
        assert!(!s.notes[0].is_encrypted());
        assert!(s.notes[1].is_locked() && s.notes[1].text.is_empty());
        // Options added later default to on, except reading by scripts.
        assert!(s.auto_lock && s.auto_rehide && s.warnings && s.script_reading == ReadMode::Off);
        assert!(s.run_names.is_empty(), "a file from before remembers no --run names");
        let s: State = serde_json::from_str(r#"{"auto_lock":false}"#).unwrap();
        assert!(!s.auto_lock && s.auto_rehide);
        let s: State = serde_json::from_str(r#"{"auto_lock":false,"auto_rehide":false}"#).unwrap();
        assert!(s.warnings, "a file from before the option loads with warnings on");
        let s: State = serde_json::from_str(r#"{"warnings":false}"#).unwrap();
        assert!(!s.warnings);
        let s: State = serde_json::from_str(r#"{"script_reading":"ask"}"#).unwrap();
        assert_eq!(s.script_reading, ReadMode::Ask);
        let s: State = serde_json::from_str(&serde_json::to_string(&s).unwrap()).unwrap();
        assert_eq!(s.script_reading, ReadMode::Ask);
        let s: State = serde_json::from_str(r#"{"script_reading":"bogus"}"#).unwrap();
        assert_eq!(s.script_reading, ReadMode::Off);
    }

    #[test]
    fn corrupt_files_use_new_backup_names() {
        let dir = test_dir("backups");
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("notes.json");
        std::fs::write(dir.join("notes.json.bak"), "older backup").unwrap();
        std::fs::write(&path, "first corrupt file").unwrap();
        State::load(&path).unwrap();
        std::fs::write(&path, "second corrupt file").unwrap();
        State::load(&path).unwrap();
        assert_eq!(std::fs::read_to_string(dir.join("notes.json.bak")).unwrap(), "older backup");
        assert_eq!(std::fs::read_to_string(dir.join("notes.json.bak.1")).unwrap(), "first corrupt file");
        assert_eq!(std::fs::read_to_string(dir.join("notes.json.bak.2")).unwrap(), "second corrupt file");
        let _ = std::fs::remove_dir_all(dir);
    }

    #[cfg(unix)]
    #[test]
    fn corrupt_file_is_untouched_when_backup_fails() {
        use std::os::unix::fs::PermissionsExt;
        let dir = test_dir("backup-failure");
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("notes.json");
        std::fs::write(&path, "corrupt but important").unwrap();
        std::fs::set_permissions(&dir, std::fs::Permissions::from_mode(0o500)).unwrap();
        let error = State::load(&path).unwrap_err();
        std::fs::set_permissions(&dir, std::fs::Permissions::from_mode(0o700)).unwrap();
        assert!(error.to_string().contains("could not move it"));
        assert_eq!(std::fs::read_to_string(&path).unwrap(), "corrupt but important");
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn save_uses_unique_temp_and_cleans_it_after_failure() {
        let dir = test_dir("temp");
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("notes.json");
        let predictable = dir.join("notes.json.tmp");
        std::fs::write(&predictable, "leave this alone").unwrap();
        State::default().save(&path).unwrap();
        assert_eq!(std::fs::read_to_string(&predictable).unwrap(), "leave this alone");

        std::fs::remove_file(&path).unwrap();
        std::fs::create_dir(&path).unwrap();
        assert!(State::default().save(&path).is_err());
        let leftovers = std::fs::read_dir(&dir)
            .unwrap()
            .filter_map(Result::ok)
            .filter(|entry| entry.file_name().to_string_lossy().contains(".tmp."))
            .count();
        assert_eq!(leftovers, 0);
        let _ = std::fs::remove_dir_all(dir);
    }

    #[cfg(unix)]
    #[test]
    fn unsupported_directory_sync_is_not_a_save_failure() {
        use std::io::{Error, ErrorKind};
        assert!(ignore_unsupported_sync(Err(Error::from(ErrorKind::Unsupported))).is_ok());
        assert!(ignore_unsupported_sync(Err(Error::from(ErrorKind::InvalidInput))).is_ok());
        assert!(ignore_unsupported_sync(Err(Error::from(ErrorKind::PermissionDenied))).is_err());
    }

    #[cfg(unix)]
    #[test]
    fn saved_files_are_private() {
        use std::os::unix::fs::PermissionsExt;
        let dir = test_dir("private");
        let path = dir.join("notes.json");
        State::default().save(&path).unwrap();
        assert_eq!(std::fs::metadata(&path).unwrap().permissions().mode() & 0o777, 0o600);
        let _ = std::fs::remove_dir_all(dir);
    }
}
