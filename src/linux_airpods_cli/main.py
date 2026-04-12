from __future__ import annotations

import argparse
import json
import os
import re
import shutil
import struct as pystruct
import subprocess
import sys
import tempfile
import time
import wave
from functools import lru_cache
from dataclasses import dataclass
from pathlib import Path
from typing import Any

from .aap import AAPError, AAPSession


class AirPodsCliError(RuntimeError):
    pass


@dataclass
class CommandResult:
    code: int
    stdout: str
    stderr: str


@dataclass
class Device:
    mac: str
    name: str


@dataclass
class Sink:
    sink_id: int
    name: str
    state: str


@dataclass
class Card:
    card_id: int
    name: str


def require_command(name: str) -> None:
    if shutil.which(name) is None:
        raise AirPodsCliError(f"Required command not found in PATH: {name}")


def run_command(args: list[str], *, timeout: int = 15, check: bool = False) -> CommandResult:
    try:
        completed = subprocess.run(
            args,
            text=True,
            capture_output=True,
            timeout=timeout,
            check=False,
        )
    except subprocess.TimeoutExpired as exc:
        raise AirPodsCliError(f"Command timed out: {' '.join(args)}") from exc
    except OSError as exc:
        raise AirPodsCliError(f"Failed to execute command: {' '.join(args)}") from exc

    result = CommandResult(completed.returncode, completed.stdout, completed.stderr)
    if check and result.code != 0:
        stderr = result.stderr.strip()
        stdout = result.stdout.strip()
        details = stderr or stdout or f"exit code {result.code}"
        raise AirPodsCliError(f"Command failed: {' '.join(args)}\n{details}")
    return result


def parse_bluetooth_devices(output: str) -> list[Device]:
    devices: list[Device] = []
    for line in output.splitlines():
        line = re.sub(r"\x1b\[[0-9;]*m", "", line).strip()
        if not line.startswith("Device "):
            continue
        parts = line.split(maxsplit=2)
        if len(parts) < 3:
            continue
        mac = parts[1].strip()
        name = parts[2].strip()
        devices.append(Device(mac=mac, name=name))
    return devices


def parse_bluetooth_info(output: str) -> dict[str, str]:
    info: dict[str, str] = {}
    for raw_line in output.splitlines():
        line = re.sub(r"\x1b\[[0-9;]*m", "", raw_line).rstrip()
        if not line or ":" not in line or line.startswith("Device "):
            continue
        key, value = line.split(":", 1)
        info[key.strip()] = value.strip()
    return info


def _strip_tree_prefix(raw_line: str) -> str:
    return re.sub(r"^[\s├└─]+", "", raw_line).strip()


@lru_cache(maxsize=256)
def bluez_device_path(mac: str) -> str | None:
    if shutil.which("busctl") is None:
        return None

    slug = normalize_mac_underscore(mac)
    result = run_command(["busctl", "--system", "tree", "org.bluez"], timeout=10)
    if result.code != 0:
        return None

    suffix = f"/dev_{slug}"
    for raw_line in result.stdout.splitlines():
        path = _strip_tree_prefix(raw_line)
        if path.endswith(suffix):
            return path
    return None


def bluez_device_property(mac: str, prop: str) -> Any | None:
    path = bluez_device_path(mac)
    if path is None:
        return None

    result = run_command(
        [
            "busctl",
            "--json=short",
            "--system",
            "get-property",
            "org.bluez",
            path,
            "org.bluez.Device1",
            prop,
        ],
        timeout=10,
    )
    if result.code != 0:
        return None

    try:
        payload = json.loads(result.stdout)
    except json.JSONDecodeError:
        return None
    return payload.get("data")


def _stringify_bluez_value(value: Any) -> str:
    if isinstance(value, bool):
        return "yes" if value else "no"
    if isinstance(value, list):
        return " ".join(str(item) for item in value)
    return str(value)


