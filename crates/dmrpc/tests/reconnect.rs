//! 协议流错位的回归测试。
//!
//! 现象：暂停导出后进别的目录，报「helper 返回了意料之外的响应: End (total: N)」。
//! 原因：读文件中途写端失败（用户点了暂停 / 本地盘写不进去）后，helper 还在往
//! socket 发 chunk，客户端却直接返回；残留帧被下一条请求当成了响应。
//!
//! 修复：客户端标记连接作废，下次调用前重连（helper 的 session 在服务端，不丢卷）。

use dmrpc::proto::{self, Reply, Request};
use std::io::{BufReader, Write};
use std::os::unix::net::{UnixListener, UnixStream};
use std::path::PathBuf;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;

fn sock_path() -> PathBuf {
    static N: AtomicUsize = AtomicUsize::new(0);
    let n = N.fetch_add(1, Ordering::SeqCst);
    std::env::temp_dir().join(format!("dmrpc-rc-{}-{}.sock", std::process::id(), n))
}

/// 假 helper：hello 回 hello；read_path 发两帧 chunk 再 end（模拟"服务端还在写"）；
/// ls 回空列表。每来一个连接开一个线程，方便统计重连次数。
fn spawn_fake(sock: PathBuf, conns: Arc<AtomicUsize>) -> std::thread::JoinHandle<()> {
    let listener = UnixListener::bind(&sock).expect("bind");
    std::thread::spawn(move || {
        for stream in listener.incoming() {
            let Ok(stream) = stream else { break };
            conns.fetch_add(1, Ordering::SeqCst);
            std::thread::spawn(move || serve(stream));
        }
    })
}

fn serve(stream: UnixStream) {
    let mut w = stream.try_clone().expect("clone");
    let mut r = BufReader::new(stream);
    while let Ok(Some(req)) = proto::read_request(&mut r) {
        let ok = match req {
            Request::Hello { .. } => proto::write_reply(
                &mut w,
                &Reply::Hello { version: "test".into(), pid: 1 },
            ),
            Request::ReadPath { .. } => {
                let data = [7u8; 8];
                let mut r = Ok(());
                for _ in 0..2 {
                    r = r
                        .and_then(|_| proto::write_reply(&mut w, &Reply::Chunk { len: 8 }))
                        .and_then(|_| w.write_all(&data));
                }
                r.and_then(|_| proto::write_reply(&mut w, &Reply::End { total: 16 }))
            }
            Request::Ls { .. } => {
                proto::write_reply(&mut w, &Reply::Entries { entries: vec![] })
            }
            _ => proto::write_reply(&mut w, &Reply::Error { message: "unsupported".into() }),
        };
        if ok.is_err() {
            break; // 对端已断开
        }
    }
}

/// 写第一个字节就失败，模拟 CancelWriter 被用户暂停。
struct FailFast;

impl Write for FailFast {
    fn write(&mut self, _buf: &[u8]) -> std::io::Result<usize> {
        Err(std::io::Error::other("用户已暂停导出"))
    }
    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

#[test]
fn client_reconnects_after_interrupted_read() {
    let sock = sock_path();
    let conns = Arc::new(AtomicUsize::new(0));
    let server = spawn_fake(sock.clone(), conns.clone());

    let mut c = dmrpc::Client::connect(&sock, "tok").expect("首次连接");
    let err = c
        .read_path("/big.bin", &mut FailFast)
        .expect_err("写端失败必须报错");
    assert!(err.contains("接收数据失败"), "错误文案要指出是接收阶段: {err}");

    // 关键断言：下一条请求不能读到上一个流的残留 End，而应自动重连后成功
    let items = c
        .list_dir("/", false)
        .expect("暂停后继续浏览必须成功（客户端应自动重连）");
    assert!(items.is_empty());
    assert!(
        conns.load(Ordering::SeqCst) >= 2,
        "必须建立新连接，实际连接数 {}",
        conns.load(Ordering::SeqCst)
    );

    drop(c);
    drop(server);
    let _ = std::fs::remove_file(&sock);
}
