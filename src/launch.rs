//! What a launch does, shared by the binaries: `scrpad` (`scrpad.exe` on Windows) and, on
//! Windows, the console build `scrpad.com`, which shows terminals the output and exit codes.

/// Set at runtime because eframe otherwise puts its own egui icon on the window, the Windows
/// taskbar and the macOS Dock and Cmd-Tab switcher, over the one in the .app bundle.
/// macOS gets the icon with the padding of Apple's icon grid, elsewhere it fills its square.
#[cfg(target_os = "macos")]
const ICON: &[u8] = include_bytes!("../assets/app-icon/scrpad.png");
#[cfg(not(target_os = "macos"))]
const ICON: &[u8] = include_bytes!("../assets/app-icon/scrpad-256.png");

use crate::instance::{Claim, Reply};

/// Where a launch that is no script command opens the window.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Window {
    /// In this process: `scrpad`, and `scrpad.exe` on Windows.
    Here,
    /// In `scrpad.exe` next to this program, started on its own so that the terminal is free
    /// again at once, as after starting any window app: `scrpad.com`, the console build.
    Detached,
}

/// Runs the command line; the window, too, with `Window::Here`, and the terminal UI either way.
pub fn run(window: Window) -> eframe::Result {
    let (command, tui) = match crate::cli::parse(std::env::args().skip(1), std::io::stdin()) {
        Ok(crate::cli::Parsed::Launch(launch)) => {
            let tui = launch.tui || !launch.command.is_script() && no_display();
            (launch.command, tui)
        }
        Ok(crate::cli::Parsed::Print(text)) => {
            print!("{text}");
            return Ok(());
        }
        Err(error) => {
            eprintln!("scrpad: {error}");
            std::process::exit(2);
        }
    };
    // A running scrpad takes over a second launch (and `--toggle` from a compositor key).
    let address = crate::instance::address();
    let listener = match &address {
        Some(address) => match crate::instance::claim(address, &command) {
            Ok(Claim::Answered(Reply::Ok)) if tui => {
                // Two of them would save over each other's notes.
                eprintln!("scrpad: another scrpad (a window, or a terminal) has the notes; close it to open them here");
                std::process::exit(1);
            }
            Ok(Claim::Answered(reply)) => std::process::exit(match outcome(&command, reply) {
                Outcome::Print(reply) => finish(reply),
                Outcome::Run { argv, env } => crate::handoff::run_child(&argv, &env),
            }),
            Ok(Claim::Refused(error)) => {
                // Starting a second instance would have two windows overwrite each other's notes.
                eprintln!("scrpad: {error}");
                std::process::exit(1);
            }
            Ok(Claim::Listening(listener)) => Some(listener),
            Err(error) => {
                eprintln!("scrpad: {}: {error}", address.display());
                None
            }
        },
        None => None,
    };
    if command.is_script() {
        // No window runs: the notes file is changed or read right here, and no window starts.
        let reply = match listener {
            // Held until done, answering `busy`: a scrpad started meanwhile waits, then loads the
            // file with the append in it, instead of loading it before and saving over it later.
            Some(listener) => crate::instance::hold(listener, address.as_deref().expect("a listener has an address"), || {
                crate::scripts::answer(crate::store::State::default_path().as_deref(), &command, None)
            }),
            // Without the socket a running window can't be told from none, and would save over the append.
            None if matches!(command, crate::cli::Command::Append { .. }) => {
                Reply::Error("no socket to reach a running scrpad, so nothing was appended (a window could save over it)".into())
            }
            None => crate::scripts::answer(crate::store::State::default_path().as_deref(), &command, None),
        };
        std::process::exit(finish(said(&command, reply)));
    }
    if tui {
        let (state, path, startup_error) = load();
        let note = match command {
            crate::cli::Command::Note(text) => Some(text),
            _ => None,
        };
        if let Err(error) = crate::tui::run(state, path, startup_error, note, listener) {
            eprintln!("scrpad: {error}");
            std::process::exit(1);
        }
        return Ok(());
    }
    if window == Window::Detached {
        // The window's scrpad claims the socket itself; holding it here would turn it away.
        drop(listener);
        if let Err(error) = open_detached(&command) {
            eprintln!("scrpad: could not start the window: {error}");
            std::process::exit(1);
        }
        return Ok(());
    }
    let (state, path, startup_error) = load();
    let mut viewport = egui::ViewportBuilder::default()
        .with_title("Scratchpad")
        .with_app_id("scrpad")
        .with_icon(eframe::icon_data::from_png_bytes(ICON).expect("the embedded icon is a valid PNG"))
        .with_decorations(false)
        .with_transparent(true)
        .with_inner_size(state.window_size.unwrap_or([380.0, 300.0]))
        .with_min_inner_size([260.0, 170.0]);
    if let Some(pos) = state.window_pos {
        viewport = viewport.with_position(pos);
    }
    if state.pinned {
        viewport = viewport.with_always_on_top();
    }
    let options = eframe::NativeOptions { viewport, ..Default::default() };
    eframe::run_native(
        "Scratchpad",
        options,
        Box::new(move |cc| {
            let mut app = crate::App::new(cc, state, path, startup_error);
            if let crate::cli::Command::Note(text) = command {
                app.session.add_note_with_text(text, 0.0);
            }
            if let Some(listener) = listener {
                app.serve_commands(&cc.egui_ctx, listener);
            }
            Ok(Box::new(app))
        }),
    )
}

