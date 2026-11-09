#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

use axum::{
    body::Body,
    extract::State,
    http::{header, HeaderMap, HeaderValue, StatusCode, Uri},
    response::{IntoResponse, Response},
    routing::get,
    Router,
};
use percent_encoding::{percent_decode_str, utf8_percent_encode, NON_ALPHANUMERIC};
use serde::{Deserialize, Serialize};
use std::{
    fs,
    io,
    net::{IpAddr, Ipv4Addr, SocketAddr, UdpSocket},
    path::{Path, PathBuf},
    sync::{atomic::{AtomicBool, Ordering}, Arc, Mutex},
    thread,
    time::{Duration, SystemTime, UNIX_EPOCH},
};
use tauri::Manager;
use tokio::{
    fs::File,
    io::{AsyncBufReadExt, AsyncReadExt, AsyncWriteExt, BufReader},
    net::{TcpListener, TcpStream},
    runtime::Runtime,
    sync::watch,
    time::timeout,
};
use tokio_util::io::ReaderStream;

const APP_NAME: &str = "TauriFtpServer";
const CONFIG_FILE: &str = "config.json";
const MAX_LOGS: usize = 500;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ServerConfig {
    #[serde(default = "default_service_mode")]
    pub service_mode: String,
    pub root_dir: String,
    pub ftp_port: u16,
    pub http_port: u16,
    pub passive_start: u16,
    pub passive_end: u16,
    pub public_host: String,
    pub anonymous_enabled: bool,
    pub anonymous_write_enabled: bool,
}

fn default_service_mode() -> String { "both".to_string() }

impl Default for ServerConfig {
    fn default() -> Self {
        let root = dirs::download_dir()
            .or_else(dirs::document_dir)
            .or_else(|| std::env::current_dir().ok())
            .unwrap_or_else(|| PathBuf::from("."));
        Self {
            service_mode: default_service_mode(),
            root_dir: root.to_string_lossy().to_string(),
            ftp_port: 21,
            http_port: 8088,
            passive_start: 50000,
            passive_end: 50100,
            public_host: String::new(),
            anonymous_enabled: true,
            anonymous_write_enabled: false,
        }
    }
}

#[derive(Debug, Serialize)]
pub struct ServerStatus {
    pub running: bool,
}

struct RuntimeHandle {
    shutdown: watch::Sender<bool>,
    thread: Option<thread::JoinHandle<()>>,
}

pub struct AppState {
    config: Mutex<ServerConfig>,
    runtime: Mutex<Option<RuntimeHandle>>,
    running: Arc<AtomicBool>,
    logs: Arc<Mutex<Vec<String>>>,
}

impl AppState {
    fn new() -> Self {
        Self {
            config: Mutex::new(load_config()),
            runtime: Mutex::new(None),
            running: Arc::new(AtomicBool::new(false)),
            logs: Arc::new(Mutex::new(Vec::new())),
        }
    }
}

type LogSink = Arc<Mutex<Vec<String>>>;

fn config_path() -> PathBuf {
    dirs::config_dir()
        .unwrap_or_else(|| PathBuf::from("."))
        .join(APP_NAME)
        .join(CONFIG_FILE)
}

fn load_config() -> ServerConfig {
    let path = config_path();
    match fs::read_to_string(path) {
        Ok(text) => serde_json::from_str(&text).unwrap_or_default(),
        Err(_) => ServerConfig::default(),
    }
}

fn save_config_file(config: &ServerConfig) -> Result<(), String> {
    let path = config_path();
    let parent = path.parent().ok_or_else(|| "无法确定配置目录".to_string())?;
    fs::create_dir_all(parent).map_err(|e| format!("创建配置目录失败：{e}"))?;
    let text = serde_json::to_string_pretty(config).map_err(|e| format!("序列化配置失败：{e}"))?;
    fs::write(path, text).map_err(|e| format!("写入配置失败：{e}"))
}

fn log_line(logs: &LogSink, message: impl AsRef<str>) {
    let now = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default();
    let total = now.as_secs();
    let h = (total / 3600) % 24;
    let m = (total / 60) % 60;
    let s = total % 60;
    let line = format!("[{h:02}:{m:02}:{s:02}] {}", message.as_ref());
    if let Ok(mut guard) = logs.lock() {
        guard.push(line);
        if guard.len() > MAX_LOGS {
            let drop_count = guard.len() - MAX_LOGS;
            guard.drain(0..drop_count);
        }
    }
}

fn detect_local_ipv4() -> Option<Ipv4Addr> {
    let socket = UdpSocket::bind("0.0.0.0:0").ok()?;
    socket.connect("8.8.8.8:80").ok()?;
    match socket.local_addr().ok()?.ip() {
        IpAddr::V4(ip) => Some(ip),
        IpAddr::V6(_) => None,
    }
}

