#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

mod app;
mod args;

fn main() {
    if let Err(e) = app::App::run() {
        eprintln!("Daemon failed: {}", e);
        std::process::exit(1);
    }
}
