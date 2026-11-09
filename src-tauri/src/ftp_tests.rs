//! FTP 服务的端到端测试：匿名登录、PASV 应答地址、目录列表数据连接。
//!
//! 这里重点覆盖「Windows 资源管理器打不开 FTP 文件夹」这一类问题：PASV 应答
//! 中的 IPv4 必须是客户端实际可达的地址（本机回环连接应通告 127.0.0.1，而不是
//! 自动探测到的任意网卡地址）。

use std::fs;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{SystemTime, UNIX_EPOCH};

use tokio::io::{AsyncBufReadExt, AsyncReadExt, AsyncWriteExt, BufReader};
use tokio::net::tcp::{OwnedReadHalf, OwnedWriteHalf};
use tokio::net::{TcpListener, TcpStream};
use tokio::sync::watch;

use crate::{run_ftp_server, LogSink, ServerConfig};

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
            "tauri-file-server-ftp-test-{}-{}-{}",
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
}

impl Drop for TempRoot {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.path);
    }
}

/// 取一个当前空闲的 TCP 端口（通过绑定 127.0.0.1:0 得到）。
fn free_port() -> u16 {
    let listener = std::net::TcpListener::bind(("127.0.0.1", 0)).unwrap();
    listener.local_addr().unwrap().port()
}

fn test_config(root_dir: &Path, passive_port: u16) -> ServerConfig {
    ServerConfig {
        service_mode: "ftp".into(),
        root_dir: root_dir.to_string_lossy().to_string(),
        ftp_port: 0,
        http_port: 8088,
        passive_start: passive_port,
        passive_end: passive_port,
        public_host: String::new(),
        anonymous_enabled: true,
        anonymous_write_enabled: false,
    }
}

struct FtpServer {
    addr: std::net::SocketAddr,
    shutdown: watch::Sender<bool>,
    task: tokio::task::JoinHandle<()>,
}

impl Drop for FtpServer {
    fn drop(&mut self) {
        let _ = self.shutdown.send(true);
        self.task.abort();
    }
}

async fn start_ftp(root_dir: &Path, config: ServerConfig) -> FtpServer {
    let listener = TcpListener::bind(("127.0.0.1", 0)).await.unwrap();
    let addr = listener.local_addr().unwrap();
    let (shutdown, shutdown_rx) = watch::channel(false);
    let logs: LogSink = Arc::new(Mutex::new(Vec::new()));
    let root_path = root_dir.to_path_buf();
    let task = tokio::spawn(async move {
        run_ftp_server(listener, Arc::new(root_path), config, logs, shutdown_rx).await;
    });
    FtpServer {
        addr,
        shutdown,
        task,
    }
}

struct FtpClient {
    reader: BufReader<OwnedReadHalf>,
    writer: OwnedWriteHalf,
}

impl FtpClient {
    async fn connect(addr: std::net::SocketAddr) -> Self {
        let stream = TcpStream::connect(addr).await.unwrap();
        let (read_half, write_half) = stream.into_split();
        let mut client = Self {
            reader: BufReader::new(read_half),
            writer: write_half,
        };
        let greeting = client.read_reply().await;
        assert!(greeting.starts_with("220"), "欢迎语异常：{greeting}");
        client
    }

    async fn send(&mut self, command: &str) {
        self.writer
            .write_all(format!("{command}\r\n").as_bytes())
            .await
            .unwrap();
        self.writer.flush().await.unwrap();
    }

    async fn read_reply(&mut self) -> String {
        let mut line = String::new();
        let read = self.reader.read_line(&mut line).await.unwrap();
        assert!(read > 0, "FTP 控制连接意外关闭");
        line.trim_end().to_string()
    }

    /// 发送命令并返回单行应答。
    async fn cmd(&mut self, command: &str) -> String {
        self.send(command).await;
        self.read_reply().await
    }

    async fn login(&mut self) {
        assert!(self.cmd("USER anonymous").await.starts_with("331"));
        assert!(self.cmd("PASS test").await.starts_with("230"));
    }

    /// 进入 PASV，返回 PASV 应答中通告的 (IPv4, 端口)。
    async fn pasv(&mut self) -> (std::net::Ipv4Addr, u16) {
        let reply = self.cmd("PASV").await;
        assert!(reply.starts_with("227"), "PASV 应答异常：{reply}");
        let start = reply.find('(').unwrap();
        let end = reply.find(')').unwrap();
        let numbers: Vec<u16> = reply[start + 1..end]
            .split(',')
            .map(|part| part.trim().parse().unwrap())
            .collect();
        assert_eq!(numbers.len(), 6, "PASV 应答格式异常：{reply}");
        let ip = std::net::Ipv4Addr::new(
            numbers[0] as u8,
            numbers[1] as u8,
            numbers[2] as u8,
            numbers[3] as u8,
        );
        let port = numbers[4] * 256 + numbers[5];
        (ip, port)
    }
}

#[tokio::test]
async fn pasv_advertises_loopback_for_local_client() {
    let root = TempRoot::new();
    fs::write(root.dir().join("hello.txt"), b"hello").unwrap();
    let port = free_port();
    let server = start_ftp(root.dir(), test_config(root.dir(), port)).await;

    let mut client = FtpClient::connect(server.addr).await;
    client.login().await;

    let (ip, data_port) = client.pasv().await;
    assert_eq!(
        ip,
        std::net::Ipv4Addr::new(127, 0, 0, 1),
        "本机客户端应收到 127.0.0.1 的 PASV 通告，而不是其他网卡地址"
    );

    // 用通告出来的地址真正建立数据连接并 LIST，模拟 Windows 资源管理器的流程
    let mut data = TcpStream::connect((ip, data_port)).await.unwrap();
    let reply = client.cmd("LIST").await;
    assert!(reply.starts_with("150"), "LIST 应答异常：{reply}");
    let mut listing = Vec::new();
    data.read_to_end(&mut listing).await.unwrap();
    let listing = String::from_utf8_lossy(&listing);
    assert!(
        listing.contains("hello.txt"),
        "目录列表缺少文件：{listing}"
    );
    assert!(client.read_reply().await.starts_with("226"));

    assert!(client.cmd("QUIT").await.starts_with("221"));
}

#[tokio::test]
async fn pasv_still_respects_public_host_override() {
    let root = TempRoot::new();
    let port = free_port();
    let mut config = test_config(root.dir(), port);
    config.public_host = "192.168.1.50".into();
    let server = start_ftp(root.dir(), config).await;

    let mut client = FtpClient::connect(server.addr).await;
    client.login().await;
    let (ip, _) = client.pasv().await;
    assert_eq!(ip, std::net::Ipv4Addr::new(192, 168, 1, 50));
}