def bluetooth_info_via_busctl(mac: str) -> dict[str, str]:
    property_map = {
        "Name": "Name",
        "Alias": "Alias",
        "Address": "Address",
        "AddressType": "AddressType",
        "Paired": "Paired",
        "Bonded": "Bonded",
        "Trusted": "Trusted",
        "Blocked": "Blocked",
        "Connected": "Connected",
        "LegacyPairing": "LegacyPairing",
        "CablePairing": "CablePairing",
        "Modalias": "Modalias",
        "PreferredBearer": "PreferredBearer",
        "ServicesResolved": "ServicesResolved",
    }

    info: dict[str, str] = {}
    for prop, key in property_map.items():
        value = bluez_device_property(mac, prop)
        if value is None:
            continue
        info[key] = _stringify_bluez_value(value)

    if "Bonded" in info:
        info.setdefault("BREDR.Bonded", info["Bonded"])
    if "Connected" in info:
        info.setdefault("BREDR.Connected", info["Connected"])

    return info


def parse_short_sinks(output: str) -> list[Sink]:
    sinks: list[Sink] = []
    for line in output.splitlines():
        parts = line.split()
        if len(parts) < 5:
            continue
        try:
            sink_id = int(parts[0])
        except ValueError:
            continue
        sinks.append(Sink(sink_id=sink_id, name=parts[1], state=parts[-1]))
    return sinks


def parse_short_cards(output: str) -> list[Card]:
    cards: list[Card] = []
    for line in output.splitlines():
        parts = line.split()
        if len(parts) < 2:
            continue
        try:
            card_id = int(parts[0])
        except ValueError:
            continue
        cards.append(Card(card_id=card_id, name=parts[1]))
    return cards


def normalize_mac_colon(mac: str) -> str:
    return mac.upper().replace("_", ":")


def normalize_mac_underscore(mac: str) -> str:
    return normalize_mac_colon(mac).replace(":", "_")


def airpods_like_name(name: str) -> bool:
    lowered = name.lower()
    return "airpods" in lowered or "beats" in lowered


def bluetooth_devices() -> list[Device]:
    result = run_command(["bluetoothctl", "devices"], timeout=10, check=True)
    return parse_bluetooth_devices(result.stdout)


def bluetooth_info(mac: str) -> dict[str, str]:
    info = bluetooth_info_via_busctl(mac)
    if info:
        return info

    result = run_command(["bluetoothctl", "info", mac], timeout=10)
    if result.code != 0:
        return {}
    return parse_bluetooth_info(result.stdout)


def connected(mac: str) -> bool:
    info = bluetooth_info(mac)
    return info.get("Connected") == "yes"


def discover_device(requested_mac: str | None, requested_name: str | None) -> Device:
    if requested_mac:
        mac = normalize_mac_colon(requested_mac)
        info = bluetooth_info(mac)
        name = info.get("Name") or info.get("Alias") or mac
        return Device(mac=mac, name=name)

    devices = bluetooth_devices()
    if requested_name:
        lowered = requested_name.lower()
        matches = [d for d in devices if lowered in d.name.lower()]
        if not matches:
            raise AirPodsCliError(f"No Bluetooth device matched name: {requested_name}")
        if len(matches) == 1:
            return matches[0]
        connected_matches = [d for d in matches if connected(d.mac)]
        if len(connected_matches) == 1:
            return connected_matches[0]
        raise AirPodsCliError(
            "Multiple devices matched the requested name. Use --mac to choose one explicitly."
        )

    env_mac = os.environ.get("AIRPODS_MAC")
    if env_mac:
        return discover_device(env_mac, None)

    candidates = [d for d in devices if airpods_like_name(d.name)]
    if not candidates:
        raise AirPodsCliError(
            "No AirPods-like devices found. Pair first or pass --mac / AIRPODS_MAC."
        )
    if len(candidates) == 1:
        return candidates[0]

    connected_candidates = [d for d in candidates if connected(d.mac)]
    if len(connected_candidates) == 1:
        return connected_candidates[0]

    raise AirPodsCliError(
        "Multiple AirPods-like devices found. Use --mac to choose one explicitly."
    )


def current_default_sink() -> str | None:
    result = run_command(["pactl", "get-default-sink"], timeout=10)
    if result.code != 0:
        return None
    value = result.stdout.strip()
    return value or None


