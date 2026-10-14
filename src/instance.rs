//! One scrpad per user: a second launch hands its command to the running one and exits. They
//! meet at a Unix domain socket in a directory only the user can enter (`$XDG_RUNTIME_DIR` on
//! Linux, the per-user `$TMPDIR` on macOS) or at a named pipe that only the user can open and
//! that refuses remote clients (Windows). Nothing here ever touches the network. `scrpad
//! --toggle` bound to a key in the compositor is how a hidden window comes back on Wayland,
//! where apps get no global keys.
//!
//! The protocol is one request and one reply per connection. A request is one line, `show`,
//! `toggle` or `list`, or a line of lengths followed by that many bytes of UTF-8:
//! `note <len>` + text, `append <title len> <text len>` + title + text, `read <title len>` +
//! title, `write <title len> <dir len>` + title + dir, `run <title len> <argv len>` + title +
//! argv joined by NUL; those three wait for the user's answer up to 2 minutes (`read` only when
//! reading is set to ask each time). The reply is `ok`, `text <len>` + text, `error <message>`, or `busy` from a
//! `scrpad --append` that writes the notes file with no window running: try again in a moment.
//! Older versions read one line, answer at most `note` (with `ok`) and hang up. Every read is
//! bounded in size and time.

use std::io::{self, BufRead, BufReader, Read, Write};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{Duration, Instant};

use interprocess::local_socket::prelude::*;
use interprocess::local_socket::{GenericFilePath, Listener, ListenerNonblockingMode, ListenerOptions, Stream};

use crate::cli::{self, Command, MAX_NOTE, MAX_TITLE};

/// Longer than any request line (`append 1024 1048576`).
const MAX_LINE: u64 = 32;
/// The longest reply line; error messages are cut to fit.
const MAX_REPLY_LINE: u64 = 1024;
/// The most text `--list` and `--read` hand out.
pub const MAX_TEXT: usize = 16 << 20;
/// How long the running instance waits for a request to arrive in full, or its reply to leave.
const TIMEOUT: Duration = Duration::from_secs(2);
/// How long a launch waits for its answer: an instance that is starting answers once its
/// window is up, and one that is busy is asked again until then.
const ANSWER_TIMEOUT: Duration = Duration::from_secs(10);
/// How long `--write` and `--run` wait for the user to answer in the running scrpad: longer than
/// it waits itself (`session::APPROVAL_WAIT`), so its own answer arrives first.
pub const APPROVAL_TIMEOUT: Duration = Duration::from_secs(130);

/// What the running instance answers.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Reply {
    Ok,
    /// What `--list` and `--read` print.
    Text(String),
    /// Refused, and why: printed on stderr.
    Error(String),
    /// The notes file is being written by a `scrpad --append` with no window: ask again.
    Busy,
}

fn encode(command: &Command) -> Vec<u8> {
    match command {
        Command::Show => b"show\n".to_vec(),
        Command::Toggle => b"toggle\n".to_vec(),
        Command::Note(text) => [format!("note {}\n", text.len()).as_bytes(), text.as_bytes()].concat(),
        Command::Append { title, text } => {
            [format!("append {} {}\n", title.len(), text.len()).as_bytes(), title.as_bytes(), text.as_bytes()].concat()
        }
        Command::List => b"list\n".to_vec(),
        Command::Read(title) => [format!("read {}\n", title.len()).as_bytes(), title.as_bytes()].concat(),
        Command::Write { title, dir } => {
            [format!("write {} {}\n", title.len(), dir.len()).as_bytes(), title.as_bytes(), dir.as_bytes()].concat()
        }
        Command::Run { title, argv } => {
            let argv = argv.join("\0");
            [format!("run {} {}\n", title.len(), argv.len()).as_bytes(), title.as_bytes(), argv.as_bytes()].concat()
        }
    }
}

