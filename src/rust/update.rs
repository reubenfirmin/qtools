//! Lightweight update notification shared by pq and dq. The actual AppImage replacement remains
//! the launcher's job; binaries only maintain notification state and check the stable metadata.

use std::fs;
use std::io::{self, IsTerminal};
use std::path::PathBuf;
use std::process::{Command, Stdio};
use std::time::{SystemTime, UNIX_EPOCH};
use std::os::unix::fs::{OpenOptionsExt, PermissionsExt};
use std::os::unix::fs::MetadataExt;
use std::io::Write;

use crate::style;

const CHECK_ENV: &str = "QTOOLS_INTERNAL_UPDATE_CHECK";
const CHECK_TOKEN: &str = "qtools-daily-v1";
const VERSION_URL: &str = "https://repo.qtools.sh/version.json";
const DAY: u64 = 86_400;

fn state_dir() -> Option<PathBuf> {
    state_dir_from(std::env::var_os("XDG_STATE_HOME"), std::env::var_os("HOME"))
}

fn state_dir_from(xdg_state: Option<std::ffi::OsString>, home: Option<std::ffi::OsString>) -> Option<PathBuf> {
    xdg_state.map(PathBuf::from)
        .or_else(|| home.map(|home| PathBuf::from(home).join(".local/state")))
        .map(|path| path.join("qtools"))
}

fn now() -> u64 {
    SystemTime::now().duration_since(UNIX_EPOCH).unwrap_or_default().as_secs()
}

/// Handle the detached checker subprocess before either binary parses its normal arguments.
pub fn run_internal_check() -> bool {
    if std::env::var(CHECK_ENV).as_deref() != Ok(CHECK_TOKEN) { return false; }
    check();
    true
}

/// Append a queued notice to interactive human output, then launch today's check without waiting.
pub fn after_invocation(machine_output: bool, colors: bool) {
    if !machine_output && io::stdout().is_terminal() { show_notice(colors); }
    if !check_due() { return; }
    let Ok(executable) = std::env::current_exe() else { return };
    let _ = Command::new(executable)
        .env(CHECK_ENV, CHECK_TOKEN)
        .stdin(Stdio::null()).stdout(Stdio::null()).stderr(Stdio::null())
        .spawn();
}

fn show_notice(colors: bool) {
    let Some(state) = state_dir() else { return };
    let Some(notice) = queued_notice(&state, now(), colors) else { return };
    println!("\n{notice}");
}

fn queued_notice(state: &std::path::Path, timestamp: u64, colors: bool) -> Option<String> {
    let Ok(latest) = fs::read_to_string(state.join("update-available")) else { return None };
    let snoozed_until = fs::read_to_string(state.join("update-snoozed-until")).ok()
        .and_then(|value| value.trim().parse::<u64>().ok()).unwrap_or(0);
    if timestamp < snoozed_until { return None; }
    let label = style::paint("UPDATE", 36, true, colors);
    let version = style::bold(latest.trim(), colors);
    let update = style::paint("qtools update", 32, true, colors);
    let snooze = style::bold("qtools snooze-update-notification", colors);
    Some(format!("{label}  qtools {version} is available — run {update} or {snooze}"))
}

fn check() {
    let Some(state) = state_dir() else { return };
    check_state(&state, now(), || {
        let response = Command::new("curl")
            .args(["--fail", "--location", "--proto", "=https", "--tlsv1.2", "--silent",
                "--show-error", "--connect-timeout", "3", "--max-time", "8", VERSION_URL])
            .output().ok()?;
        response.status.success().then_some(response.stdout)
    });
}

