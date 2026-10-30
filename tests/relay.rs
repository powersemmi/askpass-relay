//! Runs `serve` with a fake pinentry and `ask` against it through a real unix socket.

use std::io::{BufRead, BufReader, Write};
use std::os::unix::fs::{PermissionsExt, symlink};
use std::os::unix::net::{UnixListener, UnixStream};
use std::path::{Path, PathBuf};
use std::process::{Child, ChildStderr, Command, Output, Stdio};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::{Duration, Instant};
use std::{env, fs, process};

use askpass_relay::protocol::{self, Response};

const BIN: &str = env!("CARGO_BIN_EXE_askpass-relay");
const PROMPT: &str = "[sudo] password for tester: ";

const FAKE_PINENTRY: &str = r#"#!/bin/sh
echo "OK fake pinentry"
while IFS= read -r line; do
  echo "$line" >> "$FAKE_PINENTRY_LOG"
  case "$line" in
    GETPIN)
      case "$FAKE_PINENTRY_MODE" in
        cancel) echo "ERR 83886179 Operation cancelled <Pinentry>" ;;
        hang) exec sleep 30 ;;
        *) echo "D hunter%252"; echo "OK" ;;
      esac ;;
    BYE) echo "OK"; exit 0 ;;
    *) echo "OK" ;;
  esac
done
"#;

/// Writes an executable script from a child process. A test that writes it from its own process
/// races with the forks of tests running beside it: a fork that inherits the open file makes the
/// later exec fail with "Text file busy".
fn write_executable(path: &Path, body: &str) {
    let mut writer = Command::new("sh")
        .args(["-c", "cat > \"$1\" && chmod 700 \"$1\"", "sh"])
        .arg(path)
        .stdin(Stdio::piped())
        .spawn()
        .expect("start the script writer");
    writer
        .stdin
        .take()
        .expect("the writer's stdin is piped")
        .write_all(body.as_bytes())
        .expect("send the script");
    assert!(
        writer.wait().expect("wait for the writer").success(),
        "writing {} failed",
        path.display()
    );
}

static NEXT: AtomicUsize = AtomicUsize::new(0);

struct Scratch(PathBuf);

impl Scratch {
    fn new() -> Self {
        let dir = env::temp_dir().join(format!(
            "askpass-relay-test-{}-{}",
            process::id(),
            NEXT.fetch_add(1, Ordering::Relaxed)
        ));
        fs::create_dir_all(&dir).expect("create the scratch directory");
        Self(dir)
    }

    fn socket(&self) -> PathBuf {
        self.0.join("run").join("sock")
    }
}

impl Drop for Scratch {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.0);
    }
}

struct Relay {
    child: Child,
    _stderr: BufReader<ChildStderr>,
    scratch: Scratch,
}

impl Relay {
    fn start(mode: &str) -> Self {
        let scratch = Scratch::new();
        let pinentry = scratch.0.join("fake-pinentry");
        write_executable(&pinentry, FAKE_PINENTRY);
        let mut child = Command::new(BIN)
            .args(["serve", "--no-fallback", "--timeout", "1", "--socket"])
            .arg(scratch.socket())
            .arg("--pinentry")
            .arg(&pinentry)
            .env("FAKE_PINENTRY_MODE", mode)
            .env("FAKE_PINENTRY_LOG", scratch.0.join("pinentry.log"))
            .stderr(Stdio::piped())
            .spawn()
            .expect("start serve");
        let mut first = String::new();
        let mut stderr = BufReader::new(child.stderr.take().expect("serve stderr is piped"));
        stderr
            .read_line(&mut first)
            .expect("read the first line of serve");
        assert!(
            first.contains("listening on"),
            "serve did not start: {first:?}"
        );
        Self {
            child,
            _stderr: stderr,
            scratch,
        }
    }

    fn pinentry_log(&self) -> String {
        fs::read_to_string(self.scratch.0.join("pinentry.log")).unwrap_or_default()
    }
}

