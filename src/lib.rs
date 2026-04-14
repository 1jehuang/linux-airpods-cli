pub mod aap;

use std::collections::HashMap;
use std::env;
use std::ffi::OsString;
use std::fs;
use std::io::{Read, Write};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::{Mutex, OnceLock};
use std::thread;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use anyhow::{Context, Result, anyhow, bail};
use clap::{ArgAction, Args, Parser, Subcommand};
use regex::Regex;
use serde::Serialize;
use serde_json::{Value, json};
use tempfile::Builder;
use wait_timeout::ChildExt;
use which::which;

use crate::aap::{AAPSession, BatteryStatus, MagicKeys, Metadata, SessionState};

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct Device {
    pub mac: String,
    pub name: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Sink {
    pub sink_id: i32,
    pub name: String,
    pub state: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Card {
    pub card_id: i32,
    pub name: String,
}

#[derive(Debug, Clone)]
struct CommandResult {
    code: i32,
    stdout: String,
    stderr: String,
}

#[derive(Debug, Clone, Serialize)]
struct StatusPayload {
    name: String,
    alias: String,
    mac: String,
    paired: bool,
    trusted: bool,
    connected: bool,
    breder_connected: bool,
    sink: Option<String>,
    sink_state: Option<String>,
    card: Option<String>,
    default_sink: Option<String>,
    default_is_airpods: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    battery: Option<BatteryStatus>,
    #[serde(skip_serializing_if = "Option::is_none")]
    metadata: Option<Metadata>,
    #[serde(skip_serializing_if = "Option::is_none")]
    magic_keys: Option<MagicKeys>,
    #[serde(skip_serializing_if = "Option::is_none")]
    noise_control_mode: Option<u8>,
    #[serde(skip_serializing_if = "Option::is_none")]
    conversational_awareness_enabled: Option<bool>,
    #[serde(skip_serializing_if = "Option::is_none")]
    raw_packets: Option<Vec<String>>,
}

impl StatusPayload {
    fn apply_session_state(&mut self, state: SessionState) {
        self.battery = state.battery;
        self.metadata = state.metadata;
        self.magic_keys = state.magic_keys;
        self.noise_control_mode = state.noise_control_mode;
        self.conversational_awareness_enabled = state.conversational_awareness_enabled;
        self.raw_packets = Some(state.raw_packets);
    }
}

#[derive(Debug, Parser)]
#[command(
    name = "airpods",
    version,
    about = "A small AirPods CLI for Linux using BlueZ and PipeWire."
)]
struct Cli {
    #[arg(long, global = true, help = "Target Bluetooth MAC address")]
    mac: Option<String>,
    #[arg(long, global = true, help = "Target device name substring")]
    name: Option<String>,
    #[command(subcommand)]
    command: Commands,
}

#[derive(Debug, Subcommand)]
enum Commands {
    Status(StatusArgs),
    Devices(DevicesArgs),
    Sink,
    Connect(ConnectArgs),
    Disconnect(DisconnectArgs),
    #[command(name = "set-default")]
    SetDefault(SetDefaultArgs),
    Fix(FixArgs),
    Battery(BatteryArgs),
    Keys(KeysArgs),
    Monitor(MonitorArgs),
}

#[derive(Debug, Args)]
struct StatusArgs {
    #[arg(long, action = ArgAction::SetTrue, help = "Emit machine-readable JSON")]
    json: bool,
    #[arg(long, action = ArgAction::SetTrue, help = "Also query AAP battery / metadata over the native AirPods control channel")]
    aap: bool,
    #[arg(long, action = ArgAction::SetTrue, help = "Include raw AAP packets when using --aap")]
    raw_packets: bool,
    #[arg(long, default_value_t = 12.0, help = "Seconds to wait for AAP packets")]
    wait: f64,
}

#[derive(Debug, Args)]
struct DevicesArgs {
    #[arg(long, action = ArgAction::SetTrue, help = "Emit machine-readable JSON")]
    json: bool,
}

#[derive(Debug, Args)]
struct ConnectArgs {
    #[arg(long, action = ArgAction::SetTrue, help = "Try pairing if connect fails")]
    pair: bool,
    #[arg(long, default_value_t = 12.0, help = "Seconds to wait for the sink")]
    wait: f64,
    #[arg(long = "no-default", action = ArgAction::SetFalse, default_value_t = true, help = "Do not set the AirPods sink as default")]
    set_default: bool,
    #[arg(long = "no-move", action = ArgAction::SetFalse, default_value_t = true, help = "Do not move active streams to the new default sink")]
    move_streams: bool,
    #[arg(long, action = ArgAction::SetTrue, help = "Emit machine-readable JSON")]
    json: bool,
}

#[derive(Debug, Args)]
struct DisconnectArgs {
    #[arg(long = "no-fallback", action = ArgAction::SetFalse, default_value_t = true, help = "Do not switch to a non-Bluetooth fallback sink")]
    fallback: bool,
    #[arg(long = "no-move", action = ArgAction::SetFalse, default_value_t = true, help = "Do not move active streams when changing fallback sink")]
    move_streams: bool,
    #[arg(long, action = ArgAction::SetTrue, help = "Emit machine-readable JSON")]
    json: bool,
}

#[derive(Debug, Args)]
struct SetDefaultArgs {
    #[arg(long = "no-move", action = ArgAction::SetFalse, default_value_t = true, help = "Do not move active streams to the AirPods sink")]
    move_streams: bool,
}

#[derive(Debug, Args)]
struct FixArgs {
    #[arg(long = "no-restart-audio", action = ArgAction::SetFalse, default_value_t = true, help = "Skip restarting pipewire / wireplumber")]
    restart_audio: bool,
    #[arg(
        long,
        default_value_t = 3.0,
        help = "Seconds to wait after audio restart"
    )]
    restart_wait: f64,
    #[arg(long, default_value_t = 12.0, help = "Seconds to wait for the sink")]
    wait: f64,
    #[arg(long = "no-move", action = ArgAction::SetFalse, default_value_t = true, help = "Do not move active streams to the recovered sink")]
    move_streams: bool,
    #[arg(long, action = ArgAction::SetTrue, help = "Emit machine-readable JSON")]
    json: bool,
}

#[derive(Debug, Args)]
struct BatteryArgs {
    #[arg(long, default_value_t = 12.0, help = "Seconds to wait for AAP packets")]
    wait: f64,
    #[arg(long, action = ArgAction::SetTrue, help = "Emit machine-readable JSON")]
    json: bool,
    #[arg(long, action = ArgAction::SetTrue, help = "Include raw AAP packets in the JSON output")]
    raw_packets: bool,
    #[arg(long = "no-wake", action = ArgAction::SetFalse, default_value_t = true, help = "Skip the silent sink wake-up before opening AAP")]
    wake: bool,
}

