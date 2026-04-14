# linux-airpods-cli

A small AirPods CLI for Linux using only system Bluetooth and audio tooling.

It intentionally avoids depending on helper daemons like LibrePods. The first version focuses on the parts Linux already exposes reliably through BlueZ and PipeWire:

- device discovery
- connect / disconnect
- default sink switching
- active stream movement
- transport recovery
- status inspection
- native AAP battery querying
- native AirPods key retrieval

## Why

AirPods on Linux often fail in a frustrating half-connected state:

- paired but not usable as an audio sink
- connected over Bluetooth but missing in PipeWire
- still routed to laptop speakers
- stuck after a failed reconnect

This CLI gives you one place to recover that state quickly.

## Dependencies

Required runtime tools:

- `bluetoothctl`
- `pactl`

Optional but used by the `fix` command if present in your user session:

- `systemctl --user`
- running `pipewire`, `pipewire-pulse`, and `wireplumber`

No Python runtime is required.

## Install

### Install from GitHub with Cargo

```bash
cargo install --git https://github.com/1jehuang/linux-airpods-cli.git --bin airpods
```

### Install locally

```bash
cargo install --path . --root ~/.local --bin airpods
```

That exposes the `airpods` command.

## Usage

### Auto-detect your AirPods-like device

```bash
airpods status
airpods connect
airpods disconnect
airpods fix
```

### Target a specific device

```bash
airpods --mac 74:77:86:57:67:2A status
airpods --mac 74:77:86:57:67:2A connect
```

You can also set a default device once:

```bash
export AIRPODS_MAC=74:77:86:57:67:2A
```

### Commands

#### `airpods devices`
List AirPods-like Bluetooth devices.

#### `airpods status`
Show Bluetooth state, PipeWire sink, card, and whether the device is the current default output.

#### `airpods connect`
Connect the device, wait for its PipeWire sink, and set it as the default sink.

Useful flags:

```bash
airpods connect --pair
airpods connect --no-default
airpods connect --no-move
airpods connect --json
```

#### `airpods disconnect`
Disconnect the device. By default it also switches back to a non-Bluetooth fallback sink if the AirPods were the default output.

#### `airpods set-default`
Set the AirPods sink as the default output and move active streams to it.

#### `airpods sink`
Print the active AirPods sink name.

#### `airpods battery`
Query exact battery information from the AirPods over AAP.

```bash
airpods battery
airpods battery --json
```

#### `airpods keys`
Request Magic Cloud Keys from the AirPods over AAP.

```bash
airpods keys
airpods keys --json
```

#### `airpods monitor`
Run a persistent AirPods AAP monitor that writes a JSON cache file.

```bash
airpods monitor
airpods monitor --once
```

This is useful for status bars like Waybar, where repeated one-shot control-channel connections can be less reliable than a single long-lived session.

#### `airpods fix`
Run the recovery flow that has proven useful for broken AirPods sessions:

1. stop Bluetooth scanning
2. disconnect the AirPods
3. restart the user audio stack
4. reconnect the AirPods
5. wait for the sink to appear
6. make it the default sink
7. move active streams

```bash
airpods fix
airpods fix --no-restart-audio
airpods fix --json
```

## Examples

### Print status as JSON

```bash
airpods status --json
```

### Pair only if direct connect fails

```bash
airpods connect --pair
```

### Connect but leave your current default sink alone

```bash
airpods connect --no-default
```

## Design notes

This project intentionally stays close to Linux primitives:

- BlueZ via `bluetoothctl`
- PipeWire / PulseAudio via `pactl`

That keeps the failure surface smaller than relying on larger third-party helper services.

## Native AAP support

`linux-airpods-cli` now speaks the AirPods control channel itself over authenticated / encrypted classic L2CAP.

That powers:

- exact battery packets
- metadata packets
- Magic Cloud Key retrieval

Examples:

```bash
airpods battery
airpods battery --json
airpods keys
airpods monitor --once
```

You can also fold the AAP data into `status`:

```bash
airpods status --aap
airpods status --aap --json
```

## What is not implemented yet

A full long-running monitor daemon and BLE advertisement decryption are not included yet.

Those are good next steps, but the CLI already owns the native control-channel path itself instead of relying on LibrePods.

## Systemd user service

You can keep a persistent monitor running through a user service instead of spawning it from your status bar.

An example unit lives at:

- `contrib/systemd/airpods-monitor.service.example`

Example install:

```bash
mkdir -p ~/.config/systemd/user
cp contrib/systemd/airpods-monitor.service.example ~/.config/systemd/user/airpods-monitor.service
systemctl --user daemon-reload
systemctl --user enable --now airpods-monitor.service
```

Then your bar can just read the cache file written by the service.

## Development

Run tests:

```bash
cargo test
```
