use std::fs::{self, File};
use std::io::{BufRead, BufReader, Read, Write};
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{self, Sender};
use std::sync::OnceLock;
use std::thread;
use std::time::Duration;

use sha2::{Digest, Sha256};

use crate::MainWindow;

const MAX_RESTARTS: u32 = 3;
const MIN_BINARY_BYTES: u64 = 1_000_000;
static UI: OnceLock<slint::Weak<MainWindow>> = OnceLock::new();

#[derive(Clone, Copy, PartialEq, Eq)]
enum Phase {
    Idle,
    Downloading,
    Running,
    Stopped,
    Error,
}

enum Order {
    Start(u16),
    Stop,
}

struct ReleaseAsset {
    name: String,
    url: String,
    digest: Option<String>,
}

pub fn attach(ui: slint::Weak<MainWindow>) {
    let _ = UI.set(ui.clone());
    let tx = supervisor();
    let start_ui = ui.clone();
    ui.upgrade().expect("window").on_start_playit(move || {
        let Some(ui) = start_ui.upgrade() else { return };
        let port = ui.get_server_port().to_string();
        let port = port.trim().parse::<u16>().unwrap_or(25565);
        let _ = tx.send(Order::Start(port));
    });
    let tx = supervisor();
    ui.upgrade().expect("window").on_stop_playit(move || {
        let _ = tx.send(Order::Stop);
    });
    let open_ui = ui.clone();
    ui.upgrade().expect("window").on_open_playit_claim(move || {
        let Some(ui) = open_ui.upgrade() else { return };
        open_url(&ui.get_playit_claim());
    });
}

fn supervisor() -> Sender<Order> {
    static TX: OnceLock<Sender<Order>> = OnceLock::new();
    TX.get_or_init(|| {
        let (tx, rx) = mpsc::channel::<Order>();
        thread::Builder::new()
            .name("playit".into())
            .spawn(move || run_supervisor(rx))
            .expect("playit thread");
        tx
    })
    .clone()
}

fn run_supervisor(rx: mpsc::Receiver<Order>) {
    let mut child: Option<Child> = None;
    let mut wanted = false;
    let mut restarts = 0u32;
    let mut last_port = 25565u16;
    let stop_readers = AtomicBool::new(false);
    set_phase(Phase::Idle, "");
    loop {
        let order = if wanted {
            rx.recv_timeout(Duration::from_millis(400))
        } else {
            rx.recv().map_err(|_| mpsc::RecvTimeoutError::Disconnected)
        };
        match order {
            Ok(Order::Stop) | Err(mpsc::RecvTimeoutError::Disconnected) => {
                wanted = false;
                restarts = 0;
                stop_readers.store(true, Ordering::Relaxed);
                stop_child(child.take());
                set_phase(Phase::Stopped, "");
                if order.is_err() {
                    break;
                }
            }
            Ok(Order::Start(port)) => {
                wanted = true;
                restarts = 0;
                last_port = port;
                stop_readers.store(true, Ordering::Relaxed);
                stop_child(child.take());
                stop_readers.store(false, Ordering::Relaxed);
                match ensure_binary() {
                    Ok(bin) => match spawn_agent(&bin, port, &stop_readers) {
                        Ok(proc) => {
                            child = Some(proc);
                            set_phase(Phase::Running, "");
                        }
                        Err(err) => {
                            wanted = false;
                            set_phase(Phase::Error, &err);
                        }
                    },
                    Err(err) => {
                        wanted = false;
                        set_phase(Phase::Error, &err);
                    }
                }
            }
            Err(mpsc::RecvTimeoutError::Timeout) => {
                if let Some(proc) = child.as_mut() {
                    if let Ok(Some(status)) = proc.try_wait() {
                        let code = status.code().unwrap_or(-1);
                        child = None;
                        if !wanted {
                            set_phase(Phase::Stopped, "");
                            continue;
                        }
                        if restarts >= MAX_RESTARTS {
                            wanted = false;
                            set_phase(Phase::Error, &format!("playit kapandı ({code})"));
                            continue;
                        }
                        restarts += 1;
                        let wait = Duration::from_secs(1u64 << (restarts - 1));
                        set_phase(Phase::Error, &format!("yeniden deneniyor {restarts}/{MAX_RESTARTS}"));
                        thread::sleep(wait);
                        if let Ok(bin) = ensure_binary() {
                            if let Ok(proc) = spawn_agent(&bin, last_port, &stop_readers) {
                                child = Some(proc);
                                set_phase(Phase::Running, "");
                            }
                        }
                    }
                }
            }
        }
    }
}

