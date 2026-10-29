//! 一个已挂载会话：打开源 → 定位卷 → 列目录 / 预览 / 读文件。
//!
//! GUI（本地镜像）和 helper（特权设备）都调这里的实现，保证两边行为完全一致；
//! 提权只是把 `AnyFs` 放在另一个进程里，解析逻辑一行都没有分叉。

use dmdisk::{human_size, Source};
use dmfs::{probe_partition, resolve, AnyFs, Entry, FileSystem, FsKind};
use std::collections::HashMap;
use std::io::Write;

use crate::model::{EntryDto, PartDto, PreviewDto, SourceDto};

/// 打开失败。`permission` 为真表示"这台机器上需要更高权限"，GUI 据此弹授权窗。
#[derive(Debug, Clone)]
pub struct OpenError {
    pub message: String,
    pub permission: bool,
}

impl OpenError {
    pub fn plain(message: impl Into<String>) -> Self {
        Self { message: message.into(), permission: false }
    }
}

impl From<dmfs::Error> for OpenError {
    fn from(e: dmfs::Error) -> Self {
        Self { permission: matches!(e, dmfs::Error::Permission(_)), message: e.to_string() }
    }
}

pub struct Session {
    pub source: String,
    pub desc: String,
    pub fs: AnyFs,
    /// 见过条目按 id 缓存：已删除项没有可用路径，按 id 读最稳。
    cache: HashMap<u64, Entry>,
}

impl Session {
    pub fn list_dir(&mut self, path: &str, show_deleted: bool) -> Result<Vec<EntryDto>, String> {
        let node = if path == "/" || path.is_empty() {
            self.fs.root()
        } else {
            resolve(&self.fs, path)
                .map_err(|e| e.to_string())?
                .ok_or_else(|| format!("路径不存在: {path}"))?
                .id
        };
        let entries = self.fs.list(node).map_err(|e| e.to_string())?;
        self.remember(&entries);
        let mut out = entries;
        if !show_deleted {
            out.retain(|e| !e.deleted);
        }
        out.sort_by_key(|e| (!e.is_dir(), e.name.to_lowercase()));
        let base = if path.is_empty() { "/" } else { path };
        Ok(out.iter().map(|e| to_dto(base, e)).collect())
    }

    /// 取单个条目的信息（判断是文件还是目录）。路径不存在返回 `Ok(None)`。
    ///
    /// 顺手把条目放进缓存：紧接着的 `read_id` 依赖它，否则会报"条目已失效"
    /// （症状是导出的文件存在但内容为空）。
    pub fn stat(&mut self, path: &str) -> Result<Option<EntryDto>, String> {
        let e = resolve(&self.fs, path).map_err(|e| e.to_string())?;
        Ok(match e {
            Some(e) => {
                let id = e.id.0;
                let mut d = to_dto("", &e);
                d.path = path.to_string();
                self.cache.insert(id, e);
                Some(d)
            }
            None => None,
        })
    }

    pub fn preview(&self, path: &str, max: usize) -> Result<PreviewDto, String> {
        let entry = resolve(&self.fs, path)
            .map_err(|e| e.to_string())?
            .ok_or_else(|| format!("路径不存在: {path}"))?;
        self.preview_entry(&entry, max)
    }

    /// 按 id 预览：已删除项没有可信路径，只能走这条。
    pub fn preview_by_id(&self, id: u64, max: usize) -> Result<PreviewDto, String> {
        let entry = self
            .cache
            .get(&id)
            .ok_or_else(|| "条目已失效，请重新浏览或重新扫描".to_string())?;
        self.preview_entry(entry, max)
    }

    fn preview_entry(&self, entry: &Entry, max: usize) -> Result<PreviewDto, String> {
        if entry.is_dir() {
            return Err("目录不能预览".into());
        }
        let mut buf = Vec::new();
        self.fs.read(&entry, &mut buf).map_err(|e| e.to_string())?;
        let truncated = buf.len() > max;
        if truncated {
            buf.truncate(max);
        }
        let (text, mode) = match String::from_utf8(buf.clone()) {
            Ok(t) => {
                let ctrl = t
                    .chars()
                    .filter(|c| c.is_control() && *c != '\n' && *c != '\r' && *c != '\t')
                    .count();
                if ctrl * 20 > t.chars().count().max(1) {
                    (hex_dump(&buf), "hex".to_string())
                } else {
                    (t, "text".to_string())
                }
            }
            Err(_) => (hex_dump(&buf), "hex".to_string()),
        };
        Ok(PreviewDto { text, mode, size: entry.size, truncated })
    }

