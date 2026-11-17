use std::fs;
use std::io::{self, Read};
use std::os::unix::fs::PermissionsExt;
use std::os::unix::process::CommandExt;
use std::process::{Command, Output, Stdio};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Mutex, MutexGuard};
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant};

use luyaz_policy::scenario::Network;
use luyaz_scenario::Io;
use luyaz_wifi::{self, WifiLock};
use serde_json::{json, Map, Value};

const SCAN_IFACE: &str = "scen-scan0";
const SCAN_OWNER: &str = "/tmp/scenario.scan-wdev";
const RESET_DUMP: &str = "/sys/devices/virtual/misc/wcn/devices/reset_dump";
const RESET_DUMP_SAFE: &str = "/sys/module/wcn_bsp/parameters/reset_dump_safe";
const RESET_DUMP_OWNER: &str = "/tmp/scenario.reset-dump-owner";
const RESET_DUMP_VALUE: &str = "reset_dump";
static RESET_DUMP_LIMIT_LOGGED: AtomicBool = AtomicBool::new(false);
static SCAN_LOCK: Mutex<()> = Mutex::new(());
const CMD_LIMIT: Duration = Duration::from_secs(20);
const WIFI_POLL: Duration = Duration::from_millis(1500);
const WIFI_ON_DEADLINE: Duration = Duration::from_secs(45);
const WIFI_OFF_DEADLINE: Duration = Duration::from_secs(20);

pub struct OpenWrtIo;

struct NoHost;

impl luyaz_wifi::Host for NoHost {
    fn uci_get(&self, _key: &str) -> Option<String> {
        None
    }

    fn write_aps(&self, _ap_2g: bool, _ap_5g: bool) -> Result<Option<String>, String> {
        Err("OpenWrt Wi-Fi changes must use OpenWrtIo".to_string())
    }

    fn vendor_on(&self) -> Result<(), String> {
        Err("OpenWrt Wi-Fi changes must use OpenWrtIo".to_string())
    }

    fn reload(&self) -> Result<Value, String> {
        Err("OpenWrt Wi-Fi changes must use OpenWrtIo".to_string())
    }
}

static NO_HOST: NoHost = NoHost;

fn lock_recover<T>(mutex: &Mutex<T>) -> MutexGuard<'_, T> {
    mutex.lock().unwrap_or_else(|poisoned| poisoned.into_inner())
}

fn kill_command_group(pid: u32) {
    unsafe extern "C" {
        fn kill(pid: i32, signal: i32) -> i32;
    }
    // Each command is spawned into its own process group below. Descendants
    // must not outlive its deadline or keep the output pipes open forever.
    unsafe { kill(-(pid as i32), 9); }
}

fn output_within(command: &mut Command, limit: Duration) -> io::Result<Output> {
    command.process_group(0);
    let mut child = command.stdin(Stdio::null()).stdout(Stdio::piped()).stderr(Stdio::piped()).spawn()?;
    let stdout = child.stdout.take().map(drain);
    let stderr = child.stderr.take().map(drain);
    let started = Instant::now();
    let mut nap = Duration::from_millis(1);
    let status = loop {
        match child.try_wait() {
            Ok(Some(status)) => break status,
            Ok(None) if started.elapsed() < limit => {
                thread::sleep(nap);
                nap = (nap * 2).min(Duration::from_millis(20));
            }
            other => {
                kill_command_group(child.id());
                let _ = child.kill();
                let _ = child.wait();
                return Err(match other {
                    Err(error) => error,
                    _ => io::Error::new(io::ErrorKind::TimedOut, format!("timed out after {} ms", limit.as_millis())),
                });
            }
        }
    };
    kill_command_group(child.id());
    let join = |handle: Option<JoinHandle<Vec<u8>>>| handle.and_then(|h| h.join().ok()).unwrap_or_default();
    Ok(Output { status, stdout: join(stdout), stderr: join(stderr) })
}

fn drain(mut reader: impl Read + Send + 'static) -> JoinHandle<Vec<u8>> {
    thread::spawn(move || {
        let mut bytes = Vec::new();
        let _ = reader.read_to_end(&mut bytes);
        bytes
    })
}

