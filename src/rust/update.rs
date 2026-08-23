//! Lightweight update notification shared by pq and dq. The actual AppImage replacement remains
//! the launcher's job; binaries only maintain notification state and check the stable metadata.

use std::fs;
use std::io::{self, IsTerminal};
use std::path::PathBuf;
use std::process::{Command, Stdio};
use std::time::{SystemTime, UNIX_EPOCH};

use crate::style;

const CHECK_ENV: &str = "QTOOLS_INTERNAL_UPDATE_CHECK";
const VERSION_URL: &str = "https://repo.qtools.sh/version.json";
const DAY: u64 = 86_400;

fn state_dir() -> Option<PathBuf> {
    std::env::var_os("XDG_STATE_HOME").map(PathBuf::from)
        .or_else(|| std::env::var_os("HOME").map(|home| PathBuf::from(home).join(".local/state")))
        .map(|path| path.join("qtools"))
}

fn now() -> u64 {
    SystemTime::now().duration_since(UNIX_EPOCH).unwrap_or_default().as_secs()
}

/// Handle the detached checker subprocess before either binary parses its normal arguments.
pub fn run_internal_check() -> bool {
    if std::env::var_os(CHECK_ENV).is_none() { return false; }
    check();
    true
}

/// Append a queued notice to interactive human output, then launch today's check without waiting.
pub fn after_invocation(machine_output: bool, colors: bool) {
    if !machine_output && io::stdout().is_terminal() { show_notice(colors); }
    let Ok(executable) = std::env::current_exe() else { return };
    let _ = Command::new(executable)
        .env(CHECK_ENV, "1")
        .stdin(Stdio::null()).stdout(Stdio::null()).stderr(Stdio::null())
        .spawn();
}

fn show_notice(colors: bool) {
    let Some(state) = state_dir() else { return };
    let Ok(latest) = fs::read_to_string(state.join("update-available")) else { return };
    let snoozed_until = fs::read_to_string(state.join("update-snoozed-until")).ok()
        .and_then(|value| value.trim().parse::<u64>().ok()).unwrap_or(0);
    if now() < snoozed_until { return; }
    let label = style::paint("UPDATE", 36, true, colors);
    let version = style::bold(latest.trim(), colors);
    let update = style::paint("qtools update", 32, true, colors);
    let snooze = style::bold("qtools snooze-update-notification", colors);
    println!("\n{label}  qtools {version} is available — run {update} or {snooze}");
}

fn check() {
    let Some(state) = state_dir() else { return };
    if fs::create_dir_all(&state).is_err() { return; }
    let lock = state.join("update-check.lock");
    if fs::create_dir(&lock).is_err() { return; }
    struct Lock(PathBuf);
    impl Drop for Lock { fn drop(&mut self) { let _ = fs::remove_dir(&self.0); } }
    let _lock = Lock(lock);

    let timestamp = now();
    let last = fs::read_to_string(state.join("last-update-check")).ok()
        .and_then(|value| value.trim().parse::<u64>().ok()).unwrap_or(0);
    if timestamp.saturating_sub(last) < DAY { return; }
    if fs::write(state.join("last-update-check"), format!("{timestamp}\n")).is_err() { return; }

    let response = Command::new("curl")
        .args(["--fail", "--location", "--proto", "=https", "--tlsv1.2", "--silent",
            "--show-error", "--connect-timeout", "3", "--max-time", "8", VERSION_URL])
        .output();
    let Ok(response) = response else { return };
    if !response.status.success() { return; }
    let Ok(metadata) = serde_json::from_slice::<serde_json::Value>(&response.stdout) else { return };
    let Some(latest) = metadata.get("version").and_then(|value| value.as_str()) else { return };
    let available = state.join("update-available");
    if newer_than(latest, env!("CARGO_PKG_VERSION")) {
        let _ = fs::write(available, format!("{latest}\n"));
    } else {
        let _ = fs::remove_file(available);
    }
}

fn newer_than(candidate: &str, current: &str) -> bool {
    let numbers = |version: &str| version.split('-').next().unwrap_or(version).split('.')
        .map(|part| part.parse::<u64>().unwrap_or(0)).collect::<Vec<_>>();
    let mut candidate = numbers(candidate);
    let mut current = numbers(current);
    let length = candidate.len().max(current.len());
    candidate.resize(length, 0);
    current.resize(length, 0);
    candidate > current
}

#[cfg(test)]
mod tests {
    use super::newer_than;

    #[test]
    fn compares_release_versions() {
        assert!(newer_than("0.2.0", "0.1.9"));
        assert!(newer_than("1.0.1", "1.0"));
        assert!(!newer_than("0.1.1", "0.1.1"));
        assert!(!newer_than("0.1.0", "0.1.1"));
    }
}
