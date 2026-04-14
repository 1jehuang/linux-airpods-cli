use std::io;
use std::mem::{size_of, zeroed};
use std::os::fd::RawFd;
use std::time::{Duration, Instant};

use anyhow::{Context, Result, anyhow, bail};
use serde::Serialize;

const AAPP_HANDSHAKE: &[u8] = &hex_literal::hex!("00000400010002000000000000000000");
const AAPP_SET_SPECIFIC_FEATURES: &[u8] = &hex_literal::hex!("040004004d00d700000000000000");
const AAPP_REQUEST_NOTIFICATIONS: &[u8] = &hex_literal::hex!("040004000f00ffffffffff");
const AAPP_REQUEST_MAGIC_KEYS: &[u8] = &hex_literal::hex!("0400040030000500");

const HANDSHAKE_ACK_PREFIX: &[u8] = &hex_literal::hex!("01000400");
const FEATURES_ACK_PREFIX: &[u8] = &hex_literal::hex!("040004002b00");
const BATTERY_PREFIX: &[u8] = &hex_literal::hex!("040004000400");
const METADATA_PREFIX: &[u8] = &hex_literal::hex!("040004001d");
const MAGIC_KEYS_PREFIX: &[u8] = &hex_literal::hex!("04000400310002");
const NOISE_CONTROL_PREFIX: &[u8] = &hex_literal::hex!("0400040009000d");
const CONVERSATIONAL_AWARENESS_PREFIX: &[u8] = &hex_literal::hex!("04000400090028");

const SOL_L2CAP: libc::c_int = 6;
const L2CAP_LM: libc::c_int = 0x03;
const L2CAP_LM_AUTH: libc::c_int = 0x0002;
const L2CAP_LM_ENCRYPT: libc::c_int = 0x0004;
const AAP_PSM: u16 = 0x1001;
const BTPROTO_L2CAP: libc::c_int = 0;

#[repr(C)]
#[derive(Clone, Copy)]
struct BdAddr {
    b: [u8; 6],
}

#[repr(C)]
#[derive(Clone, Copy)]
struct SockAddrL2 {
    l2_family: libc::sa_family_t,
    l2_psm: u16,
    l2_bdaddr: BdAddr,
    l2_cid: u16,
    l2_bdaddr_type: u8,
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize)]
pub struct BatteryComponent {
    pub level: Option<u8>,
    pub charging: bool,
    pub available: bool,
    pub status_code: Option<u8>,
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize)]
pub struct BatteryStatus {
    pub left: BatteryComponent,
    pub right: BatteryComponent,
    #[serde(rename = "case")]
    pub case_unit: BatteryComponent,
    pub headset: BatteryComponent,
    pub primary: Option<String>,
    pub secondary: Option<String>,
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize)]
pub struct Metadata {
    pub device_name: Option<String>,
    pub model_number: Option<String>,
    pub manufacturer: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct MagicKeys {
    pub irk: String,
    pub enc_key: String,
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize)]
pub struct SessionState {
    pub battery: Option<BatteryStatus>,
    pub metadata: Option<Metadata>,
    pub magic_keys: Option<MagicKeys>,
    pub noise_control_mode: Option<u8>,
    pub conversational_awareness_enabled: Option<bool>,
    pub raw_packets: Vec<String>,
}

pub struct AAPSession {
    fd: RawFd,
    timeout: Duration,
}

impl Drop for AAPSession {
    fn drop(&mut self) {
        let _ = unsafe { libc::close(self.fd) };
    }
}

impl AAPSession {
    pub fn open(mac: &str, timeout_seconds: f64) -> Result<Self> {
        let timeout = Duration::from_secs_f64(timeout_seconds.max(0.1));
        let fd = unsafe { libc::socket(libc::AF_BLUETOOTH, libc::SOCK_SEQPACKET, BTPROTO_L2CAP) };
        if fd < 0 {
            return Err(last_os_error()).context("Failed to open AAP socket");
        }

        if let Err(error) = set_l2cap_security(fd) {
            unsafe {
                libc::close(fd);
            }
            return Err(error);
        }

        if let Err(error) = set_socket_timeout(fd, timeout) {
            unsafe {
                libc::close(fd);
            }
            return Err(error);
        }

        let address = SockAddrL2 {
            l2_family: libc::AF_BLUETOOTH as libc::sa_family_t,
            l2_psm: htobs(AAP_PSM),
            l2_bdaddr: parse_bdaddr(mac)?,
            l2_cid: 0,
            l2_bdaddr_type: 0,
        };

        let connect_result = unsafe {
            libc::connect(
                fd,
                &address as *const SockAddrL2 as *const libc::sockaddr,
                size_of::<SockAddrL2>() as libc::socklen_t,
            )
        };
        if connect_result < 0 {
            let error = last_os_error();
            unsafe {
                libc::close(fd);
            }
            bail!("Failed to open AAP session to {mac}: {error}");
        }

        Ok(Self { fd, timeout })
    }

