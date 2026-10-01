//! 启动器侧只读观察器，不属于 Python Job，不执行任何恢复或终止。

use std::{
    collections::{BTreeMap, BTreeSet},
    fs::{self, OpenOptions},
    io::Write,
    net::TcpStream,
    path::PathBuf,
    sync::{atomic::{AtomicBool, Ordering}, Arc, Mutex, OnceLock},
    thread,
    time::{Duration, Instant},
};

use chrono::{Local, Utc};
use serde_json::{json, Value};
use sysinfo::{ProcessRefreshKind, ProcessesToUpdate, System};

use crate::backend::{pids_using_tcp_port, ManagedBackend};

const SESSION_ENV: &str = "AZURPILOT_DIAGNOSTIC_SESSION";
static STARTED: OnceLock<Instant> = OnceLock::new();
static IDENTITY: OnceLock<(Option<u32>, Option<u64>)> = OnceLock::new();
static WRITE_LOCK: Mutex<()> = Mutex::new(());

#[cfg(windows)]
struct ExitHandle(winapi::um::winnt::HANDLE);

#[cfg(windows)]
impl ExitHandle {
    fn open(pid: u32) -> Option<Self> {
        let handle = unsafe { winapi::um::processthreadsapi::OpenProcess(
            winapi::um::winnt::PROCESS_QUERY_LIMITED_INFORMATION, 0, pid) };
        if handle.is_null() { None } else { Some(Self(handle)) }
    }

    fn exit_fields(&self) -> Value {
        let mut code = 259;
        let success = unsafe { winapi::um::processthreadsapi::GetExitCodeProcess(self.0, &mut code) };
        if success != 0 && code != 259 {
            json!({"exit_code": code, "exit_code_hex": format!("0x{code:08X}")})
        } else {
            json!({"exit_code": null})
        }
    }
}

#[cfg(windows)]
impl Drop for ExitHandle {
    fn drop(&mut self) {
        unsafe { winapi::um::handleapi::CloseHandle(self.0) };
    }
}

pub fn initialize_session() {
    let session = format!("{:032x}", rand::random::<u128>());
    std::env::set_var(SESSION_ENV, session);
    STARTED.get_or_init(Instant::now);
    record("launcher_start", json!({"version": env!("CARGO_PKG_VERSION")}));
}

pub fn record(event: &str, mut fields: Value) {
    let Ok(_lock) = WRITE_LOCK.try_lock() else { return };
    let Ok(session) = std::env::var(SESSION_ENV) else { return };
    fields["event"] = json!(event);
    fields["session_id"] = json!(session);
    fields["pid"] = json!(std::process::id());
    let identity = IDENTITY.get_or_init(|| {
        let system = System::new_all();
        system.process(sysinfo::Pid::from_u32(std::process::id()))
            .map(|process| (process.parent().map(|pid| pid.as_u32()), Some(process.start_time())))
            .unwrap_or((None, None))
    });
    fields["ppid"] = json!(identity.0);
    fields["created_at"] = json!(identity.1);
    fields["role"] = json!("launcher-observer");
    fields["local_time"] = json!(Local::now().to_rfc3339());
    fields["utc_time"] = json!(Utc::now().to_rfc3339());
    fields["monotonic"] = json!(STARTED.get_or_init(Instant::now).elapsed().as_secs_f64());
    let root = PathBuf::from("log/diagnostics");
    let _ = fs::create_dir_all(&root);
    if let Ok(mut file) = OpenOptions::new().create(true).append(true)
        .open(root.join(format!("{session}-launcher.jsonl"))) {
        // 不传入命令行、环境变量、HTTP响应正文或认证数据。
        if let Ok(mut bytes) = serde_json::to_vec(&fields) {
            bytes.push(b'\n');
            let _ = file.write_all(&bytes);
            let _ = file.flush();
        }
    }
}

pub fn exit_fields(status: std::process::ExitStatus) -> Value {
    json!({"exit_code": status.code(),
           "exit_code_hex": status.code().map(|code| format!("0x{:08X}", code as u32))})
}

