//! HTTP 服务：浏览器文件浏览页外壳、文件下载、目录列表 JSON API、上传与新建文件夹。
//!
//! 本模块由 `lib.rs` 的 `run_servers` 通过 [`build_http_app`] 构建 Router。
//! 所有请求都复用 `lib.rs` 中的路径安全辅助函数（`normalize_virtual_path`、
//! `virtual_to_local`、`safe_existing`、`safe_for_write`、`virtual_parent`），
//! 保证任何请求都无法越出共享根目录。
//!
//! 接口契约见 `webui/README.md`，三个静态文件（index.html / app.css / app.js）
//! 在编译期通过 `include_str!` 内嵌进二进制。

use axum::{
    body::Body,
    extract::{DefaultBodyLimit, State},
    http::{header, HeaderValue, StatusCode, Uri},
    response::Response,
    routing::{get, post},
    Router,
};
use http_body_util::BodyExt;
use percent_encoding::{
    percent_decode_str, utf8_percent_encode, AsciiSet, CONTROLS, NON_ALPHANUMERIC,
};
use serde_json::json;

/// URL 路径段中需要百分号编码的字符集合。
///
/// 与 `NON_ALPHANUMERIC` 不同，它保留 `.`、`-`、`_`、`~` 等 URL 非保留字符，
/// 因此 JSON 里的 `path` 与 README 示例（如 `"/sub/报告.pdf"`）保持一致。
/// 注意：Content-Disposition 里的文件名仍使用 `NON_ALPHANUMERIC`。
const PATH_SEGMENT_ENCODE_SET: &AsciiSet = &CONTROLS
    .add(b' ')
    .add(b'"')
    .add(b'#')
    .add(b'%')
    .add(b'/')
    .add(b':')
    .add(b'<')
    .add(b'>')
    .add(b'?')
    .add(b'[')
    .add(b'\\')
    .add(b']')
    .add(b'^')
    .add(b'`')
    .add(b'{')
    .add(b'|')
    .add(b'}');
use std::{
    fs,
    path::{Path, PathBuf},
    sync::Arc,
    time::UNIX_EPOCH,
};
use tokio::io::AsyncWriteExt;
use tokio_util::io::ReaderStream;

/// 内嵌的页面外壳
const INDEX_HTML: &str = include_str!("webui/index.html");
/// 内嵌的页面样式
const APP_CSS: &str = include_str!("webui/app.css");
/// 内嵌的页面脚本
const APP_JS: &str = include_str!("webui/app.js");

/// 上传写盘时使用的缓冲大小（64KB）
const UPLOAD_BUFFER_SIZE: usize = 64 * 1024;

/// HTTP 服务的共享状态：共享根目录、日志与「允许匿名写入」开关。
#[derive(Clone)]
pub struct HttpState {
    root: Arc<PathBuf>,
    logs: crate::LogSink,
    write_enabled: bool,
}

impl HttpState {
    /// 创建 HTTP 共享状态。
    pub fn new(root: Arc<PathBuf>, logs: crate::LogSink, write_enabled: bool) -> Self {
        Self {
            root,
            logs,
            write_enabled,
        }
    }
}

/// 构建 HTTP Router（由 `lib.rs` 的 `run_servers` 调用）。
///
/// 注册顺序上先注册 `/_api/*` 与静态资源，最后挂通配路由；axum 对静态路径的
/// 匹配优先级高于通配路由，因此 API 不会被页面下载逻辑拦截。
/// 关闭请求体大小限制以支持大文件上传（上传按块流式写盘，不整文件读入内存）。
pub fn build_http_app(root: Arc<PathBuf>, logs: crate::LogSink, write_enabled: bool) -> Router {
    let state = HttpState::new(root, logs, write_enabled);
    Router::new()
        .route("/", get(page_handler))
        .route("/_assets/app.css", get(serve_css))
        .route("/_assets/app.js", get(serve_js))
        .route("/_api/list", get(api_list))
        .route("/_api/upload", post(http_upload))
        .route("/_api/mkdir", post(api_mkdir))
        .route("/{*path}", get(page_handler))
        .layer(DefaultBodyLimit::disable())
        .with_state(state)
}

