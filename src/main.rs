//! The `askpass-relay` command: `serve` on the desktop, `ask` on a remote host.
#![forbid(unsafe_code)]

use std::env;
use std::fs::File;
use std::io::{self, Write};
use std::os::fd::AsFd;
use std::os::unix::process::CommandExt;
use std::os::unix::process::parent_id;
use std::path::{Path, PathBuf};
use std::process::{Command, ExitCode, Stdio};
use std::time::Duration;

use anyhow::{Context, Result};
use argh::FromArgs;
use askpass_relay::client;
use askpass_relay::dialog::{Dialog, DialogProgram};
use askpass_relay::protocol::Request;
use askpass_relay::server;
use zeroize::Zeroizing;

/// Relays `sudo -A` password prompts from remote hosts to a dialog on the desktop.
#[derive(FromArgs)]
struct Cli {
    #[argh(subcommand)]
    command: Subcommand,
}

#[derive(FromArgs)]
#[argh(subcommand)]
enum Subcommand {
    Serve(Serve),
    Ask(Ask),
    Sudo(Sudo),
}

/// Listen on a unix socket and answer each request with a password dialog.
#[derive(FromArgs)]
#[argh(subcommand, name = "serve")]
struct Serve {
    /// the socket to listen on (default: $XDG_RUNTIME_DIR/askpass-relay/sock).
    #[argh(option)]
    socket: Option<PathBuf>,
    /// the pinentry program (default: pinentry-gnome3).
    #[argh(option, default = "PathBuf::from(\"pinentry-gnome3\")")]
    pinentry: PathBuf,
    /// the fallback dialog program, run as zenity (default: zenity).
    #[argh(option, default = "PathBuf::from(\"zenity\")")]
    fallback: PathBuf,
    /// use no fallback dialog.
    #[argh(switch)]
    no_fallback: bool,
    /// seconds a dialog waits for an answer before it counts as a cancel (default: 60).
    #[argh(option, default = "60")]
    timeout: u64,
}

/// Ask the relay for a password; run by sudo as SUDO_ASKPASS.
#[derive(FromArgs)]
#[argh(subcommand, name = "ask")]
struct Ask {
    /// the relay socket (default: $ASKPASS_RELAY_SOCKET, else ~/.askpass-relay/sock).
    #[argh(option)]
    socket: Option<PathBuf>,
    /// seconds to wait for an answer (default: 180).
    #[argh(option, default = "DEFAULT_ASK_TIMEOUT")]
    timeout: u64,
    /// the prompt sudo passes.
    #[argh(positional, greedy)]
    prompt: Vec<String>,
}

/// Run a command with `sudo -A` once the relay answers; never ask for a password through a dead
/// relay.
#[derive(FromArgs)]
#[argh(subcommand, name = "sudo")]
struct Sudo {
    /// why the command needs root, shown in the dialog (default: $ASKPASS_RELAY_REASON).
    #[argh(option)]
    reason: Option<String>,
    /// the relay socket (default: $ASKPASS_RELAY_SOCKET, else ~/.askpass-relay/sock).
    #[argh(option)]
    socket: Option<PathBuf>,
    /// the command and its arguments.
    #[argh(positional, greedy)]
    command: Vec<String>,
}

/// Seconds `ask` waits for an answer by default.
const DEFAULT_ASK_TIMEOUT: u64 = 180;

/// The variable a caller sets to tell the maintainer why it runs sudo.
const REASON_VARIABLE: &str = "ASKPASS_RELAY_REASON";

/// Run under this name, the binary is `ask` with the prompt as its only argument: sudo runs
/// `SUDO_ASKPASS` as a bare path, so it cannot pass a subcommand.
const ASK_LINK_NAME: &str = "askpass-relay-ask";

/// Run under this name, the binary is `askpass-relay sudo`.
const SUDO_LINK_NAME: &str = "rsudo";

/// How long the wrapper waits for the relay to answer a ping.
const PING_TIMEOUT: Duration = Duration::from_secs(5);

