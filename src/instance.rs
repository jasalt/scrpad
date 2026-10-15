//! One scrpad per user: a second launch hands its command to the running one and exits. They
//! meet at a Unix domain socket in a directory only the user can enter (`$XDG_RUNTIME_DIR` on
//! Linux, the per-user `$TMPDIR` on macOS) or at a named pipe that only the user can open, that
//! refuses remote clients, and that a launch talks to only once it has seen the user owns it
//! (Windows). Nothing here ever touches the network. `scrpad --toggle` bound to a key in the
//! compositor is how a hidden window comes back on Wayland, where apps get no global keys.
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
    /// `Text`, with a warning for stderr from the scan of that text, made by the side that
    /// knows whether the user has Warnings on. One line, never the text.
    Warned {
        text: String,
        warning: String,
    },
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
        Reply::Warned { text, warning } => {
            [format!("warned {} {}\n", text.len(), one_line_of(warning, "warned 16777216 …\n".len())).as_bytes(), text.as_bytes()].concat()
        }
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
        Some(("warned", rest)) => {
            let (len, warning) = rest.split_once(' ').ok_or_else(invalid)?;
            let len = len.parse::<usize>().ok().filter(|&len| len <= MAX_TEXT).ok_or_else(invalid)?;
            Reply::Warned { warning: warning.to_owned(), text: read_text(stream, len).ok_or_else(invalid)? }
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
    one_line_of(message, "error …\n".len())
}

/// `one_line`, leaving `reserved` bytes of the line for the rest of it.
fn one_line_of(message: &str, reserved: usize) -> String {
    let mut line: String = message.chars().map(|c| if c.is_control() { ' ' } else { c }).collect();
    let max = MAX_REPLY_LINE as usize - reserved;
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
/// machine), named after the user's SID: two accounts of the same name, one local and one on a
/// domain, get two pipes. The pipe itself lets in only its owner (see `listen`), and a launch
/// checks the owner before it talks (see `connect`); the name is no secret.
#[cfg(windows)]
pub fn address() -> Option<PathBuf> {
    let user = sid_string(&pipe::user().ok()?)?;
    Some(PathBuf::from(format!(r"\\.\pipe\scrpad-{user}")))
}

/// What a launch says of a pipe it did not create: someone took the name first.
#[cfg(any(windows, test))]
const NOT_OURS: &str = "the scrpad pipe belongs to another user";

/// Talks only to a pipe owned by this user: another user who creates the pipe before this
/// user's first launch would otherwise get every request, and answer `--run` as they like.
#[cfg(any(windows, test))]
fn check_owner(owner: &[u8], user: &[u8]) -> io::Result<()> {
    if owner == user { Ok(()) } else { Err(io::Error::new(io::ErrorKind::PermissionDenied, NOT_OURS)) }
}

/// A SID in its string form (`S-1-5-21-…`), from its bytes: revision, count of sub-authorities,
/// a 48-bit big-endian authority, then each sub-authority as 32 bits little-endian.
#[cfg(any(windows, test))]
fn sid_string(sid: &[u8]) -> Option<String> {
    use std::fmt::Write as _;
    let [revision, count, rest @ ..] = sid else { return None };
    let (authority, subs) = rest.split_first_chunk::<6>()?;
    if subs.len() != usize::from(*count) * 4 {
        return None;
    }
    let authority = authority.iter().fold(0u64, |sum, &byte| sum << 8 | u64::from(byte));
    let mut string = if authority >> 32 == 0 { format!("S-{revision}-{authority}") } else { format!("S-{revision}-0x{authority:012X}") };
    for sub in subs.chunks_exact(4) {
        let _ = write!(string, "-{}", u32::from_le_bytes([sub[0], sub[1], sub[2], sub[3]]));
    }
    Some(string)
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
        match connect(address, name.borrow()) {
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
            // Nobody answered: a socket from a crashed run is safe to replace, and where there is
            // none this launch is the first. Either way it listens, one launch at a time.
            Err(e) if matches!(e.kind(), io::ErrorKind::ConnectionRefused | io::ErrorKind::NotFound) => {
                match replace_stale(address, name.borrow())? {
                    Some(listener) => return Ok(Claim::Listening(listener)),
                    // Another launch got there first and listens: ask it instead.
                    None if Instant::now() < give_up => {}
                    None => return Err(io::Error::new(io::ErrorKind::AddrInUse, "another scrpad is starting")),
                }
            }
            // Another user's pipe (see `connect`): nothing was sent, and no second window may start.
            Err(e) if cfg!(windows) && e.kind() == io::ErrorKind::PermissionDenied => return Ok(Claim::Refused(e)),
            Err(e) => return Err(e),
        }
    }
}