fn local_addresses() -> Vec<String> {
    let mut result = vec!["127.0.0.1".to_string()];
    if let Some(ip) = detect_local_ipv4() {
        if !result.iter().any(|x| x == &ip.to_string()) {
            result.push(ip.to_string());
        }
    }
    result
}

fn validate_config(config: &ServerConfig) -> Result<PathBuf, String> {
    if !matches!(config.service_mode.as_str(), "ftp" | "http" | "both") {
        return Err("服务模式必须是 ftp、http 或 both".into());
    }
    if matches!(config.service_mode.as_str(), "ftp" | "both") && config.ftp_port == 0 {
        return Err("FTP 端口必须在 1-65535 范围内".into());
    }
    if matches!(config.service_mode.as_str(), "http" | "both") && config.http_port == 0 {
        return Err("HTTP 端口必须在 1-65535 范围内".into());
    }
    if config.service_mode == "both" && config.ftp_port == config.http_port {
        return Err("FTP 端口和 HTTP 端口不能相同".into());
    }
    if matches!(config.service_mode.as_str(), "ftp" | "both") {
        if config.passive_start < 1024 || config.passive_end < 1024 {
            return Err("被动端口范围应使用 1024 以上的端口".into());
        }
        if config.passive_start > config.passive_end {
            return Err("被动端口起始值不能大于结束值".into());
        }
    }
    let root = PathBuf::from(config.root_dir.trim());
    if !root.is_dir() {
        return Err(format!("共享目录不存在或不是目录：{}", root.display()));
    }
    fs::canonicalize(root).map_err(|e| format!("无法访问共享目录：{e}"))
}

fn normalize_virtual_path(base: &str, input: &str) -> Result<String, String> {
    let decoded = percent_decode_str(input).decode_utf8_lossy().replace('\\', "/");
    let mut parts: Vec<String> = if decoded.starts_with('/') {
        Vec::new()
    } else {
        base.split('/').filter(|x| !x.is_empty()).map(str::to_owned).collect()
    };

    for part in decoded.split('/') {
        if part.is_empty() || part == "." {
            continue;
        }
        if part == ".." {
            if parts.pop().is_none() {
                return Err("路径不能超出共享根目录".into());
            }
            continue;
        }
        if part.contains(':') || part.contains('\0') {
            return Err("路径包含非法字符".into());
        }
        parts.push(part.to_string());
    }

    if parts.is_empty() {
        Ok("/".into())
    } else {
        Ok(format!("/{}", parts.join("/")))
    }
}

fn virtual_to_local(root: &Path, virtual_path: &str) -> PathBuf {
    let mut path = root.to_path_buf();
    for part in virtual_path.split('/').filter(|x| !x.is_empty()) {
        path.push(part);
    }
    path
}

fn ensure_inside(root: &Path, candidate: &Path, allow_missing_leaf: bool) -> Result<PathBuf, String> {
    let root = fs::canonicalize(root).map_err(|e| format!("共享目录不可访问：{e}"))?;
    if candidate.exists() {
        let canonical = fs::canonicalize(candidate).map_err(|e| format!("无法访问目标：{e}"))?;
        if canonical.starts_with(&root) {
            return Ok(canonical);
        }
        return Err("目标位于共享根目录之外".into());
    }
    if !allow_missing_leaf {
        return Err("目标不存在".into());
    }
    let parent = candidate.parent().ok_or_else(|| "无效目标路径".to_string())?;
    let canonical_parent = fs::canonicalize(parent).map_err(|e| format!("目标目录不可访问：{e}"))?;
    if !canonical_parent.starts_with(&root) {
        return Err("目标目录位于共享根目录之外".into());
    }
    Ok(candidate.to_path_buf())
}

fn safe_existing(root: &Path, candidate: &Path) -> Result<PathBuf, String> {
    ensure_inside(root, candidate, false)
}

fn safe_for_write(root: &Path, candidate: &Path) -> Result<PathBuf, String> {
    ensure_inside(root, candidate, true)
}

#[derive(Clone)]
struct HttpState {
    root: Arc<PathBuf>,
    logs: LogSink,
}