fn check_state(state: &std::path::Path, timestamp: u64, fetch: impl FnOnce() -> Option<Vec<u8>>) {
    if fs::create_dir_all(state).is_err() { return; }
    let Ok(metadata) = fs::symlink_metadata(state) else { return };
    if !metadata.is_dir() || metadata.file_type().is_symlink()
        || metadata.uid() != unsafe { libc::geteuid() } { return; }
    let _ = fs::set_permissions(state, fs::Permissions::from_mode(0o700));
    let lock = state.join("update-check.lock");
    if fs::create_dir(&lock).is_err() {
        let stale = fs::metadata(&lock).and_then(|meta| meta.modified()).ok()
            .and_then(|modified| SystemTime::now().duration_since(modified).ok())
            .is_some_and(|age| age.as_secs() > 600);
        if !stale || fs::remove_dir(&lock).is_err() || fs::create_dir(&lock).is_err() { return; }
    }
    struct Lock(PathBuf);
    impl Drop for Lock { fn drop(&mut self) { let _ = fs::remove_dir(&self.0); } }
    let _lock = Lock(lock);

    let last = fs::read_to_string(state.join("last-update-check")).ok()
        .and_then(|value| value.trim().parse::<u64>().ok()).unwrap_or(0);
    if timestamp.saturating_sub(last) < DAY { return; }
    if atomic_write(state, "last-update-check", &format!("{timestamp}\n")).is_err() { return; }

    let Some(response) = fetch() else { return };
    let Ok(metadata) = serde_json::from_slice::<serde_json::Value>(&response) else { return };
    let Some(latest) = metadata.get("version").and_then(|value| value.as_str()) else { return };
    let available = state.join("update-available");
    if newer_than(latest, env!("CARGO_PKG_VERSION")) {
        let _ = atomic_write(state, "update-available", &format!("{latest}\n"));
    } else {
        let _ = fs::remove_file(available);
    }
}

fn check_due() -> bool {
    let Some(state) = state_dir() else { return false };
    check_due_at(&state, now())
}

fn check_due_at(state: &std::path::Path, timestamp: u64) -> bool {
    let last = fs::read_to_string(state.join("last-update-check")).ok()
        .and_then(|value| value.trim().parse::<u64>().ok()).unwrap_or(0);
    timestamp.saturating_sub(last) >= DAY
}

fn atomic_write(state: &std::path::Path, name: &str, value: &str) -> io::Result<()> {
    let temporary = state.join(format!(".{name}.{}.tmp", std::process::id()));
    let mut file = fs::OpenOptions::new().write(true).create_new(true).mode(0o600).open(&temporary)?;
    if let Err(error) = file.write_all(value.as_bytes()).and_then(|_| file.sync_all()) {
        let _ = fs::remove_file(&temporary); return Err(error);
    }
    fs::rename(temporary, state.join(name))
}

fn newer_than(candidate: &str, current: &str) -> bool {
    let numbers = |version: &str| version.split('.').map(str::parse::<u64>)
        .collect::<Result<Vec<_>, _>>().ok();
    let (Some(mut candidate), Some(mut current)) = (numbers(candidate), numbers(current)) else { return false };
    let length = candidate.len().max(current.len());
    candidate.resize(length, 0);
    current.resize(length, 0);
    candidate > current
}

#[cfg(test)]
mod tests {
    use std::fs;
    use std::os::unix::fs::PermissionsExt;
    use std::path::PathBuf;

    use super::*;

    fn temporary_state(name: &str) -> PathBuf {
        let path = std::env::temp_dir().join(format!("qtools-update-{name}-{}", std::process::id()));
        let _ = fs::remove_dir_all(&path);
        fs::create_dir(&path).unwrap();
        path
    }

    #[test]
    fn compares_release_versions() {
        assert!(newer_than("0.2.0", "0.1.9"));
        assert!(newer_than("1.0.1", "1.0"));
        assert!(!newer_than("0.1.1", "0.1.1"));
        assert!(!newer_than("0.1.0", "0.1.1"));
        assert!(!newer_than("next", "0.1.1"));
        assert!(!newer_than("0.2.0", "broken"));
        assert!(!newer_than("1.0", "1.0.0"));
    }

    #[test]
    fn daily_check_deadline_handles_missing_invalid_and_future_timestamps() {
        let state = temporary_state("deadline");
        assert!(check_due_at(&state, DAY));
        fs::write(state.join("last-update-check"), "invalid\n").unwrap();
        assert!(check_due_at(&state, DAY));
        fs::write(state.join("last-update-check"), "100\n").unwrap();
        assert!(!check_due_at(&state, 100 + DAY - 1));
        assert!(check_due_at(&state, 100 + DAY));
        assert!(!check_due_at(&state, 99));
        fs::remove_dir_all(state).unwrap();
    }