fn run(program: &str, args: &[&str], limit: Duration) -> Result<String, String> {
    let output = output_within(&mut Command::new(program).args(args), limit)
        .map_err(|error| format!("{program}: {error}"))?;
    if !output.status.success() {
        return Err(format!(
            "{program} {}: {}",
            args.join(" "),
            String::from_utf8_lossy(&output.stderr).trim()
        ));
    }
    Ok(String::from_utf8_lossy(&output.stdout).into_owned())
}

fn discovery_devices(text: &str) -> Vec<u64> {
    let mut devices = Vec::new();
    let mut on_phy0 = false;
    let mut wdev = None;
    for line in text.lines().map(str::trim) {
        if line.starts_with("phy#") {
            on_phy0 = line == "phy#0";
            wdev = None;
        } else if line.starts_with("Interface ") || line == "Unnamed/non-netdev interface" {
            wdev = None;
        } else if let Some(id) = line.strip_prefix("wdev 0x") {
            wdev = u64::from_str_radix(id, 16).ok();
        } else if on_phy0 && line == "type P2P-device" {
            if let Some(id) = wdev.take() {
                devices.push(id);
            }
        }
    }
    devices
}

fn created_discovery_device(before: &[u64], after: &[u64]) -> Result<u64, String> {
    let mut created = after.iter().copied().filter(|id| !before.contains(id));
    match (created.next(), created.next()) {
        (Some(id), None) => Ok(id),
        _ => Err("cannot uniquely identify the new phy0 discovery device".to_string()),
    }
}

/// "02:00:00:00:00:aa" — six hex pairs. This iw also prints element lines that
/// start with "BSS " after trimming ("BSS color: 47", "BSS load:", "BSS color
/// disabled"); only a real MAC starts a BSS.
fn is_mac(text: &str) -> bool {
    let bytes = text.as_bytes();
    bytes.len() == 17
        && bytes.iter().enumerate().all(|(i, b)| match i {
            2 | 5 | 8 | 11 | 14 => *b == b':',
            _ => b.is_ascii_hexdigit(),
        })
}

fn parse_scan(text: &str) -> Vec<Network> {
    let mut networks = Vec::new();
    let mut bssid = String::new();
    let mut ssid: Option<String> = None;
    let mut signal: Option<f64> = None;

    let flush = |out: &mut Vec<Network>, bssid: &str, ssid: Option<String>, signal: Option<f64>| {
        if bssid.is_empty() {
            return;
        }
        let candidate = Network {
            ssid: ssid.unwrap_or_default(),
            bssid: bssid.to_string(),
            signal: signal.unwrap_or(-100.0),
        };
        if let Some(existing) = out.iter_mut().find(|network| network.bssid == candidate.bssid) {
            if candidate.signal > existing.signal {
                *existing = candidate;
            }
        } else {
            out.push(candidate);
        }
    };

    for line in text.lines() {
        let trimmed = line.trim();
        if let Some(rest) = trimmed.strip_prefix("BSS ") {
            let candidate = rest.split('(').next().unwrap_or("").trim().to_ascii_lowercase();
            if is_mac(&candidate) {
                flush(&mut networks, &bssid, ssid.take(), signal.take());
                bssid = candidate;
            }
        } else if let Some(rest) = trimmed.strip_prefix("signal:") {
            signal = rest.trim().split_whitespace().next().and_then(|value| value.parse().ok());
        } else if let Some(rest) = trimmed.strip_prefix("SSID:") {
            ssid = Some(rest.trim().to_string());
        }
    }
    flush(&mut networks, &bssid, ssid, signal);
    networks.sort_by(|a, b| b.signal.partial_cmp(&a.signal).unwrap_or(std::cmp::Ordering::Equal));
    networks
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum DumpOwnership {
    Owned,
    Surrendered,
}

fn normalized_dump_mode(value: &str) -> Option<&'static str> {
    match value.trim() {
        "dump" => Some("dump"),
        "reset" => Some("reset"),
        RESET_DUMP_VALUE => Some(RESET_DUMP_VALUE),
        _ => None,
    }
}

