//! dmde-rs 命令行入口。
//!
//! 设计原则：默认全只读、绝不写源设备；所有"写"只发生在 -o 指定的输出目录。

mod args;
mod cmd_browse;
mod cmd_disk;
mod cmd_recover;
mod fmt;
mod source;

use args::{Opts, HELP};

const VERSION: &str = env!("CARGO_PKG_VERSION");

fn main() {
    let argv: Vec<String> = std::env::args().skip(1).collect();
    if argv.is_empty() || argv[0] == "help" || argv[0] == "-h" || argv[0] == "--help" {
        print!("{HELP}");
        return;
    }
    if argv[0] == "-V" || argv[0] == "--version" || argv[0] == "version" {
        println!("dmde-rs {VERSION}");
        return;
    }
    let opts = match args::parse(argv) {
        Ok(o) => o,
        Err(e) => {
            eprintln!("参数错误: {e}");
            eprintln!("用 `dmde-rs help` 查看用法");
            std::process::exit(2);
        }
    };
    if opts.help {
        print!("{HELP}");
        return;
    }
    if opts.version {
        println!("dmde-rs {VERSION}");
        return;
    }
    if let Err(e) = dispatch(&opts) {
        eprintln!("{e}");
        std::process::exit(1);
    }
}

fn dispatch(o: &Opts) -> Result<(), String> {
    match o.cmd.as_str() {
        "devices" => cmd_disk::devices(o),
        "volumes" => cmd_disk::volumes(o),
        "info" => cmd_disk::info(o),
        "ls" => cmd_browse::ls(o),
        "tree" => cmd_browse::tree(o),
        "cat" => cmd_browse::cat(o),
        "extract" => cmd_browse::extract(o),
        "recover" => cmd_recover::recover(o),
        other => Err(format!("未知命令: {other}\n用 `dmde-rs help` 查看用法")),
    }
}
