use std::fmt;
use std::sync::mpsc::{SendError, TryRecvError};

use crate::RunSignal;

pub enum SanityCheckError<'a> {
    DependencyNotFound(&'a str),
    DependencyLoop(Vec<&'a str>),
}

impl fmt::Display for SanityCheckError<'_> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            SanityCheckError::DependencyNotFound(dependency) => {
                write!(f, "Dependency {} not found.", dependency)
            }
            SanityCheckError::DependencyLoop(dependencies) => {
                write!(f, "Dependency loop: [{}]", dependencies.join(", "))
            }
        }
    }
}

pub enum BuildLoopError<'a> {
    BuildFailed(&'a str),
    UnspecifiedChannelError,
    SendError(SendError<RunSignal>),
    RecvError(TryRecvError),
    WatcherSetupError(notify::Error),
    WatcherPathError(&'a str, notify::Error),
    CwdIOError(std::io::Error),
    CwdUtf8Error,
}

impl fmt::Display for BuildLoopError<'_> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            BuildLoopError::BuildFailed(target) => write!(f, "Build failed for target {}", target),
            BuildLoopError::UnspecifiedChannelError => {
                write!(f, "Unknown channel parllelism failure")
            }
            BuildLoopError::SendError(send_err) => write!(
                f,
                "Failed to send run signal '{}' to running process",
                send_err.0
            ),
            BuildLoopError::RecvError(recv_err) => {
                write!(f, "Channel receive failure: {}", recv_err)
            }
            BuildLoopError::WatcherSetupError(notify_err) => {
                write!(f, "Watcher setup error: {}", notify_err)
            }
            BuildLoopError::WatcherPathError(path, notify_err) => {
                write!(f, "File watch error: {}: {}", path, notify_err)
            }
            BuildLoopError::CwdIOError(io_err) => {
                write!(f, "IO Error while getting current directory: {}", io_err)
            }
            BuildLoopError::CwdUtf8Error => write!(f, "Current directory was not valid utf-8"),
        }
    }
}
