//! 特权 helper 的服务端：Unix socket + 行协议，**只做读**。
//!
//! 安全边界（按重要性排序）：
//! 1. socket 放在仅本用户可进入的目录里，且 `chown` 回调用者、权限 0600——
//!    其它本地用户连不上；
//! 2. 连接后必须先 `hello` 带 token，token 由 GUI 每次随机生成；
//! 3. 本进程没有任何写接口：恢复文件由 GUI 以自己的身份落盘，
//!    避免 root 写进你的目录带来属主/TCC 问题，也让 helper 的攻击面只剩"读"；
//! 4. 空闲自动退出，不留常驻 root 进程。

use crate::proto::{self, Reply, Request};
use crate::session::Session;
use std::fs;
use std::io::{BufReader, Write};
use std::os::unix::net::{UnixListener, UnixStream};
use std::path::PathBuf;
use std::time::{Duration, Instant};

pub struct ServeOptions {
    pub socket: PathBuf,
    pub token: String,
    /// 调用者 uid/gid：socket 归属于它，这样 GUI（普通用户）才连得上
    pub uid: Option<u32>,
    pub gid: Option<u32>,
    /// 连接断开后多久没有新连接就退出（秒）
    pub idle_secs: u64,
}

#[derive(Default)]
struct State {
    session: Option<Session>,
}

pub fn serve(opt: &ServeOptions) -> std::io::Result<()> {
    if let Some(dir) = opt.socket.parent() {
        let _ = fs::create_dir_all(dir);
    }
    let _ = fs::remove_file(&opt.socket);
    let listener = UnixListener::bind(&opt.socket)?;
    set_socket_owner(&opt.socket, opt.uid, opt.gid)?;
    listener.set_nonblocking(true)?;
    eprintln!(
        "dmde-rs-helper: 已监听 {} （pid={}, 空闲 {}s 退出）",
        opt.socket.display(),
        std::process::id(),
        opt.idle_secs
    );

    let idle = Duration::from_secs(opt.idle_secs.max(30));
    let mut deadline = Instant::now() + idle;
    let mut state = State::default();
    loop {
        match listener.accept() {
            Ok((stream, _)) => {
                stream.set_nonblocking(false)?;
                if let Ok(true) = handle(stream, opt, &mut state) {
                    eprintln!("dmde-rs-helper: 收到 shutdown");
                    break;
                }
                deadline = Instant::now() + idle;
            }
            Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => {
                if Instant::now() >= deadline {
                    eprintln!("dmde-rs-helper: 空闲超时，退出");
                    break;
                }
                std::thread::sleep(Duration::from_millis(120));
            }
            Err(e) => {
                let _ = fs::remove_file(&opt.socket);
                return Err(e);
            }
        }
    }
    let _ = fs::remove_file(&opt.socket);
    Ok(())
}

#[cfg(unix)]
fn set_socket_owner(path: &PathBuf, uid: Option<u32>, gid: Option<u32>) -> std::io::Result<()> {
    use std::os::unix::fs::PermissionsExt;
    fs::set_permissions(path, fs::Permissions::from_mode(0o600))?;
    if uid.is_some() || gid.is_some() {
        let _ = std::os::unix::fs::chown(path, uid, gid);
    }
    Ok(())
}

