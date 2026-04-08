from __future__ import annotations

import socket
import struct
import time
from dataclasses import dataclass, field


AAPP_HANDSHAKE = bytes.fromhex("00000400010002000000000000000000")
AAPP_SET_SPECIFIC_FEATURES = bytes.fromhex("040004004d00d700000000000000")
AAPP_REQUEST_NOTIFICATIONS = bytes.fromhex("040004000f00ffffffffff")
AAPP_REQUEST_MAGIC_KEYS = bytes.fromhex("0400040030000500")

HANDSHAKE_ACK_PREFIX = bytes.fromhex("01000400")
FEATURES_ACK_PREFIX = bytes.fromhex("040004002b00")
BATTERY_PREFIX = bytes.fromhex("040004000400")
METADATA_PREFIX = bytes.fromhex("040004001d")
MAGIC_KEYS_PREFIX = bytes.fromhex("04000400310002")
NOISE_CONTROL_PREFIX = bytes.fromhex("0400040009000d")
CONVERSATIONAL_AWARENESS_PREFIX = bytes.fromhex("04000400090028")
EAR_DETECTION_PREFIX = bytes.fromhex("040004000600")


class AAPError(RuntimeError):
    pass


@dataclass
class BatteryComponent:
    level: int | None = None
    charging: bool = False
    available: bool = False
    status_code: int | None = None

    def to_dict(self) -> dict[str, int | bool | None]:
        return {
            "level": self.level,
            "charging": self.charging,
            "available": self.available,
            "status_code": self.status_code,
        }


@dataclass
class BatteryStatus:
    left: BatteryComponent = field(default_factory=BatteryComponent)
    right: BatteryComponent = field(default_factory=BatteryComponent)
    case: BatteryComponent = field(default_factory=BatteryComponent)
    headset: BatteryComponent = field(default_factory=BatteryComponent)
    primary: str | None = None
    secondary: str | None = None

    def to_dict(self) -> dict[str, object]:
        return {
            "left": self.left.to_dict(),
            "right": self.right.to_dict(),
            "case": self.case.to_dict(),
            "headset": self.headset.to_dict(),
            "primary": self.primary,
            "secondary": self.secondary,
        }


@dataclass
class Metadata:
    device_name: str | None = None
    model_number: str | None = None
    manufacturer: str | None = None

    def to_dict(self) -> dict[str, str | None]:
        return {
            "device_name": self.device_name,
            "model_number": self.model_number,
            "manufacturer": self.manufacturer,
        }


@dataclass
class MagicKeys:
    irk: bytes
    enc_key: bytes

    def to_dict(self) -> dict[str, str]:
        return {
            "irk": self.irk.hex(),
            "enc_key": self.enc_key.hex(),
        }


@dataclass
class SessionState:
    battery: BatteryStatus | None = None
    metadata: Metadata | None = None
    magic_keys: MagicKeys | None = None
    noise_control_mode: int | None = None
    conversational_awareness_enabled: bool | None = None
    raw_packets: list[str] = field(default_factory=list)

    def to_dict(self) -> dict[str, object]:
        return {
            "battery": self.battery.to_dict() if self.battery else None,
            "metadata": self.metadata.to_dict() if self.metadata else None,
            "magic_keys": self.magic_keys.to_dict() if self.magic_keys else None,
            "noise_control_mode": self.noise_control_mode,
            "conversational_awareness_enabled": self.conversational_awareness_enabled,
            "raw_packets": list(self.raw_packets),
        }


def _component_name(component_type: int) -> str | None:
    return {
        0x01: "headset",
        0x02: "right",
        0x04: "left",
        0x08: "case",
    }.get(component_type)