impl Drop for Relay {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

fn ask(socket: &Path) -> Output {
    Command::new(BIN)
        .args(["ask", PROMPT])
        .env("ASKPASS_RELAY_SOCKET", socket)
        .output()
        .expect("run ask")
}

#[test]
fn approved_dialog_prints_the_password() {
    let relay = Relay::start("ok");
    let output = ask(&relay.scratch.socket());
    assert!(output.status.success(), "ask failed: {output:?}");
    assert_eq!(output.stdout, b"hunter%2\n");
}

#[test]
fn dialog_names_the_host_the_user_and_the_command() {
    let relay = Relay::start("ok");
    ask(&relay.scratch.socket());
    let log = relay.pinentry_log();
    let desc = log.lines().find(|l| l.starts_with("SETDESC ")).unwrap();
    let host = Command::new("uname").arg("-n").output().unwrap().stdout;
    let host = String::from_utf8(host).unwrap();
    let user = env::var("USER").unwrap();
    let me = env::current_exe().unwrap();
    let me = me.file_name().unwrap().to_str().unwrap();
    assert!(desc.contains(host.trim()), "{desc}");
    assert!(desc.contains(&user), "{desc}");
    assert!(
        desc.contains(me),
        "the caller's command line is missing: {desc}"
    );
}

#[test]
fn socket_is_private() {
    let relay = Relay::start("ok");
    let socket = relay.scratch.socket();
    let mode = fs::metadata(&socket).unwrap().permissions().mode() & 0o777;
    let dir_mode = fs::metadata(socket.parent().unwrap())
        .unwrap()
        .permissions()
        .mode()
        & 0o777;
    assert_eq!((mode, dir_mode), (0o600, 0o700));
}

#[test]
fn cancelled_dialog_fails_with_no_output() {
    let relay = Relay::start("cancel");
    let output = ask(&relay.scratch.socket());
    assert!(!output.status.success());
    assert_eq!(output.stdout, b"");
    assert!(String::from_utf8_lossy(&output.stderr).contains("cancelled"));
}

#[test]
fn unanswered_dialog_times_out_as_a_cancel() {
    let relay = Relay::start("hang");
    let started = Instant::now();
    let output = ask(&relay.scratch.socket());
    assert!(!output.status.success());
    assert_eq!(output.stdout, b"");
    assert!(
        started.elapsed() < Duration::from_secs(10),
        "{:?}",
        started.elapsed()
    );
    // The relay keeps serving after a timeout.
    assert!(!ask(&relay.scratch.socket()).status.success());
}

#[test]
fn missing_socket_fails_at_once() {
    let scratch = Scratch::new();
    let started = Instant::now();
    let output = ask(&scratch.socket());
    assert!(!output.status.success());
    assert_eq!(output.stdout, b"");
    assert!(started.elapsed() < Duration::from_secs(5));
    assert!(String::from_utf8_lossy(&output.stderr).contains("not running"));
}

#[test]
fn stale_socket_fails_and_is_removed() {
    let scratch = Scratch::new();
    let socket = scratch.socket();
    fs::create_dir_all(socket.parent().unwrap()).unwrap();
    drop(UnixListener::bind(&socket).unwrap());
    let output = ask(&socket);
    assert!(!output.status.success());
    assert!(String::from_utf8_lossy(&output.stderr).contains("stale"));
    assert!(!socket.exists());
}

#[test]
fn malformed_frame_is_rejected_and_the_relay_keeps_serving() {
    let relay = Relay::start("ok");
    let mut stream = UnixStream::connect(relay.scratch.socket()).unwrap();
    stream.write_all(&[0, 0, 0, 2, 9, 9]).unwrap();
    let response = protocol::read_response(&mut stream).unwrap();
    assert!(matches!(response, Response::Failed(_)), "{response:?}");
    assert_eq!(ask(&relay.scratch.socket()).stdout, b"hunter%2\n");
}

#[test]
fn second_relay_on_a_live_socket_refuses_to_start() {
    let relay = Relay::start("ok");
    let output = Command::new(BIN)
        .args(["serve", "--socket"])
        .arg(relay.scratch.socket())
        .output()
        .unwrap();
    assert!(!output.status.success());
    assert!(String::from_utf8_lossy(&output.stderr).contains("already"));
}

#[test]
fn askpass_link_name_runs_ask_with_the_prompt_as_its_only_argument() {
    let relay = Relay::start("ok");
    let link = relay.scratch.0.join("askpass-relay-ask");
    symlink(BIN, &link).unwrap();
    let output = Command::new(&link)
        .arg(PROMPT)
        .env("ASKPASS_RELAY_SOCKET", relay.scratch.socket())
        .output()
        .unwrap();
    assert!(output.status.success(), "{output:?}");
    assert_eq!(output.stdout, b"hunter%2\n");
}
