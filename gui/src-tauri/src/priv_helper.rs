//! macOS 提权：用系统标准授权弹窗拉起只读 helper。
//!
//! 为什么不整块 App 以 root 重启：Tauri 的 WebView 以 root 运行会破坏 TCC/窗口服务，
//! 且把解析代码全放进特权进程也没必要。这里只把"读设备"这件事交出去：
//!
//! * GUI 进程永远是普通用户；输出文件也由 GUI 自己写，属主/权限天然正确；
//! * helper 只做读，socket 归属当前用户、0600、放在仅本用户可进入的目录；
//! * 连接必须先 `hello` 带一次性 token；
//! * helper 空闲自动退出，退出即销毁 socket。

use dmrpc::Client;
use std::fs;
use std::path::PathBuf;
use std::process::{Command, Stdio};
use std::sync::Mutex;
use std::time::{Duration, Instant};

struct Helper {
    socket: PathBuf,
    token: String,
}

static HELPER: Mutex<Option<Helper>> = Mutex::new(None);

/// 拉起（或复用）helper 并返回已握手的连接。
pub fn connect() -> Result<Client, String> {
    let helper = ensure_helper()?;
    Client::connect(&helper.socket, &helper.token).map_err(|e| {
        format!("无法连接只读 helper（可能授权被拒绝或 helper 已退出）：{e}")
    })
}

/// 当前是否有可用的提权通道。
pub fn alive() -> bool {
    let guard = match HELPER.lock() {
        Ok(g) => g,
        Err(_) => return false,
    };
    let Some(h) = guard.as_ref() else {
        return false;
    };
    // 只做一次轻量握手，失败就认为需要重新授权
    matches!(Client::connect(&h.socket, &h.token), Ok(_))
}

/// 主动断开：让 helper 收到 shutdown 自行退出。
pub fn shutdown() -> bool {
    let mut guard = match HELPER.lock() {
        Ok(g) => g,
        Err(_) => return false,
    };
    if let Some(h) = guard.take() {
        if let Ok(mut c) = Client::connect(&h.socket, &h.token) {
            c.shutdown();
        }
        let _ = fs::remove_file(&h.socket);
    }
    true
}

fn ensure_helper() -> Result<Helper, String> {
    let mut guard = HELPER.lock().map_err(|_| "提权状态锁失效".to_string())?;

    // 已有且活着 → 直接复用
    if let Some(h) = guard.as_ref() {
        if h.socket.exists() && Client::connect(&h.socket, &h.token).is_ok() {
            return Ok(Helper { socket: h.socket.clone(), token: h.token.clone() });
        }
    }

    let dir = helper_dir()?;
    fs::create_dir_all(&dir).map_err(|e| format!("创建 {} 失败: {e}", dir.display()))?;
    let socket = socket_path(&dir)?;
    let token = random_token()?;
    let helper = Helper { socket, token };
    spawn(&helper, &dir)?;
    wait_ready(&helper)?;
    *guard = Some(Helper { socket: helper.socket.clone(), token: helper.token.clone() });
    Ok(helper)
}

/// helper 可执行文件：bundle 里与 GUI 同目录，开发态在 workspace target 下。
fn helper_binary() -> Result<PathBuf, String> {
    if let Ok(p) = std::env::var("DMDE_HELPER") {
        let p = PathBuf::from(p);
        if p.is_file() {
            return Ok(p);
        }
    }
    let exe = std::env::current_exe().map_err(|e| e.to_string())?;
    let dir = exe.parent().ok_or("无法定位可执行文件目录")?;
    let mut candidates = vec![dir.join("dmde-rs-helper")];
    for up in 1..=5 {
        let mut base = dir.to_path_buf();
        for _ in 0..up {
            base = match base.parent() {
                Some(p) => p.to_path_buf(),
                None => break,
            };
        }
        candidates.push(base.join("target/debug/dmde-rs-helper"));
        candidates.push(base.join("target/release/dmde-rs-helper"));
    }
    for c in candidates {
        if c.is_file() {
            return Ok(c.canonicalize().unwrap_or(c));
        }
    }
    Err("找不到 dmde-rs-helper（请先 cargo build -p dmhelper，或设置 DMDE_HELPER）".into())
}

fn helper_dir() -> Result<PathBuf, String> {
    let home = std::env::var("HOME").map_err(|_| "读不到 HOME".to_string())?;
    Ok(PathBuf::from(home).join("Library/Application Support/dmde-rs"))
}

