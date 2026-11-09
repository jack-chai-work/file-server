//! HTTP 服务的端到端测试：用真实 TCP 连接请求内嵌的 axum 应用，
//! 覆盖目录列表 JSON、静态资源、目录页重定向、文件下载、上传、新建文件夹、
//! 写权限开关与路径越界防护。

use std::fs;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{SystemTime, UNIX_EPOCH};

use percent_encoding::{utf8_percent_encode, NON_ALPHANUMERIC};
use serde_json::Value;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};

use crate::http::build_http_app;
use crate::LogSink;

static TEST_ID: AtomicU64 = AtomicU64::new(0);

struct TempRoot {
    path: PathBuf,
}

impl TempRoot {
    fn new() -> Self {
        let id = TEST_ID.fetch_add(1, Ordering::SeqCst);
        let nanos = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap_or_default()
            .as_nanos();
        let path = std::env::temp_dir().join(format!(
            "tauri-file-server-http-test-{}-{}-{}",
            std::process::id(),
            nanos,
            id
        ));
        fs::create_dir_all(&path).unwrap();
        Self { path }
    }

    fn dir(&self) -> &Path {
        &self.path
    }

    fn write(&self, relative: &str, content: &[u8]) {
        let path = self.path.join(relative);
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent).unwrap();
        }
        fs::write(path, content).unwrap();
    }
}

impl Drop for TempRoot {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.path);
    }
}

struct Server {
    addr: std::net::SocketAddr,
    task: tokio::task::JoinHandle<()>,
}

impl Drop for Server {
    fn drop(&mut self) {
        self.task.abort();
    }
}

async fn start_server(root: &Path, writable: bool) -> Server {
    let listener = TcpListener::bind(("127.0.0.1", 0)).await.unwrap();
    let addr = listener.local_addr().unwrap();
    let logs: LogSink = Arc::new(Mutex::new(Vec::new()));
    let app = build_http_app(Arc::new(root.to_path_buf()), logs, writable);
    let task = tokio::spawn(async move {
        let _ = axum::serve(listener, app).await;
    });
    Server { addr, task }
}

struct HttpResponse {
    status: u16,
    headers: Vec<(String, String)>,
    body: Vec<u8>,
}

impl HttpResponse {
    fn header(&self, name: &str) -> Option<&str> {
        self.headers
            .iter()
            .find(|(key, _)| key.eq_ignore_ascii_case(name))
            .map(|(_, value)| value.as_str())
    }

    fn json(&self) -> Value {
        serde_json::from_slice(&self.body).unwrap_or(Value::Null)
    }

    fn text(&self) -> String {
        String::from_utf8_lossy(&self.body).to_string()
    }
}

async fn send_raw(addr: std::net::SocketAddr, request: &[u8]) -> HttpResponse {
    let mut stream = TcpStream::connect(addr).await.unwrap();
    stream.write_all(request).await.unwrap();
    stream.flush().await.unwrap();
    let mut raw = Vec::new();
    stream.read_to_end(&mut raw).await.unwrap();
    parse_response(&raw)
}

fn parse_response(raw: &[u8]) -> HttpResponse {
    let split = raw
        .windows(4)
        .position(|window| window == b"\r\n\r\n")
        .expect("HTTP 响应缺少头部结束标记");
    let head = String::from_utf8_lossy(&raw[..split]).to_string();
    let mut lines = head.split("\r\n");
    let status = lines
        .next()
        .and_then(|line| line.split_whitespace().nth(1))
        .and_then(|code| code.parse::<u16>().ok())
        .expect("无法解析状态码");
    let headers: Vec<(String, String)> = lines
        .filter_map(|line| {
            line.split_once(':')
                .map(|(key, value)| (key.trim().to_ascii_lowercase(), value.trim().to_string()))
        })
        .collect();
    let mut body = raw[split + 4..].to_vec();
    let response = HttpResponse {
        status,
        headers,
        body: Vec::new(),
    };
    if response
        .header("transfer-encoding")
        .map(|value| value.to_ascii_lowercase().contains("chunked"))
        .unwrap_or(false)
    {
        body = decode_chunked(&body);
    } else if let Some(length) = response
        .header("content-length")
        .and_then(|value| value.parse::<usize>().ok())
    {
        body.truncate(length.min(body.len()));
    }
    HttpResponse { body, ..response }
}