// ---------------------------------------------------------------------------
// 静态资源
// ---------------------------------------------------------------------------

/// `GET /_assets/app.css`
async fn serve_css() -> Response {
    static_asset_response(APP_CSS, "text/css; charset=utf-8")
}

/// `GET /_assets/app.js`
async fn serve_js() -> Response {
    static_asset_response(APP_JS, "text/javascript; charset=utf-8")
}

/// 返回内嵌的静态资源，禁止浏览器缓存。
fn static_asset_response(content: &'static str, content_type: &'static str) -> Response {
    let mut response = Response::new(Body::from(content));
    *response.status_mut() = StatusCode::OK;
    let headers = response.headers_mut();
    headers.insert(
        header::CONTENT_TYPE,
        HeaderValue::from_static(content_type),
    );
    headers.insert(header::CACHE_CONTROL, HeaderValue::from_static("no-cache"));
    response
}

// ---------------------------------------------------------------------------
// 目录页面 / 文件下载
// ---------------------------------------------------------------------------

/// `GET /` 与 `GET /{*path}`：目录返回页面外壳，文件保持下载行为。
async fn page_handler(State(state): State<HttpState>, uri: Uri) -> Response {
    let raw = uri.path();
    let virtual_path = match crate::normalize_virtual_path("/", raw) {
        Ok(path) => path,
        Err(_) => return simple_text(StatusCode::BAD_REQUEST, "无效路径"),
    };
    let local = crate::virtual_to_local(state.root.as_path(), &virtual_path);
    let local = match crate::safe_existing(state.root.as_path(), &local) {
        Ok(path) => path,
        Err(_) => return simple_text(StatusCode::NOT_FOUND, "文件或目录不存在"),
    };
    let metadata = match fs::metadata(&local) {
        Ok(meta) => meta,
        Err(_) => return simple_text(StatusCode::NOT_FOUND, "文件或目录不存在"),
    };

    if metadata.is_dir() {
        // 目录地址必须以 “/” 结尾，否则页面里的相对链接会被解析到父目录
        if !raw.ends_with('/') {
            return redirect_to_directory(&uri);
        }
        crate::log_line(&state.logs, format!("HTTP 目录浏览：{virtual_path}"));
        return html_shell_response();
    }

    if metadata.is_file() {
        return match tokio::fs::File::open(&local).await {
            Ok(file) => file_download_response(&state, &virtual_path, &local, metadata.len(), file),
            Err(_) => simple_text(StatusCode::NOT_FOUND, "无法读取文件"),
        };
    }

    simple_text(StatusCode::NOT_FOUND, "文件或目录不存在")
}

/// 返回内嵌的页面外壳（Content-Type: text/html，禁止缓存）。
fn html_shell_response() -> Response {
    let mut response = Response::new(Body::from(INDEX_HTML));
    *response.status_mut() = StatusCode::OK;
    let headers = response.headers_mut();
    headers.insert(
        header::CONTENT_TYPE,
        HeaderValue::from_static("text/html; charset=utf-8"),
    );
    headers.insert(header::CACHE_CONTROL, HeaderValue::from_static("no-cache"));
    response
}

/// 目录 URL 未以 “/” 结尾时 301 重定向到带 “/” 的地址（保留查询串）。
fn redirect_to_directory(uri: &Uri) -> Response {
    let mut location = uri.path().to_string();
    location.push('/');
    if let Some(query) = uri.query() {
        location.push('?');
        location.push_str(query);
    }
    let mut response = Response::new(Body::empty());
    *response.status_mut() = StatusCode::MOVED_PERMANENTLY;
    match HeaderValue::from_str(&location) {
        Ok(value) => {
            response.headers_mut().insert(header::LOCATION, value);
            response
        }
        Err(_) => simple_text(StatusCode::BAD_REQUEST, "无效的重定向地址"),
    }
}

