// Prevent a second console window on Windows release builds.
#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

//! Entry point for the W1VE Remote Keyer **Server** (remote radio site).

fn main() {
    wrk_app::run_server();
}
