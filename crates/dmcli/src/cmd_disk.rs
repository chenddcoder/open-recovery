//! 磁盘级命令：devices / volumes / info

use crate::args::Opts;
use crate::fmt;
use crate::source::open_fs;
use dmdisk::{human_size, scheme, Source};
use dmfs::probe;
use dmfs::FileSystem;

pub fn devices(o: &Opts) -> Result<(), String> {
    let disks = dmdisk::list_disks().map_err(|e| e.to_string())?;
    if disks.is_empty() {
        println!("没有找到物理磁盘（Windows 平台尚未支持）");
        return Ok(());
    }
    println!(
        "{:<3} {:<16} {:>10}  {:<28} {:<6} {}",
        "#", "设备", "容量", "型号", "总线", "类型"
    );
    println!("{}", "-".repeat(88));
    for (i, d) in disks.iter().enumerate() {
        println!(
            "{:<3} {:<16} {:>10}  {:<28} {:<6} {}",
            i + 1,
            d.dev_node,
            human_size(d.size),
            truncate(&d.model, 28),
            if d.bus.is_empty() { "-" } else { &d.bus },
            if d.internal { "内置" } else { "外置" }
        );
    }
    if o.verbose {
        println!();
        for d in &disks {
            println!("== {} ({}) ==", d.dev_node, d.bsd);
            println!("  容量: {}  可移动: {}  内容: {}", human_size(d.size), d.removable, d.content);
            // 尝试解析分区表（需要 root，失败就提示）
            match Source::open(&d.dev_node, d.size) {
                Ok(src) => match src.table() {
                    Ok(t) => {
                        println!("  分区表: {}", t.scheme.as_str());
                        for p in &t.partitions {
                            let kind = probe(&src.volume(p.offset, p.size));
                            println!(
                                "    #{:<2} {:<26} {:>10}  {}",
                                p.index,
                                truncate(&p.type_name, 26),
                                human_size(p.size),
                                kind.as_str()
                            );
                        }
                    }
                    Err(e) => println!("  分区表解析失败: {e}"),
                },
                Err(_) => println!("  （需要 sudo 才能读取设备内容）"),
            }
        }
    }
    Ok(())
}

pub fn volumes(o: &Opts) -> Result<(), String> {
    let source_path = o.positional.first().ok_or("volumes 需要指定源")?;
    let src = Source::open(source_path, o.size).map_err(|e| e.to_string())?;
    println!("源      : {}", src.label);
    println!("容量    : {}", human_size(src.size));
    println!("扇区    : {} 字节", src.sector_size);
    println!();

    let table = src.table().map_err(|e| e.to_string())?;
    println!("分区表  : {}", table.scheme.as_str());
    if table.verified {
        println!("校验    : 通过");
    }
    for n in &table.notes {
        println!("备注    : {n}");
    }
    if table.partitions.is_empty() {
        // 可能是整盘文件系统
        let kind = probe(&src.reader);
        if kind != dmfs::FsKind::Unknown {
            println!("整个源就是一个 {} 卷（无分区表）", kind.as_str());
        } else {
            println!("未发现分区，也未识别出文件系统");
        }
        return Ok(());
    }
    println!();
    println!(
        "{:<3} {:<8} {:>12} {:>14}  {:<10} {:<24} 文件系统",
        "#", "类型", "起始偏移", "大小", "格式", "分区标识"
    );
    println!("{}", "-".repeat(96));
    let mut has_apfs = false;
    for p in &table.partitions {
        let kind = probe(&src.volume(p.offset, p.size));
        println!(
            "{:<3} {:<8} {:>12} {:>14}  {:<10} {:<24} {}",
            p.index,
            match p.kind {
                scheme::PartitionKind::Primary => "主分区",
                scheme::PartitionKind::Extended => "扩展",
                scheme::PartitionKind::Logical => "逻辑",
                scheme::PartitionKind::Backup => "备份",
            },
            p.offset,
            human_size(p.size),
            kind.as_str(),
            truncate(&p.type_name, 24),
            probe_detail(&kind)
        );
        // APFS 是容器套卷，把内层卷也列出来，否则用户不知道 --volume 该给几
        if kind == dmfs::FsKind::Apfs {
            let reader = src.volume(p.offset, p.size);
            if let Ok((c, vols)) = dmfs::apfs::volumes(&reader) {
                has_apfs = true;
                for (i, v) in vols {
                    match v {
                        Ok(v) => println!(
                            "      └ 卷{i}  {:<24} {:>10}  {}",
                            truncate(&v.name, 24),
                            human_size(v.alloc_count.saturating_mul(c.bs as u64)),
                            v.role_name()
                        ),
                        Err(e) => println!("      └ 卷{i}  解析失败: {e}"),
                    }
                }
            }
        }
    }
    if has_apfs {
        println!();
        println!("提示: APFS 是容器套卷，用 `--volume N` 选择内层卷（默认 0）");
    }
    Ok(())
}

fn probe_detail(k: &dmfs::FsKind) -> String {
    match k {
        dmfs::FsKind::Unknown => "（无法识别）".into(),
        dmfs::FsKind::Apfs => "可挂载为 APFS".into(),
        other => format!("可挂载为 {}", other.as_str()),
    }
}

pub fn info(o: &Opts) -> Result<(), String> {
    let source = o.positional.first().ok_or("info 需要指定源")?;
    let opened = open_fs(source, o.partition, o.offset, o.size, o.volume)?;
    println!("源      : {}", opened.desc);
    if let Some(p) = &opened.partition {
        println!("分区    : #{:<2} {:<26} {}", p.index, p.type_name, human_size(p.size));
        println!("偏移    : {} 字节", p.offset);
        if let Some(u) = &p.uuid {
            println!("分区 UUID: {u}");
        }
    }
    println!("只读    : {}", if opened.read_only { "是（安全模式）" } else { "否" });
    println!();
    fmt::print_fs_info(opened.fs.info());
    Ok(())
}

fn truncate(s: &str, n: usize) -> String {
    let chars: Vec<char> = s.chars().collect();
    if chars.len() <= n {
        s.to_string()
    } else {
        let mut t: String = chars[..n.saturating_sub(1)].iter().collect();
        t.push('…');
        t
    }
}