def list_sinks() -> list[Sink]:
    result = run_command(["pactl", "list", "short", "sinks"], timeout=10, check=True)
    return parse_short_sinks(result.stdout)


def list_cards() -> list[Card]:
    result = run_command(["pactl", "list", "short", "cards"], timeout=10, check=True)
    return parse_short_cards(result.stdout)


def list_sink_inputs() -> list[int]:
    result = run_command(["pactl", "list", "short", "sink-inputs"], timeout=10)
    if result.code != 0:
        return []
    sink_inputs: list[int] = []
    for line in result.stdout.splitlines():
        parts = line.split()
        if not parts:
            continue
        try:
            sink_inputs.append(int(parts[0]))
        except ValueError:
            continue
    return sink_inputs


def find_airpods_sink(mac: str) -> Sink | None:
    mac_colon = normalize_mac_colon(mac)
    mac_underscore = normalize_mac_underscore(mac)
    for sink in list_sinks():
        if mac_colon in sink.name or mac_underscore in sink.name:
            return sink
    return None


def find_airpods_card(mac: str) -> Card | None:
    mac_underscore = normalize_mac_underscore(mac)
    for card in list_cards():
        if mac_underscore in card.name:
            return card
    return None


def find_fallback_sink(exclude_name: str | None) -> Sink | None:
    for sink in list_sinks():
        if exclude_name and sink.name == exclude_name:
            continue
        if sink.name.startswith("bluez_"):
            continue
        return sink
    return None


def bluetoothctl_command(*subcommand: str, timeout: int = 20, check: bool = False) -> CommandResult:
    return run_command(["bluetoothctl", *subcommand], timeout=timeout, check=check)


def set_default_sink(sink_name: str, move_streams: bool) -> None:
    run_command(["pactl", "set-default-sink", sink_name], timeout=10, check=True)
    if move_streams:
        for sink_input in list_sink_inputs():
            run_command(
                ["pactl", "move-sink-input", str(sink_input), sink_name],
                timeout=10,
            )


def wait_for_sink(mac: str, timeout_seconds: float = 12.0, poll_interval: float = 0.5) -> Sink | None:
    deadline = time.monotonic() + timeout_seconds
    while time.monotonic() < deadline:
        sink = find_airpods_sink(mac)
        if sink is not None:
            return sink
        time.sleep(poll_interval)
    return None


def restart_audio_stack() -> None:
    run_command(
        ["systemctl", "--user", "restart", "wireplumber", "pipewire", "pipewire-pulse"],
        timeout=20,
        check=True,
    )


def ensure_a2dp_profile(mac: str) -> Card | None:
    card = find_airpods_card(mac)
    if card is None:
        return None
    run_command(["pactl", "set-card-profile", card.name, "a2dp-sink"], timeout=10)
    return card


def do_connect(device: Device, pair: bool) -> tuple[dict[str, Any], int]:
    bluetoothctl_command("scan", "off", timeout=8)

    if connected(device.mac):
        return {"connected": True, "already_connected": True}, 0

    connect_result = bluetoothctl_command("connect", device.mac, timeout=20)
    combined = (connect_result.stdout + "\n" + connect_result.stderr).lower()
    if connect_result.code == 0 or "connection successful" in combined:
        return {"connected": True, "already_connected": False}, 0

    if not pair:
        return {
            "connected": False,
            "error": combined.strip() or "connect failed",
        }, 1

    bluetoothctl_command("pair", device.mac, timeout=25)
    bluetoothctl_command("trust", device.mac, timeout=10)
    connect_result = bluetoothctl_command("connect", device.mac, timeout=20)
    combined = (connect_result.stdout + "\n" + connect_result.stderr).lower()
    if connect_result.code == 0 or "connection successful" in combined:
        return {"connected": True, "paired_during_connect": True}, 0

    return {
        "connected": False,
        "error": combined.strip() or "connect failed after pair",
    }, 1


def do_disconnect(device: Device) -> tuple[dict[str, Any], int]:
    result = bluetoothctl_command("disconnect", device.mac, timeout=12)
    combined = (result.stdout + "\n" + result.stderr).lower()
    disconnected = result.code == 0 or "successful" in combined or "not connected" in combined
    return {"disconnected": disconnected}, 0 if disconnected else 1


