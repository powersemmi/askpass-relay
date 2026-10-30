//! The client side of the Assuan conversation with a pinentry program.
//!
//! The conversation is short: the greeting, a few `SET*` commands, `GETPIN`, `BYE`. The PIN
//! arrives percent-encoded on a `D` line; every buffer that holds it is wiped on drop.

use std::fmt;
use std::io::{self, Read, Write};
use std::str;
use std::time::Duration;

use thiserror::Error;
use zeroize::Zeroizing;

/// The Assuan error code for a cancelled operation (`GPG_ERR_CANCELED`).
const ERR_CANCELED: u32 = 99;
/// The Assuan error code for a timed-out operation (`GPG_ERR_TIMEOUT`).
const ERR_TIMEOUT: u32 = 62;
/// The longest line accepted from pinentry. Assuan caps lines at 1000 bytes.
const MAX_LINE: usize = 4096;

/// The texts the dialog shows.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PromptText {
    /// The window title.
    pub title: String,
    /// The paragraph above the entry field.
    pub description: String,
    /// The label of the entry field.
    pub prompt: String,
}

/// How a dialog ended.
#[derive(PartialEq, Eq)]
pub enum Outcome {
    /// The user typed a password.
    Password(Zeroizing<Vec<u8>>),
    /// The user closed or cancelled the dialog.
    Cancelled,
    /// Nobody answered in time.
    TimedOut,
}

impl fmt::Debug for Outcome {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Password(_) => f.write_str("Password(<redacted>)"),
            Self::Cancelled => f.write_str("Cancelled"),
            Self::TimedOut => f.write_str("TimedOut"),
        }
    }
}

