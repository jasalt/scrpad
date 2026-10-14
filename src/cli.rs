//! The command line: `scrpad [--tui] [--toggle | --new | - | --append TITLE TEXT | --list | --read TITLE |
//! --write TITLE DIR | --run TITLE -- CMD… | --skill]`, and `scrpad --install-agent TARGET` or
//! `--uninstall-agent TARGET`, each with `--dry-run`, to set scrpad up for an AI agent.

use std::io::Read;

/// The longest note `scrpad -` takes from stdin, and the longest text `--append` adds.
pub const MAX_NOTE: usize = 1 << 20;
/// The longest title `--append` and `--read` take, in bytes.
pub const MAX_TITLE: usize = 1024;
/// The longest folder `--write` takes, in bytes.
pub const MAX_PATH: usize = 4096;
/// The longest command `--run` takes: its arguments joined, one byte between each.
pub const MAX_ARGV: usize = 64 << 10;

pub const HELP: &str = "\
scrpad: a tiny floating scratchpad

Usage: scrpad [--tui] [--toggle | --new | - | --append TITLE TEXT | --list | --read TITLE
              | --write TITLE DIR | --run TITLE -- CMD… | --skill]
       scrpad --install-agent codex|skill|claude [--dry-run]
       scrpad --uninstall-agent codex|skill|claude [--dry-run]

  (nothing)            show the window
  --tui                open the notes in this terminal instead (alone, or with --new or -);
                       chosen by itself on Linux and BSD when there is no display (SSH)
  --toggle             hide the window when it has the focus, show it otherwise
  --new                open a new empty note
  -                    open a new note with the text from stdin (UTF-8, at most 1 MiB)
  --append TITLE TEXT  add TEXT as a new paragraph to the note titled TITLE, made if missing;
                       TEXT - reads it from stdin (UTF-8, at most 1 MiB)
  --list               print the titles of the notes scripts may read, one per line
  --read TITLE         print the text of the first readable note titled TITLE
  --write TITLE DIR    write the note titled TITLE to the file DIR/TITLE, once you allow it
                       in scrpad; TITLE must be a plain file name (.env, config.json)
  --run TITLE -- CMD   run CMD with the note's KEY=VALUE lines as environment variables,
                       once you allow it in scrpad; the note's text is never printed
  --skill              print the SKILL.md that teaches an AI agent these commands
  --install-agent T    set scrpad up for an agent: codex = the skill and the hook that keeps it
                       from reading .env files; skill = the skill only (Gemini CLI, and others
                       that read ~/.agents/skills); claude = how, as it comes with the plugin.
                       Run it again after updating scrpad: that is the upgrade
  --uninstall-agent T  take away what --install-agent put there
  --dry-run            with these two: say what would change, and change nothing
  -h, --help           show this help

A running scrpad takes over: it shows the window and adds the note. --append, --list and
--read leave the window as it is, and work on the notes file when no scrpad runs.
--list and --read need \"Let scripts read notes\" in the … menu; hidden and encrypted
notes are never listed or read, and --append refuses an encrypted note.
--write and --run need a running scrpad (window or --tui) to ask you, every time.
";

/// The agent skill (`plugin/skills/scrpad/SKILL.md`) that `--skill` prints, for agents that
/// read skills; Claude Code gets it with the plugin.
pub const SKILL: &str = include_str!("../plugin/skills/scrpad/SKILL.md");

/// What the command line asks for: a launch, or a text to print before stopping.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Parsed {
    Launch(Launch),
    /// `--help` and `--skill`.
    Print(&'static str),
    /// `--install-agent` and `--uninstall-agent`: set an agent up, or undo it. Starts no window.
    Agent {
        target: AgentTarget,
        uninstall: bool,
        dry_run: bool,
    },
}

/// Which agent `--install-agent` sets scrpad up for.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum AgentTarget {
    /// The skill, and the `.env` guard hook.
    Codex,
    /// The skill only, for any agent that reads Agent Skills from `~/.agents/skills`.
    Skill,
    /// Nothing to do: the plugin brings both.
    Claude,
}

impl AgentTarget {
    fn parse(name: &str) -> Result<Self, String> {
        match name {
            "codex" => Ok(Self::Codex),
            "skill" => Ok(Self::Skill),
            "claude" => Ok(Self::Claude),
            other => Err(format!("unknown agent “{other}”: codex, skill or claude (see scrpad --help)")),
        }
    }