#[derive(Debug, Args)]
struct KeysArgs {
    #[arg(long, default_value_t = 12.0, help = "Seconds to wait for AAP packets")]
    wait: f64,
    #[arg(long, action = ArgAction::SetTrue, help = "Emit machine-readable JSON")]
    json: bool,
    #[arg(long, action = ArgAction::SetTrue, help = "Include raw AAP packets in the JSON output")]
    raw_packets: bool,
    #[arg(long = "no-wake", action = ArgAction::SetFalse, default_value_t = true, help = "Skip the silent sink wake-up before opening AAP")]
    wake: bool,
}

#[derive(Debug, Args)]
struct MonitorArgs {
    #[arg(long, help = "Path to the JSON cache file")]
    cache_file: Option<PathBuf>,
    #[arg(
        long,
        default_value_t = 12.0,
        help = "Seconds to wait for the initial AAP setup"
    )]
    wait: f64,
    #[arg(
        long,
        default_value_t = 1.0,
        help = "Seconds to wait for the next packet"
    )]
    poll_interval: f64,
    #[arg(
        long,
        default_value_t = 20.0,
        help = "How often to re-request notifications while idle"
    )]
    refresh_interval: f64,
    #[arg(
        long,
        default_value_t = 3.0,
        help = "Retry delay after disconnect or monitor error"
    )]
    retry_interval: f64,
    #[arg(long, action = ArgAction::SetTrue, help = "Also request Magic Cloud Keys during monitor startup")]
    request_keys: bool,
    #[arg(long, action = ArgAction::SetTrue, help = "Store raw AAP packets in the cache file")]
    raw_packets: bool,
    #[arg(long = "no-wake", action = ArgAction::SetFalse, default_value_t = true, help = "Skip the silent sink wake-up before opening AAP")]
    wake: bool,
    #[arg(long, action = ArgAction::SetTrue, help = "Prime the cache once and exit")]
    once: bool,
}

static ANSI_ESCAPE_RE: OnceLock<Regex> = OnceLock::new();
static TREE_PREFIX_RE: OnceLock<Regex> = OnceLock::new();
static BLUEZ_PATH_CACHE: OnceLock<Mutex<HashMap<String, Option<String>>>> = OnceLock::new();

fn ansi_escape_re() -> &'static Regex {
    ANSI_ESCAPE_RE.get_or_init(|| Regex::new(r"\x1b\[[0-9;]*m").expect("valid ansi regex"))
}

fn tree_prefix_re() -> &'static Regex {
    TREE_PREFIX_RE.get_or_init(|| Regex::new(r"^[\s├└─]+").expect("valid tree prefix regex"))
}

fn bluez_path_cache() -> &'static Mutex<HashMap<String, Option<String>>> {
    BLUEZ_PATH_CACHE.get_or_init(|| Mutex::new(HashMap::new()))
}

pub fn run() -> Result<()> {
    require_command("bluetoothctl")?;
    require_command("pactl")?;

    let cli = Cli::parse();
    let mac = cli.mac.clone();
    let name = cli.name.clone();
    match cli.command {
        Commands::Status(args) => cmd_status(mac.as_deref(), name.as_deref(), &args),
        Commands::Devices(args) => cmd_devices(&args),
        Commands::Sink => cmd_sink(mac.as_deref(), name.as_deref()),
        Commands::Connect(args) => cmd_connect(mac.as_deref(), name.as_deref(), &args),
        Commands::Disconnect(args) => cmd_disconnect(mac.as_deref(), name.as_deref(), &args),
        Commands::SetDefault(args) => cmd_set_default(mac.as_deref(), name.as_deref(), &args),
        Commands::Fix(args) => cmd_fix(mac.as_deref(), name.as_deref(), &args),
        Commands::Battery(args) => cmd_battery(mac.as_deref(), name.as_deref(), &args),
        Commands::Keys(args) => cmd_keys(mac.as_deref(), name.as_deref(), &args),
        Commands::Monitor(args) => cmd_monitor(mac.as_deref(), name.as_deref(), &args),
    }
}

fn require_command(name: &str) -> Result<()> {
    which(name).with_context(|| format!("Required command not found in PATH: {name}"))?;
    Ok(())
}

fn run_command<I, S>(args: I, timeout: Duration, check: bool) -> Result<CommandResult>
where
    I: IntoIterator<Item = S>,
    S: Into<OsString>,
{
    run_command_with_env(args, timeout, check, &[])
}

fn run_command_with_env<I, S>(
    args: I,
    timeout: Duration,
    check: bool,
    env_overrides: &[(String, String)],
) -> Result<CommandResult>
where
    I: IntoIterator<Item = S>,
    S: Into<OsString>,
{
    let args: Vec<OsString> = args.into_iter().map(Into::into).collect();
    let Some(program) = args.first() else {
        bail!("No command provided")
    };

    let display = args
        .iter()
        .map(|part| part.to_string_lossy())
        .collect::<Vec<_>>()
        .join(" ");

    let mut command = Command::new(program);
    command.args(&args[1..]);
    command.stdout(Stdio::piped());
    command.stderr(Stdio::piped());
    for (key, value) in env_overrides {
        command.env(key, value);
    }

    let mut child = command
        .spawn()
        .with_context(|| format!("Failed to execute command: {display}"))?;

    let status = child
        .wait_timeout(timeout)
        .with_context(|| format!("Failed waiting for command: {display}"))?;

    let Some(status) = status else {
        let _ = child.kill();
        let _ = child.wait();
        bail!("Command timed out: {display}");
    };

    let mut stdout = String::new();
    if let Some(mut handle) = child.stdout.take() {
        handle
            .read_to_string(&mut stdout)
            .with_context(|| format!("Failed reading stdout: {display}"))?;
    }

    let mut stderr = String::new();
    if let Some(mut handle) = child.stderr.take() {
        handle
            .read_to_string(&mut stderr)
            .with_context(|| format!("Failed reading stderr: {display}"))?;
    }

    let result = CommandResult {
        code: status.code().unwrap_or(1),
        stdout,
        stderr,
    };

    if check && result.code != 0 {
        let details = result
            .stderr
            .trim()
            .strip_prefix("error: ")
            .unwrap_or(result.stderr.trim());
        let details = if details.is_empty() {
            let stdout = result.stdout.trim();
            if stdout.is_empty() {
                format!("exit code {}", result.code)
            } else {
                stdout.to_owned()
            }
        } else {
            details.to_owned()
        };
        bail!("Command failed: {display}\n{details}");
    }

    Ok(result)
}

