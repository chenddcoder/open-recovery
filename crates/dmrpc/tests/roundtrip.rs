//! helper 协议端到端测试：真起一个服务端线程，用真客户端跑一遍浏览/预览/恢复。
//!
//! 需要一个真实的 FAT16 镜像。默认用 `scripts/make-test-images.sh` 产出的
//! `/tmp/dmde-test/fat16raw.dmg`，也可以用 `DMDE_TEST_IMAGE` 指定；找不到就跳过
//! （CI 里没镜像时不该假装通过）。

use dmrpc::client::Client;
use dmrpc::proto::Request;
use dmrpc::server::{serve, ServeOptions};
use std::io::Write;
use std::path::{Path, PathBuf};
use std::time::Duration;

fn test_image() -> Option<PathBuf> {
    let candidates: Vec<PathBuf> = std::env::var("DMDE_TEST_IMAGE")
        .ok()
        .map(PathBuf::from)
        .into_iter()
        .chain([PathBuf::from("/tmp/dmde-test/fat16raw.dmg")])
        .collect();
    candidates.into_iter().find(|p| p.is_file())
}

fn socket_path() -> PathBuf {
    let pid = std::process::id();
    let stamp = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_nanos())
        .unwrap_or(0);
    std::env::temp_dir().join(format!("dmde-rpc-test-{pid}-{stamp}.sock"))
}

fn spawn_server(socket: &Path, token: &str) -> std::thread::JoinHandle<std::io::Result<()>> {
    let opt = ServeOptions {
        socket: socket.to_path_buf(),
        token: token.to_string(),
        uid: None,
        gid: None,
        idle_secs: 30,
    };
    std::thread::spawn(move || serve(&opt))
}

/// 服务端是异步起来的，轮询到能连上为止。
fn connect(socket: &Path, token: &str) -> Client {
    for _ in 0..200 {
        match Client::connect(socket, token) {
            Ok(c) => return c,
            Err(_) => std::thread::sleep(Duration::from_millis(25)),
        }
    }
    panic!("helper 在 5 秒内没起来: {}", socket.display());
}

#[test]
fn token_is_required() {
    let socket = socket_path();
    let _server = spawn_server(&socket, "right-token");

    // 先等 socket 存在，再试错 token
    for _ in 0..200 {
        if socket.exists() {
            break;
        }
        std::thread::sleep(Duration::from_millis(25));
    }
    let err = Client::connect(&socket, "wrong-token").err().expect("错 token 必须被拒绝");
    assert!(
        err.to_string().contains("token"),
        "错误信息应指出 token 问题: {err}"
    );

    // 错 token 后服务端应断开该连接，但仍在监听，正确 token 依然可用
    let mut ok = connect(&socket, "right-token");
    ok.ping().expect("正确 token 应能 ping 通");
    ok.shutdown();
}

#[test]
fn hostile_request_before_hello_is_rejected() {
    use std::io::BufReader;
    let socket = socket_path();
    let _server = spawn_server(&socket, "tok");
    for _ in 0..200 {
        if socket.exists() {
            break;
        }
        std::thread::sleep(Duration::from_millis(25));
    }
    let stream = std::os::unix::net::UnixStream::connect(&socket).expect("连接 helper");
    let mut w = stream.try_clone().unwrap();
    let mut r = BufReader::new(stream);
    // 不发 hello 直接要磁盘列表
    dmrpc::proto::write_request(&mut w, &Request::Disks).expect("写请求");
    let reply = dmrpc::proto::read_reply(&mut r).expect("读响应");
    match reply {
        dmrpc::proto::Reply::Error { message } => assert!(message.contains("hello"), "{message}"),
        other => panic!("应当被拒绝，实际: {other:?}"),
    }
}

