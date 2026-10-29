//! 验证真正被 GUI 拉起的那个可执行文件：参数解析、socket 权限、能否服务。
//!
//! GUI 走的是 `dmde-rs-helper --socket ... --token ... --uid ... --gid ...`，
//! 这里就用同样的方式起它，确保打包后的二进制本身没问题（不是只有进程内 serve 能跑）。

use dmrpc::proto::Request;
use std::io::BufReader;
use std::os::unix::fs::PermissionsExt;
use std::os::unix::net::UnixStream;
use std::path::{Path, PathBuf};
use std::time::Duration;

fn bin() -> PathBuf {
    PathBuf::from(env!("CARGO_BIN_EXE_dmde-rs-helper"))
}

fn tmp_socket(name: &str) -> PathBuf {
    let stamp = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_nanos())
        .unwrap_or(0);
    std::env::temp_dir().join(format!("dmde-helper-{name}-{}-{stamp}.sock", std::process::id()))
}

fn wait_socket(path: &Path) {
    for _ in 0..400 {
        if path.exists() {
            return;
        }
        std::thread::sleep(Duration::from_millis(25));
    }
    panic!("helper 没创建 socket: {}", path.display());
}

fn rpc(w: &mut UnixStream, r: &mut BufReader<UnixStream>, req: &Request) -> dmrpc::proto::Reply {
    dmrpc::proto::write_request(w, req).expect("写请求");
    dmrpc::proto::read_reply(r).expect("读响应")
}

#[test]
fn binary_serves_and_exits_on_shutdown() {
    let socket = tmp_socket("serve");
    let mut child = std::process::Command::new(bin())
        .args([
            "--socket",
            socket.to_str().unwrap(),
            "--token",
            "tok-123",
            "--idle",
            "30",
        ])
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .spawn()
        .expect("启动 helper");
    wait_socket(&socket);

    // socket 权限必须是 0600，否则同机其它用户能连
    let mode = std::fs::metadata(&socket).expect("stat socket").permissions().mode() & 0o777;
    assert_eq!(mode, 0o600, "socket 权限应为 600，实际 {mode:o}");

    let stream = UnixStream::connect(&socket).expect("连接");
    let mut w = stream.try_clone().unwrap();
    let mut r = BufReader::new(stream);

    match rpc(&mut w, &mut r, &Request::Hello { token: "tok-123".into() }) {
        dmrpc::proto::Reply::Hello { pid, .. } => assert_eq!(pid, child.id(), "pid 应对上"),
        other => panic!("hello 失败: {other:?}"),
    }
    match rpc(&mut w, &mut r, &Request::Ping) {
        dmrpc::proto::Reply::Ok => {}
        other => panic!("ping 失败: {other:?}"),
    }
    match rpc(&mut w, &mut r, &Request::Shutdown) {
        dmrpc::proto::Reply::Ok => {}
        other => panic!("shutdown 失败: {other:?}"),
    }

    // 等进程自己退出（不能靠 SIGKILL 掩盖清理逻辑）
    let mut exited = false;
    for _ in 0..200 {
        if let Ok(Some(_)) = child.try_wait() {
            exited = true;
            break;
        }
        std::thread::sleep(Duration::from_millis(25));
    }
    let _ = child.kill();
    assert!(exited, "收到 shutdown 后 helper 应自行退出");
    assert!(!socket.exists(), "退出时应清理 socket");
}

#[test]
fn refuses_to_start_without_token() {
    let socket = tmp_socket("notoken");
    let out = std::process::Command::new(bin())
        .args(["--socket", socket.to_str().unwrap()])
        .output()
        .expect("运行 helper");
    assert!(!out.status.success(), "缺 token 必须拒绝启动");
    let err = String::from_utf8_lossy(&out.stderr);
    assert!(err.contains("token"), "错误信息应提到 token: {err}");
    assert!(!socket.exists());
}

#[test]
fn wrong_token_closes_connection_but_keeps_listening() {
    let socket = tmp_socket("badtoken");
    let mut child = std::process::Command::new(bin())
        .args(["--socket", socket.to_str().unwrap(), "--token", "good", "--idle", "30"])
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .spawn()
        .expect("启动 helper");
    wait_socket(&socket);

    {
        let stream = UnixStream::connect(&socket).expect("连接");
        let mut w = stream.try_clone().unwrap();
        let mut r = BufReader::new(stream);
        match rpc(&mut w, &mut r, &Request::Hello { token: "bad".into() }) {
            dmrpc::proto::Reply::Error { message } => assert!(message.contains("token")),
            other => panic!("错 token 应被拒: {other:?}"),
        }
    }

    // 服务端仍应存活并可接受正确 token
    std::thread::sleep(Duration::from_millis(100));
    let mut c = loop {
        match dmrpc::Client::connect(&socket, "good") {
            Ok(c) => break c,
            Err(_) => std::thread::sleep(Duration::from_millis(50)),
        }
    };
    c.ping().expect("正确 token 仍应可用");
    c.shutdown();
    for _ in 0..200 {
        if matches!(child.try_wait(), Ok(Some(_))) {
            break;
        }
        std::thread::sleep(Duration::from_millis(25));
    }
    let _ = child.kill();
}
