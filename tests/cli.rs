use std::fs;
use std::path::PathBuf;
use std::io::{BufRead, BufReader, Write};
use std::process::{Command, Output, Stdio};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::{SystemTime, UNIX_EPOCH};

fn state_dir(label: &str) -> PathBuf {
    static NEXT_STATE: AtomicUsize = AtomicUsize::new(0);
    let sequence = NEXT_STATE.fetch_add(1, Ordering::Relaxed);
    let path = std::env::temp_dir().join(format!("qtools-cli-{label}-{}-{sequence}", std::process::id()));
    let _ = fs::remove_dir_all(&path);
    fs::create_dir_all(path.join("qtools")).unwrap();
    let timestamp = SystemTime::now().duration_since(UNIX_EPOCH).unwrap().as_secs();
    fs::write(path.join("qtools/last-update-check"), format!("{timestamp}\n")).unwrap();
    path
}

fn run(binary: &str, label: &str, arguments: &[&str]) -> Output {
    let executable = match binary {
        "pq" => env!("CARGO_BIN_EXE_pq"),
        "dq" => env!("CARGO_BIN_EXE_dq"),
        _ => unreachable!(),
    };
    Command::new(executable)
        .args(arguments)
        .env("XDG_STATE_HOME", state_dir(label))
        .env_remove("QTOOLS_INTERNAL_UPDATE_CHECK")
        .output()
        .unwrap()
}

fn run_with_input(binary: &str, label: &str, arguments: &[&str], input: &[u8]) -> Output {
    let executable = if binary == "pq" { env!("CARGO_BIN_EXE_pq") } else { env!("CARGO_BIN_EXE_dq") };
    let mut child = Command::new(executable)
        .args(arguments)
        .env("XDG_STATE_HOME", state_dir(label))
        .env_remove("QTOOLS_INTERNAL_UPDATE_CHECK")
        .stdin(Stdio::piped()).stdout(Stdio::piped()).stderr(Stdio::piped())
        .spawn().unwrap();
    child.stdin.take().unwrap().write_all(input).unwrap();
    child.wait_with_output().unwrap()
}

fn successful_json(binary: &str, label: &str, arguments: &[&str]) -> serde_json::Value {
    let output = run(binary, label, arguments);
    assert!(output.status.success(), "stderr: {}", String::from_utf8_lossy(&output.stderr));
    serde_json::from_slice(&output.stdout).unwrap_or_else(|error| {
        panic!("invalid JSON ({error}): {}", String::from_utf8_lossy(&output.stdout))
    })
}

#[test]
fn commands_generate_shell_completions() {
    let pq = run("pq", "pq-completion", &["--generate-completion", "bash"]);
    assert!(pq.status.success(), "{}", String::from_utf8_lossy(&pq.stderr));
    let pq_script = String::from_utf8_lossy(&pq.stdout);
    assert!(pq_script.contains("--state"));
    assert!(pq_script.contains("listen established bound other"));

    let dq = run("dq", "dq-completion", &["--generate-completion", "fish"]);
    assert!(dq.status.success(), "{}", String::from_utf8_lossy(&dq.stderr));
    let dq_script = String::from_utf8_lossy(&dq.stdout);
    assert!(dq_script.contains("complete -c dq"));
    assert!(dq_script.contains("older-than"));
}

#[test]
fn pq_process_network_and_io_views_run_end_to_end() {
    let process = successful_json("pq", "process", &["--json", "--interval", "0", "--top", "2"]);
    assert!(process.get("clusters").is_some());

    let network = successful_json("pq", "network", &["--net", "--json", "--interval", "0"]);
    assert!(network.get("clusters").is_some());

    let io = successful_json("pq", "io", &["--io", "--json", "--interval", "0"]);
    assert!(io.get("clusters").is_some());
}

