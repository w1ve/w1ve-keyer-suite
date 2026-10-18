// Prevent a second console window on Windows release builds.
#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

//! Entry point for the standalone **W1VE Keyer** — a software WinKeyer.

fn main() {
    wk_app::run();
}
