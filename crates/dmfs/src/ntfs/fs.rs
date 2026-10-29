//! NTFS 只读文件系统 + 删除恢复。
//!
//! 两个关键设计决定：
//! 1. 列目录走 $INDEX_ROOT + $INDEX_ALLOCATION，而不是"扫全 MFT 按父引用过滤"。
//!    前者是 O(目录项)，后者在百万级文件的大卷上会慢到不可用。
//! 2. 恢复已删除文件靠扫全 MFT：被删除记录只是清掉"在用"标志位，
//!    runlist 通常仍然完整，所以 NTFS 的删除恢复成功率通常比 FAT 高。

use super::attr::{self, Attribute};
use super::boot::{self, NtfsBoot};
use super::index::{self, IndexEntry};
use super::record::{self, Record};
use super::runlist::Run;
use crate::types::*;
use dmdisk::{Error, Result, Reader};
use std::io::Write;

const ROOT_RECORD: u64 = 5;

pub struct Ntfs {
    reader: Reader,
    boot: NtfsBoot,
    info: FsInfo,
    mft_runs: Vec<Run>,
    record_count: u64,
    index_block_size: u32,
}

impl Ntfs {
    pub fn open(reader: Reader) -> Result<Self> {
        let boot = boot::parse(&reader)?;
        let mft_off = boot.lcn_offset(boot.mft_lcn);
        let mut rec = reader.read_vec(mft_off, boot.record_size as usize)?;
        let rec0 = record::parse(0, &mut rec, boot.bytes_per_sector)?
            .ok_or_else(|| Error::Format("无法读取 $MFT 自身记录".into()))?;
        let data = attr::find(&rec0.attrs, attr::ATTR_DATA, None)
            .ok_or_else(|| Error::Format("$MFT 缺少 $DATA 属性".into()))?;
        let mft_runs = data.runs.clone();
        if mft_runs.is_empty() {
            return Err(Error::Format("$MFT 的 runlist 为空".into()));
        }
        let mft_size = data.data_size;
        let record_count = (mft_size / boot.record_size as u64).max(32);
        let index_block_size = if boot.clusters_per_index > 0 {
            boot.clusters_per_index as u32 * boot.cluster_size
        } else {
            1u32 << (-(boot.clusters_per_index as i32)).clamp(9, 20) as u32
        };

        let mut info = FsInfo::new(FsKind::Ntfs);
        info.block_size = boot.cluster_size;
        info.total_bytes = boot.volume_bytes();
        info.uuid = Some(format!("{:016X}", boot.serial));
        info.detail("扇区大小", boot.bytes_per_sector);
        info.detail("每簇扇区", boot.sectors_per_cluster);
        info.detail("$MFT LCN", boot.mft_lcn);
        info.detail("MFT 大小", format!("{} 字节", mft_size));
        info.detail("MFT 记录大小", boot.record_size);
        info.detail("MFT 记录数", record_count);
        info.detail("索引块大小", index_block_size);

        // 卷标在记录 3 的 $VOLUME_NAME
        if let Ok(Some(vol)) = read_record_at(&reader, &boot, &mft_runs, 3) {
            if let Some(a) = attr::find(&vol.attrs, 0x60, None) {
                info.label = crate::util::utf16le_to_string(&a.resident);
            }
        }

        Ok(Self {
            reader,
            boot,
            info,
            mft_runs,
            record_count,
            index_block_size,
        })
    }

    pub fn record_count(&self) -> u64 {
        self.record_count
    }

    fn read_record(&self, number: u64) -> Result<Option<Record>> {
        if number >= self.record_count {
            return Ok(None);
        }
        let off = number * self.boot.record_size as u64;
        let mut buf = vec![0u8; self.boot.record_size as usize];
        let n = self.read_logical(&self.mft_runs, off, &mut buf)?;
        if n < 48 {
            return Ok(None);
        }
        record::parse(number, &mut buf, self.boot.bytes_per_sector)
    }

