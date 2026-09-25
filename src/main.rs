#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

slint::include_modules!();

mod cloud;
mod playit;

use std::collections::{HashMap, HashSet, VecDeque};
use std::fs::{self, File};
use std::io::{Read, Write};
use std::net::{SocketAddr, TcpStream};
use std::path::{Path, PathBuf};
use std::process::{Child, ChildStdin, Command, Stdio};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex, OnceLock};
use std::thread;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};
use zip::write::FileOptions;
use zip::CompressionMethod;
use zip::ZipWriter;

#[derive(serde::Serialize, serde::Deserialize, Clone)]
struct ServerMetadata {
    id: String,
    name: String,
    software: String,
    version: String,
    port: String,
    ram: String,
    #[serde(default = "default_java_ver")]
    selected_java: String,
    #[serde(default = "default_timezone")]
    timezone: String,
    #[serde(default)]
    join_host: String,
    #[serde(default)]
    backup_every_min: u32,
}

fn default_java_ver() -> String { "java25".to_string() }
fn default_timezone() -> String { "Europe/Istanbul".to_string() }

#[derive(serde::Deserialize)]
struct ModrinthSearchResponse { hits: Vec<ModrinthHit> }
#[derive(serde::Deserialize)]
struct ModrinthHit {
    slug: String,
    title: String,
    description: String,
    downloads: u64,
    icon_url: Option<String>,
}
#[derive(serde::Deserialize)]
struct ModrinthVersion { files: Vec<ModrinthFile> }
#[derive(serde::Deserialize)]
struct ModrinthFile { url: String, filename: String }
#[derive(serde::Deserialize)]
struct SpigetResource {
    id: u64,
    name: String,
    tag: Option<String>,
    downloads: Option<u64>,
    #[serde(default)]
    icon: Option<SpigetIcon>,
}
#[derive(serde::Deserialize)]
struct SpigetIcon {
    url: Option<String>,
}
#[derive(serde::Deserialize)]
struct PurpurVersionsResponse { versions: Vec<String> }
#[derive(serde::Deserialize)]
struct MojangManifest { versions: Vec<MojangManifestEntry> }
#[derive(serde::Deserialize)]
struct MojangManifestEntry {
    id: String,
    #[serde(rename = "type")]
    kind: String,
    url: String,
}
#[derive(serde::Deserialize)]
struct MojangVersionMeta { downloads: MojangDownloads }
#[derive(serde::Deserialize)]
struct MojangDownloads { server: Option<MojangDownload> }
#[derive(serde::Deserialize)]
struct MojangDownload { url: String }
#[derive(serde::Deserialize)]
struct FabricEntry {
    version: String,
    stable: bool,
}
#[derive(serde::Deserialize)]
struct FillProject { versions: serde_json::Map<String, serde_json::Value> }
#[derive(serde::Deserialize)]
struct FillLatestBuild { downloads: FillDownloads }
#[derive(serde::Deserialize)]
struct FillDownloads {
    #[serde(rename = "server:default")]
    server: Option<FillFile>,
}
#[derive(serde::Deserialize)]
struct FillFile { url: String }
#[derive(serde::Deserialize)]
struct GithubRelease { tag_name: String, html_url: String }

struct ServerProcess {
    child: Option<Child>,
    stdin: Option<ChildStdin>,
    logs: VecDeque<String>,
    log_text: String,
}

impl ServerProcess {
    fn new() -> Self {
        Self {
            child: None,
            stdin: None,
            logs: VecDeque::with_capacity(300),
            log_text: String::with_capacity(24 * 1024),
        }
    }

    fn append_log(&mut self, line: String) {
        if self.logs.len() >= 300 {
            if let Some(old) = self.logs.pop_front() {
                let skip = old.len() + 1;
                if self.log_text.len() >= skip && self.log_text.as_bytes().get(skip - 1) == Some(&b'\n') {
                    self.log_text.drain(..skip);
                } else {
                    self.log_text.clear();
                    for l in &self.logs {
                        self.log_text.push_str(l);
                        self.log_text.push('\n');
                    }
                }
            }
        }
        self.log_text.push_str(&line);
        self.log_text.push('\n');
        self.logs.push_back(line);
    }
}

fn hide_window(cmd: &mut Command) {
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        const CREATE_NO_WINDOW: u32 = 0x0800_0000;
        cmd.creation_flags(CREATE_NO_WINDOW);
    }
}

fn java_bin() -> &'static str {
    if cfg!(windows) { "java.exe" } else { "java" }
}

fn ensure_executable(path: &Path) {
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        if let Ok(meta) = fs::metadata(path) {
            let mut perms = meta.permissions();
            perms.set_mode(perms.mode() | 0o111);
            let _ = fs::set_permissions(path, perms);
        }
    }
    #[cfg(not(unix))]
    {
        let _ = path;
    }
}

fn adoptium_os_arch() -> Result<(&'static str, &'static str), String> {
    let os = match std::env::consts::OS {
        "windows" => "windows",
        "linux" => "linux",
        "macos" => "mac",
        other => return Err(format!("Bu işletim sistemi henüz desteklenmiyor: {other}")),
    };
    let arch = match std::env::consts::ARCH {
        "x86_64" => "x64",
        "aarch64" => "aarch64",
        "x86" => "x86",
        other => return Err(format!("Bu CPU mimarisi henüz desteklenmiyor: {other}")),
    };
    Ok((os, arch))
}

fn open_path(path: &Path) {
    #[cfg(target_os = "windows")]
    { let _ = Command::new("explorer").arg(path).spawn(); }
    #[cfg(target_os = "linux")]
    {
        if Command::new("xdg-open").arg(path).spawn().is_err() {
            let _ = Command::new("gio").args(["open"]).arg(path).spawn();
        }
    }
    #[cfg(target_os = "macos")]
    { let _ = Command::new("open").arg(path).spawn(); }
}

#[cfg(not(target_os = "windows"))]
fn unix_pick(args_zenity: &[&str], args_kdialog: &[&str]) -> Option<String> {
    for (bin, args) in [("zenity", args_zenity), ("kdialog", args_kdialog)] {
        if let Ok(output) = Command::new(bin).args(args).output() {
            if output.status.success() {
                let s = String::from_utf8_lossy(&output.stdout).trim().to_string();
                if !s.is_empty() {
                    return Some(s);
                }
            }
        }
    }
    None
}

fn http_client() -> &'static reqwest::blocking::Client {
    static CLIENT: OnceLock<reqwest::blocking::Client> = OnceLock::new();
    CLIENT.get_or_init(|| {
        reqwest::blocking::Client::builder()
            .user_agent(concat!("PanelMC/", env!("CARGO_PKG_VERSION")))
            .timeout(Duration::from_secs(30))
            .connect_timeout(Duration::from_secs(8))
            .pool_max_idle_per_host(2)
            .build()
            .expect("http client")
    })
}

fn http_client_long() -> reqwest::blocking::Client {
    reqwest::blocking::Client::builder()
        .user_agent(concat!("PanelMC/", env!("CARGO_PKG_VERSION")))
        .timeout(Duration::from_secs(600))
        .connect_timeout(Duration::from_secs(15))
        .pool_max_idle_per_host(1)
        .build()
        .expect("http client")
}

fn detect_system_dark_theme() -> bool {
    #[cfg(target_os = "windows")]
    {
        if let Ok(output) = {
            let mut cmd = Command::new("reg");
            cmd.args(["query", r"HKCU\Software\Microsoft\Windows\CurrentVersion\Themes\Personalize", "/v", "AppsUseLightTheme"]);
            hide_window(&mut cmd);
            cmd.output()
        }
        {
            let text = String::from_utf8_lossy(&output.stdout);
            for line in text.lines() {
                if line.contains("AppsUseLightTheme") {
                    return !line.contains("0x1");
                }
            }
        }
    }
    #[cfg(target_os = "linux")]
    {
        if let Ok(output) = Command::new("gsettings")
            .args(["get", "org.gnome.desktop.interface", "color-scheme"])
            .output()
        {
            let text = String::from_utf8_lossy(&output.stdout).to_lowercase();
            if text.contains("default") {
                return false;
            }
            if text.contains("dark") {
                return true;
            }
        }
    }
    true
}

fn detect_system_language() -> &'static str {
    #[cfg(target_os = "windows")]
    {
        if let Ok(output) = {
            let mut cmd = Command::new("reg");
            cmd.args(["query", r"HKCU\Control Panel\International", "/v", "LocaleName"]);
            hide_window(&mut cmd);
            cmd.output()
        }
        {
            let text = String::from_utf8_lossy(&output.stdout);
            for line in text.lines() {
                if line.contains("LocaleName") && line.to_lowercase().contains("tr") {
                    return "tr";
                }
            }
        }
    }
    #[cfg(unix)]
    {
        let loc = std::env::var("LC_ALL")
            .or_else(|_| std::env::var("LANG"))
            .unwrap_or_default()
            .to_lowercase();
        if loc.starts_with("tr") {
            return "tr";
        }
    }
    "en"
}

fn find_java_in_dir(dir: &Path) -> Option<PathBuf> {
    let name = java_bin();
    let direct = dir.join("bin").join(name);
    if direct.is_file() {
        ensure_executable(&direct);
        return Some(direct);
    }
    if let Ok(entries) = fs::read_dir(dir) {
        for entry in entries.flatten() {
            let path = entry.path();
            if path.is_dir() {
                let nested = path.join("bin").join(name);
                if nested.is_file() {
                    ensure_executable(&nested);
                    return Some(nested);
                }
            }
        }
    }
    None
}

fn get_executable_for_selected_java(preferred: &str) -> Option<PathBuf> {
    let p = Path::new("runtimes").join(preferred);
    if p.exists() {
        if let Some(exe) = find_java_in_dir(&p) {
            return Some(exe);
        }
    }

    for v in ["java25", "java21", "java17"] {
        let p = Path::new("runtimes").join(v);
        if p.exists() {
            if let Some(exe) = find_java_in_dir(&p) {
                return Some(exe);
            }
        }
    }

    if Command::new("java").arg("-version").output().is_ok() {
        return Some(PathBuf::from("java"));
    }

    None
}

fn extract_zip_archive(archive_path: &Path, target_dir: &Path) -> Result<(), Box<dyn std::error::Error>> {
    let file = File::open(archive_path)?;
    let mut archive = zip::ZipArchive::new(file)?;
    for i in 0..archive.len() {
        let mut file = archive.by_index(i)?;
        let outpath = match file.enclosed_name() {
            Some(path) => target_dir.join(path),
            None => continue,
        };

        if file.name().ends_with('/') {
            fs::create_dir_all(&outpath)?;
        } else {
            if let Some(p) = outpath.parent() {
                if !p.exists() { fs::create_dir_all(p)?; }
            }
            let mut outfile = File::create(&outpath)?;
            std::io::copy(&mut file, &mut outfile)?;
        }
    }
    Ok(())
}

fn extract_jdk_archive(archive_path: &Path, target_dir: &Path) -> Result<(), String> {
    fs::create_dir_all(target_dir).map_err(|e| e.to_string())?;
    #[cfg(windows)]
    {
        extract_zip_archive(archive_path, target_dir).map_err(|e| e.to_string())
    }
    #[cfg(unix)]
    {
        let status = Command::new("tar")
            .arg("-xzf")
            .arg(archive_path)
            .arg("-C")
            .arg(target_dir)
            .status()
            .map_err(|e| format!("tar yok veya çalışmadı: {e}"))?;
        if status.success() {
            Ok(())
        } else {
            Err("JDK arşivi çıkarılamadı.".into())
        }
    }
}
fn get_servers_config_path() -> PathBuf { Path::new("servers").join("servers.json") }
fn get_server_dir(server_id: &str) -> PathBuf { Path::new("servers").join(server_id) }

fn load_servers_list() -> Vec<ServerMetadata> {
    let path = get_servers_config_path();
    if let Ok(content) = fs::read_to_string(path) {
        if let Ok(list) = serde_json::from_str::<Vec<ServerMetadata>>(&content) {
            return list;
        }
    }
    Vec::new()
}

fn save_servers_list(list: &[ServerMetadata]) {
    let _ = fs::create_dir_all("servers");
    if let Ok(json) = serde_json::to_string_pretty(list) {
        let _ = fs::write(get_servers_config_path(), json);
    }
}

fn read_server_properties_of(server_id: &str) -> HashMap<String, String> {
    let mut map = HashMap::new();
    let prop_path = get_server_dir(server_id).join("server.properties");
    if let Ok(content) = fs::read_to_string(prop_path) {
        for line in content.lines() {
            let trimmed = line.trim();
            if trimmed.starts_with('#') || trimmed.is_empty() { continue; }
            if let Some((k, v)) = trimmed.split_once('=') {
                map.insert(k.trim().to_string(), v.trim().to_string());
            }
        }
    }
    map
}

fn save_server_properties_of(server_id: &str, updates: &HashMap<String, String>) -> Result<(), std::io::Error> {
    let prop_path = get_server_dir(server_id).join("server.properties");
    let mut existing = read_server_properties_of(server_id);
    for (k, v) in updates {
        existing.insert(k.clone(), v.clone());
    }

    let mut out = String::new();
    out.push_str("# Minecraft Server Properties - Managed by PanelMC\n");
    for (k, v) in &existing {
        out.push_str(&format!("{}={}\n", k, v));
    }
    fs::write(prop_path, out)?;
    Ok(())
}

fn get_server_files_of(server_id: &str) -> Vec<FileItem> {
    let mut files = Vec::new();
    let server_path = get_server_dir(server_id);

    if let Ok(entries) = fs::read_dir(server_path) {
        for entry in entries.flatten() {
            if let Ok(metadata) = entry.metadata() {
                let is_dir = metadata.is_dir();
                let name = entry.file_name().to_string_lossy().to_string();
                let size = if is_dir {
                    "Klasör".to_string()
                } else {
                    let bytes = metadata.len();
                    if bytes > 1024 * 1024 {
                        format!("{:.1} MB", bytes as f64 / (1024.0 * 1024.0))
                    } else if bytes > 1024 {
                        format!("{:.1} KB", bytes as f64 / 1024.0)
                    } else {
                        format!("{} B", bytes)
                    }
                };

                files.push(FileItem {
                    name: name.into(),
                    is_dir,
                    size: size.into(),
                });
            }
        }
    }
    files.sort_by(|a, b| {
        b.is_dir.cmp(&a.is_dir).then_with(|| {
            a.name.as_str().to_lowercase().cmp(&b.name.as_str().to_lowercase())
        })
    });
    files
}

fn read_latest_log(server_id: &str) -> String {
    let path = get_server_dir(server_id).join("logs").join("latest.log");
    let data = match fs::read(&path) {
        Ok(d) if !d.is_empty() => d,
        _ => return "logs/latest.log yok. Sunucuyu bir kez çalıştırın.".into(),
    };
    let start = data.len().saturating_sub(256 * 1024);
    let text = String::from_utf8_lossy(&data[start..]);
    let lines: Vec<&str> = text.lines().collect();
    let skip = lines.len().saturating_sub(300);
    lines[skip..].join("\n")
}

#[derive(serde::Deserialize)]
struct McNamed {
    #[serde(default)]
    name: String,
    #[serde(default)]
    uuid: String,
}

fn load_named_json(path: &Path) -> Vec<McNamed> {
    fs::read_to_string(path)
        .ok()
        .and_then(|s| serde_json::from_str(&s).ok())
        .unwrap_or_default()
}

fn load_json_array(path: &Path) -> Vec<serde_json::Value> {
    fs::read_to_string(path)
        .ok()
        .and_then(|s| serde_json::from_str(&s).ok())
        .unwrap_or_default()
}

fn json_player_name(v: &serde_json::Value) -> String {
    v.get("name").and_then(|x| x.as_str()).unwrap_or("").to_string()
}

fn json_player_uuid(v: &serde_json::Value) -> String {
    v.get("uuid").and_then(|x| x.as_str()).unwrap_or("").to_string()
}

fn write_json_array(path: &Path, list: &[serde_json::Value]) {
    if let Ok(s) = serde_json::to_string_pretty(list) {
        let _ = fs::write(path, s);
    }
}

fn find_player_uuid(server_dir: &Path, name: &str) -> String {
    for file in ["usercache.json", "ops.json", "whitelist.json", "banned-players.json"] {
        for p in load_named_json(&server_dir.join(file)) {
            if p.name.eq_ignore_ascii_case(name) && !p.uuid.is_empty() {
                return p.uuid;
            }
        }
    }
    String::new()
}

fn upsert_named_file(path: &Path, name: &str, uuid: &str, as_op: bool) {
    let mut list = load_json_array(path);
    if let Some(existing) = list.iter_mut().find(|v| json_player_name(v).eq_ignore_ascii_case(name)) {
        if !uuid.is_empty() {
            existing["uuid"] = serde_json::Value::String(uuid.to_string());
        }
        existing["name"] = serde_json::Value::String(name.to_string());
        if as_op {
            existing["level"] = serde_json::json!(4);
            existing["bypassesPlayerLimit"] = serde_json::json!(false);
        }
    } else {
        let mut obj = serde_json::Map::new();
        obj.insert("name".into(), serde_json::Value::String(name.to_string()));
        obj.insert("uuid".into(), serde_json::Value::String(uuid.to_string()));
        if as_op {
            obj.insert("level".into(), serde_json::json!(4));
            obj.insert("bypassesPlayerLimit".into(), serde_json::json!(false));
        }
        list.push(serde_json::Value::Object(obj));
    }
    write_json_array(path, &list);
}

fn remove_named_file(path: &Path, name: &str) {
    let mut list = load_json_array(path);
    list.retain(|v| !json_player_name(v).eq_ignore_ascii_case(name));
    write_json_array(path, &list);
}

fn get_players_of(server_id: &str) -> Vec<PlayerItem> {
    let dir = get_server_dir(server_id);
    let cache = load_named_json(&dir.join("usercache.json"));
    let ops = load_json_array(&dir.join("ops.json"));
    let white = load_json_array(&dir.join("whitelist.json"));

    let mut map: std::collections::BTreeMap<String, (String, String, bool, bool)> = std::collections::BTreeMap::new();
    for p in cache {
        if p.name.is_empty() {
            continue;
        }
        map.entry(p.name.to_lowercase())
            .or_insert((p.name, p.uuid, false, false));
    }
    for v in &ops {
        let name = json_player_name(v);
        if name.is_empty() {
            continue;
        }
        let uuid = json_player_uuid(v);
        let e = map.entry(name.to_lowercase()).or_insert((name, uuid.clone(), false, false));
        e.2 = true;
        if e.1.is_empty() {
            e.1 = uuid;
        }
    }
    for v in &white {
        let name = json_player_name(v);
        if name.is_empty() {
            continue;
        }
        let uuid = json_player_uuid(v);
        let e = map.entry(name.to_lowercase()).or_insert((name, uuid.clone(), false, false));
        e.3 = true;
        if e.1.is_empty() {
            e.1 = uuid;
        }
    }

    map.into_values()
        .map(|(name, uuid, is_op, is_whitelisted)| PlayerItem {
            name: name.into(),
            uuid: uuid.into(),
            is_op,
            is_whitelisted,
        })
        .collect()
}

fn level_name_of(server_id: &str) -> String {
    read_server_properties_of(server_id)
        .get("level-name")
        .cloned()
        .filter(|s| !s.is_empty())
        .unwrap_or_else(|| "world".to_string())
}