/// 构造文件下载响应：按扩展名猜测 MIME，使用附件方式下载，流式输出文件内容。
fn file_download_response(
    state: &HttpState,
    virtual_path: &str,
    local: &Path,
    size: u64,
    file: tokio::fs::File,
) -> Response {
    crate::log_line(&state.logs, format!("HTTP 下载：{virtual_path}"));
    let mut response = Response::new(Body::from_stream(ReaderStream::new(file)));
    *response.status_mut() = StatusCode::OK;
    let headers = response.headers_mut();

    let mime = mime_guess::from_path(local).first_or_octet_stream();
    if let Ok(value) = HeaderValue::from_str(mime.as_ref()) {
        headers.insert(header::CONTENT_TYPE, value);
    }
    if let Ok(value) = HeaderValue::from_str(&size.to_string()) {
        headers.insert(header::CONTENT_LENGTH, value);
    }
    let filename = local
        .file_name()
        .and_then(|name| name.to_str())
        .unwrap_or("download");
    let encoded = utf8_percent_encode(filename, NON_ALPHANUMERIC).to_string();
    let disposition = format!("attachment; filename*=UTF-8''{encoded}");
    if let Ok(value) = HeaderValue::from_str(&disposition) {
        headers.insert(header::CONTENT_DISPOSITION, value);
    }
    response
}

// ---------------------------------------------------------------------------
// 目录列表 API
// ---------------------------------------------------------------------------

/// `GET /_api/list?dir=<虚拟目录绝对路径>`：按 README 契约返回目录内容 JSON。
async fn api_list(State(state): State<HttpState>, uri: Uri) -> Response {
    let raw_dir = raw_query_param(uri.query(), "dir").unwrap_or("/");
    let virtual_dir = match crate::normalize_virtual_path("/", raw_dir) {
        Ok(path) => path,
        Err(message) => return json_error(StatusCode::BAD_REQUEST, message),
    };

    let local = crate::virtual_to_local(state.root.as_path(), &virtual_dir);
    let local = match crate::safe_existing(state.root.as_path(), &local) {
        Ok(path) => path,
        Err(_) => return json_error(StatusCode::NOT_FOUND, "目录不存在"),
    };
    let metadata = match fs::metadata(&local) {
        Ok(meta) => meta,
        Err(_) => return json_error(StatusCode::NOT_FOUND, "目录不存在"),
    };
    if !metadata.is_dir() {
        return json_error(StatusCode::NOT_FOUND, "目录不存在");
    }

    let mut reader = match tokio::fs::read_dir(&local).await {
        Ok(reader) => reader,
        Err(_) => return json_error(StatusCode::INTERNAL_SERVER_ERROR, "无法读取目录"),
    };

    // 元素为 (排序用小写名称, 是否目录, 对外 JSON)，先收集后排序
    let mut entries: Vec<(String, bool, serde_json::Value)> = Vec::new();
    loop {
        let entry = match reader.next_entry().await {
            Ok(Some(entry)) => entry,
            Ok(None) => break,
            Err(_) => return json_error(StatusCode::INTERNAL_SERVER_ERROR, "无法读取目录"),
        };
        let name = entry.file_name().to_string_lossy().to_string();
        let meta = match entry.metadata().await {
            Ok(meta) => meta,
            Err(_) => continue,
        };
        let is_dir = meta.is_dir();
        let size = if is_dir { 0 } else { meta.len() };
        let modified = meta
            .modified()
            .ok()
            .and_then(|time| time.duration_since(UNIX_EPOCH).ok())
            .map(|duration| duration.as_secs());
        let ext = if is_dir {
            String::new()
        } else {
            Path::new(&name)
                .extension()
                .and_then(|value| value.to_str())
                .map(|value| value.to_ascii_lowercase())
                .unwrap_or_default()
        };
        let child_virtual = if virtual_dir == "/" {
            format!("/{name}")
        } else {
            format!("{virtual_dir}/{name}")
        };
        entries.push((
            name.to_lowercase(),
            is_dir,
            json!({
                "name": name,
                "path": encode_virtual_path(&child_virtual),
                "is_dir": is_dir,
                "size": size,
                "modified": modified,
                "ext": ext,
            }),
        ));
    }
    // 目录在前、文件在后；同类按名称小写升序（前端拿到后还会再排序）
    entries.sort_by(|a, b| b.1.cmp(&a.1).then_with(|| a.0.cmp(&b.0)));
    let entries: Vec<serde_json::Value> = entries
        .into_iter()
        .map(|(_, _, value)| value)
        .collect();

    let parent = crate::virtual_parent(&virtual_dir);
    json_response(
        StatusCode::OK,
        json!({
            "ok": true,
            "dir": virtual_dir,
            "parent": parent,
            "writable": state.write_enabled,
            "entries": entries,
        }),
    )
}

