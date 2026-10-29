//! dmde-rs 只读特权 helper。
//!
//! 由 GUI 通过 macOS 标准授权弹窗（`with administrator privileges`）拉起，
//! 只监听本机 Unix socket，**不提供任何写操作**。也可以手工跑起来调试：
//!
//! ```text
//! dmde-rs-helper --socket /tmp/dmde.sock --token test --idle 60
//! ```

use dmrpc::server::{serve, ServeOptions};
use std::path::PathBuf;

fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let mut socket: Option<PathBuf> = None;
    let mut token = String::new();
    let mut uid: Option<u32> = None;
    let mut gid: Option<u32> = None;
    let mut idle_secs = 600u64;

    let mut it = args.into_iter();
    while let Some(a) = it.next() {
        let mut value = || it.next().unwrap_or_default();
        match a.as_str() {
            "--socket" => socket = Some(PathBuf::from(value())),
            "--token" => token = value(),
            "--uid" => uid = value().parse().ok(),
            "--gid" => gid = value().parse().ok(),
            "--idle" => idle_secs = value().parse().unwrap_or(idle_secs),
            "-h" | "--help" => {
                usage();
                return;
            }
            other => {
                eprintln!("dmde-rs-helper: 未知参数 {other}");
                usage();
                std::process::exit(2);
            }
        }
    }

    let socket = match socket {
        Some(p) => p,
        None => {
            eprintln!("dmde-rs-helper: 缺少 --socket");
            usage();
            std::process::exit(2);
        }
    };
    if token.is_empty() {
        eprintln!("dmde-rs-helper: 缺少 --token（拒绝无认证启动）");
        std::process::exit(2);
    }

    let opt = ServeOptions { socket, token, uid, gid, idle_secs };
    match serve(&opt) {
        Ok(()) => {}
        Err(e) => {
            eprintln!("dmde-rs-helper: {e}");
            std::process::exit(1);
        }
    }
}

fn usage() {
    eprintln!(
        "dmde-rs-helper —— 只读特权 helper（一般由 GUI 自动拉起，不需要手工运行）\n\
         \n\
         用法: dmde-rs-helper --socket <path> --token <hex> [--uid N] [--gid N] [--idle 秒]\n\
         \n\
         安全说明: 仅提供读取（列目录/预览/导出内容），不写任何设备与文件。"
    );
}
