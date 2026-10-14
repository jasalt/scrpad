//! `scrpad --install-agent` and `--uninstall-agent`: put the agent skill (and, for Codex, the
//! hook that keeps `.env` files from the agent) where an agent finds them, and take them away.
//! Running the install again is the upgrade. A file that is not scrpad's is moved aside, never
//! overwritten, and hooks.json keeps every entry that is not ours. Every directory is a
//! parameter (`Dirs`), so the tests never touch the real home or `~/.codex`.

use std::io::Write as _;
use std::path::{Path, PathBuf};

use serde_json::{Value, json};
use sha2::{Digest, Sha256};

use crate::cli::AgentTarget;
use crate::store::{create_unique_temp, next_backup_path, sync_parent};

/// The skill scrpad writes, with a marker. Codex and Gemini CLI both read `~/.agents/skills`.
const SKILL: &str = crate::cli::SKILL;
/// The `.env` guard (`plugin/hooks`), the same files the Claude Code plugin runs.
const HOOK_SH: &str = include_str!("../plugin/hooks/handoff-files.sh");
const HOOK_JQ: &str = include_str!("../plugin/hooks/handoff-files.jq");

// Files are told apart by a digest of their body (the marker lines out, line ends normalised),
// not by comparing text, so rewording a file now does not make the copies from older scrpads
// look like the user's own. Each list holds the digest of every version ever shipped; when you
// change a file, add its new digest here (the test `the_current_files_are_known` prints it).
// The SKILL.md ones also cover files made with `scrpad --skill >`, which have no marker.
const SKILL_DIGESTS: &[&str] = &[
    "35158bffb03f8608826a0cf8c74b3ce6c11b757a31a7741e2e55be61c73229da",
    "560fae66d7e0a016ac990460ef0ad2697f4b2e5b10fb9958d62781c7bc912371",
];
const SH_DIGESTS: &[&str] = &["7fddac538679daba80d4ccfe3eb6a1ada7faa45c43237ed19fdc0126d3a3ae21"];
const JQ_DIGESTS: &[&str] = &["b5ce7674c323114a0f83a50a2da8c80d8cca84e696217f32528c03b26ae2d00e"];

/// What the hook command in hooks.json contains, whoever wrote the rest of the command.
const HOOK_SCRIPT: &str = "scrpad/handoff-files.sh";
/// Where the marker starts, in SKILL.md and in the hook files.
const MARKER_START: [&str; 2] = ["<!-- scrpad-owned", "# scrpad-owned"];
/// The most of a file `inspect` reads: a bigger one is not ours.
const MAX_FILE: u64 = 1 << 20;

/// The two places the files go, and how to tell which programs are installed.
#[derive(Clone, Debug)]
pub struct Dirs {
    pub home: PathBuf,
    /// `$CODEX_HOME`, `~/.codex` when unset. Always absolute: hooks do not expand `~`.
    pub codex_home: PathBuf,
    /// Whether a program is on PATH; a parameter so the tests do not depend on the machine.
    pub have: fn(&str) -> bool,
}

impl Dirs {
    pub fn from_env() -> Result<Self, String> {
        let home = dirs::home_dir().ok_or("could not find your home folder")?;
        let codex_home = match std::env::var_os("CODEX_HOME").filter(|v| !v.is_empty()) {
            Some(dir) => std::path::absolute(dir).map_err(|e| format!("CODEX_HOME: {e}"))?,
            None => home.join(".codex"),
        };
        Ok(Self { home, codex_home, have: on_path })
    }

    fn skill(&self) -> PathBuf {
        self.home.join(".agents/skills/scrpad/SKILL.md")
    }

    fn hook_dir(&self) -> PathBuf {
        self.codex_home.join("scrpad")
    }

    fn hooks_json(&self) -> PathBuf {
        self.codex_home.join("hooks.json")
    }
}

/// The command line: prints each step, errors to stderr; the exit code.
pub fn run(target: AgentTarget, uninstall: bool, dry_run: bool) -> i32 {
    let result = Dirs::from_env().and_then(|dirs| {
        apply(&dirs, target, uninstall, dry_run, &mut |line| {
            // `| head`: a closed pipe is no reason to stop half way.
            let _ =
                if line.starts_with("warning:") { writeln!(std::io::stderr(), "{line}") } else { writeln!(std::io::stdout(), "{line}") };
        })
    });
    match result {
        Ok(()) => 0,
        Err(error) => {
            eprintln!("scrpad: {error}");
            1
        }
    }
}

/// Installs or removes what `target` needs, saying each step through `say` as it happens: a
/// file moved aside is told at once, as a later step may fail.
pub fn apply(dirs: &Dirs, target: AgentTarget, uninstall: bool, dry_run: bool, say: &mut dyn FnMut(&str)) -> Result<(), String> {
    let mut job = Job { dry_run, say, touched: false, hooked: false };
    let codex = target == AgentTarget::Codex;
    let result = match (target, uninstall) {
        (AgentTarget::Claude, _) => {
            (job.say)("Claude Code gets the skill and the .env guard from the plugin:");
            (job.say)(if uninstall { "  /plugin uninstall scrpad" } else { "  /plugin install scrpad --marketplace mhavo/scrpad" });
            return Ok(());
        }
        (_, false) => job.install(dirs, target),
        (_, true) => job.uninstall(dirs, codex),
    };
    result?;
    if dry_run {
        (job.say)("Dry run: nothing was changed.");
    } else if uninstall && !job.touched {
        (job.say)("Nothing to remove: scrpad had put nothing there.");
    } else if job.hooked {
        (job.say)("Restart Codex, then trust the new hook in /hooks: Codex skips a hook until you do.");
    }
    Ok(())
}

/// What a file at some path is, to us.
enum Found {
    Missing,
    /// scrpad's, by its body's digest; the bytes it holds.
    Ours(Vec<u8>),
    /// The user's, edited by the user, a link, or too big to be ours.
    Foreign,
}