// ---------------------------------------------------------------------------
// 上传 API
// ---------------------------------------------------------------------------

/// `POST /_api/upload?dir=<虚拟目录绝对路径>&path=<相对路径>`。
///
/// `dir` / `path` 均为查询参数（前端用 `encodeURIComponent` 编码）。这里保留原始
/// 编码形态，交给 `normalize_virtual_path` 统一解码一次，避免双重解码把文件名里
/// 的 `%XX` 字样误解码。请求体是单个文件的原始字节，按块流式写入磁盘；文件夹
/// 上传由前端对其中每个文件各发一次请求完成。
async fn http_upload(State(state): State<HttpState>, uri: Uri, body: Body) -> Response {
    // 写入功能关闭时一律拒绝，且不写磁盘
    if !state.write_enabled {
        return json_error(StatusCode::FORBIDDEN, "上传功能未启用");
    }

    let raw_dir = raw_query_param(uri.query(), "dir").unwrap_or("/");
    let virtual_dir = match crate::normalize_virtual_path("/", raw_dir) {
        Ok(path) => path,
        Err(message) => return upload_error(&state, StatusCode::BAD_REQUEST, message),
    };

    // 上传目标目录必须已存在
    let base_local = crate::virtual_to_local(state.root.as_path(), &virtual_dir);
    let base_local = match crate::safe_existing(state.root.as_path(), &base_local) {
        Ok(path) => path,
        Err(_) => return upload_error(&state, StatusCode::NOT_FOUND, "目标目录不存在"),
    };
    match fs::metadata(&base_local) {
        Ok(meta) if meta.is_dir() => {}
        _ => return upload_error(&state, StatusCode::NOT_FOUND, "目标目录不存在"),
    }

    let raw_path = raw_query_param(uri.query(), "path").unwrap_or("").trim();
    if raw_path.is_empty() {
        return upload_error(&state, StatusCode::BAD_REQUEST, "缺少 path 参数");
    }
    // 非法字符校验用解码后的文件名形态（与 normalize_virtual_path 的单次解码一致），
    // 真正构造虚拟路径时再把原始值交给 normalize_virtual_path 解码。
    let decoded_path = percent_decode_str(raw_path).decode_utf8_lossy();
    if decoded_path.is_empty() {
        return upload_error(&state, StatusCode::BAD_REQUEST, "缺少 path 参数");
    }
    if decoded_path.ends_with('/') || decoded_path.ends_with('\\') {
        return upload_error(&state, StatusCode::BAD_REQUEST, "缺少文件名");
    }
    if let Err(message) = validate_relative_upload_path(&decoded_path) {
        return upload_error(&state, StatusCode::BAD_REQUEST, message);
    }
    let virtual_path = match crate::normalize_virtual_path(&virtual_dir, raw_path) {
        Ok(path) => path,
        Err(message) => return upload_error(&state, StatusCode::BAD_REQUEST, message),
    };
    if virtual_path == "/" {
        return upload_error(&state, StatusCode::BAD_REQUEST, "无效的上传路径");
    }

    // 逐级安全创建中间目录（每一级都确认 canonicalize 后仍在根目录内）
    let parent_virtual = crate::virtual_parent(&virtual_path);
    if let Err(message) = create_upload_directories(state.root.as_path(), &parent_virtual) {
        return upload_error(&state, StatusCode::BAD_REQUEST, message);
    }
    let local = crate::virtual_to_local(state.root.as_path(), &virtual_path);
    let local = match crate::safe_for_write(state.root.as_path(), &local) {
        Ok(path) => path,
        Err(message) => return upload_error(&state, StatusCode::BAD_REQUEST, message),
    };
    if local.is_dir() {
        return upload_error(&state, StatusCode::CONFLICT, "同名目录已存在");
    }

    let mut file = match tokio::fs::File::create(&local).await {
        Ok(file) => file,
        Err(error) => {
            return upload_error(
                &state,
                StatusCode::INTERNAL_SERVER_ERROR,
                format!("创建文件失败：{error}"),
            )
        }
    };
    match write_body_to_file(&mut file, body).await {
        Ok(bytes) => {
            crate::log_line(&state.logs, format!("HTTP 上传：{virtual_path}（{bytes} 字节）"));
            json_response(
                StatusCode::OK,
                json!({ "ok": true, "path": encode_virtual_path(&virtual_path) }),
            )
        }
        Err(message) => {
            // 中断或出错时删除半成品文件
            let _ = tokio::fs::remove_file(&local).await;
            upload_error(&state, StatusCode::BAD_REQUEST, message)
        }
    }
}