def parse_battery_packet(packet: bytes) -> BatteryStatus | None:
    if not packet.startswith(BATTERY_PREFIX):
        return None
    if len(packet) < 7:
        return None
    battery_count = packet[6]
    expected_size = 7 + battery_count * 5
    if battery_count > 4 or len(packet) != expected_size:
        return None

    result = BatteryStatus()
    pod_order: list[str] = []
    for index in range(battery_count):
        offset = 7 + index * 5
        component_type = packet[offset]
        spacer = packet[offset + 1]
        level = packet[offset + 2]
        status_code = packet[offset + 3]
        end_marker = packet[offset + 4]
        if spacer != 0x01 or end_marker != 0x01:
            return None

        name = _component_name(component_type)
        if name is None:
            continue
        component = getattr(result, name)
        component.status_code = status_code
        component.available = status_code != 0x04
        component.charging = status_code == 0x01
        component.level = None if not component.available else level
        if name in {"left", "right", "headset"}:
            pod_order.append(name)

    if pod_order:
        result.primary = pod_order[0]
    if len(pod_order) > 1:
        result.secondary = pod_order[1]
    return result


def parse_metadata_packet(packet: bytes) -> Metadata | None:
    if not packet.startswith(METADATA_PREFIX):
        return None
    if len(packet) <= len(METADATA_PREFIX) + 6:
        return None

    payload = packet[len(METADATA_PREFIX) + 6 :]
    parts = payload.split(b"\x00")
    if len(parts) < 3:
        return None
    return Metadata(
        device_name=_safe_decode(parts[0]),
        model_number=_safe_decode(parts[1]),
        manufacturer=_safe_decode(parts[2]),
    )


def parse_magic_keys_packet(packet: bytes) -> MagicKeys | None:
    if not packet.startswith(MAGIC_KEYS_PREFIX):
        return None
    if len(packet) < 7:
        return None
    key_count = packet[6]
    index = 7
    irk: bytes | None = None
    enc_key: bytes | None = None
    for _ in range(key_count):
        if index + 4 > len(packet):
            return None
        key_type = packet[index]
        key_length = packet[index + 2]
        index += 4
        if index + key_length > len(packet):
            return None
        key_data = packet[index : index + key_length]
        index += key_length
        if key_type == 0x01:
            irk = key_data
        elif key_type == 0x04:
            enc_key = key_data
    if irk is None or enc_key is None:
        return None
    return MagicKeys(irk=irk, enc_key=enc_key)


def parse_noise_control_mode(packet: bytes) -> int | None:
    if packet.startswith(NOISE_CONTROL_PREFIX) and len(packet) >= len(NOISE_CONTROL_PREFIX) + 1:
        value = packet[len(NOISE_CONTROL_PREFIX)]
        if 1 <= value <= 4:
            return value
    return None


def parse_conversational_awareness(packet: bytes) -> bool | None:
    if packet.startswith(CONVERSATIONAL_AWARENESS_PREFIX) and len(packet) >= len(CONVERSATIONAL_AWARENESS_PREFIX) + 1:
        value = packet[len(CONVERSATIONAL_AWARENESS_PREFIX)]
        if value == 0x01:
            return True
        if value == 0x02:
            return False
    return None


