// Prevent a second console window on Windows release builds.
#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

//! Entry point for the W1VE Remote Keyer **Player** (remote radio site), built as
//! `w1ve-remote-keyer-player`.

fn main() {
    wrk_app::run_server();
}
