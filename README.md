# linux-airpods-cli

A small AirPods CLI for Linux using only system Bluetooth and audio tooling.

It intentionally avoids depending on helper daemons like LibrePods. The first version focuses on the parts Linux already exposes reliably through BlueZ and PipeWire:

- device discovery
- connect / disconnect
- default sink switching
- active stream movement
- transport recovery
- status inspection

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

No third-party Python packages are required.

## Install

### Local editable install

```bash
python -m pip install -e .
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

## What is not implemented yet

Battery reporting is not included in `v0.1.0`.

That is deliberate. Reliable AirPods battery support on Linux usually requires Apple-specific protocol handling that is not exposed cleanly through standard BlueZ audio interfaces. A future version can add a native implementation rather than depending on an external helper daemon.

## Development

Run tests:

```bash
python -m unittest discover -s tests -v
```