pub fn cancel_child(child: &mut std::process::Child, caller: &str) {
    let pid = child.id();
    let system = System::new_all();
    let created_at = system.process(sysinfo::Pid::from_u32(pid)).map(|process| process.start_time());
    record("terminate_intent", json!({"trigger_reason": "setup_cancel", "caller": caller,
        "target_pid": pid, "target_created_at": created_at, "method": "kill"}));
    let result = child.kill();
    record("terminate_result", json!({"trigger_reason": "setup_cancel", "target_pid": pid,
        "signal_sent": result.is_ok(), "error": result.err().map(|error| error.to_string())}));
    match child.wait() {
        Ok(status) => {
            let mut fields = exit_fields(status);
            fields["target_pid"] = json!(pid);
            record("cancelled_child_exit", fields);
        },
        Err(error) => record("cancelled_child_wait_error", json!({"target_pid": pid, "error": error.to_string()})),
    }
}

pub fn start_observer(backend: Arc<Mutex<Option<ManagedBackend>>>, port: u16,
                      ssl: bool, allow_exit: Arc<AtomicBool>) {
    thread::spawn(move || {
        let client = match reqwest::blocking::Client::builder().no_proxy()
            .connect_timeout(Duration::from_secs(1)).timeout(Duration::from_secs(2)).build() {
            Ok(client) => client,
            Err(error) => {
                record("observer_failed", json!({"error": format!("{error:#}")}));
                return;
            }
        };
        let mut system = System::new();
        let mut known = BTreeMap::<u32, u64>::new();
        #[cfg(windows)]
        let mut handles = BTreeMap::<u32, ExitHandle>::new();
        let mut failures = 0;
        let mut captures = 0;
        while !allow_exit.load(Ordering::SeqCst) {
            let sample_at = Instant::now();
            // 慢网络、进程枚举与磁盘操作均不持有后端锁，不阻止用户退出。
            let root_pid = match backend.try_lock() {
                Ok(mut slot) => slot.as_mut().map(|child| {
                    child.observe_exit();
                    if child.exited() { None } else { Some(child.pid()) }
                }).flatten(),
                Err(_) => None,
            };
            system.refresh_processes_specifics(ProcessesToUpdate::All, true,
                                               ProcessRefreshKind::nothing().with_memory());
            let mut selected = BTreeSet::new();
            if let Some(pid) = root_pid { selected.insert(pid); }
            for (pid, created) in &known {
                if system.process(sysinfo::Pid::from_u32(*pid))
                    .is_some_and(|process| process.start_time() == *created) {
                    selected.insert(*pid);
                }
            }
            loop {
                let count = selected.len();
                for (pid, process) in system.processes() {
                    if process.parent().is_some_and(|parent| selected.contains(&parent.as_u32())) {
                        selected.insert(pid.as_u32());
                    }
                }
                if selected.len() == count { break; }
            }
            let mut current = BTreeMap::new();
            let mut processes = Vec::new();
            for pid in selected {
                if let Some(process) = system.process(sysinfo::Pid::from_u32(pid)) {
                    #[cfg(windows)]
                    if !handles.contains_key(&pid) {
                        if let Some(handle) = ExitHandle::open(pid) { handles.insert(pid, handle); }
                    }
                    current.insert(pid, process.start_time());
                    processes.push(json!({"pid": pid, "ppid": process.parent().map(|p| p.as_u32()),
                        "created_at": process.start_time(), "status": format!("{:?}", process.status()),
                        "memory_bytes": process.memory()}));
                }
            }
            for (pid, created) in &known {
                if current.get(pid) != Some(created) {
                    #[cfg(windows)]
                    let mut fields = handles.remove(pid).map(|handle| handle.exit_fields()).unwrap_or(json!({"exit_code": null}));
                    #[cfg(not(windows))]
                    let mut fields = json!({"exit_code": null});
                    fields["target_pid"] = json!(pid);
                    fields["target_created_at"] = json!(created);
                    record("process_disappeared", fields);
                }
            }
            known = current;
            let listeners = pids_using_tcp_port(port);
            let address = format!("127.0.0.1:{port}").parse().expect("loopback address");
            let tcp = TcpStream::connect_timeout(&address, Duration::from_secs(1));
            let scheme = if ssl { "https" } else { "http" };
            let health = probe_health(&client, &format!("{scheme}://127.0.0.1:{port}/healthz"), tcp.is_ok());
            record("heartbeat", json!({"root_pid": root_pid, "port": port, "processes": processes,
                "listener_pids": listeners.as_ref().ok(),
                "listener_error": listeners.err().map(|error| format!("{error:#}")),
                "tcp_connected": tcp.is_ok(), "tcp_error": tcp.err().map(|error| error.to_string()),
                "health": health}));
            if health["healthy"] == true { failures = 0; } else { failures += 1; }
            if failures == 3 && captures < 8 {
                captures += 1;
                if let Ok(session) = std::env::var(SESSION_ENV) {
                    let result = fs::write(format!("log/diagnostics/{session}.dump-request"), Utc::now().to_rfc3339());
                    record("stack_request", json!({"trigger_reason": "three_health_failures", "success": result.is_ok()}));
                }
            }
            while sample_at.elapsed() < Duration::from_secs(5) && !allow_exit.load(Ordering::SeqCst) {
                thread::sleep(Duration::from_millis(100));
            }
        }
        record("observer_stop", json!({"trigger_reason": "launcher_exit"}));
    });
}