#[test]
fn pq_diagnostic_views_run_end_to_end() {
    for view in ["--blocked", "--oom", "--limits", "--pss", "--maps", "--unix"] {
        let value = successful_json("pq", &view[2..], &[view, "--json", "--interval", "0"]);
        assert!(value.is_object(), "{view} did not return an object");
    }

    let mut sleeper = Command::new("sleep").arg("30").spawn().unwrap();
    let pid = sleeper.id().to_string();
    let access = successful_json("pq", "access", &["--access", "--pid", &pid, "--json", "--interval", "1"]);
    let _ = sleeper.kill();
    let _ = sleeper.wait();
    assert_eq!(access["mode"], "access");
}

#[test]
fn pq_human_renderers_run_end_to_end() {
    for (label, arguments) in [
        ("cpu-text", vec!["--no-color", "--interval", "0", "--top", "2"]),
        ("memory-text", vec!["--memory", "--verbose", "--no-color", "--interval", "0", "--top", "2"]),
        ("swap-text", vec!["--swap", "--no-color", "--interval", "0", "--top", "2"]),
        ("tree-text", vec!["--tree", "--no-color", "--interval", "0", "--top", "2"]),
        ("net-text", vec!["--net", "--verbose", "--no-color", "--interval", "0", "--top", "2"]),
        ("io-text", vec!["--read", "--verbose", "--no-color", "--interval", "0", "--top", "2"]),
        ("fds-text", vec!["--fds", "--verbose", "--no-color", "--interval", "0", "--top", "2"]),
        ("deleted-text", vec!["--deleted", "--verbose", "--no-color", "--interval", "0", "--top", "2"]),
        ("holders-text", vec!["--mount", "/tmp", "--no-color", "--interval", "0", "--top", "2"]),
        ("blocked-text", vec!["--blocked", "--no-color", "--interval", "0"]),
        ("limits-text", vec!["--limits", "--verbose", "--no-color", "--interval", "0", "--top", "2"]),
        ("maps-text", vec!["--maps", "--verbose", "--no-color", "--interval", "0", "--top", "2"]),
    ] {
        let output = run("pq", label, &arguments);
        assert!(output.status.success(), "{label}: {}", String::from_utf8_lossy(&output.stderr));
        assert!(!output.stdout.is_empty(), "{label} produced no report");
    }
}

#[test]
fn pq_help_and_validation_errors_have_stable_streams_and_statuses() {
    let help = run("pq", "help", &["help", "reporting"]);
    assert!(help.status.success());
    assert!(String::from_utf8_lossy(&help.stdout).contains("pq reporting"));
    assert!(help.stderr.is_empty());

    let memory_help = run("pq", "memory-help", &["help", "memory"]);
    assert!(memory_help.status.success());
    assert!(String::from_utf8_lossy(&memory_help.stdout).contains("--access"));

    let invalid = run("pq", "invalid", &["--kill"]);
    assert!(!invalid.status.success());
    assert!(String::from_utf8_lossy(&invalid.stderr).contains("needs a pattern"));

    let unknown = run("pq", "unknown-help", &["help", "definitely-unknown"]);
    assert!(!unknown.status.success());
    assert!(String::from_utf8_lossy(&unknown.stderr).contains("unknown help topic"));
}

#[test]
fn internal_update_checker_entrypoint_exits_before_cli_parsing() {
    let state = state_dir("internal-check");
    let output = Command::new(env!("CARGO_BIN_EXE_pq"))
        .arg("--definitely-invalid-if-parsed")
        .env("XDG_STATE_HOME", state)
        .env("QTOOLS_INTERNAL_UPDATE_CHECK", "qtools-daily-v1")
        .output().unwrap();
    assert!(output.status.success());
    assert!(output.stdout.is_empty());
    assert!(output.stderr.is_empty());
}

#[test]
fn rich_output_terminal_probe_restores_a_pseudo_terminal_cleanly() {
    let executable = env!("CARGO_BIN_EXE_pq");
    let harness = r#"import os,pty,sys
pid,fd=pty.fork()
if pid == 0:
    os.execv(sys.argv[1],[sys.argv[1],'--interval','0','--top','1'])
data=b''
while True:
    try:
        chunk=os.read(fd,4096)
        if not chunk: break
        data+=chunk
    except OSError:
        break
_,status=os.waitpid(pid,0)
sys.stdout.buffer.write(data)
sys.exit(os.waitstatus_to_exitcode(status))"#;
    let output = Command::new("python3")
        .args(["-c", harness, executable])
        .env("XDG_STATE_HOME", state_dir("pseudo-terminal"))
        .env("TERM", "dumb")
        .output().unwrap();
    assert!(output.status.success(), "{}", String::from_utf8_lossy(&output.stderr));
    assert!(String::from_utf8_lossy(&output.stdout).contains("pq"));
}