    pub fn deleted(&mut self, limit: usize) -> Result<Vec<EntryDto>, String> {
        if !self.fs.supports_deleted() {
            return Err(format!(
                "{} 删除时会销毁目录记录，需要用签名雕刻恢复（规划中）",
                self.fs.info().kind.as_str()
            ));
        }
        let mut items = self.fs.deleted().map_err(|e| e.to_string())?;
        self.remember(&items);
        if limit > 0 {
            items.truncate(limit);
        }
        Ok(items.iter().map(|e| to_dto("", e)).collect())
    }

    pub fn read_id(&self, id: u64, out: &mut dyn Write) -> Result<u64, String> {
        let entry = self
            .cache
            .get(&id)
            .ok_or_else(|| "条目已失效，请重新浏览或重新扫描".to_string())?;
        self.fs.read(entry, out).map_err(|e| e.to_string())
    }

    pub fn read_path(&self, path: &str, out: &mut dyn Write) -> Result<u64, String> {
        let entry = resolve(&self.fs, path)
            .map_err(|e| e.to_string())?
            .ok_or_else(|| format!("路径不存在: {path}"))?;
        if entry.is_dir() {
            return Err("目录不能按文件读取".into());
        }
        self.fs.read(&entry, out).map_err(|e| e.to_string())
    }

    fn remember(&mut self, entries: &[Entry]) {
        for e in entries {
            self.cache.insert(e.id.0, e.clone());
        }
    }
}

/// 打开源；`partition` 为 None 时自动挑选唯一可挂载分区。
pub fn open(source: &str, partition: Option<usize>) -> Result<(Session, SourceDto), OpenError> {
    let src = Source::open(source, 0)?;
    let table = src.table().map_err(OpenError::from)?;

    let mut partitions = Vec::new();
    for p in &table.partitions {
        let kind = probe_partition(&src.volume(p.offset, p.size));
        partitions.push(PartDto {
            index: p.index,
            type_name: p.type_name.clone(),
            size: p.size,
            size_human: human_size(p.size),
            offset: p.offset,
            fs_kind: kind.as_str().to_string(),
            mountable: kind != FsKind::Unknown,
        });
    }

    let (reader, desc) = match partition {
        Some(idx) => {
            let p = table
                .partitions
                .get(idx.saturating_sub(1))
                .ok_or_else(|| OpenError::plain(format!("分区 {idx} 不存在")))?;
            (src.volume(p.offset, p.size), format!("{source} 分区{idx}（{}）", p.type_name))
        }
        None => {
            if dmfs::probe(&src.reader) != FsKind::Unknown {
                (src.reader.slice(0, src.size), source.to_string())
            } else {
                // 自动挑"最像数据分区"的那个，而不是一上来就让用户做选择题：
                // 一块 Mac 盘通常是 [EFI 200MB(FAT32)] + [APFS 1.8TB]，
                // 让人在两者之间选毫无意义。
                let mut cands: Vec<(&dmdisk::Partition, FsKind)> = table
                    .partitions
                    .iter()
                    .filter_map(|p| {
                        let k = probe_partition(&src.volume(p.offset, p.size));
                        if k == FsKind::Unknown {
                            None
                        } else {
                            Some((p, k))
                        }
                    })
                    // EFI 系统分区只放引导文件，绝不该是"恢复目标"
                    .filter(|(p, _)| {
                        !(p.type_name.to_ascii_lowercase().contains("efi") && p.size < 4 << 30)
                    })
                    .collect();
                // APFS 优先，其余按体积降序（数据分区总是最大的那个）
                cands.sort_by(|a, b| {
                    let ka = (a.1 == FsKind::Apfs) as u8;
                    let kb = (b.1 == FsKind::Apfs) as u8;
                    kb.cmp(&ka).then_with(|| b.0.size.cmp(&a.0.size))
                });
                if cands.is_empty() {
                    return Err(OpenError::plain(format!(
                        "{source} 上没有可挂载的文件系统（分区表 {}）",
                        table.scheme.as_str()
                    )));
                }
                let (p, _k) = cands[0];
                (
                    src.volume(p.offset, p.size),
                    format!("{source} 分区{}（{}）", p.index, p.type_name),
                )
            }
        }
    };

    // APFS 是容器套卷：真机上卷 0 常常是 Preboot/System，恢复要的却是 Data 卷。
    // 这里自动挑「角色含 Data 的卷」，挑不到再退回 0，并把卷名写进描述。
    let mut desc = desc;
    let fs = if probe_partition(&reader) == FsKind::Apfs {
        let idx = pick_apfs_volume(&reader);
        let fs = AnyFs::open_volume(reader, idx).map_err(OpenError::from)?;
        desc = format!("{desc} · 卷「{}」", fs.info().label);
        fs
    } else {
        AnyFs::open(reader).map_err(OpenError::from)?
    };
    let info = fs.info().clone();
    let dto = SourceDto {
        desc: desc.clone(),
        scheme: table.scheme.as_str().to_string(),
        verified: table.verified,
        notes: table.notes.clone(),
        partitions,
        fs_kind: info.kind.as_str().to_string(),
        label: info.label.clone(),
        uuid: info.uuid.clone().unwrap_or_default(),
        block_size: info.block_size,
        total_bytes: info.total_bytes,
        total_human: human_size(info.total_bytes),
        free_bytes: info.free_bytes,
        details: info.details.clone(),
    };
    Ok((Session { source: source.to_string(), desc, fs, cache: HashMap::new() }, dto))
}