    pub fn query(&mut self, request_keys: bool, capture_packets: bool) -> Result<SessionState> {
        let mut state = SessionState::default();
        self.send(AAPP_HANDSHAKE)?;
        self.drain_packets(&mut state, Duration::from_secs_f64(1.0), capture_packets)?;
        self.send(AAPP_SET_SPECIFIC_FEATURES)?;
        self.drain_packets(&mut state, Duration::from_secs_f64(1.5), capture_packets)?;
        self.send(AAPP_REQUEST_NOTIFICATIONS)?;
        std::thread::sleep(Duration::from_millis(100));
        self.send(AAPP_REQUEST_NOTIFICATIONS)?;
        if request_keys {
            std::thread::sleep(Duration::from_millis(100));
            self.send(AAPP_REQUEST_MAGIC_KEYS)?;
        }
        self.collect_until_complete(&mut state, request_keys, capture_packets)?;
        Ok(state)
    }

    pub fn request_notifications(&mut self) -> Result<()> {
        self.send(AAPP_REQUEST_NOTIFICATIONS)
    }

    pub fn request_magic_keys(&mut self) -> Result<()> {
        self.send(AAPP_REQUEST_MAGIC_KEYS)
    }

    pub fn read_next(
        &mut self,
        state: &mut SessionState,
        timeout_seconds: f64,
        capture_packets: bool,
    ) -> Result<bool> {
        let Some(packet) = self.recv(Duration::from_secs_f64(timeout_seconds.max(0.05)))? else {
            return Ok(false);
        };
        ingest_packet(&packet, state, capture_packets);
        Ok(true)
    }

    fn send(&mut self, payload: &[u8]) -> Result<()> {
        let sent = unsafe {
            libc::send(
                self.fd,
                payload.as_ptr() as *const libc::c_void,
                payload.len(),
                0,
            )
        };
        if sent < 0 {
            return Err(last_os_error()).context("Failed sending AAP payload");
        }
        Ok(())
    }

    fn collect_until_complete(
        &mut self,
        state: &mut SessionState,
        wait_for_keys: bool,
        capture_packets: bool,
    ) -> Result<()> {
        let deadline = Instant::now() + self.timeout;
        let mut last_notification_request = Instant::now();
        let mut last_key_request = Instant::now();

        while Instant::now() < deadline {
            let now = Instant::now();
            if state.battery.is_none()
                && now.duration_since(last_notification_request) >= Duration::from_secs(2)
            {
                self.send(AAPP_REQUEST_NOTIFICATIONS)?;
                last_notification_request = now;
            }
            if wait_for_keys
                && state.magic_keys.is_none()
                && now.duration_since(last_key_request) >= Duration::from_secs(3)
            {
                self.send(AAPP_REQUEST_MAGIC_KEYS)?;
                last_key_request = now;
            }

            let remaining = deadline.saturating_duration_since(now);
            let Some(packet) = self.recv(
                remaining
                    .min(Duration::from_secs(1))
                    .max(Duration::from_millis(100)),
            )?
            else {
                continue;
            };
            ingest_packet(&packet, state, capture_packets);
            if state.battery.is_some()
                && state.metadata.is_some()
                && (state.magic_keys.is_some() || !wait_for_keys)
            {
                break;
            }
        }

        Ok(())
    }

    fn drain_packets(
        &mut self,
        state: &mut SessionState,
        duration: Duration,
        capture_packets: bool,
    ) -> Result<()> {
        let deadline = Instant::now() + duration;
        while Instant::now() < deadline {
            let timeout = deadline
                .saturating_duration_since(Instant::now())
                .max(Duration::from_millis(50));
            let Some(packet) = self.recv(timeout)? else {
                break;
            };
            ingest_packet(&packet, state, capture_packets);
        }
        Ok(())
    }