fn spawn_agent(bin: &Path, port: u16, stop_readers: &AtomicBool) -> Result<Child, String> {
    let secret = secret_path()?;
    let mut cmd = Command::new(bin);
    cmd.arg(format!("--secret_path={}", secret.display()))
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    hide_window(&mut cmd);
    #[cfg(unix)]
    {
        use std::os::unix::process::CommandExt;
        cmd.process_group(0);
    }
    let mut child = cmd.spawn().map_err(|e| format!("playit başlatılamadı: {e}"))?;
    assign_kill_on_close(&child);
    let hint = if ui_is_tr() {
        format!("Tünel hedefi 127.0.0.1:{port}. Playit sitesinde TCP tünelini bu porta ayarla.")
    } else {
        format!("Tunnel target is 127.0.0.1:{port}. Set the TCP tunnel to that port on playit.gg.")
    };
    set_phase(Phase::Running, &hint);
    if let Some(out) = child.stdout.take() {
        pump_pipe(out, stop_readers);
    }
    if let Some(err) = child.stderr.take() {
        pump_pipe(err, stop_readers);
    }
    let _ = port;
    Ok(child)
}

fn pump_pipe(pipe: impl Read + Send + 'static, _stop: &AtomicBool) {
    thread::spawn(move || {
        let reader = BufReader::new(pipe);
        for line in reader.lines().map_while(Result::ok) {
            inspect_line(&line);
        }
    });
}

fn inspect_line(line: &str) {
    if let Some(url) = capture_claim(line) {
        slint::invoke_from_event_loop(move || {
            if let Some(ui) = UI.get().and_then(|w| w.upgrade()) {
                ui.set_playit_claim(url.into());
            }
        })
        .ok();
    }
    if let Some(addr) = capture_address(line) {
        slint::invoke_from_event_loop(move || {
            if let Some(ui) = UI.get().and_then(|w| w.upgrade()) {
                ui.set_playit_address(addr.into());
            }
        })
        .ok();
    }
}

fn capture_claim(line: &str) -> Option<String> {
    let marker = "https://playit.gg/claim/";
    let start = line.find(marker)?;
    let rest = &line[start..];
    let end = rest.find(char::is_whitespace).unwrap_or(rest.len());
    let url = rest[..end].trim_matches(|c: char| c == '"' || c == '\'' || c == ')' || c == ']');
    if url.len() > marker.len() { Some(url.to_string()) } else { None }
}

fn capture_address(line: &str) -> Option<String> {
    let lower = line.to_ascii_lowercase();
    let idx = lower.find(".ply.gg").or_else(|| lower.find(".playit.gg"))?;
    let bytes = lower.as_bytes();
    let mut begin = idx;
    while begin > 0 && is_host_char(bytes[begin - 1] as char) {
        begin -= 1;
    }
    let mut end = idx;
    while end < lower.len() && is_host_char(lower[end..].chars().next().unwrap_or(' ')) {
        end += lower[end..].chars().next().unwrap().len_utf8();
    }
    let mut host = lower[begin..end].trim_matches('.').to_string();
    if host.is_empty() {
        return None;
    }
    let after = line.get(end..).unwrap_or("");
    if let Some(rest) = after.strip_prefix(':') {
        let port: String = rest.chars().take_while(|c| c.is_ascii_digit()).collect();
        if (2..=5).contains(&port.len()) {
            host.push(':');
            host.push_str(&port);
        }
    }
    Some(host)
}