fn decode_chunked(input: &[u8]) -> Vec<u8> {
    let mut output = Vec::new();
    let mut rest = input;
    loop {
        let Some(line_end) = rest.windows(2).position(|window| window == b"\r\n") else {
            break;
        };
        let size_text = String::from_utf8_lossy(&rest[..line_end]);
        let size = usize::from_str_radix(size_text.trim(), 16).unwrap_or(0);
        rest = &rest[line_end + 2..];
        if size == 0 {
            break;
        }
        let take = size.min(rest.len());
        output.extend_from_slice(&rest[..take]);
        rest = &rest[take..];
        if rest.starts_with(b"\r\n") {
            rest = &rest[2..];
        }
    }
    output
}

fn enc(value: &str) -> String {
    utf8_percent_encode(value, NON_ALPHANUMERIC).to_string()
}

fn host_header(addr: std::net::SocketAddr) -> String {
    format!("{addr}")
}

async fn get(addr: std::net::SocketAddr, path: &str) -> HttpResponse {
    let request = format!(
        "GET {path} HTTP/1.1\r\nHost: {}\r\nConnection: close\r\n\r\n",
        host_header(addr)
    );
    send_raw(addr, request.as_bytes()).await
}

async fn post(addr: std::net::SocketAddr, path: &str, content_type: &str, body: &[u8]) -> HttpResponse {
    let mut request = format!(
        "POST {path} HTTP/1.1\r\nHost: {}\r\nConnection: close\r\nContent-Type: {content_type}\r\nContent-Length: {}\r\n\r\n",
        host_header(addr),
        body.len()
    )
    .into_bytes();
    request.extend_from_slice(body);
    send_raw(addr, &request).await
}

/// 构造 multipart 之外的原始上传请求：请求体就是文件内容。
async fn upload_file(
    addr: std::net::SocketAddr,
    dir: &str,
    relative_path: &str,
    content: &[u8],
) -> HttpResponse {
    let path = format!(
        "/_api/upload?dir={}&path={}",
        enc(dir),
        enc(relative_path)
    );
    post(addr, &path, "application/octet-stream", content).await
}

#[tokio::test]
async fn list_root_returns_entries_with_extension_and_metadata() {
    let root = TempRoot::new();
    root.write("a.txt", b"hello");
    root.write("B.PNG", b"image");
    root.write("noext", b"raw");
    root.write("sub/c.md", b"# title");

    let server = start_server(root.dir(), true).await;
    let response = get(server.addr, "/_api/list?dir=%2F").await;

    assert_eq!(response.status, 200);
    assert!(response
        .header("content-type")
        .unwrap_or_default()
        .contains("application/json"));
    let json = response.json();
    assert_eq!(json["ok"], true);
    assert_eq!(json["dir"], "/");
    assert_eq!(json["parent"], "/");
    assert_eq!(json["writable"], true);
    let entries = json["entries"].as_array().expect("entries 应为数组");
    let find = |name: &str| {
        entries
            .iter()
            .find(|entry| entry["name"] == name)
            .unwrap_or_else(|| panic!("缺少条目 {name}"))
            .clone()
    };

    let a = find("a.txt");
    assert_eq!(a["is_dir"], false);
    assert_eq!(a["size"], 5);
    assert_eq!(a["ext"], "txt");
    assert_eq!(a["path"], "/a.txt");

    let b = find("B.PNG");
    assert_eq!(b["ext"], "png");

    let noext = find("noext");
    assert_eq!(noext["ext"], "");

    let sub = find("sub");
    assert_eq!(sub["is_dir"], true);
    assert_eq!(sub["size"], 0);
    assert_eq!(sub["ext"], "");
    assert_eq!(sub["path"], "/sub");
}

#[tokio::test]
async fn list_uses_root_when_dir_is_omitted() {
    let root = TempRoot::new();
    root.write("x.txt", b"x");
    let server = start_server(root.dir(), true).await;
    let response = get(server.addr, "/_api/list").await;
    assert_eq!(response.status, 200);
    assert_eq!(response.json()["dir"], "/");
}

