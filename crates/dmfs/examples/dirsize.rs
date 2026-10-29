//! 诊断：目录统计（dir_stats）与记录类型分布。
//!
//! ```text
//! dirsize <镜像/设备> [偏移]            查指定路径的目录统计
//! HIST=1 dirsize <镜像/设备> [偏移]     全树扫描，输出每种记录的条数
//! ```
use dmfs::{AnyFs, FileSystem};

fn main() {
    let mut a = std::env::args().skip(1);
    let path = a.next().expect("用法: dirsize <镜像> [偏移]");
    let off: u64 = a.next().map(|v| v.parse().unwrap()).unwrap_or(0);
    let src = dmdisk::Source::open(&path, 0).unwrap();
    let reader = src.volume(off, src.size.saturating_sub(off));
    let fs = AnyFs::open(reader).unwrap();
    println!("卷: {}  {}", fs.info().kind.as_str(), fs.info().label);

    if std::env::var("HIST").is_ok() {
        let root = fs.root();
        println!("根目录 id = {}", root.0);
        match fs.dir_stats(root) {
            Some((n, b)) => println!("根 dir_stats: 子项 {n} 大小 {b}"),
            None => println!("根 dir_stats: 无"),
        }
        match fs.type_histogram() {
            Some(Ok(h)) => {
                println!("记录类型分布:");
                for (t, c) in h {
                    println!("  type {t:2}  {c:>10} 条");
                }
            }
            Some(Err(e)) => println!("扫描失败: {e}"),
            None => println!("不是 APFS"),
        }
        return;
    }

    let targets: Vec<String> = if a.len() > 0 { a.collect() } else { vec!["/".into()] };
    for p in targets {
        match dmfs::resolve(&fs, &p) {
            Ok(Some(e)) => match fs.dir_stats(e.id) {
                Some((n, b)) => println!("{p:20} id={:<6} 子项 {n:>8}  总大小 {b:>14} 字节", e.id.0),
                None => println!("{p:20} id={:<6} 无 dir_stats 记录", e.id.0),
            },
            Ok(None) => println!("{p:20} 不存在"),
            Err(e) => println!("{p:20} 错误 {e}"),
        }
    }
}
