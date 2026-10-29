//! helper 客户端。所有调用都返回人类可读的中文错误，方便直接展示到状态栏。

use crate::model::{DirSizeDto, DiskDto, EntryDto, PreviewDto, SourceDto};
use crate::proto::{self, Reply, Request};
use std::io::{BufReader, Write};
use std::os::unix::net::UnixStream;
use std::path::Path;

pub struct Client {
    stream: UnixStream,
    reader: BufReader<UnixStream>,
}

impl Client {
    /// 连接并完成 hello 握手；失败说明 socket 不是我们的 helper。
    pub fn connect(socket: &Path, token: &str) -> std::io::Result<Client> {
        let stream = UnixStream::connect(socket)?;
        let mut c = Client { reader: BufReader::new(stream.try_clone()?), stream };
        match c.call(&Request::Hello { token: token.to_string() })? {
            Reply::Hello { .. } => Ok(c),
            Reply::Error { message } => Err(std::io::Error::new(std::io::ErrorKind::PermissionDenied, message)),
            other => Err(unexpected(&other)),
        }
    }

    fn call(&mut self, req: &Request) -> std::io::Result<Reply> {
        proto::write_request(&mut self.stream, req)?;
        proto::read_reply(&mut self.reader)
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

    pub fn dir_size(&mut self, path: &str, budget_ms: u64) -> Result<DirSizeDto, String> {
        match self.call(&Request::DirSize { path: path.to_string(), budget_ms }) {
            Ok(Reply::DirSize { size }) => Ok(size),
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
    fn drain(&mut self, out: &mut dyn Write) -> Result<u64, String> {
        loop {
            let reply = proto::read_reply(&mut self.reader)
                .map_err(|e| format!("读取数据失败: {e}"))?;
            match reply {
                Reply::Chunk { len } => {
                    proto::read_exact(&mut self.reader, len, out)
                        .map_err(|e| format!("接收数据失败: {e}"))?;
                }
                Reply::End { total } => {
                    out.flush().ok();
                    return Ok(total);
                }
                Reply::Error { message } => return Err(message),
                other => return Err(unexpected(&other).to_string()),
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
