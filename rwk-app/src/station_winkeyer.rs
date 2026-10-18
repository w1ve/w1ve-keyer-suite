//! The Station's logger-WinKeyer input.
//!
//! The implementation is shared with the Player and the stand-alone W1VE Keyer, in
//! `rwk_core::engine::logger_host`. The Station keys its radio through the arbiter's local source,
//! so the logger's CW takes priority over remote replay while it plays.

pub use rwk_core::engine::logger_host::{HostReport, HostReporter, LoggerHost as StationWinKeyerHost};