    pub fn name(self) -> &'static str {
        match self {
            Self::Codex => "codex",
            Self::Skill => "skill",
            Self::Claude => "claude",
        }
    }
}

/// A launch: what it asks for, and whether it opens the notes in the terminal (`--tui`).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Launch {
    pub command: Command,
    pub tui: bool,
}

/// What a launch asks the app for, which may be one already running.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Command {
    /// A plain launch: bring the window here.
    Show,
    /// `--toggle`: hide the window when it has the focus, otherwise show it.
    Toggle,
    /// `--new` (empty) and `-` (stdin): add a note with this text and bring the window here.
    Note(String),
    /// `--append`: add `text` as a paragraph to the note titled `title`, without showing anything.
    Append { title: String, text: String },
    /// `--list`: the titles of the readable notes.
    List,
    /// `--read`: the text of a readable note.
    Read(String),
    /// `--write`: put the note titled `title` in the file `dir/title`, once the user allows it.
    /// `dir` is absolute (made so here, from where the launch runs).
    Write { title: String, dir: String },
    /// `--run`: run `argv` with the note's `KEY=VALUE` lines in its environment, once the user
    /// allows it. Only this launch gets the text; it is never printed.
    Run { title: String, argv: Vec<String> },
}

impl Command {
    /// For scripts: carried out on the notes file when no window runs, never starting one.
    pub fn is_script(&self) -> bool {
        matches!(self, Command::Append { .. } | Command::List | Command::Read(_) | Command::Write { .. } | Command::Run { .. })
    }

    /// Carried out only once the user allows it in the running scrpad.
    pub fn needs_approval(&self) -> bool {
        matches!(self, Command::Write { .. } | Command::Run { .. })
    }
}

/// What `args` (without the program name) ask for. Reads `stdin` only for `-` and
/// `--append TITLE -`.
pub fn parse(args: impl Iterator<Item = String>, stdin: impl Read) -> Result<Parsed, String> {
    let mut args = args.peekable();
    let mut wish = None;
    let mut tui = false;
    let mut dry_run = false;
    while let Some(arg) = args.next() {
        let mut value = |what: &str| args.next().ok_or_else(|| format!("{arg} needs {what} (see scrpad --help)"));
        let command = match arg.as_str() {
            "-h" | "--help" => return Ok(Parsed::Print(HELP)),
            "--skill" => {
                if dry_run || args.any(|a| a == "--dry-run") {
                    return Err("--dry-run goes only with --install-agent or --uninstall-agent (see scrpad --help)".into());
                }
                return Ok(Parsed::Print(SKILL));
            }
            "--tui" => {
                tui = true;
                continue;
            }
            "--dry-run" => {
                dry_run = true;
                continue;
            }
            "--install-agent" | "--uninstall-agent" => {
                let target = AgentTarget::parse(&value("an agent: codex, skill or claude")?)?;
                Wish::Agent(target, arg == "--uninstall-agent")
            }
            "--toggle" => Wish::Toggle,
            "--new" => Wish::New,
            "-" => Wish::Stdin,
            "--append" => {
                let title = value("a title and a text")?;
                Wish::Append(title, value("a text, or - for stdin")?)
            }
            "--list" => Wish::List,
            "--read" => Wish::Read(value("a title")?),
            "--write" => {
                let title = value("a title and a folder")?;
                Wish::Write(title, value("a folder")?)
            }
            "--run" => {
                let title = value("a title, -- and a command")?;
                if args.next().as_deref() != Some("--") {
                    return Err("--run TITLE needs -- before the command (see scrpad --help)".into());
                }
                // Everything after `--` is the command, flags included.
                Wish::Run(title, args.by_ref().collect())
            }
            // Finder adds a process serial number when it starts an app on older macOS.
            psn if psn.starts_with("-psn_") => continue,
            other => return Err(format!("unknown argument “{other}” (see scrpad --help)")),
        };
        if wish.replace(command).is_some() {
            return Err("give only one command (see scrpad --help)".into());
        }
    }
    if dry_run && !matches!(wish, Some(Wish::Agent(..))) {
        return Err("--dry-run goes only with --install-agent or --uninstall-agent (see scrpad --help)".into());
    }
    // The terminal UI opens the notes: what hides a window or only runs a script has no place.
    if tui && !matches!(wish, None | Some(Wish::New | Wish::Stdin)) {
        return Err("--tui goes only with --new or - (see scrpad --help)".into());
    }
    if let Some(Wish::Agent(target, uninstall)) = wish {
        return Ok(Parsed::Agent { target, uninstall, dry_run });
    }
    let command = match wish {
        None => Command::Show,
        Some(Wish::Toggle) => Command::Toggle,
        Some(Wish::New) => Command::Note(String::new()),
        Some(Wish::Stdin) => Command::Note(read_note(stdin)?),
        Some(Wish::Append(title, text)) => {
            check_title(&title)?;
            let text = if text == "-" { read_note(stdin)? } else { text };
            check_text(&text)?;
            Command::Append { title, text }
        }
        Some(Wish::List) => Command::List,
        Some(Wish::Read(title)) => {
            check_title(&title)?;
            Command::Read(title)
        }
        Some(Wish::Write(title, dir)) => {
            check_title(&title)?;
            crate::handoff::check_file_name(&title)?;
            let dir = std::fs::canonicalize(&dir).map_err(|e| format!("{dir}: {e}"))?;
            let dir = dir.into_os_string().into_string().map_err(|_| "the folder's path is not UTF-8".to_owned())?;
            if dir.len() > MAX_PATH {
                return Err(format!("the folder's path is longer than {MAX_PATH} bytes"));
            }
            Command::Write { title, dir }
        }
        Some(Wish::Agent(..)) => unreachable!("returned above"),
        Some(Wish::Run(title, argv)) => {
            check_title(&title)?;
            if argv.is_empty() {
                return Err("--run needs a command after -- (see scrpad --help)".into());
            }
            if argv.iter().map(|a| a.len() + 1).sum::<usize>() > MAX_ARGV || argv.iter().any(|a| a.contains('\0')) {
                return Err("the command is longer than 64 KiB".into());
            }
            Command::Run { title, argv }
        }
    };
    Ok(Parsed::Launch(Launch { command, tui }))
}

