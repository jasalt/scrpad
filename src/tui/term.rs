//! Runs the terminal UI in the terminal this process was started from: raw mode on the alternate
//! screen, so note text never stays in the scrollback, and the terminal back as it was on every
//! way out: quitting, an error, a signal, a closed terminal, a panic.

use std::io::{self, Write};
use std::path::PathBuf;
use std::sync::{Arc, Mutex, MutexGuard, PoisonError};
use std::time::{Duration, Instant};

use interprocess::local_socket::Listener;
use ratatui::Terminal;
use ratatui::backend::CrosstermBackend;
use ratatui::crossterm::event::{self, DisableBracketedPaste, EnableBracketedPaste};
use ratatui::crossterm::terminal::{self, EnterAlternateScreen, LeaveAlternateScreen};
use ratatui::crossterm::{cursor, execute};

use super::Tui;
use crate::cli::Command;
use crate::instance::Reply;
use crate::session::{Session, Wake};
use crate::store::State;

/// The longest the loop waits for a key: a note from `scrpad --new` or an append shows by then.
const MAX_WAIT: Duration = Duration::from_millis(250);

/// Opens the notes in this terminal until the user quits; `note` is a note to add first
/// (`--new`, `-`), and `listener` the socket other launches reach this one through.
pub fn run(
    state: State,
    path: Option<PathBuf>,
    startup_error: Option<String>,
    note: Option<String>,
    listener: Option<Listener>,
) -> io::Result<()> {
    // When the session asks to be woken: the loop waits at most until then.
    let due = Arc::new(Mutex::new(None::<Instant>));
    let wake: Wake = {
        let due = due.clone();
        Arc::new(move |delay| {
            let at = Instant::now() + delay;
            let mut due = due.lock().unwrap_or_else(PoisonError::into_inner);
            if due.is_none_or(|d| at < d) {
                *due = Some(at);
            }
        })
    };
    let mut tui = Tui::new(Session::new(state, path, startup_error, wake.clone()));
    if let Some(text) = note {
        tui.session.add_note_with_text(text, 0.0);
    }
    if let Some(listener) = listener {
        serve(&tui, listener, wake);
    }
    // Shared with the thread that saves on a signal or a closed console; the loop holds it
    // only while it handles input and draws, never while it waits.
    let tui = Arc::new(Mutex::new(tui));
    save_when_stopped(&tui)?;
    // Saves on every way out, a panic included, unless the user chose to quit without saving.
    let _save = SaveOnExit(tui.clone());
    drive(&tui, &due)
}

/// The notes saved as on quitting: dropped on the way out of `run`.
struct SaveOnExit(Arc<Mutex<Tui>>);

impl Drop for SaveOnExit {
    fn drop(&mut self) {
        lock(&self.0).finish();
    }
}

fn lock(tui: &Mutex<Tui>) -> MutexGuard<'_, Tui> {
    tui.lock().unwrap_or_else(PoisonError::into_inner)
}

fn drive(shared: &Mutex<Tui>, due: &Mutex<Option<Instant>>) -> io::Result<()> {
    let _screen = Screen::enter()?;
    let mut terminal = Terminal::new(CrosstermBackend::new(io::stdout()))?;
    let start = Instant::now();
    let now = || start.elapsed().as_secs_f64();
    loop {
        let mut tui = lock(shared);
        tui.tick(now());
        if tui.take_redraw() {
            terminal.clear()?;
        }
        terminal.draw(|frame| tui.draw(frame))?;
        if let Some(sequence) = tui.take_clipboard() {
            let out = terminal.backend_mut();
            out.write_all(sequence.as_bytes())?;
            out.flush()?;
        }
        if tui.quit_requested() {
            return Ok(());
        }
        drop(tui);
        let wait = {
            let mut due = due.lock().unwrap_or_else(PoisonError::into_inner);
            let left = due.map_or(MAX_WAIT, |at| at.saturating_duration_since(Instant::now()));
            if left.is_zero() {
                *due = None;
            }
            left.min(MAX_WAIT)
        };
        if event::poll(wait)? {
            // Everything that arrived, then one frame: a paste or key repeat doesn't draw each key.
            loop {
                let event = event::read()?;
                lock(shared).handle(event, now());
                if !event::poll(Duration::ZERO)? {
                    break;
                }
            }
        }
    }
}