/// Reads one command; `None` for anything malformed, too long or too slow.
fn decode(stream: &mut impl BufRead) -> Option<Command> {
    let line = read_line(stream, MAX_LINE).ok()??;
    let words: Vec<&str> = line.split(' ').collect();
    let len = |i: usize, max: usize| words[i].parse::<usize>().ok().filter(|&len| len <= max);
    let command = match words[..] {
        ["show"] => Command::Show,
        ["toggle"] => Command::Toggle,
        ["note", _] => Command::Note(read_text(stream, len(1, MAX_NOTE)?)?),
        ["append", _, _] => {
            let (title, text) = (len(1, MAX_TITLE)?, len(2, MAX_NOTE)?);
            let (title, text) = (read_text(stream, title)?, read_text(stream, text)?);
            cli::check_title(&title).and(cli::check_text(&text)).ok()?;
            Command::Append { title, text }
        }
        ["list"] => Command::List,
        ["read", _] => {
            let title = read_text(stream, len(1, MAX_TITLE)?)?;
            cli::check_title(&title).ok()?;
            Command::Read(title)
        }
        ["write", _, _] => {
            let (title, dir) = (len(1, MAX_TITLE)?, len(2, cli::MAX_PATH)?);
            let (title, dir) = (read_text(stream, title)?, read_text(stream, dir)?);
            cli::check_title(&title).and(crate::handoff::check_file_name(&title)).ok()?;
            Command::Write { title, dir }
        }
        ["run", _, _] => {
            let (title, argv) = (len(1, MAX_TITLE)?, len(2, cli::MAX_ARGV)?);
            let (title, argv) = (read_text(stream, title)?, read_text(stream, argv)?);
            cli::check_title(&title).ok()?;
            let argv: Vec<String> = argv.split('\0').map(str::to_owned).collect();
            (!argv[0].is_empty()).then_some(())?;
            Command::Run { title, argv }
        }
        _ => return None,
    };
    Some(command)
}

fn encode_reply(reply: &Reply) -> Vec<u8> {
    match reply {
        Reply::Ok => b"ok\n".to_vec(),
        Reply::Busy => b"busy\n".to_vec(),
        Reply::Text(text) => [format!("text {}\n", text.len()).as_bytes(), text.as_bytes()].concat(),
        Reply::Error(message) => format!("error {}\n", one_line(message)).into_bytes(),
    }
}

/// Reads the reply; `None` when the other side hung up without one (as older versions do).
fn decode_reply(stream: &mut impl BufRead) -> io::Result<Option<Reply>> {
    let invalid = || io::Error::new(io::ErrorKind::InvalidData, "the running scrpad sent a reply this version does not understand");
    let Some(line) = read_line(stream, MAX_REPLY_LINE)? else { return Ok(None) };
    Ok(Some(match line.split_once(' ') {
        None if line == "ok" => Reply::Ok,
        None if line == "busy" => Reply::Busy,
        Some(("error", message)) => Reply::Error(message.to_owned()),
        Some(("text", len)) => {
            let len = len.parse::<usize>().ok().filter(|&len| len <= MAX_TEXT).ok_or_else(invalid)?;
            Reply::Text(read_text(stream, len).ok_or_else(invalid)?)
        }
        _ => return Err(invalid()),
    }))
}

/// One line without its `\n`, at most `max` bytes with it; `None` at the end of the stream.
fn read_line(stream: &mut impl BufRead, max: u64) -> io::Result<Option<String>> {
    let mut line = String::new();
    if stream.take(max).read_line(&mut line)? == 0 {
        return Ok(None);
    }
    match line.strip_suffix('\n') {
        Some(line) => Ok(Some(line.to_owned())),
        None => Err(io::Error::new(io::ErrorKind::InvalidData, "line too long or cut short")),
    }
}

fn read_text(stream: &mut impl Read, len: usize) -> Option<String> {
    let mut bytes = vec![0; len];
    stream.read_exact(&mut bytes).ok()?;
    String::from_utf8(bytes).ok()
}

/// An error message as one reply line, cut short if need be.
fn one_line(message: &str) -> String {
    let mut line: String = message.chars().map(|c| if c.is_control() { ' ' } else { c }).collect();
    let max = MAX_REPLY_LINE as usize - "error …\n".len();
    if line.len() > max {
        let cut = (0..=max).rev().find(|&i| line.is_char_boundary(i)).unwrap_or(0);
        line.truncate(cut);
        line.push('…');
    }
    line
}