/// 上传失败：记录日志并返回统一格式的失败 JSON。
fn upload_error(state: &HttpState, status: StatusCode, message: impl Into<String>) -> Response {
    let message: String = message.into();
    crate::log_line(&state.logs, format!("HTTP 上传失败：{message}"));
    json_error(status, message)
}

/// 校验浏览器提交的相对路径：禁止 “..” 越级、冒号、控制字符与空文件名。
fn validate_relative_upload_path(relative: &str) -> Result<(), String> {
    let normalized = relative.replace('\\', "/");
    for part in normalized.split('/') {
        if part.is_empty() || part == "." {
            continue;
        }
        if part == ".." {
            return Err("路径不能超出共享根目录".into());
        }
        if part.contains(':') || part.chars().any(char::is_control) {
            return Err("文件名或路径包含非法字符".into());
        }
    }
    Ok(())
}

/// 逐级创建上传路径中的中间目录。
///
/// 已存在的每一级都会 canonicalize 后确认仍在共享根目录内，避免通过符号链接
/// 跳出共享目录；不存在的级别再逐级创建。
fn create_upload_directories(root: &Path, parent_virtual: &str) -> Result<(), String> {
    let canonical_root =
        fs::canonicalize(root).map_err(|error| format!("共享目录不可访问：{error}"))?;
    let mut current = root.to_path_buf();
    for part in parent_virtual.split('/').filter(|part| !part.is_empty()) {
        current.push(part);
        if current.exists() {
            let canonical = fs::canonicalize(&current)
                .map_err(|error| format!("无法访问目录 {}：{error}", current.display()))?;
            if !canonical.starts_with(&canonical_root) {
                return Err("目标目录位于共享根目录之外".into());
            }
            if !canonical.is_dir() {
                return Err(format!("路径中存在同名文件：{part}"));
            }
        } else {
            fs::create_dir(&current).map_err(|error| format!("创建目录失败：{error}"))?;
        }
    }
    Ok(())
}