/// Listens at `address`, replacing the socket of a crashed run. Two launches that both find a
/// socket refused would otherwise both remove it, the second removing the first's live socket
/// (or one that is between bind and listen, which refuses too), and both would act as the only
/// instance. So, on Unix, one at a time holds the lock `scrpad.lock` from the second look at the
/// socket until the listener exists; `None` when that look finds a live instance, or another
/// launch listening first. (Windows says so with access denied; a pipe is never stale.)
fn replace_stale(address: &Path, name: interprocess::local_socket::Name<'_>) -> io::Result<Option<Listener>> {
    #[cfg(unix)]
    let _lock = lock(address)?;
    if connect(address, name.borrow()).is_ok() {
        return Ok(None);
    }
    #[cfg(unix)]
    match std::fs::remove_file(address) {
        Err(e) if e.kind() != io::ErrorKind::NotFound => return Err(e),
        _ => {}
    }
    match listen(name) {
        Ok(listener) => Ok(Some(listener)),
        Err(e) if e.kind() == io::ErrorKind::AddrInUse => Ok(None),
        // Someone created the pipe since the look above: another launch, or another user who
        // took the name. The next `connect` in `claim` tells them apart by the pipe's owner.
        Err(e) if cfg!(windows) && e.kind() == io::ErrorKind::PermissionDenied => Ok(None),
        Err(e) => Err(e),
    }
}

/// The other end of the conversation with a running instance, as `claim` sees it.
#[cfg(unix)]
type Client = Stream;
/// The pipe is opened by hand (see `connect`); a synchronous handle reads and writes as a file.
#[cfg(windows)]
type Client = std::fs::File;

/// Connects to the running instance. The socket's directory lets in only this user (`address`).
#[cfg(unix)]
fn connect(_: &Path, name: interprocess::local_socket::Name<'_>) -> io::Result<Client> {
    Stream::connect(name)
}

/// Connects to the running instance's pipe, whose name anyone can take, so only once it has seen
/// that the pipe's owner is this user (`check_owner`), and never letting the other end act as
/// this user: interprocess opens the client end with the default, impersonation, and reopens any
/// handle it is given the same way. Access denied comes from a pipe that lets in someone else.
#[cfg(windows)]
fn connect(address: &Path, _: interprocess::local_socket::Name<'_>) -> io::Result<Client> {
    use std::os::windows::fs::OpenOptionsExt;
    use std::os::windows::io::AsHandle;
    use windows_sys::Win32::Foundation::ERROR_PIPE_BUSY;
    use windows_sys::Win32::Storage::FileSystem::{SECURITY_IDENTIFICATION, SECURITY_SQOS_PRESENT};
    let give_up = Instant::now() + ANSWER_TIMEOUT;
    let pipe = loop {
        let mut options = std::fs::OpenOptions::new();
        // Identification: the server may ask who connects, but never act as them.
        options.read(true).write(true).security_qos_flags(SECURITY_SQOS_PRESENT | SECURITY_IDENTIFICATION);
        match options.open(address) {
            // Every instance of the pipe has a client: the server makes another one as it accepts.
            Err(e) if e.raw_os_error().and_then(|code| u32::try_from(code).ok()) == Some(ERROR_PIPE_BUSY) && Instant::now() < give_up => {
                std::thread::sleep(Duration::from_millis(50));
            }
            Err(e) if e.kind() == io::ErrorKind::PermissionDenied => return Err(io::Error::new(e.kind(), NOT_OURS)),
            result => break result?,
        }
    };
    check_owner(&pipe::owner(pipe.as_handle())?, &pipe::user()?)?;
    Ok(pipe)
}

/// An exclusive lock on `scrpad.lock` next to the socket, held until the returned file is dropped.
#[cfg(unix)]
fn lock(address: &Path) -> io::Result<std::fs::File> {
    use std::os::unix::fs::OpenOptionsExt;
    let file =
        std::fs::OpenOptions::new().create(true).truncate(false).write(true).mode(0o600).open(address.with_file_name("scrpad.lock"))?;
    rustix::fs::flock(&file, rustix::fs::FlockOperation::LockExclusive)?;
    Ok(file)
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
    // The owner is the user, which a launch checks (`connect`): left out, an elevated scrpad
    // would make it the Administrators group. Then a protected DACL with one entry: generic all
    // for OWNER RIGHTS.
    let user = sid_string(&pipe::user()?).ok_or_else(|| io::Error::other("the user's SID is malformed"))?;
    let sddl = widestring::U16CString::from_str(format!("O:{user}D:P(A;;GA;;;OW)")).map_err(io::Error::other)?;
    let owner_only = SecurityDescriptor::deserialize(&sddl)?;
    ListenerOptions::new().name(name).security_descriptor(owner_only).create_sync()
}