def build_status(device: Device) -> dict[str, Any]:
    info = bluetooth_info(device.mac)
    sink = find_airpods_sink(device.mac)
    card = find_airpods_card(device.mac)
    default_sink = current_default_sink()
    return {
        "name": info.get("Name") or device.name,
        "alias": info.get("Alias") or device.name,
        "mac": device.mac,
        "paired": info.get("Paired") == "yes",
        "trusted": info.get("Trusted") == "yes",
        "connected": info.get("Connected") == "yes",
        "breder_connected": info.get("BREDR.Connected") == "yes",
        "sink": sink.name if sink else None,
        "sink_state": sink.state if sink else None,
        "card": card.name if card else None,
        "default_sink": default_sink,
        "default_is_airpods": sink is not None and default_sink == sink.name,
    }


def state_dir() -> Path:
    base = os.environ.get("XDG_STATE_HOME")
    if base:
        return Path(base) / "linux-airpods-cli"
    return Path.home() / ".local" / "state" / "linux-airpods-cli"


def default_cache_file(mac: str) -> str:
    slug = normalize_mac_underscore(mac)
    return str(state_dir() / f"linux-airpods-cli-{slug}.json")


def write_json_file(file_path: str, payload: dict[str, Any]) -> bool:
    target_path = Path(file_path)
    target_path.parent.mkdir(parents=True, exist_ok=True)
    tmp_path = f"{file_path}.tmp"
    try:
        with open(tmp_path, "w", encoding="utf-8") as handle:
            json.dump(payload, handle)
        os.replace(tmp_path, file_path)
        return True
    except OSError as exc:
        print(f"warning: failed to write cache file {target_path}: {exc}", file=sys.stderr)
        try:
            if os.path.exists(tmp_path):
                os.remove(tmp_path)
        except OSError:
            pass
        return False


def print_human_status(status: dict[str, Any]) -> None:
    print(f"Name:            {status['name']}")
    print(f"MAC:             {status['mac']}")
    print(f"Paired:          {yes_no(status['paired'])}")
    print(f"Trusted:         {yes_no(status['trusted'])}")
    print(f"Connected:       {yes_no(status['connected'])}")
    print(f"BR/EDR:          {yes_no(status['breder_connected'])}")
    print(f"Card:            {status['card'] or '-'}")
    print(f"Sink:            {status['sink'] or '-'}")
    print(f"Sink state:      {status['sink_state'] or '-'}")
    print(f"Default sink:    {status['default_sink'] or '-'}")
    print(f"Default AirPods: {yes_no(status['default_is_airpods'])}")
    if status.get("battery"):
        battery = status["battery"]
        print("Battery:")
        for name in ("left", "right", "case", "headset"):
            component = battery.get(name)
            if not component:
                continue
            if component.get("available"):
                level = component.get("level")
                charging = " charging" if component.get("charging") else ""
                print(f"  {name:<7} {level}%{charging}")
            else:
                print(f"  {name:<7} unavailable")
    if status.get("metadata"):
        metadata = status["metadata"]
        print("Metadata:")
        print(f"  Model number:  {metadata.get('model_number') or '-'}")
        print(f"  Manufacturer:  {metadata.get('manufacturer') or '-'}")
    if status.get("noise_control_mode") is not None:
        print(f"Noise control:   {status['noise_control_mode']}")
    if status.get("conversational_awareness_enabled") is not None:
        print(f"CA enabled:      {yes_no(status['conversational_awareness_enabled'])}")


def yes_no(value: bool) -> str:
    return "yes" if value else "no"


