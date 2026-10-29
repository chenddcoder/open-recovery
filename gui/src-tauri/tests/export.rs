//! 导出链路端到端测试：真实 APFS 镜像 → 递归导出 → 校验内容与目录结构。
//!
//! 这段逻辑是界面上"导出当前目录"的唯一实现，必须保证第一次点击就能用。

use dmde_gui_lib::commands::do_export;
use dmrpc::Backend;

fn image() -> Option<&'static str> {
    let p = "/tmp/dmde-test/apfs-broken0.dmg";
    if std::path::Path::new(p).exists() {
        Some(p)
    } else {
        None
    }
}

#[test]
fn export_directory_recursively() {
    let Some(img) = image() else {
        eprintln!("跳过：测试镜像不存在");
        return;
    };
    let (s, dto) = dmrpc::open_local(img, None).expect("打开镜像");
    assert_eq!(dto.fs_kind, "APFS");
    let mut b = Backend::Local(s);

    let out = std::env::temp_dir().join(format!("dmde-export-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&out);

    let msg = do_export(&mut b, "/sub", out.to_str().unwrap()).expect("导出应成功");
    eprintln!("{msg}");

    let n = out.join("sub/deeper/n.txt");
    assert!(n.exists(), "应保留子目录结构：{}", n.display());
    assert_eq!(std::fs::read_to_string(&n).unwrap(), "nested\n");
    assert!(out.join("sub/keep.bin").exists());

    let _ = std::fs::remove_dir_all(&out);
}

#[test]
fn export_single_file_by_path() {
    let Some(img) = image() else {
        return;
    };
    let (s, _) = dmrpc::open_local(img, None).expect("打开镜像");
    let mut b = Backend::Local(s);

    let out = std::env::temp_dir().join(format!("dmde-export-file-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&out);

    do_export(&mut b, "/readme.txt", out.to_str().unwrap()).expect("单文件导出应成功");
    let f = out.join("readme.txt");
    assert!(f.exists(), "{}", f.display());
    assert_eq!(std::fs::read_to_string(&f).unwrap(), "apfs hello\n");

    let _ = std::fs::remove_dir_all(&out);
}
