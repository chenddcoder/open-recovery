//! HFS+ 目录文件（Catalog B-tree）解析。
//!
//! 这里刻意不去做 B-tree 的键查找/树下降：catalog 的叶子节点是一条双向链表，
//! 顺序扫一遍就能拿到全卷所有目录项，逻辑更短、也不怕树结构损坏。
//! 代价是首次列目录要把 catalog 读一遍——对几百万文件的卷也能接受（几百 MB 上限）。

use crate::util::{be16, be32, be64, hfs_to_unix, utf16be_to_string};

#[derive(Debug, Clone)]
pub struct CatEntry {
    pub cnid: u32,
    pub parent: u32,
    pub name: String,
    pub is_dir: bool,
    pub size: u64,
    pub mtime: Option<i64>,
    pub total_blocks: u32,
    pub extents: Vec<(u32, u32)>,
}

/// 从 catalog fork 数据里读出 B-tree 节点大小（header record 在节点 0）。
pub fn node_size(data: &[u8], offset: usize) -> Option<u16> {
    // B-tree header record 的 nodeSize 在 header record 偏移 18 处
    // 节点 0：14 字节描述符 + 2 字节偏移 + 记录
    if data.len() < offset + 14 + 2 + 20 {
        return None;
    }
    let p = offset + 14 + 2;
    let n = be16(data, p + 18);
    if (512..=32768).contains(&n) && n.is_power_of_two() {
        Some(n)
    } else {
        None
    }
}

/// 扫描全部叶节点，收集目录项。
pub fn parse_catalog(data: &[u8], node_size: u16) -> Vec<CatEntry> {
    let mut out = Vec::new();
    let ns = node_size as usize;
    let mut node_off = 0usize;
    let mut guard = 0usize;
    while node_off + ns <= data.len() && guard < 10_000_000 {
        guard += 1;
        let node = &data[node_off..node_off + ns];
        let kind = node[8] as i8;
        let num_records = be16(node, 10) as usize;
        if kind == -1 {
            // 叶子节点
            for r in 0..num_records {
                let off_pos = ns - 2 * (r + 1);
                if off_pos < 14 {
                    break;
                }
                let rec_off = be16(node, off_pos) as usize;
                if rec_off + 2 > ns {
                    continue;
                }
                if let Some(e) = parse_record(node, rec_off) {
                    out.push(e);
                }
            }
        }
        node_off += ns;
        if out.len() > 500_000 {
            break;
        }
    }
    out
}

fn parse_record(node: &[u8], off: usize) -> Option<CatEntry> {
    let key_len = be16(node, off) as usize;
    if key_len == 0 || off + 2 + key_len > node.len() {
        return None;
    }
    let key = &node[off + 2..off + 2 + key_len];
    if key.len() < 6 {
        return None;
    }
    let parent = be32(key, 0);
    let name_len = be16(key, 4) as usize;
    let name_end = (6 + name_len * 2).min(key.len());
    let name = utf16be_to_string(&key[6..name_end]);

    // 记录数据从 2+key_len 开始，且要偶数对齐
    let mut d = off + 2 + key_len;
    if d % 2 == 1 {
        d += 1;
    }
    if d + 2 > node.len() {
        return None;
    }
    let rec_type = be16(node, d) as i16;
    match rec_type {
        1 => {
            // 文件夹
            if d + 12 > node.len() {
                return None;
            }
            Some(CatEntry {
                cnid: be32(node, d + 8),
                parent,
                name,
                is_dir: true,
                size: 0,
                mtime: hfs_to_unix(be32(node, d + 16)),
                total_blocks: 0,
                extents: Vec::new(),
            })
        }
        2 => {
            // 文件：dataFork 在记录内偏移 88
            if d + 168 > node.len() {
                return None;
            }
            let fork = &node[d + 88..d + 168];
            let size = be64(fork, 0);
            let total_blocks = be32(fork, 12);
            let mut extents = Vec::new();
            for i in 0..8 {
                let eo = 16 + i * 8;
                let start = be32(fork, eo);
                let count = be32(fork, eo + 4);
                if count > 0 {
                    extents.push((start, count));
                }
            }
            Some(CatEntry {
                cnid: be32(node, d + 8),
                parent,
                name,
                is_dir: false,
                size,
                mtime: hfs_to_unix(be32(node, d + 16)),
                total_blocks,
                extents,
            })
        }
        // 3/4 是 thread 记录，跳过
        _ => None,
    }
}
