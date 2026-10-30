//! Shows the password dialog: a pinentry program, with zenity as a fallback.

use std::io::{self, Read};
use std::path::PathBuf;
use std::process::{Child, Command, Stdio};
use std::sync::mpsc;
use std::thread;
use std::time::Duration;

use thiserror::Error;
use zeroize::Zeroizing;

use crate::assuan::{self, AssuanError, Outcome, PromptText};

/// A program that can show the dialog.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum DialogProgram {
    /// A pinentry program spoken to over Assuan (`pinentry-gnome3`, `pinentry-qt`).
    Pinentry(PathBuf),
    /// `zenity --entry --hide-text`.
    Zenity(PathBuf),
}

impl DialogProgram {
    fn path(&self) -> &PathBuf {
        match self {
            Self::Pinentry(path) | Self::Zenity(path) => path,
        }
    }
}

/// The dialog programs to try, in order. The next one runs only when the previous one could not
/// show a dialog at all; a cancel or a timeout is an answer and ends the request.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Dialog {
    programs: Vec<DialogProgram>,
}

/// No dialog program could ask.
#[derive(Debug, Error)]
#[error("no dialog program could ask: {}", .0.join("; "))]
pub struct DialogError(pub Vec<String>);

#[derive(Debug, Error)]
enum ProgramError {
    #[error("cannot start: {0}")]
    Spawn(io::Error),
    #[error(transparent)]
    Assuan(#[from] AssuanError),
    #[error("i/o error: {0}")]
    Io(#[from] io::Error),
    #[error("exited with {0}")]
    Exit(std::process::ExitStatus),
}

impl Dialog {
    /// A dialog that tries `programs` in order.
    #[must_use]
    pub const fn new(programs: Vec<DialogProgram>) -> Self {
        Self { programs }
    }

    /// Asks for a password and waits at most `timeout` for the answer.
    ///
    /// # Errors
    ///
    /// Fails when no program could show a dialog; each program's reason is listed.
    pub fn ask(&self, text: &PromptText, timeout: Duration) -> Result<Outcome, DialogError> {
        let mut reasons = Vec::new();
        for program in &self.programs {
            let result = match program {
                DialogProgram::Pinentry(path) => run_pinentry(path, text, timeout),
                DialogProgram::Zenity(path) => run_zenity(path, text, timeout),
            };
            match result {
                Ok(outcome) => return Ok(outcome),
                Err(error) => reasons.push(format!("{}: {error}", program.path().display())),
            }
        }
        Err(DialogError(reasons))
    }
}

fn run_pinentry(
    path: &PathBuf,
    text: &PromptText,
    timeout: Duration,
) -> Result<Outcome, ProgramError> {
    let mut child = Command::new(path)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .map_err(ProgramError::Spawn)?;
    let (Some(stdin), Some(stdout)) = (child.stdin.take(), child.stdout.take()) else {
        reap(&mut child);
        return Err(ProgramError::Io(io::Error::other("pinentry pipes missing")));
    };
    let text = text.clone();
    let answer = with_deadline(&mut child, timeout, move || {
        assuan::converse(stdout, stdin, &text, timeout)
    });
    reap(&mut child);
    match answer {
        Some(result) => Ok(result?),
        None => Ok(Outcome::TimedOut),
    }
}

fn run_zenity(
    path: &PathBuf,
    text: &PromptText,
    timeout: Duration,
) -> Result<Outcome, ProgramError> {
    let mut child = Command::new(path)
        .args(["--entry", "--hide-text", "--title"])
        .arg(&text.title)
        .arg("--text")
        .arg(pango_escape(&format!(
            "{}\n\n{}",
            text.description, text.prompt
        )))
        .arg("--timeout")
        .arg(timeout.as_secs().to_string())
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .map_err(ProgramError::Spawn)?;
    let Some(mut stdout) = child.stdout.take() else {
        reap(&mut child);
        return Err(ProgramError::Io(io::Error::other("zenity stdout missing")));
    };
    let answer = with_deadline(&mut child, timeout, move || {
        // Reserved up front so the password is not copied by a reallocation.
        let mut out = Zeroizing::new(Vec::with_capacity(4096));
        stdout.read_to_end(&mut out).map(|_| out)
    });
    let Some(output) = answer else {
        reap(&mut child);
        return Ok(Outcome::TimedOut);
    };
    let mut output = output?;
    let status = child.wait()?;
    match status.code() {
        Some(0) => {
            if output.last() == Some(&b'\n') {
                output.pop();
            }
            Ok(Outcome::Password(output))
        }
        Some(1) => Ok(Outcome::Cancelled),
        Some(5) => Ok(Outcome::TimedOut),
        _ => Err(ProgramError::Exit(status)),
    }
}

/// Runs `work` on a thread and waits for it at most `timeout`. On timeout the child is killed,
/// which closes its pipes and lets the thread finish on its own.
fn with_deadline<T: Send + 'static>(
    child: &mut Child,
    timeout: Duration,
    work: impl FnOnce() -> T + Send + 'static,
) -> Option<T> {
    let (sender, receiver) = mpsc::channel();
    thread::spawn(move || {
        let _ = sender.send(work());
    });
    if let Ok(answer) = receiver.recv_timeout(timeout) {
        Some(answer)
    } else {
        reap(child);
        None
    }
}

fn reap(child: &mut Child) {
    let _ = child.kill();
    let _ = child.wait();
}

fn pango_escape(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    for ch in text.chars() {
        match ch {
            '&' => out.push_str("&amp;"),
            '<' => out.push_str("&lt;"),
            '>' => out.push_str("&gt;"),
            other => out.push(other),
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use std::fs;
    use std::os::unix::fs::PermissionsExt;
    use std::path::Path;
    use std::time::Instant;

    use super::*;

    fn text() -> PromptText {
        PromptText {
            title: "sudo on pro".into(),
            description: "a <b> & c".into(),
            prompt: "Password:".into(),
        }
    }

    fn script(dir: &Path, name: &str, body: &str) -> PathBuf {
        let path = dir.join(name);
        fs::write(&path, format!("#!/bin/sh\n{body}")).unwrap();
        fs::set_permissions(&path, fs::Permissions::from_mode(0o700)).unwrap();
        path
    }

    fn scratch(name: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!(
            "askpass-relay-dialog-{name}-{}",
            std::process::id()
        ));
        fs::create_dir_all(&dir).unwrap();
        dir
    }

    const PINENTRY_OK: &str = "echo OK\nwhile read -r l; do case \"$l\" in \
        GETPIN) echo 'D pw'; echo OK;; BYE) exit 0;; *) echo OK;; esac; done\n";

    #[test]
    fn pinentry_answer_is_returned() {
        let dir = scratch("ok");
        let dialog = Dialog::new(vec![DialogProgram::Pinentry(script(
            &dir,
            "p",
            PINENTRY_OK,
        ))]);
        let outcome = dialog.ask(&text(), Duration::from_secs(5)).unwrap();
        assert_eq!(outcome, Outcome::Password(Zeroizing::new(b"pw".to_vec())));
        fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn hanging_pinentry_is_killed_at_the_deadline() {
        let dir = scratch("hang");
        let pinentry = script(&dir, "p", "echo OK\nexec sleep 30\n");
        let dialog = Dialog::new(vec![DialogProgram::Pinentry(pinentry)]);
        let started = Instant::now();
        let outcome = dialog.ask(&text(), Duration::from_millis(300)).unwrap();
        assert_eq!(outcome, Outcome::TimedOut);
        assert!(started.elapsed() < Duration::from_secs(5));
        fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn zenity_runs_when_pinentry_is_missing() {
        let dir = scratch("fallback");
        let args = dir.join("args");
        let zenity = script(
            &dir,
            "z",
            &format!("printf '%s\\n' \"$@\" > {}\necho 'zpw'\n", args.display()),
        );
        let dialog = Dialog::new(vec![
            DialogProgram::Pinentry(dir.join("missing")),
            DialogProgram::Zenity(zenity),
        ]);
        let outcome = dialog.ask(&text(), Duration::from_secs(5)).unwrap();
        assert_eq!(outcome, Outcome::Password(Zeroizing::new(b"zpw".to_vec())));
        let args = fs::read_to_string(args).unwrap();
        assert!(args.contains("a &lt;b&gt; &amp; c"), "{args}");
        fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn zenity_cancel_is_a_cancel() {
        let dir = scratch("zcancel");
        let dialog = Dialog::new(vec![DialogProgram::Zenity(script(&dir, "z", "exit 1\n"))]);
        assert_eq!(
            dialog.ask(&text(), Duration::from_secs(5)).unwrap(),
            Outcome::Cancelled
        );
        fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn every_failure_is_reported() {
        let dialog = Dialog::new(vec![
            DialogProgram::Pinentry("/nonexistent/pinentry".into()),
            DialogProgram::Zenity("/nonexistent/zenity".into()),
        ]);
        let error = dialog.ask(&text(), Duration::from_secs(1)).unwrap_err();
        assert_eq!(error.0.len(), 2, "{error}");
    }
}
