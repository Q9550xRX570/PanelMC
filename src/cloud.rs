use std::fs::{self, File};
use std::io::{Read, Write};
use std::net::TcpListener;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::thread;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use base64::Engine;
use sha2::{Digest, Sha256};

#[derive(serde::Serialize, serde::Deserialize, Clone, Default)]
pub struct OAuthToken {
    #[serde(default)]
    pub access_token: String,
    #[serde(default)]
    pub refresh_token: String,
    #[serde(default)]
    pub expires_at: u64,
    #[serde(default)]
    pub email: String,
    #[serde(default)]
    pub folder_id: String,
}

#[derive(serde::Serialize, serde::Deserialize, Clone, Default)]
pub struct CloudTokens {
    #[serde(default)]
    pub google: Option<OAuthToken>,
    #[serde(default)]
    pub onedrive: Option<OAuthToken>,
}

fn tokens_path() -> PathBuf {
    Path::new("servers").join("cloud-tokens.json")
}

pub fn load_tokens() -> CloudTokens {
    fs::read_to_string(tokens_path())
        .ok()
        .and_then(|s| serde_json::from_str(&s).ok())
        .unwrap_or_default()
}

pub fn save_tokens(tokens: &CloudTokens) {
    let _ = fs::create_dir_all("servers");
    if let Ok(json) = serde_json::to_string_pretty(tokens) {
        let _ = fs::write(tokens_path(), json);
    }
}

fn now_unix() -> u64 {
    SystemTime::now().duration_since(UNIX_EPOCH).map(|d| d.as_secs()).unwrap_or(0)
}

fn http_client() -> Result<reqwest::blocking::Client, String> {
    reqwest::blocking::Client::builder()
        .user_agent(concat!("PanelMC/", env!("CARGO_PKG_VERSION")))
        .timeout(Duration::from_secs(600))
        .redirect(reqwest::redirect::Policy::limited(8))
        .build()
        .map_err(|e| e.to_string())
}

fn url_encode(s: &str) -> String {
    let mut out = String::new();
    for b in s.bytes() {
        match b {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' => out.push(b as char),
            _ => out.push_str(&format!("%{:02X}", b)),
        }
    }
    out
}

fn url_decode(s: &str) -> String {
    let bytes = s.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'%' && i + 2 < bytes.len() {
            if let Ok(v) = u8::from_str_radix(std::str::from_utf8(&bytes[i + 1..i + 3]).unwrap_or(""), 16) {
                out.push(v);
                i += 3;
                continue;
            }
        } else if bytes[i] == b'+' {
            out.push(b' ');
            i += 1;
            continue;
        }
        out.push(bytes[i]);
        i += 1;
    }
    String::from_utf8_lossy(&out).into_owned()
}

fn query_param(query: &str, key: &str) -> Option<String> {
    for part in query.split('&') {
        if let Some((k, v)) = part.split_once('=') {
            if k == key {
                return Some(url_decode(v));
            }
        }
    }
    None
}