pub fn parse_bluetooth_devices(output: &str) -> Vec<Device> {
    output
        .lines()
        .filter_map(|line| {
            let line = ansi_escape_re().replace_all(line, "");
            let line = line.trim();
            if !line.starts_with("Device ") {
                return None;
            }
            let mut parts = line
                .splitn(3, char::is_whitespace)
                .filter(|part| !part.is_empty());
            let tag = parts.next()?;
            if tag != "Device" {
                return None;
            }
            let mac = parts.next()?.trim().to_owned();
            let name = parts.next()?.trim().to_owned();
            Some(Device { mac, name })
        })
        .collect()
}

pub fn parse_bluetooth_info(output: &str) -> HashMap<String, String> {
    let mut info = HashMap::new();
    for raw_line in output.lines() {
        let line = ansi_escape_re().replace_all(raw_line, "");
        let line = line.trim_end();
        if line.is_empty() || line.starts_with("Device ") {
            continue;
        }
        let Some((key, value)) = line.split_once(':') else {
            continue;
        };
        info.insert(key.trim().to_owned(), value.trim().to_owned());
    }
    info
}

fn strip_tree_prefix(raw_line: &str) -> String {
    tree_prefix_re().replace(raw_line, "").trim().to_owned()
}

fn bluez_device_path(mac: &str) -> Option<String> {
    if which("busctl").is_err() {
        return None;
    }

    let mac = normalize_mac_colon(mac);
    if let Some(cached) = bluez_path_cache().lock().ok()?.get(&mac).cloned() {
        return cached;
    }

    let result = run_command(
        ["busctl", "--system", "tree", "org.bluez"],
        Duration::from_secs(10),
        false,
    )
    .ok()?;

    if result.code != 0 {
        return None;
    }

    let suffix = format!("/dev_{}", normalize_mac_underscore(&mac));
    let path = result
        .stdout
        .lines()
        .map(strip_tree_prefix)
        .find(|path| path.ends_with(&suffix));

    if let Ok(mut cache) = bluez_path_cache().lock() {
        cache.insert(mac, path.clone());
    }

    path
}

fn bluez_device_property(mac: &str, prop: &str) -> Option<Value> {
    let path = bluez_device_path(mac)?;
    let result = run_command(
        [
            "busctl",
            "--json=short",
            "--system",
            "get-property",
            "org.bluez",
            path.as_str(),
            "org.bluez.Device1",
            prop,
        ],
        Duration::from_secs(10),
        false,
    )
    .ok()?;

    if result.code != 0 {
        return None;
    }

    let payload: Value = serde_json::from_str(&result.stdout).ok()?;
    payload.get("data").cloned()
}

fn stringify_bluez_value(value: &Value) -> String {
    match value {
        Value::Bool(boolean) => yes_no(*boolean).to_owned(),
        Value::Array(items) => items
            .iter()
            .map(stringify_bluez_value)
            .collect::<Vec<_>>()
            .join(" "),
        Value::String(string) => string.clone(),
        other => other.to_string(),
    }
}

fn bluetooth_info_via_busctl(mac: &str) -> HashMap<String, String> {
    let property_map = [
        ("Name", "Name"),
        ("Alias", "Alias"),
        ("Address", "Address"),
        ("AddressType", "AddressType"),
        ("Paired", "Paired"),
        ("Bonded", "Bonded"),
        ("Trusted", "Trusted"),
        ("Blocked", "Blocked"),
        ("Connected", "Connected"),
        ("LegacyPairing", "LegacyPairing"),
        ("CablePairing", "CablePairing"),
        ("Modalias", "Modalias"),
        ("PreferredBearer", "PreferredBearer"),
        ("ServicesResolved", "ServicesResolved"),
    ];

    let mut info = HashMap::new();
    for (prop, key) in property_map {
        if let Some(value) = bluez_device_property(mac, prop) {
            info.insert(key.to_owned(), stringify_bluez_value(&value));
        }
    }

    if let Some(value) = info.get("Bonded").cloned() {
        info.entry("BREDR.Bonded".to_owned()).or_insert(value);
    }
    if let Some(value) = info.get("Connected").cloned() {
        info.entry("BREDR.Connected".to_owned()).or_insert(value);
    }

    info
}

pub fn parse_short_sinks(output: &str) -> Vec<Sink> {
    output
        .lines()
        .filter_map(|line| {
            let parts: Vec<&str> = line.split_whitespace().collect();
            if parts.len() < 5 {
                return None;
            }
            Some(Sink {
                sink_id: parts[0].parse().ok()?,
                name: parts[1].to_owned(),
                state: parts.last()?.to_string(),
            })
        })
        .collect()
}

pub fn parse_short_cards(output: &str) -> Vec<Card> {
    output
        .lines()
        .filter_map(|line| {
            let parts: Vec<&str> = line.split_whitespace().collect();
            if parts.len() < 2 {
                return None;
            }
            Some(Card {
                card_id: parts[0].parse().ok()?,
                name: parts[1].to_owned(),
            })
        })
        .collect()
}

pub fn normalize_mac_colon(mac: &str) -> String {
    mac.to_ascii_uppercase().replace('_', ":")
}

pub fn normalize_mac_underscore(mac: &str) -> String {
    normalize_mac_colon(mac).replace(':', "_")
}

pub fn airpods_like_name(name: &str) -> bool {
    let lowered = name.to_ascii_lowercase();
    lowered.contains("airpods") || lowered.contains("beats")
}

fn bluetooth_devices() -> Result<Vec<Device>> {
    let result = run_command(["bluetoothctl", "devices"], Duration::from_secs(10), true)?;
    Ok(parse_bluetooth_devices(&result.stdout))
}

fn bluetooth_info(mac: &str) -> HashMap<String, String> {
    let info = bluetooth_info_via_busctl(mac);
    if !info.is_empty() {
        return info;
    }

    let result = match run_command(
        ["bluetoothctl", "info", mac],
        Duration::from_secs(10),
        false,
    ) {
        Ok(result) => result,
        Err(_) => return HashMap::new(),
    };

    if result.code != 0 {
        return HashMap::new();
    }

    parse_bluetooth_info(&result.stdout)
}

fn connected(mac: &str) -> bool {
    bluetooth_info(mac)
        .get("Connected")
        .is_some_and(|value| value == "yes")
}