/// The Windows calls behind the pipe's owner check: who owns a pipe, and who this process runs as.
/// Both give a SID as its bytes, which `check_owner` compares (as EqualSid does).
#[cfg(windows)]
mod pipe {
    use std::io;
    use std::os::windows::io::{AsRawHandle, BorrowedHandle, FromRawHandle, OwnedHandle};
    use std::ptr::null_mut;

    use windows_sys::Win32::Foundation::{ERROR_SUCCESS, LocalFree};
    use windows_sys::Win32::Security::Authorization::{GetSecurityInfo, SE_KERNEL_OBJECT};
    use windows_sys::Win32::Security::{
        GetLengthSid, GetTokenInformation, OWNER_SECURITY_INFORMATION, PSECURITY_DESCRIPTOR, PSID, TOKEN_QUERY, TOKEN_USER, TokenUser,
    };
    use windows_sys::Win32::System::Threading::{GetCurrentProcess, OpenProcessToken};

    /// The SID that owns the pipe; empty if it has no owner.
    pub fn owner(pipe: BorrowedHandle<'_>) -> io::Result<Vec<u8>> {
        let (mut owner, mut descriptor): (PSID, PSECURITY_DESCRIPTOR) = (null_mut(), null_mut());
        // SAFETY: `pipe` is open for the call (opened with read access, which includes reading its
        // owner), and every out pointer is either valid for a write or null where it may be.
        let status = unsafe {
            GetSecurityInfo(
                pipe.as_raw_handle(),
                SE_KERNEL_OBJECT,
                OWNER_SECURITY_INFORMATION,
                &mut owner,
                null_mut(),
                null_mut(),
                null_mut(),
                &mut descriptor,
            )
        };
        if status != ERROR_SUCCESS {
            // Win32 error codes are far below i32::MAX.
            return Err(io::Error::from_raw_os_error(i32::try_from(status).unwrap_or(i32::MAX)));
        }
        // SAFETY: on success `owner` is null or a valid SID inside `descriptor`, not yet freed.
        let owner = if owner.is_null() { Vec::new() } else { unsafe { sid_bytes(owner) } };
        // SAFETY: GetSecurityInfo allocated `descriptor` for us to free with LocalFree, and
        // nothing points into it any more: `owner` was copied out.
        unsafe { LocalFree(descriptor) };
        Ok(owner)
    }

    /// The SID of the user this process runs as.
    pub fn user() -> io::Result<Vec<u8>> {
        // A TOKEN_USER and the SID it points to, at most 68 bytes; u64s align the pointer in it.
        const LEN: u32 = 128;
        let mut token = null_mut();
        // SAFETY: GetCurrentProcess gives a pseudo handle, valid always and never closed;
        // `token` is valid for a write.
        if unsafe { OpenProcessToken(GetCurrentProcess(), TOKEN_QUERY, &mut token) } == 0 {
            return Err(io::Error::last_os_error());
        }
        // SAFETY: OpenProcessToken succeeded, so `token` is an open handle that only we own.
        let token = unsafe { OwnedHandle::from_raw_handle(token) };
        let mut buffer = [0u64; LEN as usize / 8];
        let mut len = 0;
        // SAFETY: `token` is open with TOKEN_QUERY, `buffer` is valid for writes of LEN bytes and
        // aligned for TOKEN_USER, and `len` is valid for a write.
        if unsafe { GetTokenInformation(token.as_raw_handle(), TokenUser, buffer.as_mut_ptr().cast(), LEN, &mut len) } == 0 {
            return Err(io::Error::last_os_error());
        }
        // SAFETY: on success `buffer` starts with a TOKEN_USER whose SID is valid and lies in
        // `buffer`, which lives until the bytes are copied out.
        Ok(unsafe { sid_bytes(buffer.as_ptr().cast::<TOKEN_USER>().read().User.Sid) })
    }