fn get_worlds_of(server_id: &str) -> Vec<WorldItem> {
    let base = level_name_of(server_id);
    let dir = get_server_dir(server_id);
    let mut items = Vec::new();
    let mut seen = HashSet::new();
    let nether = format!("{}_nether", base);
    let end = format!("{}_the_end", base);

    let overworld_path = dir.join(&base);
    seen.insert(base.clone());
    items.push(WorldItem {
        name: base.clone().into(),
        kind: "Overworld".into(),
        exists: world_folder_ready(&overworld_path),
    });

    let nether_path = dir.join(&nether);
    if world_folder_ready(&nether_path) {
        seen.insert(nether.clone());
        items.push(WorldItem {
            name: nether.into(),
            kind: "Nether".into(),
            exists: true,
        });
    }
    let end_path = dir.join(&end);
    if world_folder_ready(&end_path) {
        seen.insert(end.clone());
        items.push(WorldItem {
            name: end.into(),
            kind: "The End".into(),
            exists: true,
        });
    }

    if let Ok(entries) = fs::read_dir(&dir) {
        for entry in entries.flatten() {
            let name = entry.file_name().to_string_lossy().to_string();
            if seen.contains(&name) || name == "backups" {
                continue;
            }
            let path = entry.path();
            if world_folder_ready(&path) {
                items.push(WorldItem {
                    name: name.into(),
                    kind: "Dünya".into(),
                    exists: true,
                });
            }
        }
    }
    items
}

fn world_folder_ready(path: &Path) -> bool {
    if !path.is_dir() {
        return false;
    }
    path.join("level.dat").is_file()
        || path.join("region").is_dir()
        || path.join("DIM-1").is_dir()
        || path.join("DIM1").is_dir()
}

fn format_bytes(bytes: u64) -> String {
    if bytes > 1024 * 1024 {
        format!("{:.1} MB", bytes as f64 / (1024.0 * 1024.0))
    } else if bytes > 1024 {
        format!("{:.1} KB", bytes as f64 / 1024.0)
    } else {
        format!("{} B", bytes)
    }
}

fn get_backups_of(server_id: &str) -> Vec<BackupItem> {
    let dir = get_server_dir(server_id).join("backups");
    let mut items = Vec::new();
    if let Ok(entries) = fs::read_dir(dir) {
        for entry in entries.flatten() {
            let path = entry.path();
            let name = entry.file_name().to_string_lossy().to_string();
            if !name.ends_with(".zip") || !path.is_file() {
                continue;
            }
            let size = entry.metadata().map(|m| format_bytes(m.len())).unwrap_or_else(|_| "-".into());
            items.push(BackupItem {
                name: name.into(),
                size: size.into(),
            });
        }
    }
    items.sort_by(|a, b| b.name.as_str().cmp(a.name.as_str()));
    items
}

fn is_safe_leaf_name(name: &str) -> bool {
    !name.is_empty()
        && !name.contains("..")
        && !name.contains('/')
        && !name.contains('\\')
        && !name.contains(':')
}

fn zip_world_folder(
    zip: &mut ZipWriter<File>,
    folder: &Path,
    zip_prefix: &str,
    buf: &mut [u8],
) -> Result<(), Box<dyn std::error::Error>> {
    let options = FileOptions::default().compression_method(CompressionMethod::Stored);
    zip.add_directory(format!("{}/", zip_prefix), options)?;
    let mut stack = vec![folder.to_path_buf()];
    while let Some(dir) = stack.pop() {
        let entries = match fs::read_dir(&dir) {
            Ok(e) => e,
            Err(_) => continue,
        };
        for entry in entries.flatten() {
            let path = entry.path();
            let fname = entry.file_name();
            let fname_str = fname.to_string_lossy();
            if fname_str == "session.lock" || fname_str == "session.lock.tmp" {
                continue;
            }
            let rel = match path.strip_prefix(folder) {
                Ok(r) => r,
                Err(_) => continue,
            };
            let zip_name = format!("{}/{}", zip_prefix, rel.to_string_lossy().replace('\\', "/"));
            if path.is_dir() {
                zip.add_directory(format!("{}/", zip_name), options)?;
                stack.push(path);
            } else if path.is_file() {
                zip.start_file(&zip_name, options)?;
                let mut f = File::open(&path)?;
                loop {
                    let n = f.read(buf)?;
                    if n == 0 {
                        break;
                    }
                    zip.write_all(&buf[..n])?;
                }
            }
        }
    }
    Ok(())
}

fn backup_worlds_of(server_id: &str) -> Result<String, String> {
    backup_worlds_prefixed(server_id, "yedek")
}

fn backup_every_min(raw: u32) -> u32 {
    match raw {
        30 | 60 | 360 => raw,
        _ => 0,
    }
}

fn backup_is_due(last: Option<SystemTime>, every_min: u32, now: SystemTime) -> bool {
    let every = backup_every_min(every_min);
    if every == 0 {
        return false;
    }
    match last {
        None => false,
        Some(t) => now
            .duration_since(t)
            .map(|d| d.as_secs() >= u64::from(every) * 60)
            .unwrap_or(true),
    }
}

fn auto_backup_allowed(free: Option<u64>) -> bool {
    match free {
        Some(n) => n >= 2 * 1024 * 1024 * 1024,
        None => true,
    }
}

fn save_finished(tail: &str) -> bool {
    tail.contains("Saved the game")
}

fn backup_worlds_prefixed(server_id: &str, prefix: &str) -> Result<String, String> {
    let worlds = get_worlds_of(server_id);
    let server_dir = get_server_dir(server_id);
    let folders: Vec<String> = worlds
        .iter()
        .map(|w| w.name.to_string())
        .filter(|name| server_dir.join(name).is_dir())
        .collect();
    let ts = SystemTime::now().duration_since(UNIX_EPOCH).map(|d| d.as_secs()).unwrap_or(0);
    backup_folders(&server_dir, &folders, prefix, ts)
}

fn backup_folders(server_dir: &Path, folders: &[String], prefix: &str, ts: u64) -> Result<String, String> {
    if folders.is_empty() {
        return Err("Dünya klasörü yok. Sunucuyu bir kez çalıştırın.".into());
    }
    if !prefix.chars().all(|c| c.is_ascii_alphanumeric()) {
        return Err("Geçersiz yedek adı.".into());
    }
    let backups = server_dir.join("backups");
    fs::create_dir_all(&backups).map_err(|e| e.to_string())?;
    let name = format!("{prefix}-{ts}.zip");
    let zip_path = backups.join(&name);
    let file = File::create(&zip_path).map_err(|e| e.to_string())?;
    let mut zip = ZipWriter::new(file);
    let mut buf = vec![0u8; 32 * 1024];
    let mut wrote = false;
    for folder_name in folders {
        if !is_safe_leaf_name(folder_name) {
            continue;
        }
        let folder = server_dir.join(folder_name);
        if folder.is_dir() {
            zip_world_folder(&mut zip, &folder, folder_name, &mut buf).map_err(|e| e.to_string())?;
            wrote = true;
        }
    }
    if !wrote {
        let _ = fs::remove_file(&zip_path);
        return Err("Dünya klasörü yok. Sunucuyu bir kez çalıştırın.".into());
    }
    zip.finish().map_err(|e| e.to_string())?;
    Ok(name)
}

fn prune_backups(server_dir: &Path, prefix: &str, keep: usize) -> Vec<String> {
    let dir = server_dir.join("backups");
    let mut names = Vec::new();
    if let Ok(entries) = fs::read_dir(&dir) {
        for entry in entries.flatten() {
            let name = entry.file_name().to_string_lossy().to_string();
            if name.starts_with(&format!("{prefix}-")) && name.ends_with(".zip") && entry.path().is_file() {
                names.push(name);
            }
        }
    }
    names.sort();
    let mut removed = Vec::new();
    while names.len() > keep {
        let name = names.remove(0);
        if fs::remove_file(dir.join(&name)).is_ok() {
            removed.push(name);
        }
    }
    removed
}

fn wait_until_saved(server_id: &str, timeout: Duration) -> bool {
    let path = get_server_dir(server_id).join("logs").join("latest.log");
    let start_len = fs::metadata(&path).map(|m| m.len()).unwrap_or(0) as usize;
    let start = Instant::now();
    while start.elapsed() < timeout {
        thread::sleep(Duration::from_millis(200));
        let Ok(data) = fs::read(&path) else { continue };
        let tail = if data.len() > start_len {
            String::from_utf8_lossy(&data[start_len..]).into_owned()
        } else {
            String::new()
        };
        if save_finished(&tail) {
            return true;
        }
    }
    false
}

fn send_console_line(procs: &Mutex<HashMap<String, ServerProcess>>, id: &str, line: &str) -> bool {
    let mut procs = procs.lock().unwrap();
    let Some(proc) = procs.get_mut(id) else { return false };
    let Some(stdin) = proc.stdin.as_mut() else { return false };
    if writeln!(stdin, "{line}").is_err() || stdin.flush().is_err() {
        return false;
    }
    proc.append_log(format!("> {line}"));
    true
}

#[derive(serde::Serialize, serde::Deserialize, Clone, Default)]
struct PanelSettings {
    #[serde(default)]
    cloud_folder: String,
    #[serde(default)]
    cloud_target: String,
    #[serde(default)]
    google_client_id: String,
    #[serde(default)]
    google_client_secret: String,
    #[serde(default)]
    onedrive_client_id: String,
    #[serde(default)]
    cloudflare_token: String,
}

fn panel_settings_path() -> PathBuf {
    Path::new("servers").join("panel.json")
}

fn load_panel_settings() -> PanelSettings {
    fs::read_to_string(panel_settings_path())
        .ok()
        .and_then(|s| serde_json::from_str(&s).ok())
        .unwrap_or_default()
}

fn save_panel_settings(settings: &PanelSettings) {
    let _ = fs::create_dir_all("servers");
    if let Ok(json) = serde_json::to_string_pretty(settings) {
        let _ = fs::write(panel_settings_path(), json);
    }
}

fn first_existing_dir(paths: impl IntoIterator<Item = PathBuf>) -> String {
    for p in paths {
        if p.is_dir() {
            return p.to_string_lossy().to_string();
        }
    }
    String::new()
}

fn detect_onedrive_folder() -> String {
    let mut paths = Vec::new();
    for key in ["OneDrive", "OneDriveConsumer", "OneDriveCommercial"] {
        if let Ok(v) = std::env::var(key) {
            paths.push(PathBuf::from(v));
        }
    }
    if let Ok(home) = std::env::var("USERPROFILE").or_else(|_| std::env::var("HOME")) {
        let home = PathBuf::from(home);
        paths.push(home.join("OneDrive"));
        paths.push(home.join("OneDrive - Personal"));
    }
    first_existing_dir(paths)
}

fn detect_gdrive_folder() -> String {
    let mut paths = Vec::new();
    if let Ok(home) = std::env::var("USERPROFILE").or_else(|_| std::env::var("HOME")) {
        let home = PathBuf::from(home);
        paths.push(home.join("Google Drive"));
        paths.push(home.join("GoogleDrive"));
        paths.push(home.join("My Drive"));
    }
    first_existing_dir(paths)
}

fn detect_dropbox_folder() -> String {
    let mut paths = Vec::new();
    if let Ok(home) = std::env::var("USERPROFILE").or_else(|_| std::env::var("HOME")) {
        paths.push(PathBuf::from(home).join("Dropbox"));
    }
    if let Ok(local) = std::env::var("LOCALAPPDATA") {
        let info = PathBuf::from(local).join("Dropbox").join("info.json");
        if let Ok(text) = fs::read_to_string(info) {
            if let Ok(v) = serde_json::from_str::<serde_json::Value>(&text) {
                for key in ["personal", "business"] {
                    if let Some(p) = v.get(key).and_then(|x| x.get("path")).and_then(|x| x.as_str()) {
                        paths.insert(0, PathBuf::from(p));
                    }
                }
            }
        }
    }
    first_existing_dir(paths)
}

fn pick_directory_dialog() -> Option<String> {
    #[cfg(target_os = "windows")]
    {
        let output = Command::new("powershell")
            .args([
                "-STA",
                "-NoProfile",
                "-Command",
                "Add-Type -AssemblyName System.Windows.Forms; $d = New-Object System.Windows.Forms.FolderBrowserDialog; $d.Description = 'OneDrive veya Google Drive klasorunu sec'; $d.ShowNewFolderButton = $true; if ($d.ShowDialog() -eq [System.Windows.Forms.DialogResult]::OK) { [Console]::Write($d.SelectedPath) }",
            ])
            .output()
            .ok()?;
        let s = String::from_utf8_lossy(&output.stdout).trim().to_string();
        if s.is_empty() { None } else { Some(s) }
    }
    #[cfg(not(target_os = "windows"))]
    {
        unix_pick(
            &["--file-selection", "--directory", "--title=OneDrive / Google Drive"],
            &["--getexistingdirectory"],
        )
    }
}

fn copy_file_buffered(src: &Path, dst: &Path) -> Result<(), String> {
    if let Some(parent) = dst.parent() {
        fs::create_dir_all(parent).map_err(|e| e.to_string())?;
    }
    let mut input = File::open(src).map_err(|e| e.to_string())?;
    let mut output = File::create(dst).map_err(|e| e.to_string())?;
    let mut buf = [0u8; 32 * 1024];
    loop {
        let n = input.read(&mut buf).map_err(|e| e.to_string())?;
        if n == 0 {
            break;
        }
        output.write_all(&buf[..n]).map_err(|e| e.to_string())?;
    }
    Ok(())
}

fn snapshot_panel_settings(ui: &MainWindow) -> PanelSettings {
    PanelSettings {
        cloud_folder: ui.get_cloud_folder().to_string(),
        cloud_target: ui.get_cloud_target().to_string(),
        google_client_id: ui.get_cloud_google_client_id().to_string(),
        google_client_secret: ui.get_cloud_google_secret().to_string(),
        onedrive_client_id: ui.get_cloud_onedrive_client_id().to_string(),
        cloudflare_token: ui.get_cloudflare_token().to_string(),
    }
}

fn push_backup_to_cloud(
    server_id: &str,
    name: &str,
    target: &str,
    folder: &str,
    google_id: &str,
    google_secret: &str,
    onedrive_id: &str,
) -> Result<String, String> {
    let zip = get_server_dir(server_id).join("backups").join(name);
    match target {
        "google" => cloud::upload_google(google_id, google_secret, server_id, name, &zip),
        "onedrive" => cloud::upload_onedrive(onedrive_id, server_id, name, &zip),
        _ => copy_backup_to_cloud_folder(server_id, name, folder).map(|p| p.display().to_string()),
    }
}

fn copy_backup_to_cloud_folder(server_id: &str, name: &str, folder: &str) -> Result<PathBuf, String> {
    if !is_safe_leaf_name(name) || !name.ends_with(".zip") {
        return Err("Geçersiz yedek.".into());
    }
    let folder = folder.trim();
    if folder.is_empty() {
        return Err("Uygulama ayarlarından OneDrive veya Google Drive klasörünü seçin.".into());
    }
    let dest_root = PathBuf::from(folder);
    if !dest_root.is_dir() {
        return Err("Seçilen klasör yok. OneDrive/Drive masaüstü uygulaması kurulu mu?".into());
    }
    let src = get_server_dir(server_id).join("backups").join(name);
    if !src.is_file() {
        return Err("Yedek bulunamadı.".into());
    }
    let dest = dest_root.join("PanelMC").join(server_id).join(name);
    copy_file_buffered(&src, &dest)?;
    Ok(dest)
}

fn reset_world_of(server_id: &str, name: &str) -> Result<(), String> {
    if !is_safe_leaf_name(name) {
        return Err("Geçersiz dünya adı.".into());
    }
    let path = get_server_dir(server_id).join(name);
    if !path.is_dir() {
        return Ok(());
    }
    fs::remove_dir_all(path).map_err(|e| e.to_string())
}

fn restore_backup_of(server_id: &str, name: &str) -> Result<(), String> {
    if !is_safe_leaf_name(name) || !name.ends_with(".zip") {
        return Err("Geçersiz yedek.".into());
    }
    let path = get_server_dir(server_id).join("backups").join(name);
    if !path.is_file() {
        return Err("Yedek bulunamadı.".into());
    }
    extract_zip_archive(&path, &get_server_dir(server_id)).map_err(|e| e.to_string())
}

fn delete_backup_of(server_id: &str, name: &str) -> Result<(), String> {
    if !is_safe_leaf_name(name) || !name.ends_with(".zip") {
        return Err("Geçersiz yedek.".into());
    }
    let path = get_server_dir(server_id).join("backups").join(name);
    if !path.is_file() {
        return Err("Yedek bulunamadı.".into());
    }
    fs::remove_file(path).map_err(|e| e.to_string())
}

fn server_is_running(procs: &HashMap<String, ServerProcess>, server_id: &str) -> bool {
    procs.get(server_id).map(|p| p.child.is_some()).unwrap_or(false)
}

fn server_online_now(procs: &HashMap<String, ServerProcess>, server_id: &str) -> bool {
    if server_is_running(procs, server_id) {
        return true;
    }
    let port = load_servers_list()
        .iter()
        .find(|s| s.id == server_id)
        .map(|s| s.port.clone())
        .unwrap_or_default();
    tcp_port_open(&port)
}

fn parse_ram(raw: &str) -> (String, String) {
    let s = raw.trim().replace(' ', "").to_uppercase();
    if let Some(n) = s.strip_suffix("GB").or_else(|| s.strip_suffix('G')) {
        if !n.is_empty() && n.chars().all(|c| c.is_ascii_digit()) {
            return (n.to_string(), "GB".into());
        }
    }
    if let Some(n) = s.strip_suffix("MB").or_else(|| s.strip_suffix('M')) {
        if !n.is_empty() && n.chars().all(|c| c.is_ascii_digit()) {
            return (n.to_string(), "MB".into());
        }
    }
    ("2".into(), "GB".into())
}

fn ram_stored(amount: &str, unit: &str) -> String {
    let n: u32 = amount.trim().parse().unwrap_or(2).max(1);
    if unit.eq_ignore_ascii_case("MB") {
        format!("{}M", n.max(256))
    } else {
        format!("{}G", n.clamp(1, 32))
    }
}

fn ram_xmx(amount: &str, unit: &str) -> String {
    format!("-Xmx{}", ram_stored(amount, unit))
}

fn apply_ram_to_ui(ui: &MainWindow, ram: &str) {
    let (amount, unit) = parse_ram(ram);
    ui.set_ram_amount(amount.clone().into());
    ui.set_ram_unit(unit.clone().into());
    ui.set_server_ram(ram_stored(&amount, &unit).into());
}

fn persist_ram_for(server_id: &str, amount: &str, unit: &str) {
    let stored = ram_stored(amount, unit);
    let mut list = load_servers_list();
    if let Some(srv) = list.iter_mut().find(|s| s.id == server_id) {
        srv.ram = stored;
        save_servers_list(&list);
    }
}

fn persist_timezone_for(server_id: &str, tz: &str) {
    let value = if tz.trim().is_empty() { "Europe/Istanbul".to_string() } else { tz.trim().to_string() };
    let mut list = load_servers_list();
    if let Some(srv) = list.iter_mut().find(|s| s.id == server_id) {
        srv.timezone = value;
        save_servers_list(&list);
    }
}

fn sha1_hex(bytes: &[u8]) -> String {
    use sha1::{Digest, Sha1};
    let mut hasher = Sha1::new();
    hasher.update(bytes);
    format!("{:x}", hasher.finalize())
}

