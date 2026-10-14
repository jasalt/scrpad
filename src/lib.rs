//! Scratchpad: a tiny frameless floating notepad. egui draws every pixel; the OS only gives a window.

pub mod agents;
mod app;
pub mod cli;
pub mod clipboard;
pub mod crypto;
pub mod handoff;
#[cfg(target_os = "linux")]
pub mod hyprland;
pub mod icons;
pub mod instance;
pub mod launch;
pub mod scan;
pub mod scripts;
pub mod session;
pub mod store;
pub mod theme;
pub mod tui;
pub mod update;

pub use app::App;
pub use session::Session;