/// A pinentry conversation that went wrong.
#[derive(Debug, Error)]
#[non_exhaustive]
pub enum AssuanError {
    /// The pipe to or from pinentry failed.
    #[error("pinentry i/o error: {0}")]
    Io(#[from] io::Error),
    /// Pinentry closed its output before answering.
    #[error("pinentry closed the connection")]
    Closed,
    /// Pinentry answered a command with an error other than cancel or timeout.
    #[error("pinentry refused `{command}`: {message}")]
    Refused {
        /// The command pinentry refused.
        command: &'static str,
        /// The error line pinentry sent.
        message: String,
    },
    /// Pinentry sent a line this client does not understand.
    #[error("unexpected line from pinentry: {0}")]
    Unexpected(String),
}

/// Percent-encodes the characters Assuan reserves in command arguments.
#[must_use]
pub fn escape(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    for ch in text.chars() {
        match ch {
            '%' => out.push_str("%25"),
            '\n' => out.push_str("%0A"),
            '\r' => out.push_str("%0D"),
            other => out.push(other),
        }
    }
    out
}

/// Decodes the percent-encoding of an Assuan data line into a buffer wiped on drop.
///
/// # Errors
///
/// Fails on a `%` not followed by two hex digits.
pub fn unescape(data: &[u8]) -> Result<Zeroizing<Vec<u8>>, AssuanError> {
    let mut out = Zeroizing::new(Vec::with_capacity(data.len()));
    let mut bytes = data.iter();
    while let Some(&byte) = bytes.next() {
        if byte == b'%' {
            let (Some(hi), Some(lo)) = (bytes.next(), bytes.next()) else {
                return Err(AssuanError::Unexpected(
                    "truncated escape in data line".into(),
                ));
            };
            let (Some(hi), Some(lo)) = (hex(*hi), hex(*lo)) else {
                return Err(AssuanError::Unexpected(
                    "invalid escape in data line".into(),
                ));
            };
            out.push((hi << 4) | lo);
        } else {
            out.push(byte);
        }
    }
    Ok(out)
}

const fn hex(digit: u8) -> Option<u8> {
    match digit {
        b'0'..=b'9' => Some(digit - b'0'),
        b'a'..=b'f' => Some(digit - b'a' + 10),
        b'A'..=b'F' => Some(digit - b'A' + 10),
        _ => None,
    }
}

/// Runs the whole conversation over pinentry's stdout (`input`) and stdin (`output`).
///
/// `timeout` is passed to pinentry with `SETTIMEOUT`; the caller still enforces it, because a
/// pinentry may ignore the option.
///
/// # Errors
///
/// Fails when the pipes fail, pinentry closes early, or it refuses a command for a reason other
/// than cancel or timeout.
pub fn converse(
    input: impl Read,
    mut output: impl Write,
    text: &PromptText,
    timeout: Duration,
) -> Result<Outcome, AssuanError> {
    let mut lines = LineReader::new(input);
    expect_ok(&mut lines, "greeting")?;
    let commands = [
        ("SETTITLE", format!("SETTITLE {}", escape(&text.title))),
        ("SETDESC", format!("SETDESC {}", escape(&text.description))),
        ("SETPROMPT", format!("SETPROMPT {}", escape(&text.prompt))),
        ("SETTIMEOUT", format!("SETTIMEOUT {}", timeout.as_secs())),
    ];
    for (name, command) in commands {
        writeln!(output, "{command}")?;
        output.flush()?;
        match expect_ok(&mut lines, name) {
            // An older pinentry may not know SETTIMEOUT; the caller enforces the deadline anyway.
            Err(AssuanError::Refused { .. }) if name == "SETTIMEOUT" => {}
            other => other?,
        }
    }
    writeln!(output, "GETPIN")?;
    output.flush()?;
    let mut pin = None;
    let outcome = loop {
        let line = lines.next_line()?;
        if let Some(data) = line.strip_prefix(b"D ") {
            pin = Some(unescape(data)?);
        } else if line.as_slice() == b"OK" || line.starts_with(b"OK ") {
            break Outcome::Password(pin.take().unwrap_or_default());
        } else if line.starts_with(b"ERR ") {
            break match error_code(&line) {
                Some(ERR_CANCELED) => Outcome::Cancelled,
                Some(ERR_TIMEOUT) => Outcome::TimedOut,
                _ => return Err(refused("GETPIN", &line)),
            };
        } else if !is_status(&line) {
            return Err(AssuanError::Unexpected(printable(&line)));
        }
    };
    // Pinentry exits on BYE; a failure to say goodbye does not change the answer.
    let _ = writeln!(output, "BYE").and_then(|()| output.flush());
    Ok(outcome)
}

fn expect_ok(lines: &mut LineReader<impl Read>, command: &'static str) -> Result<(), AssuanError> {
    loop {
        let line = lines.next_line()?;
        if line.as_slice() == b"OK" || line.starts_with(b"OK ") {
            return Ok(());
        }
        if line.starts_with(b"ERR ") {
            return Err(refused(command, &line));
        }
        if !is_status(&line) {
            return Err(AssuanError::Unexpected(printable(&line)));
        }
    }
}

fn is_status(line: &[u8]) -> bool {
    line.starts_with(b"S ") || line.starts_with(b"#") || line.is_empty()
}

fn error_code(line: &[u8]) -> Option<u32> {
    let rest = line.strip_prefix(b"ERR ")?;
    let code = rest.split(|&b| b == b' ').next()?;
    let code: u32 = str::from_utf8(code).ok()?.parse().ok()?;
    // The low 16 bits are the error code; the high bits name the source.
    Some(code & 0xffff)
}

fn refused(command: &'static str, line: &[u8]) -> AssuanError {
    AssuanError::Refused {
        command,
        message: printable(line),
    }
}

fn printable(line: &[u8]) -> String {
    String::from_utf8_lossy(line).into_owned()
}

/// Splits pinentry's output into lines; every buffer it uses is wiped on drop.
struct LineReader<R> {
    inner: R,
    pending: Zeroizing<Vec<u8>>,
}

impl<R: Read> LineReader<R> {
    fn new(inner: R) -> Self {
        Self {
            inner,
            pending: Zeroizing::new(Vec::with_capacity(MAX_LINE)),
        }
    }