    /// 按逻辑偏移从 runlist 读取（稀疏 run 补零）。
    fn read_logical(&self, runs: &[Run], offset: u64, buf: &mut [u8]) -> Result<usize> {
        let cs = self.boot.cluster_size as u64;
        let mut done = 0usize;
        // 定位第一个可能覆盖的 run
        for run in runs {
            if done >= buf.len() {
                break;
            }
            let run_start = run.vcn * cs;
            let run_end = run_start + run.len * cs;
            let cur = offset + done as u64;
            if cur >= run_end {
                continue;
            }
            if cur + (buf.len() - done) as u64 <= run_start {
                break;
            }
            let skip = cur.saturating_sub(run_start);
            let take = ((run_end - cur) as usize).min(buf.len() - done);
            match run.lcn {
                Some(lcn) => {
                    let phys = lcn * cs + skip;
                    let mut tmp = vec![0u8; take];
                    self.reader.read(phys, &mut tmp)?;
                    buf[done..done + take].copy_from_slice(&tmp);
                }
                None => {
                    for b in &mut buf[done..done + take] {
                        *b = 0;
                    }
                }
            }
            done += take;
        }
        Ok(done)
    }

    fn attr_bytes(&self, a: &Attribute, cap: u64) -> Result<Vec<u8>> {
        if !a.non_resident {
            let n = a.resident.len().min(cap as usize);
            return Ok(a.resident[..n].to_vec());
        }
        let size = a.data_size.min(cap);
        let mut buf = vec![0u8; size as usize];
        self.read_logical(&a.runs, 0, &mut buf)?;
        Ok(buf)
    }

    /// 列出目录的全部索引项。
    ///
    /// 目录索引的属性名固定是 `$I30`（NTFS 3.x），少数工具会写成空名，
    /// 所以这里两种都接受——只按类型找会漏掉全部目录。
    fn dir_entries(&self, rec: &Record) -> Result<Vec<IndexEntry>> {
        let mut out = Vec::new();
        let pick = |type_id: u32| -> Option<&Attribute> {
            rec.attrs
                .iter()
                .find(|a| a.type_id == type_id && a.name.eq_ignore_ascii_case("$I30"))
                .or_else(|| rec.attrs.iter().find(|a| a.type_id == type_id && a.name.is_empty()))
        };
        if let Some(root) = pick(attr::ATTR_INDEX_ROOT) {
            if let Ok((start, len, _)) = attr::index_root_layout(root) {
                out.extend(index::parse_entries(&root.resident, start, start + len));
            }
        }
        if let Some(alloc) = pick(attr::ATTR_INDEX_ALLOC) {
            // 索引分配区上限 256 MiB，防止坏 runlist 拉爆内存
            let bytes = self.attr_bytes(alloc, 256 * 1024 * 1024)?;
            let bs = self.index_block_size.max(512) as usize;
            let mut off = 0usize;
            while off + bs <= bytes.len() {
                let mut block = bytes[off..off + bs].to_vec();
                if &block[0..4] == b"INDX" {
                    let _ = super::fixup::apply(&mut block, self.boot.bytes_per_sector);
                    out.extend(index::parse_indx_block(&block));
                }
                off += bs;
            }
        }
        Ok(out)
    }
}

fn read_record_at(
    reader: &Reader,
    boot: &NtfsBoot,
    runs: &[Run],
    number: u64,
) -> Result<Option<Record>> {
    let off = number * boot.record_size as u64;
    let cs = boot.cluster_size as u64;
    let mut buf = vec![0u8; boot.record_size as usize];
    let mut done = 0usize;
    for run in runs {
        if done >= buf.len() {
            break;
        }
        let run_start = run.vcn * cs;
        let run_end = run_start + run.len * cs;
        let cur = off + done as u64;
        if cur >= run_end || cur + (buf.len() - done) as u64 <= run_start {
            continue;
        }
        let skip = cur.saturating_sub(run_start);
        let take = ((run_end - cur) as usize).min(buf.len() - done);
        if let Some(lcn) = run.lcn {
            let mut tmp = vec![0u8; take];
            reader.read(lcn * cs + skip, &mut tmp)?;
            buf[done..done + take].copy_from_slice(&tmp);
        }
        done += take;
    }
    if done < 48 {
        return Ok(None);
    }
    record::parse(number, &mut buf, boot.bytes_per_sector)
}

