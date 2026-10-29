//! helper 客户端。所有调用都返回人类可读的中文错误，方便直接展示到状态栏。

use crate::model::{DiskDto, EntryDto, PreviewDto, SourceDto};
use crate::proto::{self, Reply, Request};
use std::io::{BufReader, Write};
use std::os::unix::net::UnixStream;
use std::path::Path;

pub struct Client {
    stream: UnixStream,
    reader: BufReader<UnixStream>,
    /// 重连要用：helper 的 session 存在服务端、不随连接断开而丢
    socket: std::path::PathBuf,
    token: String,
    /// 读到一半被打断（用户暂停 / 写本地盘失败）时置位：
    /// 此时 socket 里还剩着上一个文件的 chunk/end，直接再发请求会读到残留帧，
    /// 报「helper 返回了意料之外的响应: End」。下一次调用前整体重连即可。
    broken: bool,
}

impl Client {
    /// 连接并完成 hello 握手；失败说明 socket 不是我们的 helper。
    pub fn connect(socket: &Path, token: &str) -> std::io::Result<Client> {
        let stream = UnixStream::connect(socket)?;
        let mut c = Client {
            reader: BufReader::new(stream.try_clone()?),
            stream,
            socket: socket.to_path_buf(),
            token: token.to_string(),
            broken: false,
        };
        match c.call_raw(&Request::Hello { token: token.to_string() })? {
            Reply::Hello { .. } => Ok(c),
            Reply::Error { message } => Err(std::io::Error::new(std::io::ErrorKind::PermissionDenied, message)),
            other => Err(unexpected(&other)),
        }
    }

    fn call(&mut self, req: &Request) -> std::io::Result<Reply> {
        self.ensure_alive()
            .map_err(|e| std::io::Error::other(e))?;
        self.call_raw(req)
    }

    fn call_raw(&mut self, req: &Request) -> std::io::Result<Reply> {
        proto::write_request(&mut self.stream, req)?;
        proto::read_reply(&mut self.reader)
    }

    /// 上一次读取被中断过就重连。helper 的 session 在服务端，重连不丢卷。
    fn ensure_alive(&mut self) -> Result<(), String> {
        if !self.broken {
            return Ok(());
        }
        self.broken = false;
        let stream = UnixStream::connect(&self.socket)
            .map_err(|e| format!("重连只读 helper 失败（可能需要重新授权）: {e}"))?;
        let reader = BufReader::new(
            stream
                .try_clone()
                .map_err(|e| format!("重连 helper 失败: {e}"))?,
        );
        self.stream = stream;
        self.reader = reader;
        match self.call_raw(&Request::Hello { token: self.token.clone() }) {
            Ok(Reply::Hello { .. }) => Ok(()),
            Ok(Reply::Error { message }) => {
                self.broken = true;
                Err(message)
            }
            Ok(other) => {
                self.broken = true;
                Err(unexpected(&other).to_string())
            }
            Err(e) => {
                self.broken = true;
                Err(format!("helper 握手失败: {e}"))
            }
        }
    }

    pub fn ping(&mut self) -> Result<(), String> {
        match self.call(&Request::Ping) {
            Ok(Reply::Ok) => Ok(()),
            Ok(Reply::Error { message }) => Err(message),
            Ok(other) => Err(unexpected(&other).to_string()),
            Err(e) => Err(format!("helper 无响应: {e}")),
        }
    }

    pub fn disks(&mut self) -> Result<Vec<DiskDto>, String> {
        match self.call(&Request::Disks) {
            Ok(Reply::Disks { disks }) => Ok(disks),
            Ok(Reply::Error { message }) => Err(message),
            Ok(other) => Err(unexpected(&other).to_string()),
            Err(e) => Err(format!("连接 helper 失败: {e}")),
        }
    }

    pub fn open(&mut self, source: &str, partition: Option<usize>) -> Result<SourceDto, String> {
        let req = Request::Open { source: source.to_string(), partition };
        match self.call(&req) {
            Ok(Reply::Source { source }) => Ok(source),
            Ok(Reply::Error { message }) => Err(message),
            Ok(other) => Err(unexpected(&other).to_string()),
            Err(e) => Err(format!("连接 helper 失败: {e}")),
        }
    }

    pub fn list_dir(&mut self, path: &str, show_deleted: bool) -> Result<Vec<EntryDto>, String> {
        let req = Request::Ls { path: path.to_string(), show_deleted };
        match self.call(&req) {
            Ok(Reply::Entries { entries }) => Ok(entries),
            Ok(Reply::Error { message }) => Err(message),
            Ok(other) => Err(unexpected(&other).to_string()),
            Err(e) => Err(format!("连接 helper 失败: {e}")),
        }
    }

