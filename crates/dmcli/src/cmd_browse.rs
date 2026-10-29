//! 浏览类命令：ls / tree / cat / extract

use crate::args::Opts;
use crate::fmt;
use crate::source::open_fs;
use dmfs::{resolve, sanitize_name, Entry, FileSystem};
use std::fs;
use std::io::Write;
use std::path::{Path, PathBuf};

pub fn ls(o: &Opts) -> Result<(), String> {
    let source = o.positional.first().ok_or("ls 需要指定源")?;
    let path = o.positional.get(1).map(|s| s.as_str()).unwrap_or("/");
    let opened = open_fs(source, o.partition, o.offset, o.size, o.volume)?;
    let fs = &opened.fs;

    let mut entries = if path == "/" || path.is_empty() {
        fs.list(fs.root()).map_err(|e| e.to_string())?
    } else {
        let target = resolve(fs, path)
            .map_err(|e| e.to_string())?
            .ok_or_else(|| format!("路径不存在: {path}"))?;
        if target.is_dir() {
            fs.list(target.id).map_err(|e| e.to_string())?
        } else {
            vec![target]
        }
    };
    if o.deleted {
        entries.retain(|e| e.deleted);
    } else {
        // 默认也显示删除项，但排在后面
        entries.sort_by_key(|e| (e.deleted, e.name.to_lowercase()));
    }
    fmt::print_entries(&entries, o.long);
    if o.verbose {
        println!();
        fmt::print_entry_detail(&entries);
    }
    Ok(())
}

pub fn tree(o: &Opts) -> Result<(), String> {
    let source = o.positional.first().ok_or("tree 需要指定源")?;
    let path = o.positional.get(1).map(|s| s.as_str()).unwrap_or("/");
    let opened = open_fs(source, o.partition, o.offset, o.size, o.volume)?;
    let fs = &opened.fs;
    let root = if path == "/" || path.is_empty() {
        fs.root()
    } else {
        resolve(fs, path)
            .map_err(|e| e.to_string())?
            .ok_or_else(|| format!("路径不存在: {path}"))?
            .id
    };

    let mut dirs = 0u64;
    let mut files = 0u64;
    let mut bytes = 0u64;
    walk(fs, root, path, 0, o.depth, &mut dirs, &mut files, &mut bytes);
    println!();
    println!("{} 个目录, {} 个文件, 合计 {}", dirs, files, fmt::size_str(bytes));
    Ok(())
}

fn walk(
    fs: &impl FileSystem,
    dir: dmfs::NodeId,
    display: &str,
    depth: usize,
    max_depth: usize,
    dirs: &mut u64,
    files: &mut u64,
    bytes: &mut u64,
) {
    if depth > max_depth {
        return;
    }
    let entries = match fs.list(dir) {
        Ok(e) => e,
        Err(e) => {
            println!("{}└── <读取失败: {e}>", "    ".repeat(depth));
            return;
        }
    };
    let mut visible: Vec<&Entry> = entries.iter().filter(|e| !e.deleted).collect();
    visible.sort_by(|a, b| (a.is_dir(), a.name.to_lowercase()).cmp(&(b.is_dir(), b.name.to_lowercase())));
    let n = visible.len();
    for (i, e) in visible.iter().enumerate() {
        let last = i + 1 == n;
        let branch = if last { "└── " } else { "├── " };
        println!(
            "{}{}{}{}{}",
            "    ".repeat(depth),
            branch,
            e.name,
            if e.is_dir() { "/" } else { "" },
            if e.is_dir() {
                String::new()
            } else {
                format!("  ({})", fmt::size_str(e.size))
            }
        );
        if e.is_dir() {
            *dirs += 1;
            walk(fs, e.id, display, depth + 1, max_depth, dirs, files, bytes);
        } else {
            *files += 1;
            *bytes += e.size;
        }
    }
}