/// Answers other launches like the window does: scripts the same way, `--new` and `-` add a
/// note. `scrpad` and `--toggle` get an ok and change nothing: a terminal can't be raised or
/// hidden from here.
fn serve(tui: &Tui, listener: Listener, wake: Wake) {
    let answer_script = tui.session.script_server();
    let answer_approval = tui.session.approval_server();
    let asks = tui.session.asks();
    let notes = tui.session.note_requests.0.clone();
    crate::instance::serve(listener, move |command| {
        // A terminal can't be raised: the box and the loop's wake-up are all the question gets.
        if asks(&command) {
            return answer_approval(&command);
        }
        if command.is_script() {
            return answer_script(&command);
        }
        if let Command::Note(text) = command {
            let _ = notes.send(text);
            wake(Duration::ZERO);
        }
        Reply::Ok
    });
}

/// The terminal in raw mode on the alternate screen, with bracketed paste (a paste arrives as one
/// event, not as keys that could trigger shortcuts). Dropping it, or a panic, restores it.
struct Screen;

impl Screen {
    fn enter() -> io::Result<Self> {
        terminal::enable_raw_mode()?;
        let screen = Screen;
        execute!(io::stdout(), EnterAlternateScreen, EnableBracketedPaste)?;
        let previous = std::panic::take_hook();
        std::panic::set_hook(Box::new(move |info| {
            // First, so the message lands on the normal screen and not in raw mode.
            leave();
            previous(info);
        }));
        Ok(screen)
    }
}

impl Drop for Screen {
    fn drop(&mut self) {
        leave();
    }
}

/// Puts the terminal back. Errors are ignored: a closed terminal has nothing to restore.
fn leave() {
    let _ = execute!(io::stdout(), DisableBracketedPaste, LeaveAlternateScreen, cursor::Show);
    let _ = terminal::disable_raw_mode();
}

/// Saves on SIGTERM, and on SIGHUP when the terminal closes, then leaves. That happens on a
/// thread of its own: on a closed terminal crossterm keeps reading it and never returns to the
/// loop. Ctrl+C and Ctrl+\ are keys in raw mode, not signals.
#[cfg(unix)]
fn save_when_stopped(tui: &Arc<Mutex<Tui>>) -> io::Result<()> {
    use signal_hook::consts::{SIGHUP, SIGTERM};
    let mut signals = signal_hook::iterator::Signals::new([SIGTERM, SIGHUP])?;
    let tui = tui.clone();
    std::thread::spawn(move || {
        if let Some(signal) = signals.forever().next() {
            lock(&tui).finish();
            leave();
            std::process::exit(128 + signal);
        }
    });
    Ok(())
}

/// Saves when the console window is closed, or the user logs off or shuts down: Windows ends
/// the process once the handler returns (after at most a few seconds), so it saves right there.
#[cfg(windows)]
fn save_when_stopped(tui: &Arc<Mutex<Tui>>) -> io::Result<()> {
    use std::sync::OnceLock;
    use windows_sys::Win32::System::Console::{CTRL_CLOSE_EVENT, CTRL_LOGOFF_EVENT, CTRL_SHUTDOWN_EVENT, SetConsoleCtrlHandler};
    use windows_sys::core::BOOL;
    static SHARED: OnceLock<Arc<Mutex<Tui>>> = OnceLock::new();
    unsafe extern "system" fn handler(kind: u32) -> BOOL {
        if !matches!(kind, CTRL_CLOSE_EVENT | CTRL_LOGOFF_EVENT | CTRL_SHUTDOWN_EVENT) {
            return 0;
        }
        if let Some(tui) = SHARED.get() {
            lock(tui).finish();
        }
        1
    }
    let _ = SHARED.set(tui.clone());
    // SAFETY: registers a function that only saves through the shared, locked `Tui`.
    if unsafe { SetConsoleCtrlHandler(Some(handler), 1) } == 0 {
        return Err(io::Error::last_os_error());
    }
    Ok(())
}