    fn recv(&mut self, timeout: Duration) -> Result<Option<Vec<u8>>> {
        let mut poll_fd = libc::pollfd {
            fd: self.fd,
            events: libc::POLLIN,
            revents: 0,
        };
        let timeout_ms = timeout.as_millis().min(i32::MAX as u128) as libc::c_int;
        let poll_result = unsafe { libc::poll(&mut poll_fd, 1, timeout_ms) };
        if poll_result < 0 {
            return Err(last_os_error()).context("Failed polling AAP socket");
        }
        if poll_result == 0 {
            return Ok(None);
        }

        let mut buffer = vec![0u8; 4096];
        let received = unsafe {
            libc::recv(
                self.fd,
                buffer.as_mut_ptr() as *mut libc::c_void,
                buffer.len(),
                0,
            )
        };
        if received < 0 {
            let error = io::Error::last_os_error();
            if matches!(
                error.kind(),
                io::ErrorKind::WouldBlock | io::ErrorKind::TimedOut
            ) {
                return Ok(None);
            }
            return Err(anyhow!(error)).context("Failed reading AAP packet");
        }
        buffer.truncate(received as usize);
        Ok(Some(buffer))
    }
}

pub fn parse_battery_packet(packet: &[u8]) -> Option<BatteryStatus> {
    if !packet.starts_with(BATTERY_PREFIX) || packet.len() < 7 {
        return None;
    }
    let battery_count = packet[6] as usize;
    let expected_size = 7 + battery_count * 5;
    if battery_count > 4 || packet.len() != expected_size {
        return None;
    }

    let mut result = BatteryStatus::default();
    let mut pod_order = Vec::new();

    for index in 0..battery_count {
        let offset = 7 + index * 5;
        let component_type = packet[offset];
        let spacer = packet[offset + 1];
        let level = packet[offset + 2];
        let status_code = packet[offset + 3];
        let end_marker = packet[offset + 4];
        if spacer != 0x01 || end_marker != 0x01 {
            return None;
        }

        let component = match component_name(component_type) {
            Some("headset") => &mut result.headset,
            Some("right") => &mut result.right,
            Some("left") => &mut result.left,
            Some("case") => &mut result.case_unit,
            _ => continue,
        };

        component.status_code = Some(status_code);
        component.available = status_code != 0x04;
        component.charging = status_code == 0x01;
        component.level = if component.available {
            Some(level)
        } else {
            None
        };

        if matches!(
            component_name(component_type),
            Some("left" | "right" | "headset")
        ) {
            pod_order.push(component_name(component_type).unwrap().to_owned());
        }
    }

    result.primary = pod_order.first().cloned();
    result.secondary = pod_order.get(1).cloned();
    Some(result)
}

pub fn parse_metadata_packet(packet: &[u8]) -> Option<Metadata> {
    if !packet.starts_with(METADATA_PREFIX) || packet.len() <= METADATA_PREFIX.len() + 6 {
        return None;
    }

    let payload = &packet[METADATA_PREFIX.len() + 6..];
    let parts: Vec<&[u8]> = payload.split(|byte| *byte == 0).collect();
    if parts.len() < 3 {
        return None;
    }

    Some(Metadata {
        device_name: Some(safe_decode(parts[0])),
        model_number: Some(safe_decode(parts[1])),
        manufacturer: Some(safe_decode(parts[2])),
    })
}

pub fn parse_magic_keys_packet(packet: &[u8]) -> Option<MagicKeys> {
    if !packet.starts_with(MAGIC_KEYS_PREFIX) || packet.len() < 7 {
        return None;
    }
    let key_count = packet[6] as usize;
    let mut index = 7usize;
    let mut irk = None;
    let mut enc_key = None;

    for _ in 0..key_count {
        if index + 4 > packet.len() {
            return None;
        }
        let key_type = packet[index];
        let key_length = packet[index + 2] as usize;
        index += 4;
        if index + key_length > packet.len() {
            return None;
        }
        let key_data = &packet[index..index + key_length];
        index += key_length;
        match key_type {
            0x01 => irk = Some(hex::encode(key_data)),
            0x04 => enc_key = Some(hex::encode(key_data)),
            _ => {}
        }
    }

    Some(MagicKeys {
        irk: irk?,
        enc_key: enc_key?,
    })
}