fn socket_path(dir: &PathBuf) -> Result<PathBuf, String> {
    // unix socket 路径上限 104 字节，家目录太长时退到 /tmp（同名文件仅本用户可建）
    let p = dir.join("helper.sock");
    if p.as_os_str().len() < 100 {
        return Ok(p);
    }
    let uid = current_uid()?;
    Ok(PathBuf::from(format!("/tmp/dmde-rs-{uid}.sock")))
}

fn current_uid() -> Result<u32, String> {
    use std::os::unix::fs::MetadataExt;
    let home = std::env::var("HOME").map_err(|_| "读不到 HOME".to_string())?;
    Ok(fs::metadata(home).map_err(|e| e.to_string())?.uid())
}

fn current_gid() -> Result<u32, String> {
    use std::os::unix::fs::MetadataExt;
    let home = std::env::var("HOME").map_err(|_| "读不到 HOME".to_string())?;
    Ok(fs::metadata(home).map_err(|e| e.to_string())?.gid())
}

/// 16 字节随机 token（读 /dev/urandom，不引第三方 crate）。
fn random_token() -> Result<String, String> {
    use std::io::Read;
    let mut buf = [0u8; 16];
    fs::File::open("/dev/urandom")
        .and_then(|mut f| f.read_exact(&mut buf))
        .map_err(|e| format!("生成 token 失败: {e}"))?;
    Ok(buf.iter().map(|b| format!("{b:02x}")).collect())
}

fn spawn(helper: &Helper, dir: &PathBuf) -> Result<(), String> {
    let bin = helper_binary()?;
    let uid = current_uid()?;
    let gid = current_gid()?;
    let log = dir.join("helper.log");

    let cmd = format!(
        "{bin} --socket {sock} --token {token} --uid {uid} --gid {gid} --idle 900 >>{log} 2>&1 & echo started",
        bin = sh_quote(&bin.to_string_lossy()),
        sock = sh_quote(&helper.socket.to_string_lossy()),
        token = sh_quote(&helper.token),
        log = sh_quote(&log.to_string_lossy()),
    );

    let script = format!("do shell script \"{}\" with administrator privileges", as_escape(&cmd));
    let out = run_with_timeout("osascript", &["-e", &script], Duration::from_secs(180))?;

    if !out.status.success() {
        let err = String::from_utf8_lossy(&out.stderr);
        if err.contains("-128") || err.to_lowercase().contains("cancel") {
            return Err("已取消管理员授权，无法读取原始磁盘（/dev/rdisk* 需要 root 权限）".into());
        }
        return Err(format!(
            "请求管理员权限失败：{}（详见 {}）",
            err.trim(),
            log.display()
        ));
    }
    Ok(())
}

/// 等 helper 把 socket 建起来并完成握手。
fn wait_ready(helper: &Helper) -> Result<(), String> {
    let deadline = Instant::now() + Duration::from_secs(20);
    let mut last = String::new();
    while Instant::now() < deadline {
        if helper.socket.exists() {
            match Client::connect(&helper.socket, &helper.token) {
                Ok(_) => return Ok(()),
                Err(e) => last = e.to_string(),
            }
        }
        std::thread::sleep(Duration::from_millis(120));
    }
    if last.is_empty() {
        Err("helper 没有启动（授权后仍未创建 socket，检查 helper 是否随 App 一起打包）".into())
    } else {
        Err(format!("helper 启动异常：{last}"))
    }
}

/// 带超时的子进程执行：osascript 会一直等密码窗，不能无限期阻塞。
fn run_with_timeout(
    program: &str,
    args: &[&str],
    limit: Duration,
) -> Result<std::process::Output, String> {
    let mut child = Command::new(program)
        .args(args)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .map_err(|e| format!("启动 {program} 失败: {e}"))?;

    let start = Instant::now();
    loop {
        match child.try_wait() {
            Ok(Some(_)) => {
                return child.wait_with_output().map_err(|e| e.to_string());
            }
            Ok(None) => {
                if start.elapsed() > limit {
                    let _ = child.kill();
                    return Err("等待授权超时（3 分钟），请重试".into());
                }
                std::thread::sleep(Duration::from_millis(80));
            }
            Err(e) => return Err(format!("等待 {program} 失败: {e}")),
        }
    }
}

/// shell 单引号转义：' → '\''
fn sh_quote(s: &str) -> String {
    format!("'{}'", s.replace('\'', r"'\''"))
}

/// AppleScript 字符串字面量转义
fn as_escape(s: &str) -> String {
    s.replace('\\', r"\\").replace('"', "\\\"")
}
