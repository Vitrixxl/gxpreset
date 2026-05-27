# gxpreset

Terminal UI for a small headless Guitarix/PipeWire rig.

It can browse and download Guitarix presets from Musical Artifacts, inspect PipeWire audio nodes, connect sources to targets, show a lightweight spectrum meter, and control Guitarix banks/presets over its JSON-RPC interface.

## Build

```sh
go build -trimpath -ldflags='-s -w' -o gxpreset .
```

For ARM64 Linux:

```sh
GOOS=linux GOARCH=arm64 CGO_ENABLED=0 go build -trimpath -ldflags='-s -w' -o gxpreset-linux-arm64 .
```

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

- `tab`: switch view
- `h` / `l`: move focus left/right where applicable
- `enter`: select/open/connect
- `space`: toggle target connection in the audio target picker
- `x`: disconnect or delete a Guitarix bank when focused
- `r`: refresh
- `q`: quit