class AAPSession:
    def __init__(self, mac: str, timeout_seconds: float = 12.0) -> None:
        self.mac = mac
        self.timeout_seconds = timeout_seconds
        self.sock: socket.socket | None = None

    def __enter__(self) -> "AAPSession":
        self.open()
        return self

    def __exit__(self, exc_type, exc, tb) -> None:
        self.close()

    def open(self) -> None:
        if not hasattr(socket, "AF_BLUETOOTH"):
            raise AAPError("Bluetooth sockets are not available in this Python build")
        try:
            sock = socket.socket(socket.AF_BLUETOOTH, socket.SOCK_SEQPACKET, socket.BTPROTO_L2CAP)
            sock.setsockopt(
                socket.SOL_L2CAP,
                socket.L2CAP_LM,
                struct.pack("I", socket.L2CAP_LM_AUTH | socket.L2CAP_LM_ENCRYPT),
            )
            sock.settimeout(self.timeout_seconds)
            sock.connect((self.mac, 0x1001))
        except OSError as exc:
            raise AAPError(f"Failed to open AAP session to {self.mac}: {exc}") from exc
        self.sock = sock

    def close(self) -> None:
        if self.sock is not None:
            try:
                self.sock.close()
            finally:
                self.sock = None

    def query(self, *, request_keys: bool = False, capture_packets: bool = False) -> SessionState:
        if self.sock is None:
            raise AAPError("AAP session is not open")

        state = SessionState()
        self._send(AAPP_HANDSHAKE)
        self._drain_packets(state, duration=1.0, capture_packets=capture_packets)
        self._send(AAPP_SET_SPECIFIC_FEATURES)
        self._drain_packets(state, duration=1.5, capture_packets=capture_packets)
        self._send(AAPP_REQUEST_NOTIFICATIONS)
        time.sleep(0.1)
        self._send(AAPP_REQUEST_NOTIFICATIONS)
        if request_keys:
            time.sleep(0.1)
            self._send(AAPP_REQUEST_MAGIC_KEYS)
        self._collect_until_complete(
            state,
            wait_for_keys=request_keys,
            capture_packets=capture_packets,
        )
        return state

    def request_notifications(self) -> None:
        self._send(AAPP_REQUEST_NOTIFICATIONS)

    def request_magic_keys(self) -> None:
        self._send(AAPP_REQUEST_MAGIC_KEYS)

    def read_next(self, state: SessionState, *, timeout: float = 1.0, capture_packets: bool = False) -> bool:
        packet = self._recv(timeout=timeout)
        if packet is None:
            return False
        self._ingest_packet(packet, state, capture_packets=capture_packets)
        return True

    def _send(self, payload: bytes) -> None:
        if self.sock is None:
            raise AAPError("AAP session is not open")
        try:
            self.sock.send(payload)
        except OSError as exc:
            raise AAPError(f"Failed sending AAP payload: {exc}") from exc

    def _collect_until_complete(self, state: SessionState, *, wait_for_keys: bool, capture_packets: bool) -> None:
        deadline = time.monotonic() + self.timeout_seconds
        last_notification_request = time.monotonic()
        last_key_request = time.monotonic()
        while time.monotonic() < deadline:
            now = time.monotonic()
            if state.battery is None and now - last_notification_request >= 2.0:
                self._send(AAPP_REQUEST_NOTIFICATIONS)
                last_notification_request = now
            if wait_for_keys and state.magic_keys is None and now - last_key_request >= 3.0:
                self._send(AAPP_REQUEST_MAGIC_KEYS)
                last_key_request = now

            remaining = max(0.1, deadline - now)
            packet = self._recv(timeout=min(1.0, remaining))
            if packet is None:
                continue
            self._ingest_packet(packet, state, capture_packets=capture_packets)
            if state.battery is not None and state.metadata is not None and (state.magic_keys is not None or not wait_for_keys):
                break

    def _drain_packets(self, state: SessionState, *, duration: float, capture_packets: bool) -> None:
        deadline = time.monotonic() + duration
        while time.monotonic() < deadline:
            packet = self._recv(timeout=max(0.05, deadline - time.monotonic()))
            if packet is None:
                break
            self._ingest_packet(packet, state, capture_packets=capture_packets)

    def _recv(self, *, timeout: float) -> bytes | None:
        if self.sock is None:
            raise AAPError("AAP session is not open")
        self.sock.settimeout(timeout)
        try:
            return self.sock.recv(4096)
        except TimeoutError:
            return None
        except OSError as exc:
            raise AAPError(f"Failed reading AAP packet: {exc}") from exc

    def _ingest_packet(self, packet: bytes, state: SessionState, *, capture_packets: bool) -> None:
        if capture_packets:
            state.raw_packets.append(packet.hex())
        battery = parse_battery_packet(packet)
        if battery is not None:
            state.battery = battery
        metadata = parse_metadata_packet(packet)
        if metadata is not None:
            state.metadata = metadata
        keys = parse_magic_keys_packet(packet)
        if keys is not None:
            state.magic_keys = keys
        noise_mode = parse_noise_control_mode(packet)
        if noise_mode is not None:
            state.noise_control_mode = noise_mode
        ca_enabled = parse_conversational_awareness(packet)
        if ca_enabled is not None:
            state.conversational_awareness_enabled = ca_enabled


def _safe_decode(raw: bytes) -> str:
    try:
        return raw.decode("utf-8")
    except UnicodeDecodeError:
        return raw.decode("utf-8", errors="replace")