/// 把原始请求体按块流式写入文件（64KB 缓冲，不整文件读入内存）。
async fn write_body_to_file(file: &mut tokio::fs::File, mut body: Body) -> Result<u64, String> {
    let mut buffer: Vec<u8> = Vec::with_capacity(UPLOAD_BUFFER_SIZE);
    let mut total: u64 = 0;
    while let Some(frame) = body.frame().await {
        let frame = frame.map_err(|error| format!("读取上传数据失败：{error}"))?;
        if let Ok(data) = frame.into_data() {
            total += data.len() as u64;
            buffer.extend_from_slice(&data);
            if buffer.len() >= UPLOAD_BUFFER_SIZE {
                file.write_all(&buffer)
                    .await
                    .map_err(|error| format!("写入文件失败：{error}"))?;
                buffer.clear();
            }
        }
    }
    if !buffer.is_empty() {
        file.write_all(&buffer)
            .await
            .map_err(|error| format!("写入文件失败：{error}"))?;
    }
    file.flush()
        .await
        .map_err(|error| format!("写入文件失败：{error}"))?;
    Ok(total)
}

// ---------------------------------------------------------------------------
// 新建文件夹 API
// ---------------------------------------------------------------------------

/// `POST /_api/mkdir?dir=<虚拟父目录>&name=<新文件夹名>`：创建文件夹。
async fn api_mkdir(State(state): State<HttpState>, uri: Uri) -> Response {
    if !state.write_enabled {
        return json_error(StatusCode::FORBIDDEN, "上传功能未启用");
    }

    let raw_dir = raw_query_param(uri.query(), "dir").unwrap_or("/");
    let raw_name = raw_query_param(uri.query(), "name").unwrap_or("");
    let name = decode_query_value(raw_name);
    if let Err(message) = validate_folder_name(&name) {
        return mkdir_error(&state, StatusCode::BAD_REQUEST, message);
    }

    let virtual_dir = match crate::normalize_virtual_path("/", raw_dir) {
        Ok(path) => path,
        Err(message) => return mkdir_error(&state, StatusCode::BAD_REQUEST, message),
    };
    let parent_local = crate::virtual_to_local(state.root.as_path(), &virtual_dir);
    let parent_local = match crate::safe_existing(state.root.as_path(), &parent_local) {
        Ok(path) => path,
        Err(_) => return mkdir_error(&state, StatusCode::NOT_FOUND, "父目录不存在"),
    };
    match fs::metadata(&parent_local) {
        Ok(meta) if meta.is_dir() => {}
        _ => return mkdir_error(&state, StatusCode::NOT_FOUND, "父目录不存在"),
    }

    let target_virtual = if virtual_dir == "/" {
        format!("/{name}")
    } else {
        format!("{virtual_dir}/{name}")
    };
    let target_local = crate::virtual_to_local(state.root.as_path(), &target_virtual);
    if target_local.exists() {
        return mkdir_error(&state, StatusCode::CONFLICT, "目录已存在");
    }
    let target_local = match crate::safe_for_write(state.root.as_path(), &target_local) {
        Ok(path) => path,
        Err(message) => return mkdir_error(&state, StatusCode::FORBIDDEN, message),
    };

    match tokio::fs::create_dir(&target_local).await {
        Ok(()) => {
            crate::log_line(&state.logs, format!("HTTP 新建文件夹：{target_virtual}"));
            json_response(
                StatusCode::OK,
                json!({ "ok": true, "path": encode_virtual_path(&target_virtual) }),
            )
        }
        Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => {
            mkdir_error(&state, StatusCode::CONFLICT, "目录已存在")
        }
        Err(error) => {
            let message = format!("创建文件夹失败：{error}");
            mkdir_error(&state, StatusCode::INTERNAL_SERVER_ERROR, message)
        }
    }
}

/// 建文件夹失败：记录日志并返回统一格式的失败 JSON。
fn mkdir_error(state: &HttpState, status: StatusCode, message: impl Into<String>) -> Response {
    let message: String = message.into();
    crate::log_line(&state.logs, format!("HTTP 新建文件夹失败：{message}"));
    json_error(status, message)
}