def wake_airpods_sink(mac: str) -> bool:
    sink = find_airpods_sink(mac)
    if sink is None:
        return False

    sample_path = None
    try:
        with tempfile.NamedTemporaryFile(prefix="linux-airpods-cli-", suffix=".wav", delete=False) as handle:
            sample_path = handle.name
        with wave.open(sample_path, "wb") as wav_file:
            wav_file.setnchannels(2)
            wav_file.setsampwidth(2)
            wav_file.setframerate(48000)
            silence_frame = pystruct.pack("<hh", 0, 0)
            wav_file.writeframes(silence_frame * int(48000 * 0.2))

        if shutil.which("paplay"):
            result = subprocess.run(
                ["paplay", sample_path],
                env={**os.environ, "PULSE_SINK": sink.name},
                timeout=5,
                capture_output=True,
                text=True,
                check=False,
            )
            return result.returncode == 0
        if shutil.which("pw-play"):
            result = subprocess.run(
                ["pw-play", "--target", sink.name, sample_path],
                timeout=5,
                capture_output=True,
                text=True,
                check=False,
            )
            return result.returncode == 0
        return False
    except Exception:
        return False
    finally:
        if sample_path and os.path.exists(sample_path):
            try:
                os.remove(sample_path)
            except OSError:
                pass


def cmd_status(args: argparse.Namespace) -> int:
    device = discover_device(args.mac, args.name)
    status = build_status(device)
    if args.aap:
        if not status["connected"]:
            raise AirPodsCliError("AirPods are not connected, so AAP status is unavailable.")
        try:
            with AAPSession(device.mac, timeout_seconds=args.wait) as session:
                aap_state = session.query(request_keys=False, capture_packets=args.raw_packets)
        except AAPError as exc:
            raise AirPodsCliError(str(exc)) from exc
        status.update(aap_state.to_dict())
    if args.json:
        print(json.dumps(status, indent=2, sort_keys=True))
    else:
        print_human_status(status)
    return 0


def cmd_devices(args: argparse.Namespace) -> int:
    devices = [d for d in bluetooth_devices() if airpods_like_name(d.name)]
    payload = [{"mac": d.mac, "name": d.name, "connected": connected(d.mac)} for d in devices]
    if args.json:
        print(json.dumps(payload, indent=2, sort_keys=True))
    else:
        if not payload:
            print("No AirPods-like devices found.")
        for item in payload:
            suffix = " (connected)" if item["connected"] else ""
            print(f"{item['mac']}  {item['name']}{suffix}")
    return 0


def cmd_sink(args: argparse.Namespace) -> int:
    device = discover_device(args.mac, args.name)
    sink = find_airpods_sink(device.mac)
    if sink is None:
        raise AirPodsCliError("No AirPods sink exists yet. Connect first.")
    print(sink.name)
    return 0


def cmd_connect(args: argparse.Namespace) -> int:
    device = discover_device(args.mac, args.name)
    payload, code = do_connect(device, pair=args.pair)
    if code != 0:
        raise AirPodsCliError(payload.get("error", "Failed to connect"))

    ensure_a2dp_profile(device.mac)
    sink = wait_for_sink(device.mac, timeout_seconds=args.wait)
    if sink is None:
        raise AirPodsCliError("Bluetooth connected, but no PipeWire sink appeared.")

    if args.set_default:
        set_default_sink(sink.name, move_streams=args.move)

    status = build_status(device)
    if args.json:
        print(json.dumps(status, indent=2, sort_keys=True))
    else:
        print(f"Connected to {status['name']}")
        print(f"Sink: {status['sink']}")
        if args.set_default:
            print(f"Default sink set to: {status['sink']}")
    return 0


def cmd_disconnect(args: argparse.Namespace) -> int:
    device = discover_device(args.mac, args.name)
    sink = find_airpods_sink(device.mac)
    was_default = sink is not None and current_default_sink() == sink.name

    payload, code = do_disconnect(device)
    if code != 0:
        raise AirPodsCliError("Failed to disconnect device")

    if was_default and args.fallback:
        fallback = find_fallback_sink(exclude_name=sink.name if sink else None)
        if fallback is not None:
            set_default_sink(fallback.name, move_streams=args.move)

    if args.json:
        print(json.dumps(payload, indent=2, sort_keys=True))
    else:
        print(f"Disconnected {device.name}")
    return 0


