//! 容器头修复的端到端测试。
//!
//! 用 `apfs-broken0.dmg`（块 0 被清空的整盘镜像）走完整链路：
//! 扫描 → 挑副本 → 写回 → 重新打开。修复要能真的把盘救回"不依赖副本兜底"的状态。

use dmrpc::repair;

fn image() -> Option<&'static str> {
    let p = "/tmp/dmde-test/apfs-broken0.dmg";
    if std::path::Path::new(p).exists() {
        Some(p)
    } else {
        None
    }
}

/// 复制一份再动，绝不能让测试改掉共享的测试镜像。
fn scratch(tag: &str) -> Option<(std::path::PathBuf, String)> {
    let img = image()?;
    let p = std::env::temp_dir().join(format!("dmde-{tag}-{}.img", std::process::id()));
    let _ = std::fs::remove_file(&p);
    std::fs::copy(img, &p).expect("复制测试镜像");
    let s = p.to_str().unwrap().to_string();
    Some((p, s))
}

/// 扫描能发现块 0 已损坏，并列出可用的检查点副本。
#[test]
fn scan_reports_broken_block0_and_lists_copies() {
    let Some((tmp, path)) = scratch("scan") else {
        eprintln!("跳过：测试镜像不存在");
        return;
    };
    let (_s, dto) = dmrpc::open_local(&path, None).expect("打开镜像");
    let rep = repair::scan_nxsb(&path, dto.container_offset, dto.block_size).expect("扫描容器头");

    assert!(!rep.block0_ok, "这个镜像的块 0 本来就是坏的");
    assert!(
        !rep.candidates.is_empty(),
        "检查点区里应该有可用的 NXSB 副本"
    );
    // 候选按 xid 降序：第一份就是最新的
    assert!(
        rep.candidates.windows(2).all(|w| w[0].xid >= w[1].xid),
        "候选必须按 xid 从新到旧排列"
    );
    assert!(rep.candidates[0].volume_count > 0, "副本里应能读出卷");

    let _ = std::fs::remove_file(&tmp);
}

/// 写回最新副本后：块 0 有效、能正常打开、不再走"副本兜底"。
#[test]
fn repair_restores_block0_and_normal_open() {
    let Some((tmp, path)) = scratch("repair") else {
        eprintln!("跳过：测试镜像不存在");
        return;
    };
    let (_s, dto) = dmrpc::open_local(&path, None).expect("打开镜像");
    let off = dto.container_offset;
    let bs = dto.block_size;

    let rep = repair::scan_nxsb(&path, off, bs).expect("扫描");
    let best = rep.candidates.first().expect("应有候选").paddr;

    let backup = std::env::temp_dir().join(format!("dmde-block0-{}.bin", std::process::id()));
    let _ = std::fs::remove_file(&backup);
    let out = repair::repair_nxsb(&path, off, bs, best, backup.to_str().unwrap()).expect("修复应成功");

    assert_eq!(out.bytes, bs as u64, "只写一个块");
    assert!(backup.exists(), "必须留下原块 0 的备份");
    assert_eq!(
        std::fs::metadata(&backup).unwrap().len(),
        bs as u64,
        "备份就是一个块"
    );

    // 块 0 现在应该是一份有效 NXSB
    let rep2 = repair::scan_nxsb(&path, off, bs).expect("再次扫描");
    assert!(rep2.block0_ok, "修复后块 0 必须是有效 NXSB");
    assert_eq!(rep2.block0_xid, Some(out.xid));

    // 打开时不该再提示"块 0 已损坏，已改用检查点副本"
    let (_s2, dto2) = dmrpc::open_local(&path, None).expect("修复后应能打开");
    assert_eq!(dto2.fs_kind, "APFS");
    assert!(
        !dto2.notes.iter().any(|n| n.contains("块 0")),
        "修复后不该再走副本兜底，notes={:?}",
        dto2.notes
    );

    let _ = std::fs::remove_file(&tmp);
    let _ = std::fs::remove_file(&backup);
}

/// 源块号传 0（等于把块 0 写回自己）必须被拒绝——
/// 这条是防止误操作把本来还有救的盘写得更烂。
#[test]
fn repair_rejects_block0_as_source() {
    let Some((tmp, path)) = scratch("reject") else {
        return;
    };
    let err = repair::repair_nxsb(&path, 0, 4096, 0, "/tmp/should-not-exist.bin")
        .expect_err("源块 0 应被拒绝");
    assert!(err.contains("块 0"), "错误信息要说清原因: {err}");
    let _ = std::fs::remove_file(&tmp);
}

/// 指向一个不含 NXSB 的块时必须拒绝，不能拿垃圾覆盖块 0。
#[test]
fn repair_rejects_non_nxsb_block() {
    let Some((tmp, path)) = scratch("junk") else {
        return;
    };
    // 镜像末尾的块基本都是空的
    let blocks = std::fs::metadata(&path).unwrap().len() / 4096;
    let err = repair::repair_nxsb(&path, 0, 4096, blocks - 2, "/tmp/should-not-exist.bin")
        .expect_err("空块应被拒绝");
    assert!(err.contains("NXSB"), "错误信息要说清原因: {err}");
    let _ = std::fs::remove_file(&tmp);
}