async fn http_handler(State(state): State<HttpState>, uri: Uri) -> Response {
    let raw = uri.path();
    let virtual_path = match normalize_virtual_path("/", raw) {
        Ok(p) => p,
        Err(_) => return simple_response(StatusCode::BAD_REQUEST, "无效路径"),
    };
    let local = virtual_to_local(state.root.as_path(), &virtual_path);
    let local = match safe_existing(state.root.as_path(), &local) {
        Ok(p) => p,
        Err(_) => return simple_response(StatusCode::NOT_FOUND, "文件或目录不存在"),
    };

    match fs::metadata(&local) {
        Ok(metadata) if metadata.is_dir() => match render_directory(&virtual_path, &local) {
            Ok(html) => {
                log_line(&state.logs, format!("HTTP 目录浏览：{}", virtual_path));
                let mut headers = HeaderMap::new();
                headers.insert(header::CONTENT_TYPE, HeaderValue::from_static("text/html; charset=utf-8"));
                (StatusCode::OK, headers, html).into_response()
            }
            Err(_) => simple_response(StatusCode::INTERNAL_SERVER_ERROR, "无法读取目录"),
        },
        Ok(metadata) if metadata.is_file() => match File::open(&local).await {
            Ok(file) => {
                log_line(&state.logs, format!("HTTP 下载：{}", virtual_path));
                let mut headers = HeaderMap::new();
                let mime = mime_guess::from_path(&local).first_or_octet_stream();
                if let Ok(value) = HeaderValue::from_str(mime.as_ref()) {
                    headers.insert(header::CONTENT_TYPE, value);
                }
                headers.insert(header::CONTENT_LENGTH, HeaderValue::from_str(&metadata.len().to_string()).unwrap());
                let filename = local.file_name().and_then(|x| x.to_str()).unwrap_or("download");
                let encoded = utf8_percent_encode(filename, NON_ALPHANUMERIC);
                let disposition = format!("attachment; filename*=UTF-8''{encoded}");
                headers.insert(header::CONTENT_DISPOSITION, HeaderValue::from_str(&disposition).unwrap_or(HeaderValue::from_static("attachment")));
                            let mut response = Response::new(Body::from_stream(ReaderStream::new(file)));
                *response.status_mut() = StatusCode::OK;
                *response.headers_mut() = headers;
                response
            }
            Err(_) => simple_response(StatusCode::NOT_FOUND, "无法读取文件"),
        },
        _ => simple_response(StatusCode::NOT_FOUND, "文件或目录不存在"),
    }
}

fn render_directory(virtual_path: &str, local: &Path) -> Result<String, io::Error> {
    let mut entries = fs::read_dir(local)?.filter_map(Result::ok).collect::<Vec<_>>();
    entries.sort_by_key(|entry| entry.file_name().to_string_lossy().to_lowercase());
    let mut html = String::from(
        "<!doctype html><html lang=\"zh-CN\"><head><meta charset=\"utf-8\"><meta name=\"viewport\" content=\"width=device-width,initial-scale=1\"><title>文件列表</title><style>body{font-family:Segoe UI,Microsoft YaHei,sans-serif;max-width:1000px;margin:30px auto;padding:0 20px;color:#1e293b}a{text-decoration:none;color:#2563eb}li{padding:9px;border-bottom:1px solid #eee}ul{list-style:none;padding:0}.muted{color:#64748b}</style></head><body><h2>文件列表</h2>",
    );
    html.push_str(&format!("<div class=\"muted\">当前位置：{}</div>", html_escape(virtual_path)));
    html.push_str("<ul>");
    if virtual_path != "/" {
        let parent = virtual_parent(virtual_path);
        html.push_str(&format!("<li>📁 <a href=\"{}\">返回上一级</a></li>", href_for_virtual(&parent, true)));
    }
    for entry in entries {
        let name = entry.file_name().to_string_lossy().to_string();
        let meta = match entry.metadata() { Ok(x) => x, Err(_) => continue };
        let is_dir = meta.is_dir();
        let child = if virtual_path == "/" { format!("/{name}") } else { format!("{virtual_path}/{name}") };
        let href = href_for_virtual(&child, is_dir);
        let display = html_escape(&name);
        let prefix = if is_dir { "📁" } else { "📄" };
        let size = if is_dir { String::new() } else { format!(" · {}", format_size(meta.len())) };
        html.push_str(&format!("<li>{prefix} <a href=\"{href}\">{display}</a><span class=\"muted\">{size}</span></li>"));
    }
    html.push_str("</ul></body></html>");
    Ok(html)
}

fn href_for_virtual(path: &str, directory: bool) -> String {
    let mut parts = vec!["".to_string()];
    for part in path.split('/').filter(|x| !x.is_empty()) {
        parts.push(utf8_percent_encode(part, NON_ALPHANUMERIC).to_string());
    }
    let mut value = parts.join("/");
    if directory && !value.ends_with('/') {
        value.push('/');
    }
    if value.is_empty() { "/".to_string() } else { value }
}

fn virtual_parent(path: &str) -> String {
    let mut parts: Vec<&str> = path.split('/').filter(|x| !x.is_empty()).collect();
    parts.pop();
    if parts.is_empty() { "/".into() } else { format!("/{}", parts.join("/")) }
}

fn html_escape(value: impl AsRef<str>) -> String {
    value.as_ref().replace('&', "&amp;").replace('<', "&lt;").replace('>', "&gt;").replace('"', "&quot;").replace('\'', "&#39;")
}