fn main() -> ExitCode {
    let mut args = env::args_os();
    let invoked_as = args.next().map(PathBuf::from);
    let name = invoked_as.as_deref().and_then(Path::file_name);
    let result = if name == Some(ASK_LINK_NAME.as_ref()) {
        run_ask(Ask {
            socket: None,
            timeout: DEFAULT_ASK_TIMEOUT,
            prompt: args.map(|arg| arg.to_string_lossy().into_owned()).collect(),
        })
    } else if name == Some(SUDO_LINK_NAME.as_ref()) {
        let rest: Vec<String> = args.map(|arg| arg.to_string_lossy().into_owned()).collect();
        let rest: Vec<&str> = rest.iter().map(String::as_str).collect();
        match Sudo::from_args(&[SUDO_LINK_NAME], &rest) {
            Ok(sudo) => run_sudo(sudo),
            Err(early) => {
                let status = early
                    .status
                    .map_or(ExitCode::FAILURE, |()| ExitCode::SUCCESS);
                if early.status.is_ok() {
                    println!("{}", early.output);
                } else {
                    eprintln!("{}", early.output);
                }
                return status;
            }
        }
    } else {
        match argh::from_env::<Cli>().command {
            Subcommand::Serve(serve) => run_serve(serve),
            Subcommand::Ask(ask) => run_ask(ask),
            Subcommand::Sudo(sudo) => run_sudo(sudo),
        }
    };
    match result {
        Ok(()) => ExitCode::SUCCESS,
        Err(error) => {
            eprintln!("askpass-relay: {error}");
            ExitCode::FAILURE
        }
    }
}

fn run_serve(args: Serve) -> Result<()> {
    let socket = match args.socket {
        Some(socket) => socket,
        None => {
            PathBuf::from(env::var_os("XDG_RUNTIME_DIR").context("XDG_RUNTIME_DIR is not set")?)
                .join("askpass-relay")
                .join("sock")
        }
    };
    let mut programs = vec![DialogProgram::Pinentry(args.pinentry)];
    if !args.no_fallback {
        programs.push(DialogProgram::Zenity(args.fallback));
    }
    let listener = server::bind(&socket)?;
    eprintln!("askpass-relay: listening on {}", socket.display());
    server::serve(
        &listener,
        &Dialog::new(programs),
        Duration::from_secs(args.timeout),
    );
    Ok(())
}

/// The relay socket on a remote host: the option, `ASKPASS_RELAY_SOCKET`, else
/// `~/.askpass-relay/sock`.
fn remote_socket(option: Option<PathBuf>) -> Result<PathBuf> {
    match option.or_else(|| env::var_os("ASKPASS_RELAY_SOCKET").map(PathBuf::from)) {
        Some(socket) => Ok(socket),
        None => Ok(
            PathBuf::from(env::var_os("HOME").context("HOME is not set")?)
                .join(".askpass-relay")
                .join("sock"),
        ),
    }
}

/// Checks the relay before sudo can ask for a password: once sudo runs the askpass program, a
/// failure counts as a failed login against the host's lockout policy.
fn run_sudo(args: Sudo) -> Result<()> {
    anyhow::ensure!(!args.command.is_empty(), "no command to run");
    let sudo = env::var_os("ASKPASS_RELAY_SUDO").unwrap_or_else(|| "sudo".into());
    // `sudo -n` fails without authenticating when a password is needed, so it counts nothing.
    let needs_password = !Command::new(&sudo)
        .args(["-n", "true"])
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .with_context(|| format!("cannot run {}", sudo.to_string_lossy()))?
        .success();
    if needs_password {
        let socket = remote_socket(args.socket)?;
        client::ping(&socket, PING_TIMEOUT).map_err(|error| {
            anyhow::anyhow!(
                "sudo was not run: {error}. On the desktop, run `systemctl --user restart \
                 askpass-relay askpass-relay-forward@<this host's ssh name>`, then try again."
            )
        })?;
    }
    let mut command = Command::new(&sudo);
    command.arg("-A").args(&args.command);
    if let Some(reason) = args.reason {
        command.env(REASON_VARIABLE, reason);
    }
    if env::var_os("SUDO_ASKPASS").is_none() {
        let askpass = env::current_exe()?.with_file_name(ASK_LINK_NAME);
        command.env("SUDO_ASKPASS", askpass);
    }
    Err(command.exec()).context("cannot run sudo")
}

fn run_ask(args: Ask) -> Result<()> {
    let socket = remote_socket(args.socket)?;
    let request = Request {
        host: client::host_name(),
        user: client::user_name(),
        prompt: args.prompt.join(" "),
        command: client::command_line(parent_id()),
        // sudo starts the askpass program with the invoking user's environment, so a reason set
        // on the sudo command line arrives here, on Linux and on macOS alike.
        reason: env::var(REASON_VARIABLE)
            .ok()
            .filter(|reason| !reason.is_empty()),
    };
    let password = client::ask(&socket, &request, Duration::from_secs(args.timeout))?;
    let mut line = Zeroizing::new(Vec::with_capacity(password.len() + 1));
    line.extend_from_slice(&password);
    line.push(b'\n');
    // Written straight to the descriptor: the buffered stdout would keep a copy of the password.
    let mut stdout = File::from(io::stdout().as_fd().try_clone_to_owned()?);
    stdout.write_all(&line)?;
    Ok(())
}