fn read_dump_owner() -> Result<Option<(&'static str, DumpOwnership)>, String> {
    let record = match fs::read_to_string(RESET_DUMP_OWNER) {
        Ok(record) => record,
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(None),
        Err(error) => return Err(format!("reset_dump ownership: {error}")),
    };
    let mut lines = record.lines();
    let original = lines.next().and_then(normalized_dump_mode)
        .ok_or_else(|| "invalid reset_dump ownership record".to_string())?;
    let ownership = match lines.next() {
        Some("owned") => DumpOwnership::Owned,
        Some("surrendered") => DumpOwnership::Surrendered,
        _ => return Err("invalid reset_dump ownership record".to_string()),
    };
    if lines.next().is_some() {
        return Err("invalid reset_dump ownership record".to_string());
    }
    Ok(Some((original, ownership)))
}

fn borrow_reset_dump() {
    let capability = fs::read_to_string(RESET_DUMP_SAFE).ok();
    if !matches!(capability.as_deref().map(str::trim), Some("Y" | "1")) {
        if !RESET_DUMP_LIMIT_LOGGED.swap(true, Ordering::Relaxed) {
            eprintln!("scenario: reset_dump borrowing unavailable; kernel reset_dump_safe is missing or disabled");
        }
        return;
    }
    let current = match fs::read_to_string(RESET_DUMP) {
        Ok(current) => current,
        Err(_) => return,
    };
    let current_mode = normalized_dump_mode(&current);
    match read_dump_owner() {
        Ok(Some((_original, DumpOwnership::Surrendered))) => return,
        Ok(Some((_original, DumpOwnership::Owned))) if current_mode == Some(RESET_DUMP_VALUE) => return,
        Ok(Some((original, DumpOwnership::Owned))) => {
            let record = format!("{original}\nsurrendered\n");
            if let Err(error) = atomic_write(RESET_DUMP_OWNER, record.as_bytes()) {
                eprintln!("scenario: reset_dump operator-change marker failed: {error}");
            }
            return;
        }
        Ok(None) => {
            let Some(original) = current_mode else { return };
            if original == RESET_DUMP_VALUE {
                return;
            }
            let record = format!("{original}\nowned\n");
            if let Err(error) = atomic_write(RESET_DUMP_OWNER, record.as_bytes()) {
                eprintln!("scenario: reset_dump ownership save failed: {error}");
                return;
            }
        }
        Err(error) => {
            eprintln!("scenario: {error}");
            return;
        }
    }
    if let Err(error) = fs::write(RESET_DUMP, format!("{RESET_DUMP_VALUE}\n")) {
        eprintln!("scenario: reset_dump borrow failed: {error}");
    }
}

fn restore_reset_dump() -> Result<(), String> {
    let Some((original, ownership)) = read_dump_owner()? else { return Ok(()) };
    if ownership == DumpOwnership::Owned {
        match fs::read_to_string(RESET_DUMP) {
            Ok(current) if normalized_dump_mode(&current) == Some(RESET_DUMP_VALUE) => {
                fs::write(RESET_DUMP, format!("{original}\n"))
                    .map_err(|error| format!("restore reset_dump: {error}"))?;
            }
            Ok(_) => {},
            Err(error) if error.kind() == io::ErrorKind::NotFound => {},
            Err(error) => return Err(format!("read reset_dump for restore: {error}")),
        }
    }
    fs::remove_file(RESET_DUMP_OWNER)
        .map_err(|error| format!("remove reset_dump ownership: {error}"))
}

fn cleanup_recorded_scan(limit: Duration) -> Result<(), String> {
    let owner = match fs::read_to_string(SCAN_OWNER) {
        Ok(owner) => owner,
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(()),
        Err(error) => return Err(format!("scan ownership: {error}")),
    };
    let owner = owner.trim();
    let id = owner.strip_prefix("0x")
        .and_then(|id| u64::from_str_radix(id, 16).ok())
        .ok_or_else(|| "invalid recorded discovery device".to_string())?;
    let devices = discovery_devices(&run("iw", &["dev"], limit)?);
    if devices.contains(&id) {
        let _ = run("iw", &["wdev", owner, "p2p", "stop"], limit);
        run("iw", &["wdev", owner, "del"], limit)?;
    }
    fs::remove_file(SCAN_OWNER).map_err(|error| format!("scan ownership: {error}"))
}