fn format_size(size: u64) -> String {
    const UNITS: &[&str] = &["B", "KB", "MB", "GB", "TB"];
    let mut n = size as f64;
    let mut idx = 0usize;
    while n >= 1024.0 && idx + 1 < UNITS.len() {
        n /= 1024.0;
        idx += 1;
    }
    if idx == 0 { format!("{} {}", size, UNITS[idx]) } else { format!("{n:.1} {}", UNITS[idx]) }
}

fn simple_response(status: StatusCode, text: &str) -> Response {
    Response::builder()
        .status(status)
        .header(header::CONTENT_TYPE, "text/plain; charset=utf-8")
        .body(Body::from(text.to_owned()))
        .unwrap()
}

async fn run_servers(
    config: ServerConfig,
    root: Arc<PathBuf>,
    logs: LogSink,
    shutdown: watch::Receiver<bool>,
    shutdown_tx: watch::Sender<bool>,
) {
    let mode = config.service_mode.as_str();
    let ftp_enabled = matches!(mode, "ftp" | "both");
    let http_enabled = matches!(mode, "http" | "both");

    let ftp_listener = if ftp_enabled {
        match TcpListener::bind(SocketAddr::from(([0, 0, 0, 0], config.ftp_port))).await {
            Ok(x) => Some(x),
            Err(e) => {
                log_line(&logs, format!("FTP 启动失败：{e}"));
                let _ = shutdown_tx.send(true);
                return;
            }
        }
    } else {
        None
    };

    let http_listener = if http_enabled {
        match TcpListener::bind(SocketAddr::from(([0, 0, 0, 0], config.http_port))).await {
            Ok(x) => Some(x),
            Err(e) => {
                log_line(&logs, format!("HTTP 启动失败：{e}"));
                let _ = shutdown_tx.send(true);
                return;
            }
        }
    } else {
        None
    };

    let host = if config.public_host.trim().is_empty() {
        detect_local_ipv4().map(|x| x.to_string()).unwrap_or_else(|| "127.0.0.1".into())
    } else {
        config.public_host.clone()
    };
    log_line(&logs, format!("启动模式：{}", match mode { "ftp" => "仅 FTP", "http" => "仅 HTTP", _ => "FTP + HTTP" }));
    if let Some(listener) = ftp_listener.as_ref() {
        let addr = listener.local_addr().map(|x| x.to_string()).unwrap_or_else(|_| format!("0.0.0.0:{}", config.ftp_port));
        log_line(&logs, format!("FTP 已监听 {addr}"));
    }
    if http_listener.is_some() {
        log_line(&logs, format!("HTTP 已监听 0.0.0.0:{}", config.http_port));
    }
    log_line(&logs, format!("共享目录：{}", root.display()));
    if ftp_enabled && http_enabled {
        log_line(&logs, format!("访问地址：ftp://anonymous@{}:{} / http://{}:{}/", host, config.ftp_port, host, config.http_port));
    } else if ftp_enabled {
        log_line(&logs, format!("访问地址：ftp://anonymous@{}:{}/", host, config.ftp_port));
    } else {
        log_line(&logs, format!("访问地址：http://{}:{}/", host, config.http_port));
    }

    match (ftp_listener, http_listener) {
        (Some(ftp_listener), Some(http_listener)) => {
            let ftp_root = root.clone();
            let ftp_logs = logs.clone();
            let ftp_config = config.clone();
            let ftp_shutdown = shutdown.clone();
            let mut ftp_task = tokio::spawn(async move {
                run_ftp_server(ftp_listener, ftp_root, ftp_config, ftp_logs, ftp_shutdown).await;
            });

            let http_state = HttpState { root: root.clone(), logs: logs.clone() };
            let http_app = Router::new()
                .route("/", get(http_handler))
                .route("/{*path}", get(http_handler))
                .with_state(http_state);
            let http_logs = logs.clone();
            let http_shutdown = shutdown.clone();
            let mut http_task = tokio::spawn(async move {
                let result = axum::serve(http_listener, http_app)
                    .with_graceful_shutdown(async move { wait_for_shutdown(http_shutdown).await })
                    .await;
                if let Err(e) = result {
                    log_line(&http_logs, format!("HTTP 服务结束：{e}"));
                }
            });

            tokio::select! {
                _ = wait_for_shutdown(shutdown.clone()) => {},
                _ = &mut ftp_task => { let _ = shutdown_tx.send(true); },
                _ = &mut http_task => { let _ = shutdown_tx.send(true); },
            }
            let _ = ftp_task.await;
            let _ = http_task.await;
        }
        (Some(ftp_listener), None) => {
            let ftp_root = root.clone();
            let ftp_logs = logs.clone();
            let ftp_config = config.clone();
            let ftp_shutdown = shutdown.clone();
            let mut ftp_task = tokio::spawn(async move {
                run_ftp_server(ftp_listener, ftp_root, ftp_config, ftp_logs, ftp_shutdown).await;
            });
            tokio::select! {
                _ = wait_for_shutdown(shutdown.clone()) => {},
                _ = &mut ftp_task => {},
            }
            let _ = ftp_task.await;
        }
        (None, Some(http_listener)) => {
            let http_state = HttpState { root: root.clone(), logs: logs.clone() };
            let http_app = Router::new()
                .route("/", get(http_handler))
                .route("/{*path}", get(http_handler))
                .with_state(http_state);
            let http_logs = logs.clone();
            let http_shutdown = shutdown.clone();
            let mut http_task = tokio::spawn(async move {
                let result = axum::serve(http_listener, http_app)
                    .with_graceful_shutdown(async move { wait_for_shutdown(http_shutdown).await })
                    .await;
                if let Err(e) = result {
                    log_line(&http_logs, format!("HTTP 服务结束：{e}"));
                }
            });
            tokio::select! {
                _ = wait_for_shutdown(shutdown.clone()) => {},
                _ = &mut http_task => {},
            }
            let _ = http_task.await;
        }
        (None, None) => {}
    }

    log_line(&logs, "服务器已停止");
}