fn write_varint(out: &mut Vec<u8>, mut value: i32) {
    loop {
        let mut byte = (value & 0x7F) as u8;
        value >>= 7;
        if value != 0 {
            byte |= 0x80;
        }
        out.push(byte);
        if value == 0 {
            break;
        }
    }
}

fn read_varint(stream: &mut TcpStream) -> Option<i32> {
    let mut num = 0i32;
    for i in 0..5 {
        let mut buf = [0u8; 1];
        stream.read_exact(&mut buf).ok()?;
        num |= ((buf[0] & 0x7F) as i32) << (7 * i);
        if buf[0] & 0x80 == 0 {
            return Some(num);
        }
    }
    None
}

struct McStatus {
    online: i32,
    max: i32,
    names: String,
}

fn mc_status(port: &str) -> Option<McStatus> {
    let port_n: u16 = port.parse().ok()?;
    let mut stream = TcpStream::connect_timeout(
        &SocketAddr::from(([127, 0, 0, 1], port_n)),
        Duration::from_millis(500),
    ).ok()?;
    let _ = stream.set_read_timeout(Some(Duration::from_millis(800)));
    let _ = stream.set_write_timeout(Some(Duration::from_millis(500)));

    let host = b"localhost";
    let mut body = Vec::new();
    write_varint(&mut body, 0);
    write_varint(&mut body, 47);
    write_varint(&mut body, host.len() as i32);
    body.extend_from_slice(host);
    body.extend_from_slice(&port_n.to_be_bytes());
    write_varint(&mut body, 1);

    let mut packet = Vec::new();
    write_varint(&mut packet, body.len() as i32);
    packet.extend_from_slice(&body);
    packet.extend_from_slice(&[0x01, 0x00]);
    stream.write_all(&packet).ok()?;

    let len = read_varint(&mut stream)?;
    if !(2..=1_048_576).contains(&len) {
        return None;
    }
    let mut payload = vec![0u8; len as usize];
    stream.read_exact(&mut payload).ok()?;
    let mut i = 0usize;
    while i < payload.len() {
        let b = payload[i];
        i += 1;
        if b & 0x80 == 0 {
            break;
        }
    }
    let mut slen = 0i32;
    let mut shift = 0;
    while i < payload.len() && shift <= 28 {
        let b = payload[i];
        i += 1;
        slen |= ((b & 0x7F) as i32) << shift;
        if b & 0x80 == 0 {
            break;
        }
        shift += 7;
    }
    if slen < 2 || i + slen as usize > payload.len() {
        return None;
    }
    let json: serde_json::Value = serde_json::from_slice(&payload[i..i + slen as usize]).ok()?;
    let players = json.get("players")?;
    let online = players.get("online")?.as_i64()? as i32;
    let max = players.get("max")?.as_i64()? as i32;
    let names = players
        .get("sample")
        .and_then(|v| v.as_array())
        .map(|list| {
            list.iter()
                .filter_map(|p| p.get("name").and_then(|n| n.as_str()))
                .take(8)
                .collect::<Vec<_>>()
                .join(", ")
        })
        .unwrap_or_default();
    Some(McStatus { online, max, names })
}

fn format_uptime(secs: u64) -> String {
    let h = secs / 3600;
    let m = (secs % 3600) / 60;
    let s = secs % 60;
    if h > 0 {
        format!("{h}h {m:02}m")
    } else if m > 0 {
        format!("{m}m {s:02}s")
    } else {
        format!("{s}s")
    }
}

fn disk_free_bytes(path: &Path) -> Option<u64> {
    #[cfg(windows)]
    {
        use std::os::windows::ffi::OsStrExt;
        #[link(name = "kernel32")]
        extern "system" {
            fn GetDiskFreeSpaceExW(
                directory: *const u16,
                available: *mut u64,
                total: *mut u64,
                total_free: *mut u64,
            ) -> i32;
        }
        let wide: Vec<u16> = path.as_os_str().encode_wide().chain(std::iter::once(0)).collect();
        let mut available = 0u64;
        let ok = unsafe {
            GetDiskFreeSpaceExW(wide.as_ptr(), &mut available, std::ptr::null_mut(), std::ptr::null_mut())
        };
        if ok == 0 { None } else { Some(available) }
    }
    #[cfg(not(windows))]
    {
        let out = Command::new("df")
            .args(["-B1", "-P", path.to_str()?])
            .output()
            .ok()?;
        let text = String::from_utf8_lossy(&out.stdout);
        let line = text.lines().nth(1)?;
        let avail = line.split_whitespace().nth(3)?;
        avail.parse().ok()
    }
}

fn tcp_port_open(port: &str) -> bool {
    let Ok(port) = port.trim().parse::<u16>() else {
        return false;
    };
    let addr = SocketAddr::from(([127, 0, 0, 1], port));
    TcpStream::connect_timeout(&addr, Duration::from_millis(80)).is_ok()
}

fn pid_file_path(server_id: &str) -> PathBuf {
    get_server_dir(server_id).join("panelmc.pid")
}

fn write_server_pid(server_id: &str, pid: u32) {
    let _ = fs::write(pid_file_path(server_id), pid.to_string());
}

fn read_server_pid(server_id: &str) -> Option<u32> {
    fs::read_to_string(pid_file_path(server_id)).ok()?.trim().parse().ok()
}

fn clear_server_pid(server_id: &str) {
    let _ = fs::remove_file(pid_file_path(server_id));
}

#[cfg(windows)]
fn local_listen_port(addr: &str) -> Option<u16> {
    let addr = addr.trim();
    if let Some(rest) = addr.strip_prefix('[') {
        let end = rest.find(']')?;
        return rest.get(end + 1..)?.strip_prefix(':')?.parse().ok();
    }
    addr.rsplit_once(':')?.1.parse().ok()
}

fn pids_listening_on_port(port: u16) -> Vec<u32> {
    #[cfg(windows)]
    {
        let Ok(output) = ({
            let mut cmd = Command::new("netstat");
            cmd.args(["-ano"]);
            hide_window(&mut cmd);
            cmd.output()
        }) else {
            return Vec::new();
        };
        let text = String::from_utf8_lossy(&output.stdout);
        let mut pids = Vec::new();
        for line in text.lines() {
            let cols: Vec<&str> = line.split_whitespace().collect();
            if cols.len() < 5 {
                continue;
            }
            if !cols.iter().any(|c| c.eq_ignore_ascii_case("LISTENING")) {
                continue;
            }
            let Some(p) = local_listen_port(cols[1]) else {
                continue;
            };
            if p != port {
                continue;
            }
            if let Ok(pid) = cols[cols.len() - 1].parse::<u32>() {
                if pid != 0 && !pids.contains(&pid) {
                    pids.push(pid);
                }
            }
        }
        #[allow(clippy::needless_return)]
        return pids;
    }
    #[cfg(unix)]
    {
        pids_listening_linux(port)
    }
}

#[cfg(unix)]
fn pids_listening_linux(port: u16) -> Vec<u32> {
    let mut inodes = Vec::new();
    for table in ["/proc/net/tcp", "/proc/net/tcp6"] {
        let Ok(text) = fs::read_to_string(table) else { continue };
        for line in text.lines().skip(1) {
            let cols: Vec<&str> = line.split_whitespace().collect();
            if cols.len() < 10 {
                continue;
            }
            let Some((_, phex)) = cols[1].rsplit_once(':') else { continue };
            let Ok(p) = u16::from_str_radix(phex, 16) else { continue };
            if p != port || cols[3] != "0A" {
                continue;
            }
            if let Ok(inode) = cols[9].parse::<u64>() {
                if inode != 0 {
                    inodes.push(inode);
                }
            }
        }
    }
    if inodes.is_empty() {
        return Vec::new();
    }
    let mut pids = Vec::new();
    let Ok(proc_dir) = fs::read_dir("/proc") else {
        return pids;
    };
    for ent in proc_dir.flatten() {
        let name = ent.file_name();
        let Some(pid) = name.to_str().and_then(|s| s.parse::<u32>().ok()) else {
            continue;
        };
        let Ok(fds) = fs::read_dir(ent.path().join("fd")) else { continue };
        for fd in fds.flatten() {
            let Ok(link) = fs::read_link(fd.path()) else { continue };
            let s = link.to_string_lossy();
            let Some(rest) = s.strip_prefix("socket:[") else { continue };
            let Some(num) = rest.strip_suffix(']') else { continue };
            let Ok(ino) = num.parse::<u64>() else { continue };
            if inodes.contains(&ino) && !pids.contains(&pid) {
                pids.push(pid);
            }
        }
    }
    pids
}

fn kill_os_pid(pid: u32) {
    if pid == 0 {
        return;
    }
    #[cfg(windows)]
    {
        let mut cmd = Command::new("taskkill");
        cmd.args(["/PID", &pid.to_string(), "/T", "/F"]);
        hide_window(&mut cmd);
        let _ = cmd.output();
    }
    #[cfg(not(windows))]
    {
        let _ = Command::new("kill").args(["-9", &pid.to_string()]).output();
    }
}

fn force_stop_java(server_id: &str, port: &str) {
    if let Ok(port_n) = port.trim().parse::<u16>() {
        for pid in pids_listening_on_port(port_n) {
            kill_os_pid(pid);
        }
    }
    if let Some(pid) = read_server_pid(server_id) {
        kill_os_pid(pid);
    }
    clear_server_pid(server_id);
}

fn reap_finished_child(proc: &mut ServerProcess) -> bool {
    if let Some(child) = proc.child.as_mut() {
        match child.try_wait() {
            Ok(None) => return true,
            Ok(Some(_)) | Err(_) => {
                proc.child = None;
                proc.stdin = None;
                proc.append_log("[SİSTEM] Java süreci sonlandı.".to_string());
                return false;
            }
        }
    }
    false
}

fn pipe_lines_lossy<R: Read>(reader: R, mut on_line: impl FnMut(String)) {
    let mut reader = reader;
    let mut buf = Vec::with_capacity(4096);
    let mut chunk = [0u8; 4096];
    loop {
        let n = match reader.read(&mut chunk) {
            Ok(0) => break,
            Ok(n) => n,
            Err(_) => break,
        };
        buf.extend_from_slice(&chunk[..n]);
        while let Some(pos) = buf.iter().position(|&b| b == b'\n') {
            let line: Vec<u8> = buf.drain(..=pos).collect();
            let s = String::from_utf8_lossy(&line).trim_end_matches(['\r', '\n']).to_string();
            if !s.is_empty() {
                on_line(s);
            }
        }
    }
    if !buf.is_empty() {
        let s = String::from_utf8_lossy(&buf).trim().to_string();
        if !s.is_empty() {
            on_line(s);
        }
    }
}

fn pick_zip_dialog() -> Option<String> {
    #[cfg(target_os = "windows")]
    {
        let output = Command::new("powershell")
            .args([
                "-STA",
                "-NoProfile",
                "-Command",
                "Add-Type -AssemblyName System.Windows.Forms; $d = New-Object System.Windows.Forms.OpenFileDialog; $d.Filter = 'Zip (*.zip)|*.zip'; $d.Title = 'Dunya yukle'; if ($d.ShowDialog() -eq [System.Windows.Forms.DialogResult]::OK) { [Console]::Write($d.FileName) }",
            ])
            .output()
            .ok()?;
        let s = String::from_utf8_lossy(&output.stdout).trim().to_string();
        if s.is_empty() { None } else { Some(s) }
    }
    #[cfg(not(target_os = "windows"))]
    {
        unix_pick(
            &["--file-selection", "--file-filter=*.zip", "--title=Dunya yukle"],
            &["--getopenfilename", ".", "*.zip"],
        )
    }
}

fn extract_world_zip(server_id: &str, world_name: &str, zip_path: &Path) -> Result<(), String> {
    if !is_safe_leaf_name(world_name) {
        return Err("Geçersiz dünya adı.".into());
    }
    let dest = get_server_dir(server_id).join(world_name);
    if dest.exists() {
        fs::remove_dir_all(&dest).map_err(|e| e.to_string())?;
    }
    fs::create_dir_all(&dest).map_err(|e| e.to_string())?;
    extract_zip_archive(zip_path, &dest).map_err(|e| e.to_string())?;
    if dest.join("level.dat").is_file() {
        return Ok(());
    }
    if let Ok(entries) = fs::read_dir(&dest) {
        let nested: Vec<PathBuf> = entries.flatten().map(|e| e.path()).filter(|p| p.is_dir() && p.join("level.dat").is_file()).collect();
        if nested.len() == 1 {
            let tmp = dest.with_extension("import-tmp");
            if tmp.exists() {
                let _ = fs::remove_dir_all(&tmp);
            }
            fs::rename(&nested[0], &tmp).map_err(|e| e.to_string())?;
            let _ = fs::remove_dir_all(&dest);
            fs::rename(&tmp, &dest).map_err(|e| e.to_string())?;
        }
    }
    if dest.join("level.dat").is_file() {
        Ok(())
    } else {
        Err("Zip içinde level.dat bulunamadı.".into())
    }
}

fn create_or_reset_world(server_id: &str, name: &str) -> Result<(), String> {
    let base = level_name_of(server_id);
    reset_world_of(server_id, name)?;
    if name == base {
        let _ = reset_world_of(server_id, &format!("{}_nether", base));
        let _ = reset_world_of(server_id, &format!("{}_the_end", base));
    }
    Ok(())
}

fn format_version_rows(versions: Vec<String>) -> Vec<VersionRow> {
    let mut rows = Vec::new();
    for chunk in versions.chunks(4) {
        let v1 = chunk.first().cloned().unwrap_or_default();
        let has1 = !chunk.is_empty();
        let v2 = chunk.get(1).cloned().unwrap_or_default();
        let has2 = chunk.get(1).is_some();
        let v3 = chunk.get(2).cloned().unwrap_or_default();
        let has3 = chunk.get(2).is_some();
        let v4 = chunk.get(3).cloned().unwrap_or_default();
        let has4 = chunk.get(3).is_some();

        rows.push(VersionRow {
            v1: v1.into(), has1,
            v2: v2.into(), has2,
            v3: v3.into(), has3,
            v4: v4.into(), has4,
        });
    }
    rows
}

fn http_json<T: serde::de::DeserializeOwned>(client: &reqwest::blocking::Client, url: &str) -> Result<T, String> {
    let resp = client.get(url).send().map_err(|e| e.to_string())?;
    let status = resp.status();
    if !status.is_success() {
        return Err(format!("HTTP {status}"));
    }
    resp.json::<T>().map_err(|e| e.to_string())
}

fn parse_version(tag: &str) -> Option<(u64, u64, u64)> {
    let tag = tag.trim().trim_start_matches('v');
    let mut parts = tag.split(['.', '-']);
    let major = parts.next()?.parse().ok()?;
    let minor = parts.next()?.parse().ok()?;
    let patch_src = parts.next().unwrap_or("0");
    let patch: String = patch_src.chars().take_while(|c| c.is_ascii_digit()).collect();
    let patch = if patch.is_empty() { 0 } else { patch.parse().ok()? };
    Some((major, minor, patch))
}

fn vanilla_versions(client: &reqwest::blocking::Client) -> Result<Vec<String>, String> {
    let manifest: MojangManifest = http_json(client, "https://piston-meta.mojang.com/mc/game/version_manifest_v2.json")?;
    Ok(manifest.versions.into_iter().filter(|v| v.kind == "release").take(40).map(|v| v.id).collect())
}

fn vanilla_server_url(client: &reqwest::blocking::Client, version: &str) -> Result<String, String> {
    let manifest: MojangManifest = http_json(client, "https://piston-meta.mojang.com/mc/game/version_manifest_v2.json")?;
    let entry = manifest.versions.into_iter().find(|v| v.id == version)
        .ok_or_else(|| format!("Vanilla {version} Mojang listesinde yok."))?;
    let meta: MojangVersionMeta = http_json(client, &entry.url)?;
    meta.downloads.server.map(|d| d.url).ok_or_else(|| format!("Vanilla {version} için sunucu jar yok."))
}

fn fabric_versions(client: &reqwest::blocking::Client) -> Result<Vec<String>, String> {
    let games: Vec<FabricEntry> = http_json(client, "https://meta.fabricmc.net/v2/versions/game")?;
    Ok(games.into_iter().filter(|g| g.stable).take(40).map(|g| g.version).collect())
}

fn fabric_server_url(client: &reqwest::blocking::Client, version: &str) -> Result<String, String> {
    let loaders: Vec<FabricEntry> = http_json(client, "https://meta.fabricmc.net/v2/versions/loader")?;
    let installers: Vec<FabricEntry> = http_json(client, "https://meta.fabricmc.net/v2/versions/installer")?;
    let loader = loaders.iter().find(|l| l.stable).or(loaders.first())
        .ok_or_else(|| "Fabric loader bulunamadı.".to_string())?;
    let installer = installers.iter().find(|l| l.stable).or(installers.first())
        .ok_or_else(|| "Fabric installer bulunamadı.".to_string())?;
    Ok(format!(
        "https://meta.fabricmc.net/v2/versions/loader/{}/{}/{}/server/jar",
        version, loader.version, installer.version
    ))
}

fn primary_lan_ip() -> String {
    let Ok(socket) = std::net::UdpSocket::bind("0.0.0.0:0") else { return String::new() };
    if socket.connect("1.1.1.1:80").is_err() { return String::new() }
    match socket.local_addr() {
        Ok(addr) => {
            let ip = addr.ip();
            if ip.is_ipv4() && !ip.is_loopback() { ip.to_string() } else { String::new() }
        }
        Err(_) => String::new(),
    }
}

static PUBLIC_IP: Mutex<Option<String>> = Mutex::new(None);
static WAN_TRIED: AtomicBool = AtomicBool::new(false);

fn set_public_ip(ip: String) {
    if let Ok(mut guard) = PUBLIC_IP.lock() {
        *guard = Some(ip);
    }
    WAN_TRIED.store(true, Ordering::Relaxed);
}

fn mark_wan_tried() {
    WAN_TRIED.store(true, Ordering::Relaxed);
}

fn valid_port(raw: &str) -> String {
    match raw.trim().parse::<u16>() {
        Ok(port) if port > 0 => port.to_string(),
        _ => "25565".to_string(),
    }
}

fn apply_join(ui: &MainWindow, port: &str) {
    let port = valid_port(port);
    let lan = primary_lan_ip();
    let lan_show = if lan.is_empty() {
        format!("127.0.0.1:{port}")
    } else {
        format!("{lan}:{port}")
    };
    ui.set_join_lan(lan_show.clone().into());
    ui.set_server_ip(lan_show.into());
    let known = PUBLIC_IP.lock().ok().and_then(|guard| guard.clone());
    let wan = if let Some(ip) = known.as_deref() {
        format!("{ip}:{port}")
    } else if WAN_TRIED.load(Ordering::Relaxed) {
        if ui.get_app_lang() == "tr" { "alınamadı".to_string() } else { "unavailable".to_string() }
    } else {
        "…".to_string()
    };
    ui.set_join_wan(wan.into());
    let host = ui.get_join_host().trim().to_string();
    if host.is_empty() {
        ui.set_join_play("".into());
    } else {
        ui.set_join_play(host.into());
    }
    let warn = match known.as_deref() {
        Some(ip) if is_cgnat(ip) => {
            if ui.get_app_lang() == "tr" {
                "Bu dış IP CGNAT aralığında (100.64). Port yönlendirme çalışmaz.".to_string()
            } else {
                "This public IP is in the CGNAT range (100.64). Port forwarding will not work.".to_string()
            }
        }
        _ => String::new(),
    };
    ui.set_join_warn(warn.into());
}

