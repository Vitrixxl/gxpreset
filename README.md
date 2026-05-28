# gxpreset

Ratatui terminal UI for a small headless Guitarix/PipeWire rig.

It can browse and download Guitarix presets from Musical Artifacts, inspect PipeWire audio nodes, connect sources to targets, show a lightweight volume-history visualizer, record the selected meter source, play/rename/delete recordings, manage pedal groups made of Guitarix presets, and control Guitarix banks/presets over its JSON-RPC interface.

## Build

```sh
cargo build --release
cp target/release/gxpreset ./gxpreset
```

On the ARM64 board, build natively with the same command after installing Rust:

```sh
curl https://sh.rustup.rs -sSf | sh
. "$HOME/.cargo/env"
cargo build --release
```

Debian's `cargo` package can be too old. Cargo 1.65 cannot read the current `Cargo.lock` format.

## System dependencies

On Debian:

```sh
sudo apt update && sudo apt install -y pipewire-bin pipewire-jack pipewire-audio guitarix
```

For a PipeWire-only audio setup, make sure the user services are running and stop any old JACK daemon:

```sh
systemctl --user enable --now pipewire pipewire-pulse wireplumber
pkill -x jackd 2>/dev/null || true
pkill -x jackdbus 2>/dev/null || true
```

On a headless board where user services stop after logout, enable lingering once:

```sh
sudo loginctl enable-linger "$USER"
```

If `systemctl --user` says `Failed to connect to bus: Host is down`, install the user D-Bus session support, enable the user's systemd instance, then reconnect or reboot:

```sh
sudo apt install -y dbus-user-session
sudo loginctl enable-linger "$USER"
sudo systemctl start "user@$(id -u).service"
systemctl --user enable --now pipewire pipewire-pulse wireplumber
```

Do not run `pw-link`, `pw-cat`, `pw-jack`, or `systemctl --user` with `sudo`; they need the normal user's PipeWire socket in `/run/user/$(id -u)`.

You can also ask the CLI to report missing dependencies:

```sh
./gxpreset -deps
```

The PipeWire tools are separate commands with hyphens: `pw-link`, `pw-cat`, and `pw-jack`.
Run JACK apps through PipeWire with `pw-jack`, for example `pw-jack guitarix -N -p 7000`.

## Usage

```sh
./gxpreset
```

Main keys:

- `tab` / `shift-tab`: switch view
- `,` / `;`: switch the active pedal group to the previous/next preset globally
- `h` / `l`: move focus left/right where applicable
- `enter`: select/open/connect
- `space`: toggle target connection in the audio target picker
- `n`: create a pedal group in the Pedals tab
- `a`: add a Guitarix preset to a pedal group in the Pedals tab
- `x`: disconnect or delete a Guitarix bank when focused
- `R`: start/stop recording from the Audio visualizer
- `p` / `s`: play or stop a recording in the Records tab
- `e`: rename a recording in the Records tab
- `r`: refresh
- `q`: quit

Recordings are saved as WAV files in:

```sh
${XDG_DATA_HOME:-$HOME/.local/share}/gxpreset/recordings
```

Pedal groups are saved in:

```sh
${XDG_CONFIG_HOME:-$HOME/.config}/gxpreset/pedals.json
```