struct Job<'a> {
    dry_run: bool,
    say: &'a mut dyn FnMut(&str),
    /// Something was found to remove or leave.
    touched: bool,
    /// The Codex hook is in place (installed, updated or already there).
    hooked: bool,
}

impl Job<'_> {
    /// `did` in a real run, `would` in a dry one.
    fn tell(&mut self, would: &str, did: &str, what: &dyn std::fmt::Display) {
        self.touched = true;
        let verb = if self.dry_run { would } else { did };
        (self.say)(&format!("{verb} {what}"));
    }

    fn install(&mut self, dirs: &Dirs, target: AgentTarget) -> Result<(), String> {
        let name = target.name();
        self.put(&dirs.skill(), &with_marker(SKILL, name, Place::AfterFrontMatter), SKILL_DIGESTS)?;
        if target != AgentTarget::Codex {
            return Ok(());
        }
        let missing: Vec<&str> = ["sh", "jq"].into_iter().filter(|tool| !(dirs.have)(tool)).collect();
        if !missing.is_empty() {
            let they = if missing.len() == 1 { "it" } else { "them" };
            let what = missing.join(" and ");
            let head = if self.dry_run { "would not install the .env guard hook" } else { "the .env guard hook was not installed" };
            (self.say)(&format!("warning: {head}: it needs {what} on PATH. Install {they} and run scrpad --install-agent codex again."));
            return Ok(());
        }
        let hook_dir = dirs.hook_dir();
        self.put(&hook_dir.join("handoff-files.sh"), &with_marker(HOOK_SH, name, Place::AfterFirstLine), SH_DIGESTS)?;
        self.put(&hook_dir.join("handoff-files.jq"), &with_marker(HOOK_JQ, name, Place::First), JQ_DIGESTS)?;
        self.add_hook(&dirs.hooks_json(), &hook_dir.join("handoff-files.sh"))?;
        self.hooked = true;
        Ok(())
    }

    fn uninstall(&mut self, dirs: &Dirs, codex: bool) -> Result<(), String> {
        if codex {
            self.remove_hook(&dirs.hooks_json())?;
            let hook_dir = dirs.hook_dir();
            self.take(&hook_dir.join("handoff-files.sh"), SH_DIGESTS)?;
            self.take(&hook_dir.join("handoff-files.jq"), JQ_DIGESTS)?;
            self.remove_empty(&hook_dir);
        }
        let skill = dirs.skill();
        self.take(&skill, SKILL_DIGESTS)?;
        if let Some(dir) = skill.parent() {
            self.remove_empty(dir);
        }
        Ok(())
    }

    /// Makes `path` hold `content`: the file is created, updated if it is scrpad's, or moved
    /// aside first if it is anyone else's.
    fn put(&mut self, path: &Path, content: &str, known: &[&str]) -> Result<(), String> {
        match inspect(path, known)? {
            Found::Missing => {}
            Found::Ours(bytes) if bytes == content.as_bytes() => {
                self.touched = true;
                (self.say)(&format!("{} already up to date", path.display()));
                return Ok(());
            }
            Found::Ours(_) => {
                self.write(path, content.as_bytes())?;
                self.tell("would update", "updated", &path.display());
                return Ok(());
            }
            Found::Foreign => {
                let backup = next_backup_path(path);
                if self.dry_run {
                    (self.say)(&format!("would move your {} to {}", path.display(), backup.display()));
                } else {
                    std::fs::rename(path, &backup).map_err(|e| format!("could not move {} aside: {e}", path.display()))?;
                    (self.say)(&format!("Your {} was moved to {}", path.display(), backup.display()));
                }
            }
        }
        self.write(path, content.as_bytes())?;
        self.tell("would create", "created", &path.display());
        Ok(())
    }

    fn write(&self, path: &Path, bytes: &[u8]) -> Result<(), String> {
        if self.dry_run { Ok(()) } else { write_atomic(path, bytes) }
    }

    /// Removes `path` only if it is scrpad's.
    fn take(&mut self, path: &Path, known: &[&str]) -> Result<(), String> {
        match inspect(path, known)? {
            Found::Missing => {}
            Found::Foreign => {
                self.touched = true;
                (self.say)(&format!("left {}: changed since scrpad wrote it", path.display()));
            }
            Found::Ours(_) => {
                if !self.dry_run {
                    std::fs::remove_file(path).map_err(|e| format!("could not remove {}: {e}", path.display()))?;
                }
                self.tell("would remove", "removed", &path.display());
            }
        }
        Ok(())
    }

    /// Never a recursive delete: a folder with anything left in it stays.
    fn remove_empty(&mut self, dir: &Path) {
        if !self.dry_run && std::fs::remove_dir(dir).is_ok() {
            (self.say)(&format!("removed {}", dir.display()));
        }
    }

    fn add_hook(&mut self, hooks_json: &Path, script: &Path) -> Result<(), String> {
        let Some(command) = script.to_str().and_then(|s| hook_command(s, cfg!(windows))) else {
            return Err(format!("{} has a character a hook command cannot hold; set CODEX_HOME to another folder", script.display()));
        };
        let wanted = json!({ "matcher": "Bash|apply_patch", "hooks": [{ "type": "command", "command": command }] });
        let existing = load_hooks(hooks_json)?;
        let existed = existing.is_some();
        let mut root = existing.unwrap_or_else(|| json!({}));
        let hooks = child(&mut root, "hooks", json!({}), "an object", Value::as_object_mut)?;
        let groups = child_of(hooks, "PreToolUse", json!([]), "an array", Value::as_array_mut)?;
        let (did, would) = if groups.iter().any(has_ours) {
            ("updated the .env guard in", "would update the .env guard in")
        } else {
            ("added the .env guard to", "would add the .env guard to")
        };
        let rebuilt = rebuild(groups, Some(&wanted));
        if rebuilt == *groups {
            self.touched = true;
            (self.say)(&format!("the .env guard in {} is already present", hooks_json.display()));
            return Ok(());
        }
        *groups = rebuilt;
        self.save_hooks(hooks_json, &root, existed)?;
        self.tell(would, did, &hooks_json.display());
        Ok(())
    }

    fn remove_hook(&mut self, hooks_json: &Path) -> Result<(), String> {
        let Some(mut root) = load_hooks(hooks_json)? else { return Ok(()) };
        let Some(hooks) = root.get_mut("hooks").and_then(Value::as_object_mut) else { return Ok(()) };
        let Some(groups) = hooks.get_mut("PreToolUse").and_then(Value::as_array_mut) else { return Ok(()) };
        let rebuilt = rebuild(groups, None);
        if rebuilt == *groups {
            return Ok(());
        }
        *groups = rebuilt;
        // Only the keys we emptied go; an empty `PreToolUse` the user had stays.
        if groups.is_empty() {
            hooks.remove("PreToolUse");
        }
        if hooks.is_empty() {
            root.as_object_mut().expect("loaded as an object").remove("hooks");
        }
        self.save_hooks(hooks_json, &root, true)?;
        self.tell("would remove the .env guard from", "removed the .env guard from", &hooks_json.display());
        Ok(())
    }

    /// Writes hooks.json after copying the old one to `hooks.json.scrpad.bak` (a name only
    /// scrpad uses, over the last one: rtk keeps `hooks.json.bak`).
    fn save_hooks(&mut self, path: &Path, root: &Value, existed: bool) -> Result<(), String> {
        // A link (dotfiles) keeps pointing at its file.
        let real = std::fs::canonicalize(path).unwrap_or_else(|_| path.to_owned());
        if existed {
            let mut name = real.file_name().unwrap_or_default().to_os_string();
            name.push(".scrpad.bak");
            let backup = real.with_file_name(name);
            if self.dry_run {
                (self.say)(&format!("would back up {} to {}", path.display(), backup.display()));
            } else {
                std::fs::copy(&real, &backup).map_err(|e| format!("could not back up {}: {e}", path.display()))?;
                (self.say)(&format!("backed up {} to {}", path.display(), backup.display()));
            }
        }
        let mut text = serde_json::to_string_pretty(root).map_err(|e| e.to_string())?;
        text.push('\n');
        self.write(&real, text.as_bytes())
    }
}

