//! The stand-alone W1VE Keyer's logger-WinKeyer input.
//!
//! The implementation is shared with the Remote Keyer's Player and Station, in
//! `rwk_core::engine::logger_host`. CW from the logger keys through the arbiter's local source,
//! alongside the operator's paddle on the same output line.

pub use rwk_core::engine::logger_host::{HostReport, HostReporter, LoggerHost as WinKeyerHost};
