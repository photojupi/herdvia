pub mod agent;
#[cfg(any(windows, test))]
mod autostart_logic;
pub mod control;
pub mod doctor;
pub mod exec;
pub mod frame;
pub mod ipc;
#[cfg(unix)]
pub mod launchd;
#[cfg(windows)]
#[path = "autostart_windows.rs"]
pub mod launchd;
mod log;
pub mod network;
pub mod pair;
pub mod peers;
pub mod template;
pub mod via;