    /// Copies out the bytes of the SID at `sid`.
    ///
    /// # Safety
    /// `sid` points to a valid SID that stays allocated during the call.
    unsafe fn sid_bytes(sid: PSID) -> Vec<u8> {
        // SAFETY: the caller promises a valid SID; GetLengthSid gives its length in bytes.
        unsafe { std::slice::from_raw_parts(sid.cast::<u8>(), GetLengthSid(sid) as usize).to_vec() }
    }
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
fn send(stream: &Client, command: &Command) -> io::Result<Reply> {
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
        Reply::Text(text) | Reply::Warned { text, .. } if text.len() > MAX_TEXT => Reply::Error("the text is larger than 16 MiB".into()),
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
/// The server's `Stream` does overlapped I/O, which `CancelIoEx` on its handle cancels; the
/// client's `File` does synchronous I/O, which `CancelSynchronousIo` on this thread cancels.
/// Both are called each time, for either end.
#[cfg(windows)]
fn within<T>(stream: &impl AsPipe, limit: Duration, exchange: impl FnOnce() -> T) -> T {
    use std::os::windows::io::{AsRawHandle, FromRawHandle, OwnedHandle};
    use std::sync::mpsc;
    use windows_sys::Win32::System::IO::{CancelIoEx, CancelSynchronousIo};
    use windows_sys::Win32::System::Threading::{GetCurrentThreadId, OpenThread, THREAD_TERMINATE};
    let handle = stream.pipe().as_raw_handle() as usize;
    // SAFETY: OpenThread takes no pointers; this thread's ID is valid while it runs.
    let thread = unsafe { OpenThread(THREAD_TERMINATE, 0, GetCurrentThreadId()) };
    // SAFETY: a handle OpenThread returned is open and only we own it; null is checked first.
    let owned = (!thread.is_null()).then(|| unsafe { OwnedHandle::from_raw_handle(thread) });
    let thread = owned.as_ref().map(|thread| thread.as_raw_handle() as usize);
    let (done, finished) = mpsc::channel::<()>();
    // The scope waits for the canceller before this thread does anything else, so it never
    // cancels I/O beyond `exchange`.
    std::thread::scope(|scope| {
        scope.spawn(move || {
            let mut wait = limit;
            while finished.recv_timeout(wait) == Err(mpsc::RecvTimeoutError::Timeout) {
                // SAFETY: `stream` outlives the scope, so the handle stays open while this runs.
                unsafe { CancelIoEx(handle as _, std::ptr::null()) };
                if let Some(thread) = thread {
                    // SAFETY: the thread handle is owned outside the scope, so it stays open, and
                    // it was opened with THREAD_TERMINATE, which CancelSynchronousIo needs.
                    unsafe { CancelSynchronousIo(thread as _) };
                }
                wait = Duration::from_millis(50);
            }
        });
        let result = exchange();
        drop(done);
        result
    })
}

/// The pipe handle under either end: the server's `Stream`, the client's `File`.
#[cfg(windows)]
trait AsPipe {
    fn pipe(&self) -> std::os::windows::io::BorrowedHandle<'_>;
}

#[cfg(windows)]
impl AsPipe for Stream {
    fn pipe(&self) -> std::os::windows::io::BorrowedHandle<'_> {
        use std::os::windows::io::AsHandle;
        let Stream::NamedPipe(pipe) = self;
        pipe.as_handle()
    }
}

#[cfg(windows)]
impl AsPipe for std::fs::File {
    fn pipe(&self) -> std::os::windows::io::BorrowedHandle<'_> {
        std::os::windows::io::AsHandle::as_handle(self)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::mpsc;