pub fn parse_noise_control_mode(packet: &[u8]) -> Option<u8> {
    if packet.starts_with(NOISE_CONTROL_PREFIX) && packet.len() >= NOISE_CONTROL_PREFIX.len() + 1 {
        let value = packet[NOISE_CONTROL_PREFIX.len()];
        if (1..=4).contains(&value) {
            return Some(value);
        }
    }
    None
}

pub fn parse_conversational_awareness(packet: &[u8]) -> Option<bool> {
    if packet.starts_with(CONVERSATIONAL_AWARENESS_PREFIX)
        && packet.len() >= CONVERSATIONAL_AWARENESS_PREFIX.len() + 1
    {
        return match packet[CONVERSATIONAL_AWARENESS_PREFIX.len()] {
            0x01 => Some(true),
            0x02 => Some(false),
            _ => None,
        };
    }
    None
}

fn ingest_packet(packet: &[u8], state: &mut SessionState, capture_packets: bool) {
    if capture_packets {
        state.raw_packets.push(hex::encode(packet));
    }
    if packet.starts_with(HANDSHAKE_ACK_PREFIX) || packet.starts_with(FEATURES_ACK_PREFIX) {
        return;
    }
    if let Some(battery) = parse_battery_packet(packet) {
        state.battery = Some(battery);
    }
    if let Some(metadata) = parse_metadata_packet(packet) {
        state.metadata = Some(metadata);
    }
    if let Some(keys) = parse_magic_keys_packet(packet) {
        state.magic_keys = Some(keys);
    }
    if let Some(mode) = parse_noise_control_mode(packet) {
        state.noise_control_mode = Some(mode);
    }
    if let Some(enabled) = parse_conversational_awareness(packet) {
        state.conversational_awareness_enabled = Some(enabled);
    }
}

fn component_name(component_type: u8) -> Option<&'static str> {
    match component_type {
        0x01 => Some("headset"),
        0x02 => Some("right"),
        0x04 => Some("left"),
        0x08 => Some("case"),
        _ => None,
    }
}

fn safe_decode(raw: &[u8]) -> String {
    String::from_utf8_lossy(raw).to_string()
}

fn parse_bdaddr(mac: &str) -> Result<BdAddr> {
    let parts: Vec<&str> = mac.split(':').collect();
    if parts.len() != 6 {
        bail!("Invalid Bluetooth MAC address: {mac}");
    }

    let mut bytes = [0u8; 6];
    for (index, part) in parts.iter().rev().enumerate() {
        bytes[index] = u8::from_str_radix(part, 16)
            .with_context(|| format!("Invalid Bluetooth MAC address: {mac}"))?;
    }
    Ok(BdAddr { b: bytes })
}

fn htobs(value: u16) -> u16 {
    value.to_le()
}

fn set_l2cap_security(fd: RawFd) -> Result<()> {
    let flags: libc::c_int = L2CAP_LM_AUTH | L2CAP_LM_ENCRYPT;
    let result = unsafe {
        libc::setsockopt(
            fd,
            SOL_L2CAP,
            L2CAP_LM,
            &flags as *const libc::c_int as *const libc::c_void,
            size_of::<libc::c_int>() as libc::socklen_t,
        )
    };
    if result < 0 {
        return Err(last_os_error()).context("Failed configuring AAP socket security");
    }
    Ok(())
}

fn set_socket_timeout(fd: RawFd, timeout: Duration) -> Result<()> {
    let mut tv: libc::timeval = unsafe { zeroed() };
    tv.tv_sec = timeout.as_secs() as libc::time_t;
    tv.tv_usec = timeout.subsec_micros() as libc::suseconds_t;

    for option in [libc::SO_RCVTIMEO, libc::SO_SNDTIMEO] {
        let result = unsafe {
            libc::setsockopt(
                fd,
                libc::SOL_SOCKET,
                option,
                &tv as *const libc::timeval as *const libc::c_void,
                size_of::<libc::timeval>() as libc::socklen_t,
            )
        };
        if result < 0 {
            return Err(last_os_error()).context("Failed configuring AAP socket timeout");
        }
    }

    Ok(())
}

fn last_os_error() -> anyhow::Error {
    anyhow!(io::Error::last_os_error())
}
