# askpass-relay

`askpass-relay` lets a non-interactive `ssh host 'sudo -A ...'` ask for the sudo password in a
dialog on your desktop. sudo on the remote host runs `askpass-relay-ask` as its askpass program.
It sends the request over a unix socket that ssh forwards back to your desktop. There
`askpass-relay serve` shows a pinentry dialog that asks for the password of that account on that
host and names the command, and returns the password you type, or a cancel.

```text
remote host                                   desktop
sudo -A cmd
  -> askpass-relay-ask "<prompt>"
       ~/.askpass-relay/sock  == ssh RemoteForward ==>  $XDG_RUNTIME_DIR/askpass-relay/sock
                                                          askpass-relay serve
                                                            -> pinentry-gnome3 dialog
  <- password on stdout, or exit 1               <-
```

## Behaviour

- One dialog at a time. A request waits for the requests before it.
- A dialog that gets no answer in 60 seconds (`serve --timeout`) counts as a cancel.
- On a cancel, a timeout, a missing socket or a relay that is down, `ask` prints nothing and
  exits with status 1, so sudo fails at once.
- The dialog is `pinentry-gnome3` by default (`serve --pinentry <program>` picks another
  pinentry, for example `pinentry-qt`). When the pinentry cannot show a dialog at all, `zenity`
  asks instead (`--fallback <program>`, or `--no-fallback`).
- The password lives only in memory, in buffers that are wiped when dropped. It is never logged.
  The service log names the host, the user, the command and the outcome.
- Both sockets are owned by the user with mode 0600, inside a directory with mode 0700.

## Protocol

Each message is one frame: a big-endian `u32` payload length, then the payload. The payload is
the protocol version (`1`), a message kind, and fields, each a big-endian `u16` length and its
bytes. A request carries the host, the user, sudo's prompt and the command line of the sudo
process. The answer is a password, a cancel, or a failure with its reason. Frames over 64 KiB,
another version, an unknown kind, a missing field or trailing bytes are rejected.

## Install on the desktop

```sh
cargo install --path .
mkdir -p ~/.config/systemd/user
cp contrib/askpass-relay.service ~/.config/systemd/user/
systemctl --user daemon-reload
systemctl --user enable --now askpass-relay.service
systemctl --user status askpass-relay.service
```

The unit starts with the graphical session, so the dialog finds the Wayland or X11 display.
`journalctl --user -u askpass-relay` shows each request and its outcome.

## The ssh configuration

Add a block per host to `~/.ssh/config` on the desktop. The remote path is absolute, because
sshd resolves no `~` in a forwarded socket path. Replace `1000` with your uid (`id -u`).

```sshconfig
Host pro
    RemoteForward /home/remote-user/.askpass-relay/sock /run/user/1000/askpass-relay/sock

Host air
    RemoteForward /Users/remote-user/.askpass-relay/sock /run/user/1000/askpass-relay/sock

Host pro air
    StreamLocalBindUnlink yes
```

A shared connection (`ControlMaster`) carries the forward for every command that reuses it.

sshd leaves the forwarded socket file behind when a connection closes. The next connection then
cannot bind it, and a shared connection never retries. Set `StreamLocalBindUnlink yes` in the
remote sshd, so that each new connection replaces the old file (see below). On a host without
it, `ask` removes a stale socket it finds and fails; the next new connection forwards a live one
(`ssh -O exit <host>` closes a shared connection).

## Install on a remote host

```sh
rsync -a --exclude target --exclude .git ./ host:src/askpass-relay/
ssh host 'cd ~/src/askpass-relay && cargo install --locked --path .'
ssh host 'mkdir -p -m 700 ~/.askpass-relay && ln -sf askpass-relay ~/.cargo/bin/askpass-relay-ask'
```

sudo runs `SUDO_ASKPASS` as a bare path with the prompt as its only argument. Under the name
`askpass-relay-ask` the binary acts as `askpass-relay ask`.

Export `SUDO_ASKPASS` in the file the remote login shell reads for non-interactive ssh commands.
For zsh that is `~/.zshenv` (with a custom `ZDOTDIR`, `$ZDOTDIR/.zshenv`); for bash it is
`~/.bashrc`, above its interactive-only guard.

```sh
export SUDO_ASKPASS="$HOME/.cargo/bin/askpass-relay-ask"
```

Then let the remote sshd replace stale sockets. This needs root once, through the relay itself:

```sh
ssh host 'printf "StreamLocalBindUnlink yes\n" | sudo -A tee /etc/ssh/sshd_config.d/10-askpass-relay.conf'
ssh host 'sudo -A sshd -t'                     # check the configuration
ssh host 'sudo -A systemctl reload sshd'       # Linux; macOS reads it on the next connection
```

## Use from an agent or a script

```sh
ssh pro 'sudo -A pacman -Syu --noconfirm'
ssh air 'sudo -A -k true && echo ok'
```

The desktop shows a dialog. Approve it, and the command runs. Cancel it, and sudo exits with an
error at once.

The dialog is titled "sudo password for <user>@<host>". It asks for the login password of that
account on that host and shows the command that needs it. A wrong password makes sudo ask
again, up to three times, and each failure counts against the host's lockout policy
(`pam_faillock` on Arch locks the account for ten minutes after three). Cancel a dialog you did
not expect.

`ask` has its own options for a manual check: `askpass-relay ask --socket <path> "<prompt>"`.
`ASKPASS_RELAY_SOCKET` overrides the default socket `~/.askpass-relay/sock`.

## Development

```sh
cargo test
cargo clippy --all-targets -- -D warnings
cargo fmt --check
```

The tests drive `serve` with a fake pinentry script and `ask` against it through a real socket:
approval, cancel, timeout, a missing socket, a stale socket and a malformed frame.
