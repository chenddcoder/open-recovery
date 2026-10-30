//! APFS 容器头修复的执行体（helper 与本地后端共用）。
//!
//! 顺序刻意写成"先校验、再备份、后写入、最后回读"，任何一步失败都不会
//! 让盘处在"写了一半"的状态：
//!
//! 1. 读源块，校验它确实是 NXSB（不校验通过就拒绝写，避免拿垃圾覆盖垃圾）；
//! 2. 读出原块 0 存到备份文件；
//! 3. 写入源块内容；
//! 4. 回读比对，不一致就明确告诉用户用备份回滚。
//!
//! 全程只碰 `[container_offset, container_offset + block_size)` 这一个块。

use crate::model::{NxsbCandidateDto, NxsbReportDto};
use dmdisk::write;
use dmfs::apfs::repair;

#[derive(Debug)]
pub struct RepairOutcome {
    pub bytes: u64,
    /// 写进去的那份副本的事务号，界面上用来确认"选的是哪一版"
    pub xid: u64,
    pub backup_path: String,
}

/// 扫容器开头的块，列出可用的 NXSB 副本。
///
/// 走独立读通道而不是复用会话：块 0 坏掉时 `Container::open` 是靠"扫到副本"
/// 才活下来的，但这里要把**所有**副本都列出来给用户挑，不能只挑最新的那份。
pub fn scan_nxsb(
    source: &str,
    container_offset: u64,
    block_size: u32,
) -> Result<NxsbReportDto, String> {
    let src = dmdisk::Source::open(source, 0).map_err(|e| format!("打开 {source} 失败: {e}"))?;
    if container_offset >= src.size {
        return Err(format!(
            "容器偏移 {container_offset} 超出设备大小 {}",
            src.size
        ));
    }
    let reader = src.reader.slice(container_offset, src.size - container_offset);
    let rep = repair::scan(&reader).map_err(|e| format!("扫描容器头失败: {e}"))?;
    Ok(NxsbReportDto {
        block0_ok: rep.block0_ok,
        block0_xid: rep.block0_xid,
        // 调用方给了块大小就用它的（那是打开时探测出来的真实值）
        block_size: if block_size > 0 { block_size } else { rep.block_size },
        candidates: rep
            .candidates
            .iter()
            .map(|c| NxsbCandidateDto {
                paddr: c.paddr,
                xid: c.xid,
                volume_count: c.volume_count,
                uuid: c.uuid.clone(),
            })
            .collect(),
        cksum_bad: rep.cksum_bad,
        scanned_blocks: rep.scanned_blocks,
    })
}

/// 把 `src_paddr` 处的 NXSB 副本写回容器块 0。
pub fn repair_nxsb(
    source: &str,
    container_offset: u64,
    block_size: u32,
    src_paddr: u64,
    backup_path: &str,
) -> Result<RepairOutcome, String> {
    if src_paddr == 0 {
        return Err("源块不能是块 0 自己".into());
    }
    if !block_size.is_power_of_two() || !(512..=65_536).contains(&block_size) {
        return Err(format!("块大小 {block_size} 不合理，拒绝写入"));
    }
    if source.is_empty() || backup_path.is_empty() {
        return Err("设备路径和备份路径都不能为空".into());
    }

    let bs = block_size as u64;
    let src_off = container_offset
        .checked_add(src_paddr.checked_mul(bs).ok_or("源块号溢出")?)
        .ok_or("源块偏移溢出")?;

    // 1) 读源块并校验
    let data = write::read_at_path(source, src_off, block_size as usize)
        .map_err(|e| format!("读源块（块 {src_paddr}）失败: {e}"))?;
    let cand = repair::verify_block(src_paddr, &data).ok_or_else(|| {
        format!("块 {src_paddr} 不是校验通过的 NXSB 超级块，拒绝写入（避免用垃圾覆盖块 0）")
    })?;

    // 2) 备份原块 0
    let orig = write::read_at_path(source, container_offset, block_size as usize)
        .map_err(|e| format!("读原块 0 失败: {e}"))?;
    if let Some(dir) = std::path::Path::new(backup_path).parent() {
        std::fs::create_dir_all(dir).map_err(|e| format!("创建备份目录失败: {e}"))?;
    }
    std::fs::write(backup_path, &orig).map_err(|e| format!("写备份失败: {e}"))?;

    // 3) 写入
    write::write_at_path(source, container_offset, &data)
        .map_err(|e| format!("写块 0 失败: {e}"))?;

    // 4) 回读校验
    let back = write::read_at_path(source, container_offset, block_size as usize)
        .map_err(|e| format!("写后回读失败: {e}"))?;
    if back != data {
        return Err(format!(
            "写后回读与源块不一致，块 0 可能没写好；请用备份回滚：{backup_path}"
        ));
    }

    Ok(RepairOutcome {
        bytes: data.len() as u64,
        xid: cand.xid,
        backup_path: backup_path.to_string(),
    })
}
