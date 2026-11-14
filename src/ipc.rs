#[cfg(any(windows, test))]
mod pipe_name;
#[cfg(any(windows, test))]
mod pipe_state;
#[cfg(unix)]
mod unix;
#[cfg(windows)]
mod windows;

#[cfg(unix)]
pub use unix::*;
#[cfg(windows)]
pub use windows::*;