fn discover_device(requested_mac: Option<&str>, requested_name: Option<&str>) -> Result<Device> {
    if let Some(requested_mac) = requested_mac {
        let mac = normalize_mac_colon(requested_mac);
        let info = bluetooth_info(&mac);
        let name = info
            .get("Name")
            .or_else(|| info.get("Alias"))
            .cloned()
            .unwrap_or_else(|| mac.clone());
        return Ok(Device { mac, name });
    }

    let devices = bluetooth_devices()?;
    if let Some(requested_name) = requested_name {
        let lowered = requested_name.to_ascii_lowercase();
        let matches: Vec<Device> = devices
            .into_iter()
            .filter(|device| device.name.to_ascii_lowercase().contains(&lowered))
            .collect();
        if matches.is_empty() {
            bail!("No Bluetooth device matched name: {requested_name}");
        }
        if matches.len() == 1 {
            return Ok(matches.into_iter().next().expect("one match exists"));
        }
        let connected_matches: Vec<Device> = matches
            .iter()
            .filter(|device| connected(&device.mac))
            .cloned()
            .collect();
        if connected_matches.len() == 1 {
            return Ok(connected_matches
                .into_iter()
                .next()
                .expect("one connected match exists"));
        }
        bail!("Multiple devices matched the requested name. Use --mac to choose one explicitly.");
    }

    if let Some(env_mac) = env::var_os("AIRPODS_MAC") {
        let env_mac = env_mac.to_string_lossy().to_string();
        return discover_device(Some(&env_mac), None);
    }

    let candidates: Vec<Device> = devices
        .into_iter()
        .filter(|device| airpods_like_name(&device.name))
        .collect();
    if candidates.is_empty() {
        bail!("No AirPods-like devices found. Pair first or pass --mac / AIRPODS_MAC.");
    }
    if candidates.len() == 1 {
        return Ok(candidates.into_iter().next().expect("one candidate exists"));
    }

    let connected_candidates: Vec<Device> = candidates
        .iter()
        .filter(|device| connected(&device.mac))
        .cloned()
        .collect();
    if connected_candidates.len() == 1 {
        return Ok(connected_candidates
            .into_iter()
            .next()
            .expect("one connected candidate exists"));
    }

    bail!("Multiple AirPods-like devices found. Use --mac to choose one explicitly.");
}

fn current_default_sink() -> Option<String> {
    let result = run_command(
        ["pactl", "get-default-sink"],
        Duration::from_secs(10),
        false,
    )
    .ok()?;
    if result.code != 0 {
        return None;
    }
    let sink = result.stdout.trim();
    (!sink.is_empty()).then(|| sink.to_owned())
}

fn list_sinks() -> Result<Vec<Sink>> {
    let result = run_command(
        ["pactl", "list", "short", "sinks"],
        Duration::from_secs(10),
        true,
    )?;
    Ok(parse_short_sinks(&result.stdout))
}

fn list_cards() -> Result<Vec<Card>> {
    let result = run_command(
        ["pactl", "list", "short", "cards"],
        Duration::from_secs(10),
        true,
    )?;
    Ok(parse_short_cards(&result.stdout))
}

fn list_sink_inputs() -> Vec<i32> {
    let result = match run_command(
        ["pactl", "list", "short", "sink-inputs"],
        Duration::from_secs(10),
        false,
    ) {
        Ok(result) => result,
        Err(_) => return Vec::new(),
    };

    if result.code != 0 {
        return Vec::new();
    }

    result
        .stdout
        .lines()
        .filter_map(|line| line.split_whitespace().next()?.parse().ok())
        .collect()
}

fn find_airpods_sink(mac: &str) -> Result<Option<Sink>> {
    let mac_colon = normalize_mac_colon(mac);
    let mac_underscore = normalize_mac_underscore(mac);
    Ok(list_sinks()?
        .into_iter()
        .find(|sink| sink.name.contains(&mac_colon) || sink.name.contains(&mac_underscore)))
}

fn find_airpods_card(mac: &str) -> Result<Option<Card>> {
    let mac_underscore = normalize_mac_underscore(mac);
    Ok(list_cards()?
        .into_iter()
        .find(|card| card.name.contains(&mac_underscore)))
}

fn find_fallback_sink(exclude_name: Option<&str>) -> Result<Option<Sink>> {
    Ok(list_sinks()?
        .into_iter()
        .find(|sink| exclude_name != Some(sink.name.as_str()) && !sink.name.starts_with("bluez_")))
}

fn bluetoothctl_command(
    subcommand: &[&str],
    timeout: Duration,
    check: bool,
) -> Result<CommandResult> {
    let mut args = vec![OsString::from("bluetoothctl")];
    args.extend(subcommand.iter().map(OsString::from));
    run_command(args, timeout, check)
}

fn set_default_sink(sink_name: &str, move_streams: bool) -> Result<()> {
    run_command(
        ["pactl", "set-default-sink", sink_name],
        Duration::from_secs(10),
        true,
    )?;
    if move_streams {
        for sink_input in list_sink_inputs() {
            let _ = run_command(
                [
                    "pactl",
                    "move-sink-input",
                    &sink_input.to_string(),
                    sink_name,
                ],
                Duration::from_secs(10),
                false,
            );
        }
    }
    Ok(())
}

fn wait_for_sink(mac: &str, timeout_seconds: f64, poll_interval: f64) -> Result<Option<Sink>> {
    let deadline = Instant::now() + Duration::from_secs_f64(timeout_seconds.max(0.1));
    while Instant::now() < deadline {
        if let Some(sink) = find_airpods_sink(mac)? {
            return Ok(Some(sink));
        }
        thread::sleep(Duration::from_secs_f64(poll_interval.max(0.05)));
    }
    Ok(None)
}

fn restart_audio_stack() -> Result<()> {
    run_command(
        [
            "systemctl",
            "--user",
            "restart",
            "wireplumber",
            "pipewire",
            "pipewire-pulse",
        ],
        Duration::from_secs(20),
        true,
    )?;
    Ok(())
}

fn ensure_a2dp_profile(mac: &str) -> Result<Option<Card>> {
    let card = find_airpods_card(mac)?;
    if let Some(card) = card.clone() {
        let _ = run_command(
            ["pactl", "set-card-profile", card.name.as_str(), "a2dp-sink"],
            Duration::from_secs(10),
            false,
        );
    }
    Ok(card)
}

