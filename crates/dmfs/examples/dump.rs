//! 调试用：把某个卷的目录树按原始字段打印出来。
//! 用法：cargo run -p dmfs --example dump -- <镜像> [分区号] [路径]

use dmfs::{resolve, FileSystem};
use std::sync::Arc;

fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();
    if args.is_empty() {
        eprintln!("用法: dump <镜像> [分区号] [路径]");
        return;
    }
    let path = &args[0];
    let part: usize = args.get(1).and_then(|s| s.parse().ok()).unwrap_or(0);
    let sub = args.get(2).cloned().unwrap_or_else(|| "/".to_string());

    let dev = dmdisk::open_source(path, 0).expect("打开源失败");
    let src = dmdisk::Source {
        reader: dmdisk::Reader::new(Arc::clone(&dev)),
        label: path.clone(),
        size: dev.size(),
        sector_size: dev.sector_size(),
        read_only: true,
    };
    let table = src.table().expect("分区表解析失败");
    eprintln!("分区表: {:?} 共 {} 项", table.scheme, table.partitions.len());
    for p in &table.partitions {
        eprintln!(
            "  #{:<2} {:>12} {:>12} {}",
            p.index, p.offset, p.size, p.type_name
        );
    }
    let reader = if part == 0 {
        if dmfs::probe(&src.reader) == dmfs::FsKind::Unknown {
            let p = &table.partitions[0];
            src.volume(p.offset, p.size)
        } else {
            src.reader.slice(0, src.size)
        }
    } else {
        let p = &table.partitions[part - 1];
        src.volume(p.offset, p.size)
    };

    let fs = dmfs::AnyFs::open(reader).expect("挂载失败");
    let info = fs.info();
    println!("== {} label={:?} block={} total={}", info.kind.as_str(), info.label, info.block_size, info.total_bytes);
    for (k, v) in &info.details {
        println!("   {k}: {v}");
    }

    let dir = if sub == "/" {
        fs.root()
    } else {
        match resolve(&fs, &sub) {
            Ok(Some(e)) => e.id,
            _ => {
                eprintln!("路径不存在: {sub}");
                return;
            }
        }
    };
    println!("-- 目录 id={} --", dir.0);
    match fs.list(dir) {
        Ok(items) => {
            for e in items {
                println!(
                    "  id={:<8} parent={:<6} dir={:<5} size={:<10} del={:<5} start={:<10} {:?}",
                    e.id.0, e.parent.0, e.is_dir(), e.size, e.deleted, e.start, e.name
                );
            }
        }
        Err(e) => println!("列目录失败: {e}"),
    }
    match fs.deleted() {
        Ok(d) => {
            println!("-- 可恢复 {} 项 --", d.len());
            for e in d.iter().take(20) {
                println!("  {:>10}  {:?}", e.size, e.name);
            }
        }
        Err(e) => println!("扫描失败: {e}"),
    }
}
