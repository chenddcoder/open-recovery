//! 输出格式化。

use dmdisk::human_size;
use dmfs::{util::fmt_unix, Entry, FsInfo, NodeKind};

pub fn size_str(n: u64) -> String {
    human_size(n)
}

pub fn time_str(ts: Option<i64>) -> String {
    fmt_unix(ts)
}

fn kind_char(k: NodeKind) -> char {
    k.as_char()
}

/// 列目录的表格输出
pub fn print_entries(entries: &[Entry], long: bool) {
    if entries.is_empty() {
        println!("（空目录）");
        return;
    }
    if long {
        println!(
            "{:<3} {:>10}  {:<19}  {:<10} 名称",
            "T", "大小", "修改时间", "起始"
        );
        println!("{}", "-".repeat(72));
        for e in entries {
            println!(
                "{:<3} {:>10}  {:<19}  {:<10} {}{}{}",
                kind_char(e.kind),
                size_str(e.size),
                time_str(e.mtime),
                if e.start == 0 { "-".to_string() } else { e.start.to_string() },
                e.name,
                if e.is_dir() { "/" } else { "" },
                if e.deleted { "  [已删除]" } else { "" }
            );
        }
        println!("共 {} 项", entries.len());
    } else {
        for e in entries {
            print!("{}{}", e.name, if e.is_dir() { "/" } else { "" });
            if e.deleted {
                print!(" [已删除]");
            }
            println!();
        }
    }
}

/// 详细模式：额外打印 meta
pub fn print_entry_detail(entries: &[Entry]) {
    for e in entries {
        println!("{}{}", e.name, if e.is_dir() { "/" } else { "" });
        println!("  类型: {}  大小: {}", e.kind.as_str(), size_str(e.size));
        println!("  修改时间: {}", time_str(e.mtime));
        println!("  标识: {}  起始: {}", e.id.0, e.start);
        if e.deleted {
            println!("  状态: 已删除");
        }
        for (k, v) in &e.meta {
            println!("  {k}: {v}");
        }
        println!();
    }
}

pub fn print_fs_info(info: &FsInfo) {
    println!("文件系统: {}", info.kind.as_str());
    if !info.label.is_empty() {
        println!("卷标    : {}", info.label);
    }
    if let Some(u) = &info.uuid {
        println!("UUID    : {u}");
    }
    println!("块大小  : {} 字节", info.block_size);
    println!("总容量  : {}", size_str(info.total_bytes));
    match info.free_bytes {
        Some(f) => println!("剩余空间: {} ({:.1}%)", size_str(f), f as f64 / info.total_bytes.max(1) as f64 * 100.0),
        None => println!("剩余空间: 未知"),
    }
    if !info.details.is_empty() {
        println!("--- 细节 ---");
        for (k, v) in &info.details {
            println!("{:<14}: {}", k, v);
        }
    }
}
