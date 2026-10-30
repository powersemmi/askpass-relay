//! Relays `sudo -A` password prompts from remote hosts to a dialog on the local desktop.
//!
//! `ask` runs on the remote host as `SUDO_ASKPASS` and sends a [`protocol::Request`] over a unix
//! socket that ssh forwards back to this machine, where `serve` shows a pinentry dialog and
//! answers with a [`protocol::Response`].
#![forbid(unsafe_code)]

pub mod assuan;
pub mod client;
pub mod dialog;
pub mod protocol;
pub mod server;
