//! The remote side: sends one request over the forwarded socket and returns the answer.

use std::fs;
use std::io;
use std::os::unix::net::UnixStream;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::Duration;

use thiserror::Error;
use zeroize::Zeroizing;

use crate::protocol::{self, ProtocolError, Request, Response};

/// Why `ask` has no password to print.
#[derive(Debug, Error)]
#[non_exhaustive]
pub enum AskError {
    /// No socket exists: the ssh connection does not forward one.
    #[error("the relay is not running: no socket at {0} (is the ssh connection forwarding it?)")]
    NoSocket(PathBuf),
    /// A socket file exists, but nothing listens on it. It was removed so the next ssh
    /// connection can forward a live one.
    #[error(
        "stale relay socket at {0} was removed; reconnect (`ssh -O exit <host>` closes a shared \
         connection) and retry"
    )]
    StaleSocket(PathBuf),
    /// The socket could not be reached.
    #[error("cannot reach the relay at {path}: {source}")]
    Connect {
        /// The socket path.
        path: PathBuf,
        /// The cause.
        source: io::Error,
    },
    /// The relay closed the connection or sent a malformed answer.
    #[error("the relay did not answer (is `askpass-relay serve` running on the desktop?): {0}")]
    Protocol(#[from] ProtocolError),
    /// The user cancelled the dialog, or it timed out.
    #[error("the password dialog was cancelled or timed out")]
    Cancelled,
    /// The relay could not show a dialog.
    #[error("the relay could not ask: {0}")]
    Failed(String),
}

/// Sends `request` to the relay at `socket` and waits at most `timeout` for the password.
///
/// # Errors
///
/// See [`AskError`]: every path that does not end with a password fails at once or at the
/// deadline, never hangs.
pub fn ask(
    socket: &Path,
    request: &Request,
    timeout: Duration,
) -> Result<Zeroizing<Vec<u8>>, AskError> {
    let mut stream = match UnixStream::connect(socket) {
        Ok(stream) => stream,
        Err(error) if error.kind() == io::ErrorKind::NotFound => {
            return Err(AskError::NoSocket(socket.to_path_buf()));
        }
        Err(error) if error.kind() == io::ErrorKind::ConnectionRefused => {
            // The ssh connection that bound it is gone; sshd leaves the file behind.
            let _ = fs::remove_file(socket);
            return Err(AskError::StaleSocket(socket.to_path_buf()));
        }
        Err(source) => {
            return Err(AskError::Connect {
                path: socket.to_path_buf(),
                source,
            });
        }
    };
    stream
        .set_read_timeout(Some(timeout))
        .and_then(|()| stream.set_write_timeout(Some(timeout)))
        .map_err(ProtocolError::Io)?;
    protocol::write_request(&mut stream, request)?;
    match protocol::read_response(&mut stream)? {
        Response::Password(password) => Ok(password),
        Response::Cancelled => Err(AskError::Cancelled),
        Response::Failed(reason) => Err(AskError::Failed(reason)),
    }
}

/// The command line of process `pid`, the sudo that runs this askpass program.
#[must_use]
pub fn command_line(pid: u32) -> String {
    if let Ok(raw) = fs::read(format!("/proc/{pid}/cmdline")) {
        let args: Vec<_> = raw
            .split(|&b| b == 0)
            .filter(|arg| !arg.is_empty())
            .map(String::from_utf8_lossy)
            .collect();
        if !args.is_empty() {
            return args.join(" ");
        }
    }
    // macOS has no procfs; `ps` reads the same arguments through sysctl.
    run(Command::new("ps").args(["-o", "args=", "-p", &pid.to_string()]))
        .unwrap_or_else(|| format!("pid {pid}"))
}

/// The name of this host.
#[must_use]
pub fn host_name() -> String {
    run(Command::new("uname").arg("-n")).unwrap_or_else(|| "unknown host".into())
}

/// The name of the user this program runs as.
#[must_use]
pub fn user_name() -> String {
    std::env::var("USER")
        .or_else(|_| std::env::var("LOGNAME"))
        .ok()
        .filter(|name| !name.is_empty())
        .or_else(|| run(Command::new("id").arg("-un")))
        .unwrap_or_else(|| "unknown user".into())
}

fn run(command: &mut Command) -> Option<String> {
    let output = command.output().ok()?;
    let text = String::from_utf8_lossy(&output.stdout).trim().to_owned();
    (output.status.success() && !text.is_empty()).then_some(text)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn command_line_lists_the_program_and_its_arguments() {
        let line = command_line(std::process::id());
        let me = std::env::current_exe().unwrap();
        let me = me.file_name().unwrap().to_str().unwrap();
        assert!(line.contains(me), "{line}");
    }

    #[test]
    fn host_and_user_are_known() {
        assert!(!host_name().is_empty());
        assert!(!user_name().is_empty());
    }
}
