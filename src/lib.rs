//! Find existing tmux sessions across your computers and attach through SSH.

mod ansi;
mod app;
mod cancel;
mod crt;
mod display;
mod embedded;
mod exit_codes;
mod files;
mod options;
mod process;
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
