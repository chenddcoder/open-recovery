//! 文件系统无关的公共类型：条目、节点、卷信息。

use std::fmt;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FsKind {
    Fat12,
    Fat16,
    Fat32,
    ExFat,
    Ntfs,
    Ext2,
    Ext3,
    Ext4,
    HfsPlus,
    Apfs,
    Unknown,
}

impl FsKind {
    pub fn as_str(&self) -> &'static str {
        match self {
            FsKind::Fat12 => "FAT12",
            FsKind::Fat16 => "FAT16",
            FsKind::Fat32 => "FAT32",
            FsKind::ExFat => "exFAT",
            FsKind::Ntfs => "NTFS",
            FsKind::Ext2 => "ext2",
            FsKind::Ext3 => "ext3",
            FsKind::Ext4 => "ext4",
            FsKind::HfsPlus => "HFS+",
            FsKind::Apfs => "APFS",
            FsKind::Unknown => "未知",
        }
    }

    pub fn is_fat(&self) -> bool {
        matches!(self, FsKind::Fat12 | FsKind::Fat16 | FsKind::Fat32)
    }
}

impl fmt::Display for FsKind {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

/// 节点标识。语义由具体文件系统决定：
/// FAT/exFAT = 起始簇号，NTFS = MFT 记录号，ext = inode 号，HFS+ = CNID。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct NodeId(pub u64);

impl fmt::Display for NodeId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}", self.0)
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum NodeKind {
    File,
    Dir,
    Symlink,
    /// 设备/管道/套接字等
    Other,
}

impl NodeKind {
    pub fn as_char(&self) -> char {
        match self {
            NodeKind::File => '-',
            NodeKind::Dir => 'd',
            NodeKind::Symlink => 'l',
            NodeKind::Other => '?',
        }
    }

    pub fn as_str(&self) -> &'static str {
        match self {
            NodeKind::File => "文件",
            NodeKind::Dir => "目录",
            NodeKind::Symlink => "符号链接",
            NodeKind::Other => "其它",
        }
    }
}

/// 一条目录项 / 一个可恢复对象。
#[derive(Debug, Clone)]
pub struct Entry {
    pub id: NodeId,
    pub parent: NodeId,
    pub name: String,
    pub kind: NodeKind,
    /// 逻辑大小（字节）。删除后可能不准，恢复时按实际读到的字节为准。
    pub size: u64,
    pub deleted: bool,
    /// Unix 时间戳（秒）
    pub mtime: Option<i64>,
    /// 起始簇/块，恢复与雕刻的线索
    pub start: u64,
    /// 文件名是否已丢失（FAT 删除后首字节被抹成 0xE5）
    pub name_lost: bool,
    /// 额外展示字段（属性、权限、flags 等）
    pub meta: Vec<(String, String)>,
}

impl Entry {
    pub fn new(id: NodeId, parent: NodeId, name: impl Into<String>, kind: NodeKind) -> Self {
        Self {
            id,
            parent,
            name: name.into(),
            kind,
            size: 0,
            deleted: false,
            mtime: None,
            start: 0,
            name_lost: false,
            meta: Vec::new(),
        }
    }

    pub fn is_dir(&self) -> bool {
        self.kind == NodeKind::Dir
    }
}

/// 卷/文件系统摘要。
#[derive(Debug, Clone)]
pub struct FsInfo {
    pub kind: FsKind,
    pub label: String,
    pub uuid: Option<String>,
    /// 最小分配单元（簇/块）大小
    pub block_size: u32,
    pub total_bytes: u64,
    pub free_bytes: Option<u64>,
    pub details: Vec<(String, String)>,
}

impl FsInfo {
    pub fn new(kind: FsKind) -> Self {
        Self {
            kind,
            label: String::new(),
            uuid: None,
            block_size: 0,
            total_bytes: 0,
            free_bytes: None,
            details: Vec::new(),
        }
    }

    pub fn detail(&mut self, k: impl Into<String>, v: impl ToString) {
        self.details.push((k.into(), v.to_string()));
    }
}

/// 统一文件系统接口。所有实现都是只读。
pub trait FileSystem {
    fn info(&self) -> &FsInfo;
    fn root(&self) -> NodeId;
    /// 列目录（含已删除项，`Entry::deleted` 标记）。
    fn list(&self, dir: NodeId) -> crate::Result<Vec<Entry>>;
    /// 读出完整文件内容。`entry` 提供大小与起始位置（删除项需要它）。
    fn read(&self, entry: &Entry, out: &mut dyn std::io::Write) -> crate::Result<u64>;
    /// 全卷扫描可恢复的已删除文件。
    fn deleted(&self) -> crate::Result<Vec<Entry>>;
    /// 是否存在"按目录项恢复"的能力（HFS+/APFS 删除即销毁记录）。
    fn supports_deleted(&self) -> bool {
        true
    }

    /// 目录的聚合统计 `(子项数, 总字节数)`。
    ///
    /// 只有自己维护统计的文件系统才返回 `Some`（APFS 有 `j_dir_stats_val_t`）。
    /// 返回 `None` 时调用方需要自己递归遍历——在几百万文件的卷上那是分钟级操作，
    /// 所以「有没有这个接口」直接决定了能不能"秒看目录大小"。
    fn dir_stats(&self, _dir: NodeId) -> Option<(u64, u64)> {
        None
    }
}

/// 按路径逐级解析，例如 `/Users/a.txt` 或 `a/b/c`。
pub fn resolve<F: FileSystem + ?Sized>(fs: &F, path: &str) -> crate::Result<Option<Entry>> {
    let trimmed = path.trim_matches('/');
    if trimmed.is_empty() || trimmed == "." {
        let mut e = Entry::new(fs.root(), fs.root(), "/", NodeKind::Dir);
        e.meta.push(("说明".into(), "根目录".into()));
        return Ok(Some(e));
    }
    let mut cur = fs.root();
    let parts: Vec<&str> = trimmed.split('/').filter(|s| !s.is_empty()).collect();
    let mut found: Option<Entry> = None;
    for (i, part) in parts.iter().enumerate() {
        let entries = fs.list(cur)?;
        let hit = entries
            .into_iter()
            .find(|e| !e.deleted && e.name.eq_ignore_ascii_case(part))
            .or_else(|| {
                // 大小写不敏感没命中时再做一次精确匹配（区分大小写的卷）
                None
            });
        match hit {
            Some(e) => {
                cur = e.id;
                if i == parts.len() - 1 {
                    found = Some(e);
                }
            }
            None => return Ok(None),
        }
    }
    Ok(found)
}

/// 把路径拆成 (父目录路径, 名字)
pub fn split_parent(path: &str) -> (String, String) {
    let t = path.trim_end_matches('/');
    match t.rfind('/') {
        Some(i) => (t[..i].to_string(), t[i + 1..].to_string()),
        None => ("/".to_string(), t.to_string()),
    }
}

/// 文件名安全化：去掉路径分隔符与控制字符，避免恢复时越界写盘。
pub fn sanitize_name(name: &str) -> String {
    let mut s: String = name
        .chars()
        .map(|c| match c {
            '/' | '\\' | '\0' => '_',
            c if (c as u32) < 0x20 => '_',
            c => c,
        })
        .collect();
    if s.is_empty() || s == "." || s == ".." {
        s = format!("unnamed_{s}");
    }
    s
}