fn probe_health(client: &reqwest::blocking::Client, url: &str, tcp_connected: bool) -> Value {
    if !tcp_connected { return json!({"healthy": false, "stage": "tcp"}); }
    match client.get(url).send() {
        Ok(response) => json!({"healthy": response.status().is_success(), "stage": "http", "status": response.status().as_u16()}),
        Err(error) => json!({"healthy": false, "stage": "http", "timeout": error.is_timeout(), "error": format!("{error:#}")}),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn disconnected_tcp_does_not_send_http() {
        let client = reqwest::blocking::Client::new();
        assert_eq!(probe_health(&client, "invalid-url", false)["stage"], "tcp");
    }

    #[test]
    fn health_checks_http_status() {
        use std::io::Read;
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let address = listener.local_addr().unwrap();
        let server = thread::spawn(move || {
            let (mut socket, _) = listener.accept().unwrap();
            let mut request = [0; 1024];
            let _ = socket.read(&mut request).unwrap();
            socket.write_all(b"HTTP/1.1 503 Service Unavailable\r\nContent-Length: 0\r\nConnection: close\r\n\r\n").unwrap();
        });
        let client = reqwest::blocking::Client::builder().no_proxy().build().unwrap();
        let result = probe_health(&client, &format!("http://{address}/healthz"), true);
        assert_eq!(result["status"], 503);
        assert_eq!(result["healthy"], false);
        server.join().unwrap();
    }

    #[test]
    fn http_timeout_is_distinct_from_tcp_failure() {
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let address = listener.local_addr().unwrap();
        let server = thread::spawn(move || {
            let (_socket, _) = listener.accept().unwrap();
            thread::sleep(Duration::from_millis(250));
        });
        let client = reqwest::blocking::Client::builder().no_proxy()
            .timeout(Duration::from_millis(50)).build().unwrap();
        let result = probe_health(&client, &format!("http://{address}/healthz"), true);
        assert_eq!(result["stage"], "http");
        assert_eq!(result["timeout"], true);
        server.join().unwrap();
    }

    #[cfg(windows)]
    #[test]
    fn retained_handle_records_exit_after_process_disappears() {
        use std::process::Command;
        use crate::window_util::CreateNoWindow as _;
        let mut child = Command::new("cmd").args(["/c", "ping -n 2 127.0.0.1 >nul & exit /b 53"])
            .create_no_window().spawn().unwrap();
        let handle = ExitHandle::open(child.id()).unwrap();
        child.wait().unwrap();
        assert_eq!(handle.exit_fields()["exit_code"], 53);
    }

    #[cfg(windows)]
    #[test]
    fn setup_cancellation_still_reaps_the_child() {
        use std::process::Command;
        use crate::window_util::CreateNoWindow as _;
        let mut child = Command::new("cmd").args(["/c", "pause >nul"])
            .stdin(std::process::Stdio::piped()).create_no_window().spawn().unwrap();
        cancel_child(&mut child, "test");
        assert!(child.try_wait().unwrap().is_some());
    }
}