fn is_cgnat(ip: &str) -> bool {
    let Ok(addr) = ip.parse::<std::net::Ipv4Addr>() else { return false };
    let o = addr.octets();
    o[0] == 100 && (64..128).contains(&o[1])
}

fn valid_hostname(raw: &str) -> Result<String, String> {
    let host = raw.trim().trim_end_matches('.').to_ascii_lowercase();
    if host.len() < 4 || host.len() > 253 || !host.contains('.') {
        return Err("Alan adı geçersiz. Örnek: mc.duckdns.org".into());
    }
    let ok = host.split('.').all(|label| {
        !label.is_empty()
            && label.len() <= 63
            && !label.starts_with('-')
            && !label.ends_with('-')
            && label.chars().all(|c| c.is_ascii_alphanumeric() || c == '-')
    });
    if !ok {
        return Err("Alan adında sadece harf, rakam ve tire olabilir.".into());
    }
    Ok(host)
}

fn cf_call(token: &str, method: &str, url: &str, body: Option<serde_json::Value>) -> Result<serde_json::Value, String> {
    let client = http_client();
    let builder = match method {
        "POST" => client.post(url),
        "PUT" => client.put(url),
        _ => client.get(url),
    };
    let builder = builder
        .header("Authorization", format!("Bearer {token}"))
        .header("Content-Type", "application/json");
    let response = match body {
        Some(body) => builder.json(&body).send(),
        None => builder.send(),
    }.map_err(|e| e.to_string())?;
    let status = response.status();
    let value: serde_json::Value = response.json().map_err(|e| e.to_string())?;
    let success = value.get("success").and_then(|v| v.as_bool()).unwrap_or(status.is_success());
    if !success {
        let msg = value["errors"][0]["message"].as_str().unwrap_or("Cloudflare reddetti");
        return Err(msg.to_string());
    }
    Ok(value)
}

fn publish_minecraft_dns(token: &str, host: &str, ip: &str, port: u16) -> Result<String, String> {
    let zones = cf_call(token, "GET", "https://api.cloudflare.com/client/v4/zones?per_page=50", None)?;
    let list = zones["result"].as_array().ok_or("Cloudflare bölge listesi boş.")?;
    let zone = list.iter().filter_map(|z| {
        let name = z["name"].as_str()?;
        let id = z["id"].as_str()?;
        if host == name || host.ends_with(&format!(".{name}")) {
            Some((name.len(), id.to_string(), name.to_string()))
        } else {
            None
        }
    }).max_by_key(|item| item.0).ok_or("Bu alan adı token'daki bir Cloudflare bölgesine ait değil.")?;
    let zone_id = zone.1;
    upsert_a(token, &zone_id, host, ip)?;
    upsert_srv(token, &zone_id, host, port)?;
    Ok(format!("{host} için A ve SRV yazıldı. Oyuncu port yazmadan {host} girer."))
}

fn upsert_a(token: &str, zone_id: &str, host: &str, ip: &str) -> Result<(), String> {
    let url = format!("https://api.cloudflare.com/client/v4/zones/{zone_id}/dns_records?type=A&name={host}");
    let existing = cf_call(token, "GET", &url, None)?;
    let body = serde_json::json!({
        "type": "A",
        "name": host,
        "content": ip,
        "ttl": 120,
        "proxied": false
    });
    if let Some(id) = existing["result"][0]["id"].as_str() {
        let put = format!("https://api.cloudflare.com/client/v4/zones/{zone_id}/dns_records/{id}");
        cf_call(token, "PUT", &put, Some(body))?;
    } else {
        let post = format!("https://api.cloudflare.com/client/v4/zones/{zone_id}/dns_records");
        cf_call(token, "POST", &post, Some(body))?;
    }
    Ok(())
}

fn upsert_srv(token: &str, zone_id: &str, host: &str, port: u16) -> Result<(), String> {
    let name = format!("_minecraft._tcp.{host}");
    let url = format!("https://api.cloudflare.com/client/v4/zones/{zone_id}/dns_records?type=SRV&name={name}");
    let existing = cf_call(token, "GET", &url, None)?;
    let body = serde_json::json!({
        "type": "SRV",
        "name": name,
        "ttl": 120,
        "data": { "priority": 0, "weight": 5, "port": port, "target": host }
    });
    if let Some(id) = existing["result"][0]["id"].as_str() {
        let put = format!("https://api.cloudflare.com/client/v4/zones/{zone_id}/dns_records/{id}");
        cf_call(token, "PUT", &put, Some(body))?;
    } else {
        let post = format!("https://api.cloudflare.com/client/v4/zones/{zone_id}/dns_records");
        cf_call(token, "POST", &post, Some(body))?;
    }
    Ok(())
}

fn fetch_public_ip() -> Option<String> {
    let client = http_client();
    let text = client.get("https://api.ipify.org").send().ok()?.text().ok()?;
    let ip = text.trim();
    ip.parse::<std::net::Ipv4Addr>().ok()?;
    Some(ip.to_string())
}

fn newer_release(local: &str, remote_tag: &str) -> bool {
    match (parse_version(local), parse_version(remote_tag)) {
        (Some(local), Some(remote)) => remote > local,
        _ => false,
    }
}