pub(crate) fn release_scan_device() -> Result<(), String> {
    let _guard = lock_recover(&SCAN_LOCK);
    let cleanup = cleanup_recorded_scan(CMD_LIMIT);
    let restore = restore_reset_dump();
    cleanup.and(restore)
}

struct ScanIfaceGuard {
    before: Vec<u64>,
    wdev: String,
    keep_alive: bool,
}

impl ScanIfaceGuard {
    fn wdev(&mut self) -> Result<&str, String> {
        if self.wdev.is_empty() {
            let after = discovery_devices(&run("iw", &["dev"], CMD_LIMIT)?);
            self.wdev = format!("0x{:x}", created_discovery_device(&self.before, &after)?);
        }
        Ok(&self.wdev)
    }
}

impl Drop for ScanIfaceGuard {
    fn drop(&mut self) {
        if self.keep_alive {
            return;
        }
        if let Ok(wdev) = self.wdev() {
            let _ = run("iw", &["wdev", wdev, "p2p", "stop"], CMD_LIMIT);
            if run("iw", &["wdev", wdev, "del"], CMD_LIMIT).is_ok() {
                let _ = fs::remove_file(SCAN_OWNER);
            }
        }
    }
}

fn discovery_device() -> Result<ScanIfaceGuard, String> {
    borrow_reset_dump();
    let before = discovery_devices(&run("iw", &["dev"], CMD_LIMIT)?);
    let mut wdev = String::new();
    match fs::read_to_string(SCAN_OWNER) {
        Ok(mut owner) => {
            owner.truncate(owner.trim_end().len());
            let id = owner.strip_prefix("0x")
                .and_then(|id| u64::from_str_radix(id, 16).ok())
                .ok_or_else(|| "invalid recorded discovery device".to_string())?;
            if before.contains(&id) {
                wdev = owner;
            } else {
                fs::remove_file(SCAN_OWNER).map_err(|error| format!("scan ownership: {error}"))?;
            }
        }
        Err(error) if error.kind() == io::ErrorKind::NotFound => {}
        Err(error) => return Err(format!("scan ownership: {error}")),
    }
    let created = wdev.is_empty();
    if created {
        run("iw", &["phy", "phy0", "interface", "add", SCAN_IFACE, "type", "__p2pdev"], CMD_LIMIT)?;
    }
    let mut guard = ScanIfaceGuard { before, wdev, keep_alive: false };
    let wdev = guard.wdev()?;
    if created {
        atomic_write(SCAN_OWNER, wdev.as_bytes()).map_err(|error| format!("scan ownership: {error}"))?;
    }
    // NL80211 start is idempotent for a running discovery device. Reuse its
    // firmware context, including after procd restarts, instead of cold-loading
    // WCN and CMD_OPEN on every home scan (the saved RF assertions occur there).
    run("iw", &["wdev", wdev, "p2p", "start"], CMD_LIMIT)?;
    guard.keep_alive = true;
    Ok(guard)
}

fn scan_networks() -> Result<Vec<Network>, String> {
    let mut guard = discovery_device()?;
    let wdev = guard.wdev()?;

    // Retry on a command error or an empty result (this firmware's scan sometimes
    // comes back with nothing), but a small result is a valid observation: a quiet
    // street with three networks is home too, and treating it as a failure would
    // fail open to away and turn the AP on for nothing.
    let mut last_error = "scan failed".to_string();
    for attempt in 0..3 {
        match run("iw", &["wdev", wdev, "scan"], CMD_LIMIT) {
            Ok(text) => {
                let networks = parse_scan(&text);
                if !networks.is_empty() {
                    return Ok(networks);
                }
                last_error = "scan saw no networks".to_string();
            }
            Err(error) => last_error = error,
        }
        if attempt < 2 {
            thread::sleep(Duration::from_millis(1500));
        }
    }
    // With a live AP, rebuilding an invalid scan context stays warm. While
    // home/AP-off, retain it until fail-open brings the AP back or resets WCN.
    if hostapd_enabled() {
        guard.keep_alive = false;
    }
    Err(last_error)
}

