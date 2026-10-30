//! The `askpass-relay` command: `serve` on the desktop, `ask` on a remote host.
#![forbid(unsafe_code)]

use std::env;
use std::fs::File;
use std::io::{self, Write};
use std::os::fd::AsFd;
use std::os::unix::process::parent_id;
use std::path::{Path, PathBuf};
use std::process::ExitCode;
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

/// Run under this name, the binary is `ask` with the prompt as its only argument: sudo runs
/// `SUDO_ASKPASS` as a bare path, so it cannot pass a subcommand.
/// Seconds `ask` waits for an answer by default.
const DEFAULT_ASK_TIMEOUT: u64 = 180;

const ASK_LINK_NAME: &str = "askpass-relay-ask";

fn main() -> ExitCode {
    let mut args = env::args_os();
    let invoked_as = args.next().map(PathBuf::from);
    let result = if invoked_as.as_deref().and_then(Path::file_name) == Some(ASK_LINK_NAME.as_ref())
    {
        run_ask(Ask {
            socket: None,
            timeout: DEFAULT_ASK_TIMEOUT,
            prompt: args.map(|arg| arg.to_string_lossy().into_owned()).collect(),
        })
    } else {
        match argh::from_env::<Cli>().command {
            Subcommand::Serve(serve) => run_serve(serve),
            Subcommand::Ask(ask) => run_ask(ask),
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

fn run_ask(args: Ask) -> Result<()> {
    let socket = args
        .socket
        .or_else(|| env::var_os("ASKPASS_RELAY_SOCKET").map(PathBuf::from));
    let socket = match socket {
        Some(socket) => socket,
        None => PathBuf::from(env::var_os("HOME").context("HOME is not set")?)
            .join(".askpass-relay")
            .join("sock"),
    };
    let request = Request {
        host: client::host_name(),
        user: client::user_name(),
        prompt: args.prompt.join(" "),
        command: client::command_line(parent_id()),
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