/// hooks.json as an object, or `None` if there is none. Anything else is an error that
/// leaves the file alone.
fn load_hooks(path: &Path) -> Result<Option<Value>, String> {
    // Writing would put a plain file where the user's link is.
    if std::fs::symlink_metadata(path).is_ok_and(|m| m.file_type().is_symlink()) && std::fs::metadata(path).is_err() {
        return Err(format!("{} is a link to nothing; left it as it was", path.display()));
    }
    let text = match std::fs::read_to_string(path) {
        Ok(text) => text,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(e) => return Err(format!("could not read {}: {e}", path.display())),
    };
    match serde_json::from_str::<Value>(&text) {
        Ok(root) if root.is_object() => Ok(Some(root)),
        Ok(_) => Err(format!("{} is not a JSON object; left it as it was", path.display())),
        Err(e) => Err(format!("{} is not valid JSON ({e}); left it as it was", path.display())),
    }
}

/// `root[key]`, made with `default` if missing, as the kind `get` says; an error if it is another.
fn child<'a, T: 'a>(
    root: &'a mut Value,
    key: &str,
    default: Value,
    kind: &str,
    get: fn(&mut Value) -> Option<&mut T>,
) -> Result<&'a mut T, String> {
    let object = root.as_object_mut().expect("loaded as an object");
    let value = object.entry(key).or_insert(default);
    get(value).ok_or_else(|| format!("“{key}” in hooks.json is not {kind}; left it as it was"))
}

fn child_of<'a, T: 'a>(
    object: &'a mut serde_json::Map<String, Value>,
    key: &str,
    default: Value,
    kind: &str,
    get: fn(&mut Value) -> Option<&mut T>,
) -> Result<&'a mut T, String> {
    let value = object.entry(key).or_insert(default);
    get(value).ok_or_else(|| format!("“{key}” in hooks.json is not {kind}; left it as it was"))
}

fn is_scrpad_hook(hook: &Value) -> bool {
    hook["command"].as_str().is_some_and(|c| c.contains(HOOK_SCRIPT))
}

/// A PreToolUse group made of our hook alone: ours to replace or drop.
fn is_ours(group: &Value) -> bool {
    group["hooks"].as_array().is_some_and(|h| !h.is_empty() && h.iter().all(is_scrpad_hook))
}

/// A group with our hook in it, maybe next to others' (a hand-made `"matcher": "Bash"` group).
fn has_ours(group: &Value) -> bool {
    group["hooks"].as_array().is_some_and(|h| h.iter().any(is_scrpad_hook))
}

/// `groups` without our hook: groups that are only ours go, in a mixed group just our hook
/// object, and a group that empties goes. With `wanted`, it takes the place of the first group
/// that was wholly ours (the end if there was none), so there is exactly one of ours.
fn rebuild(groups: &[Value], wanted: Option<&Value>) -> Vec<Value> {
    let mut out = Vec::with_capacity(groups.len() + 1);
    let mut placed = wanted.is_none();
    let mut place = |out: &mut Vec<Value>| {
        if !placed {
            placed = true;
            out.extend(wanted.cloned());
        }
    };
    for group in groups {
        if is_ours(group) {
            place(&mut out);
        } else if has_ours(group) {
            let mut rest = group.clone();
            if let Some(hooks) = rest["hooks"].as_array_mut() {
                hooks.retain(|h| !is_scrpad_hook(h));
            }
            if rest["hooks"].as_array().is_some_and(|h| !h.is_empty()) {
                out.push(rest);
            }
        } else {
            out.push(group.clone());
        }
    }
    place(&mut out);
    out
}

