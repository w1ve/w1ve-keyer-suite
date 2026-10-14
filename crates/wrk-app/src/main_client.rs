// Prevent a second console window on Windows release builds.
#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

//! Entry point for the W1VE Remote Keyer **Client** (operating position).

fn main() {
    wrk_app::run_client();
}