#[tokio::test]
async fn list_subdirectory_reports_parent() {
    let root = TempRoot::new();
    root.write("sub/c.md", b"# title");
    let server = start_server(root.dir(), true).await;
    let response = get(server.addr, "/_api/list?dir=%2Fsub").await;
    assert_eq!(response.status, 200);
    let json = response.json();
    assert_eq!(json["dir"], "/sub");
    assert_eq!(json["parent"], "/");
    assert_eq!(json["entries"][0]["name"], "c.md");
}

#[tokio::test]
async fn list_missing_directory_and_traversal_are_rejected() {
    let root = TempRoot::new();
    let server = start_server(root.dir(), true).await;

    let missing = get(server.addr, "/_api/list?dir=%2Fmissing").await;
    assert_eq!(missing.status, 404);
    assert_eq!(missing.json()["ok"], false);

    let traversal = get(server.addr, "/_api/list?dir=%2F..%2F").await;
    assert_eq!(traversal.status, 400);
    assert_eq!(traversal.json()["ok"], false);
}

#[tokio::test]
async fn directory_page_redirects_to_trailing_slash_and_serves_shell() {
    let root = TempRoot::new();
    root.write("sub/c.md", b"# title");
    let server = start_server(root.dir(), true).await;

    let redirect = get(server.addr, "/sub").await;
    assert_eq!(redirect.status, 301);
    assert_eq!(redirect.header("location"), Some("/sub/"));

    let shell = get(server.addr, "/sub/").await;
    assert_eq!(shell.status, 200);
    assert!(shell
        .header("content-type")
        .unwrap_or_default()
        .contains("text/html"));
    let html = shell.text();
    assert!(html.contains("/_assets/app.js"), "目录页应引用内嵌脚本");
    assert!(html.contains("/_assets/app.css"), "目录页应引用内嵌样式");
}

#[tokio::test]
async fn embedded_assets_are_served_with_correct_content_types() {
    let root = TempRoot::new();
    let server = start_server(root.dir(), true).await;

    let js = get(server.addr, "/_assets/app.js").await;
    assert_eq!(js.status, 200);
    assert!(js
        .header("content-type")
        .unwrap_or_default()
        .contains("javascript"));
    assert!(!js.body.is_empty());

    let css = get(server.addr, "/_assets/app.css").await;
    assert_eq!(css.status, 200);
    assert!(css
        .header("content-type")
        .unwrap_or_default()
        .contains("text/css"));
    assert!(!css.body.is_empty());
}

#[tokio::test]
async fn file_download_returns_content_and_attachment_header() {
    let root = TempRoot::new();
    root.write("a.txt", b"hello world");
    let server = start_server(root.dir(), true).await;
    let response = get(server.addr, "/a.txt").await;
    assert_eq!(response.status, 200);
    assert_eq!(response.text(), "hello world");
    assert!(response
        .header("content-disposition")
        .unwrap_or_default()
        .contains("attachment"));
}

#[tokio::test]
async fn upload_streams_file_and_creates_nested_directories() {
    let root = TempRoot::new();
    let server = start_server(root.dir(), true).await;

    let content = b"hello upload".to_vec();
    let response = upload_file(server.addr, "/", "dir1/dir2/hello.txt", &content).await;
    assert_eq!(response.status, 200, "响应：{}", response.text());
    let json = response.json();
    assert_eq!(json["ok"], true);
    assert_eq!(json["path"], "/dir1/dir2/hello.txt");

    let saved = fs::read(root.dir().join("dir1/dir2/hello.txt")).unwrap();
    assert_eq!(saved, content);
}

#[tokio::test]
async fn upload_supports_chinese_names() {
    let root = TempRoot::new();
    let server = start_server(root.dir(), true).await;
    let response = upload_file(server.addr, "/", "资料/说明.txt", "中文内容".as_bytes()).await;
    assert_eq!(response.status, 200, "响应：{}", response.text());
    let saved = fs::read(root.dir().join("资料/说明.txt")).unwrap();
    assert_eq!(String::from_utf8_lossy(&saved), "中文内容");
}

#[tokio::test]
async fn upload_requires_write_permission() {
    let root = TempRoot::new();
    let server = start_server(root.dir(), false).await;
    let response = upload_file(server.addr, "/", "a.txt", b"x").await;
    assert_eq!(response.status, 403);
    assert_eq!(response.json()["ok"], false);
    assert!(!root.dir().join("a.txt").exists());
}

