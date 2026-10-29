//! 目录大小统计：真实 APFS 镜像上的累加结果 + 参数校验。

use dmrpc::open_local;

fn image() -> Option<&'static str> {
    let p = "/tmp/dmde-test/apfs-broken0.dmg";
    if std::path::Path::new(p).exists() {
        Some(p)
    } else {
        None
    }
}

#[test]
fn dir_size_sums_subtree() {
    let Some(img) = image() else {
        eprintln!("跳过：测试镜像不存在");
        return;
    };
    let (s, _) = open_local(img, None).expect("打开镜像");
    let r = s.dir_size("/sub", 5000).expect("统计 /sub");

    // /sub 下有 keep.bin(9000) 与 deeper/n.txt(7)，至少 9007
    assert!(r.bytes >= 9007, "总字节数偏小: {} ({})", r.bytes, r.bytes_human);
    assert_eq!(r.files, 2, "文件数应为 2，实际 {}", r.files);
    assert_eq!(r.dirs, 1, "目录数应为 1（deeper），实际 {}", r.dirs);
    assert!(r.exact, "小目录应完整统计");
    assert_eq!(r.source, "scan");
}

#[test]
fn dir_size_of_file_is_its_size() {
    let Some(img) = image() else { return };
    let (s, _) = open_local(img, None).expect("打开镜像");
    let r = s.dir_size("/readme.txt", 5000).expect("统计文件");
    assert_eq!(r.source, "file");
    assert_eq!(r.files, 1);
    assert_eq!(r.dirs, 0);
    assert_eq!(r.bytes, "apfs hello\n".len() as u64);
}

#[test]
fn dir_size_reports_missing_path() {
    let Some(img) = image() else { return };
    let (s, _) = open_local(img, None).expect("打开镜像");
    let e = s.dir_size("/no-such-dir", 1000).unwrap_err();
    assert!(e.contains("不存在"), "应明确说不存在，实际: {e}");
}

/// 时间预算必须真的生效：给 1ms 也不该 panic，且要么精确要么明确标不精确。
#[test]
fn dir_size_respects_tiny_budget() {
    let Some(img) = image() else { return };
    let (s, _) = open_local(img, None).expect("打开镜像");
    let r = s.dir_size("/", 1).expect("不应报错");
    assert!(!r.exact || r.bytes > 0, "极小预算下要么部分统计，要么已统计到内容");
}