impl FileSystem for Ntfs {
    fn info(&self) -> &FsInfo {
        &self.info
    }

    fn root(&self) -> NodeId {
        NodeId(ROOT_RECORD)
    }

    fn list(&self, dir: NodeId) -> Result<Vec<Entry>> {
        let rec = match self.read_record(dir.0)? {
            Some(r) => r,
            None => return Ok(Vec::new()),
        };
        let entries = self.dir_entries(&rec)?;
        let mut out = Vec::new();
        for e in entries {
            if e.file_ref == 0 || e.name.is_empty() {
                continue;
            }
            let kind = if e.is_dir { NodeKind::Dir } else { NodeKind::File };
            let mut ent = Entry::new(NodeId(e.file_ref), dir, e.name.clone(), kind);
            ent.size = e.size;
            ent.mtime = e.mtime;
            if let Some(vcn) = e.child_vcn {
                ent.meta.push(("子索引 VCN".into(), vcn.to_string()));
            }
            out.push(ent);
        }
        Ok(out)
    }

    fn read(&self, entry: &Entry, out: &mut dyn Write) -> Result<u64> {
        if entry.is_dir() {
            return Err(Error::Invalid("目录不能按文件读取".into()));
        }
        let rec = match self.read_record(entry.id.0)? {
            Some(r) => r,
            None => {
                return Err(Error::Invalid(format!(
                    "记录 {} 不存在（{}）",
                    entry.id.0, entry.name
                )))
            }
        };
        if rec.is_dir {
            return Err(Error::Invalid(format!("{} 是目录", entry.name)));
        }
        let data = match attr::find(&rec.attrs, attr::ATTR_DATA, None) {
            Some(a) => a,
            None => return Ok(0),
        };
        if data.is_compressed() {
            return Err(Error::Unsupported(format!(
                "{} 使用了 NTFS 压缩（暂不支持解压读取）",
                entry.name
            )));
        }
        if data.is_encrypted() {
            return Err(Error::Unsupported(format!(
                "{} 是 EFS 加密文件（无法读取）",
                entry.name
            )));
        }
        if !data.non_resident {
            let n = data.resident.len().min(entry.size as usize);
            out.write_all(&data.resident[..n])?;
            return Ok(n as u64);
        }
        let limit = if entry.size > 0 { entry.size } else { data.data_size };
        let cs = self.boot.cluster_size as u64;
        let mut written = 0u64;
        for run in &data.runs {
            if written >= limit {
                break;
            }
            let bytes = run.len * cs;
            let want = (limit - written).min(bytes) as usize;
            match run.lcn {
                Some(lcn) => {
                    let mut buf = vec![0u8; want];
                    self.reader.read(lcn * cs, &mut buf)?;
                    out.write_all(&buf)?;
                }
                None => {
                    let zeros = vec![0u8; want.min(1024 * 1024)];
                    let mut left = want;
                    while left > 0 {
                        let n = left.min(zeros.len());
                        out.write_all(&zeros[..n])?;
                        left -= n;
                    }
                }
            }
            written += want as u64;
        }
        Ok(written)
    }

    fn deleted(&self) -> Result<Vec<Entry>> {
        let mut out = Vec::new();
        for n in 24..self.record_count {
            let rec = match self.read_record(n) {
                Ok(Some(r)) => r,
                _ => continue,
            };
            if rec.in_use || rec.is_dir || rec.base_ref != 0 || rec.name.is_empty() {
                continue;
            }
            if rec.size == 0 {
                continue;
            }
            let mut e = Entry::new(NodeId(n), NodeId(rec.parent), rec.name.clone(), NodeKind::File);
            e.size = rec.size;
            e.deleted = true;
            e.mtime = rec.mtime;
            e.meta.push(("MFT 记录".into(), n.to_string()));
            if let Some(d) = attr::find(&rec.attrs, attr::ATTR_DATA, None) {
                if d.runs.is_empty() && d.non_resident {
                    e.meta.push(("警告".into(), "runlist 已清空，需雕刻恢复".into()));
                }
            }
            out.push(e);
            if out.len() > 200_000 {
                break;
            }
        }
        out.sort_by(|a, b| b.size.cmp(&a.size));
        Ok(out)
    }
}