fn open_url(url: &str) {
    if url.is_empty() || url.contains('"') || url.contains(' ') { return; }
    #[cfg(target_os = "windows")]
    {
        let cmdline = format!("start \"\" \"{url}\"");
        let mut cmd = Command::new("cmd");
        cmd.args(["/C", &cmdline]);
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

fn copy_text(text: &str) {
    if text.is_empty() || text == "…" || text == "alınamadı" || text == "unavailable" { return; }
    #[cfg(target_os = "windows")]
    {
        let mut cmd = Command::new("clip");
        cmd.stdin(Stdio::piped());
        hide_window(&mut cmd);
        if let Ok(mut child) = cmd.spawn() {
            if let Some(mut stdin) = child.stdin.take() {
                let _ = stdin.write_all(text.as_bytes());
            }
        }
    }
    #[cfg(target_os = "linux")]
    {
        let mut wl = Command::new("wl-copy");
        wl.stdin(Stdio::piped());
        if let Ok(mut child) = wl.spawn() {
            if let Some(mut stdin) = child.stdin.take() {
                let _ = stdin.write_all(text.as_bytes());
            }
            return;
        }
        let mut xclip = Command::new("xclip");
        xclip.args(["-selection", "clipboard"]).stdin(Stdio::piped());
        if let Ok(mut child) = xclip.spawn() {
            if let Some(mut stdin) = child.stdin.take() {
                let _ = stdin.write_all(text.as_bytes());
            }
        }
    }
    #[cfg(target_os = "macos")]
    {
        let mut cmd = Command::new("pbcopy");
        cmd.stdin(Stdio::piped());
        if let Ok(mut child) = cmd.spawn() {
            if let Some(mut stdin) = child.stdin.take() {
                let _ = stdin.write_all(text.as_bytes());
            }
        }
    }
}

fn paper_versions(client: &reqwest::blocking::Client) -> Result<Vec<String>, String> {
    let project: FillProject = http_json(client, "https://fill.papermc.io/v3/projects/paper")?;
    let mut list = Vec::new();
    for value in project.versions.values() {
        if let Some(arr) = value.as_array() {
            for item in arr {
                if let Some(version) = item.as_str() {
                    list.push(version.to_string());
                }
            }
        }
    }
    list.truncate(48);
    Ok(list)
}

fn paper_server_url(client: &reqwest::blocking::Client, version: &str) -> Result<String, String> {
    let url = format!("https://fill.papermc.io/v3/projects/paper/versions/{version}/builds/latest");
    let build: FillLatestBuild = http_json(client, &url)?;
    build.downloads.server.map(|f| f.url).ok_or_else(|| format!("Paper {version} için jar yok."))
}

fn fetch_versions_for_software(software: &str) -> Vec<String> {
    let client = http_client();

    let fetched = match software {
        "Purpur" => client.get("https://api.purpurmc.org/v2/purpur").send().ok()
            .and_then(|r| r.json::<PurpurVersionsResponse>().ok())
            .map(|data| {
                let mut list = data.versions;
                list.reverse();
                list.truncate(40);
                list
            }),
        "Paper" => paper_versions(client).ok(),
        "Vanilla" => vanilla_versions(client).ok(),
        "Fabric" => fabric_versions(client).ok(),
        _ => None,
    };

    fetched.unwrap_or_default()
}

fn resolve_server_url(client: &reqwest::blocking::Client, software: &str, version: &str) -> Result<String, String> {
    match software {
        "Purpur" => Ok(format!("https://api.purpurmc.org/v2/purpur/{version}/latest/download")),
        "Paper" => paper_server_url(client, version),
        "Fabric" => fabric_server_url(client, version),
        "Vanilla" => vanilla_server_url(client, version),
        _ => Err(format!("{software} jar indirmesi bu sürümde yok. Vanilla, Paper, Purpur veya Fabric seç.")),
    }
}

fn sync_java_status(ui: &MainWindow, selected_pref: &str) {
    let p25 = Path::new("runtimes").join("java25");
    let p21 = Path::new("runtimes").join("java21");
    let p17 = Path::new("runtimes").join("java17");

    let has25 = find_java_in_dir(&p25).is_some();
    let has21 = find_java_in_dir(&p21).is_some();
    let has17 = find_java_in_dir(&p17).is_some();

    ui.set_has_java25(has25);
    ui.set_has_java21(has21);
    ui.set_has_java17(has17);

    if let Some(exe) = get_executable_for_selected_java(selected_pref) {
        let label = match selected_pref {
            "java25" => "Java 25 (Seçili)",
            "java21" => "Java 21 (Seçili)",
            "java17" => "Java 17 (Seçili)",
            _ => "Sistem Java'sı",
        };
        ui.set_active_java_info(format!("{} - {:?}", label, exe.file_name().unwrap_or_default()).into());
    } else {
        ui.set_active_java_info("Java Bulunamadı! Lütfen Java 25 indirin.".into());
    }
}

struct IconPixels {
    width: u32,
    height: u32,
    rgba: Vec<u8>,
}

struct TempModItem {
    slug: String,
    title: String,
    description: String,
    downloads: String,
    icon_pixels: Option<IconPixels>,
}

fn format_download_count(count: u64) -> String {
    if count >= 1_000_000 {
        format!("{:.1}M", count as f64 / 1_000_000.0)
    } else if count >= 1_000 {
        format!("{:.1}K", count as f64 / 1_000.0)
    } else {
        count.to_string()
    }
}

fn percent_encode_path(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for &b in s.as_bytes() {
        match b {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' => out.push(b as char),
            _ => out.push_str(&format!("%{:02X}", b)),
        }
    }
    out
}

fn safe_cache_name(slug: &str) -> String {
    let mut s: String = slug
        .chars()
        .map(|c| if c.is_ascii_alphanumeric() || c == '-' || c == '_' { c } else { '_' })
        .collect();
    if s.is_empty() {
        s.push_str("icon");
    }
    s
}

fn normalize_icon_url(url: &str) -> Option<String> {
    let t = url.trim();
    if t.is_empty() {
        return None;
    }
    if t.starts_with("//") {
        Some(format!("https:{t}"))
    } else if t.starts_with("http://") || t.starts_with("https://") {
        Some(t.to_string())
    } else {
        None
    }
}

fn bytes_to_icon_pixels(bytes: &[u8]) -> Option<IconPixels> {
    let dyn_img = image::load_from_memory(bytes).ok()?;
    let rgba = dyn_img.thumbnail(64, 64).to_rgba8();
    let (width, height) = rgba.dimensions();
    if width == 0 || height == 0 {
        return None;
    }
    Some(IconPixels { width, height, rgba: rgba.into_raw() })
}

fn icon_pixels_to_slint(pixels: IconPixels) -> slint::Image {
    let buffer = slint::SharedPixelBuffer::<slint::Rgba8Pixel>::clone_from_slice(
        &pixels.rgba,
        pixels.width,
        pixels.height,
    );
    slint::Image::from_rgba8(buffer)
}

fn download_icon_bytes(client: &reqwest::blocking::Client, url: &str) -> Option<Vec<u8>> {
    let resp = client.get(url).send().ok()?;
    if !resp.status().is_success() {
        return None;
    }
    let bytes = resp.bytes().ok()?;
    if bytes.len() < 24 || bytes.len() > 1_500_000 {
        return None;
    }
    Some(bytes.to_vec())
}

fn icon_from_url(client: &reqwest::blocking::Client, cache_dir: &Path, slug: &str, url: &str) -> Option<IconPixels> {
    let url = normalize_icon_url(url)?;
    let path = cache_dir.join(format!("{}.bin", safe_cache_name(slug)));
    if let Ok(existing) = fs::read(&path) {
        if existing.len() >= 24 && existing.len() <= 1_500_000 {
            if let Some(img) = bytes_to_icon_pixels(&existing) {
                return Some(img);
            }
        }
        let _ = fs::remove_file(&path);
    }
    let downloaded = download_icon_bytes(client, &url)?;
    let img = bytes_to_icon_pixels(&downloaded)?;
    let _ = fs::write(&path, &downloaded);
    Some(img)
}

fn main() -> Result<(), slint::PlatformError> {
    let ui = MainWindow::new()?;
    playit::attach(ui.as_weak());

    let is_dark = detect_system_dark_theme();
    let lang = detect_system_language();
    ui.set_is_dark_theme(is_dark);
    ui.set_app_lang(lang.into());
    ui.set_app_version(env!("CARGO_PKG_VERSION").into());
    let panel = load_panel_settings();
    let onedrive = detect_onedrive_folder();
    let gdrive = detect_gdrive_folder();
    let dropbox = detect_dropbox_folder();
    ui.set_cloud_onedrive(onedrive.clone().into());
    ui.set_cloud_gdrive(gdrive.clone().into());
    ui.set_cloud_dropbox(dropbox.clone().into());
    ui.set_cloud_google_client_id(panel.google_client_id.clone().into());
    ui.set_cloud_google_secret(panel.google_client_secret.clone().into());
    ui.set_cloud_onedrive_client_id(panel.onedrive_client_id.clone().into());
    ui.set_cloudflare_token(panel.cloudflare_token.clone().into());
    let tokens = cloud::load_tokens();
    ui.set_cloud_google_email(tokens.google.as_ref().map(|t| t.email.clone()).unwrap_or_default().into());
    ui.set_cloud_onedrive_email(tokens.onedrive.as_ref().map(|t| t.email.clone()).unwrap_or_default().into());
    let target = if !panel.cloud_target.is_empty() {
        panel.cloud_target.clone()
    } else if tokens.google.is_some() {
        "google".into()
    } else if tokens.onedrive.is_some() {
        "onedrive".into()
    } else {
        "folder".into()
    };
    ui.set_cloud_target(target.into());
    let folder = if !panel.cloud_folder.is_empty() {
        panel.cloud_folder.clone()
    } else if !onedrive.is_empty() {
        onedrive
    } else if !gdrive.is_empty() {
        gdrive
    } else {
        dropbox
    };
    ui.set_cloud_folder(folder.into());

    let processes = Arc::new(Mutex::new(HashMap::<String, ServerProcess>::new()));
    let active_server_id = Arc::new(Mutex::new(String::new()));
    let backup_busy: Arc<Mutex<HashSet<String>>> = Arc::new(Mutex::new(HashSet::new()));
    let backup_last: Arc<Mutex<HashMap<String, SystemTime>>> = Arc::new(Mutex::new(HashMap::new()));

    let _ = fs::create_dir_all("servers");
    let mut current_servers = load_servers_list();
    if current_servers.is_empty() {
        let default_srv = ServerMetadata {
            id: "survival".to_string(),
            name: "Survival Dünyası".to_string(),
            software: "Purpur".to_string(),
            version: "26.2".to_string(),
            port: "25565".to_string(),
            ram: "2G".to_string(),
            selected_java: "java25".to_string(),
            timezone: "Europe/Istanbul".to_string(),
            join_host: String::new(),
            backup_every_min: 0,
        };
        let target_dir = get_server_dir(&default_srv.id);
        let _ = fs::create_dir_all(&target_dir);

        let old_jar = Path::new("server").join("server.jar");
        let new_jar = target_dir.join("server.jar");
        if old_jar.exists() && !new_jar.exists() {
            let _ = fs::copy(&old_jar, &new_jar);
        }

        let old_eula = Path::new("server").join("eula.txt");
        let new_eula = target_dir.join("eula.txt");
        if old_eula.exists() && !new_eula.exists() {
            let _ = fs::copy(&old_eula, &new_eula);
        }

        current_servers.push(default_srv);
        save_servers_list(&current_servers);
    }

    let sync_servers_to_ui = |ui: &MainWindow, procs: &HashMap<String, ServerProcess>| {
        let list = load_servers_list();
        let items: Vec<ServerCardItem> = list.into_iter().map(|s| {
            let is_running = procs.get(&s.id).map(|p| p.child.is_some()).unwrap_or(false) || tcp_port_open(&s.port);
            ServerCardItem {
                id: s.id.into(),
                name: s.name.into(),
                software: s.software.into(),
                version: s.version.into(),
                port: s.port.into(),
                ram: s.ram.into(),
                status: if is_running { "Çalışıyor".into() } else { "Durduruldu".into() },
                is_running,
            }
        }).collect();
        ui.set_servers_list(std::rc::Rc::new(slint::VecModel::from(items)).into());
    };

    {
        let p = processes.lock().unwrap();
        sync_servers_to_ui(&ui, &p);
    }

    sync_java_status(&ui, "java25");
    apply_join(&ui, "25565");

    let ui_boot = ui.as_weak();
    thread::spawn(move || {
        let release = http_json::<GithubRelease>(
            http_client(),
            "https://api.github.com/repos/Q9550xRX570/PanelMC/releases/latest",
        ).ok().filter(|rel| newer_release(env!("CARGO_PKG_VERSION"), &rel.tag_name));
        if let Some(ip) = fetch_public_ip() {
            set_public_ip(ip);
        } else {
            mark_wan_tried();
        }
        let _ = slint::invoke_from_event_loop(move || {
            let Some(ui) = ui_boot.upgrade() else { return };
            if let Some(rel) = release {
                let label = if ui.get_app_lang() == "tr" {
                    format!("Güncelle {}", rel.tag_name)
                } else {
                    format!("Update {}", rel.tag_name)
                };
                ui.set_update_label(label.into());
                ui.set_update_url(rel.html_url.into());
                ui.set_update_available(true);
            }
            apply_join(&ui, &ui.get_server_port());
        });
    });

    let ui_upd = ui.as_weak();
    ui.on_open_update(move || {
        if let Some(ui) = ui_upd.upgrade() {
            open_url(&ui.get_update_url());
        }
    });

    let ui_copy = ui.as_weak();
    ui.on_copy_text(move |text| {
        let value = text.to_string();
        copy_text(&value);
        if let Some(ui) = ui_copy.upgrade() {
            let msg = if ui.get_app_lang() == "tr" { "Adres kopyalandı" } else { "Address copied" };
            ui.set_join_status(msg.into());
        }
    });

    let ui_refresh_join = ui.as_weak();
    ui.on_refresh_join(move || {
        let ui_thread = ui_refresh_join.clone();
        thread::spawn(move || {
            if let Some(ip) = fetch_public_ip() {
                set_public_ip(ip);
            } else {
                mark_wan_tried();
            }
            let _ = slint::invoke_from_event_loop(move || {
                if let Some(ui) = ui_thread.upgrade() {
                    apply_join(&ui, &ui.get_server_port());
                }
            });
        });
    });

    let ui_dns = ui.as_weak();
    let act_id_dns = Arc::clone(&active_server_id);
    ui.on_save_join_dns(move |host, token| {
        let Some(ui) = ui_dns.upgrade() else { return };
        let current_id = act_id_dns.lock().unwrap().clone();
        if current_id.is_empty() { return; }
        let host = host.trim().to_string();
        if !host.is_empty() && valid_hostname(&host).is_err() {
            ui.set_join_status("Alan adı geçersiz. Örnek: mc.duckdns.org".into());
            return;
        }
        let mut list = load_servers_list();
        if let Some(srv) = list.iter_mut().find(|s| s.id == current_id) {
            srv.join_host = host.clone();
        }
        save_servers_list(&list);
        ui.set_join_host(host.into());
        ui.set_cloudflare_token(token.clone());
        let mut panel = load_panel_settings();
        panel.cloudflare_token = token.to_string();
        save_panel_settings(&panel);
        apply_join(&ui, &ui.get_server_port());
        ui.set_join_status(if ui.get_app_lang() == "tr" { "Adres kaydedildi".into() } else { "Address saved".into() });
    });

    let ui_srv = ui.as_weak();
    let act_id_srv = Arc::clone(&active_server_id);
    ui.on_publish_srv(move || {
        let Some(ui) = ui_srv.upgrade() else { return };
        let current_id = act_id_srv.lock().unwrap().clone();
        if current_id.is_empty() { return; }
        let host = match valid_hostname(&ui.get_join_host()) {
            Ok(host) => host,
            Err(e) => {
                ui.set_join_status(e.into());
                return;
            }
        };
        let token = ui.get_cloudflare_token().trim().to_string();
        if token.is_empty() {
            ui.set_join_status(if ui.get_app_lang() == "tr" {
                "Cloudflare token yok. DNS → Edit izni olan bir token yapıştır.".into()
            } else {
                "Paste a Cloudflare token with DNS edit permission.".into()
            });
            return;
        }
        let port: u16 = valid_port(&ui.get_server_port()).parse().unwrap_or(25565);
        let lang_tr = ui.get_app_lang() == "tr";
        ui.set_join_status(if lang_tr { "SRV yazılıyor...".into() } else { "Writing SRV...".into() });
        let ui_thread = ui_srv.clone();
        thread::spawn(move || {
            let ip = fetch_public_ip();
            let result = match ip {
                Some(ip) => {
                    set_public_ip(ip.clone());
                    publish_minecraft_dns(&token, &host, &ip, port)
                }
                None => Err(if lang_tr { "Dış IP alınamadı.".into() } else { "Public IP unavailable.".into() }),
            };
            let _ = slint::invoke_from_event_loop(move || {
                let Some(ui) = ui_thread.upgrade() else { return };
                match result {
                    Ok(msg) => ui.set_join_status(msg.into()),
                    Err(e) => ui.set_join_status(e.into()),
                }
                apply_join(&ui, &port.to_string());
            });
        });
    });

    // --- 1. DAHİLİ JAVA İNDİRME MOTORU ---
    let ui_java_dl = ui.as_weak();
    ui.on_download_java_version(move |version| {
        let ui = match ui_java_dl.upgrade() { Some(u) => u, None => return };

        ui.set_is_downloading_java(true);
        ui.set_java_download_progress(0.0);
        ui.set_java_download_details(format!("Adoptium OpenJDK {} indiriliyor...", version).into());

        let ui_thread = ui_java_dl.clone();

        thread::spawn(move || {
            let client = http_client_long();

            let (os, arch) = match adoptium_os_arch() {
                Ok(v) => v,
                Err(e) => {
                    let ui_t = ui_thread.clone();
                    let _ = slint::invoke_from_event_loop(move || {
                        if let Some(ui) = ui_t.upgrade() {
                            ui.set_is_downloading_java(false);
                            ui.set_java_download_details(e.into());
                        }
                    });
                    return;
                }
            };
            let url = format!(
                "https://api.adoptium.net/v3/binary/latest/{}/ga/{}/{}/jdk/hotspot/normal/eclipse?project=jdk",
                version, os, arch
            );

            let runtimes_dir = Path::new("runtimes");
            let _ = fs::create_dir_all(runtimes_dir);
            let zip_path = if cfg!(windows) {
                runtimes_dir.join(format!("java{}.zip", version))
            } else {
                runtimes_dir.join(format!("java{}.tar.gz", version))
            };
            let target_extract = runtimes_dir.join(format!("java{}", version));

            let result: Result<PathBuf, String> = (|| {
                let mut response = client.get(&url).send().map_err(|e| e.to_string())?;
                if !response.status().is_success() {
                    return Err(format!("Java indirilemedi (HTTP {}).", response.status()));
                }
                let total_size = response.content_length().unwrap_or(0);

                let mut dest_file = File::create(&zip_path).map_err(|e| e.to_string())?;
                let mut downloaded: u64 = 0;
                let mut buffer = [0u8; 65536];
                let mut last_update = Instant::now();

                loop {
                    let n = response.read(&mut buffer).map_err(|e| e.to_string())?;
                    if n == 0 { break; }
                    dest_file.write_all(&buffer[..n]).map_err(|e| e.to_string())?;
                    downloaded += n as u64;

                    if last_update.elapsed().as_millis() >= 100 {
                        last_update = Instant::now();
                        let progress = if total_size > 0 { downloaded as f32 / total_size as f32 } else { 0.0 };
                        let dl_mb = downloaded as f64 / (1024.0 * 1024.0);
                        let total_mb = total_size as f64 / (1024.0 * 1024.0);
                        let details = format!("{:.1} MB / {:.1} MB (%{:.0})", dl_mb, total_mb, progress * 100.0);

                        let ui_t = ui_thread.clone();
                        let _ = slint::invoke_from_event_loop(move || {
                            if let Some(ui) = ui_t.upgrade() {
                                ui.set_java_download_progress(progress);
                                ui.set_java_download_details(details.into());
                            }
                        });
                    }
                }
                drop(dest_file);

                if downloaded < 1_000_000 {
                    return Err("Java arşivi eksik indi.".into());
                }

                let _ = fs::create_dir_all(&target_extract);
                extract_jdk_archive(&zip_path, &target_extract)?;
                let _ = fs::remove_file(&zip_path);

                if let Some(exe) = find_java_in_dir(&target_extract) {
                    Ok(exe)
                } else {
                    Err(format!("{} çıkarılamadı.", java_bin()))
                }
            })();

            if result.is_err() {
                let _ = fs::remove_file(&zip_path);
            }

            let pref = format!("java{version}");
            let _ = slint::invoke_from_event_loop(move || {
                if let Some(ui) = ui_thread.upgrade() {
                    ui.set_is_downloading_java(false);
                    match result {
                        Ok(_) => {
                            sync_java_status(&ui, &pref);
                            ui.set_java_download_details("Kurulum Tamamlandı! ✓".into());
                            ui.set_server_error_message("".into());
                        }
                        Err(e) => {
                            ui.set_java_download_details(format!("Hata: {}", e).into());
                        }
                    }
                }
            });
        });
    });

    // --- 2. JAVA SEÇİMİ ---
    let ui_set_java = ui.as_weak();
    let act_id_sjava = Arc::clone(&active_server_id);
    ui.on_set_active_server_java(move |java_ver| {
        let ui = match ui_set_java.upgrade() { Some(u) => u, None => return };
        let current_id = act_id_sjava.lock().unwrap().clone();
        if current_id.is_empty() { return; }

        let java_str = java_ver.to_string();
        ui.set_active_server_selected_java(java_str.clone().into());

        let mut list = load_servers_list();
        if let Some(srv) = list.iter_mut().find(|s| s.id == current_id) {
            srv.selected_java = java_str.clone();
        }
        save_servers_list(&list);

        sync_java_status(&ui, &java_str);
    });

    // --- 3. SUNUCU SEÇ VE YÖNET ---
    let ui_manage = ui.as_weak();
    let act_id_manage = Arc::clone(&active_server_id);
    let procs_manage = Arc::clone(&processes);
    ui.on_select_and_manage_server(move |id| {
        let ui = match ui_manage.upgrade() { Some(u) => u, None => return };
        let id_str = id.to_string();

        *act_id_manage.lock().unwrap() = id_str.clone();
        ui.set_active_server_id(id_str.clone().into());

        let list = load_servers_list();
        if let Some(srv) = list.into_iter().find(|s| s.id == id_str) {
            ui.set_active_server_name(srv.name.into());
            ui.set_selected_software(srv.software.into());
            ui.set_selected_version(srv.version.into());
            ui.set_server_port(srv.port.clone().into());
            ui.set_join_host(srv.join_host.clone().into());
            ui.set_server_ram(srv.ram.clone().into());
            apply_ram_to_ui(&ui, &srv.ram);
            apply_join(&ui, &srv.port);
            ui.set_active_server_selected_java(srv.selected_java.clone().into());
            ui.set_prop_timezone(srv.timezone.clone().into());
            ui.set_backup_every(backup_every_min(srv.backup_every_min) as i32);

            sync_java_status(&ui, &srv.selected_java);

            let p = procs_manage.lock().unwrap();
            let child_alive = p.get(&id_str).map(|proc| proc.child.is_some()).unwrap_or(false);
            let is_run = child_alive || tcp_port_open(&srv.port);
            ui.set_server_running(is_run);
            ui.set_server_status(if is_run { "Çalışıyor".into() } else { "Durduruldu".into() });

            if let Some(proc) = p.get(&id_str) {
                let mut full = String::new();
                for l in &proc.logs { full.push_str(l); full.push('\n'); }
                ui.set_console_text(full.into());
            } else {
                ui.set_console_text("[Sistem] Hazır.\n".into());
            }
        }

        let files_model: slint::ModelRc<FileItem> = std::rc::Rc::new(slint::VecModel::from(get_server_files_of(&id_str))).into();
        ui.set_server_files(files_model);
        let players_model: slint::ModelRc<PlayerItem> = std::rc::Rc::new(slint::VecModel::from(get_players_of(&id_str))).into();
        ui.set_player_list(players_model);
        let worlds_model: slint::ModelRc<WorldItem> = std::rc::Rc::new(slint::VecModel::from(get_worlds_of(&id_str))).into();
        ui.set_world_list(worlds_model);
        let backups_model: slint::ModelRc<BackupItem> = std::rc::Rc::new(slint::VecModel::from(get_backups_of(&id_str))).into();
        ui.set_backup_list(backups_model);
        ui.set_log_file_text(read_latest_log(&id_str).into());
        ui.set_player_status("".into());
        ui.set_world_status("".into());

        ui.set_server_error_message("".into());
        ui.set_in_server_view(true);
        ui.set_active_tab(0);
    });

    // --- 4. YENİ SUNUCU OLUŞTUR ---
    let ui_create = ui.as_weak();
    let procs_create = Arc::clone(&processes);
    ui.on_create_new_server(move |name, port| {
        let ui = match ui_create.upgrade() { Some(u) => u, None => return };
        let name_str = name.trim().to_string();
        if name_str.is_empty() { return; }

        let port_str = valid_port(&port);
        let mut list = load_servers_list();

        let id = format!("server-{}", list.len() + 1);
        let new_srv = ServerMetadata {
            id: id.clone(),
            name: name_str,
            software: "Purpur".to_string(),
            version: "26.2".to_string(),
            port: port_str.clone(),
            ram: "2G".to_string(),
            selected_java: "java25".to_string(),
            timezone: "Europe/Istanbul".to_string(),
            join_host: String::new(),
            backup_every_min: 0,
        };

        let _ = fs::create_dir_all(get_server_dir(&id));
        let mut port_map = HashMap::new();
        port_map.insert("server-port".to_string(), port_str);
        let _ = save_server_properties_of(&id, &port_map);
        list.push(new_srv);
        save_servers_list(&list);

        let p = procs_create.lock().unwrap();
        sync_servers_to_ui(&ui, &p);
    });

    // --- 5. SUNUCU SİL ---
    let ui_del = ui.as_weak();
    let procs_del = Arc::clone(&processes);
    ui.on_delete_server(move |id| {
        let ui = match ui_del.upgrade() { Some(u) => u, None => return };
        let id_str = id.to_string();

        let mut list = load_servers_list();
        list.retain(|s| s.id != id_str);
        save_servers_list(&list);

        let _ = fs::remove_dir_all(get_server_dir(&id_str));

        let p = procs_del.lock().unwrap();
        sync_servers_to_ui(&ui, &p);
    });

    // --- 6. SUNUCULARI YENİLE ---
    let ui_ref_srv = ui.as_weak();
    let procs_ref = Arc::clone(&processes);
    ui.on_refresh_servers_list(move || {
        if let Some(ui) = ui_ref_srv.upgrade() {
            let p = procs_ref.lock().unwrap();
            sync_servers_to_ui(&ui, &p);
        }
    });

    // --- 7. GÖRSEL 22 SEÇENEKLERİ YÜKLE ---
    let ui_load_props = ui.as_weak();
    let act_id_props = Arc::clone(&active_server_id);
    ui.on_load_server_properties(move || {
        let ui = match ui_load_props.upgrade() { Some(u) => u, None => return };
        let current_id = act_id_props.lock().unwrap().clone();
        if current_id.is_empty() { return; }

        let props = read_server_properties_of(&current_id);
        if let Some(v) = props.get("online-mode") { ui.set_prop_online_mode(v == "true"); }
        if let Some(v) = props.get("pvp") { ui.set_prop_pvp(v == "true"); }
        if let Some(v) = props.get("allow-flight") { ui.set_prop_flight(v == "true"); }
        if let Some(v) = props.get("difficulty") { ui.set_prop_difficulty(v.clone().into()); }
        if let Some(v) = props.get("gamemode") { ui.set_prop_gamemode(v.clone().into()); }
        if let Some(v) = props.get("white-list") { ui.set_prop_whitelist(v == "true"); }
        if let Some(v) = props.get("max-players") { if let Ok(n) = v.parse::<i32>() { ui.set_prop_max_players(n); } }
        if let Some(v) = props.get("spawn-protection") { if let Ok(n) = v.parse::<i32>() { ui.set_prop_spawn_protection(n); } }
        if let Some(v) = props.get("resource-pack") { ui.set_prop_resource_pack(v.clone().into()); }
        if let Some(v) = props.get("resource-pack-prompt") { ui.set_prop_resource_pack_prompt(v.clone().into()); }
        if let Some(v) = props.get("resource-pack-sha1") { ui.set_prop_resource_pack_sha1(v.clone().into()); }
        if let Some(v) = props.get("force-gamemode") { ui.set_prop_force_gamemode(v == "true"); }
        if let Some(v) = props.get("require-resource-pack") { ui.set_prop_require_resource_pack(v == "true"); }
        let list = load_servers_list();
        if let Some(srv) = list.iter().find(|s| s.id == current_id) {
            apply_ram_to_ui(&ui, &srv.ram);
            ui.set_prop_timezone(srv.timezone.clone().into());
        }
        ui.set_prop_save_status("".into());
    });

    // --- 8. GÖRSEL 22 SEÇENEKLERİ KAYDET ---
    let ui_save_props = ui.as_weak();
    let act_id_save = Arc::clone(&active_server_id);
    ui.on_save_server_properties(move || {
        let ui = match ui_save_props.upgrade() { Some(u) => u, None => return };
        let current_id = act_id_save.lock().unwrap().clone();
        if current_id.is_empty() { return; }

        let mut map = HashMap::new();
        map.insert("online-mode".to_string(), ui.get_prop_online_mode().to_string());
        map.insert("pvp".to_string(), ui.get_prop_pvp().to_string());
        map.insert("allow-flight".to_string(), ui.get_prop_flight().to_string());
        map.insert("difficulty".to_string(), ui.get_prop_difficulty().to_string());
        map.insert("gamemode".to_string(), ui.get_prop_gamemode().to_string());
        map.insert("white-list".to_string(), ui.get_prop_whitelist().to_string());
        map.insert("max-players".to_string(), ui.get_prop_max_players().to_string());
        map.insert("spawn-protection".to_string(), ui.get_prop_spawn_protection().to_string());
        map.insert("resource-pack".to_string(), ui.get_prop_resource_pack().to_string());
        map.insert("resource-pack-prompt".to_string(), ui.get_prop_resource_pack_prompt().to_string());
        map.insert("resource-pack-sha1".to_string(), ui.get_prop_resource_pack_sha1().to_string());
        map.insert("force-gamemode".to_string(), ui.get_prop_force_gamemode().to_string());
        map.insert("require-resource-pack".to_string(), ui.get_prop_require_resource_pack().to_string());

        match save_server_properties_of(&current_id, &map) {
            Ok(_) => {
                persist_ram_for(&current_id, &ui.get_ram_amount(), &ui.get_ram_unit());
                apply_ram_to_ui(&ui, &ram_stored(&ui.get_ram_amount(), &ui.get_ram_unit()));
                persist_timezone_for(&current_id, &ui.get_prop_timezone());
                ui.set_prop_save_status("Ayarlar kaydedildi! ✓".into());
            }
            Err(e) => ui.set_prop_save_status(format!("Hata: {}", e).into()),
        }
    });

    let ui_rpack = ui.as_weak();
    let act_id_rpack = Arc::clone(&active_server_id);
    ui.on_pick_resource_pack(move || {
        let ui = match ui_rpack.upgrade() { Some(u) => u, None => return };
        let current_id = act_id_rpack.lock().unwrap().clone();
        if current_id.is_empty() { return; }
        let Some(src) = pick_zip_dialog() else { return; };
        let dest = get_server_dir(&current_id).join("resourcepack.zip");
        match copy_file_buffered(Path::new(&src), &dest) {
            Ok(()) => {
                let sha = fs::read(&dest).map(|b| sha1_hex(&b)).unwrap_or_default();
                ui.set_prop_resource_pack_sha1(sha.clone().into());
                if ui.get_prop_resource_pack().is_empty() {
                    ui.set_prop_resource_pack("resourcepack.zip".into());
                }
                ui.set_prop_save_status(format!("Paket kopyalandı (SHA1 {}). Kaydet'e bas.", if sha.len() > 8 { &sha[..8] } else { &sha }).into());
            }
            Err(e) => ui.set_prop_save_status(format!("Paket yüklenemedi: {}", e).into()),
        }
    });

    // --- 9. KLASÖRÜ AÇ ---
    let act_id_folder = Arc::clone(&active_server_id);
    ui.on_open_server_folder(move || {
        let current_id = act_id_folder.lock().unwrap().clone();
        let path = get_server_dir(&current_id);
        open_path(&path);
    });

    // --- 10. YAZILIM SÜRÜMLERİ ---
    let ui_soft = ui.as_weak();
    ui.on_open_software_versions(move |software| {
        let ui = match ui_soft.upgrade() { Some(u) => u, None => return };
        let soft_str = software.to_string();
        ui.set_selected_software(soft_str.clone().into());
        ui.set_software_subview(1);

        let ui_t = ui_soft.clone();
        thread::spawn(move || {
            let versions = fetch_versions_for_software(&soft_str);
            let note = if versions.is_empty() {
                match soft_str.as_str() {
                    "Forge" | "NeoForge" => "Forge ve NeoForge bu sürümde kurulmuyor. Fabric kullan.".to_string(),
                    _ => "Sürüm listesi alınamadı.".to_string(),
                }
            } else {
                "Hazır".to_string()
            };
            let rows = format_version_rows(versions);
            let _ = slint::invoke_from_event_loop(move || {
                if let Some(ui) = ui_t.upgrade() {
                    let model: slint::ModelRc<VersionRow> = std::rc::Rc::new(slint::VecModel::from(rows)).into();
                    ui.set_version_rows(model);
                    ui.set_download_status(note.into());
                }
            });
        });
    });

    // --- 11. SÜRÜM İNDİR VE KUR ---
    let ui_install = ui.as_weak();
    let act_id_install = Arc::clone(&active_server_id);
    ui.on_select_version_and_install(move |software, version| {
        let ui = match ui_install.upgrade() { Some(u) => u, None => return };
        let current_id = act_id_install.lock().unwrap().clone();
        if current_id.is_empty() { return; }

        ui.set_selected_version(version.clone());
        ui.set_is_downloading(true);
        ui.set_download_progress(0.0);
        ui.set_download_details("Bağlantı kuruluyor...".into());
        ui.set_download_status(format!("{} {} indiriliyor...", software, version).into());

        let ui_thread = ui_install.clone();
        let software_str = software.to_string();
        let version_str = version.to_string();
        let target_dir = get_server_dir(&current_id);

        thread::spawn(move || {
            let client = http_client_long();
            let jar_dest = target_dir.join("server.jar");
            let partial = target_dir.join("server.jar.partial");

            let result: Result<(), String> = (|| {
                let target_url = resolve_server_url(&client, &software_str, &version_str)?;
                let mut response = client.get(&target_url).send().map_err(|e| e.to_string())?;
                if !response.status().is_success() {
                    return Err(format!("Jar indirilemedi (HTTP {}).", response.status()));
                }
                let total_size = response.content_length().unwrap_or(0);

                let mut dest_file = File::create(&partial).map_err(|e| e.to_string())?;
                let mut downloaded: u64 = 0;
                let mut buffer = [0u8; 32768];
                let mut last_update = Instant::now();

                loop {
                    let bytes_read = response.read(&mut buffer).map_err(|e| e.to_string())?;
                    if bytes_read == 0 { break; }
                    dest_file.write_all(&buffer[..bytes_read]).map_err(|e| e.to_string())?;
                    downloaded += bytes_read as u64;

                    if last_update.elapsed().as_millis() >= 100 {
                        last_update = Instant::now();
                        let progress = if total_size > 0 { downloaded as f32 / total_size as f32 } else { 0.0 };
                        let dl_mb = downloaded as f64 / (1024.0 * 1024.0);
                        let total_mb = total_size as f64 / (1024.0 * 1024.0);
                        let details = format!("{:.1} MB / {:.1} MB (%{:.0})", dl_mb, total_mb, progress * 100.0);

                        let ui_t = ui_thread.clone();
                        let _ = slint::invoke_from_event_loop(move || {
                            if let Some(ui) = ui_t.upgrade() {
                                ui.set_download_progress(progress);
                                ui.set_download_details(details.into());
                            }
                        });
                    }
                }
                drop(dest_file);
                if downloaded < 100_000 {
                    return Err("İnen dosya sunucu jar'ı değil. Kurulum iptal edildi.".into());
                }
                if jar_dest.exists() {
                    fs::remove_file(&jar_dest).map_err(|e| e.to_string())?;
                }
                fs::rename(&partial, &jar_dest).map_err(|e| e.to_string())?;

                let eula_file = target_dir.join("eula.txt");
                let mut eula = File::create(eula_file).map_err(|e| e.to_string())?;
                writeln!(eula, "eula=true").map_err(|e| e.to_string())?;

                Ok(())
            })();

            if result.is_err() {
                let _ = fs::remove_file(&partial);
            }

            let software_done = software_str.clone();
            let version_done = version_str.clone();
            let _ = slint::invoke_from_event_loop(move || {
                if let Some(ui) = ui_thread.upgrade() {
                    ui.set_is_downloading(false);
                    match result {
                        Ok(_) => {
                            let mut list = load_servers_list();
                            if let Some(srv) = list.iter_mut().find(|s| s.id == current_id) {
                                srv.software = software_done;
                                srv.version = version_done;
                            }
                            save_servers_list(&list);
                            ui.set_download_progress(1.0);
                            ui.set_download_details("Kurulum tamamlandı!".into());
                            ui.set_download_status("Başarıyla Kuruldu ✓ (EULA Onaylandı)".into());
                            ui.set_server_error_message("".into());
                            let files_model: slint::ModelRc<FileItem> = std::rc::Rc::new(slint::VecModel::from(get_server_files_of(&current_id))).into();
                            ui.set_server_files(files_model);
                        }
                        Err(e) => {
                            ui.set_download_status(format!("Hata: {}", e).into());
                            ui.set_download_details("".into());
                        }
                    }
                }
            });
        });
    });

    // --- 12. DOSYALARI YENİLE ---
    let ui_files = ui.as_weak();
    let act_id_files = Arc::clone(&active_server_id);
    ui.on_refresh_files(move || {
        if let Some(ui) = ui_files.upgrade() {
            let current_id = act_id_files.lock().unwrap().clone();
            let files_model: slint::ModelRc<FileItem> = std::rc::Rc::new(slint::VecModel::from(get_server_files_of(&current_id))).into();
            ui.set_server_files(files_model);
        }
    });

    let ui_players = ui.as_weak();
    let act_id_players = Arc::clone(&active_server_id);
    ui.on_refresh_players(move || {
        if let Some(ui) = ui_players.upgrade() {
            let current_id = act_id_players.lock().unwrap().clone();
            let model: slint::ModelRc<PlayerItem> = std::rc::Rc::new(slint::VecModel::from(get_players_of(&current_id))).into();
            ui.set_player_list(model);
        }
    });

    let ui_pact = ui.as_weak();
    let act_id_pact = Arc::clone(&active_server_id);
    let procs_pact = Arc::clone(&processes);
    ui.on_player_action(move |action, name| {
        let ui = match ui_pact.upgrade() { Some(u) => u, None => return };
        let current_id = act_id_pact.lock().unwrap().clone();
        if current_id.is_empty() { return; }
        let name = name.trim().to_string();
        if name.is_empty() {
            ui.set_player_status("Oyuncu adı girin.".into());
            return;
        }
        let action = action.to_string();
        let dir = get_server_dir(&current_id);
        let uuid = find_player_uuid(&dir, &name);
        let cmd = match action.as_str() {
            "op" => format!("op {}", name),
            "deop" => format!("deop {}", name),
            "kick" => format!("kick {}", name),
            "ban" => format!("ban {}", name),
            "pardon" => format!("pardon {}", name),
            "wl-add" => format!("whitelist add {}", name),
            "wl-remove" => format!("whitelist remove {}", name),
            _ => String::new(),
        };
        if cmd.is_empty() { return; }

        let sent = {
            let mut procs = procs_pact.lock().unwrap();
            if let Some(proc) = procs.get_mut(&current_id) {
                if let Some(ref mut stdin) = proc.stdin {
                    let _ = writeln!(stdin, "{}", cmd);
                    let _ = stdin.flush();
                    proc.append_log(format!("> {}", cmd));
                    true
                } else { false }
            } else { false }
        };

        match action.as_str() {
            "op" => upsert_named_file(&dir.join("ops.json"), &name, &uuid, true),
            "deop" => remove_named_file(&dir.join("ops.json"), &name),
            "wl-add" => upsert_named_file(&dir.join("whitelist.json"), &name, &uuid, false),
            "wl-remove" => remove_named_file(&dir.join("whitelist.json"), &name),
            _ => {}
        }

        let needs_live = matches!(action.as_str(), "kick" | "ban" | "pardon");
        if needs_live && !sent {
            ui.set_player_status("Sunucu kapalı. Kick/Ban için başlatın.".into());
        } else if sent {
            ui.set_player_status(format!("{} gönderildi.", cmd).into());
        } else {
            ui.set_player_status("Yerel liste güncellendi. Komut için sunucuyu başlatın.".into());
        }
        let model: slint::ModelRc<PlayerItem> = std::rc::Rc::new(slint::VecModel::from(get_players_of(&current_id))).into();
        ui.set_player_list(model);
    });

    let ui_worlds = ui.as_weak();
    let act_id_worlds = Arc::clone(&active_server_id);
    ui.on_refresh_worlds(move || {
        if let Some(ui) = ui_worlds.upgrade() {
            let current_id = act_id_worlds.lock().unwrap().clone();
            let worlds: slint::ModelRc<WorldItem> = std::rc::Rc::new(slint::VecModel::from(get_worlds_of(&current_id))).into();
            ui.set_world_list(worlds);
            let backups: slint::ModelRc<BackupItem> = std::rc::Rc::new(slint::VecModel::from(get_backups_of(&current_id))).into();
            ui.set_backup_list(backups);
        }
    });

    let ui_logs = ui.as_weak();
    let act_id_logs = Arc::clone(&active_server_id);
    ui.on_refresh_logs(move || {
        if let Some(ui) = ui_logs.upgrade() {
            let current_id = act_id_logs.lock().unwrap().clone();
            ui.set_log_file_text(read_latest_log(&current_id).into());
        }
    });

    let ui_every = ui.as_weak();
    let act_id_every = Arc::clone(&active_server_id);
    let last_every = Arc::clone(&backup_last);
    ui.on_set_backup_every(move |minutes| {
        let Some(ui) = ui_every.upgrade() else { return };
        let current_id = act_id_every.lock().unwrap().clone();
        if current_id.is_empty() { return; }
        let every = backup_every_min(minutes.max(0) as u32);
        ui.set_backup_every(every as i32);
        let mut list = load_servers_list();
        if let Some(srv) = list.iter_mut().find(|s| s.id == current_id) {
            srv.backup_every_min = every;
        }
        save_servers_list(&list);
        if every == 0 {
            last_every.lock().unwrap().remove(&current_id);
        } else {
            last_every.lock().unwrap().insert(current_id, SystemTime::now());
        }
        let msg = if every == 0 {
            if ui.get_app_lang() == "tr" { "Otomatik yedek kapalı." } else { "Automatic backup is off." }
        } else if ui.get_app_lang() == "tr" {
            "Otomatik yedek kaydedildi. İlk yedek bir aralık sonra."
        } else {
            "Automatic backup saved. The first one waits one interval."
        };
        ui.set_world_status(msg.into());
    });

    let ui_bak = ui.as_weak();
    let act_id_bak = Arc::clone(&active_server_id);
    let busy_bak = Arc::clone(&backup_busy);
    ui.on_backup_worlds(move || {
        let ui = match ui_bak.upgrade() { Some(u) => u, None => return };
        let current_id = act_id_bak.lock().unwrap().clone();
        if current_id.is_empty() { return; }
        if ui.get_is_backing_up() { return; }
        if !busy_bak.lock().unwrap().insert(current_id.clone()) {
            ui.set_world_status(if ui.get_app_lang() == "tr" { "Yedek zaten alınıyor." } else { "A backup is already running." }.into());
            return;
        }
        ui.set_is_backing_up(true);
        ui.set_world_status("Yedek alınıyor...".into());
        let ui_t = ui_bak.clone();
        let busy_t = Arc::clone(&busy_bak);
        thread::spawn(move || {
            let result = backup_worlds_of(&current_id);
            busy_t.lock().unwrap().remove(&current_id);
            let id_copy = current_id.clone();
            let _ = slint::invoke_from_event_loop(move || {
                if let Some(ui) = ui_t.upgrade() {
                    ui.set_is_backing_up(false);
                    match result {
                        Ok(name) => ui.set_world_status(format!("{} kaydedildi.", name).into()),
                        Err(e) => ui.set_world_status(e.into()),
                    }
                    let worlds: slint::ModelRc<WorldItem> = std::rc::Rc::new(slint::VecModel::from(get_worlds_of(&id_copy))).into();
                    ui.set_world_list(worlds);
                    let backups: slint::ModelRc<BackupItem> = std::rc::Rc::new(slint::VecModel::from(get_backups_of(&id_copy))).into();
                    ui.set_backup_list(backups);
                }
            });
        });
    });

    let ui_reset = ui.as_weak();
    let act_id_reset = Arc::clone(&active_server_id);
    let procs_reset = Arc::clone(&processes);
    ui.on_reset_world(move |name| {
        let ui = match ui_reset.upgrade() { Some(u) => u, None => return };
        let current_id = act_id_reset.lock().unwrap().clone();
        if current_id.is_empty() { return; }
        if server_online_now(&procs_reset.lock().unwrap(), &current_id) {
            ui.set_world_status("Dünyayı sıfırlamak için sunucuyu durdurun.".into());
            return;
        }
        match reset_world_of(&current_id, name.as_str()) {
            Ok(_) => ui.set_world_status(format!("{} silindi. Sunucu yeni dünya üretecek.", name).into()),
            Err(e) => ui.set_world_status(e.into()),
        }
        let worlds: slint::ModelRc<WorldItem> = std::rc::Rc::new(slint::VecModel::from(get_worlds_of(&current_id))).into();
        ui.set_world_list(worlds);
    });

    let act_id_wfolder = Arc::clone(&active_server_id);
    ui.on_open_world_folder(move |name| {
        let current_id = act_id_wfolder.lock().unwrap().clone();
        if current_id.is_empty() || !is_safe_leaf_name(name.as_str()) { return; }
        let path = get_server_dir(&current_id).join(name.as_str());
        let _ = fs::create_dir_all(&path);
        open_path(&path);
    });

    let ui_wcreate = ui.as_weak();
    let act_id_wcreate = Arc::clone(&active_server_id);
    let procs_wcreate = Arc::clone(&processes);
    ui.on_create_world(move |name| {
        let ui = match ui_wcreate.upgrade() { Some(u) => u, None => return };
        let current_id = act_id_wcreate.lock().unwrap().clone();
        if current_id.is_empty() { return; }
        if server_online_now(&procs_wcreate.lock().unwrap(), &current_id) {
            ui.set_world_status("Yeni dünya için sunucuyu durdurun.".into());
            return;
        }
        match create_or_reset_world(&current_id, name.as_str()) {
            Ok(_) => ui.set_world_status("Dünya sıfırlandı. Sunucuyu başlatınca Overworld, sonra Nether ve End oluşur.".into()),
            Err(e) => ui.set_world_status(e.into()),
        }
        let worlds: slint::ModelRc<WorldItem> = std::rc::Rc::new(slint::VecModel::from(get_worlds_of(&current_id))).into();
        ui.set_world_list(worlds);
    });

    let ui_wupload = ui.as_weak();
    let act_id_wupload = Arc::clone(&active_server_id);
    let procs_wupload = Arc::clone(&processes);
    ui.on_upload_world(move |name| {
        let ui = match ui_wupload.upgrade() { Some(u) => u, None => return };
        let current_id = act_id_wupload.lock().unwrap().clone();
        if current_id.is_empty() { return; }
        if server_online_now(&procs_wupload.lock().unwrap(), &current_id) {
            ui.set_world_status("Dünya yüklemek için sunucuyu durdurun.".into());
            return;
        }
        let Some(zip) = pick_zip_dialog() else { return; };
        match extract_world_zip(&current_id, name.as_str(), Path::new(&zip)) {
            Ok(_) => ui.set_world_status(format!("{} yüklendi.", name).into()),
            Err(e) => ui.set_world_status(e.into()),
        }
        let worlds: slint::ModelRc<WorldItem> = std::rc::Rc::new(slint::VecModel::from(get_worlds_of(&current_id))).into();
        ui.set_world_list(worlds);
    });

    let ui_wopt = ui.as_weak();
    let act_id_wopt = Arc::clone(&active_server_id);
    let procs_wopt = Arc::clone(&processes);
    ui.on_optimize_world(move |_name, enabled| {
        let ui = match ui_wopt.upgrade() { Some(u) => u, None => return };
        let current_id = act_id_wopt.lock().unwrap().clone();
        if current_id.is_empty() { return; }
        if enabled {
            let mut procs = procs_wopt.lock().unwrap();
            if let Some(proc) = procs.get_mut(&current_id) {
                if let Some(ref mut stdin) = proc.stdin {
                    let _ = writeln!(stdin, "save-all flush");
                    let _ = stdin.flush();
                    proc.append_log("> save-all flush".into());
                    ui.set_console_text(proc.log_text.clone().into());
                    ui.set_world_status("Optimize: kayıt temizliği gönderildi.".into());
                    return;
                }
            }
            ui.set_world_status("Optimize işaretli. Sunucu açıkken kayıt sıkıştırılır.".into());
        } else {
            ui.set_world_status("Optimize kapatıldı.".into());
        }
    });

    let ui_restore = ui.as_weak();
    let act_id_restore = Arc::clone(&active_server_id);
    let procs_restore = Arc::clone(&processes);
    ui.on_restore_backup(move |name| {
        let ui = match ui_restore.upgrade() { Some(u) => u, None => return };
        let current_id = act_id_restore.lock().unwrap().clone();
        if current_id.is_empty() { return; }
        if server_online_now(&procs_restore.lock().unwrap(), &current_id) {
            ui.set_world_status("Yedeği geri yüklemek için sunucuyu durdurun.".into());
            return;
        }
        match restore_backup_of(&current_id, name.as_str()) {
            Ok(_) => ui.set_world_status(format!("{} geri yüklendi.", name).into()),
            Err(e) => ui.set_world_status(e.into()),
        }
        let worlds: slint::ModelRc<WorldItem> = std::rc::Rc::new(slint::VecModel::from(get_worlds_of(&current_id))).into();
        ui.set_world_list(worlds);
    });

    let ui_delbak = ui.as_weak();
    let act_id_delbak = Arc::clone(&active_server_id);
    ui.on_delete_backup(move |name| {
        let ui = match ui_delbak.upgrade() { Some(u) => u, None => return };
        let current_id = act_id_delbak.lock().unwrap().clone();
        if current_id.is_empty() { return; }
        match delete_backup_of(&current_id, name.as_str()) {
            Ok(_) => ui.set_world_status(format!("{} silindi.", name).into()),
            Err(e) => ui.set_world_status(e.into()),
        }
        let backups: slint::ModelRc<BackupItem> = std::rc::Rc::new(slint::VecModel::from(get_backups_of(&current_id))).into();
        ui.set_backup_list(backups);
    });

    let act_id_bakfolder = Arc::clone(&active_server_id);
    ui.on_open_backups_folder(move || {
        let current_id = act_id_bakfolder.lock().unwrap().clone();
        let path = get_server_dir(&current_id).join("backups");
        let _ = fs::create_dir_all(&path);
        open_path(&path);
    });

    let ui_cloud_save = ui.as_weak();
    ui.on_save_cloud_settings(move || {
        let ui = match ui_cloud_save.upgrade() { Some(u) => u, None => return };
        save_panel_settings(&snapshot_panel_settings(&ui));
        ui.set_cloud_status("Bulut ayarları kaydedildi.".into());
    });

    let ui_pick = ui.as_weak();
    ui.on_pick_cloud_folder(move || {
        let ui = match ui_pick.upgrade() { Some(u) => u, None => return };
        if let Some(path) = pick_directory_dialog() {
            ui.set_cloud_folder(path.into());
            ui.set_cloud_target("folder".into());
            save_panel_settings(&snapshot_panel_settings(&ui));
            ui.set_cloud_status("Klasör seçildi.".into());
        }
    });

    let ui_glogin = ui.as_weak();
    ui.on_google_login(move || {
        let ui = match ui_glogin.upgrade() { Some(u) => u, None => return };
        if ui.get_is_cloud_logging_in() { return; }
        save_panel_settings(&snapshot_panel_settings(&ui));
        let client_id = ui.get_cloud_google_client_id().to_string();
        let secret = ui.get_cloud_google_secret().to_string();
        ui.set_is_cloud_logging_in(true);
        ui.set_cloud_login_status("Google tarayıcıda açılıyor...".into());
        let ui_t = ui_glogin.clone();
        thread::spawn(move || {
            let ui_status = ui_t.clone();
            let result = cloud::google_login(&client_id, &secret, |msg| {
                let m = msg.to_string();
                let u = ui_status.clone();
                let _ = slint::invoke_from_event_loop(move || {
                    if let Some(ui) = u.upgrade() {
                        ui.set_cloud_login_status(m.into());
                    }
                });
            });
            let _ = slint::invoke_from_event_loop(move || {
                if let Some(ui) = ui_t.upgrade() {
                    ui.set_is_cloud_logging_in(false);
                    match result {
                        Ok(email) => {
                            ui.set_cloud_google_email(email.clone().into());
                            ui.set_cloud_target("google".into());
                            ui.set_cloud_login_status(format!("{} bağlandı.", email).into());
                            save_panel_settings(&snapshot_panel_settings(&ui));
                        }
                        Err(e) => ui.set_cloud_login_status(e.into()),
                    }
                }
            });
        });
    });

    let ui_glogout = ui.as_weak();
    ui.on_google_logout(move || {
        cloud::google_logout();
        if let Some(ui) = ui_glogout.upgrade() {
            ui.set_cloud_google_email("".into());
            ui.set_cloud_login_status("Google oturumu kapatıldı.".into());
            if ui.get_cloud_target() == "google" {
                ui.set_cloud_target("folder".into());
            }
            save_panel_settings(&snapshot_panel_settings(&ui));
        }
    });

    let ui_ologin = ui.as_weak();
    ui.on_onedrive_login(move || {
        let ui = match ui_ologin.upgrade() { Some(u) => u, None => return };
        if ui.get_is_cloud_logging_in() { return; }
        save_panel_settings(&snapshot_panel_settings(&ui));
        let client_id = ui.get_cloud_onedrive_client_id().to_string();
        ui.set_is_cloud_logging_in(true);
        ui.set_cloud_login_status("OneDrive için tarayıcıda kodu gir...".into());
        let ui_t = ui_ologin.clone();
        thread::spawn(move || {
            let ui_status = ui_t.clone();
            let result = cloud::onedrive_login(&client_id, |msg| {
                let m = msg.to_string();
                let u = ui_status.clone();
                let _ = slint::invoke_from_event_loop(move || {
                    if let Some(ui) = u.upgrade() {
                        ui.set_cloud_login_status(m.into());
                    }
                });
            });
            let _ = slint::invoke_from_event_loop(move || {
                if let Some(ui) = ui_t.upgrade() {
                    ui.set_is_cloud_logging_in(false);
                    match result {
                        Ok(email) => {
                            ui.set_cloud_onedrive_email(email.clone().into());
                            ui.set_cloud_target("onedrive".into());
                            ui.set_cloud_login_status(format!("{} bağlandı.", email).into());
                            save_panel_settings(&snapshot_panel_settings(&ui));
                        }
                        Err(e) => ui.set_cloud_login_status(e.into()),
                    }
                }
            });
        });
    });

    let ui_ologout = ui.as_weak();
    ui.on_onedrive_logout(move || {
        cloud::onedrive_logout();
        if let Some(ui) = ui_ologout.upgrade() {
            ui.set_cloud_onedrive_email("".into());
            ui.set_cloud_login_status("OneDrive oturumu kapatıldı.".into());
            if ui.get_cloud_target() == "onedrive" {
                ui.set_cloud_target("folder".into());
            }
            save_panel_settings(&snapshot_panel_settings(&ui));
        }
    });

    let ui_up = ui.as_weak();
    let act_id_up = Arc::clone(&active_server_id);
    ui.on_upload_backup(move |name| {
        let ui = match ui_up.upgrade() { Some(u) => u, None => return };
        let current_id = act_id_up.lock().unwrap().clone();
        if current_id.is_empty() { return; }
        if ui.get_is_uploading_cloud() { return; }
        let name = name.to_string();
        let target = ui.get_cloud_target().to_string();
        let folder = ui.get_cloud_folder().to_string();
        let gid = ui.get_cloud_google_client_id().to_string();
        let gsec = ui.get_cloud_google_secret().to_string();
        let oid = ui.get_cloud_onedrive_client_id().to_string();
        ui.set_is_uploading_cloud(true);
        ui.set_world_status("Buluta gönderiliyor...".into());
        let ui_t = ui_up.clone();
        thread::spawn(move || {
            let result = push_backup_to_cloud(&current_id, &name, &target, &folder, &gid, &gsec, &oid);
            let _ = slint::invoke_from_event_loop(move || {
                if let Some(ui) = ui_t.upgrade() {
                    ui.set_is_uploading_cloud(false);
                    match result {
                        Ok(dest) => ui.set_world_status(format!("Gönderildi: {}", dest).into()),
                        Err(e) => ui.set_world_status(e.into()),
                    }
                }
            });
        });
    });

    let ui_bu = ui.as_weak();
    let act_id_bu = Arc::clone(&active_server_id);
    let busy_bu = Arc::clone(&backup_busy);
    ui.on_backup_and_upload(move || {
        let ui = match ui_bu.upgrade() { Some(u) => u, None => return };
        let current_id = act_id_bu.lock().unwrap().clone();
        if current_id.is_empty() { return; }
        if ui.get_is_backing_up() || ui.get_is_uploading_cloud() { return; }
        if busy_bu.lock().unwrap().contains(&current_id) {
            ui.set_world_status(if ui.get_app_lang() == "tr" { "Yedek zaten alınıyor." } else { "A backup is already running." }.into());
            return;
        }
        let target = ui.get_cloud_target().to_string();
        let folder = ui.get_cloud_folder().to_string();
        let gid = ui.get_cloud_google_client_id().to_string();
        let gsec = ui.get_cloud_google_secret().to_string();
        let oid = ui.get_cloud_onedrive_client_id().to_string();
        if target == "folder" && folder.trim().is_empty() {
            ui.set_world_status("Google/OneDrive girişi yapın veya senkron klasörü seçin.".into());
            return;
        }
        if target == "google" && ui.get_cloud_google_email().is_empty() {
            ui.set_world_status("Önce Uygulama sekmesinden Google ile giriş yapın.".into());
            return;
        }
        if target == "onedrive" && ui.get_cloud_onedrive_email().is_empty() {
            ui.set_world_status("Önce Uygulama sekmesinden OneDrive ile giriş yapın.".into());
            return;
        }
        if !busy_bu.lock().unwrap().insert(current_id.clone()) {
            ui.set_world_status(if ui.get_app_lang() == "tr" { "Yedek zaten alınıyor." } else { "A backup is already running." }.into());
            return;
        }
        ui.set_is_backing_up(true);
        ui.set_is_uploading_cloud(true);
        ui.set_world_status("Yedek alınıp buluta gönderiliyor...".into());
        let ui_t = ui_bu.clone();
        let busy_t = Arc::clone(&busy_bu);
        thread::spawn(move || {
            let result = backup_worlds_of(&current_id).and_then(|name| {
                push_backup_to_cloud(&current_id, &name, &target, &folder, &gid, &gsec, &oid)
            });
            busy_t.lock().unwrap().remove(&current_id);
            let id_copy = current_id.clone();
            let _ = slint::invoke_from_event_loop(move || {
                if let Some(ui) = ui_t.upgrade() {
                    ui.set_is_backing_up(false);
                    ui.set_is_uploading_cloud(false);
                    match result {
                        Ok(dest) => ui.set_world_status(format!("Tamam: {}", dest).into()),
                        Err(e) => ui.set_world_status(e.into()),
                    }
                    let worlds: slint::ModelRc<WorldItem> = std::rc::Rc::new(slint::VecModel::from(get_worlds_of(&id_copy))).into();
                    ui.set_world_list(worlds);
                    let backups: slint::ModelRc<BackupItem> = std::rc::Rc::new(slint::VecModel::from(get_backups_of(&id_copy))).into();
                    ui.set_backup_list(backups);
                }
            });
        });
    });

    // --- 13. GÜVENLİ VE ENCODE EDİLMİŞ EKLENTİ ARAMA MOTORU ---
    let ui_mod_search = ui.as_weak();
    let search_epoch = Arc::new(AtomicU64::new(0));
    ui.on_search_mods(move |query, source| {
        let ui = match ui_mod_search.upgrade() { Some(u) => u, None => return };

        let query_trimmed = query.trim().to_string();
        let final_query = if query_trimmed.is_empty() { "server".to_string() } else { query_trimmed };

        ui.set_is_searching_mods(true);
        ui.set_mod_status_message(format!("{} taranıyor...", source).into());

        let ui_thread = ui_mod_search.clone();
        let source_str = source.to_string();
        let epoch = search_epoch.fetch_add(1, Ordering::SeqCst) + 1;
        let search_epoch_t = Arc::clone(&search_epoch);

        thread::spawn(move || {
            let cache_dir = Path::new("servers").join(".cache").join("icons");
            let _ = fs::create_dir_all(&cache_dir);

            let results: Result<Vec<TempModItem>, String> = (|| {
                let client = http_client();
                if source_str == "SpigotMC" {
                    let encoded = percent_encode_path(&final_query);
                    let resp: Vec<SpigetResource> = client
                        .get(format!("https://api.spiget.org/v2/search/resources/{encoded}"))
                        .query(&[("size", "12")])
                        .send().map_err(|e| e.to_string())?
                        .json().map_err(|e| e.to_string())?;

                    let mut items = Vec::with_capacity(resp.len());
                    for res in resp {
                        let slug = format!("spiget:{}", res.id);
                        let icon_url = res.icon.as_ref()
                            .and_then(|i| i.url.as_deref())
                            .filter(|u| !u.trim().is_empty())
                            .map(|u| u.to_string())
                            .unwrap_or_else(|| format!("https://api.spiget.org/v2/resources/{}/icon", res.id));
                        let icon_pixels = icon_from_url(client, &cache_dir, &slug, &icon_url);
                        items.push(TempModItem {
                            title: res.name,
                            description: res.tag.unwrap_or_else(|| "Spigot eklentisi.".to_string()),
                            downloads: format_download_count(res.downloads.unwrap_or(0)),
                            slug,
                            icon_pixels,
                        });
                    }
                    Ok(items)
                } else {
                    let resp: ModrinthSearchResponse = client
                        .get("https://api.modrinth.com/v2/search")
                        .query(&[("query", final_query.as_str()), ("limit", "12")])
                        .send().map_err(|e| e.to_string())?
                        .json().map_err(|e| e.to_string())?;

                    let mut items = Vec::with_capacity(resp.hits.len());
                    for hit in resp.hits {
                        let icon_pixels = hit.icon_url.as_deref()
                            .and_then(|url| icon_from_url(client, &cache_dir, &hit.slug, url));
                        items.push(TempModItem {
                            title: hit.title,
                            description: hit.description,
                            downloads: format_download_count(hit.downloads),
                            slug: hit.slug,
                            icon_pixels,
                        });
                    }
                    Ok(items)
                }
            })();

            if search_epoch_t.load(Ordering::SeqCst) != epoch {
                return;
            }

            let _ = slint::invoke_from_event_loop(move || {
                if search_epoch_t.load(Ordering::SeqCst) != epoch {
                    return;
                }
                if let Some(ui) = ui_thread.upgrade() {
                    ui.set_is_searching_mods(false);
                    match results {
                        Ok(temp_items) => {
                            let count = temp_items.len();
                            let items: Vec<ModItem> = temp_items.into_iter().map(|d| {
                                let has_icon = d.icon_pixels.is_some();
                                let icon = d.icon_pixels.map(icon_pixels_to_slint).unwrap_or_default();
                                ModItem {
                                    title: d.title.into(),
                                    description: d.description.into(),
                                    downloads: d.downloads.into(),
                                    slug: d.slug.into(),
                                    installed: false,
                                    has_icon,
                                    icon,
                                }
                            }).collect();

                            let model: slint::ModelRc<ModItem> = std::rc::Rc::new(slint::VecModel::from(items)).into();
                            ui.set_mod_results(model);
                            ui.set_mod_status_message(format!("{} sonuç bulundu.", count).into());
                        }
                        Err(e) => {
                            ui.set_mod_status_message(format!("Arama hatası: {}", e).into());
                        }
                    }
                }
            });
        });
    });

    // --- 14. EKLENTİ İNDİR ---
    let ui_mod_install = ui.as_weak();
    let act_id_mod = Arc::clone(&active_server_id);
    ui.on_install_mod(move |slug| {
        let current_id = act_id_mod.lock().unwrap().clone();
        println!("[PANEL] Eklenti indirme butonuna tıklandı: '{}' (Sunucu: {})", slug, current_id);
        if current_id.is_empty() { return; }

        let ui = match ui_mod_install.upgrade() { Some(u) => u, None => return };
        ui.set_mod_status_message(format!("'{}' indiriliyor...", slug).into());

        let ui_thread = ui_mod_install.clone();
        let slug_str = slug.to_string();
        let software = ui.get_selected_software().to_string();
        let dest_dir = if matches!(software.as_str(), "Fabric" | "Forge" | "NeoForge") {
            get_server_dir(&current_id).join("mods")
        } else {
            get_server_dir(&current_id).join("plugins")
        };

        thread::spawn(move || {
            let client = reqwest::blocking::Client::builder()
                .user_agent("Mozilla/5.0 (Windows NT 10.0; Win64; x64) AppleWebKit/537.36")
                .build();

            let result: Result<String, String> = (|| {
                let client = client.map_err(|e| e.to_string())?;
                let _ = fs::create_dir_all(&dest_dir);

                if let Some(spiget_id) = slug_str.strip_prefix("spiget:") {
                    let download_url = format!("https://api.spiget.org/v2/resources/{}/download", spiget_id);
                    println!("[SPIGET] İndirme başlatılıyor: {}", download_url);
                    let mut response = client.get(&download_url).send().map_err(|e| e.to_string())?;
                    let mut bytes = Vec::new();
                    response.read_to_end(&mut bytes).map_err(|e| e.to_string())?;

                    let target_path = dest_dir.join(format!("spigot_{}.jar", spiget_id));
                    let mut f = File::create(&target_path).map_err(|e| e.to_string())?;
                    f.write_all(&bytes).map_err(|e| e.to_string())?;
                    println!("[SPIGET] Başarıyla indirildi: {:?}", target_path);
                    return Ok(format!("spigot_{}.jar", spiget_id));
                } else {
                    let versions_url = format!("https://api.modrinth.com/v2/project/{}/version", slug_str);
                    println!("[MODRINTH] Sürüm bilgisi alınıyor: {}", versions_url);
                    let versions: Vec<ModrinthVersion> = client.get(&versions_url).send().map_err(|e| e.to_string())?.json().map_err(|e| e.to_string())?;

                    if let Some(latest) = versions.first() {
                        if let Some(file) = latest.files.first() {
                            let target_path = dest_dir.join(&file.filename);
                            let bytes = client.get(&file.url).send().map_err(|e| e.to_string())?.bytes().map_err(|e| e.to_string())?;
                            let mut f = File::create(&target_path).map_err(|e| e.to_string())?;
                            f.write_all(&bytes).map_err(|e| e.to_string())?;
                            println!("[MODRINTH] Başarıyla indirildi: {:?}", target_path);
                            return Ok(file.filename.clone());
                        }
                    }
                }
                Err("İndirilebilir dosya bulunamadı.".to_string())
            })();

            let _ = slint::invoke_from_event_loop(move || {
                if let Some(ui) = ui_thread.upgrade() {
                    match result {
                        Ok(filename) => {
                            let hint = if ui.get_server_running() {
                                if ui.get_app_lang() == "tr" {
                                    "Kuruldu. Açılması için sunucuyu yeniden başlat."
                                } else {
                                    "Installed. Restart the server so it loads."
                                }
                            } else if ui.get_app_lang() == "tr" {
                                "Kuruldu. Sunucu açılınca yüklenecek."
                            } else {
                                "Installed. It loads when the server starts."
                            };
                            ui.set_mod_status_message(format!("'{}' {}", filename, hint).into());
                            let files_model: slint::ModelRc<FileItem> = std::rc::Rc::new(slint::VecModel::from(get_server_files_of(&current_id))).into();
                            ui.set_server_files(files_model);
                        }
                        Err(e) => {
                            ui.set_mod_status_message(format!("Kurulum hatası: {}", e).into());
                        }
                    }
                }
            });
        });
    });

    // --- 15. SUNUCUYU BAŞLAT BUTONU ---
    let ui_handle = ui.as_weak();
    let procs_start = Arc::clone(&processes);
    let act_id_start = Arc::clone(&active_server_id);
    ui.on_start_server_clicked(move || {
        let current_id = act_id_start.lock().unwrap().clone();
        if current_id.is_empty() { return; }

        let ui = match ui_handle.upgrade() { Some(u) => u, None => return };
        let server_dir = get_server_dir(&current_id);
        let jar_path = server_dir.join("server.jar");

        if !jar_path.exists() {
            ui.set_server_error_message(format!("'{}' için server.jar bulunamadı! Yazılım sekmesinden indirin.", current_id).into());
            ui.set_server_running(false);
            ui.set_server_status("Durduruldu".into());
            return;
        }

        let list = load_servers_list();
        let preferred_java = list.iter().find(|s| s.id == current_id).map(|s| s.selected_java.as_str()).unwrap_or("java25");

        let java_exe = match get_executable_for_selected_java(preferred_java) {
            Some(path) => path,
            None => {
                ui.set_server_error_message(format!("Seçili Java motoru ({}) bulunamadı! 'Java' sekmesinden indirin.", preferred_java).into());
                ui.set_server_running(false);
                ui.set_server_status("Durduruldu".into());
                return;
            }
        };

        ui.set_server_error_message("".into());

        let port = valid_port(&list.iter().find(|s| s.id == current_id).map(|s| s.port.clone())
            .unwrap_or_else(|| ui.get_server_port().to_string()));
        let mut port_map = HashMap::new();
        port_map.insert("server-port".to_string(), port.clone());
        let _ = save_server_properties_of(&current_id, &port_map);
        apply_join(&ui, &port);

        persist_ram_for(&current_id, &ui.get_ram_amount(), &ui.get_ram_unit());
        apply_ram_to_ui(&ui, &ram_stored(&ui.get_ram_amount(), &ui.get_ram_unit()));
        persist_timezone_for(&current_id, &ui.get_prop_timezone());
        let xmx = ram_xmx(&ui.get_ram_amount(), &ui.get_ram_unit());
        let tz = {
            let t = ui.get_prop_timezone().to_string();
            if t.trim().is_empty() { "Europe/Istanbul".to_string() } else { t }
        };
        let tz_arg = format!("-Duser.timezone={}", tz);

        let eula_path = server_dir.join("eula.txt");
        let _ = fs::write(&eula_path, "eula=true\n");

        let mut procs = procs_start.lock().unwrap();
        let s = procs.entry(current_id.clone()).or_insert_with(ServerProcess::new);
        if reap_finished_child(s) || tcp_port_open(&port) {
            ui.set_server_running(true);
            ui.set_server_status("Çalışıyor".into());
            return;
        }

        ui.set_server_status("Başlatılıyor...".into());
        let log = format!("[SİSTEM] '{}' başlatılıyor... (Motor: {:?}, RAM: {})", current_id, java_exe, xmx);
        s.append_log(log);
        ui.set_console_text(s.log_text.clone().into());

        let mut cmd = Command::new(&java_exe);
        hide_window(&mut cmd);
        cmd.args(["-Xms128M", &xmx, &tz_arg, "-jar", "server.jar", "nogui"])
            .current_dir(&server_dir)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());

        match cmd.spawn() {
            Ok(mut child) => {
                let stdout = child.stdout.take().expect("stdout açılamadı");
                let stderr = child.stderr.take().expect("stderr açılamadı");
                let stdin = child.stdin.take().expect("stdin açılamadı");

                s.stdin = Some(stdin);
                s.child = Some(child);
                if let Some(pid) = s.child.as_ref().map(|c| c.id()) {
                    write_server_pid(&current_id, pid);
                }

                ui.set_server_running(true);
                ui.set_server_status("Çalışıyor".into());

                let procs_stdout = Arc::clone(&procs_start);
                let ui_stdout = ui_handle.clone();
                let act_id_stdout = Arc::clone(&act_id_start);
                let target_id = current_id.clone();

                thread::spawn(move || {
                    let mut last_ui = Instant::now();
                    let mut dirty = false;
                    pipe_lines_lossy(stdout, |line| {
                        let mut p = procs_stdout.lock().unwrap();
                        if let Some(proc) = p.get_mut(&target_id) {
                            proc.append_log(line);
                            let is_active = *act_id_stdout.lock().unwrap() == target_id;
                            if is_active {
                                dirty = true;
                                if last_ui.elapsed().as_millis() >= 160 {
                                    last_ui = Instant::now();
                                    dirty = false;
                                    let text = proc.log_text.clone();
                                    drop(p);
                                    let ui_thread = ui_stdout.clone();
                                    let _ = slint::invoke_from_event_loop(move || {
                                        if let Some(ui) = ui_thread.upgrade() {
                                            ui.set_console_text(text.into());
                                        }
                                    });
                                }
                            }
                        }
                    });
                    if dirty {
                        let text = {
                            let p = procs_stdout.lock().unwrap();
                            p.get(&target_id).map(|proc| proc.log_text.clone())
                        };
                        if let Some(text) = text {
                            let ui_thread = ui_stdout.clone();
                            let _ = slint::invoke_from_event_loop(move || {
                                if let Some(ui) = ui_thread.upgrade() {
                                    ui.set_console_text(text.into());
                                }
                            });
                        }
                    }
                });

                let procs_stderr = Arc::clone(&procs_start);
                let ui_stderr = ui_handle.clone();
                let act_id_stderr = Arc::clone(&act_id_start);
                let target_id_err = current_id.clone();

                thread::spawn(move || {
                    let mut last_ui = Instant::now();
                    let mut dirty = false;
                    pipe_lines_lossy(stderr, |line| {
                        let mut p = procs_stderr.lock().unwrap();
                        if let Some(proc) = p.get_mut(&target_id_err) {
                            proc.append_log(format!("[HATA-STDERR] {}", line));
                            let is_active = *act_id_stderr.lock().unwrap() == target_id_err;
                            if is_active {
                                dirty = true;
                                if last_ui.elapsed().as_millis() >= 160 {
                                    last_ui = Instant::now();
                                    dirty = false;
                                    let text = proc.log_text.clone();
                                    drop(p);
                                    let ui_thread = ui_stderr.clone();
                                    let _ = slint::invoke_from_event_loop(move || {
                                        if let Some(ui) = ui_thread.upgrade() {
                                            ui.set_console_text(text.into());
                                        }
                                    });
                                }
                            }
                        }
                    });
                    if dirty {
                        let text = {
                            let p = procs_stderr.lock().unwrap();
                            p.get(&target_id_err).map(|proc| proc.log_text.clone())
                        };
                        if let Some(text) = text {
                            let ui_thread = ui_stderr.clone();
                            let _ = slint::invoke_from_event_loop(move || {
                                if let Some(ui) = ui_thread.upgrade() {
                                    ui.set_console_text(text.into());
                                }
                            });
                        }
                    }
                });
            }
            Err(e) => {
                ui.set_server_error_message(format!("Java başlatılamadı: {}.", e).into());
                s.append_log(format!("[KRİTİK HATA] Java başlatılamadı: {}", e));
                ui.set_console_text(s.log_text.clone().into());
                ui.set_server_running(false);
                ui.set_server_status("Durduruldu".into());
            }
        }
    });

    // --- 16. DURDUR VE KOMUT GÖNDER ---
    let procs_stop = Arc::clone(&processes);
    let act_id_stop = Arc::clone(&active_server_id);
    let ui_stop = ui.as_weak();
    ui.on_stop_server_clicked(move || {
        let current_id = act_id_stop.lock().unwrap().clone();
        if current_id.is_empty() { return; }

        let Some(ui) = ui_stop.upgrade() else { return };
        let port = {
            let from_meta = load_servers_list()
                .into_iter()
                .find(|s| s.id == current_id)
                .map(|s| s.port);
            from_meta.unwrap_or_else(|| ui.get_server_port().to_string())
        };
        ui.set_server_status("Durduruluyor...".into());

        let mut sent_stop = false;
        {
            let mut procs = procs_stop.lock().unwrap();
            if let Some(proc) = procs.get_mut(&current_id) {
                if let Some(ref mut stdin) = proc.stdin {
                    if writeln!(stdin, "stop").is_ok() {
                        let _ = stdin.flush();
                        sent_stop = true;
                    }
                }
            }
        }

        if sent_stop {
            let procs_later = Arc::clone(&procs_stop);
            let id_later = current_id.clone();
            let port_later = port.clone();
            let ui_later = ui_stop.clone();
            thread::spawn(move || {
                thread::sleep(Duration::from_secs(8));
                if !tcp_port_open(&port_later) {
                    clear_server_pid(&id_later);
                    return;
                }
                {
                    let mut procs = procs_later.lock().unwrap();
                    if let Some(proc) = procs.get_mut(&id_later) {
                        if let Some(child) = proc.child.as_mut() {
                            let _ = child.kill();
                        }
                        proc.stdin = None;
                    }
                }
                force_stop_java(&id_later, &port_later);
                if let Some(ui) = ui_later.upgrade() {
                    ui.set_server_running(false);
                    ui.set_server_status("Durduruldu".into());
                }
            });
            return;
        }

        {
            let mut procs = procs_stop.lock().unwrap();
            if let Some(proc) = procs.get_mut(&current_id) {
                if let Some(child) = proc.child.as_mut() {
                    let _ = child.kill();
                }
                proc.stdin = None;
                proc.append_log("[SİSTEM] Java süreci durduruldu.".to_string());
                ui.set_console_text(proc.log_text.clone().into());
            }
        }
        force_stop_java(&current_id, &port);
        ui.set_server_running(false);
        ui.set_server_status("Durduruldu".into());
    });

    let procs_cmd = Arc::clone(&processes);
    let act_id_cmd = Arc::clone(&active_server_id);
    let ui_cmd = ui.as_weak();
    ui.on_send_command(move |cmd| {
        let current_id = act_id_cmd.lock().unwrap().clone();
        if current_id.is_empty() { return; }

        let mut procs = procs_cmd.lock().unwrap();
        if let Some(proc) = procs.get_mut(&current_id) {
            if let Some(ref mut stdin) = proc.stdin {
                let _ = writeln!(stdin, "{}", cmd);
                let _ = stdin.flush();
                let echo = format!("> {}", cmd);
                proc.append_log(echo);
                if let Some(ui) = ui_cmd.upgrade() {
                    ui.set_console_text(proc.log_text.clone().into());
                }
            }
        }
    });

    let ui_sync = ui.as_weak();
    let procs_sync = Arc::clone(&processes);
    let act_id_sync = Arc::clone(&active_server_id);
    thread::spawn(move || {
        let mut cached_id = String::new();
        let mut cached_port = String::new();
        let mut tick: u32 = 0;
        let mut up_since: Option<Instant> = None;
        loop {
            thread::sleep(Duration::from_secs(1));
            tick = tick.wrapping_add(1);
            {
                let mut procs = procs_sync.lock().unwrap();
                for proc in procs.values_mut() {
                    let _ = reap_finished_child(proc);
                }
            }

            let active = act_id_sync.lock().unwrap().clone();
            if active.is_empty() {
                cached_id.clear();
                up_since = None;
                continue;
            }
            if active != cached_id {
                up_since = None;
            }
            if active != cached_id || tick.is_multiple_of(8) {
                cached_id = active.clone();
                cached_port = load_servers_list()
                    .into_iter()
                    .find(|s| s.id == active)
                    .map(|s| s.port)
                    .unwrap_or_default();
            }
            if cached_port.is_empty() {
                continue;
            }
            let child_alive = {
                let procs = procs_sync.lock().unwrap();
                procs.get(&active).map(|p| p.child.is_some()).unwrap_or(false)
            };
            let port_open = tcp_port_open(&cached_port);
            let players = if port_open && tick.is_multiple_of(3) {
                mc_status(&cached_port)
            } else {
                None
            };
            let disk = if tick % 15 == 1 {
                disk_free_bytes(Path::new("servers")).or_else(|| disk_free_bytes(Path::new(".")))
            } else {
                None
            };
            let running = child_alive || port_open;
            if running {
                if up_since.is_none() {
                    up_since = Some(Instant::now());
                }
            } else {
                up_since = None;
            }
            let uptime = up_since.map(|t| format_uptime(t.elapsed().as_secs())).unwrap_or_default();
            let detached_log = if running && !child_alive {
                Some(read_latest_log(&active))
            } else {
                None
            };
            let worlds = if running && tick.is_multiple_of(5) {
                Some(get_worlds_of(&active))
            } else {
                None
            };
            let active_copy = active.clone();
            let ui_tick = ui_sync.clone();

            let _ = slint::invoke_from_event_loop(move || {
                if let Some(ui) = ui_tick.upgrade() {
                    if ui.get_active_server_id().as_str() != active_copy {
                        return;
                    }
                    let was_running = ui.get_server_running();
                    ui.set_server_running(running);
                    ui.set_server_status(if running { "Çalışıyor".into() } else { "Durduruldu".into() });
                    ui.set_uptime_label(uptime.into());
                    if !running {
                        ui.set_players_online(-1);
                        ui.set_players_max(0);
                        ui.set_online_names("".into());
                    } else if let Some(status) = players {
                        ui.set_players_online(status.online);
                        ui.set_players_max(status.max);
                        ui.set_online_names(status.names.into());
                    }
                    if let Some(free) = disk {
                        let low = free < 2 * 1024 * 1024 * 1024;
                        ui.set_disk_low(low);
                        ui.set_disk_label(format_bytes(free).into());
                    }
                    if let Some(text) = detached_log {
                        if ui.get_active_tab() == 2 {
                            ui.set_console_text(text.into());
                        }
                    }
                    if let Some(w) = worlds {
                        if ui.get_active_tab() == 9 || (running && !was_running) {
                            ui.set_world_list(std::rc::Rc::new(slint::VecModel::from(w)).into());
                        }
                    }
                }
            });
        }
    });

    let ui_auto = ui.as_weak();
    let procs_auto = Arc::clone(&processes);
    let busy_auto = Arc::clone(&backup_busy);
    let last_auto = Arc::clone(&backup_last);
    thread::spawn(move || {
        loop {
            thread::sleep(Duration::from_secs(20));
            let now = SystemTime::now();
            for srv in load_servers_list() {
                let every = backup_every_min(srv.backup_every_min);
                if every == 0 {
                    last_auto.lock().unwrap().remove(&srv.id);
                    continue;
                }
                let due = {
                    let mut last = last_auto.lock().unwrap();
                    if !last.contains_key(&srv.id) {
                        last.insert(srv.id.clone(), now);
                        false
                    } else {
                        backup_is_due(last.get(&srv.id).copied(), every, now)
                    }
                };
                if !due {
                    continue;
                }
                {
                    let mut busy = busy_auto.lock().unwrap();
                    if !busy.insert(srv.id.clone()) {
                        continue;
                    }
                }
                let running = server_is_running(&procs_auto.lock().unwrap(), &srv.id);
                if !running {
                    busy_auto.lock().unwrap().remove(&srv.id);
                    continue;
                }
                let free = disk_free_bytes(Path::new("servers")).or_else(|| disk_free_bytes(Path::new(".")));
                if !auto_backup_allowed(free) {
                    busy_auto.lock().unwrap().remove(&srv.id);
                    last_auto.lock().unwrap().insert(srv.id.clone(), now);
                    let id_low = srv.id.clone();
                    let ui_low = ui_auto.clone();
                    let _ = slint::invoke_from_event_loop(move || {
                        if let Some(ui) = ui_low.upgrade() {
                            if ui.get_active_server_id().as_str() == id_low {
                                let msg = if ui.get_app_lang() == "tr" {
                                    "Otomatik yedek atlandı: disk 2 GB altında."
                                } else {
                                    "Automatic backup skipped: under 2 GB free."
                                };
                                ui.set_world_status(msg.into());
                            }
                        }
                    });
                    continue;
                }
                let id = srv.id.clone();
                let sent = send_console_line(&procs_auto, &id, "save-all flush");
                if !sent {
                    busy_auto.lock().unwrap().remove(&id);
                    last_auto.lock().unwrap().insert(id.clone(), now);
                    let ui_skip = ui_auto.clone();
                    let id_skip = id.clone();
                    let _ = slint::invoke_from_event_loop(move || {
                        if let Some(ui) = ui_skip.upgrade() {
                            if ui.get_active_server_id().as_str() == id_skip {
                                let msg = if ui.get_app_lang() == "tr" {
                                    "Otomatik yedek için sunucuyu PanelMC'den başlat."
                                } else {
                                    "Start the server from PanelMC for automatic backups."
                                };
                                ui.set_world_status(msg.into());
                            }
                        }
                    });
                    continue;
                }
                let ui_begin = ui_auto.clone();
                let id_begin = id.clone();
                let _ = slint::invoke_from_event_loop(move || {
                    if let Some(ui) = ui_begin.upgrade() {
                        if ui.get_active_server_id().as_str() == id_begin {
                            ui.set_is_backing_up(true);
                            let msg = if ui.get_app_lang() == "tr" { "Otomatik yedek alınıyor..." } else { "Automatic backup..." };
                            ui.set_world_status(msg.into());
                        }
                    }
                });
                let _ = wait_until_saved(&id, Duration::from_secs(20));
                let result = backup_worlds_prefixed(&id, "oto").inspect(|_name| {
                    prune_backups(&get_server_dir(&id), "oto", 5);
                });
                last_auto.lock().unwrap().insert(id.clone(), SystemTime::now());
                busy_auto.lock().unwrap().remove(&id);
                let ui_done = ui_auto.clone();
                let id_done = id.clone();
                let _ = slint::invoke_from_event_loop(move || {
                    if let Some(ui) = ui_done.upgrade() {
                        ui.set_is_backing_up(false);
                        if ui.get_active_server_id().as_str() != id_done {
                            return;
                        }
                        match result {
                            Ok(name) => {
                                let msg = if ui.get_app_lang() == "tr" {
                                    format!("{name} otomatik kaydedildi.")
                                } else {
                                    format!("{name} saved automatically.")
                                };
                                ui.set_world_status(msg.into());
                            }
                            Err(e) => ui.set_world_status(e.into()),
                        }
                        let backups: slint::ModelRc<BackupItem> = std::rc::Rc::new(slint::VecModel::from(get_backups_of(&id_done))).into();
                        ui.set_backup_list(backups);
                    }
                });
            }
        }
    });

    ui.run()
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::net::TcpListener;

    #[test]
    fn uptime_formats_hours() {
        assert_eq!(format_uptime(0), "0s");
        assert_eq!(format_uptime(61), "1m 01s");
        assert_eq!(format_uptime(3661), "1h 01m");
    }

    #[test]
    fn disk_free_is_readable() {
        let free = disk_free_bytes(Path::new(".")).expect("disk");
        assert!(free > 0);
    }

    #[test]
    fn status_ping_reads_players_and_names() {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let port = listener.local_addr().unwrap().port();
        thread::spawn(move || {
            let (mut sock, _) = listener.accept().unwrap();
            let mut buf = [0u8; 256];
            let _ = sock.read(&mut buf);
            let json = br#"{"players":{"online":2,"max":8,"sample":[{"name":"Ada","id":"a"},{"name":"Bea","id":"b"}]}}"#;
            let mut body = Vec::new();
            write_varint(&mut body, 0);
            write_varint(&mut body, json.len() as i32);
            body.extend_from_slice(json);
            let mut packet = Vec::new();
            write_varint(&mut packet, body.len() as i32);
            packet.extend_from_slice(&body);
            let _ = sock.write_all(&packet);
        });
        let status = mc_status(&port.to_string()).expect("status");
        assert_eq!(status.online, 2);
        assert_eq!(status.max, 8);
        assert_eq!(status.names, "Ada, Bea");
    }

    #[test]
    fn backup_schedule_rules() {
        let t0 = UNIX_EPOCH + Duration::from_secs(1_000_000);
        assert!(!backup_is_due(None, 30, t0));
        assert!(!backup_is_due(Some(t0), 0, t0 + Duration::from_secs(3600)));
        assert!(!backup_is_due(Some(t0), 30, t0 + Duration::from_secs(29 * 60)));
        assert!(backup_is_due(Some(t0), 30, t0 + Duration::from_secs(30 * 60)));
        assert_eq!(backup_every_min(15), 0);
        assert_eq!(backup_every_min(60), 60);
        assert!(!auto_backup_allowed(Some(1024)));
        assert!(auto_backup_allowed(Some(3 * 1024 * 1024 * 1024)));
        assert!(auto_backup_allowed(None));
        assert!(save_finished("... Saved the game\n"));
        assert!(!save_finished("Saving the game"));
    }

    #[test]
    fn auto_backup_zips_world_and_prunes() {
        let root = std::env::temp_dir().join(format!("panelmc-bak-{}", std::process::id()));
        let _ = fs::remove_dir_all(&root);
        let world = root.join("world");
        fs::create_dir_all(world.join("region")).unwrap();
        fs::write(world.join("level.dat"), b"level").unwrap();
        fs::write(world.join("session.lock"), b"lock").unwrap();
        fs::write(world.join("region").join("r.0.0.mca"), b"chunk").unwrap();

        let name = backup_folders(&root, &["world".into()], "oto", 100).unwrap();
        assert_eq!(name, "oto-100.zip");
        let zip_file = File::open(root.join("backups").join(&name)).unwrap();
        let mut archive = zip::ZipArchive::new(zip_file).unwrap();
        let mut stored = Vec::new();
        for i in 0..archive.len() {
            stored.push(archive.by_index(i).unwrap().name().to_string());
        }
        assert!(stored.iter().any(|n| n.ends_with("level.dat")));
        assert!(stored.iter().any(|n| n.ends_with("r.0.0.mca")));
        assert!(!stored.iter().any(|n| n.contains("session.lock")));

        for ts in 1..7 {
            fs::write(root.join("backups").join(format!("oto-{ts}.zip")), b"zip").unwrap();
        }
        fs::write(root.join("backups").join("yedek-9.zip"), b"keep").unwrap();
        let removed = prune_backups(&root, "oto", 5);
        assert_eq!(removed.len(), 2);
        assert!(root.join("backups").join("yedek-9.zip").is_file());
        let left = fs::read_dir(root.join("backups")).unwrap().flatten().filter(|e| {
            e.file_name().to_string_lossy().starts_with("oto-")
        }).count();
        assert_eq!(left, 5);
        let _ = fs::remove_dir_all(&root);
    }
}