fn is_host_char(c: char) -> bool {
    c.is_ascii_alphanumeric() || c == '.' || c == '-'
}

fn ensure_binary() -> Result<PathBuf, String> {
    let dest = binary_path()?;
    if dest.is_file() && fs::metadata(&dest).map(|m| m.len()).unwrap_or(0) >= MIN_BINARY_BYTES {
        make_executable(&dest)?;
        return Ok(dest);
    }
    let asset = pick_asset()?;
    set_phase(Phase::Downloading, &asset.name);
    download_asset(&asset, &dest)?;
    make_executable(&dest)?;
    Ok(dest)
}

fn pick_asset() -> Result<ReleaseAsset, String> {
    let client = client()?;
    let response = client
        .get("https://api.github.com/repos/playit-cloud/playit-agent/releases/latest")
        .header("Accept", "application/vnd.github+json")
        .header("User-Agent", concat!("PanelMC/", env!("CARGO_PKG_VERSION")))
        .send()
        .map_err(|e| e.to_string())?;
    if !response.status().is_success() {
        return Err(format!("playit listesi alınamadı (HTTP {})", response.status()));
    }
    let value: serde_json::Value = response.json().map_err(|e| e.to_string())?;
    let want = wanted_asset_name();
    let assets = value["assets"].as_array().ok_or("playit release boş")?;
    let asset = assets.iter().find(|a| a["name"].as_str() == Some(want)).ok_or_else(|| format!("{want} bulunamadı"))?;
    Ok(ReleaseAsset {
        name: want.to_string(),
        url: asset["browser_download_url"].as_str().unwrap_or("").to_string(),
        digest: asset["digest"].as_str().map(|s| s.to_string()),
    })
}

fn wanted_asset_name() -> &'static str {
    if cfg!(windows) {
        "playit-windows-x86_64-signed.exe"
    } else if cfg!(target_arch = "aarch64") {
        "playit-linux-aarch64"
    } else {
        "playit-linux-amd64"
    }
}

fn download_asset(asset: &ReleaseAsset, dest: &Path) -> Result<(), String> {
    if asset.url.is_empty() {
        return Err("indirme adresi boş".into());
    }
    let client = client()?;
    let mut response = client
        .get(&asset.url)
        .header("User-Agent", concat!("PanelMC/", env!("CARGO_PKG_VERSION")))
        .send()
        .map_err(|e| e.to_string())?;
    if !response.status().is_success() {
        return Err(format!("playit indirilemedi (HTTP {})", response.status()));
    }
    let total = response.content_length().unwrap_or(0);
    if let Some(parent) = dest.parent() {
        fs::create_dir_all(parent).map_err(|e| e.to_string())?;
    }
    let partial = dest.with_extension("partial");
    let mut file = File::create(&partial).map_err(|e| e.to_string())?;
    let mut hasher = Sha256::new();
    let mut got = 0u64;
    let mut buf = [0u8; 64 * 1024];
    loop {
        let n = response.read(&mut buf).map_err(|e| e.to_string())?;
        if n == 0 {
            break;
        }
        file.write_all(&buf[..n]).map_err(|e| e.to_string())?;
        hasher.update(&buf[..n]);
        got += n as u64;
        if let Some(pct) = got.saturating_mul(100).checked_div(total) {
            set_phase(Phase::Downloading, &format!("{}%", pct.min(100)));
        }
    }
    drop(file);
    if got < MIN_BINARY_BYTES {
        let _ = fs::remove_file(&partial);
        return Err("playit dosyası eksik indi".into());
    }
    if let Some(digest) = asset.digest.as_deref().and_then(|d| d.strip_prefix("sha256:")) {
        let actual = hex_encode(&hasher.finalize());
        if !actual.eq_ignore_ascii_case(digest) {
            let _ = fs::remove_file(&partial);
            return Err("playit sağlama toplamı uyuşmadı".into());
        }
    }
    if dest.exists() {
        let _ = fs::remove_file(dest);
    }
    fs::rename(&partial, dest).map_err(|e| e.to_string())?;
    Ok(())
}