fn open_browser(url: &str) {
    #[cfg(target_os = "windows")]
    {
        let cmdline = format!("start \"\" \"{}\"", url.replace('"', ""));
        let _ = Command::new("cmd").arg("/C").arg(cmdline).spawn();
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

fn pkce_pair() -> Result<(String, String), String> {
    let mut raw = [0u8; 32];
    getrandom::getrandom(&mut raw).map_err(|e| e.to_string())?;
    let verifier = URL_SAFE_NO_PAD.encode(raw);
    let challenge = URL_SAFE_NO_PAD.encode(Sha256::digest(verifier.as_bytes()));
    Ok((verifier, challenge))
}

fn wait_oauth_code(listener: TcpListener) -> Result<String, String> {
    listener.set_nonblocking(false).ok();
    let (mut stream, _) = listener.accept().map_err(|_| "Giriş zaman aşımı. Tarayıcıda oturum açmadınız.".to_string())?;
    let _ = stream.set_read_timeout(Some(Duration::from_secs(30)));
    let mut buf = [0u8; 4096];
    let n = stream.read(&mut buf).map_err(|e| e.to_string())?;
    let req = String::from_utf8_lossy(&buf[..n]);
    let first = req.lines().next().unwrap_or("");
    let path = first.split_whitespace().nth(1).unwrap_or("/");
    let query = path.split_once('?').map(|(_, q)| q).unwrap_or("");
    let html = "<html><body style='font-family:sans-serif;background:#161c21;color:#f0f0f0;padding:40px'><h2>PanelMC bağlandı</h2><p>Bu pencereyi kapatıp panele dönebilirsiniz.</p></body></html>";
    let _ = write!(stream, "HTTP/1.1 200 OK\r\nContent-Type: text/html; charset=utf-8\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}", html.len(), html);
    if let Some(err) = query_param(query, "error") {
        return Err(format!("Giriş reddedildi: {}", err));
    }
    query_param(query, "code").ok_or_else(|| "Tarayıcıdan kod gelmedi.".to_string())
}

fn apply_tokens(existing: &mut OAuthToken, json: &serde_json::Value, fallback_refresh: &str) {
    if let Some(at) = json.get("access_token").and_then(|v| v.as_str()) {
        existing.access_token = at.to_string();
    }
    if let Some(rt) = json.get("refresh_token").and_then(|v| v.as_str()) {
        if !rt.is_empty() {
            existing.refresh_token = rt.to_string();
        }
    } else if existing.refresh_token.is_empty() {
        existing.refresh_token = fallback_refresh.to_string();
    }
    let expires = json.get("expires_in").and_then(|v| v.as_u64()).unwrap_or(3600);
    existing.expires_at = now_unix().saturating_add(expires.saturating_sub(60));
}

fn google_userinfo(client: &reqwest::blocking::Client, access: &str) -> String {
    client
        .get("https://www.googleapis.com/oauth2/v2/userinfo")
        .bearer_auth(access)
        .send()
        .ok()
        .and_then(|r| r.json::<serde_json::Value>().ok())
        .and_then(|v| v.get("email").and_then(|e| e.as_str()).map(|s| s.to_string()))
        .unwrap_or_default()
}

fn onedrive_userinfo(client: &reqwest::blocking::Client, access: &str) -> String {
    client
        .get("https://graph.microsoft.com/v1.0/me")
        .bearer_auth(access)
        .send()
        .ok()
        .and_then(|r| r.json::<serde_json::Value>().ok())
        .map(|v| {
            v.get("userPrincipalName")
                .or_else(|| v.get("mail"))
                .and_then(|e| e.as_str())
                .unwrap_or("")
                .to_string()
        })
        .unwrap_or_default()
}

pub fn google_login(client_id: &str, client_secret: &str, status: impl Fn(&str)) -> Result<String, String> {
    let client_id = client_id.trim();
    if client_id.is_empty() {
        return Err("Google Client ID girin. Uygulama > Google Drive (console.cloud.google.com, Desktop OAuth).".into());
    }
    let (verifier, challenge) = pkce_pair()?;
    let listener = TcpListener::bind("127.0.0.1:0").map_err(|e| e.to_string())?;
    let port = listener.local_addr().map_err(|e| e.to_string())?.port();
    let redirect = format!("http://127.0.0.1:{}", port);
    let scope = url_encode("https://www.googleapis.com/auth/drive.file");
    let auth = format!(
        "https://accounts.google.com/o/oauth2/v2/auth?client_id={}&redirect_uri={}&response_type=code&scope={}&access_type=offline&prompt=consent&code_challenge={}&code_challenge_method=S256",
        url_encode(client_id),
        url_encode(&redirect),
        scope,
        challenge
    );
    status("Tarayıcıda Google hesabınla oturum aç...");
    open_browser(&auth);
    let code = wait_oauth_code(listener)?;
    status("Google jetonu alınıyor...");
    let http = http_client()?;
    let mut form = vec![
        ("code", code),
        ("client_id", client_id.to_string()),
        ("redirect_uri", redirect),
        ("grant_type", "authorization_code".into()),
        ("code_verifier", verifier),
    ];
    if !client_secret.trim().is_empty() {
        form.push(("client_secret", client_secret.trim().to_string()));
    }
    let resp = http
        .post("https://oauth2.googleapis.com/token")
        .form(&form)
        .send()
        .map_err(|e| e.to_string())?;
    let json: serde_json::Value = resp.json().map_err(|e| e.to_string())?;
    if let Some(err) = json.get("error").and_then(|v| v.as_str()) {
        let desc = json.get("error_description").and_then(|v| v.as_str()).unwrap_or("");
        return Err(format!("Google: {} {}", err, desc));
    }
    let mut tok = OAuthToken::default();
    apply_tokens(&mut tok, &json, "");
    tok.email = google_userinfo(&http, &tok.access_token);
    let mut all = load_tokens();
    all.google = Some(tok.clone());
    save_tokens(&all);
    Ok(if tok.email.is_empty() { "Google Drive".into() } else { tok.email })
}

pub fn onedrive_login(client_id: &str, status: impl Fn(&str)) -> Result<String, String> {
    let client_id = client_id.trim();
    if client_id.is_empty() {
        return Err("OneDrive Client ID girin. Azure > App registrations, public client (device code) açık olsun.".into());
    }
    let http = http_client()?;
    let start = http
        .post("https://login.microsoftonline.com/common/oauth2/v2.0/devicecode")
        .form(&[
            ("client_id", client_id),
            ("scope", "offline_access User.Read Files.ReadWrite.AppFolder"),
        ])
        .send()
        .map_err(|e| e.to_string())?;
    let dc: serde_json::Value = start.json().map_err(|e| e.to_string())?;
    if let Some(err) = dc.get("error").and_then(|v| v.as_str()) {
        let desc = dc.get("error_description").and_then(|v| v.as_str()).unwrap_or("");
        return Err(format!("OneDrive: {} {}", err, desc));
    }
    let device_code = dc.get("device_code").and_then(|v| v.as_str()).unwrap_or("").to_string();
    let user_code = dc.get("user_code").and_then(|v| v.as_str()).unwrap_or("").to_string();
    let uri = dc
        .get("verification_uri_complete")
        .and_then(|v| v.as_str())
        .or_else(|| dc.get("verification_uri").and_then(|v| v.as_str()))
        .unwrap_or("https://microsoft.com/devicelogin")
        .to_string();
    let interval = dc.get("interval").and_then(|v| v.as_u64()).unwrap_or(5).max(3);
    let expires = dc.get("expires_in").and_then(|v| v.as_u64()).unwrap_or(900);
    status(&format!("Tarayıcıda oturum aç. Kod: {}  —  {}", user_code, uri));
    open_browser(&uri);

    let deadline = now_unix() + expires;
    loop {
        if now_unix() >= deadline {
            return Err("OneDrive girişi zaman aşımına uğradı.".into());
        }
        thread::sleep(Duration::from_secs(interval));
        let poll = http
            .post("https://login.microsoftonline.com/common/oauth2/v2.0/token")
            .form(&[
                ("grant_type", "urn:ietf:params:oauth:grant-type:device_code"),
                ("client_id", client_id),
                ("device_code", device_code.as_str()),
            ])
            .send()
            .map_err(|e| e.to_string())?;
        let json: serde_json::Value = poll.json().map_err(|e| e.to_string())?;
        if let Some(err) = json.get("error").and_then(|v| v.as_str()) {
            match err {
                "authorization_pending" => continue,
                "slow_down" => {
                    thread::sleep(Duration::from_secs(interval));
                    continue;
                }
                other => {
                    let desc = json.get("error_description").and_then(|v| v.as_str()).unwrap_or("");
                    return Err(format!("OneDrive: {} {}", other, desc));
                }
            }
        }
        let mut tok = OAuthToken::default();
        apply_tokens(&mut tok, &json, "");
        tok.email = onedrive_userinfo(&http, &tok.access_token);
        let mut all = load_tokens();
        all.onedrive = Some(tok.clone());
        save_tokens(&all);
        return Ok(if tok.email.is_empty() { "OneDrive".into() } else { tok.email });
    }
}

pub fn google_logout() {
    let mut all = load_tokens();
    all.google = None;
    save_tokens(&all);
}

pub fn onedrive_logout() {
    let mut all = load_tokens();
    all.onedrive = None;
    save_tokens(&all);
}

fn refresh_google(client_id: &str, client_secret: &str, tok: &mut OAuthToken) -> Result<(), String> {
    if now_unix() < tok.expires_at && !tok.access_token.is_empty() {
        return Ok(());
    }
    if tok.refresh_token.is_empty() {
        return Err("Google oturumu düştü. Uygulama sekmesinden tekrar giriş yapın.".into());
    }
    let http = http_client()?;
    let mut form = vec![
        ("client_id", client_id.to_string()),
        ("refresh_token", tok.refresh_token.clone()),
        ("grant_type", "refresh_token".into()),
    ];
    if !client_secret.trim().is_empty() {
        form.push(("client_secret", client_secret.trim().to_string()));
    }
    let json: serde_json::Value = http
        .post("https://oauth2.googleapis.com/token")
        .form(&form)
        .send()
        .map_err(|e| e.to_string())?
        .json()
        .map_err(|e| e.to_string())?;
    if json.get("error").is_some() {
        return Err("Google oturumu yenilenemedi. Tekrar giriş yapın.".into());
    }
    let refresh = tok.refresh_token.clone();
    apply_tokens(tok, &json, &refresh);
    Ok(())
}

fn refresh_onedrive(client_id: &str, tok: &mut OAuthToken) -> Result<(), String> {
    if now_unix() < tok.expires_at && !tok.access_token.is_empty() {
        return Ok(());
    }
    if tok.refresh_token.is_empty() {
        return Err("OneDrive oturumu düştü. Uygulama sekmesinden tekrar giriş yapın.".into());
    }
    let http = http_client()?;
    let json: serde_json::Value = http
        .post("https://login.microsoftonline.com/common/oauth2/v2.0/token")
        .form(&[
            ("client_id", client_id),
            ("refresh_token", tok.refresh_token.as_str()),
            ("grant_type", "refresh_token"),
        ])
        .send()
        .map_err(|e| e.to_string())?
        .json()
        .map_err(|e| e.to_string())?;
    if json.get("error").is_some() {
        return Err("OneDrive oturumu yenilenemedi. Tekrar giriş yapın.".into());
    }
    let refresh = tok.refresh_token.clone();
    apply_tokens(tok, &json, &refresh);
    Ok(())
}

fn persist_google(tok: OAuthToken) {
    let mut all = load_tokens();
    all.google = Some(tok);
    save_tokens(&all);
}

fn persist_onedrive(tok: OAuthToken) {
    let mut all = load_tokens();
    all.onedrive = Some(tok);
    save_tokens(&all);
}

fn ensure_google_folder(http: &reqwest::blocking::Client, access: &str, tok: &mut OAuthToken) -> Result<String, String> {
    if !tok.folder_id.is_empty() {
        return Ok(tok.folder_id.clone());
    }
    let q = url_encode("name = 'PanelMC' and mimeType = 'application/vnd.google-apps.folder' and trashed = false");
    let url = format!("https://www.googleapis.com/drive/v3/files?q={}&fields=files(id,name)&pageSize=1&spaces=drive", q);
    let found: serde_json::Value = http
        .get(&url)
        .bearer_auth(access)
        .send()
        .map_err(|e| e.to_string())?
        .json()
        .map_err(|e| e.to_string())?;
    if let Some(id) = found
        .get("files")
        .and_then(|f| f.as_array())
        .and_then(|a| a.first())
        .and_then(|f| f.get("id"))
        .and_then(|v| v.as_str())
    {
        tok.folder_id = id.to_string();
        return Ok(tok.folder_id.clone());
    }
    let created: serde_json::Value = http
        .post("https://www.googleapis.com/drive/v3/files")
        .bearer_auth(access)
        .json(&serde_json::json!({
            "name": "PanelMC",
            "mimeType": "application/vnd.google-apps.folder"
        }))
        .send()
        .map_err(|e| e.to_string())?
        .json()
        .map_err(|e| e.to_string())?;
    let id = created
        .get("id")
        .and_then(|v| v.as_str())
        .ok_or_else(|| "Google Drive klasörü oluşturulamadı.".to_string())?
        .to_string();
    tok.folder_id = id.clone();
    Ok(id)
}

fn put_chunked(http: &reqwest::blocking::Client, session_url: &str, access: Option<&str>, src: &Path, chunk: usize) -> Result<(), String> {
    let meta = fs::metadata(src).map_err(|e| e.to_string())?;
    let total = meta.len();
    let mut file = File::open(src).map_err(|e| e.to_string())?;
    let mut buf = vec![0u8; chunk];
    let mut offset: u64 = 0;
    loop {
        let n = file.read(&mut buf).map_err(|e| e.to_string())?;
        if n == 0 {
            break;
        }
        let start = offset;
        let end = offset + n as u64 - 1;
        offset += n as u64;
        let mut req = http
            .put(session_url)
            .header("Content-Length", n)
            .header("Content-Range", format!("bytes {}-{}/{}", start, end, total))
            .header("Content-Type", "application/octet-stream")
            .body(buf[..n].to_vec());
        if let Some(token) = access {
            req = req.bearer_auth(token);
        }
        let resp = req.send().map_err(|e| e.to_string())?;
        let status = resp.status().as_u16();
        if status != 200 && status != 201 && status != 202 && status != 308 {
            return Err(format!("Yükleme kesildi: HTTP {}", status));
        }
    }
    if total == 0 {
        return Err("Yedek dosyası boş.".into());
    }
    Ok(())
}

pub fn upload_google(client_id: &str, client_secret: &str, server_id: &str, zip_name: &str, zip_path: &Path) -> Result<String, String> {
    let mut tok = load_tokens().google.ok_or_else(|| "Google ile giriş yapılmamış.".to_string())?;
    refresh_google(client_id, client_secret, &mut tok)?;
    let http = http_client()?;
    let access = tok.access_token.clone();
    let folder = ensure_google_folder(&http, &access, &mut tok)?;
    persist_google(tok.clone());
    let filename = format!("{}-{}", server_id, zip_name);
    let len = fs::metadata(zip_path).map_err(|e| e.to_string())?.len();
    let start = http
        .post("https://www.googleapis.com/upload/drive/v3/files?uploadType=resumable")
        .bearer_auth(&tok.access_token)
        .header("Content-Type", "application/json; charset=UTF-8")
        .header("X-Upload-Content-Type", "application/zip")
        .header("X-Upload-Content-Length", len)
        .json(&serde_json::json!({ "name": filename, "parents": [folder] }))
        .send()
        .map_err(|e| e.to_string())?;
    if !start.status().is_success() {
        return Err(format!("Google Drive oturumu açılamadı: HTTP {}", start.status().as_u16()));
    }
    let session = start
        .headers()
        .get("location")
        .and_then(|v| v.to_str().ok())
        .ok_or_else(|| "Google yükleme adresi yok.".to_string())?
        .to_string();
    put_chunked(&http, &session, Some(&tok.access_token), zip_path, 256 * 1024)?;
    Ok(format!("Google Drive / PanelMC / {}", filename))
}

pub fn upload_onedrive(client_id: &str, server_id: &str, zip_name: &str, zip_path: &Path) -> Result<String, String> {
    let mut tok = load_tokens().onedrive.ok_or_else(|| "OneDrive ile giriş yapılmamış.".to_string())?;
    refresh_onedrive(client_id, &mut tok)?;
    persist_onedrive(tok.clone());
    let http = http_client()?;
    let remote = format!("{}/{}", server_id, zip_name);
    let encoded = remote.split('/').map(url_encode).collect::<Vec<_>>().join("/");
    let session_api = format!(
        "https://graph.microsoft.com/v1.0/me/drive/special/approot:/{}:/createUploadSession",
        encoded
    );
    let start = http
        .post(&session_api)
        .bearer_auth(&tok.access_token)
        .json(&serde_json::json!({
            "item": { "@microsoft.graph.conflictBehavior": "replace", "name": zip_name }
        }))
        .send()
        .map_err(|e| e.to_string())?;
    if !start.status().is_success() {
        return Err(format!("OneDrive oturumu açılamadı: HTTP {}", start.status().as_u16()));
    }
    let json: serde_json::Value = start.json().map_err(|e| e.to_string())?;
    let session = json
        .get("uploadUrl")
        .and_then(|v| v.as_str())
        .ok_or_else(|| "OneDrive yükleme adresi yok.".to_string())?
        .to_string();
    put_chunked(&http, &session, None, zip_path, 320 * 1024)?;
    Ok(format!("OneDrive / Apps / PanelMC / {}", remote))
}