fn do_connect(device: &Device, pair: bool) -> Result<(Value, i32)> {
    let _ = bluetoothctl_command(&["scan", "off"], Duration::from_secs(8), false);

    if connected(&device.mac) {
        return Ok((json!({ "connected": true, "already_connected": true }), 0));
    }

    let connect_result = bluetoothctl_command(
        &["connect", device.mac.as_str()],
        Duration::from_secs(20),
        false,
    )?;
    let combined =
        format!("{}\n{}", connect_result.stdout, connect_result.stderr).to_ascii_lowercase();
    if connect_result.code == 0 || combined.contains("connection successful") {
        return Ok((json!({ "connected": true, "already_connected": false }), 0));
    }

    if !pair {
        return Ok((
            json!({
                "connected": false,
                "error": non_empty_or(&combined.trim().to_owned(), "connect failed"),
            }),
            1,
        ));
    }

    let _ = bluetoothctl_command(
        &["pair", device.mac.as_str()],
        Duration::from_secs(25),
        false,
    );
    let _ = bluetoothctl_command(
        &["trust", device.mac.as_str()],
        Duration::from_secs(10),
        false,
    );
    let connect_result = bluetoothctl_command(
        &["connect", device.mac.as_str()],
        Duration::from_secs(20),
        false,
    )?;
    let combined =
        format!("{}\n{}", connect_result.stdout, connect_result.stderr).to_ascii_lowercase();
    if connect_result.code == 0 || combined.contains("connection successful") {
        return Ok((
            json!({ "connected": true, "paired_during_connect": true }),
            0,
        ));
    }

    Ok((
        json!({
            "connected": false,
            "error": non_empty_or(&combined.trim().to_owned(), "connect failed after pair"),
        }),
        1,
    ))
}

fn do_disconnect(device: &Device) -> Result<(Value, i32)> {
    let result = bluetoothctl_command(
        &["disconnect", device.mac.as_str()],
        Duration::from_secs(12),
        false,
    )?;
    let combined = format!("{}\n{}", result.stdout, result.stderr).to_ascii_lowercase();
    let disconnected =
        result.code == 0 || combined.contains("successful") || combined.contains("not connected");
    Ok((
        json!({ "disconnected": disconnected }),
        if disconnected { 0 } else { 1 },
    ))
}

fn build_status(device: &Device) -> Result<StatusPayload> {
    let info = bluetooth_info(&device.mac);
    let sink = find_airpods_sink(&device.mac)?;
    let card = find_airpods_card(&device.mac)?;
    let default_sink = current_default_sink();
    Ok(StatusPayload {
        name: info
            .get("Name")
            .cloned()
            .unwrap_or_else(|| device.name.clone()),
        alias: info
            .get("Alias")
            .cloned()
            .unwrap_or_else(|| device.name.clone()),
        mac: device.mac.clone(),
        paired: info.get("Paired").is_some_and(|value| value == "yes"),
        trusted: info.get("Trusted").is_some_and(|value| value == "yes"),
        connected: info.get("Connected").is_some_and(|value| value == "yes"),
        breder_connected: info
            .get("BREDR.Connected")
            .is_some_and(|value| value == "yes"),
        sink: sink.as_ref().map(|sink| sink.name.clone()),
        sink_state: sink.as_ref().map(|sink| sink.state.clone()),
        card: card.map(|card| card.name),
        default_is_airpods: sink
            .as_ref()
            .zip(default_sink.as_ref())
            .is_some_and(|(sink, current)| sink.name == *current),
        default_sink,
        battery: None,
        metadata: None,
        magic_keys: None,
        noise_control_mode: None,
        conversational_awareness_enabled: None,
        raw_packets: None,
    })
}

fn state_dir() -> Result<PathBuf> {
    if let Some(base) = env::var_os("XDG_STATE_HOME") {
        return Ok(PathBuf::from(base).join("linux-airpods-cli"));
    }
    let Some(home) = env::var_os("HOME") else {
        bail!("HOME is not set")
    };
    Ok(PathBuf::from(home)
        .join(".local")
        .join("state")
        .join("linux-airpods-cli"))
}

fn default_cache_file(mac: &str) -> Result<PathBuf> {
    Ok(state_dir()?.join(format!(
        "linux-airpods-cli-{}.json",
        normalize_mac_underscore(mac)
    )))
}

fn write_json_file(file_path: &Path, payload: &Value) -> bool {
    if let Err(error) = write_json_file_inner(file_path, payload) {
        eprintln!(
            "warning: failed to write cache file {}: {error}",
            file_path.display()
        );
        return false;
    }
    true
}

fn write_json_file_inner(file_path: &Path, payload: &Value) -> Result<()> {
    if let Some(parent) = file_path.parent() {
        fs::create_dir_all(parent)
            .with_context(|| format!("Failed to create cache directory: {}", parent.display()))?;
    }

    let tmp_path = file_path.with_extension(
        file_path
            .extension()
            .map(|ext| format!("{}.tmp", ext.to_string_lossy()))
            .unwrap_or_else(|| "tmp".to_owned()),
    );

    let bytes = serde_json::to_vec(payload).context("Failed to encode JSON cache payload")?;
    fs::write(&tmp_path, bytes)
        .with_context(|| format!("Failed to write cache temp file: {}", tmp_path.display()))?;
    fs::rename(&tmp_path, file_path).with_context(|| {
        format!(
            "Failed to move cache temp file {} to {}",
            tmp_path.display(),
            file_path.display()
        )
    })?;
    Ok(())
}

fn print_human_status(status: &StatusPayload) {
    println!("Name:            {}", status.name);
    println!("MAC:             {}", status.mac);
    println!("Paired:          {}", yes_no(status.paired));
    println!("Trusted:         {}", yes_no(status.trusted));
    println!("Connected:       {}", yes_no(status.connected));
    println!("BR/EDR:          {}", yes_no(status.breder_connected));
    println!("Card:            {}", status.card.as_deref().unwrap_or("-"));
    println!("Sink:            {}", status.sink.as_deref().unwrap_or("-"));
    println!(
        "Sink state:      {}",
        status.sink_state.as_deref().unwrap_or("-")
    );
    println!(
        "Default sink:    {}",
        status.default_sink.as_deref().unwrap_or("-")
    );
    println!("Default AirPods: {}", yes_no(status.default_is_airpods));

    if let Some(battery) = &status.battery {
        println!("Battery:");
        for (name, component) in [
            ("left", &battery.left),
            ("right", &battery.right),
            ("case", &battery.case_unit),
            ("headset", &battery.headset),
        ] {
            if component.available {
                let charging = if component.charging { " charging" } else { "" };
                let level = component
                    .level
                    .map(|level| level.to_string())
                    .unwrap_or_else(|| "-".to_owned());
                println!("  {name:<7} {level}%{charging}");
            } else {
                println!("  {name:<7} unavailable");
            }
        }
    }

    if let Some(metadata) = &status.metadata {
        println!("Metadata:");
        println!(
            "  Model number:  {}",
            metadata.model_number.as_deref().unwrap_or("-")
        );
        println!(
            "  Manufacturer:  {}",
            metadata.manufacturer.as_deref().unwrap_or("-")
        );
    }

    if let Some(mode) = status.noise_control_mode {
        println!("Noise control:   {mode}");
    }
    if let Some(enabled) = status.conversational_awareness_enabled {
        println!("CA enabled:      {}", yes_no(enabled));
    }
}