fn ap_is_on_from_disabled(disabled: Option<&str>) -> bool {
    disabled != Some("1")
}

fn ap_is_on() -> bool {
    ap_is_on_from_disabled(uci_get("wireless.ap0.disabled").as_deref())
}

fn uci_get(key: &str) -> Option<String> {
    run("uci", &["-q", "get", key], Duration::from_secs(5))
        .ok()
        .map(|value| value.trim().to_string())
        .filter(|value| !value.is_empty())
}

fn hostapd_enabled() -> bool {
    run("ubus", &["call", "hostapd.wlan0", "get_status"], Duration::from_secs(5))
        .map(|status| status.contains("\"status\": \"ENABLED\""))
        .unwrap_or(false)
}


fn beaconing_now() -> bool {
    hostapd_enabled()
}

fn wifi_apply_locked(_lk: &WifiLock, ap_2g: bool, ap_5g: bool) -> Result<(), String> {
    let action_started = Instant::now();
    let want_on = ap_2g || ap_5g;
    run("uci", &["set", if want_on { "wireless.ap0.disabled=0" } else { "wireless.ap0.disabled=1" }], Duration::from_secs(10))?;
    let commit_result = run("uci", &["commit", "wireless"], Duration::from_secs(10));
    let reload_result = run("ubus", &["call", "network", "reload"], Duration::from_secs(15));
    // Still attempt reload after a commit failure, but never report a failed
    // persistent write or reload as a successful action just because RF is up.
    commit_result?;
    reload_result?;
    let deadline = if want_on { WIFI_ON_DEADLINE } else { WIFI_OFF_DEADLINE };
    let started = Instant::now();
    let mut last_observed: String;
    loop {
        if want_on {
            let status = run("ubus", &["call", "hostapd.wlan0", "get_status"], Duration::from_secs(5));
            match status {
                Ok(status) if status.contains("\"status\": \"ENABLED\"") => return Ok(()),
                Ok(status) => last_observed = format!("hostapd status: {}", status.trim()),
                Err(error) => last_observed = error,
            }
        } else {
            match run("iw", &["dev"], CMD_LIMIT) {
                Ok(text) if !text.lines().any(|line| line.trim() == "type AP") => return Ok(()),
                Ok(text) => last_observed = format!("AP interface still present: {}", text.lines().filter(|line| line.trim().starts_with("Interface ")).collect::<Vec<_>>().join(", ")),
                Err(error) => last_observed = error,
            }
        }
        if started.elapsed() >= deadline {
            if want_on {
                let cleanup_budget = luyaz_wifi::MAX_HOLD
                    .saturating_sub(action_started.elapsed())
                    .saturating_sub(Duration::from_secs(5))
                    .as_secs() / 3;
                if cleanup_budget > 0 {
                    let _scan = lock_recover(&SCAN_LOCK);
                    if let Err(error) = cleanup_recorded_scan(Duration::from_secs(cleanup_budget.min(5))) {
                        eprintln!("scenario: scan cleanup before radio recovery failed: {error}");
                    }
                }
                let budget = luyaz_wifi::MAX_HOLD
                    .saturating_sub(action_started.elapsed())
                    .saturating_sub(Duration::from_secs(5))
                    .as_secs().min(70);
                if budget > 0 {
                    let recovery = run("/usr/share/scenario/radio-recover.sh", &[&budget.to_string()], Duration::from_secs(budget));
                    if recovery.is_err() {
                        // A timed-out helper cannot run its EXIT trap. Reconcile
                        // the saved configuration without forcing any UCI value.
                        let _ = run("ubus", &["-t", "3", "call", "network.wireless", "up", "{\"device\":\"radio0\"}"], Duration::from_secs(3));
                    }
                    recovery?;
                    if hostapd_enabled() {
                        return Ok(());
                    }
                }
            }
            return Err(format!("Wi-Fi did not turn {} within {}s (last observation: {last_observed})", if want_on { "on" } else { "off" }, deadline.as_secs()));
        }
        thread::sleep(WIFI_POLL);
    }
}