async fn wait_for_shutdown(mut shutdown: watch::Receiver<bool>) {
    if *shutdown.borrow() { return; }
    while shutdown.changed().await.is_ok() {
        if *shutdown.borrow() { return; }
    }
}

async fn run_ftp_server(listener: TcpListener, root: Arc<PathBuf>, config: ServerConfig, logs: LogSink, shutdown: watch::Receiver<bool>) {
    loop {
        tokio::select! {
            _ = wait_for_shutdown(shutdown.clone()) => break,
            accepted = listener.accept() => {
                let Ok((stream, peer)) = accepted else { continue; };
                let root = root.clone();
                let config = config.clone();
                let logs = logs.clone();
                tokio::spawn(async move {
                    log_line(&logs, format!("FTP 客户端连接：{peer}"));
                    if let Err(e) = handle_ftp_client(stream, root, config, logs.clone()).await {
                        log_line(&logs, format!("FTP 客户端结束：{peer}，原因：{e}"));
                    }
                });
            }
        }
    }
}

struct FtpSession {
    control: tokio::net::tcp::OwnedWriteHalf,
    root: Arc<PathBuf>,
    config: ServerConfig,
    cwd: String,
    user: Option<String>,
    authenticated: bool,
    passive: Option<TcpListener>,
    logs: LogSink,
}

async fn handle_ftp_client(stream: TcpStream, root: Arc<PathBuf>, config: ServerConfig, logs: LogSink) -> Result<(), String> {
    let peer = stream.peer_addr().ok();
    let (read_half, write_half) = stream.into_split();
    let mut session = FtpSession {
        control: write_half,
        root,
        config,
        cwd: "/".into(),
        user: None,
        authenticated: false,
        passive: None,
        logs,
    };
    session.reply("220 轻型文件服务器 FTP 服务就绪").await?;
    let mut reader = BufReader::new(read_half);
    loop {
        let mut line = String::new();
        let n = reader.read_line(&mut line).await.map_err(|e| e.to_string())?;
        if n == 0 { break; }
        let line = line.trim_end_matches(['\r', '\n']).to_string();
        let mut pieces = line.splitn(2, ' ');
        let command = pieces.next().unwrap_or("").to_ascii_uppercase();
        let arg = pieces.next().unwrap_or("").trim();
        if command.is_empty() { continue; }
        let should_quit = session.handle_command(&command, arg).await?;
        if should_quit { break; }
    }
    log_line(&session.logs, format!("FTP 断开：{:?}", peer));
    Ok(())
}

impl FtpSession {
    async fn reply(&mut self, text: impl AsRef<str>) -> Result<(), String> {
        let data = format!("{}\r\n", text.as_ref());
        self.control.write_all(data.as_bytes()).await.map_err(|e| e.to_string())
    }