fn yes_no(value: bool) -> &'static str {
    if value { "yes" } else { "no" }
}

fn wake_airpods_sink(mac: &str) -> bool {
    let Ok(Some(sink)) = find_airpods_sink(mac) else {
        return false;
    };

    let Ok(mut temp_file) = Builder::new()
        .prefix("linux-airpods-cli-")
        .suffix(".wav")
        .tempfile()
    else {
        return false;
    };

    if temp_file.write_all(&build_silence_wav(0.2)).is_err() || temp_file.flush().is_err() {
        return false;
    }

    if which("paplay").is_ok() {
        let envs = vec![("PULSE_SINK".to_owned(), sink.name.clone())];
        return run_command_with_env(
            [
                OsString::from("paplay"),
                temp_file.path().as_os_str().to_os_string(),
            ],
            Duration::from_secs(5),
            false,
            &envs,
        )
        .is_ok_and(|result| result.code == 0);
    }

    if which("pw-play").is_ok() {
        return run_command(
            [
                OsString::from("pw-play"),
                OsString::from("--target"),
                OsString::from(&sink.name),
                temp_file.path().as_os_str().to_os_string(),
            ],
            Duration::from_secs(5),
            false,
        )
        .is_ok_and(|result| result.code == 0);
    }

    false
}

fn build_silence_wav(duration_seconds: f64) -> Vec<u8> {
    let sample_rate = 48_000u32;
    let channels = 2u16;
    let bits_per_sample = 16u16;
    let bytes_per_sample = (bits_per_sample / 8) as u32;
    let frames = (sample_rate as f64 * duration_seconds.max(0.0)).round() as u32;
    let data_size = frames * channels as u32 * bytes_per_sample;
    let byte_rate = sample_rate * channels as u32 * bytes_per_sample;
    let block_align = channels * (bits_per_sample / 8);

    let mut wav = Vec::with_capacity((44 + data_size) as usize);
    wav.extend_from_slice(b"RIFF");
    wav.extend_from_slice(&(36 + data_size).to_le_bytes());
    wav.extend_from_slice(b"WAVE");
    wav.extend_from_slice(b"fmt ");
    wav.extend_from_slice(&16u32.to_le_bytes());
    wav.extend_from_slice(&1u16.to_le_bytes());
    wav.extend_from_slice(&channels.to_le_bytes());
    wav.extend_from_slice(&sample_rate.to_le_bytes());
    wav.extend_from_slice(&byte_rate.to_le_bytes());
    wav.extend_from_slice(&block_align.to_le_bytes());
    wav.extend_from_slice(&bits_per_sample.to_le_bytes());
    wav.extend_from_slice(b"data");
    wav.extend_from_slice(&data_size.to_le_bytes());
    wav.resize((44 + data_size) as usize, 0);
    wav
}

fn cmd_status(mac: Option<&str>, name: Option<&str>, args: &StatusArgs) -> Result<()> {
    let device = discover_device(mac, name)?;
    let mut status = build_status(&device)?;
    if args.aap {
        if !status.connected {
            bail!("AirPods are not connected, so AAP status is unavailable.");
        }
        let mut session = AAPSession::open(&device.mac, args.wait)?;
        let aap_state = session.query(false, args.raw_packets)?;
        status.apply_session_state(aap_state);
    }
    if args.json {
        print_json(&status)?;
    } else {
        print_human_status(&status);
    }
    Ok(())
}

fn cmd_devices(args: &DevicesArgs) -> Result<()> {
    let devices = bluetooth_devices()?;
    let payload: Vec<Value> = devices
        .into_iter()
        .filter(|device| airpods_like_name(&device.name))
        .map(|device| {
            json!({
                "mac": device.mac,
                "name": device.name,
                "connected": connected(&device.mac),
            })
        })
        .collect();

    if args.json {
        print_json(&payload)?;
    } else {
        if payload.is_empty() {
            println!("No AirPods-like devices found.");
        }
        for item in &payload {
            let suffix = if item
                .get("connected")
                .and_then(Value::as_bool)
                .unwrap_or(false)
            {
                " (connected)"
            } else {
                ""
            };
            println!(
                "{}  {}{}",
                item.get("mac").and_then(Value::as_str).unwrap_or(""),
                item.get("name").and_then(Value::as_str).unwrap_or(""),
                suffix
            );
        }
    }
    Ok(())
}

fn cmd_sink(mac: Option<&str>, name: Option<&str>) -> Result<()> {
    let device = discover_device(mac, name)?;
    let sink = find_airpods_sink(&device.mac)?
        .ok_or_else(|| anyhow!("No AirPods sink exists yet. Connect first."))?;
    println!("{}", sink.name);
    Ok(())
}

fn cmd_connect(mac: Option<&str>, name: Option<&str>, args: &ConnectArgs) -> Result<()> {
    let device = discover_device(mac, name)?;
    let (payload, code) = do_connect(&device, args.pair)?;
    if code != 0 {
        let error = payload
            .get("error")
            .and_then(Value::as_str)
            .unwrap_or("Failed to connect");
        bail!(error.to_owned());
    }

    let _ = ensure_a2dp_profile(&device.mac)?;
    let sink = wait_for_sink(&device.mac, args.wait, 0.5)?
        .ok_or_else(|| anyhow!("Bluetooth connected, but no PipeWire sink appeared."))?;

    if args.set_default {
        set_default_sink(&sink.name, args.move_streams)?;
    }

    let status = build_status(&device)?;
    if args.json {
        print_json(&status)?;
    } else {
        println!("Connected to {}", status.name);
        println!("Sink: {}", status.sink.as_deref().unwrap_or("-"));
        if args.set_default {
            println!(
                "Default sink set to: {}",
                status.sink.as_deref().unwrap_or("-")
            );
        }
    }
    Ok(())
}