    pub fn preview(&mut self, path: &str, max: usize) -> Result<PreviewDto, String> {
        let req = Request::Preview { path: path.to_string(), max };
        match self.call(&req) {
            Ok(Reply::Preview { preview }) => Ok(preview),
            Ok(Reply::Error { message }) => Err(message),
            Ok(other) => Err(unexpected(&other).to_string()),
            Err(e) => Err(format!("连接 helper 失败: {e}")),
        }
    }

    pub fn preview_id(&mut self, id: u64, max: usize) -> Result<PreviewDto, String> {
        match self.call(&Request::PreviewId { id, max }) {
            Ok(Reply::Preview { preview }) => Ok(preview),
            Ok(Reply::Error { message }) => Err(message),
            Ok(other) => Err(unexpected(&other).to_string()),
            Err(e) => Err(format!("连接 helper 失败: {e}")),
        }
    }

    pub fn stat(&mut self, path: &str) -> Result<Option<EntryDto>, String> {
        match self.call(&Request::Stat { path: path.to_string() }) {
            Ok(Reply::Entry { entry }) => Ok(entry),
            Ok(Reply::Error { message }) => Err(message),
            Ok(other) => Err(unexpected(&other).to_string()),
            Err(e) => Err(format!("连接 helper 失败: {e}")),
        }
    }

    pub fn deleted(&mut self, limit: usize) -> Result<Vec<EntryDto>, String> {
        match self.call(&Request::Deleted { limit }) {
            Ok(Reply::Entries { entries }) => Ok(entries),
            Ok(Reply::Error { message }) => Err(message),
            Ok(other) => Err(unexpected(&other).to_string()),
            Err(e) => Err(format!("连接 helper 失败: {e}")),
        }
    }

    pub fn read_id(&mut self, id: u64, out: &mut dyn Write) -> Result<u64, String> {
        self.start(&Request::ReadId { id })?;
        self.drain(out)
    }

    pub fn read_path(&mut self, path: &str, out: &mut dyn Write) -> Result<u64, String> {
        self.start(&Request::ReadPath { path: path.to_string() })?;
        self.drain(out)
    }

    /// 只发请求，不读响应——读文件的响应本身就是 chunk 流，
    /// 若先用 `call` 吃掉首帧，就会把后续帧错位、甚至死等。
    fn start(&mut self, req: &Request) -> Result<(), String> {
        proto::write_request(&mut self.stream, req).map_err(|e| format!("连接 helper 失败: {e}"))
    }

    /// 收 `chunk… end` 帧。中途出错时已写出的字节会留在 writer 里，
    /// 调用方负责删掉半成品文件。
    ///
    /// 关键：**写端一错就作废连接**。用户点暂停时 `CancelWriter` 立刻报错，
    /// 但 helper 那边还在发后续帧；如果就这么返回，残留的 chunk/end 会留给
    /// 下一条请求去读，表现为「暂停后进别的目录报 helper 返回了意料之外的响应」。
    /// 标记 broken 后，下一次调用会重连（服务端 session 还在），流就干净了。
    fn drain(&mut self, out: &mut dyn Write) -> Result<u64, String> {
        loop {
            let reply = match proto::read_reply(&mut self.reader) {
                Ok(r) => r,
                Err(e) => {
                    self.broken = true;
                    return Err(format!("读取数据失败: {e}"));
                }
            };
            match reply {
                Reply::Chunk { len } => {
                    if let Err(e) = proto::read_exact(&mut self.reader, len, out) {
                        // 写端出错（用户暂停 / 本地盘满）：立刻断开，让 helper 那边
                        // 的下一次 write 马上 EPIPE，停止继续读盘；否则它会把
                        // socket 缓冲写满后才卡住，还会挡住 accept 新连接。
                        let _ = self.stream.shutdown(std::net::Shutdown::Both);
                        self.broken = true;
                        return Err(format!("接收数据失败: {e}"));
                    }
                }
                Reply::End { total } => {
                    out.flush().ok();
                    return Ok(total);
                }
                Reply::Error { message } => {
                    // 服务端主动报错时流是完整的，不需要重连
                    return Err(message);
                }
                other => {
                    self.broken = true;
                    return Err(unexpected(&other).to_string());
                }
            }
        }
    }

    pub fn shutdown(&mut self) {
        let _ = self.call(&Request::Shutdown);
    }
}

fn unexpected(reply: &Reply) -> std::io::Error {
    std::io::Error::new(
        std::io::ErrorKind::InvalidData,
        format!("helper 返回了意料之外的响应: {reply:?}"),
    )
}
