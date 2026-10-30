//! The local side: listens on a private unix socket and answers each request with a dialog.

use std::fs::{self, DirBuilder};
use std::io::{self, Write};
use std::os::unix::fs::{DirBuilderExt, PermissionsExt};
use std::os::unix::net::{UnixListener, UnixStream};
use std::path::{Path, PathBuf};
use std::time::Duration;

use thiserror::Error;

use crate::assuan::{Outcome, PromptText};
use crate::dialog::Dialog;
use crate::protocol::{self, Request, Response};

/// Writes a log line to stderr (the journal under systemd). A closed stderr does not stop the
/// relay, so the write result is ignored.
macro_rules! log {
    ($($arg:tt)*) => {{
        let _ = writeln!(io::stderr(), $($arg)*);
    }};
}

/// How long a connected client may take to send its request.
const REQUEST_READ_TIMEOUT: Duration = Duration::from_secs(10);
/// The longest command line the dialog shows.
const MAX_COMMAND_CHARS: usize = 400;
/// The longest reason the dialog shows.
const MAX_REASON_CHARS: usize = 200;

/// The relay could not start.
#[derive(Debug, Error)]
#[non_exhaustive]
pub enum ServeError {
    /// Another relay answers on the socket.
    #[error("a relay is already listening on {0}")]
    AlreadyRunning(PathBuf),
    /// The socket or its directory could not be prepared.
    #[error("cannot prepare {path}: {source}")]
    Prepare {
        /// The path that failed.
        path: PathBuf,
        /// The cause.
        source: io::Error,
    },
}

/// Binds the relay socket at `path`: its directory is created with mode 0700, a stale socket
/// left by a previous run is removed, and the socket gets mode 0600.
///
/// # Errors
///
/// Fails when another relay answers on `path` or the filesystem refuses.
pub fn bind(path: &Path) -> Result<UnixListener, ServeError> {
    if let Some(dir) = path.parent() {
        DirBuilder::new()
            .recursive(true)
            .mode(0o700)
            .create(dir)
            .map_err(prepare(dir))?;
        fs::set_permissions(dir, fs::Permissions::from_mode(0o700)).map_err(prepare(dir))?;
    }
    if path.exists() {
        if UnixStream::connect(path).is_ok() {
            return Err(ServeError::AlreadyRunning(path.to_path_buf()));
        }
        fs::remove_file(path).map_err(prepare(path))?;
    }
    let listener = UnixListener::bind(path).map_err(prepare(path))?;
    fs::set_permissions(path, fs::Permissions::from_mode(0o600)).map_err(prepare(path))?;
    Ok(listener)
}

fn prepare(path: &Path) -> impl FnOnce(io::Error) -> ServeError + '_ {
    move |source| ServeError::Prepare {
        path: path.to_path_buf(),
        source,
    }
}

/// Answers requests one at a time, forever. A failed request is logged and does not stop the
/// relay.
pub fn serve(listener: &UnixListener, dialog: &Dialog, timeout: Duration) {
    for stream in listener.incoming() {
        match stream {
            Ok(stream) => handle(stream, dialog, timeout),
            Err(error) => log!("askpass-relay: accept failed: {error}"),
        }
    }
}

fn handle(mut stream: UnixStream, dialog: &Dialog, timeout: Duration) {
    let _ = stream.set_read_timeout(Some(REQUEST_READ_TIMEOUT));
    let response = match protocol::read_request(&mut stream) {
        Ok(request) => answer(&request, dialog, timeout),
        Err(error) => {
            log!("askpass-relay: rejected a malformed request: {error}");
            Response::Failed(format!("malformed request: {error}"))
        }
    };
    if let Err(error) = protocol::write_response(&mut stream, &response) {
        log!("askpass-relay: could not send the answer: {error}");
    }
}