/// Where the running instance listens: a socket in a directory only this user can enter.
#[cfg(unix)]
pub fn address() -> Option<PathBuf> {
    use std::os::unix::fs::MetadataExt;
    // macOS has no runtime directory, but its $TMPDIR is per user and private.
    let dir = if cfg!(target_os = "macos") { Some(std::env::temp_dir()) } else { dirs::runtime_dir() }?;
    // Whoever else could enter the directory could connect, or plant a socket of their own.
    let private = std::fs::metadata(&dir).is_ok_and(|m| m.is_dir() && m.mode() & 0o077 == 0);
    private.then(|| dir.join("scrpad.sock"))
}

/// Where the running instance listens: a named pipe, one per user (pipe names are global to the
/// machine). The pipe itself lets in only its owner; see `listen`.
#[cfg(windows)]
pub fn address() -> Option<PathBuf> {
    let user: String =
        std::env::var("USERNAME").unwrap_or_default().chars().filter(|c| c.is_alphanumeric() || "-_.".contains(*c)).collect();
    Some(PathBuf::from(format!(r"\\.\pipe\scrpad-{user}")))
}

pub enum Claim {
    /// Another instance took the command and answered.
    Answered(Reply),
    /// Another instance runs but did not take the command (an older version, say).
    Refused(io::Error),
    /// This is the only instance; commands for it arrive here.
    Listening(Listener),
}

/// Sends `command` to a running instance, or becomes the instance that listens.
pub fn claim(address: &Path, command: &Command) -> io::Result<Claim> {
    let name = address.to_fs_name::<GenericFilePath>()?;
    let give_up = Instant::now() + ANSWER_TIMEOUT;
    loop {
        match Stream::connect(name.borrow()) {
            Ok(stream) => match send(&stream, command) {
                Ok(Reply::Busy) if Instant::now() < give_up => {
                    std::thread::sleep(Duration::from_millis(50));
                    continue;
                }
                Ok(Reply::Busy) => return Ok(Claim::Refused(io::Error::other("another scrpad kept the notes file busy; try again"))),
                Ok(reply) => return Ok(Claim::Answered(reply)),
                // It went away mid-answer (a `scrpad --append` done with the file, say): ask again.
                Err(e) if e.kind() == io::ErrorKind::ConnectionReset && Instant::now() < give_up => continue,
                Err(e) => return Ok(Claim::Refused(e)),
            },
            // Nobody answered: what is left is a socket from a crashed run, safe to replace.
            Err(e) if e.kind() == io::ErrorKind::ConnectionRefused => match std::fs::remove_file(address) {
                Err(e) if e.kind() != io::ErrorKind::NotFound => return Err(e),
                _ => {}
            },
            Err(e) if e.kind() == io::ErrorKind::NotFound => {}
            Err(e) => return Err(e),
        }
        match listen(name.borrow()) {
            Ok(listener) => return Ok(Claim::Listening(listener)),
            // Another launch got there first: ask it instead. (Windows says so with access denied.)
            Err(e)
                if (e.kind() == io::ErrorKind::AddrInUse || cfg!(windows) && e.kind() == io::ErrorKind::PermissionDenied)
                    && Instant::now() < give_up => {}
            Err(e) => return Err(e),
        }
    }
}

/// The socket is left behind on exit (a later launch replaces it), so the listener never removes it:
/// that could remove the socket of an instance started since.
#[cfg(unix)]
fn listen(name: interprocess::local_socket::Name<'_>) -> io::Result<Listener> {
    ListenerOptions::new().name(name).reclaim_name(false).create_sync()
}

/// The default security descriptor of a named pipe lets everyone read it, so the pipe gets one
/// that lets in its owner (this user) and nobody else. interprocess makes the pipe refuse remote
/// clients and fail if another process (another user, say) already has the name.
#[cfg(windows)]
fn listen(name: interprocess::local_socket::Name<'_>) -> io::Result<Listener> {
    use interprocess::os::windows::local_socket::ListenerOptionsExt;
    use interprocess::os::windows::security_descriptor::SecurityDescriptor;
    // Protected DACL with one entry: generic all for OWNER RIGHTS.
    let owner_only = SecurityDescriptor::deserialize(widestring::u16cstr!("D:P(A;;GA;;;OW)"))?;
    ListenerOptions::new().name(name).security_descriptor(owner_only).create_sync()
}