// Emulate protocol replies without opening desktop terminals. The deadline makes an unexpected
// second probe fail the test instead of hanging the suite; terminal modes are checked after exit.
fn terminal_report(binary: &str, arguments: &[&str], profile: &str, columns: usize) -> serde_json::Value {
    let executable = if binary == "pq" { env!("CARGO_BIN_EXE_pq") } else { env!("CARGO_BIN_EXE_dq") };
    let harness = r#"import fcntl,json,os,pty,re,select,signal,struct,sys,termios,time
profile=sys.argv[2]
ready_read,ready_write=os.pipe()
pid,fd=pty.fork()
if pid == 0:
    os.close(ready_write)
    os.read(ready_read,1)
    os.close(ready_read)
    for key in ('TERM_PROGRAM','LC_TERMINAL','KONSOLE_VERSION','TMUX','STY','SSH_CONNECTION'):
        os.environ.pop(key,None)
    os.environ['TERM']='xterm-256color'
    if profile == 'iterm': os.environ['TERM_PROGRAM']='WezTerm'
    if profile == 'iterm_ssh':
        os.environ['LC_TERMINAL']='iTerm2'
        os.environ['SSH_CONNECTION']='192.0.2.1 12345 192.0.2.2 22'
    if profile == 'tmux':
        os.environ['TMUX']='/tmp/test-tmux,123,0'
        os.environ['TERM_PROGRAM']='WezTerm'
    if profile == 'screen': os.environ['STY']='123.test-screen'
    if profile == 'term_tmux': os.environ['TERM']='tmux-256color'
    if profile in ('kitty_remote','text_ssh'):
        os.environ['SSH_CONNECTION']='192.0.2.1 12345 192.0.2.2 22'
    os.execv(sys.argv[1],[sys.argv[1],*sys.argv[4:]])
fcntl.ioctl(fd,termios.TIOCSWINSZ,struct.pack('HHHH',24,int(sys.argv[3]),0,0))
original=termios.tcgetattr(fd)
os.close(ready_read)
os.write(ready_write,b'1')
os.close(ready_write)
data=bytearray()
pending=bytearray()
queries=re.compile(rb'\x1b_G[^\x1b]*\x1b\\|\x1b\[c|\x1b\[5n')
attributes=0
status=None
deadline=time.monotonic()+5
try:
    while time.monotonic()<deadline:
        if select.select([fd],[],[],0.05)[0]:
            try: chunk=os.read(fd,65536)
            except OSError: break
            if not chunk: break
            data.extend(chunk)
            pending.extend(chunk)
            while True:
                match=queries.search(pending)
                if match is None: break
                token=match.group()
                del pending[:match.end()]
                if token==b'\x1b[c':
                    attributes+=1
                    if profile=='sixel': os.write(fd,b'\x1b[?62;1;4c')
                    elif profile.startswith('text') or (profile.startswith('kitty') and attributes==1):
                        os.write(fd,b'\x1b[?62;1;2c')
                    # Kitty answers only the original DA1: rendering must not query Sixel again.
                elif token==b'\x1b[5n' and profile=='kitty_local':
                    os.write(fd,b'\x1b[0n')
                elif b'a=q' in token and profile.startswith('kitty'):
                    reply=b'OK' if b't=d' in token or profile=='kitty_local' else b'ENOENT'
                    os.write(fd,b'\x1b_Gi=31;'+reply+b'\x1b\\')
        done,result=os.waitpid(pid,os.WNOHANG)
        if done:
            status=result
            break
    if status is None:
        done,result=os.waitpid(pid,os.WNOHANG)
        if done: status=result
    if status is None:
        os.kill(pid,signal.SIGKILL)
        os.waitpid(pid,0)
        raise AssertionError('terminal report hung: '+profile)
    assert os.waitstatus_to_exitcode(status)==0, data[-1000:].decode(errors='replace')
    assert termios.tcgetattr(fd)==original, 'terminal mode was not restored: '+profile
    chart=re.search(rb'\x1b_Gf=32[^;]*',data)
    width=re.search(rb',c=(\d+),r=(\d+)',chart.group()) if chart else None
    print(json.dumps({'attributes':attributes,'kitty':chart is not None,
        'width':int(width[1]) if width else None,'height':int(width[2]) if width else None,
        'iterm':b'\x1b]1337;File=' in data,'sixel':re.search(rb'\x1bP[0-9;]*q',data) is not None,
        'bars':b'###' in data}))
finally:
    os.close(fd)"#;
    let output = Command::new("python3")
        .args(["-c", harness, executable, profile, &columns.to_string()])
        .args(arguments)
        .env("XDG_STATE_HOME", state_dir(profile))
        .env_remove("QTOOLS_INTERNAL_UPDATE_CHECK")
        .output().unwrap();
    assert!(output.status.success(), "{}", String::from_utf8_lossy(&output.stderr));
    serde_json::from_slice(&output.stdout).unwrap()
}