fn cmd_disconnect(mac: Option<&str>, name: Option<&str>, args: &DisconnectArgs) -> Result<()> {
    let device = discover_device(mac, name)?;
    let sink = find_airpods_sink(&device.mac)?;
    let was_default = sink
        .as_ref()
        .zip(current_default_sink().as_ref())
        .is_some_and(|(sink, default_sink)| sink.name == *default_sink);

    let (payload, code) = do_disconnect(&device)?;
    if code != 0 {
        bail!("Failed to disconnect device");
    }

    if was_default && args.fallback {
        if let Some(fallback) = find_fallback_sink(sink.as_ref().map(|sink| sink.name.as_str()))? {
            set_default_sink(&fallback.name, args.move_streams)?;
        }
    }

    if args.json {
        print_json(&payload)?;
    } else {
        println!("Disconnected {}", device.name);
    }
    Ok(())
}

fn cmd_set_default(mac: Option<&str>, name: Option<&str>, args: &SetDefaultArgs) -> Result<()> {
    let device = discover_device(mac, name)?;
    let sink = find_airpods_sink(&device.mac)?
        .ok_or_else(|| anyhow!("No AirPods sink exists yet. Connect first."))?;
    set_default_sink(&sink.name, args.move_streams)?;
    println!("{}", sink.name);
    Ok(())
}

fn cmd_fix(mac: Option<&str>, name: Option<&str>, args: &FixArgs) -> Result<()> {
    let device = discover_device(mac, name)?;
    let _ = bluetoothctl_command(&["scan", "off"], Duration::from_secs(8), false);
    let _ = bluetoothctl_command(
        &["disconnect", device.mac.as_str()],
        Duration::from_secs(10),
        false,
    );
    if args.restart_audio {
        restart_audio_stack()?;
        thread::sleep(Duration::from_secs_f64(args.restart_wait.max(0.0)));
    }
    let (payload, code) = do_connect(&device, false)?;
    if code != 0 {
        let error = payload
            .get("error")
            .and_then(Value::as_str)
            .unwrap_or("Reconnect failed during fix");
        bail!(error.to_owned());
    }

    let _ = ensure_a2dp_profile(&device.mac)?;
    let sink = wait_for_sink(&device.mac, args.wait, 0.5)?
        .ok_or_else(|| anyhow!("Reconnect succeeded, but no AirPods sink appeared."))?;
    set_default_sink(&sink.name, args.move_streams)?;

    let status = build_status(&device)?;
    if args.json {
        print_json(&status)?;
    } else {
        println!("Recovered {}", status.name);
        println!("Sink: {}", status.sink.as_deref().unwrap_or("-"));
        println!(
            "Default sink: {}",
            status.default_sink.as_deref().unwrap_or("-")
        );
    }
    Ok(())
}

fn cmd_battery(mac: Option<&str>, name: Option<&str>, args: &BatteryArgs) -> Result<()> {
    let device = discover_device(mac, name)?;
    if !connected(&device.mac) {
        bail!("AirPods must be connected before querying AAP battery.");
    }
    if args.wake {
        let _ = wake_airpods_sink(&device.mac);
    }
    let mut session = AAPSession::open(&device.mac, args.wait)?;
    let state = session.query(false, args.raw_packets)?;
    let battery = state
        .battery
        .clone()
        .ok_or_else(|| anyhow!("No battery packet received from the AirPods."))?;

    let payload = json!({
        "name": device.name,
        "mac": device.mac,
        "battery": state.battery,
        "metadata": state.metadata,
        "magic_keys": state.magic_keys,
        "noise_control_mode": state.noise_control_mode,
        "conversational_awareness_enabled": state.conversational_awareness_enabled,
        "raw_packets": state.raw_packets,
    });

    if args.json {
        print_json(&payload)?;
    } else {
        println!("Name: {}", device.name);
        println!("MAC:  {}", device.mac);
        for (name, component) in [
            ("left", &battery.left),
            ("right", &battery.right),
            ("case", &battery.case_unit),
            ("headset", &battery.headset),
        ] {
            if component.available {
                let charging = if component.charging { " charging" } else { "" };
                let level = component
                    .level
                    .map(|level| level.to_string())
                    .unwrap_or_else(|| "-".to_owned());
                println!("{:<7} {}%{}", capitalize(name), level, charging);
            } else {
                println!("{:<7} unavailable", capitalize(name));
            }
        }
    }
    Ok(())
}

fn cmd_keys(mac: Option<&str>, name: Option<&str>, args: &KeysArgs) -> Result<()> {
    let device = discover_device(mac, name)?;
    if !connected(&device.mac) {
        bail!("AirPods must be connected before requesting AAP keys.");
    }
    if args.wake {
        let _ = wake_airpods_sink(&device.mac);
    }
    let mut session = AAPSession::open(&device.mac, args.wait)?;
    let state = session.query(true, args.raw_packets)?;
    let keys = state
        .magic_keys
        .ok_or_else(|| anyhow!("No Magic Cloud Keys packet received from the AirPods."))?;
    let payload = json!({
        "name": device.name,
        "mac": device.mac,
        "irk": keys.irk,
        "enc_key": keys.enc_key,
    });
    if args.json {
        print_json(&payload)?;
    } else {
        println!("Name:    {}", device.name);
        println!("MAC:     {}", device.mac);
        println!(
            "IRK:     {}",
            payload.get("irk").and_then(Value::as_str).unwrap_or("")
        );
        println!(
            "Enc key: {}",
            payload.get("enc_key").and_then(Value::as_str).unwrap_or("")
        );
    }
    Ok(())
}