/// Keeps the claim while `work` runs (a script command with no window running), then gives it
/// up. Launches meanwhile wait in the backlog and are told `busy`, so they ask again and only
/// then start a window and load the notes file, after `work` saved it.
pub fn hold<T>(listener: Listener, address: &Path, work: impl FnOnce() -> T) -> T {
    let nonblocking = listener.set_nonblocking(ListenerNonblockingMode::Accept);
    let result = work();
    // No launch can connect once the socket is gone; then the waiting ones get their answer.
    #[cfg(unix)]
    let _ = std::fs::remove_file(address);
    #[cfg(windows)]
    let _ = address;
    while nonblocking.is_ok()
        && let Ok(stream) = listener.accept()
    {
        let _ = stream.set_nonblocking(false);
        answer(&stream, |_| Reply::Busy);
    }
    result
}

/// Sends `command` and waits for the answer.
fn send(stream: &Stream, command: &Command) -> io::Result<Reply> {
    let limit = if command.may_wait() { APPROVAL_TIMEOUT } else { ANSWER_TIMEOUT };
    within(stream, limit, || {
        let mut writer = stream;
        writer.write_all(&encode(command))?;
        match decode_reply(&mut BufReader::new(stream))? {
            Some(reply) => Ok(reply),
            // Older versions hang up without a word once they have shown or toggled the window.
            None if matches!(command, Command::Show | Command::Toggle) => Ok(Reply::Ok),
            // It quit while asking the user, most likely; an older version hangs up the same way.
            None if command.may_wait() => {
                Err(io::Error::other("scrpad closed before you answered (or the running scrpad is an older version; restart it)"))
            }
            None => Err(io::Error::other("the running scrpad did not take the command; restart it if it is an older version")),
        }
    })
}

/// Answers every command another launch sends, each connection on a thread of its own: a
/// question waiting for the user never holds up `--toggle` or `--append`.
pub fn serve(listener: Listener, on_command: impl Fn(Command) -> Reply + Send + Sync + 'static) {
    let on_command = Arc::new(on_command);
    std::thread::spawn(move || {
        loop {
            let stream = match listener.accept() {
                Ok(stream) => stream,
                Err(_) => {
                    // Out of file descriptors, say: don't spin.
                    std::thread::sleep(Duration::from_millis(100));
                    continue;
                }
            };
            let on_command = on_command.clone();
            std::thread::spawn(move || answer(&stream, &*on_command));
        }
    });
}

/// Reads one request from `stream` and writes the reply `on_command` gives it.
fn answer(stream: &Stream, on_command: impl Fn(Command) -> Reply) {
    let Some(command) = within(stream, TIMEOUT, || decode(&mut BufReader::new(stream))) else { return };
    let reply = match on_command(command) {
        Reply::Text(text) if text.len() > MAX_TEXT => Reply::Error("the text is larger than 16 MiB".into()),
        reply => reply,
    };
    let _ = within(stream, TIMEOUT, || (&*stream).write_all(&encode_reply(&reply)));
}

/// Runs `exchange` on `stream`, whose reads and writes give up after `limit`.
#[cfg(unix)]
fn within<T>(stream: &Stream, limit: Duration, exchange: impl FnOnce() -> T) -> T {
    let _ = stream.set_recv_timeout(Some(limit));
    let _ = stream.set_send_timeout(Some(limit));
    exchange()
}

