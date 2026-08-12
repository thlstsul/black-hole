#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

mod app;
mod args;
mod completion;

use std::process;

fn main() {
    if let Err(e) = app::App::run() {
        eprintln!("Daemon failed: {}", e);
        process::exit(1);
    }
}