    fn next_line(&mut self) -> Result<Zeroizing<Vec<u8>>, AssuanError> {
        let mut chunk = Zeroizing::new([0_u8; 512]);
        loop {
            if let Some(end) = self.pending.iter().position(|&b| b == b'\n') {
                let mut line = Zeroizing::new(Vec::with_capacity(end));
                line.extend_from_slice(&self.pending[..end]);
                self.pending.drain(..=end);
                return Ok(line);
            }
            if self.pending.len() > MAX_LINE {
                return Err(AssuanError::Unexpected("line too long".into()));
            }
            let read = self.inner.read(chunk.as_mut())?;
            if read == 0 {
                return Err(AssuanError::Closed);
            }
            self.pending.extend_from_slice(&chunk[..read]);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn text() -> PromptText {
        PromptText {
            title: "sudo on pro".into(),
            description: "moloko@pro runs\nsudo -A true (100%)".into(),
            prompt: "Password:".into(),
        }
    }

    fn run(script: &str) -> (Result<Outcome, AssuanError>, String) {
        let mut sent = Vec::new();
        let outcome = converse(
            script.as_bytes(),
            &mut sent,
            &text(),
            Duration::from_secs(60),
        );
        (outcome, String::from_utf8(sent).unwrap())
    }

    #[test]
    fn escape_encodes_reserved_characters() {
        assert_eq!(escape("a%b\nc\rd"), "a%25b%0Ac%0Dd");
    }

    #[test]
    fn unescape_decodes_hex_pairs() {
        assert_eq!(unescape(b"p%25w%0a%0D!").unwrap().as_slice(), b"p%w\n\r!");
    }

    #[test]
    fn unescape_rejects_a_broken_escape() {
        assert!(unescape(b"abc%4").is_err());
        assert!(unescape(b"abc%zz").is_err());
    }

    #[test]
    fn password_is_returned_and_the_dialog_described() {
        let (outcome, sent) =
            run("OK Pleased to meet you\nOK\nOK\nOK\nOK\nS PASSWORD_FROM_CACHE\nD s3cr%25t\nOK\n");
        assert_eq!(
            outcome.unwrap(),
            Outcome::Password(Zeroizing::new(b"s3cr%t".to_vec()))
        );
        assert_eq!(
            sent,
            "SETTITLE sudo on pro\n\
             SETDESC moloko@pro runs%0Asudo -A true (100%25)\n\
             SETPROMPT Password:\n\
             SETTIMEOUT 60\n\
             GETPIN\n\
             BYE\n"
        );
    }

    #[test]
    fn empty_password_is_a_password() {
        let (outcome, _) = run("OK\nOK\nOK\nOK\nOK\nOK\n");
        assert_eq!(
            outcome.unwrap(),
            Outcome::Password(Zeroizing::new(Vec::new()))
        );
    }

    #[test]
    fn cancel_is_reported_as_cancelled() {
        let (outcome, _) = run("OK\nOK\nOK\nOK\nOK\nERR 83886179 Operation cancelled <Pinentry>\n");
        assert_eq!(outcome.unwrap(), Outcome::Cancelled);
    }

    #[test]
    fn pinentry_timeout_is_reported_as_timed_out() {
        let (outcome, _) = run("OK\nOK\nOK\nOK\nOK\nERR 83886142 Timeout <Pinentry>\n");
        assert_eq!(outcome.unwrap(), Outcome::TimedOut);
    }

    #[test]
    fn unknown_settimeout_is_tolerated() {
        let (outcome, _) = run("OK\nOK\nOK\nOK\nERR 536871187 Unknown command\nD x\nOK\n");
        assert_eq!(
            outcome.unwrap(),
            Outcome::Password(Zeroizing::new(b"x".to_vec()))
        );
    }

    #[test]
    fn other_getpin_error_is_a_failure() {
        let (outcome, _) = run("OK\nOK\nOK\nOK\nOK\nERR 83918950 Inappropriate ioctl\n");
        assert!(matches!(
            outcome,
            Err(AssuanError::Refused {
                command: "GETPIN",
                ..
            })
        ));
    }

    #[test]
    fn early_close_is_a_failure() {
        let (outcome, _) = run("OK\nOK\n");
        assert!(matches!(outcome, Err(AssuanError::Closed)));
    }

    #[test]
    fn garbage_line_is_a_failure() {
        let (outcome, _) = run("HELLO\n");
        assert!(matches!(outcome, Err(AssuanError::Unexpected(_))));
    }
}
