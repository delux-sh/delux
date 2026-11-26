//! Browse, create, and attach to tmux sessions across your computers through SSH.

mod agents;
mod ansi;
mod app;
mod cancel;
mod crt;
mod directory_completion;
mod display;
mod embedded;
mod exit_codes;
mod files;
mod lan;
mod launcher;
mod options;
mod process;
mod reporting;
mod session_form;
mod ssh_mux;
mod state;
mod terminal;
mod theme;
mod tmux;
mod tui;
mod tui_backend;
mod workers;

#[cfg(test)]
mod test_support;

pub use app::run;