fn put_back_sets(set: &Map<String, Value>) -> Vec<String> {
    set.iter()
        .filter(|(key, _)| key.starts_with("wireless."))
        .map(|(key, value)| {
            let value = value.as_str().map(str::to_string).unwrap_or_else(|| value.to_string());
            format!("{key}={value}")
        })
        .collect()
}

fn atomic_write(path: &str, data: &[u8]) -> io::Result<()> {
    let tmp = format!("{path}.tmp");
    let existing_mode = fs::metadata(path).ok().map(|metadata| metadata.permissions().mode());
    fs::write(&tmp, data)?;
    if let Some(mode) = existing_mode {
        fs::set_permissions(&tmp, fs::Permissions::from_mode(mode))?;
    }
    fs::rename(tmp, path)
}

impl Io for OpenWrtIo {
    fn scan(&self) -> Result<Vec<Network>, String> {
        let _guard = lock_recover(&SCAN_LOCK);
        scan_networks()
    }

    fn stop_scanning(&self) {
        if let Err(error) = release_scan_device() {
            eprintln!("scenario: scan resource cleanup failed: {error}");
        }
    }

    fn wifi(&self) -> &dyn luyaz_wifi::Host {
        &NO_HOST
    }

    fn beaconing(&self) -> bool {
        beaconing_now()
    }

    fn wifi_fully_on(&self) -> bool {
        beaconing_now() && ap_is_on()
    }

    fn wifi_snapshot(&self) -> (bool, bool) {
        let on = ap_is_on();
        (on, on)
    }

    fn wifi_apply(&self, ap_2g: bool, ap_5g: bool) -> Result<(), String> {
        let lock = luyaz_wifi::lock(luyaz_wifi::ENGINE_WAIT)?;
        wifi_apply_locked(&lock, ap_2g, ap_5g)
    }

    fn wifi_apply_locked(&self, lock: &WifiLock, ap_2g: bool, ap_5g: bool) -> Result<(), String> {
        wifi_apply_locked(lock, ap_2g, ap_5g)
    }

    fn wifi_put_back(&self, set: Map<String, Value>) {
        for value in put_back_sets(&set) {
            if let Err(error) = run("uci", &["set", &value], Duration::from_secs(10)) {
                eprintln!("scenario: rollback uci set failed: {error}");
            }
        }
        if let Err(error) = run("uci", &["commit", "wireless"], Duration::from_secs(10)) {
            eprintln!("scenario: rollback uci commit failed: {error}");
        }
        if let Err(error) = run("ubus", &["call", "network", "reload"], Duration::from_secs(15)) {
            eprintln!("scenario: rollback network reload failed: {error}");
        }
    }

    fn uci_get(&self, key: &str) -> Option<String> {
        uci_get(key)
    }

    fn exec(&self, method: &str, path: &str, _body: &[u8]) -> Result<(), String> {
        Err(format!("no local API on OpenWrt: {method} {path}"))
    }

    fn get(&self, _path: &str) -> (u16, Value) {
        (503, json!({"error": "no local API"}))
    }

    fn set_auto_sleep(&self, _enabled: bool) {}

    fn charging(&self) -> Option<bool> {
        None
    }

    fn sim_mcc(&self) -> Option<String> {
        None
    }

    fn publish(&self, text: &str) {
        if let Err(error) = atomic_write("/tmp/scenario.tick.json", text.as_bytes()) {
            eprintln!("scenario: tick publish failed: {error}");
        }
    }

