//! GUI ↔ helper 的行协议。
//!
//! 每帧 = 一行 JSON（`\n` 结尾）。需要传字节流时（读文件）先发 `chunk` 帧再发原始
//! 数据，最后 `end` 收尾——这样大文件也是常数内存，不必先知道总长度。

use serde::{Deserialize, Serialize};
use std::io::{BufRead, Read, Write};

use crate::model::{DiskDto, EntryDto, PreviewDto, SourceDto};

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "op", rename_all = "snake_case")]
pub enum Request {
    /// 必须是连接上的第一条请求，否则拒绝一切操作。
    Hello { token: String },
    Ping,
    Disks,
    Open { source: String, partition: Option<usize> },
    Ls { path: String, show_deleted: bool },
    Preview { path: String, max: usize },
    PreviewId { id: u64, max: usize },
    Deleted { limit: usize },
    /// 按 id 读（已删除项没有可用路径）
    ReadId { id: u64 },
    ReadPath { path: String },
    Shutdown,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum Reply {
    Hello { version: String, pid: u32 },
    Ok,
    Error { message: String },
    Disks { disks: Vec<DiskDto> },
    Source { source: SourceDto },
    Entries { entries: Vec<EntryDto> },
    Preview { preview: PreviewDto },
    /// 后跟 `len` 字节原始数据
    Chunk { len: u64 },
    /// 流结束
    End { total: u64 },
}

pub fn write_request(w: &mut dyn Write, req: &Request) -> std::io::Result<()> {
    let mut line = serde_json::to_vec(req)?;
    line.push(b'\n');
    w.write_all(&line)?;
    w.flush()
}

pub fn read_request(r: &mut dyn BufRead) -> std::io::Result<Option<Request>> {
    match read_line(r)? {
        Some(bytes) => serde_json::from_slice(&bytes)
            .map(Some)
            .map_err(|e| std::io::Error::new(std::io::ErrorKind::InvalidData, e)),
        None => Ok(None),
    }
}

pub fn write_reply(w: &mut dyn Write, reply: &Reply) -> std::io::Result<()> {
    let mut line = serde_json::to_vec(reply)?;
    line.push(b'\n');
    w.write_all(&line)?;
    w.flush()
}

pub fn read_reply(r: &mut dyn BufRead) -> std::io::Result<Reply> {
    match read_line(r)? {
        Some(bytes) => serde_json::from_slice(&bytes)
            .map_err(|e| std::io::Error::new(std::io::ErrorKind::InvalidData, e)),
        None => Err(std::io::Error::new(
            std::io::ErrorKind::UnexpectedEof,
            "helper 关闭了连接",
        )),
    }
}

/// 读一行（去掉换行）。空行视为协议错误，避免把空行当 EOF。
fn read_line(r: &mut dyn BufRead) -> std::io::Result<Option<Vec<u8>>> {
    let mut buf = Vec::new();
    let n = r.read_until(b'\n', &mut buf)?;
    if n == 0 {
        return Ok(None);
    }
    while matches!(buf.last(), Some(b'\n') | Some(b'\r')) {
        buf.pop();
    }
    if buf.is_empty() {
        return Err(std::io::Error::new(
            std::io::ErrorKind::InvalidData,
            "收到空行",
        ));
    }
    Ok(Some(buf))
}

/// 恰好读满 `len` 字节（`Read::read_exact` 的语义，但显式拒绝 0 长度抖动）。
pub fn read_exact(r: &mut dyn Read, len: u64, out: &mut dyn Write) -> std::io::Result<u64> {
    let mut left = len;
    let mut buf = vec![0u8; 128 * 1024];
    while left > 0 {
        let want = left.min(buf.len() as u64) as usize;
        r.read_exact(&mut buf[..want])?;
        out.write_all(&buf[..want])?;
        left -= want as u64;
    }
    Ok(len)
}