pub fn cat(o: &Opts) -> Result<(), String> {
    let source = o.positional.first().ok_or("cat 需要指定源")?;
    let path = o.positional.get(1).ok_or("cat 需要指定文件路径")?;
    let opened = open_fs(source, o.partition, o.offset, o.size, o.volume)?;
    let fs = &opened.fs;
    let entry = resolve(fs, path)
        .map_err(|e| e.to_string())?
        .ok_or_else(|| format!("路径不存在: {path}"))?;
    if entry.is_dir() {
        return Err(format!("{path} 是目录"));
    }
    match &o.out {
        Some(out) => {
            let mut f = fs::File::create(out).map_err(|e| format!("创建 {out} 失败: {e}"))?;
            let n = fs.read(&entry, &mut f).map_err(|e| e.to_string())?;
            eprintln!("已写出 {} 字节到 {out}", n);
        }
        None => {
            let stdout = std::io::stdout();
            let mut lock = stdout.lock();
            let n = fs.read(&entry, &mut lock).map_err(|e| e.to_string())?;
            lock.flush().ok();
            eprintln!("（共 {} 字节）", n);
        }
    }
    Ok(())
}

pub fn extract(o: &Opts) -> Result<(), String> {
    let source = o.positional.first().ok_or("extract 需要指定源")?;
    let path = o.positional.get(1).map(|s| s.as_str()).unwrap_or("/");
    let outdir = o.out.as_ref().ok_or("extract 必须用 -o 指定输出目录")?;
    let opened = open_fs(source, o.partition, o.offset, o.size, o.volume)?;
    let fs = &opened.fs;

    let entry = if path == "/" || path.is_empty() {
        None
    } else {
        Some(resolve(fs, path).map_err(|e| e.to_string())?.ok_or_else(|| format!("路径不存在: {path}"))?)
    };

    let root_out = PathBuf::from(outdir);
    fs::create_dir_all(&root_out).map_err(|e| format!("创建 {outdir} 失败: {e}"))?;

    let mut stats = Stats::default();
    match entry {
        None => {
            // 整个卷
            let entries = fs.list(fs.root()).map_err(|e| e.to_string())?;
            for e in entries {
                export(fs, &e, &root_out, o.deleted, &mut stats);
            }
        }
        Some(e) => {
            if e.is_dir() {
                export(fs, &e, &root_out, o.deleted, &mut stats);
            } else {
                copy_file(fs, &e, &root_out, &mut stats);
            }
        }
    }
    println!(
        "完成：{} 个文件, {} 个目录, {} 失败, 共 {}",
        stats.files,
        stats.dirs,
        stats.failed,
        fmt::size_str(stats.bytes)
    );
    Ok(())
}

#[derive(Default)]
struct Stats {
    files: u64,
    dirs: u64,
    failed: u64,
    bytes: u64,
}

fn export(fs: &impl FileSystem, entry: &Entry, dir: &Path, include_deleted: bool, stats: &mut Stats) {
    if entry.deleted && !include_deleted {
        return;
    }
    if entry.is_dir() {
        let target = dir.join(sanitize_name(&entry.name));
        if let Err(e) = fs::create_dir_all(&target) {
            eprintln!("创建目录 {} 失败: {e}", target.display());
            stats.failed += 1;
            return;
        }
        stats.dirs += 1;
        match fs.list(entry.id) {
            Ok(children) => {
                for c in children {
                    export(fs, &c, &target, include_deleted, stats);
                }
            }
            Err(e) => {
                eprintln!("列目录 {} 失败: {e}", entry.name);
                stats.failed += 1;
            }
        }
    } else {
        copy_file(fs, entry, dir, stats);
    }
}

fn copy_file(fs: &impl FileSystem, entry: &Entry, dir: &Path, stats: &mut Stats) {
    let name = sanitize_name(&entry.name);
    let mut target = dir.join(&name);
    let mut n = 1;
    while target.exists() {
        target = dir.join(format!("{name}.{n}"));
        n += 1;
    }
    let file = match fs::File::create(&target) {
        Ok(f) => f,
        Err(e) => {
            eprintln!("创建 {} 失败: {e}", target.display());
            stats.failed += 1;
            return;
        }
    };
    let mut w = std::io::BufWriter::new(file);
    match fs.read(entry, &mut w) {
        Ok(n) => {
            w.flush().ok();
            stats.files += 1;
            stats.bytes += n;
            if n < entry.size {
                eprintln!(
                    "警告: {} 只读出 {} / {} 字节（块链可能已损坏）",
                    entry.name, n, entry.size
                );
            }
        }
        Err(e) => {
            eprintln!("读取 {} 失败: {e}", entry.name);
            stats.failed += 1;
            let _ = fs::remove_file(&target);
        }
    }
}