def cmd_set_default(args: argparse.Namespace) -> int:
    device = discover_device(args.mac, args.name)
    sink = find_airpods_sink(device.mac)
    if sink is None:
        raise AirPodsCliError("No AirPods sink exists yet. Connect first.")
    set_default_sink(sink.name, move_streams=args.move)
    print(sink.name)
    return 0


def cmd_fix(args: argparse.Namespace) -> int:
    device = discover_device(args.mac, args.name)
    bluetoothctl_command("scan", "off", timeout=8)
    bluetoothctl_command("disconnect", device.mac, timeout=10)
    if args.restart_audio:
        restart_audio_stack()
        time.sleep(args.restart_wait)
    payload, code = do_connect(device, pair=False)
    if code != 0:
        raise AirPodsCliError(payload.get("error", "Reconnect failed during fix"))

    ensure_a2dp_profile(device.mac)
    sink = wait_for_sink(device.mac, timeout_seconds=args.wait)
    if sink is None:
        raise AirPodsCliError("Reconnect succeeded, but no AirPods sink appeared.")
    set_default_sink(sink.name, move_streams=args.move)

    status = build_status(device)
    if args.json:
        print(json.dumps(status, indent=2, sort_keys=True))
    else:
        print(f"Recovered {status['name']}")
        print(f"Sink: {status['sink']}")
        print(f"Default sink: {status['default_sink']}")
    return 0


def cmd_battery(args: argparse.Namespace) -> int:
    device = discover_device(args.mac, args.name)
    if not connected(device.mac):
        raise AirPodsCliError("AirPods must be connected before querying AAP battery.")
    if args.wake:
        wake_airpods_sink(device.mac)
    try:
        with AAPSession(device.mac, timeout_seconds=args.wait) as session:
            state = session.query(request_keys=False, capture_packets=args.raw_packets)
    except AAPError as exc:
        raise AirPodsCliError(str(exc)) from exc
    payload = {
        "name": device.name,
        "mac": device.mac,
        **state.to_dict(),
    }
    if state.battery is None:
        raise AirPodsCliError("No battery packet received from the AirPods.")
    if args.json:
        print(json.dumps(payload, indent=2, sort_keys=True))
    else:
        battery = state.battery.to_dict()
        print(f"Name: {device.name}")
        print(f"MAC:  {device.mac}")
        for name in ("left", "right", "case", "headset"):
            component = battery[name]
            if component["available"]:
                charging = " charging" if component["charging"] else ""
                print(f"{name.capitalize():<7} {component['level']}%{charging}")
            else:
                print(f"{name.capitalize():<7} unavailable")
    return 0


def cmd_keys(args: argparse.Namespace) -> int:
    device = discover_device(args.mac, args.name)
    if not connected(device.mac):
        raise AirPodsCliError("AirPods must be connected before requesting AAP keys.")
    if args.wake:
        wake_airpods_sink(device.mac)
    try:
        with AAPSession(device.mac, timeout_seconds=args.wait) as session:
            state = session.query(request_keys=True, capture_packets=args.raw_packets)
    except AAPError as exc:
        raise AirPodsCliError(str(exc)) from exc
    if state.magic_keys is None:
        raise AirPodsCliError("No Magic Cloud Keys packet received from the AirPods.")
    payload = {
        "name": device.name,
        "mac": device.mac,
        **state.magic_keys.to_dict(),
    }
    if args.json:
        print(json.dumps(payload, indent=2, sort_keys=True))
    else:
        print(f"Name:    {device.name}")
        print(f"MAC:     {device.mac}")
        print(f"IRK:     {payload['irk']}")
        print(f"Enc key: {payload['enc_key']}")
    return 0