    #[test]
    fn queued_notice_obeys_snooze_and_color_setting() {
        let state = temporary_state("notice");
        assert!(queued_notice(&state, 100, false).is_none());
        fs::write(state.join("update-available"), "2.0.0\n").unwrap();
        let plain = queued_notice(&state, 100, false).unwrap();
        assert!(plain.contains("UPDATE  qtools 2.0.0 is available"));
        assert!(plain.contains("qtools update"));
        assert!(!plain.contains("\u{1b}["));
        assert!(queued_notice(&state, 100, true).unwrap().contains("\u{1b}["));
        fs::write(state.join("update-snoozed-until"), "200\n").unwrap();
        assert!(queued_notice(&state, 199, false).is_none());
        assert!(queued_notice(&state, 200, false).is_some());
        fs::remove_dir_all(state).unwrap();
    }

    #[test]
    fn atomic_write_is_private_atomic_and_refuses_to_clobber_temp_file() {
        let state = temporary_state("atomic");
        atomic_write(&state, "value", "first\n").unwrap();
        assert_eq!(fs::read_to_string(state.join("value")).unwrap(), "first\n");
        assert_eq!(fs::metadata(state.join("value")).unwrap().permissions().mode() & 0o777, 0o600);
        atomic_write(&state, "value", "second\n").unwrap();
        assert_eq!(fs::read_to_string(state.join("value")).unwrap(), "second\n");
        let temporary = state.join(format!(".blocked.{}.tmp", std::process::id()));
        fs::write(&temporary, "occupied").unwrap();
        assert_eq!(atomic_write(&state, "blocked", "new").unwrap_err().kind(), io::ErrorKind::AlreadyExists);
        fs::remove_dir_all(state).unwrap();
    }

    #[test]
    fn checker_queues_and_clears_available_versions() {
        let state = temporary_state("check-success");
        check_state(&state, DAY, || Some(br#"{"version":"999.0.0"}"#.to_vec()));
        assert_eq!(fs::read_to_string(state.join("last-update-check")).unwrap(), format!("{DAY}\n"));
        assert_eq!(fs::read_to_string(state.join("update-available")).unwrap(), "999.0.0\n");
        check_state(&state, DAY * 2, || Some(br#"{"version":"0.0.0"}"#.to_vec()));
        assert!(!state.join("update-available").exists());
        assert!(!state.join("update-check.lock").exists());
        assert_eq!(fs::metadata(&state).unwrap().permissions().mode() & 0o777, 0o700);
        fs::remove_dir_all(state).unwrap();
    }

    #[test]
    fn checker_throttles_and_records_failed_fetches_without_queueing_notice() {
        let state = temporary_state("check-failure");
        let called = std::cell::Cell::new(false);
        fs::write(state.join("last-update-check"), format!("{}\n", DAY)).unwrap();
        check_state(&state, DAY + 1, || { called.set(true); None });
        assert!(!called.get());
        check_state(&state, DAY * 2, || None);
        assert_eq!(fs::read_to_string(state.join("last-update-check")).unwrap(), format!("{}\n", DAY * 2));
        assert!(!state.join("update-available").exists());
        check_state(&state, DAY * 3, || Some(b"not json".to_vec()));
        check_state(&state, DAY * 4, || Some(br#"{"other":true}"#.to_vec()));
        assert!(!state.join("update-available").exists());
        fs::remove_dir_all(state).unwrap();
    }

    #[test]
    fn checker_refuses_symlink_state_and_active_lock() {
        use std::os::unix::fs::symlink;

        let parent = temporary_state("security");
        let real = parent.join("real");
        let linked = parent.join("linked");
        fs::create_dir(&real).unwrap();
        symlink(&real, &linked).unwrap();
        check_state(&linked, DAY, || Some(br#"{"version":"999.0.0"}"#.to_vec()));
        assert!(!real.join("last-update-check").exists());

        fs::create_dir(real.join("update-check.lock")).unwrap();
        check_state(&real, DAY, || Some(br#"{"version":"999.0.0"}"#.to_vec()));
        assert!(!real.join("last-update-check").exists());
        fs::remove_dir_all(parent).unwrap();
    }

    #[test]
    fn state_directory_prefers_xdg_then_falls_back_to_home() {
        assert_eq!(state_dir_from(Some("/state".into()), Some("/home/user".into())), Some(PathBuf::from("/state/qtools")));
        assert_eq!(state_dir_from(None, Some("/home/user".into())), Some(PathBuf::from("/home/user/.local/state/qtools")));
        assert_eq!(state_dir_from(None, None), None);
    }
}