/// 返回 `Ok(true)` 表示收到 shutdown，服务端应退出。
fn handle(stream: UnixStream, opt: &ServeOptions, st: &mut State) -> std::io::Result<bool> {
    let mut w = stream.try_clone()?;
    let mut r = BufReader::new(stream);
    let mut authed = false;
    loop {
        let req = match proto::read_request(&mut r) {
            Ok(Some(q)) => q,
            Ok(None) => return Ok(false), // 对端关闭
            Err(e) => {
                let _ = proto::write_reply(&mut w, &Reply::Error { message: format!("协议错误: {e}") });
                return Ok(false);
            }
        };

        match req {
            Request::Hello { ref token } => {
                if !token_eq(token, &opt.token) {
                    let _ = proto::write_reply(&mut w, &Reply::Error { message: "token 不匹配".into() });
                    return Ok(false);
                }
                authed = true;
                proto::write_reply(
                    &mut w,
                    &Reply::Hello {
                        version: env!("CARGO_PKG_VERSION").to_string(),
                        pid: std::process::id(),
                    },
                )?;
                continue;
            }
            Request::Ping => {
                proto::write_reply(&mut w, &Reply::Ok)?;
                continue;
            }
            _ if !authed => {
                let _ = proto::write_reply(&mut w, &Reply::Error { message: "请先发送 hello".into() });
                return Ok(false);
            }
            Request::Shutdown => {
                proto::write_reply(&mut w, &Reply::Ok)?;
                return Ok(true);
            }
            Request::Disks => match dmdisk::list_disks() {
                Ok(disks) => {
                    let disks = disks.iter().map(Into::into).collect();
                    proto::write_reply(&mut w, &Reply::Disks { disks })?;
                }
                Err(e) => proto::write_reply(&mut w, &Reply::Error { message: e.to_string() })?,
            },
            Request::Open { source, partition } => match crate::session::open(&source, partition) {
                Ok((s, dto)) => {
                    st.session = Some(s);
                    proto::write_reply(&mut w, &Reply::Source { source: dto })?;
                }
                Err(e) => proto::write_reply(&mut w, &Reply::Error { message: e.message })?,
            },
            Request::DirSize { path, budget_ms } => match st.session.as_ref() {
                Some(s) => match s.dir_size(&path, budget_ms) {
                    Ok(size) => proto::write_reply(&mut w, &Reply::DirSize { size })?,
                    Err(e) => proto::write_reply(&mut w, &Reply::Error { message: e })?,
                },
                None => proto::write_reply(&mut w, &Reply::Error { message: NOT_OPEN.into() })?,
            },
            Request::Stat { path } => match st.session.as_mut() {
                Some(s) => match s.stat(&path) {
                    Ok(entry) => proto::write_reply(&mut w, &Reply::Entry { entry })?,
                    Err(e) => proto::write_reply(&mut w, &Reply::Error { message: e })?,
                },
                None => proto::write_reply(&mut w, &Reply::Error { message: NOT_OPEN.into() })?,
            },
            Request::Ls { path, show_deleted } => match st.session.as_mut() {
                Some(s) => match s.list_dir(&path, show_deleted) {
                    Ok(entries) => proto::write_reply(&mut w, &Reply::Entries { entries })?,
                    Err(e) => proto::write_reply(&mut w, &Reply::Error { message: e })?,
                },
                None => proto::write_reply(&mut w, &Reply::Error { message: NOT_OPEN.into() })?,
            },
            Request::PreviewId { id, max } => match st.session.as_ref() {
                Some(s) => match s.preview_by_id(id, max) {
                    Ok(preview) => proto::write_reply(&mut w, &Reply::Preview { preview })?,
                    Err(e) => proto::write_reply(&mut w, &Reply::Error { message: e })?,
                },
                None => proto::write_reply(&mut w, &Reply::Error { message: NOT_OPEN.into() })?,
            },
            Request::Preview { path, max } => match st.session.as_ref() {
                Some(s) => match s.preview(&path, max) {
                    Ok(preview) => proto::write_reply(&mut w, &Reply::Preview { preview })?,
                    Err(e) => proto::write_reply(&mut w, &Reply::Error { message: e })?,
                },
                None => proto::write_reply(&mut w, &Reply::Error { message: NOT_OPEN.into() })?,
            },
            Request::Deleted { limit } => match st.session.as_mut() {
                Some(s) => match s.deleted(limit) {
                    Ok(entries) => proto::write_reply(&mut w, &Reply::Entries { entries })?,
                    Err(e) => proto::write_reply(&mut w, &Reply::Error { message: e })?,
                },
                None => proto::write_reply(&mut w, &Reply::Error { message: NOT_OPEN.into() })?,
            },
            Request::ReadId { id } => {
                let s = match st.session.as_ref() {
                    Some(s) => s,
                    None => {
                        proto::write_reply(&mut w, &Reply::Error { message: NOT_OPEN.into() })?;
                        continue;
                    }
                };
                stream_out(&mut w, |sink| s.read_id(id, sink))?;
            }
            Request::ReadPath { path } => {
                let s = match st.session.as_ref() {
                    Some(s) => s,
                    None => {
                        proto::write_reply(&mut w, &Reply::Error { message: NOT_OPEN.into() })?;
                        continue;
                    }
                };
                stream_out(&mut w, |sink| s.read_path(&path, sink))?;
            }
        }
    }
}

const NOT_OPEN: &str = "还没有打开任何卷";

/// 把一次读操作变成 `chunk… end` 帧序列。
fn stream_out<F>(w: &mut UnixStream, f: F) -> std::io::Result<()>
where
    F: FnOnce(&mut dyn Write) -> Result<u64, String>,
{
    // 必须显式 flush：小文件可能一个 chunk 都没触发，丢掉就会"总长度对、内容为空"。
    let (outcome, total) = {
        let mut cw = ChunkWriter { sink: w, buf: Vec::with_capacity(CHUNK), total: 0 };
        let r = f(&mut cw).and_then(|_| cw.flush().map_err(|e| e.to_string()));
        (r, cw.total)
    };
    match outcome {
        Ok(_) => proto::write_reply(w, &Reply::End { total }),
        Err(message) => proto::write_reply(w, &Reply::Error { message }),
    }
}

const CHUNK: usize = 128 * 1024;

struct ChunkWriter<'a> {
    sink: &'a mut UnixStream,
    buf: Vec<u8>,
    total: u64,
}

impl Write for ChunkWriter<'_> {
    fn write(&mut self, data: &[u8]) -> std::io::Result<usize> {
        self.buf.extend_from_slice(data);
        self.total += data.len() as u64;
        if self.buf.len() >= CHUNK {
            self.flush()?;
        }
        Ok(data.len())
    }

    fn flush(&mut self) -> std::io::Result<()> {
        if self.buf.is_empty() {
            return Ok(());
        }
        let len = self.buf.len() as u64;
        proto::write_reply(self.sink, &Reply::Chunk { len })?;
        self.sink.write_all(&self.buf)?;
        self.sink.flush()?;
        self.buf.clear();
        Ok(())
    }
}

fn token_eq(a: &str, b: &str) -> bool {
    if a.len() != b.len() {
        return false;
    }
    a.bytes().zip(b.bytes()).fold(0u8, |acc, (x, y)| acc | (x ^ y)) == 0
}