    fn switched(&self, from: &str, to: &str, name: &str, ok: bool) {
        let message = format!("switched {from:?} -> {to:?} ({name}) ok={ok}");
        let _ = run("logger", &["-t", "scenario", &message], Duration::from_secs(5));
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn timed_out_command_cannot_leave_descendant_work_running() {
        let dir = std::env::temp_dir().join(format!("scenario-command-timeout-{}", std::process::id()));
        fs::create_dir_all(&dir).unwrap();
        let ready = dir.join("ready");
        let leaked = dir.join("leaked");
        let result = output_within(
            Command::new("sh")
                .env("SCENARIO_READY", &ready)
                .env("SCENARIO_LEAK", &leaked)
                .args(["-c", "(sleep 0.4; printf leaked > \"$SCENARIO_LEAK\") & printf ready > \"$SCENARIO_READY\"; wait"]),
            Duration::from_millis(150),
        );
        assert_eq!(result.unwrap_err().kind(), io::ErrorKind::TimedOut);
        assert!(ready.exists(), "the command must have spawned its descendant before timeout");
        thread::sleep(Duration::from_millis(600));
        let descendant_ran = leaked.exists();
        fs::remove_dir_all(&dir).unwrap();
        assert!(!descendant_ran, "timeout must kill the entire hardware-command process group");
    }

    #[test]
    fn discovery_devices_exclude_other_phys_and_data_interfaces() {
        let text = "phy#1\n\tUnnamed/non-netdev interface\n\t\twdev 0x100\n\t\ttype P2P-device\n\
                    phy#0\n\tInterface wlan0\n\t\twdev 0x4b\n\t\ttype AP\n\
                    \tUnnamed/non-netdev interface\n\t\twdev 0x4c\n\t\ttype P2P-device\n\
                    \tUnnamed/non-netdev interface\n\t\ttype P2P-device\n";
        assert_eq!(discovery_devices(text), [0x4c]);
    }

    #[test]
    fn discovery_ownership_excludes_preexisting_and_ambiguous_devices() {
        assert_eq!(created_discovery_device(&[0x4c], &[0x4c, 0x4d]).unwrap(), 0x4d);
        assert!(created_discovery_device(&[0x4c], &[0x4c]).is_err());
        assert!(created_discovery_device(&[0x4c], &[0x4c, 0x4d, 0x4e]).is_err());
    }

    #[test]
    fn parses_ap_up_fixture_and_finds_yidan() {
        let networks = parse_scan(include_str!("../fixtures/scan_ap_up.txt"));
        assert!(networks.len() >= 8, "parsed {} networks", networks.len());
        let yidan = networks.iter().find(|network| network.bssid == "24:5a:5f:46:77:de").expect("fixture contains expected BSSID");
        assert_eq!(yidan.ssid, "yidan");
        assert!((yidan.signal - -68.0).abs() < 0.1);
    }

    #[test]
    fn parses_ap_off_fixture() {
        let networks = parse_scan(include_str!("../fixtures/scan_ap_off.txt"));
        assert!(networks.len() >= 8, "parsed {} networks", networks.len());
    }

    #[test]
    fn disabled_key_treats_only_one_as_off() {
        assert!(!ap_is_on_from_disabled(Some("1")));
        assert!(ap_is_on_from_disabled(Some("0")));
        assert!(ap_is_on_from_disabled(None));
        assert!(ap_is_on_from_disabled(Some("")));
    }

    #[test]
    fn put_back_only_builds_wireless_sets() {
        let mut set = Map::new();
        set.insert("wireless.ap0.disabled".to_string(), Value::String("1".to_string()));
        set.insert("wireless.ap0.ssid".to_string(), Value::String("home net".to_string()));
        set.insert("network.lan.ipaddr".to_string(), Value::String("192.0.2.1".to_string()));
        assert_eq!(
            put_back_sets(&set),
            vec!["wireless.ap0.disabled=1", "wireless.ap0.ssid=home net"]
        );
    }
}

#[cfg(test)]
mod phantom_tests {
    use super::*;

    #[test]
    fn he_element_lines_are_not_bss_headers() {
        let text = "BSS 24:5a:5f:b9:77:58(on scen-scan0)\n\
                    \tSSID: yidan\n\
                    \tsignal: -48.00 dBm\n\
                    \tBSS color: 47\n\
                    \tBSS load:\n\
                    \tbss color disabled\n\
                    BSS color disabled\n";
        let networks = parse_scan(text);
        assert_eq!(networks.len(), 1, "{networks:?}");
        assert_eq!(networks[0].bssid, "24:5a:5f:b9:77:58");
        assert_eq!(networks[0].ssid, "yidan");
        assert!(networks.iter().all(|n| is_mac(&n.bssid)));
    }
}