    fn must_auth(&self) -> Result<(), &'static str> {
        if self.authenticated { Ok(()) } else { Err("530 请先登录") }
    }

    fn can_write(&self) -> bool {
        self.authenticated && self.config.anonymous_write_enabled && self.user.as_deref().map(|x| x.eq_ignore_ascii_case("anonymous") || x.eq_ignore_ascii_case("ftp")).unwrap_or(false)
    }

    fn local_for_virtual(&self, virtual_path: &str, write: bool) -> Result<PathBuf, String> {
        let local = virtual_to_local(self.root.as_path(), virtual_path);
        if write { safe_for_write(self.root.as_path(), &local) } else { safe_existing(self.root.as_path(), &local) }
    }

    async fn handle_command(&mut self, command: &str, arg: &str) -> Result<bool, String> {
        match command {
            "USER" => {
                if !self.config.anonymous_enabled {
                    self.reply("530 匿名访问已禁用").await?;
                } else if arg.eq_ignore_ascii_case("anonymous") || arg.eq_ignore_ascii_case("ftp") {
                    self.user = Some("anonymous".into());
                    self.authenticated = false;
                    self.reply("331 匿名登录已接受，请输入任意密码").await?;
                } else {
                    self.reply("530 仅支持匿名用户").await?;
                }
            }
            "PASS" => {
                if self.user.is_some() {
                    self.authenticated = true;
                    self.reply("230 登录成功").await?;
                } else {
                    self.reply("503 请先发送 USER").await?;
                }
            }
            "SYST" => self.reply("215 UNIX Type: L8").await?,
            "FEAT" => {
                self.reply("211-Features").await?;
                self.reply(" UTF8").await?;
                self.reply(" EPSV").await?;
                self.reply(" MDTM").await?;
                self.reply(" SIZE").await?;
                self.reply("211 End").await?;
            }
            "OPTS" => self.reply("200 UTF-8 已启用").await?,
            "TYPE" => self.reply("200 Type set").await?,
            "MODE" => self.reply("200 Mode set to S").await?,
            "NOOP" => self.reply("200 OK").await?,
            "CLNT" => self.reply("200 OK").await?,
            "AUTH" | "PBSZ" | "PROT" => self.reply("502 当前版本未启用 TLS").await?,
            "QUIT" => { self.reply("221 再见").await?; return Ok(true); }
            "PWD" | "XPWD" => self.reply(format!("257 \"{}\" 是当前目录", self.cwd)).await?,
            "CWD" => {
                self.must_auth().map_err(str::to_owned)?;
                let path = normalize_virtual_path(&self.cwd, arg)?;
                let local = self.local_for_virtual(&path, false)?;
                if fs::metadata(local).map(|m| m.is_dir()).unwrap_or(false) {
                    self.cwd = path;
                    self.reply("250 目录已切换").await?;
                } else { self.reply("550 目录不存在").await?; }
            }
            "CDUP" => {
                self.must_auth().map_err(str::to_owned)?;
                let path = virtual_parent(&self.cwd);
                self.cwd = path;
                self.reply("200 已返回上一级目录").await?;
            }
            "PASV" => self.enter_passive(false).await?,
            "EPSV" => self.enter_passive(true).await?,
            "LIST" | "NLST" | "MLSD" => {
                self.must_auth().map_err(str::to_owned)?;
                self.list(command, arg).await?;
            }
            "RETR" => {
                self.must_auth().map_err(str::to_owned)?;
                self.retr(arg).await?;
            }
            "STOR" => {
                self.must_auth().map_err(str::to_owned)?;
                if !self.can_write() { self.reply("550 匿名写入未启用").await?; }
                else { self.stor(arg).await?; }
            }
            "DELE" => {
                self.must_auth().map_err(str::to_owned)?;
                if !self.can_write() { self.reply("550 匿名写入未启用").await?; }
                else {
                    let path = normalize_virtual_path(&self.cwd, arg)?;
                    let local = self.local_for_virtual(&path, false)?;
                    if fs::remove_file(&local).is_ok() { self.reply("250 文件已删除").await?; } else { self.reply("550 删除失败").await?; }
                }
            }
            "MKD" => {
                self.must_auth().map_err(str::to_owned)?;
                if !self.can_write() { self.reply("550 匿名写入未启用").await?; }
                else {
                    let path = normalize_virtual_path(&self.cwd, arg)?;
                    let local = self.local_for_virtual(&path, true)?;
                    match fs::create_dir_all(local) { Ok(_) => self.reply(format!("257 \"{}\" 创建成功", path)).await?, Err(_) => self.reply("550 创建目录失败").await? }
                }
            }
            "RMD" => {
                self.must_auth().map_err(str::to_owned)?;
                if !self.can_write() { self.reply("550 匿名写入未启用").await?; }
                else {
                    let path = normalize_virtual_path(&self.cwd, arg)?;
                    if path == "/" { self.reply("550 不能删除根目录").await?; }
                    else {
                        let local = self.local_for_virtual(&path, false)?;
                        match fs::remove_dir(&local) { Ok(_) => self.reply("250 目录已删除").await?, Err(_) => self.reply("550 删除目录失败（必须为空）").await? }
                    }
                }
            }
            "SIZE" => {
                self.must_auth().map_err(str::to_owned)?;
                let path = normalize_virtual_path(&self.cwd, arg)?;
                match self.local_for_virtual(&path, false).and_then(|p| fs::metadata(p).map_err(|e| e.to_string())) {
                    Ok(meta) => self.reply(format!("213 {}", meta.len())).await?,
                    Err(_) => self.reply("550 文件不存在").await?,
                }
            }
            "MDTM" => self.reply("550 当前版本暂不提供文件时间查询").await?,
            _ => self.reply("502 不支持的命令").await?,
        }
        Ok(false)
    }

    async fn enter_passive(&mut self, extended: bool) -> Result<(), String> {
        let listener = match bind_passive_listener(self.config.passive_start, self.config.passive_end).await {
            Ok(listener) => listener,
            Err(error) => {
                self.reply(format!("425 {error}")).await?;
                return Ok(());
            }
        };
        let port = listener.local_addr().map_err(|e| e.to_string())?.port();
        self.passive = Some(listener);
        if extended {
            self.reply(format!("229 Entering Extended Passive Mode (|||{}|)", port)).await?;
        } else {
            let ip = passive_ipv4(&self.config).unwrap_or(Ipv4Addr::new(127, 0, 0, 1));
            let octets = ip.octets();
            self.reply(format!("227 Entering Passive Mode ({},{},{},{},{},{})", octets[0], octets[1], octets[2], octets[3], port / 256, port % 256)).await?;
        }
        Ok(())
    }

    async fn data_stream(&mut self) -> Result<TcpStream, String> {
        let listener = self.passive.take().ok_or_else(|| "请先进入 PASV/EPSV 模式".to_string())?;
        self.reply("150 正在打开数据连接").await?;
        match timeout(Duration::from_secs(30), listener.accept()).await {
            Ok(Ok((stream, _))) => Ok(stream),
            Ok(Err(e)) => Err(format!("数据连接失败：{e}")),
            Err(_) => Err("等待数据连接超时".into()),
        }
    }

    async fn list(&mut self, command: &str, arg: &str) -> Result<(), String> {
        let path_arg = arg
            .split_whitespace()
            .filter(|token| !token.starts_with('-'))
            .last()
            .unwrap_or("");
        let path = normalize_virtual_path(&self.cwd, path_arg).unwrap_or_else(|_| self.cwd.clone());
        let local = match self.local_for_virtual(&path, false) { Ok(x) => x, Err(_) => { self.reply("550 路径不存在").await?; return Ok(()); } };
        let mut stream = match self.data_stream().await {
            Ok(stream) => stream,
            Err(error) => {
                let _ = self.reply(format!("425 {error}")).await;
                return Ok(());
            }
        };
        let mut output = String::new();
        let entries = fs::read_dir(local).map_err(|e| format!("读取目录失败：{e}"))?;
        for entry in entries.filter_map(Result::ok) {
            let meta = entry.metadata().ok();
            let name = entry.file_name().to_string_lossy().to_string();
            let is_dir = meta.as_ref().map(|m| m.is_dir()).unwrap_or(false);
            let size = meta.as_ref().map(|m| m.len()).unwrap_or(0);
            if command == "NLST" {
                output.push_str(&format!("{}\r\n", name));
            } else if command == "MLSD" {
                output.push_str(&format!("type={};size={}; {}\r\n", if is_dir { "dir" } else { "file" }, size, name));
            } else {
                output.push_str(&format!("{}rwxr-xr-x 1 owner group {:>12} Jan 01 00:00 {}\r\n", if is_dir { 'd' } else { '-' }, size, name));
            }
        }
        stream.write_all(output.as_bytes()).await.map_err(|e| e.to_string())?;
        self.reply("226 传输完成").await?;
        Ok(())
    }

    async fn retr(&mut self, arg: &str) -> Result<(), String> {
        let path = normalize_virtual_path(&self.cwd, arg)?;
        let local = match self.local_for_virtual(&path, false) { Ok(x) => x, Err(_) => { self.reply("550 文件不存在").await?; return Ok(()); } };
        let mut file = match File::open(&local).await { Ok(x) => x, Err(_) => { self.reply("550 无法打开文件").await?; return Ok(()); } };
        let mut data = match self.data_stream().await {
            Ok(stream) => stream,
            Err(error) => {
                let _ = self.reply(format!("425 {error}")).await;
                return Ok(());
            }
        };
        let mut buf = [0u8; 64 * 1024];
        loop {
            let n = file.read(&mut buf).await.map_err(|e| e.to_string())?;
            if n == 0 { break; }
            data.write_all(&buf[..n]).await.map_err(|e| e.to_string())?;
        }
        self.reply("226 传输完成").await?;
        log_line(&self.logs, format!("FTP 下载：{}", path));
        Ok(())
    }

    async fn stor(&mut self, arg: &str) -> Result<(), String> {
        let path = normalize_virtual_path(&self.cwd, arg)?;
        let local = self.local_for_virtual(&path, true)?;
        let mut data = match self.data_stream().await {
            Ok(stream) => stream,
            Err(error) => {
                let _ = self.reply(format!("425 {error}")).await;
                return Ok(());
            }
        };
        let mut file = File::create(&local).await.map_err(|e| format!("创建文件失败：{e}"))?;
        let mut buf = [0u8; 64 * 1024];
        loop {
            let n = data.read(&mut buf).await.map_err(|e| e.to_string())?;
            if n == 0 { break; }
            file.write_all(&buf[..n]).await.map_err(|e| e.to_string())?;
        }
        self.reply("226 上传完成").await?;
        log_line(&self.logs, format!("FTP 上传：{}", path));
        Ok(())
    }
}


