#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

fn main() {
    if let Err(e) = fastpin::run() {
        eprintln!("{e}");
        std::process::exit(1);
    }
}