fn answer(request: &Request, dialog: &Dialog, timeout: Duration) -> Response {
    let who = format!("{}@{}", request.user, request.host);
    log!("askpass-relay: {who} asks for `{}`", request.command);
    match dialog.ask(&describe(request), timeout) {
        Ok(Outcome::Password(password)) => {
            log!("askpass-relay: {who}: approved");
            Response::Password(password)
        }
        Ok(Outcome::Cancelled) => {
            log!("askpass-relay: {who}: cancelled");
            Response::Cancelled
        }
        Ok(Outcome::TimedOut) => {
            log!(
                "askpass-relay: {who}: timed out after {}s",
                timeout.as_secs()
            );
            Response::Cancelled
        }
        Err(error) => {
            log!("askpass-relay: {who}: {error}");
            Response::Failed(error.to_string())
        }
    }
}

/// The dialog texts for a request: the account, the host, the command and the caller's reason.
#[must_use]
pub fn describe(request: &Request) -> PromptText {
    let Request {
        host, user, reason, ..
    } = request;
    let command = shorten(&request.command, MAX_COMMAND_CHARS);
    let mut description =
        format!("sudo on {host} asks for the password of {user} to run:\n\n{command}");
    // The reason comes after the command and on one line, so it cannot push the command out of
    // sight or pose as part of it.
    if let Some(reason) = reason {
        let one_line: String = reason
            .chars()
            .map(|ch| if ch.is_control() { ' ' } else { ch })
            .collect();
        description.push_str("\n\nReason: ");
        description.push_str(&shorten(&one_line, MAX_REASON_CHARS));
    }
    PromptText {
        title: format!("sudo password for {user}@{host}"),
        description,
        prompt: format!("Password of {user}@{host}:"),
    }
}

fn shorten(text: &str, max_chars: usize) -> String {
    let mut short: String = text.chars().take(max_chars).collect();
    if short.len() < text.len() {
        short.push_str(" ...");
    }
    short
}

#[cfg(test)]
mod tests {
    use super::*;

    fn request(reason: Option<&str>) -> Request {
        Request {
            host: "air".into(),
            user: "alice".into(),
            prompt: "Password: ".into(),
            command: "sudo -A true".into(),
            reason: reason.map(Into::into),
        }
    }

    #[test]
    fn without_a_reason_the_description_names_the_host_and_the_command() {
        let text = describe(&request(None));
        assert_eq!(text.title, "sudo password for alice@air");
        assert_eq!(
            text.description,
            "sudo on air asks for the password of alice to run:\n\nsudo -A true"
        );
        assert_eq!(text.prompt, "Password of alice@air:");
    }

    #[test]
    fn the_reason_follows_the_command() {
        let text = describe(&request(Some("install docker for the live stands")));
        assert_eq!(text.title, "sudo password for alice@air");
        assert_eq!(
            text.description,
            "sudo on air asks for the password of alice to run:\n\nsudo -A true\n\n\
             Reason: install docker for the live stands"
        );
    }

    #[test]
    fn the_reason_is_one_short_line() {
        let reason = format!("a\n\n\nb\r\tc{}", "y".repeat(1000));
        let text = describe(&request(Some(&reason)));
        let shown = text.description.split("Reason: ").nth(1).unwrap();
        assert!(shown.starts_with("a   b  c"), "{shown}");
        assert!(!shown.contains(&"y".repeat(MAX_REASON_CHARS)), "{shown}");
        assert!(shown.ends_with(" ..."), "{shown}");
    }

    #[test]
    fn long_command_is_shortened() {
        let text = describe(&Request {
            host: "h".into(),
            user: "u".into(),
            prompt: String::new(),
            command: "x".repeat(1000),
            reason: None,
        });
        assert!(
            text.description
                .contains(&format!("{} ...", "x".repeat(MAX_COMMAND_CHARS)))
        );
        assert!(
            !text
                .description
                .contains(&"x".repeat(MAX_COMMAND_CHARS + 1))
        );
    }
}