fn hex_encode(bytes: &[u8]) -> String {
    const HEX: &[u8; 16] = b"0123456789abcdef";
    let mut out = String::with_capacity(bytes.len() * 2);
    for byte in bytes {
        out.push(HEX[(byte >> 4) as usize] as char);
        out.push(HEX[(byte & 0xf) as usize] as char);
    }
    out
}

fn binary_path() -> Result<PathBuf, String> {
    let dir = playit_dir()?;
    let name = if cfg!(windows) { "playit.exe" } else { "playit" };
    Ok(dir.join(name))
}

fn secret_path() -> Result<PathBuf, String> {
    Ok(playit_dir()?.join("playit.toml"))
}

fn playit_dir() -> Result<PathBuf, String> {
    let dir = PathBuf::from("playit");
    fs::create_dir_all(&dir).map_err(|e| e.to_string())?;
    Ok(dir)
}

fn make_executable(path: &Path) -> Result<(), String> {
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let meta = fs::metadata(path).map_err(|e| e.to_string())?;
        let mut perms = meta.permissions();
        perms.set_mode(perms.mode() | 0o755);
        fs::set_permissions(path, perms).map_err(|e| e.to_string())?;
    }
    let _ = path;
    Ok(())
}

fn ui_is_tr() -> bool {
    UI.get().and_then(|w| w.upgrade()).map(|ui| ui.get_app_lang() == "tr").unwrap_or(true)
}

fn client() -> Result<reqwest::blocking::Client, String> {
    reqwest::blocking::Client::builder()
        .user_agent(concat!("PanelMC/", env!("CARGO_PKG_VERSION")))
        .timeout(Duration::from_secs(180))
        .build()
        .map_err(|e| e.to_string())
}

fn set_phase(phase: Phase, detail: &str) {
    let detail = detail.to_string();
    slint::invoke_from_event_loop(move || {
        let Some(ui) = UI.get().and_then(|w| w.upgrade()) else { return };
        let tr = ui.get_app_lang() == "tr";
        let label = match phase {
            Phase::Idle => if tr { "Hazır" } else { "Idle" },
            Phase::Downloading => if tr { "İndiriliyor" } else { "Downloading" },
            Phase::Running => if tr { "Çalışıyor" } else { "Running" },
            Phase::Stopped => if tr { "Durdu" } else { "Stopped" },
            Phase::Error => if tr { "Hata" } else { "Error" },
        };
        let text = if detail.is_empty() { label.to_string() } else { format!("{label}: {detail}") };
        ui.set_playit_status(text.into());
        ui.set_playit_running(phase == Phase::Running || phase == Phase::Downloading);
    })
    .ok();
}

fn stop_child(child: Option<Child>) {
    let Some(mut child) = child else { return };
    let pid = child.id();
    #[cfg(windows)]
    {
        let mut cmd = Command::new("taskkill");
        cmd.args(["/PID", &pid.to_string(), "/T", "/F"]);
        hide_window(&mut cmd);
        let _ = cmd.output();
    }
    #[cfg(unix)]
    {
        let _ = Command::new("kill").args(["-TERM", &format!("-{pid}")]).output();
        thread::sleep(Duration::from_millis(300));
        let _ = Command::new("kill").args(["-KILL", &format!("-{pid}")]).output();
    }
    let _ = child.kill();
    let _ = child.wait();
}

fn hide_window(cmd: &mut Command) {
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        cmd.creation_flags(0x0800_0000);
    }
    let _ = cmd;
}