/// A command as given, before stdin is read.
enum Wish {
    Toggle,
    New,
    Stdin,
    Append(String, String),
    List,
    Read(String),
    Write(String, String),
    Run(String, Vec<String>),
    Agent(AgentTarget, bool),
}

/// A title is one line of at most `MAX_TITLE` bytes, as the rename field makes them.
pub fn check_title(title: &str) -> Result<(), String> {
    if title.is_empty() {
        Err("the title is empty".into())
    } else if title.len() > MAX_TITLE {
        Err(format!("the title is longer than {MAX_TITLE} bytes"))
    } else if title.chars().any(char::is_control) {
        Err("the title holds a line break or another control character".into())
    } else {
        Ok(())
    }
}

/// What `--append` adds: something, and at most `MAX_NOTE` bytes.
pub fn check_text(text: &str) -> Result<(), String> {
    if text.is_empty() {
        Err("there is no text to append".into())
    } else if text.len() > MAX_NOTE {
        Err("the text is longer than 1 MiB".into())
    } else {
        Ok(())
    }
}

/// At most `MAX_NOTE` bytes of UTF-8 text, without the one trailing newline `echo` adds.
pub fn read_note(input: impl Read) -> Result<String, String> {
    let mut bytes = Vec::new();
    input.take(MAX_NOTE as u64 + 1).read_to_end(&mut bytes).map_err(|e| format!("could not read stdin: {e}"))?;
    if bytes.len() > MAX_NOTE {
        return Err("stdin holds more than 1 MiB; a note takes at most that".into());
    }
    let mut text = String::from_utf8(bytes).map_err(|_| "stdin is not UTF-8 text".to_owned())?;
    if text.ends_with('\n') {
        text.pop();
        if text.ends_with('\r') {
            text.pop();
        }
    }
    Ok(text)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn parse_with(args: &[&str], stdin: &[u8]) -> Result<Option<Command>, String> {
        launch_with(args, stdin).map(|launch| launch.map(|l| l.command))
    }

    /// `None` for a text to print.
    fn launch_with(args: &[&str], stdin: &[u8]) -> Result<Option<Launch>, String> {
        parse(args.iter().map(|s| s.to_string()), stdin).map(|parsed| match parsed {
            Parsed::Launch(launch) => Some(launch),
            Parsed::Print(_) | Parsed::Agent { .. } => None,
        })
    }

    fn append(title: &str, text: &str) -> Command {
        Command::Append { title: title.into(), text: text.into() }
    }

    #[test]
    fn flags_are_recognised() {
        assert_eq!(parse_with(&[], b""), Ok(Some(Command::Show)));
        assert_eq!(parse_with(&["--toggle"], b""), Ok(Some(Command::Toggle)));
        assert_eq!(parse_with(&["--new"], b"ignored"), Ok(Some(Command::Note(String::new()))));
        assert_eq!(parse_with(&["-"], b"sk-live-TOKEN\n"), Ok(Some(Command::Note("sk-live-TOKEN".into()))));
        assert_eq!(parse(["--help".to_string()].into_iter(), &b""[..]), Ok(Parsed::Print(HELP)));
        assert_eq!(parse(["--skill".to_string()].into_iter(), &b""[..]), Ok(Parsed::Print(SKILL)));
        assert_eq!(parse_with(&["-psn_0_12345"], b""), Ok(Some(Command::Show)));
        assert!(parse_with(&["--bogus"], b"").unwrap_err().contains("--bogus"));
        assert!(parse_with(&["--new", "-"], b"").is_err());
    }

    #[test]
    fn tui_goes_with_what_opens_the_notes() {
        let tui = |command| Ok(Some(Launch { command, tui: true }));
        assert_eq!(launch_with(&["--tui"], b""), tui(Command::Show));
        assert_eq!(launch_with(&["--new", "--tui"], b""), tui(Command::Note(String::new())));
        assert_eq!(launch_with(&["--tui", "-"], b"text\n"), tui(Command::Note("text".into())));
        assert_eq!(launch_with(&[], b""), Ok(Some(Launch { command: Command::Show, tui: false })));
        for args in [&["--tui", "--toggle"][..], &["--tui", "--list"], &["--append", "a", "b", "--tui"]] {
            assert!(launch_with(args, b"").unwrap_err().contains("--tui goes only"), "{args:?}");
        }
    }

    #[test]
    fn script_commands_take_their_values() {
        assert_eq!(parse_with(&["--append", "Agent", "cargo test"], b"ignored"), Ok(Some(append("Agent", "cargo test"))));
        assert_eq!(parse_with(&["--append", "Agent", "-"], b"ls -la\n"), Ok(Some(append("Agent", "ls -la"))));
        assert_eq!(parse_with(&["--list"], b""), Ok(Some(Command::List)));
        assert_eq!(parse_with(&["--read", "Todo"], b""), Ok(Some(Command::Read("Todo".into()))));
        // A value that looks like a flag is still the value.
        assert_eq!(parse_with(&["--read", "--list"], b""), Ok(Some(Command::Read("--list".into()))));
        assert!([Command::List, Command::Read("x".into()), append("x", "y")].iter().all(Command::is_script));
        assert!(![Command::Show, Command::Toggle, Command::Note("x".into())].iter().any(Command::is_script));
    }

    #[test]
    fn script_commands_refuse_bad_values() {
        assert!(parse_with(&["--append"], b"").unwrap_err().contains("needs a title"));
        assert!(parse_with(&["--append", "Agent"], b"").unwrap_err().contains("needs a text"));
        assert!(parse_with(&["--read"], b"").unwrap_err().contains("needs a title"));
        assert!(parse_with(&["--list", "--read", "x"], b"").unwrap_err().contains("only one"));
        assert!(parse_with(&["--append", "", "x"], b"").unwrap_err().contains("empty"));
        assert!(parse_with(&["--append", "two\nlines", "x"], b"").unwrap_err().contains("line break"));
        assert!(parse_with(&["--read", &"t".repeat(MAX_TITLE + 1)], b"").unwrap_err().contains("longer"));
        assert!(parse_with(&["--append", "Agent", ""], b"").unwrap_err().contains("no text"));
        assert!(parse_with(&["--append", "Agent", "-"], b"\n").unwrap_err().contains("no text"));
        assert!(parse_with(&["--append", "Agent", &"a".repeat(MAX_NOTE + 1)], b"").unwrap_err().contains("1 MiB"));
        assert!(parse_with(&["--append", "Agent", "-"], &vec![b'a'; MAX_NOTE + 1]).unwrap_err().contains("1 MiB"));
        assert!(parse_with(&["--append", "Agent", "-"], &[0xff]).unwrap_err().contains("UTF-8"));
        assert!(check_title(&"t".repeat(MAX_TITLE)).is_ok());
    }

    #[test]
    fn handoff_commands_take_a_file_name_and_a_folder_or_a_command() {
        let dir = std::fs::canonicalize(std::env::temp_dir()).unwrap();
        let dir_s = dir.to_str().unwrap();
        assert_eq!(parse_with(&["--write", ".env", dir_s], b""), Ok(Some(Command::Write { title: ".env".into(), dir: dir_s.into() })));
        assert_eq!(
            parse_with(&["--run", ".env", "--", "npm", "--version"], b""),
            Ok(Some(Command::Run { title: ".env".into(), argv: vec!["npm".into(), "--version".into()] }))
        );
        let both = [Command::Write { title: "a".into(), dir: "/".into() }, Command::Run { title: "a".into(), argv: vec!["x".into()] }];
        assert!(both.iter().all(|c| c.is_script() && c.needs_approval()));
        assert!(![Command::List, append("a", "b")].iter().any(Command::needs_approval));
    }

    #[test]
    fn handoff_commands_refuse_bad_values() {
        assert!(parse_with(&["--write", "../x", "/tmp"], b"").unwrap_err().contains("file name"));
        assert!(parse_with(&["--write", ".env", "/no/such/dir"], b"").unwrap_err().contains("/no/such/dir"));
        assert!(parse_with(&["--write", ".env"], b"").unwrap_err().contains("needs"));
        assert!(parse_with(&["--run", ".env", "npm"], b"").unwrap_err().contains("--"));
        assert!(parse_with(&["--run", ".env", "--"], b"").unwrap_err().contains("command"));
        assert!(parse_with(&["--run", "a/b", "--", "x"], b"").is_ok(), "a --run title is a note title, not a file name");
        assert!(parse_with(&["--run", ".env", "--", &"x".repeat(MAX_ARGV)], b"").unwrap_err().contains("64 KiB"));
        assert!(launch_with(&["--tui", "--write", ".env", "/tmp"], b"").unwrap_err().contains("--tui goes only"));
    }

    #[test]
    fn agent_commands_take_a_target_and_a_dry_run() {
        let agent = |args: &[&str]| parse(args.iter().map(|s| s.to_string()), &b""[..]);
        let want = |target, uninstall, dry_run| Ok(Parsed::Agent { target, uninstall, dry_run });
        assert_eq!(agent(&["--install-agent", "codex"]), want(AgentTarget::Codex, false, false));
        assert_eq!(agent(&["--dry-run", "--install-agent", "skill"]), want(AgentTarget::Skill, false, true));
        assert_eq!(agent(&["--uninstall-agent", "claude", "--dry-run"]), want(AgentTarget::Claude, true, true));
        assert!(agent(&["--install-agent"]).unwrap_err().contains("needs an agent"));
        assert!(agent(&["--install-agent", "vim"]).unwrap_err().contains("unknown agent “vim”"));
        assert!(agent(&["--dry-run"]).unwrap_err().contains("--dry-run goes only"));
        assert!(agent(&["--skill", "--dry-run"]).unwrap_err().contains("--dry-run goes only"));
        assert!(agent(&["--dry-run", "--skill"]).unwrap_err().contains("--dry-run goes only"));
        assert!(agent(&["--list", "--dry-run"]).unwrap_err().contains("--dry-run goes only"));
        assert!(agent(&["--install-agent", "codex", "--list"]).unwrap_err().contains("only one"));
        assert!(agent(&["--install-agent", "codex", "--uninstall-agent", "codex"]).unwrap_err().contains("only one"));
        assert!(agent(&["--tui", "--install-agent", "codex"]).unwrap_err().contains("--tui goes only"));
    }

    #[test]
    fn stdin_is_capped_and_must_be_utf8() {
        assert_eq!(read_note(&b"two\nlines\r\n"[..]).unwrap(), "two\nlines");
        assert_eq!(read_note(&b"keeps\n\n"[..]).unwrap(), "keeps\n");
        assert_eq!(read_note(&vec![b'a'; MAX_NOTE][..]).unwrap().len(), MAX_NOTE);
        assert!(read_note(&vec![b'a'; MAX_NOTE + 1][..]).unwrap_err().contains("1 MiB"));
        assert!(read_note(&[0xff, 0xfe][..]).unwrap_err().contains("UTF-8"));
    }
}