    #[cfg(unix)]
    fn temp_socket(name: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("scrpad-test-{}-{name}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("scrpad.sock");
        let _ = std::fs::remove_file(&path);
        path
    }

    /// A pipe of this user's: the tests that claim one pass its owner check (`connect`).
    #[cfg(windows)]
    fn temp_socket(name: &str) -> PathBuf {
        PathBuf::from(format!(r"\\.\pipe\scrpad-test-{}-{name}", std::process::id()))
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
        for reply in [
            Reply::Ok,
            Reply::Busy,
            Reply::Text(String::new()),
            Reply::Text("a\nb".into()),
            Reply::Warned { text: "a\nb".into(), warning: "scrpad: warning: x".into() },
            Reply::Error("“X” is encrypted".into()),
        ] {
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

    #[test]
    fn a_long_warning_is_cut_to_fit_its_line() {
        // The longest text length leaves the least room for the warning.
        let text = "x".repeat(MAX_TEXT);
        for len in [1000, 1007, 1008, 1009, 1010, 1020, 1100] {
            let reply = Reply::Warned { text: text.clone(), warning: "w".repeat(len) };
            let encoded = encode_reply(&reply);
            let line = encoded.iter().position(|&b| b == b'\n').unwrap() + 1;
            assert!(line as u64 <= MAX_REPLY_LINE, "{len}: a line of {line} bytes");
            let Some(Reply::Warned { text: got, warning }) = decode_reply(&mut &encoded[..]).unwrap() else { panic!("{len}: not warned") };
            assert_eq!(got.len(), text.len());
            assert!(warning == "w".repeat(len) || (warning.ends_with('…') && warning.len() < len + 3), "{len}: {}", warning.len());
        }
    }

    #[test]
    fn only_a_pipe_of_this_user_is_talked_to() {
        let user = [1, 2, 0, 0, 0, 0, 0, 5, 21, 0, 0, 0, 0xe9, 3, 0, 0];
        assert!(check_owner(&user, &user).is_ok());
        let mut other = user;
        other[12] = 0xea;
        for owner in [&other[..], &user[..15], &[]] {
            let e = check_owner(owner, &user).unwrap_err();
            assert_eq!((e.kind(), e.to_string()), (io::ErrorKind::PermissionDenied, NOT_OURS.to_owned()));
        }
    }

    #[test]
    fn sids_read_as_windows_writes_them() {
        // A domain user, BUILTIN\Administrators, Everyone, and a 48-bit authority.
        let user = [
            &[1, 5, 0, 0, 0, 0, 0, 5][..],
            &21u32.to_le_bytes(),
            &1_004_336_348u32.to_le_bytes(),
            &1_177_238_915u32.to_le_bytes(),
            &682_003_330u32.to_le_bytes(),
            &1001u32.to_le_bytes(),
        ]
        .concat();
        assert_eq!(sid_string(&user).as_deref(), Some("S-1-5-21-1004336348-1177238915-682003330-1001"));
        assert_eq!(sid_string(&[1, 2, 0, 0, 0, 0, 0, 5, 32, 0, 0, 0, 0x20, 2, 0, 0]).as_deref(), Some("S-1-5-32-544"));
        assert_eq!(sid_string(&[1, 1, 0, 0, 0, 0, 0, 1, 0, 0, 0, 0]).as_deref(), Some("S-1-1-0"));
        assert_eq!(sid_string(&[1, 0, 0, 1, 0, 0, 0, 0]).as_deref(), Some("S-1-0x000100000000"));
        // Cut short, or longer than its count says.
        for bad in [&[][..], &[1, 1, 0, 0, 0, 0, 0, 1, 0, 0, 0], &[1, 0, 0, 0, 0, 0, 0, 1, 0, 0, 0, 0], &[1, 1, 0, 0]] {
            assert_eq!(sid_string(bad), None, "{bad:?}");
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

    #[cfg(unix)]
    #[test]
    fn launches_racing_for_a_stale_socket_lose_no_append() {
        const N: usize = 8;
        let path = temp_socket("race");
        let notes = path.with_file_name("notes.json");
        crate::store::State::default().save(&notes).unwrap();
        drop(std::os::unix::net::UnixListener::bind(&path).unwrap());
        let launches: Vec<_> = (0..N)
            .map(|i| {
                let (path, notes) = (path.clone(), notes.clone());
                std::thread::spawn(move || {
                    // What a windowless `--append` does (src/launch.rs): claim, then work under `hold`.
                    let command = append("Agent", &format!("line {i}"));
                    let Claim::Listening(listener) = claim(&path, &command).unwrap() else { panic!("nobody serves here") };
                    let reply = hold(listener, &path, || crate::scripts::answer(Some(&notes), &command, None));
                    assert_eq!(reply, Reply::Ok);
                })
            })
            .collect();
        for launch in launches {
            launch.join().unwrap();
        }
        let state = crate::store::State::load(&notes).unwrap();
        let text = &state.notes.iter().find(|n| n.title == "Agent").expect("the note").text;
        for i in 0..N {
            assert!(text.contains(&format!("line {i}")), "line {i} was lost: {text:?}");
        }
        let _ = std::fs::remove_dir_all(path.parent().unwrap());
    }

    #[test]
    fn a_server_that_never_answers_does_not_hold_a_launch() {
        let path = temp_socket("hung");
        let listener = listen(path.as_path().to_fs_name::<GenericFilePath>().unwrap()).unwrap();
        let (hang_up, held) = mpsc::channel::<()>();
        // Takes the connection, then neither reads nor writes until the test is done.
        std::thread::spawn(move || {
            let _stream = listener.accept();
            let _ = held.recv();
        });
        let started = Instant::now();
        assert!(matches!(claim(&path, &Command::Show).unwrap(), Claim::Refused(_)));
        assert!(started.elapsed() < ANSWER_TIMEOUT + Duration::from_secs(3), "{:?}", started.elapsed());
        drop(hang_up);
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