fn open_url(url: &str) {
    if !url.starts_with("https://playit.gg/") { return; }
    #[cfg(windows)]
    {
        let mut cmd = Command::new("cmd");
        cmd.args(["/C", "start", "", url]);
        hide_window(&mut cmd);
        let _ = cmd.spawn();
    }
    #[cfg(target_os = "linux")]
    {
        if Command::new("xdg-open").arg(url).spawn().is_err() {
            let _ = Command::new("gio").args(["open", url]).spawn();
        }
    }
    #[cfg(target_os = "macos")]
    {
        let _ = Command::new("open").arg(url).spawn();
    }
}

fn assign_kill_on_close(child: &Child) {
    #[cfg(windows)]
    {
        jobkill::assign(child);
    }
    let _ = child;
}

#[cfg(windows)]
mod jobkill {
    use std::ffi::c_void;
    use std::os::windows::io::AsRawHandle;
    use std::process::Child;
    use std::sync::OnceLock;

    type Handle = *mut c_void;
    const JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE: u32 = 0x2000;
    const JOB_OBJECT_EXTENDED_LIMIT_INFORMATION: i32 = 9;
    const PROCESS_SET_QUOTA: u32 = 0x0100;
    const PROCESS_TERMINATE: u32 = 0x0001;

    #[repr(C)]
    struct BasicLimit {
        per_process_user_time_limit: i64,
        per_job_user_time_limit: i64,
        limit_flags: u32,
        minimum_working_set_size: usize,
        maximum_working_set_size: usize,
        active_process_limit: u32,
        affinity: usize,
        priority_class: u32,
        scheduling_class: u32,
    }
    #[repr(C)]
    struct ExtendedLimit {
        basic: BasicLimit,
        io: [u64; 6],
        process_memory_limit: usize,
        job_memory_limit: usize,
        peak_process_memory_used: usize,
        peak_job_memory_used: usize,
    }

    #[link(name = "kernel32")]
    extern "system" {
        fn CreateJobObjectW(attr: *mut c_void, name: *const u16) -> Handle;
        fn SetInformationJobObject(job: Handle, class: i32, info: *const c_void, len: u32) -> i32;
        fn AssignProcessToJobObject(job: Handle, process: Handle) -> i32;
        fn OpenProcess(access: u32, inherit: i32, pid: u32) -> Handle;
        fn CloseHandle(handle: Handle) -> i32;
    }

    fn job() -> Handle {
        static JOB: OnceLock<usize> = OnceLock::new();
        *JOB.get_or_init(|| {
            unsafe {
                let handle = CreateJobObjectW(std::ptr::null_mut(), std::ptr::null());
                if handle.is_null() {
                    return 0;
                }
                let mut info = ExtendedLimit {
                    basic: BasicLimit {
                        per_process_user_time_limit: 0,
                        per_job_user_time_limit: 0,
                        limit_flags: JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE,
                        minimum_working_set_size: 0,
                        maximum_working_set_size: 0,
                        active_process_limit: 0,
                        affinity: 0,
                        priority_class: 0,
                        scheduling_class: 0,
                    },
                    io: [0; 6],
                    process_memory_limit: 0,
                    job_memory_limit: 0,
                    peak_process_memory_used: 0,
                    peak_job_memory_used: 0,
                };
                SetInformationJobObject(
                    handle,
                    JOB_OBJECT_EXTENDED_LIMIT_INFORMATION,
                    &mut info as *mut _ as *const c_void,
                    std::mem::size_of::<ExtendedLimit>() as u32,
                );
                handle as usize
            }
        }) as Handle
    }

    pub fn assign(child: &Child) {
        unsafe {
            let job = job();
            if job.is_null() {
                return;
            }
            let process = OpenProcess(PROCESS_SET_QUOTA | PROCESS_TERMINATE, 0, child.id());
            if process.is_null() {
                let raw = child.as_raw_handle() as Handle;
                AssignProcessToJobObject(job, raw);
                return;
            }
            AssignProcessToJobObject(job, process);
            CloseHandle(process);
        }
    }
}
