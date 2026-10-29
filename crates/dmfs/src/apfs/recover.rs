//! APFS 的"删除后还能怎么救"：说明文案 + 孤儿 inode 扫描。
//!
//! 单独成文件是因为这块**策略性远大于解析性**：APFS 的删除等于把 drec、inode、
//! extent 和 omap 条目一起回收，所以"扫目录项救文件"这条路在原理上就不通，
//! 能指望的只有 .Trashes、快照、孤儿 inode、雕刻四条。

use super::fs::Apfs;
use super::record;
use crate::types::*;
use dmdisk::Result;
use std::collections::HashSet;

/// 孤儿扫描的记录上限：超大卷上防止扫到天荒地老。
const ORPHAN_SCAN_CAP: usize = 2_000_000;

/// 给用户看的恢复路径说明（APFS 特有）。
pub fn hint(num_snaps: u64) -> Vec<String> {
    let mut v = vec![
        "① 先看 .Trashes：macOS 删除文件通常只是移动到 /.Trashes/<uid>/，\
         清空废纸篓之前文件仍是完整的，直接浏览该目录导出即可。"
            .to_string(),
    ];
    if num_snaps > 0 {
        v.push(format!(
            "② 该卷有 {num_snaps} 个本地快照。快照里保留着旧的 fs-tree，\
             按旧 xid 读 drec 并与当前树做 diff，能列出「曾经存在、现在没了」的文件。"
        ));
    } else {
        v.push(
            "② 该卷没有本地快照，旧 fs-tree 已被回收覆盖。\
             建议以后开启 `tmutil localsnapshot`，它是 APFS 上最有效的「删除后悔药」。"
                .into(),
        );
    }
    v.push(
        "③ 最后兜底是签名雕刻（v0.2 的 carving 模块）：按文件头尾特征扫原始块，\
         不依赖目录结构。"
            .into(),
    );
    v
}

/// 没有目录项指向的 inode。
///
/// 需要**两遍对齐**：drec 挂在父目录的 id 下，inode 挂在自己的 id 下，
/// 分属不同键区间，必须先把被引用的 id 收集起来再比对。
/// 全树扫描，用户显式调用 `recover` 才会跑。
pub fn orphan_scan(fs: &Apfs) -> Result<Vec<Entry>> {
    let mut referenced: HashSet<u64> = HashSet::new();
    let mut inodes: Vec<record::Inode> = Vec::new();
    let mut cap_hit = false;

    fs.walk_all(&mut |k, v| {
        match record::key_kind(k) {
            record::T_DIR_REC => {
                if let Some(d) = record::parse_drec(v) {
                    referenced.insert(d.file_id);
                }
            }
            record::T_INODE => {
                if let Some(i) = record::parse_inode(record::key_obj_id(k), v) {
                    inodes.push(i);
                }
            }
            _ => {}
        }
        if referenced.len() + inodes.len() >= ORPHAN_SCAN_CAP {
            cap_hit = true;
            return false;
        }
        true
    })?;
    fs.set_truncated(cap_hit);

    let mut out = Vec::new();
    for i in inodes {
        if referenced.contains(&i.id) || i.id <= super::fs::ROOT_DIR_ID {
            continue;
        }
        let name = i
            .name
            .clone()
            .unwrap_or_else(|| format!("orphan-{}", i.id));
        let mut e = Entry::new(NodeId(i.id), NodeId(i.parent), name, i.kind());
        e.size = i.size;
        e.mtime = record::ns_to_unix(i.mtime);
        e.deleted = true;
        e.name_lost = i.name.is_none();
        e.meta.push(("说明".into(), "无目录项指向（孤儿 inode）".into()));
        e.meta.push(("权限".into(), i.mode_string()));
        if let Some(f) = fs.first_extent(i.id) {
            e.start = f;
        }
        out.push(e);
    }
    Ok(out)
}