async fn bind_passive_listener(start: u16, end: u16) -> Result<TcpListener, String> {
    for port in start..=end {
        match TcpListener::bind(SocketAddr::from(([0, 0, 0, 0], port))).await {
            Ok(listener) => return Ok(listener),
            Err(_) => continue,
        }
    }
    Err("被动端口范围内没有可用端口".into())
}

fn passive_ipv4(config: &ServerConfig) -> Option<Ipv4Addr> {
    if let Ok(ip) = config.public_host.parse::<Ipv4Addr>() { return Some(ip); }
    detect_local_ipv4()
}

#[tauri::command]
fn get_config(state: tauri::State<'_, AppState>) -> ServerConfig {
    state.config.lock().unwrap().clone()
}

#[tauri::command]
fn save_config(state: tauri::State<'_, AppState>, config: ServerConfig) -> Result<(), String> {
    *state.config.lock().map_err(|_| "配置锁异常".to_string())? = config.clone();
    save_config_file(&config)
}

#[tauri::command]
fn reset_config(state: tauri::State<'_, AppState>) -> Result<ServerConfig, String> {
    let config = ServerConfig::default();
    *state.config.lock().map_err(|_| "配置锁异常".to_string())? = config.clone();
    save_config_file(&config)?;
    Ok(config)
}