#[test]
fn browse_preview_and_recover_over_socket() {
    let Some(image) = test_image() else {
        eprintln!("跳过：找不到测试镜像（先跑 scripts/make-test-images.sh）");
        return;
    };
    let socket = socket_path();
    let _server = spawn_server(&socket, "secret");
    let mut c = connect(&socket, "secret");

    let src = c
        .open(image.to_str().unwrap(), None)
        .unwrap_or_else(|e| panic!("打开 {} 失败: {e}", image.display()));
    assert_eq!(src.fs_kind, "FAT16", "镜像类型不对: {src:?}");

    // 目录列表：应看到 hello.txt 与 docs
    let entries = c.list_dir("/", false).expect("列根目录");
    let names: Vec<&str> = entries.iter().map(|e| e.name.as_str()).collect();
    assert!(names.contains(&"hello.txt"), "根目录缺少 hello.txt: {names:?}");
    assert!(names.contains(&"docs"), "根目录缺少 docs: {names:?}");

    // 预览
    let p = c.preview("/hello.txt", 4096).expect("预览 hello.txt");
    assert_eq!(p.mode, "text");
    assert!(p.text.contains("hello fat16"), "预览内容不对: {:?}", p.text);

    // 按路径读全量，验证分块传输的字节完整性
    let mut buf = Vec::new();
    let n = c.read_path("/hello.txt", &mut buf).expect("读 hello.txt");
    assert_eq!(n as usize, buf.len(), "End.total 与实收字节数不一致");
    assert_eq!(buf, b"hello fat16 world\n");

    // 已删除项：按 id 读回内容
    let deleted = c.deleted(100).expect("扫描已删除");
    assert!(!deleted.is_empty(), "应至少有一个已删除文件");
    let mut recovered = 0usize;
    for e in &deleted {
        if e.is_dir {
            continue;
        }
        let mut out = Vec::new();
        if let Ok(n) = c.read_id(e.id, &mut out) {
            assert_eq!(n as usize, out.len(), "{} 的字节数不一致", e.name);
            assert!(!out.is_empty(), "{} 恢复出 0 字节", e.name);
            recovered += 1;
        }
    }
    assert!(recovered >= 1, "一个都没恢复出来: {deleted:?}");

    // 未知 id 必须是干净的报错，而不是断开连接
    let mut sink = Vec::new();
    let err = c.read_id(999_999_999, &mut sink).expect_err("不存在的 id 应报错");
    assert!(err.contains("失效") || err.contains("不存在"), "{err}");
    // 连接仍可用
    c.ping().expect("报错后连接应仍然可用");
    c.shutdown();
}

#[test]
fn large_file_streams_in_chunks() {
    let Some(image) = test_image() else {
        eprintln!("跳过：找不到测试镜像");
        return;
    };
    let socket = socket_path();
    let _server = spawn_server(&socket, "t");
    let mut c = connect(&socket, "t");
    c.open(image.to_str().unwrap(), None).expect("打开镜像");

    // docs/keep.bin 是 5000 字节随机内容；用 read_path 走完整的 chunk 循环
    let mut buf = Vec::new();
    let n = c.read_path("/docs/keep.bin", &mut buf).expect("读 keep.bin");
    assert_eq!(n, 5000, "大小应为 5000");
    assert_eq!(buf.len(), 5000);

    // 同时验证写入任意 Writer（这里用内存）不会丢尾部
    let mut sink = std::io::sink();
    let n2 = c.read_path("/docs/keep.bin", &mut sink).expect("写 sink");
    assert_eq!(n2, 5000);
}

#[test]
fn session_survives_client_reconnect() {
    let Some(image) = test_image() else {
        eprintln!("跳过：找不到测试镜像");
        return;
    };
    let socket = socket_path();
    let _server = spawn_server(&socket, "t");
    {
        let mut c = connect(&socket, "t");
        c.open(image.to_str().unwrap(), None).expect("打开镜像");
        c.ping().unwrap();
        // 客户端直接断开（模拟 GUI 崩溃）——会话应留在 helper 里
    }
    std::thread::sleep(Duration::from_millis(150));
    let mut c2 = connect(&socket, "t");
    let entries = c2.list_dir("/", false).expect("重连后仍应能列目录");
    assert!(!entries.is_empty());
    let mut f = std::io::sink();
    c2.read_path("/hello.txt", &mut f).expect("重连后应能读文件");
    let _ = f.flush();
}