/// Which file the path holds: ours by digest, not found, or foreign.
fn inspect(path: &Path, known: &[&str]) -> Result<Found, String> {
    let meta = match std::fs::symlink_metadata(path) {
        Ok(meta) => meta,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(Found::Missing),
        Err(e) => return Err(format!("{}: {e}", path.display())),
    };
    // A link is never followed: it could point anywhere.
    if meta.file_type().is_symlink() {
        return Ok(Found::Foreign);
    }
    if !meta.is_file() {
        return Err(format!("{} is not a file", path.display()));
    }
    if meta.len() > MAX_FILE {
        return Ok(Found::Foreign);
    }
    let bytes = std::fs::read(path).map_err(|e| format!("could not read {}: {e}", path.display()))?;
    Ok(if known.contains(&body_digest(&bytes).as_str()) { Found::Ours(bytes) } else { Found::Foreign })
}

/// SHA-256 of a file without its marker lines, its byte order mark and its `\r`s, in hex: what
/// stays the same when scrpad marks a file, or Windows changes its line ends.
fn body_digest(bytes: &[u8]) -> String {
    let bytes = bytes.strip_prefix(b"\xEF\xBB\xBF").unwrap_or(bytes);
    let mut hasher = Sha256::new();
    for line in bytes.split_inclusive(|&b| b == b'\n') {
        if !MARKER_START.iter().any(|m| line.starts_with(m.as_bytes())) {
            hasher.update(line.iter().copied().filter(|&b| b != b'\r').collect::<Vec<_>>());
        }
    }
    hasher.finalize().iter().map(|b| format!("{b:02x}")).collect()
}

/// Where the marker goes in a file.
#[derive(Clone, Copy)]
enum Place {
    /// SKILL.md: right after the closing `---` of the front matter, which must stay first.
    AfterFrontMatter,
    /// A shell script: after the shebang.
    AfterFirstLine,
    First,
}

fn marker(target: &str, comment: &str) -> String {
    format!(
        "{comment}scrpad-owned: written by scrpad {} --install-agent {target}; removed by scrpad --uninstall-agent {target}{}\n",
        env!("CARGO_PKG_VERSION"),
        if comment.starts_with('<') { " -->" } else { "" }
    )
}

/// `text` with the marker in its place, which is where `body_digest` takes it out again.
fn with_marker(text: &str, target: &str, place: Place) -> String {
    let (at, comment) = match place {
        Place::First => (0, "# "),
        Place::AfterFirstLine => (text.find('\n').map_or(text.len(), |i| i + 1), "# "),
        Place::AfterFrontMatter => {
            let close = text.strip_prefix("---\n").and_then(|rest| rest.find("\n---\n")).expect("the skill has front matter");
            ("---\n".len() + close + "\n---\n".len(), "<!-- ")
        }
    };
    format!("{}{}{}", &text[..at], marker(target, comment), &text[at..])
}

/// A temp file next to `path`, then a rename, as the notes and `--write` are saved; the folders
/// above are made. Not a secret, so the file is world-readable like any other.
fn write_atomic(path: &Path, bytes: &[u8]) -> Result<(), String> {
    let fail = |e: std::io::Error| format!("could not write {}: {e}", path.display());
    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir).map_err(fail)?;
    }
    let mut tmp = create_unique_temp(path).map_err(fail)?;
    tmp.file_mut().write_all(bytes).map_err(fail)?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        // A file that was there keeps its mode (a private hooks.json stays private).
        let mode = std::fs::metadata(path).map_or(0o644, |m| m.permissions().mode() & 0o7777);
        tmp.file_mut().set_permissions(std::fs::Permissions::from_mode(mode)).map_err(fail)?;
    }
    tmp.file_mut().sync_all().map_err(fail)?;
    tmp.close();
    std::fs::rename(tmp.path(), path).map_err(fail)?;
    tmp.persisted();
    sync_parent(path).map_err(fail)
}

/// The hook command for the script at `script`, or `None` if the path has a character the
/// command cannot hold. On Windows `\` becomes `/`, which sh (Git Bash, MSYS2) reads and cmd
/// and PowerShell pass on untouched.
fn hook_command(script: &str, windows: bool) -> Option<String> {
    let path = if windows { script.replace('\\', "/") } else { script.to_owned() };
    (!path.contains(['"', '$', '`', '\\'])).then(|| format!("sh \"{path}\""))
}

fn on_path(program: &str) -> bool {
    let Some(path) = std::env::var_os("PATH") else { return false };
    // On Windows a program is found by any of the PATHEXT extensions (.exe, .cmd, ...).
    let extensions: Vec<String> = if cfg!(windows) {
        let pathext = std::env::var("PATHEXT").unwrap_or_default();
        let listed = pathext.split(';').filter(|e| e.starts_with('.'));
        [".exe", ".cmd", ".bat"].into_iter().chain(listed).map(str::to_lowercase).collect()
    } else {
        Vec::new()
    };
    std::env::split_paths(&path)
        .any(|dir| dir.join(program).is_file() || extensions.iter().any(|e| dir.join(format!("{program}{e}")).is_file()))
}

/// A script command's one-line warning when the installed skill is from an older scrpad.
/// Never changes what the command prints or its exit code.
pub fn warn_if_stale() {
    if let Some(dirs) = dirs::home_dir()
        && let Some(warning) = stale_warning(&dirs.join(".agents/skills/scrpad/SKILL.md"))
    {
        eprintln!("{warning}");
    }
}