#[test]
fn terminal_graphics_reuse_the_selected_protocol_locally_and_over_ssh() {
    let arguments = ["--memory", "--interval", "0", "--top", "1"];
    for profile in ["kitty_local", "kitty_remote"] {
        let result = terminal_report("pq", &arguments, profile, 80);
        assert_eq!(result["kitty"], true, "{profile}: {result}");
        assert_eq!(result["attributes"], 1, "{profile}: {result}");
        assert_eq!(result["width"], 40);
        assert_eq!(result["height"], 20);
    }
    for profile in ["iterm", "iterm_ssh"] {
        let result = terminal_report("pq", &arguments, profile, 80);
        assert_eq!(result["iterm"], true, "{profile}: {result}");
        assert_eq!(result["attributes"], 0, "{profile}: {result}");
    }
    let result = terminal_report("pq", &arguments, "sixel", 80);
    assert_eq!(result["sixel"], true, "{result}");
    assert_eq!(result["attributes"], 2, "{result}");
}

#[test]
fn terminal_graphics_fall_back_to_text_for_multiplexers_and_unresponsive_terminals() {
    let arguments = ["--memory", "--interval", "0", "--top", "1"];
    for profile in ["tmux", "screen", "term_tmux", "text", "text_ssh", "silent"] {
        let result = terminal_report("pq", &arguments, profile, 80);
        assert_eq!(result["kitty"], false, "{profile}: {result}");
        assert_eq!(result["iterm"], false, "{profile}: {result}");
        assert_eq!(result["sixel"], false, "{profile}: {result}");
        assert_eq!(result["bars"], true, "{profile}: {result}");
        if ["tmux", "screen", "term_tmux"].contains(&profile) {
            assert_eq!(result["attributes"], 0, "{profile}: {result}");
        }
    }
    let result = terminal_report("pq", &["--memory", "--interval", "0", "--top", "1", "--no-color"], "iterm", 80);
    assert_eq!(result["iterm"], false, "{result}");
    assert_eq!(result["attributes"], 0);
    assert_eq!(result["bars"], true);
}

#[test]
fn terminal_charts_fit_narrow_panes_in_process_ranked_and_disk_views() {
    for arguments in [vec!["--memory", "--interval", "0", "--top", "1"],
        vec!["--fds", "--interval", "0", "--top", "1"]] {
        let result = terminal_report("pq", &arguments, "kitty_remote", 9);
        assert_eq!(result["width"], 9, "{arguments:?}: {result}");
        assert_eq!(result["height"], 5);
    }
    let root = state_dir("narrow-dq");
    fs::write(root.join("sample"), b"chart data").unwrap();
    for arguments in [vec!["--apparent", "--files", root.to_str().unwrap()],
        vec!["--apparent", root.to_str().unwrap()]] {
        let result = terminal_report("dq", &arguments, "kitty_remote", 9);
        assert_eq!(result["width"], 9, "{arguments:?}: {result}");
        assert_eq!(result["height"], 5);
    }
    fs::remove_dir_all(root).unwrap();
}