/// The notes from the notes file, where they are saved, and why saving is off if it is.
fn load() -> (crate::store::State, Option<std::path::PathBuf>, Option<String>) {
    match crate::store::State::default_path() {
        Some(path) => match crate::store::State::load(&path) {
            Ok(state) => (state, Some(path), None),
            Err(error) => {
                eprintln!("scrpad: {error}");
                (
                    crate::store::State::default(),
                    None,
                    Some(format!("Could not preserve the unreadable notes file: {error}. The original was left untouched.")),
                )
            }
        },
        None => (crate::store::State::default(), None, None),
    }
}
/// No display to open a window on (over SSH, say), but a terminal to run in: Linux and the BSDs
/// with neither Wayland nor X11. macOS and Windows always have a display.
fn no_display() -> bool {
    use std::io::IsTerminal;
    cfg!(all(unix, not(target_os = "macos")))
        && ["WAYLAND_DISPLAY", "DISPLAY"].iter().all(|name| std::env::var_os(name).is_none_or(|v| v.is_empty()))
        && std::io::stdin().is_terminal()
        && std::io::stdout().is_terminal()
}

/// Starts `scrpad.exe` (`scrpad` elsewhere) from this program's folder with the same arguments.
/// `-` has read stdin here already, so the text goes on through the new process's stdin.
fn open_detached(command: &crate::cli::Command) -> std::io::Result<()> {
    use std::io::Write;
    use std::process::Stdio;
    let exe = std::env::current_exe()?.with_file_name(format!("scrpad{}", std::env::consts::EXE_SUFFIX));
    let mut child = std::process::Command::new(exe)
        .args(std::env::args_os().skip(1))
        .stdin(Stdio::piped())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()?;
    let mut stdin = child.stdin.take().expect("stdin is piped");
    if let crate::cli::Command::Note(text) = command
        && !text.is_empty()
    {
        // Reading drops one trailing line break; this one, so that the text arrives unchanged.
        stdin.write_all(text.as_bytes())?;
        stdin.write_all(b"\r\n")?;
    }
    Ok(())
}

/// What a launch does with its answer: `--run` runs its command with the text, anything else
/// prints it (`finish`). The text of `--run` is a secret and never reaches `finish`.
enum Outcome {
    Print(Reply),
    Run { argv: Vec<String>, env: Vec<(String, String)> },
}

fn outcome(command: &crate::cli::Command, reply: Reply) -> Outcome {
    match (command, reply) {
        (crate::cli::Command::Run { argv, .. }, Reply::Text(text)) => {
            let text = zeroize::Zeroizing::new(text);
            match crate::handoff::parse_env(&text) {
                Ok(env) => Outcome::Run { argv: argv.clone(), env },
                Err(e) => Outcome::Print(Reply::Error(e)),
            }
        }
        (command, reply) => Outcome::Print(said(command, reply)),
    }
}

/// `--append` says `ok`, so a script or an agent sees it went in; a launch that shows the
/// window stays quiet.
fn said(command: &crate::cli::Command, reply: Reply) -> Reply {
    match (command, reply) {
        (crate::cli::Command::Append { .. }, Reply::Ok) => Reply::Text("ok".into()),
        (_, reply) => reply,
    }
}

/// Prints the answer to a command; the exit code.
fn finish(reply: Reply) -> i32 {
    use std::io::Write;
    match reply {
        Reply::Ok => 0,
        Reply::Text(mut text) => {
            // `scrpad -` drops one trailing newline; `--read` puts it back.
            if !text.is_empty() && !text.ends_with('\n') {
                text.push('\n');
            }
            let mut out = std::io::stdout().lock();
            match out.write_all(text.as_bytes()).and_then(|()| out.flush()) {
                Ok(()) => 0,
                // `scrpad --list | head -1`, say.
                Err(e) if e.kind() == std::io::ErrorKind::BrokenPipe => 0,
                Err(e) => {
                    eprintln!("scrpad: could not write the text: {e}");
                    1
                }
            }
        }
        Reply::Error(message) => {
            eprintln!("scrpad: {message}");
            1
        }
        Reply::Busy => {
            eprintln!("scrpad: another scrpad is busy with the notes file; try again");
            1
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::cli::Command;

    #[test]
    fn the_text_of_run_is_never_printed() {
        let run = Command::Run { title: ".env".into(), argv: vec!["env".into()] };
        match outcome(&run, Reply::Text("TOKEN=sk-live".into())) {
            Outcome::Run { env, .. } => assert_eq!(env, [("TOKEN".to_owned(), "sk-live".to_owned())]),
            Outcome::Print(r) => panic!("would print {r:?}"),
        }
        assert!(matches!(outcome(&run, Reply::Text("sk-live".into())), Outcome::Print(Reply::Error(e)) if !e.contains("sk-live")));
        assert!(matches!(outcome(&Command::List, Reply::Text("x".into())), Outcome::Print(Reply::Text(_))));
    }

    #[test]
    fn an_append_says_ok_and_a_launch_stays_quiet() {
        let append = Command::Append { title: ".env".into(), text: "A=".into() };
        assert!(matches!(outcome(&append, Reply::Ok), Outcome::Print(Reply::Text(t)) if t == "ok"));
        assert!(matches!(outcome(&Command::Show, Reply::Ok), Outcome::Print(Reply::Ok)));
        assert!(matches!(outcome(&Command::Toggle, Reply::Ok), Outcome::Print(Reply::Ok)));
    }
}