/// 选一个适合浏览/恢复的 APFS 卷：优先角色含 Data（0x2）的，否则第 0 个。
fn pick_apfs_volume(reader: &dmdisk::Reader) -> usize {
    // 0x40 = DATA（macOS 10.15+ 的 "- Data" 卷），0x2 = USER（更早的写法）
    const ROLE_DATA: u16 = 0x0040;
    const ROLE_USER: u16 = 0x0002;
    let vols = match dmfs::apfs::volumes(reader) {
        Ok((_, v)) => v,
        Err(_) => return 0,
    };
    // 1) 角色位
    for (i, v) in &vols {
        if let Ok(v) = v {
            if v.role & (ROLE_DATA | ROLE_USER) != 0 {
                return *i;
            }
        }
    }
    // 2) 名字里带 Data（有些盘的角色位不规范，但卷名可靠）
    for (i, v) in &vols {
        if let Ok(v) = v {
            if v.name.to_ascii_lowercase().contains("data") {
                return *i;
            }
        }
    }
    0
}

fn to_dto(base: &str, e: &Entry) -> EntryDto {
    let path = if base == "/" || base.is_empty() {
        format!("/{}", e.name)
    } else {
        format!("{}/{}", base.trim_end_matches('/'), e.name)
    };
    EntryDto {
        name: e.name.clone(),
        path,
        id: e.id.0,
        is_dir: e.is_dir(),
        size: e.size,
        size_human: human_size(e.size),
        deleted: e.deleted,
        mtime: e.mtime,
        name_lost: e.name_lost,
    }
}

fn hex_dump(buf: &[u8]) -> String {
    let mut out = String::with_capacity(buf.len() * 4);
    for (i, chunk) in buf.chunks(16).enumerate() {
        out.push_str(&format!("{:08x}  ", i * 16));
        for b in chunk {
            out.push_str(&format!("{b:02x} "));
        }
        for _ in chunk.len()..16 {
            out.push_str("   ");
        }
        out.push_str(" |");
        for b in chunk {
            let c = *b as char;
            out.push(if c.is_ascii_graphic() || c == ' ' { c } else { '.' });
        }
        out.push_str("|\n");
    }
    out
}
