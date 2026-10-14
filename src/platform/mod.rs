use std::ffi::OsString;

pub mod process;

#[cfg(target_os = "linux")]
mod linux;
#[cfg(target_os = "linux")]
pub use linux::Reactor;
#[cfg(target_os = "macos")]
mod macos;
#[cfg(target_os = "macos")]
pub use macos::Reactor;

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct SourceId(pub u32);

#[derive(Debug, PartialEq, Eq)]
pub enum Wake {
    FileWritten(SourceId),
    FileGone(SourceId),
    DirEntry(SourceId, OsString),
    ProcessExited(SourceId),
    Deadline,
    #[cfg(target_os = "linux")]
    ClockJumped,
}
