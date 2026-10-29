//! 删除文件恢复命令。

use crate::args::Opts;
use crate::fmt;
use crate::source::open_fs;
use dmfs::{sanitize_name, FileSystem};
use std::fs;
use std::io::Write;
use std::path::Path;

pub fn recover(o: &Opts) -> Result<(), String> {
    let source = o.positional.first().ok_or("recover 需要指定源")?;
    let opened = open_fs(source, o.partition, o.offset, o.size, o.volume)?;
    let fs = &opened.fs;

    println!("源      : {}", opened.desc);
    println!("文件系统: {}", fs.info().kind.as_str());

    let hint = opened.fs.recovery_hint();
    if let Some(lines) = &hint {
        println!();
        for l in lines {
            println!("{l}");
        }
    }
    if !fs.supports_deleted() {
        if hint.is_none() {
            println!();
            println!(
                "{} 删除时会直接销毁目录记录，无法按目录项恢复。",
                fs.info().kind.as_str()
            );
            println!("这类卷需要「签名雕刻」(carving) 才能找回文件——该模块规划在 v0.2。");
            return Ok(());
        }
        // APFS：虽然没有目录项可救，仍扫一遍孤儿 inode（有就是赚到）
        println!();
        println!("再扫一遍孤儿 inode（全树扫描，卷越大越慢）…");
    }

    print!("正在扫描可恢复项 ... ");
    std::io::stdout().flush().ok();
    let mut items = fs.deleted().map_err(|e| e.to_string())?;
    println!("找到 {} 个", items.len());

    items.retain(|e| e.size >= o.min_size);
    if o.limit > 0 && items.len() > o.limit {
        items.truncate(o.limit);
    }
    if items.is_empty() {
        if hint.is_some() {
            println!(
                "没有孤儿 inode——APFS 上这属于正常：删除会把 drec 与 inode 一起回收。\n\
                 真正还能救的是快照和 .Trashes，见上面 ①②。"
            );
        } else {
            println!("没有符合条件的已删除文件。");
        }
        return Ok(());
    }

    if o.dry_run {
        println!();
        println!("{:<10} {:<19} 名称", "大小", "删除前修改时间");
        println!("{}", "-".repeat(72));
        let mut total = 0u64;
        for e in &items {
            total += e.size;
            println!(
                "{:>10} {:<19} {}",
                fmt::size_str(e.size),
                fmt::time_str(e.mtime),
                e.name
            );
        }
        println!();
        println!(
            "共 {} 个文件，{} 可恢复（dry-run，未写盘）",
            items.len(),
            fmt::size_str(total)
        );
        return Ok(());
    }

    let outdir = o.out.as_ref().ok_or("recover 必须用 -o 指定输出目录")?;
    fs::create_dir_all(outdir).map_err(|e| format!("创建 {outdir} 失败: {e}"))?;

    let mut ok = 0u64;
    let mut failed = 0u64;
    let mut bytes = 0u64;
    for (i, e) in items.iter().enumerate() {
        if (i + 1) % 100 == 0 || i + 1 == items.len() {
            eprint!("\r进度 {}/{}", i + 1, items.len());
            std::io::stdout().flush().ok();
        }
        match restore_one(fs, e, Path::new(outdir)) {
            Ok(n) => {
                ok += 1;
                bytes += n;
            }
            Err(err) => {
                failed += 1;
                eprintln!("\n恢复 {} 失败: {err}", e.name);
            }
        }
    }
    eprintln!();
    println!(
        "完成：成功 {} 个，失败 {} 个，共 {}",
        ok,
        failed,
        fmt::size_str(bytes)
    );
    if fs.info().kind.is_fat() || fs.info().kind == dmfs::FsKind::ExFat {
        println!("注意：FAT/exFAT 的簇链在删除时已被清空，恢复按「连续簇」推断；");
        println!("      若文件原本碎片化，请改用签名雕刻方案。");
    }
    Ok(())
}

fn restore_one(fs: &impl FileSystem, e: &dmfs::Entry, dir: &Path) -> Result<u64, String> {
    let name = sanitize_name(&e.name);
    let mut target = dir.join(&name);
    let mut n = 1;
    while target.exists() {
        target = dir.join(format!("{name}.{n}"));
        n += 1;
    }
    let file = fs::File::create(&target).map_err(|e| e.to_string())?;
    let mut w = std::io::BufWriter::new(file);
    let written = fs.read(e, &mut w).map_err(|err| err.to_string())?;
    w.flush().ok();
    if written == 0 {
        fs::remove_file(&target).ok();
        return Err("读不到数据（簇/块映射已丢失）".into());
    }
    Ok(written)
}