/// Runs `exchange` on `stream` and, once `limit` has passed, cancels its pending read or write
/// (and any it starts after that) until it returns. Named pipes have no timeouts of their own.
#[cfg(windows)]
fn within<T>(stream: &Stream, limit: Duration, exchange: impl FnOnce() -> T) -> T {
    use std::os::windows::io::{AsHandle, AsRawHandle};
    use std::sync::mpsc;
    let Stream::NamedPipe(pipe) = stream;
    let handle = pipe.as_handle().as_raw_handle() as usize;
    let (done, finished) = mpsc::channel::<()>();
    std::thread::scope(|scope| {
        scope.spawn(move || {
            let mut wait = limit;
            while finished.recv_timeout(wait) == Err(mpsc::RecvTimeoutError::Timeout) {
                // SAFETY: `stream` outlives the scope, so the handle stays open while this runs.
                unsafe { windows_sys::Win32::System::IO::CancelIoEx(handle as _, std::ptr::null()) };
                wait = Duration::from_millis(50);
            }
        });
        let result = exchange();
        drop(done);
        result
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::mpsc;

    fn temp_socket(name: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("scrpad-test-{}-{name}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("scrpad.sock");
        let _ = std::fs::remove_file(&path);
        path
    }

    fn append(title: &str, text: &str) -> Command {
        Command::Append { title: title.into(), text: text.into() }
    }

    #[test]
    fn commands_round_trip_and_garbage_is_dropped() {
        for command in [
            Command::Show,
            Command::Toggle,
            Command::Note(String::new()),
            Command::Note("två\nrader".into()),
            append("Agent", "cargo test --locked"),
            append(&"t".repeat(MAX_TITLE), &"x".repeat(MAX_NOTE)),
            Command::List,
            Command::Read("Ärende".into()),
        ] {
            assert_eq!(decode(&mut &encode(&command)[..]), Some(command));
        }
        // What older versions send.
        assert_eq!(decode(&mut &b"show\n"[..]), Some(Command::Show));
        for garbage in [
            &b"show"[..],
            b"shout\n",
            b"show 1\n",
            b"note\n",
            b"note -1\n",
            b"note 5\nabc",
            b"note 2\n\xff\xfe",
            format!("note {}\n", MAX_NOTE + 1).as_bytes(),
            &[b'x'; 1000],
            b"append 5\nAgent",
            b"append 5 3\nAgentab",
            b"append 0 1\nx",
            b"append 5 0\nAgent",
            b"append 3 1\na\nbc",
            format!("append {} 1\n", MAX_TITLE + 1).as_bytes(),
            format!("append 1 {}\n", MAX_NOTE + 1).as_bytes(),
            b"list 1\n",
            b"read\n",
            b"read 0\n",
            b"read 9\nshort",
        ] {
            assert_eq!(decode(&mut &garbage[..]), None, "{:?}", String::from_utf8_lossy(garbage));
        }
    }

    #[test]
    fn handoff_commands_round_trip() {
        for command in [
            Command::Write { title: ".env".into(), dir: "/home/u/proj".into() },
            Command::Run { title: ".env".into(), argv: vec!["npm".into(), "run".into(), "dev".into()] },
        ] {
            let bytes = encode(&command);
            assert_eq!(decode(&mut &bytes[..]), Some(command));
        }
        // The server checks the file name again: a client can't be trusted with it.
        let bad = encode(&Command::Write { title: "../x".into(), dir: "/tmp".into() });
        assert_eq!(decode(&mut &bad[..]), None);
    }

    #[test]
    fn replies_round_trip_and_garbage_is_refused() {
        for reply in
            [Reply::Ok, Reply::Busy, Reply::Text(String::new()), Reply::Text("a\nb".into()), Reply::Error("“X” is encrypted".into())]
        {
            assert_eq!(decode_reply(&mut &encode_reply(&reply)[..]).unwrap(), Some(reply));
        }
        assert_eq!(decode_reply(&mut &b""[..]).unwrap(), None);
        // Error messages stay on one line of bounded length.
        let long = Reply::Error(format!("two\nlines {}", "ä".repeat(2000)));
        let encoded = encode_reply(&long);
        assert!(encoded.len() as u64 <= MAX_REPLY_LINE);
        let Some(Reply::Error(message)) = decode_reply(&mut &encoded[..]).unwrap() else { panic!("not an error") };
        assert!(message.starts_with("two lines ä") && message.ends_with('…'));
        for garbage in [
            &b"ok"[..],
            b"fine\n",
            b"text\n",
            b"text 5\nabc",
            b"text 2\n\xff\xfe",
            format!("text {}\n", MAX_TEXT + 1).as_bytes(),
            &[b'x'; 2000],
        ] {
            assert!(decode_reply(&mut &garbage[..]).is_err(), "{:?}", String::from_utf8_lossy(garbage));
        }
    }

    fn listening(path: &Path) -> Listener {
        let Claim::Listening(listener) = claim(path, &Command::Show).unwrap() else { panic!("no instance should be running") };
        listener
    }

    fn answered(path: &Path, command: &Command) -> Reply {
        match claim(path, command).unwrap() {
            Claim::Answered(reply) => reply,
            Claim::Refused(e) => panic!("refused: {e}"),
            Claim::Listening(_) => panic!("nobody answered"),
        }
    }

    #[test]
    fn second_launch_forwards_to_the_first() {
        let path = temp_socket("forward");
        let (tx, rx) = mpsc::channel();
        serve(listening(&path), move |c| {
            let reply = if c == Command::List { Reply::Text("Agent\n".into()) } else { Reply::Ok };
            tx.send(c).unwrap();
            reply
        });
        assert_eq!(answered(&path, &Command::Toggle), Reply::Ok);
        assert_eq!(rx.recv_timeout(Duration::from_secs(2)).unwrap(), Command::Toggle);
        let note = Command::Note("x".repeat(MAX_NOTE));
        assert_eq!(answered(&path, &note), Reply::Ok);
        assert_eq!(rx.recv_timeout(Duration::from_secs(2)).unwrap(), note);
        assert_eq!(answered(&path, &Command::List), Reply::Text("Agent\n".into()));
        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn a_busy_instance_is_asked_again_until_it_is_gone() {
        let path = temp_socket("busy");
        // What `scrpad --append` does while it writes the notes file with no window running.
        let listener = listening(&path);
        let (started, writing) = mpsc::channel();
        let holder = {
            let path = path.clone();
            std::thread::spawn(move || {
                hold(listener, &path, || {
                    started.send(()).unwrap();
                    std::thread::sleep(Duration::from_millis(300));
                })
            })
        };
        writing.recv().unwrap();
        // The launch waits, then becomes the instance itself.
        assert!(matches!(claim(&path, &Command::Show).unwrap(), Claim::Listening(_)));
        holder.join().unwrap();
        let _ = std::fs::remove_file(&path);
    }

    #[cfg(unix)]
    #[test]
    fn a_note_an_older_instance_ignores_is_refused_not_lost() {
        let path = temp_socket("older");
        let listener = std::os::unix::net::UnixListener::bind(&path).unwrap();
        // Reads one line and hangs up, as versions without notes do.
        std::thread::spawn(move || {
            for stream in listener.incoming().flatten() {
                let _ = BufReader::new(stream).read_line(&mut String::new());
            }
        });
        assert!(matches!(claim(&path, &Command::Note("keep me".into())).unwrap(), Claim::Refused(_)));
        assert!(matches!(claim(&path, &append("Agent", "keep me")).unwrap(), Claim::Refused(_)));
        // A request for approval hung up on: most likely scrpad quit while asking.
        let run = Command::Run { title: ".env".into(), argv: vec!["true".into()] };
        let Claim::Refused(e) = claim(&path, &run).unwrap() else { panic!("not refused") };
        assert!(e.to_string().starts_with("scrpad closed before you answered"), "{e}");
        // Showing is all they do, and they did it.
        assert_eq!(answered(&path, &Command::Show), Reply::Ok);
        assert!(path.exists(), "the running instance keeps its socket");
        let _ = std::fs::remove_file(&path);
    }

    #[cfg(unix)]
    #[test]
    fn a_stale_socket_is_replaced() {
        let path = temp_socket("stale");
        drop(std::os::unix::net::UnixListener::bind(&path).unwrap());
        assert!(path.exists());
        assert!(matches!(claim(&path, &Command::Show).unwrap(), Claim::Listening(_)));
        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn a_silent_client_does_not_block_the_next() {
        let path = temp_socket("silent");
        serve(listening(&path), |_| Reply::Ok);
        // Connects and sends half a request, then nothing.
        let mut stuck = Stream::connect(path.as_path().to_fs_name::<GenericFilePath>().unwrap()).unwrap();
        stuck.write_all(b"note 10\nabc").unwrap();
        let started = Instant::now();
        assert_eq!(answered(&path, &Command::Show), Reply::Ok);
        assert!(started.elapsed() < TIMEOUT + Duration::from_secs(1));
        let _ = std::fs::remove_file(&path);
    }
}