#[test]
fn pq_kill_actions_are_exercised_safely_in_dry_run_mode() {
    let mut sleeper = Command::new("sleep").arg("30").spawn().unwrap();
    let process = run("pq", "kill-process", &["--kill", "sleep", "--dry-run", "--sigkill", "--yes", "--interval", "0"]);
    let _ = sleeper.kill();
    let _ = sleeper.wait();
    assert!(process.status.success(), "{}", String::from_utf8_lossy(&process.stderr));
    assert!(String::from_utf8_lossy(&process.stdout).contains("process(es)"));

    let script = "import socket,time\ns=socket.socket()\ns.bind(('127.0.0.1',0))\ns.listen()\nprint(s.getsockname()[1],flush=True)\ntime.sleep(30)";
    let mut listener = Command::new("python3")
        .args(["-c", script])
        .stdout(Stdio::piped())
        .spawn()
        .unwrap();
    let mut port = String::new();
    BufReader::new(listener.stdout.take().unwrap()).read_line(&mut port).unwrap();
    let io_owner = successful_json("pq", "io-port-owner", &["--io", "--port", port.trim(), "--json", "--interval", "0"]);
    assert!(io_owner.get("clusters").is_some());
    let diagnostic_owner = successful_json("pq", "blocked-port-owner", &["--blocked", "--port", port.trim(), "--json", "--interval", "0"]);
    assert!(diagnostic_owner.is_object());
    let filtered_net = successful_json("pq", "filtered-net", &["--net", "--min-cpu", "0", "--min-memory", "0",
        "--json", "--interval", "0"]);
    assert!(filtered_net.get("clusters").is_some());
    let selected_network = run("pq", "kill-net-filter", &["--net", "--listen", "--kill", "python3",
        "--dry-run", "--yes", "--interval", "0"]);
    assert!(selected_network.status.success());
    let network = run("pq", "kill-port", &["--port", port.trim(), "--kill", "--dry-run", "--yes", "--interval", "0"]);
    let _ = listener.kill();
    let _ = listener.wait();
    assert!(network.status.success(), "{}", String::from_utf8_lossy(&network.stderr));
    assert!(String::from_utf8_lossy(&network.stdout).contains("own port"));

    let absent = run("pq", "kill-absent-port", &["--port", "65534", "--kill", "--dry-run", "--yes", "--interval", "0"]);
    assert!(absent.status.success());
    assert!(String::from_utf8_lossy(&absent.stdout).contains("no sockets"));

    let mut sleeper = Command::new("sleep").arg("30").spawn().unwrap();
    let aborted = run_with_input("pq", "kill-abort", &["--kill", "sleep", "--interval", "0"], b"n\n");
    let _ = sleeper.kill();
    let _ = sleeper.wait();
    assert!(aborted.status.success());
    assert!(String::from_utf8_lossy(&aborted.stderr).contains("aborted"));

    let mut sleeper = Command::new("sleep").arg("30").spawn().unwrap();
    let invalid_signal = run("pq", "kill-invalid-signal", &["--kill", "sleep", "--signal", "NOPE", "--yes", "--interval", "0"]);
    let _ = sleeper.kill();
    let _ = sleeper.wait();
    assert!(!invalid_signal.status.success());
    assert!(String::from_utf8_lossy(&invalid_signal.stderr).contains("unknown signal"));

    let no_match = run("pq", "kill-no-match", &["--kill", "definitely-no-qtools-process", "--dry-run", "--interval", "0"]);
    assert!(no_match.status.success());
    assert!(String::from_utf8_lossy(&no_match.stdout).contains("no processes match"));
}