/// The warning for the skill at `path`: it is scrpad's, marked or made by hand with
/// `scrpad --skill >`, and its text is not the current one. Missing, unreadable, somebody
/// else's and current files give none.
pub fn stale_warning(path: &Path) -> Option<String> {
    std::fs::symlink_metadata(path).ok().filter(|m| m.is_file() && m.len() <= MAX_FILE)?;
    let bytes = std::fs::read(path).ok()?;
    let digest = body_digest(&bytes);
    if !SKILL_DIGESTS.contains(&digest.as_str()) {
        return None;
    }
    let text = String::from_utf8_lossy(&bytes);
    // Only a changed skill is stale: the same text from an older scrpad still teaches the same.
    if digest == body_digest(SKILL.as_bytes()) {
        return None;
    }
    let target = text
        .lines()
        .find(|l| l.starts_with(MARKER_START[0]))
        .and_then(|l| l.split_once("--install-agent ")?.1.split(';').next())
        .filter(|t| matches!(*t, "codex" | "skill"))
        .unwrap_or("skill");
    Some(format!("scrpad: the agent skill in {} is from an older scrpad; update it with scrpad --install-agent {target}", path.display()))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicU64, Ordering};

    fn temp_dir(name: &str) -> PathBuf {
        static SEQUENCE: AtomicU64 = AtomicU64::new(0);
        let n = SEQUENCE.fetch_add(1, Ordering::Relaxed);
        let dir = std::env::temp_dir().join(format!("scrpad-agents-{name}-{}-{n}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::canonicalize(dir).unwrap()
    }

    /// A fake home with a fake `.codex`, and nothing in them.
    fn dirs(name: &str) -> Dirs {
        let home = temp_dir(name);
        Dirs { codex_home: home.join(".codex"), home, have: |_| true }
    }

    /// Like `dirs`, with only the named programs on PATH.
    fn dirs_with(name: &str, tools: fn(&str) -> bool) -> Dirs {
        Dirs { have: tools, ..dirs(name) }
    }

    fn run_apply(dirs: &Dirs, target: AgentTarget, uninstall: bool, dry_run: bool) -> Result<Vec<String>, String> {
        let mut lines = Vec::new();
        apply(dirs, target, uninstall, dry_run, &mut |l| lines.push(l.to_owned())).map(|()| lines)
    }

    fn install(dirs: &Dirs, target: AgentTarget) -> Vec<String> {
        run_apply(dirs, target, false, false).unwrap()
    }

    /// Every file under `dir` with its bytes and modification time, to compare before and after.
    fn snapshot(dir: &Path) -> Vec<(PathBuf, Vec<u8>, Option<std::time::SystemTime>)> {
        let mut found = Vec::new();
        let mut todo = vec![dir.to_owned()];
        while let Some(d) = todo.pop() {
            for entry in std::fs::read_dir(d).unwrap() {
                let path = entry.unwrap().path();
                if path.is_dir() {
                    found.push((path.clone(), Vec::new(), None));
                    todo.push(path);
                } else {
                    let mtime = std::fs::metadata(&path).unwrap().modified().ok();
                    found.push((path.clone(), std::fs::read(&path).unwrap(), mtime));
                }
            }
        }
        found.sort();
        found
    }

    fn read(path: &Path) -> String {
        std::fs::read_to_string(path).unwrap()
    }

    fn put(path: &Path, text: &str) {
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(path, text).unwrap();
    }

    /// The SKILL.md of 0.2, as `scrpad --skill >` wrote it: no marker, an older digest.
    const OLD_SKILL: &str = include_str!("../tests/data/SKILL-0.2.md");

    /// hooks.json with the shape rtk's has: two PreToolUse groups, SessionStart and Stop.
    const OTHERS: &str = r#"{
  "hooks": {
    "PreToolUse": [
      { "matcher": "Bash", "hooks": [{ "type": "command", "command": "rtk hook" }] },
      { "matcher": "mcp__.*", "hooks": [{ "type": "command", "command": "other-guard" }] }
    ],
    "SessionStart": [{ "hooks": [{ "type": "command", "command": "rtk start" }] }],
    "Stop": [{ "hooks": [{ "type": "command", "command": "rtk stop" }] }]
  }
}
"#;

    #[test]
    fn the_current_files_are_known() {
        for (name, text, known) in
            [("SKILL.md", SKILL, SKILL_DIGESTS), ("handoff-files.sh", HOOK_SH, SH_DIGESTS), ("handoff-files.jq", HOOK_JQ, JQ_DIGESTS)]
        {
            let digest = body_digest(text.as_bytes());
            assert!(known.contains(&digest.as_str()), "{name} changed: add \"{digest}\" to its digest list in src/agents.rs");
        }
    }

    #[test]
    fn a_marker_does_not_change_the_digest() {
        let plain = body_digest(SKILL.as_bytes());
        for target in ["codex", "skill"] {
            assert_eq!(body_digest(with_marker(SKILL, target, Place::AfterFrontMatter).as_bytes()), plain);
        }
        assert_eq!(body_digest(with_marker(HOOK_SH, "codex", Place::AfterFirstLine).as_bytes()), body_digest(HOOK_SH.as_bytes()));
        assert_eq!(body_digest(with_marker(HOOK_JQ, "codex", Place::First).as_bytes()), body_digest(HOOK_JQ.as_bytes()));
        // Windows line ends and a byte order mark are no change either.
        let windows = format!("\u{feff}{}", SKILL.replace('\n', "\r\n"));
        assert_eq!(body_digest(windows.as_bytes()), plain);
        let marked = with_marker(SKILL, "skill", Place::AfterFrontMatter);
        assert!(marked.starts_with("---\n") && marked.contains("---\n<!-- scrpad-owned: written by scrpad "), "front matter stays first");
        assert!(with_marker(HOOK_SH, "codex", Place::AfterFirstLine).starts_with("#!/bin/sh\n# scrpad-owned: "));
    }

    #[test]
    fn a_fresh_install_makes_the_files_and_a_second_run_changes_nothing() {
        let d = dirs("fresh");
        let first = install(&d, AgentTarget::Codex);
        for path in [d.skill(), d.hook_dir().join("handoff-files.sh"), d.hook_dir().join("handoff-files.jq"), d.hooks_json()] {
            assert!(path.is_file(), "{}", path.display());
        }
        assert!(read(&d.skill()).contains("scrpad-owned: written by scrpad"));
        assert!(first.last().unwrap().contains("trust the new hook in /hooks"), "{first:?}");
        let before = snapshot(&d.home);
        std::thread::sleep(std::time::Duration::from_millis(20));
        let second = install(&d, AgentTarget::Codex);
        assert_eq!(snapshot(&d.home), before, "bytes and mtimes");
        assert_eq!(second.iter().filter(|l| l.contains("already up to date")).count(), 3, "{second:?}");
        assert!(second.iter().any(|l| l.contains("already present")), "{second:?}");
        assert!(!d.codex_home.join("hooks.json.scrpad.bak").exists());
    }

    #[test]
    fn the_skill_target_writes_only_the_skill() {
        let d = dirs("skill");
        install(&d, AgentTarget::Skill);
        assert!(d.skill().is_file());
        assert!(!d.codex_home.exists());
    }

    #[test]
    fn an_older_or_hand_made_skill_is_upgraded_without_a_backup() {
        let d = dirs("old");
        put(&d.skill(), OLD_SKILL);
        let lines = install(&d, AgentTarget::Skill);
        assert!(lines[0].starts_with("updated"), "{lines:?}");
        assert!(read(&d.skill()).contains("scrpad-owned"));
        assert!(!d.skill().with_file_name("SKILL.md.bak").exists());
    }

    #[test]
    fn another_version_of_ours_is_updated() {
        let d = dirs("version");
        install(&d, AgentTarget::Skill);
        let marked = read(&d.skill()).replace(env!("CARGO_PKG_VERSION"), "0.0.1");
        put(&d.skill(), &marked);
        assert!(install(&d, AgentTarget::Skill)[0].starts_with("updated"));
        assert!(read(&d.skill()).contains(env!("CARGO_PKG_VERSION")));
    }

    #[test]
    fn a_file_that_is_not_ours_is_moved_aside_and_told_at_once() {
        let d = dirs("foreign");
        put(&d.skill(), "my own skill");
        let lines = install(&d, AgentTarget::Skill);
        assert_eq!(lines[0], format!("Your {} was moved to {}.bak", d.skill().display(), d.skill().display()));
        assert_eq!(read(&d.skill().with_file_name("SKILL.md.bak")), "my own skill");
        // A second foreign one does not overwrite the first backup.
        put(&d.skill(), "edited again");
        install(&d, AgentTarget::Skill);
        assert_eq!(read(&d.skill().with_file_name("SKILL.md.bak")), "my own skill");
        assert_eq!(read(&d.skill().with_file_name("SKILL.md.bak.1")), "edited again");
    }

    #[test]
    fn an_edited_jq_is_moved_aside() {
        let d = dirs("jq");
        install(&d, AgentTarget::Codex);
        let jq = d.hook_dir().join("handoff-files.jq");
        put(&jq, &format!("{}|my-secrets\n", read(&jq)));
        let lines = install(&d, AgentTarget::Codex);
        assert!(lines.iter().any(|l| l.starts_with("Your ") && l.contains("handoff-files.jq")), "{lines:?}");
        assert!(read(&jq.with_file_name("handoff-files.jq.bak")).contains("my-secrets"));
        assert!(!read(&jq).contains("my-secrets"));
    }

    #[cfg(unix)]
    #[test]
    fn a_link_is_not_followed() {
        let d = dirs("link");
        let target = d.home.join("elsewhere.md");
        put(&target, "somebody's file");
        std::fs::create_dir_all(d.skill().parent().unwrap()).unwrap();
        std::os::unix::fs::symlink(&target, d.skill()).unwrap();
        install(&d, AgentTarget::Skill);
        assert_eq!(read(&target), "somebody's file");
        assert!(std::fs::symlink_metadata(d.skill().with_file_name("SKILL.md.bak")).unwrap().file_type().is_symlink());
        assert!(read(&d.skill()).contains("scrpad-owned"));
    }

    #[cfg(unix)]
    #[test]
    fn other_hooks_stay_as_they_were() {
        let d = dirs("others");
        put(&d.hooks_json(), OTHERS);
        install(&d, AgentTarget::Codex);
        let after: Value = serde_json::from_str(&read(&d.hooks_json())).unwrap();
        let before: Value = serde_json::from_str(OTHERS).unwrap();
        let groups = after["hooks"]["PreToolUse"].as_array().unwrap();
        assert_eq!(groups.len(), 3);
        assert_eq!(&groups[..2], before["hooks"]["PreToolUse"].as_array().unwrap().as_slice(), "content and order");
        assert_eq!(groups[2]["matcher"], "Bash|apply_patch");
        assert_eq!(groups[2]["hooks"][0]["command"], format!("sh \"{}/scrpad/handoff-files.sh\"", d.codex_home.display()));
        for key in ["SessionStart", "Stop"] {
            assert_eq!(after["hooks"][key], before["hooks"][key]);
        }
        let keys: Vec<_> = after["hooks"].as_object().unwrap().keys().collect();
        assert_eq!(keys, ["PreToolUse", "SessionStart", "Stop"], "key order");
        assert_eq!(read(&d.codex_home.join("hooks.json.scrpad.bak")), OTHERS);
        assert!(read(&d.hooks_json()).ends_with("}\n"));
        // A changed command of ours is replaced in place, not added again.
        let edited = read(&d.hooks_json()).replace("Bash|apply_patch", "Bash");
        put(&d.hooks_json(), &edited);
        install(&d, AgentTarget::Codex);
        let again: Value = serde_json::from_str(&read(&d.hooks_json())).unwrap();
        assert_eq!(again, after);
    }

    #[test]
    fn a_hooks_json_that_is_not_an_object_is_left_alone() {
        for bad in ["{ not json", "[]", "{\"hooks\": 1}", "{\"hooks\": {\"PreToolUse\": {}}}"] {
            let d = dirs("bad");
            put(&d.hooks_json(), bad);
            let error = run_apply(&d, AgentTarget::Codex, false, false);
            assert!(error.unwrap_err().contains("left it as it was"), "{bad}");
            assert_eq!(read(&d.hooks_json()), bad);
            assert!(!d.codex_home.join("hooks.json.scrpad.bak").exists());
        }
    }

    #[test]
    fn a_dry_run_changes_nothing() {
        let d = dirs("dry");
        put(&d.hooks_json(), OTHERS);
        put(&d.skill(), "my own skill");
        let before = snapshot(&d.home);
        let lines = run_apply(&d, AgentTarget::Codex, false, true).unwrap();
        assert_eq!(snapshot(&d.home), before);
        assert_eq!(lines.last().unwrap(), "Dry run: nothing was changed.");
        assert!(lines.iter().any(|l| l.starts_with("would move your")), "{lines:?}");
        assert!(lines.iter().all(|l| !l.contains("Restart Codex")));
        // And on a real install, then an uninstall.
        install(&d, AgentTarget::Codex);
        let before = snapshot(&d.home);
        run_apply(&d, AgentTarget::Codex, true, true).unwrap();
        assert_eq!(snapshot(&d.home), before);
        // Nothing at all there: a dry run does not even make the folders.
        let empty = dirs("dry-empty");
        run_apply(&empty, AgentTarget::Codex, false, true).unwrap();
        assert!(snapshot(&empty.home).is_empty());
    }

    #[cfg(unix)]
    #[test]
    fn an_uninstall_takes_only_what_is_ours() {
        let d = dirs("uninstall");
        put(&d.hooks_json(), OTHERS);
        install(&d, AgentTarget::Codex);
        // The user edited the jq: it stays, and so does its folder.
        let jq = d.hook_dir().join("handoff-files.jq");
        put(&jq, "mine");
        let lines = run_apply(&d, AgentTarget::Codex, true, false).unwrap();
        assert!(lines.iter().any(|l| l.starts_with("left ") && l.ends_with("changed since scrpad wrote it")), "{lines:?}");
        assert_eq!(read(&jq), "mine");
        assert!(!d.skill().exists() && !d.skill().parent().unwrap().exists(), "an emptied folder goes");
        assert!(!d.hook_dir().join("handoff-files.sh").exists());
        let after: Value = serde_json::from_str(&read(&d.hooks_json())).unwrap();
        assert_eq!(after, serde_json::from_str::<Value>(OTHERS).unwrap());
        // With only ours in it, the keys we emptied go too.
        let d = dirs("uninstall-empty");
        install(&d, AgentTarget::Codex);
        run_apply(&d, AgentTarget::Codex, true, false).unwrap();
        assert_eq!(serde_json::from_str::<Value>(&read(&d.hooks_json())).unwrap(), json!({}));
        assert!(!d.hook_dir().exists());
        // Nothing to do says so.
        let lines = run_apply(&d, AgentTarget::Codex, true, false).unwrap();
        assert!(lines[0].starts_with("Nothing to remove"), "{lines:?}");
    }

    #[test]
    fn uninstalling_the_skill_leaves_the_hook() {
        let d = dirs("skill-only");
        install(&d, AgentTarget::Codex);
        run_apply(&d, AgentTarget::Skill, true, false).unwrap();
        assert!(!d.skill().exists());
        assert!(d.hook_dir().join("handoff-files.sh").exists());
    }

    #[test]
    fn claude_only_points_to_the_plugin() {
        let d = dirs("claude");
        let lines = run_apply(&d, AgentTarget::Claude, false, false).unwrap();
        assert!(lines.iter().any(|l| l.contains("/plugin install scrpad --marketplace mhavo/scrpad")));
        assert!(snapshot(&d.home).is_empty());
    }

    /// Runs the installed hook command as Codex runs it: `sh -c` on the command in hooks.json,
    /// the tool call as JSON on stdin. Exit 2 blocks.
    #[cfg(unix)]
    fn codex_hook(d: &Dirs, command: &str) -> i32 {
        let hooks: Value = serde_json::from_str(&read(&d.hooks_json())).unwrap();
        let hook = hooks["hooks"]["PreToolUse"].as_array().unwrap().iter().find(|g| is_ours(g)).unwrap()["hooks"][0]["command"]
            .as_str()
            .unwrap()
            .to_owned();
        let mut child = std::process::Command::new("sh")
            .args(["-c", &hook])
            .stdin(std::process::Stdio::piped())
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .spawn()
            .unwrap();
        let call = json!({ "tool_name": "Bash", "tool_input": { "command": command } });
        child.stdin.take().unwrap().write_all(call.to_string().as_bytes()).unwrap();
        child.wait().unwrap().code().unwrap()
    }

    #[cfg(unix)]
    #[test]
    fn the_installed_hook_guards_env_files_as_codex_runs_it() {
        if !on_path("jq") {
            return eprintln!("skipped: jq is not installed");
        }
        let d = dirs("run hook");
        install(&d, AgentTarget::Codex);
        assert_eq!(codex_hook(&d, "cat .env"), 2);
        assert_eq!(codex_hook(&d, "scrpad --read .env"), 2);
        assert_eq!(codex_hook(&d, "scrpad --run .env -- x"), 0);
        assert_eq!(codex_hook(&d, "ls"), 0);
    }

    #[test]
    fn a_stale_skill_gets_a_warning_and_a_current_or_foreign_one_does_not() {
        let d = dirs("stale");
        let path = d.skill();
        assert_eq!(stale_warning(&path), None, "missing");
        install(&d, AgentTarget::Codex);
        assert_eq!(stale_warning(&path), None, "current");
        // The same text from another version teaches the same: no nagging after a release.
        put(&path, &read(&path).replace(env!("CARGO_PKG_VERSION"), "0.0.1"));
        assert_eq!(stale_warning(&path), None, "same text, older marker");
        // An older text, marked by the scrpad that wrote it.
        put(&path, &with_marker(OLD_SKILL, "codex", Place::AfterFrontMatter));
        assert_eq!(
            stale_warning(&path).unwrap(),
            format!("scrpad: the agent skill in {} is from an older scrpad; update it with scrpad --install-agent codex", path.display())
        );
        put(&path, &with_marker(OLD_SKILL, "skill", Place::AfterFrontMatter));
        assert!(stale_warning(&path).unwrap().ends_with("--install-agent skill"));
        // Unmarked: the current text is fine, the older one is stale, the user's own is not ours.
        put(&path, SKILL);
        assert_eq!(stale_warning(&path), None);
        put(&path, "my own skill");
        assert_eq!(stale_warning(&path), None);
        put(&path, OLD_SKILL);
        assert!(stale_warning(&path).unwrap().ends_with("--install-agent skill"));
    }

    #[cfg(unix)]
    #[test]
    fn a_group_shared_with_another_hook_keeps_that_hook() {
        let mixed = r#"{"hooks":{"PreToolUse":[{"matcher":"Bash","hooks":[
            {"type":"command","command":"rtk hook"},
            {"type":"command","command":"sh \"/old/scrpad/handoff-files.sh\""}]}]}}"#;
        let d = dirs("mixed");
        put(&d.hooks_json(), mixed);
        install(&d, AgentTarget::Codex);
        let after: Value = serde_json::from_str(&read(&d.hooks_json())).unwrap();
        let groups = after["hooks"]["PreToolUse"].as_array().unwrap();
        assert_eq!(groups.len(), 2, "{after}");
        assert_eq!(groups[0], json!({"matcher": "Bash", "hooks": [{"type": "command", "command": "rtk hook"}]}));
        assert_eq!(groups[1]["matcher"], "Bash|apply_patch");
        assert!(install(&d, AgentTarget::Codex).iter().any(|l| l.contains("already present")));
        run_apply(&d, AgentTarget::Codex, true, false).unwrap();
        let gone: Value = serde_json::from_str(&read(&d.hooks_json())).unwrap();
        assert_eq!(gone["hooks"]["PreToolUse"], json!([{"matcher": "Bash", "hooks": [{"type": "command", "command": "rtk hook"}]}]));
        // An uninstall straight from the mixed file keeps rtk's hook too.
        let d = dirs("mixed-uninstall");
        put(&d.hooks_json(), mixed);
        run_apply(&d, AgentTarget::Codex, true, false).unwrap();
        let gone: Value = serde_json::from_str(&read(&d.hooks_json())).unwrap();
        assert_eq!(gone["hooks"]["PreToolUse"][0]["hooks"], json!([{"type": "command", "command": "rtk hook"}]));
    }

    fn no_jq(program: &str) -> bool {
        program != "jq"
    }

    #[test]
    fn without_jq_only_the_skill_is_installed_and_the_warning_names_jq() {
        let d = dirs_with("no jq", no_jq);
        put(&d.hooks_json(), OTHERS);
        let lines = install(&d, AgentTarget::Codex);
        assert!(d.skill().exists());
        assert!(!d.hook_dir().exists());
        assert_eq!(read(&d.hooks_json()), OTHERS);
        let warning = lines.iter().find(|l| l.starts_with("warning:")).expect("a warning");
        assert!(warning.contains("not installed: it needs jq on PATH"), "{warning}");
        assert!(lines.iter().all(|l| !l.contains("Restart Codex")), "{lines:?}");
        let lines = run_apply(&d, AgentTarget::Codex, false, true).unwrap();
        assert!(lines.iter().any(|l| l.starts_with("warning: would not install") && l.contains("needs jq ")), "{lines:?}");
        let d = dirs_with("no tools", |_| false);
        let lines = install(&d, AgentTarget::Codex);
        assert!(lines.iter().any(|l| l.contains("needs sh and jq on PATH. Install them")), "{lines:?}");
    }

    #[test]
    fn with_sh_and_jq_the_hook_is_installed() {
        let d = dirs("both");
        let lines = install(&d, AgentTarget::Codex);
        assert!(d.hook_dir().join("handoff-files.sh").exists() && d.hooks_json().exists());
        assert!(lines.iter().all(|l| !l.starts_with("warning:")), "{lines:?}");
        assert!(lines.last().unwrap().starts_with("Restart Codex"), "{lines:?}");
    }

    #[test]
    fn a_windows_path_gets_forward_slashes() {
        assert_eq!(
            hook_command(r"C:\Users\me\.codex\scrpad\handoff-files.sh", true).unwrap(),
            r#"sh "C:/Users/me/.codex/scrpad/handoff-files.sh""#
        );
        assert_eq!(
            hook_command("/home/me/.codex/scrpad/handoff-files.sh", false).unwrap(),
            r#"sh "/home/me/.codex/scrpad/handoff-files.sh""#
        );
        assert!(hook_command(r"/home/me\x/handoff-files.sh", false).is_none());
        for bad in ["a\"b", "a$b", "a`b"] {
            assert!(hook_command(bad, true).is_none() && hook_command(bad, false).is_none(), "{bad}");
        }
    }

    #[cfg(unix)]
    #[test]
    fn hooks_json_keeps_its_mode_and_a_dangling_link_stops_the_install() {
        use std::os::unix::fs::PermissionsExt;
        let d = dirs("mode");
        put(&d.hooks_json(), OTHERS);
        std::fs::set_permissions(d.hooks_json(), std::fs::Permissions::from_mode(0o600)).unwrap();
        install(&d, AgentTarget::Codex);
        assert_eq!(std::fs::metadata(d.hooks_json()).unwrap().permissions().mode() & 0o777, 0o600);
        let d = dirs("dangling");
        std::fs::create_dir_all(&d.codex_home).unwrap();
        std::os::unix::fs::symlink(d.home.join("nowhere.json"), d.hooks_json()).unwrap();
        let error = run_apply(&d, AgentTarget::Codex, false, false).unwrap_err();
        assert!(error.contains("link to nothing"), "{error}");
        assert!(std::fs::symlink_metadata(d.hooks_json()).unwrap().file_type().is_symlink());
    }
}