fn cmd_monitor(mac: Option<&str>, name: Option<&str>, args: &MonitorArgs) -> Result<()> {
    let device = discover_device(mac, name)?;
    let cache_file = args
        .cache_file
        .clone()
        .unwrap_or(default_cache_file(&device.mac)?);

    loop {
        let is_connected = connected(&device.mac);
        if !is_connected {
            let payload = json!({
                "name": device.name,
                "mac": device.mac,
                "connected": false,
                "timestamp": now_timestamp(),
            });
            let _ = write_json_file(&cache_file, &payload);
            if args.once {
                bail!("AirPods must be connected before starting monitor.");
            }
            thread::sleep(Duration::from_secs_f64(args.retry_interval.max(0.1)));
            continue;
        }

        let session_result: Result<()> = (|| {
            if args.wake {
                let _ = wake_airpods_sink(&device.mac);
            }
            let mut session = AAPSession::open(&device.mac, args.wait)?;
            let mut state = session.query(args.request_keys, args.raw_packets)?;
            let mut payload = json!({
                "name": device.name,
                "mac": device.mac,
                "connected": true,
                "timestamp": now_timestamp(),
                "battery": state.battery,
                "metadata": state.metadata,
                "magic_keys": state.magic_keys,
                "noise_control_mode": state.noise_control_mode,
                "conversational_awareness_enabled": state.conversational_awareness_enabled,
                "raw_packets": state.raw_packets,
            });
            let _ = write_json_file(&cache_file, &payload);
            if args.once {
                print_json(&payload)?;
                return Ok(());
            }

            let mut last_notification_request = Instant::now();
            loop {
                let got_packet =
                    session.read_next(&mut state, args.poll_interval, args.raw_packets)?;
                let now = Instant::now();
                if got_packet {
                    payload = json!({
                        "name": device.name,
                        "mac": device.mac,
                        "connected": true,
                        "timestamp": now_timestamp(),
                        "battery": state.battery,
                        "metadata": state.metadata,
                        "magic_keys": state.magic_keys,
                        "noise_control_mode": state.noise_control_mode,
                        "conversational_awareness_enabled": state.conversational_awareness_enabled,
                        "raw_packets": state.raw_packets,
                    });
                    let _ = write_json_file(&cache_file, &payload);
                } else if now.duration_since(last_notification_request)
                    >= Duration::from_secs_f64(args.refresh_interval.max(0.1))
                {
                    session.request_notifications()?;
                    last_notification_request = now;
                }
                if !connected(&device.mac) {
                    break;
                }
            }
            Ok(())
        })();

        if let Err(error) = session_result {
            let payload = json!({
                "name": device.name,
                "mac": device.mac,
                "connected": connected(&device.mac),
                "error": error.to_string(),
                "timestamp": now_timestamp(),
            });
            let _ = write_json_file(&cache_file, &payload);
            if args.once {
                return Err(error);
            }
            thread::sleep(Duration::from_secs_f64(args.retry_interval.max(0.1)));
            continue;
        }

        if args.once {
            return Ok(());
        }
        thread::sleep(Duration::from_secs_f64(args.retry_interval.max(0.1)));
    }
}

fn print_json<T: Serialize>(value: &T) -> Result<()> {
    println!("{}", serde_json::to_string_pretty(value)?);
    Ok(())
}

fn now_timestamp() -> f64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|duration| duration.as_secs_f64())
        .unwrap_or(0.0)
}

fn non_empty_or(value: &str, fallback: &str) -> String {
    if value.is_empty() {
        fallback.to_owned()
    } else {
        value.to_owned()
    }
}

fn capitalize(value: &str) -> String {
    let mut chars = value.chars();
    match chars.next() {
        Some(first) => first.to_uppercase().chain(chars).collect(),
        None => String::new(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::aap::{parse_battery_packet, parse_magic_keys_packet, parse_metadata_packet};

    #[test]
    fn parses_bluetooth_devices() {
        let output = "Device 74:77:86:57:67:2A AirPods Pro 3\nDevice 28:34:FF:27:A6:B2 iPhone\n";
        let devices = parse_bluetooth_devices(output);
        assert_eq!(
            devices,
            vec![
                Device {
                    mac: "74:77:86:57:67:2A".into(),
                    name: "AirPods Pro 3".into(),
                },
                Device {
                    mac: "28:34:FF:27:A6:B2".into(),
                    name: "iPhone".into(),
                },
            ]
        );
    }

    #[test]
    fn parses_bluetooth_info() {
        let output =
            "Name: AirPods Pro 3\nConnected: yes\nPaired: yes\nTrusted: no\nBREDR.Connected: yes\n";
        let info = parse_bluetooth_info(output);
        assert_eq!(info.get("Name"), Some(&"AirPods Pro 3".to_owned()));
        assert_eq!(info.get("Connected"), Some(&"yes".to_owned()));
        assert_eq!(info.get("BREDR.Connected"), Some(&"yes".to_owned()));
    }

    #[test]
    fn parses_short_sinks() {
        let output = "63 alsa_output.pci-0000_00_1f.3-platform-sof_sdw.HiFi__Speaker__sink PipeWire s32le 2ch 48000Hz SUSPENDED\n105 bluez_output.74:77:86:57:67:2A PipeWire float32le 2ch 48000Hz RUNNING\n";
        let sinks = parse_short_sinks(output);
        assert_eq!(sinks[1].name, "bluez_output.74:77:86:57:67:2A");
        assert_eq!(sinks[1].state, "RUNNING");
    }

    #[test]
    fn parses_short_cards() {
        let output = "52 alsa_card.pci-0000_00_1f.3-platform-sof_sdw alsa\n94 bluez_card.74_77_86_57_67_2A module-bluez5-device.c\n";
        let cards = parse_short_cards(output);
        assert_eq!(cards[1].name, "bluez_card.74_77_86_57_67_2A");
    }

    #[test]
    fn detects_airpods_names() {
        assert!(airpods_like_name("AirPods Pro 3"));
        assert!(airpods_like_name("Beats Studio Buds"));
        assert!(!airpods_like_name("iPhone"));
    }

    #[test]
    fn parses_aap_battery_packet() {
        let packet = hex::decode("04000400040003020160020104016202010801000401").unwrap();
        let battery = parse_battery_packet(&packet).unwrap();
        assert_eq!(battery.right.level, Some(96));
        assert_eq!(battery.left.level, Some(98));
        assert!(battery.left.available);
        assert!(!battery.case_unit.available);
        assert_eq!(battery.primary.as_deref(), Some("right"));
        assert_eq!(battery.secondary.as_deref(), Some("left"));
    }

    #[test]
    fn parses_aap_metadata_packet() {
        let packet = hex::decode(
            "040004001d0002ed000400416972506f64732050726f2033004133303633004170706c6520496e632e00",
        )
        .unwrap();
        let metadata = parse_metadata_packet(&packet).unwrap();
        assert_eq!(metadata.device_name.as_deref(), Some("AirPods Pro 3"));
        assert_eq!(metadata.model_number.as_deref(), Some("A3063"));
        assert_eq!(metadata.manufacturer.as_deref(), Some("Apple Inc."));
    }

    #[test]
    fn parses_magic_keys_packet() {
        let packet = hex::decode(
            "0400040031000201001000b0b6db71ab06f97626b7715fad2622040400100030f3071127b81b2cb42a809fafe9d1f8",
        )
        .unwrap();
        let keys = parse_magic_keys_packet(&packet).unwrap();
        assert_eq!(keys.irk, "b0b6db71ab06f97626b7715fad262204");
        assert_eq!(keys.enc_key, "30f3071127b81b2cb42a809fafe9d1f8");
    }
}