def cmd_monitor(args: argparse.Namespace) -> int:
    device = discover_device(args.mac, args.name)
    cache_file = args.cache_file or default_cache_file(device.mac)

    while True:
        is_connected = connected(device.mac)
        if not is_connected:
            write_json_file(cache_file, {
                "name": device.name,
                "mac": device.mac,
                "connected": False,
                "timestamp": time.time(),
            })
            if args.once:
                raise AirPodsCliError("AirPods must be connected before starting monitor.")
            time.sleep(args.retry_interval)
            continue

        try:
            if args.wake:
                wake_airpods_sink(device.mac)
            with AAPSession(device.mac, timeout_seconds=args.wait) as session:
                state = session.query(request_keys=args.request_keys, capture_packets=args.raw_packets)
                payload = {
                    "name": device.name,
                    "mac": device.mac,
                    "connected": True,
                    "timestamp": time.time(),
                    **state.to_dict(),
                }
                write_json_file(cache_file, payload)
                if args.once:
                    print(json.dumps(payload, indent=2, sort_keys=True))
                    return 0

                last_notification_request = time.monotonic()
                while True:
                    got_packet = session.read_next(state, timeout=args.poll_interval, capture_packets=args.raw_packets)
                    now = time.monotonic()
                    if got_packet:
                        payload = {
                            "name": device.name,
                            "mac": device.mac,
                            "connected": True,
                            "timestamp": time.time(),
                            **state.to_dict(),
                        }
                        write_json_file(cache_file, payload)
                    elif now - last_notification_request >= args.refresh_interval:
                        session.request_notifications()
                        last_notification_request = now
                    if not connected(device.mac):
                        break
        except AAPError as exc:
            write_json_file(cache_file, {
                "name": device.name,
                "mac": device.mac,
                "connected": connected(device.mac),
                "error": str(exc),
                "timestamp": time.time(),
            })
            if args.once:
                raise AirPodsCliError(str(exc)) from exc
            time.sleep(args.retry_interval)
            continue

        if args.once:
            return 0
        time.sleep(args.retry_interval)