#[test]
fn pq_terminates_only_an_exact_test_owned_process() {
    use std::os::unix::fs::symlink;

    let root = std::env::temp_dir().join(format!("qtools-kill-fixture-{}", std::process::id()));
    let _ = fs::remove_dir_all(&root);
    fs::create_dir(&root).unwrap();
    let name = format!("qtools-kill-target-{}", std::process::id());
    let executable = root.join(&name);
    symlink("/bin/sleep", &executable).unwrap();
    let mut target = Command::new(&executable).arg("30").spawn().unwrap();
    let output = run("pq", "kill-real", &["--kill", &name, "--exact", "--yes", "--grace", "1", "--interval", "0"]);
    assert!(output.status.success(), "{}", String::from_utf8_lossy(&output.stderr));
    assert!(String::from_utf8_lossy(&output.stdout).contains("terminated 1"));
    let status = target.wait().unwrap();
    assert!(!status.success());
    fs::remove_dir_all(root).unwrap();
}

#[test]
fn dq_json_scan_and_validation_run_end_to_end() {
    let root = std::env::temp_dir().join(format!("qtools-dq-fixture-{}", std::process::id()));
    let _ = fs::remove_dir_all(&root);
    fs::create_dir_all(root.join("nested")).unwrap();
    fs::write(root.join("small.txt"), b"small").unwrap();
    fs::write(root.join("nested/larger.bin"), vec![0_u8; 4096]).unwrap();

    let root_text = root.to_str().unwrap();
    let value = successful_json("dq", "scan", &["--json", "--top", "2", root_text]);
    assert!(value.get("rows").is_some());
    assert!(value.get("filesystem_space").is_none(), "a subdirectory must not claim partition capacity");

    let invalid = run("dq", "invalid", &["--depth", "nope", root_text]);
    assert!(!invalid.status.success());
    assert!(!invalid.stderr.is_empty());

    let missing = run("dq", "missing-root", &["/definitely/missing/qtools-directory"]);
    assert!(!missing.status.success());
    assert!(String::from_utf8_lossy(&missing.stderr).contains("cannot access"));
    let file_root = run("dq", "file-root", &[root.join("small.txt").to_str().unwrap()]);
    assert!(!file_root.status.success());
    assert!(String::from_utf8_lossy(&file_root.stderr).contains("not a directory"));

    let unknown_help = run("dq", "unknown-help", &["help", "definitely-unknown"]);
    assert!(!unknown_help.status.success());
    assert!(String::from_utf8_lossy(&unknown_help.stderr).contains("unknown help topic"));
    fs::remove_dir_all(root).unwrap();
}

#[test]
fn dq_human_views_and_filters_run_end_to_end() {
    let root = std::env::temp_dir().join(format!("qtools-dq-human-{}", std::process::id()));
    let _ = fs::remove_dir_all(&root);
    fs::create_dir_all(root.join("nested")).unwrap();
    fs::write(root.join("old.txt"), vec![0_u8; 128]).unwrap();
    fs::write(root.join("nested/new.bin"), vec![0_u8; 2048]).unwrap();
    let root_text = root.to_str().unwrap();

    for (label, arguments) in [
        ("directories", vec!["--no-color", "--top", "2", root_text]),
        ("files", vec!["--files", "--no-color", "--top", "2", root_text]),
        ("owners", vec!["--owners", "--no-color", "--top", "2", root_text]),
        ("age", vec!["--age", "--zeroes", "--errors", "--no-color", "--top", "2", root_text]),
        ("apparent", vec!["--apparent", "--min-size", "1", "--no-color", root_text]),
    ] {
        let output = run("dq", label, &arguments);
        assert!(output.status.success(), "{label}: {}", String::from_utf8_lossy(&output.stderr));
        assert!(!output.stdout.is_empty(), "{label} produced no report");
        if label == "directories" {
            let text = String::from_utf8_lossy(&output.stdout);
            assert!(!text.contains("[free space"));
            assert!(!text.contains("Filesystem:"));
            assert!(text.contains("path"));
        }
    }
    let jsonl = run("dq", "jsonl", &["--files", "--jsonl", "--all", root_text]);
    assert!(jsonl.status.success());
    assert_eq!(String::from_utf8_lossy(&jsonl.stdout).lines().count(), 1);
    fs::remove_dir_all(root).unwrap();
}