#[tokio::test]
async fn upload_rejects_path_traversal() {
    let root = TempRoot::new();
    let server = start_server(root.dir(), true).await;

    for bad in ["../evil.txt", "..\\evil.txt", "dir/../../evil.txt"] {
        let response = upload_file(server.addr, "/", bad, b"evil").await;
        assert!(
            response.status == 400 || response.status == 403,
            "越界路径 {bad} 应被拒绝，实际状态 {}",
            response.status
        );
        assert_eq!(response.json()["ok"], false);
    }
    assert!(!root.path.parent().unwrap().join("evil.txt").exists());
}

#[tokio::test]
async fn mkdir_creates_directory_and_reports_conflict() {
    let root = TempRoot::new();
    let server = start_server(root.dir(), true).await;

    let path = format!("/_api/mkdir?dir=%2F&name={}", enc("新文件夹"));
    let response = post(server.addr, &path, "text/plain", b"").await;
    assert_eq!(response.status, 200, "响应：{}", response.text());
    let json = response.json();
    assert_eq!(json["ok"], true);
    assert!(root.dir().join("新文件夹").is_dir());

    let again = post(server.addr, &path, "text/plain", b"").await;
    assert_eq!(again.status, 409);
    assert_eq!(again.json()["ok"], false);
}

#[tokio::test]
async fn mkdir_validates_name_and_write_permission() {
    let root = TempRoot::new();
    let server = start_server(root.dir(), true).await;
    let bad = format!("/_api/mkdir?dir=%2F&name={}", enc("a/b"));
    assert_eq!(post(server.addr, &bad, "text/plain", b"").await.status, 400);

    let root2 = TempRoot::new();
    let readonly = start_server(root2.dir(), false).await;
    let path = "/_api/mkdir?dir=%2F&name=%E6%96%B0";
    assert_eq!(
        post(readonly.addr, path, "text/plain", b"").await.status,
        403
    );
}

#[tokio::test]
async fn list_reports_readonly_writable_flag() {
    let root = TempRoot::new();
    let server = start_server(root.dir(), false).await;
    let response = get(server.addr, "/_api/list?dir=%2F").await;
    assert_eq!(response.status, 200);
    assert_eq!(response.json()["writable"], false);
}

/// 手动 UI 检查用：在固定端口 18765 启动一个带示例文件的只读服务器。
///
/// 运行：`cargo test --lib serve_ui_fixture -- --ignored --nocapture`
#[tokio::test]
#[ignore = "手动 UI 检查时启动固定端口示例服务器"]
async fn serve_ui_fixture() {
    let root = TempRoot::new();
    root.write("照片/旅行 2026.jpg", b"jpeg-data");
    root.write("照片/合影.png", b"png-data");
    root.write("视频/演示.mp4", b"mp4-data");
    root.write("音频/歌曲.mp3", b"mp3-data");
    root.write("文档/说明.pdf", b"pdf-data");
    root.write("文档/README.md", b"# readme");
    root.write("代码/main.rs", b"fn main() {}");
    root.write("压缩包/备份.zip", b"zip-data");
    root.write("字体/字体.ttf", b"font-data");
    root.write("程序/工具.exe", b"exe-data");
    root.write("未分类文件", b"raw");
    root.write("空目录/.keep", b"");
    // 根目录也放一些不同类型的文件，方便手动检查「按后缀分类」效果
    root.write("示例图片.png", b"png-data");
    root.write("示例视频.mp4", b"mp4-data");
    root.write("示例音频.mp3", b"mp3-data");
    root.write("示例文档.pdf", b"pdf-data");
    root.write("示例文档.md", b"# readme");
    root.write("示例代码.rs", b"fn main() {}");
    root.write("示例压缩包.zip", b"zip-data");
    root.write("示例字体.ttf", b"font-data");
    root.write("示例程序.exe", b"exe-data");
    root.write("说明.txt", b"hello");

    let listener = TcpListener::bind(("127.0.0.1", 18765)).await.unwrap();
    println!(
        "UI_FIXTURE_URL=http://127.0.0.1:18765/ ROOT={}",
        root.path.display()
    );
    let logs: LogSink = Arc::new(Mutex::new(Vec::new()));
    let app = build_http_app(Arc::new(root.path.clone()), logs, true);
    let _ = axum::serve(listener, app).await;
}