def build_parser() -> argparse.ArgumentParser:
    parser = argparse.ArgumentParser(
        prog="airpods",
        description="A small AirPods CLI for Linux using BlueZ and PipeWire.",
    )
    parser.add_argument("--mac", help="Target Bluetooth MAC address")
    parser.add_argument("--name", help="Target device name substring")

    subparsers = parser.add_subparsers(dest="command", required=True)

    status_parser = subparsers.add_parser("status", help="Show AirPods Bluetooth and audio state")
    status_parser.add_argument("--json", action="store_true", help="Emit machine-readable JSON")
    status_parser.add_argument("--aap", action="store_true", help="Also query AAP battery / metadata over the native AirPods control channel")
    status_parser.add_argument("--raw-packets", action="store_true", help="Include raw AAP packets when using --aap")
    status_parser.add_argument("--wait", type=float, default=12.0, help="Seconds to wait for AAP packets")
    status_parser.set_defaults(func=cmd_status)

    devices_parser = subparsers.add_parser("devices", help="List AirPods-like Bluetooth devices")
    devices_parser.add_argument("--json", action="store_true", help="Emit machine-readable JSON")
    devices_parser.set_defaults(func=cmd_devices)

    sink_parser = subparsers.add_parser("sink", help="Print the current AirPods sink name")
    sink_parser.set_defaults(func=cmd_sink)

    connect_parser = subparsers.add_parser("connect", help="Connect and optionally set default output")
    connect_parser.add_argument("--pair", action="store_true", help="Try pairing if connect fails")
    connect_parser.add_argument("--wait", type=float, default=12.0, help="Seconds to wait for the sink")
    connect_parser.add_argument(
        "--no-default",
        action="store_false",
        dest="set_default",
        help="Do not set the AirPods sink as default",
    )
    connect_parser.add_argument(
        "--no-move",
        action="store_false",
        dest="move",
        help="Do not move active streams to the new default sink",
    )
    connect_parser.add_argument("--json", action="store_true", help="Emit machine-readable JSON")
    connect_parser.set_defaults(func=cmd_connect, set_default=True, move=True)

    disconnect_parser = subparsers.add_parser("disconnect", help="Disconnect the AirPods")
    disconnect_parser.add_argument(
        "--no-fallback",
        action="store_false",
        dest="fallback",
        help="Do not switch to a non-Bluetooth fallback sink",
    )
    disconnect_parser.add_argument(
        "--no-move",
        action="store_false",
        dest="move",
        help="Do not move active streams when changing fallback sink",
    )
    disconnect_parser.add_argument("--json", action="store_true", help="Emit machine-readable JSON")
    disconnect_parser.set_defaults(func=cmd_disconnect, fallback=True, move=True)

    set_default_parser = subparsers.add_parser("set-default", help="Set the AirPods sink as the default output")
    set_default_parser.add_argument(
        "--no-move",
        action="store_false",
        dest="move",
        help="Do not move active streams to the AirPods sink",
    )
    set_default_parser.set_defaults(func=cmd_set_default, move=True)

    fix_parser = subparsers.add_parser("fix", help="Recover a broken AirPods audio transport")
    fix_parser.add_argument(
        "--no-restart-audio",
        action="store_false",
        dest="restart_audio",
        help="Skip restarting pipewire / wireplumber",
    )
    fix_parser.add_argument("--restart-wait", type=float, default=3.0, help="Seconds to wait after audio restart")
    fix_parser.add_argument("--wait", type=float, default=12.0, help="Seconds to wait for the sink")
    fix_parser.add_argument(
        "--no-move",
        action="store_false",
        dest="move",
        help="Do not move active streams to the recovered sink",
    )
    fix_parser.add_argument("--json", action="store_true", help="Emit machine-readable JSON")
    fix_parser.set_defaults(func=cmd_fix, restart_audio=True, move=True)

    battery_parser = subparsers.add_parser("battery", help="Query exact AirPods battery over the native AAP control channel")
    battery_parser.add_argument("--wait", type=float, default=12.0, help="Seconds to wait for AAP packets")
    battery_parser.add_argument("--json", action="store_true", help="Emit machine-readable JSON")
    battery_parser.add_argument("--raw-packets", action="store_true", help="Include raw AAP packets in the JSON output")
    battery_parser.add_argument("--no-wake", action="store_false", dest="wake", help="Skip the silent sink wake-up before opening AAP")
    battery_parser.set_defaults(func=cmd_battery, wake=True)

    keys_parser = subparsers.add_parser("keys", help="Request AirPods Magic Cloud Keys over the native AAP control channel")
    keys_parser.add_argument("--wait", type=float, default=12.0, help="Seconds to wait for AAP packets")
    keys_parser.add_argument("--json", action="store_true", help="Emit machine-readable JSON")
    keys_parser.add_argument("--raw-packets", action="store_true", help="Include raw AAP packets in the JSON output")
    keys_parser.add_argument("--no-wake", action="store_false", dest="wake", help="Skip the silent sink wake-up before opening AAP")
    keys_parser.set_defaults(func=cmd_keys, wake=True)

    monitor_parser = subparsers.add_parser("monitor", help="Run a persistent native AAP monitor and write a local cache file")
    monitor_parser.add_argument("--cache-file", help="Path to the JSON cache file")
    monitor_parser.add_argument("--wait", type=float, default=12.0, help="Seconds to wait for the initial AAP setup")
    monitor_parser.add_argument("--poll-interval", type=float, default=1.0, help="Seconds to wait for the next packet")
    monitor_parser.add_argument("--refresh-interval", type=float, default=20.0, help="How often to re-request notifications while idle")
    monitor_parser.add_argument("--retry-interval", type=float, default=3.0, help="Retry delay after disconnect or monitor error")
    monitor_parser.add_argument("--request-keys", action="store_true", help="Also request Magic Cloud Keys during monitor startup")
    monitor_parser.add_argument("--raw-packets", action="store_true", help="Store raw AAP packets in the cache file")
    monitor_parser.add_argument("--no-wake", action="store_false", dest="wake", help="Skip the silent sink wake-up before opening AAP")
    monitor_parser.add_argument("--once", action="store_true", help="Prime the cache once and exit")
    monitor_parser.set_defaults(func=cmd_monitor, wake=True)

    return parser


def main(argv: list[str] | None = None) -> int:
    require_command("bluetoothctl")
    require_command("pactl")

    parser = build_parser()
    args = parser.parse_args(argv)

    try:
        return args.func(args)
    except AirPodsCliError as exc:
        print(f"error: {exc}", file=sys.stderr)
        return 1


if __name__ == "__main__":
    raise SystemExit(main())