/// 校验新文件夹名称：只允许单段名称，拒绝空名、级联路径、控制字符与 Windows 保留名。
fn validate_folder_name(name: &str) -> Result<(), String> {
    if name.is_empty() {
        return Err("文件夹名称不能为空".into());
    }
    if name == "." || name == ".." {
        return Err("文件夹名称非法".into());
    }
    if name.contains('/') || name.contains('\\') || name.contains(':') {
        return Err("文件夹名称不能包含 / \\ :".into());
    }
    if name.chars().any(char::is_control) {
        return Err("文件夹名称不能包含控制字符".into());
    }
    if name.ends_with('.') || name.ends_with(' ') {
        return Err("文件夹名称非法".into());
    }
    // Windows 保留设备名：扩展名之前的部分不区分大小写地匹配即非法
    const RESERVED: &[&str] = &[
        "CON", "PRN", "AUX", "NUL", "COM1", "COM2", "COM3", "COM4", "COM5", "COM6", "COM7",
        "COM8", "COM9", "LPT1", "LPT2", "LPT3", "LPT4", "LPT5", "LPT6", "LPT7", "LPT8", "LPT9",
    ];
    let stem = name.split('.').next().unwrap_or(name).trim_end();
    if RESERVED
        .iter()
        .any(|reserved| stem.eq_ignore_ascii_case(reserved))
    {
        return Err("文件夹名称是系统保留名".into());
    }
    Ok(())
}

// ---------------------------------------------------------------------------
// 通用辅助函数
// ---------------------------------------------------------------------------

/// 统一 JSON 响应：`Content-Type: application/json; charset=utf-8`。
fn json_response(status: StatusCode, body: serde_json::Value) -> Response {
    let mut response = Response::new(Body::from(body.to_string()));
    *response.status_mut() = status;
    response.headers_mut().insert(
        header::CONTENT_TYPE,
        HeaderValue::from_static("application/json; charset=utf-8"),
    );
    response
}

/// 统一失败 JSON：`{"ok":false,"error":"中文原因"}`。
fn json_error(status: StatusCode, message: impl Into<String>) -> Response {
    let message: String = message.into();
    json_response(status, json!({ "ok": false, "error": message }))
}

/// 页面类请求出错时的纯文本响应（保持旧版 HTTP 浏览页的行为）。
fn simple_text(status: StatusCode, text: &str) -> Response {
    let mut response = Response::new(Body::from(text.to_owned()));
    *response.status_mut() = status;
    response.headers_mut().insert(
        header::CONTENT_TYPE,
        HeaderValue::from_static("text/plain; charset=utf-8"),
    );
    response
}

/// 把虚拟路径的每一段分别做百分号编码，再以 “/” 拼接（用于 JSON 的 path 字段）。
fn encode_virtual_path(virtual_path: &str) -> String {
    let parts: Vec<String> = virtual_path
        .split('/')
        .filter(|part| !part.is_empty())
        .map(|part| utf8_percent_encode(part, PATH_SEGMENT_ENCODE_SET).to_string())
        .collect();
    if parts.is_empty() {
        "/".to_string()
    } else {
        format!("/{}", parts.join("/"))
    }
}

/// 从原始查询串中提取参数，返回值保持百分号编码原样。
///
/// 目录参数交给 `normalize_virtual_path` 统一解码一次，避免重复解码。
fn raw_query_param<'a>(query: Option<&'a str>, key: &str) -> Option<&'a str> {
    let query = query?;
    for pair in query.split('&') {
        if pair.is_empty() {
            continue;
        }
        let mut iter = pair.splitn(2, '=');
        let raw_key = iter.next().unwrap_or("");
        let value = iter.next().unwrap_or("");
        if raw_key == key {
            return Some(value);
        }
    }
    None
}

/// 解码表单编码的查询参数值（`+` 表示空格，`%XX` 表示字节）。
fn decode_query_value(raw: &str) -> String {
    let with_spaces = raw.replace('+', " ");
    percent_decode_str(&with_spaces).decode_utf8_lossy().into_owned()
}