#[tauri::command]
async fn start_server(state: tauri::State<'_, AppState>, config: ServerConfig) -> Result<(), String> {
    validate_config(&config)?;
    if !config.anonymous_enabled {
        return Err("当前版本的 FTP 只提供匿名账号，请启用匿名访问".into());
    }
    if state.running.load(Ordering::SeqCst) {
        return Err("服务器已经在运行".into());
    }
    if let Some(mut stale) = state.runtime.lock().map_err(|_| "服务器状态锁异常".to_string())?.take() {
        let _ = stale.shutdown.send(true);
        if let Some(thread) = stale.thread.take() { let _ = thread.join(); }
    }
    save_config_file(&config)?;
    *state.config.lock().map_err(|_| "配置锁异常".to_string())? = config.clone();
    let root = Arc::new(fs::canonicalize(&config.root_dir).map_err(|e| e.to_string())?);
    let logs = state.logs.clone();
    let running = state.running.clone();
    let (shutdown_tx, shutdown_rx) = watch::channel(false);
    let thread_tx = shutdown_tx.clone();
    running.store(true, Ordering::SeqCst);
    log_line(&logs, "正在启动服务器…");
    let thread = thread::spawn(move || {
        match Runtime::new() {
            Ok(rt) => rt.block_on(run_servers(config, root, logs.clone(), shutdown_rx, thread_tx)),
            Err(e) => log_line(&logs, format!("创建运行时失败：{e}")),
        }
        running.store(false, Ordering::SeqCst);
    });
    *state.runtime.lock().map_err(|_| "服务器状态锁异常".to_string())? = Some(RuntimeHandle { shutdown: shutdown_tx, thread: Some(thread) });
    Ok(())
}

#[tauri::command]
fn stop_server(state: tauri::State<'_, AppState>) -> Result<(), String> {
    let runtime = state.runtime.lock().map_err(|_| "服务器状态锁异常".to_string())?.take();
    if let Some(mut runtime) = runtime {
        let _ = runtime.shutdown.send(true);
        if let Some(thread) = runtime.thread.take() {
            let _ = thread.join();
        }
    }
    state.running.store(false, Ordering::SeqCst);
    Ok(())
}

#[tauri::command]
fn get_status(state: tauri::State<'_, AppState>) -> ServerStatus {
    ServerStatus { running: state.running.load(Ordering::SeqCst) }
}

#[tauri::command]
fn get_logs(state: tauri::State<'_, AppState>) -> Vec<String> {
    state.logs.lock().map(|x| x.clone()).unwrap_or_default()
}

#[tauri::command]
fn get_local_addresses() -> Vec<String> {
    local_addresses()
}

#[tauri::command]
async fn pick_directory() -> Result<Option<String>, String> {
    Ok(rfd::AsyncFileDialog::new()
        .set_title("选择共享目录")
        .pick_folder()
        .await
        .map(|handle| handle.path().to_string_lossy().to_string()))
}

#[cfg_attr(mobile, tauri::mobile_entry_point)]
pub fn run() {
    tauri::Builder::default()
        .manage(AppState::new())
        .setup(|app| {
            if let Some(window) = app.get_webview_window("main") {
                let _ = window.set_title("轻型文件服务器");
            }
            Ok(())
        })
        .invoke_handler(tauri::generate_handler![
            get_config,
            save_config,
            reset_config,
            start_server,
            stop_server,
            get_status,
            get_logs,
            get_local_addresses,
            pick_directory
        ])
        .run(tauri::generate_context!())
        .expect("运行 Tauri 应用失败");
}
