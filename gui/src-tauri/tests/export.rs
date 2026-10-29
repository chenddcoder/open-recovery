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


/// 暂停：标志已在起点置位时，一个文件都不该写。
#[test]
fn export_stops_immediately_when_already_paused() {
    let Some(img) = image() else { return };
    let (s, _) = dmrpc::open_local(img, None).expect("打开镜像");
    let mut b = Backend::Local(s);

    let out = std::env::temp_dir().join(format!("dmde-pause-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&out);
    std::fs::create_dir_all(&out).unwrap();

    let cancel = std::sync::atomic::AtomicBool::new(true);
    let msg = dmde_gui_lib::commands::do_export_with(
        &mut b,
        "/sub",
        out.to_str().unwrap(),
        &cancel,
        dmde_gui_lib::commands::Conflict::Skip,
        &dmde_gui_lib::skip::SkipRules::defaults(),
        &mut |_, _| {},
    )
    .expect("暂停不是错误，应返回 Ok");

    assert!(msg.starts_with("已暂停"), "应报告已暂停，实际: {msg}");
    let copied: Vec<_> = std::fs::read_dir(out.join("sub"))
        .map(|it| it.flatten().collect())
        .unwrap_or_default();
    assert!(copied.is_empty(), "暂停后不应写出任何文件：{:?}", copied);

    let _ = std::fs::remove_dir_all(&out);
}

/// 暂停后仍能正常再导一次：标志必须在每次开始前被清掉。
#[test]
fn export_works_again_after_pause() {
    let Some(img) = image() else { return };
    let out = std::env::temp_dir().join(format!("dmde-resume-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&out);
    std::fs::create_dir_all(&out).unwrap();

    // 第一次：立刻暂停
    {
        let (s, _) = dmrpc::open_local(img, None).expect("打开镜像");
        let mut b = Backend::Local(s);
        let cancel = std::sync::atomic::AtomicBool::new(true);
        dmde_gui_lib::commands::do_export_with(
            &mut b,
            "/sub",
            out.to_str().unwrap(),
            &cancel,
            dmde_gui_lib::commands::Conflict::Skip,
            &dmde_gui_lib::skip::SkipRules::defaults(),
            &mut |_, _| {},
        )
        .unwrap();
    }

    // 第二次：重新打开、标志复位，应完整导出
    {
        let (s, _) = dmrpc::open_local(img, None).expect("打开镜像");
        let mut b = Backend::Local(s);
        let cancel = std::sync::atomic::AtomicBool::new(false);
        let msg = dmde_gui_lib::commands::do_export_with(
            &mut b,
            "/sub",
            out.to_str().unwrap(),
            &cancel,
            dmde_gui_lib::commands::Conflict::Skip,
            &dmde_gui_lib::skip::SkipRules::defaults(),
            &mut |_, _| {},
        )
        .unwrap();
        assert!(msg.starts_with("已导出"), "应正常导出，实际: {msg}");
    }

    assert!(out.join("sub/deeper/n.txt").exists(), "恢复后应写出完整目录");
    let _ = std::fs::remove_dir_all(&out);
}


// ---------------------------------------------------------------------------
// 同名冲突：跳过 / 重命名 / 覆盖
// ---------------------------------------------------------------------------

use dmde_gui_lib::commands::Conflict;

/// 导出一次 `/sub`，把里面的 n.txt 内容改成"用户后来改过的"，
/// 模拟"这个目录之前已经导过、而且用户动过了"。
fn prepare_existing(out: &std::path::Path) {
    let (s, _) = dmrpc::open_local(image().unwrap(), None).expect("打开镜像");
    let mut b = Backend::Local(s);
    dmde_gui_lib::commands::do_export_with(
        &mut b,
        "/sub",
        out.to_str().unwrap(),
        &std::sync::atomic::AtomicBool::new(false),
        Conflict::Overwrite,
        &dmde_gui_lib::skip::SkipRules::defaults(),
        &mut |_, _| {},
    )
    .expect("首次导出");
    std::fs::write(out.join("sub/deeper/n.txt"), "user edited\n").unwrap();
}

#[test]
fn skip_keeps_existing_files() {
    let Some(_) = image() else { return };
    let out = std::env::temp_dir().join(format!("dmde-skip-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&out);
    prepare_existing(&out);

    let (s, _) = dmrpc::open_local(image().unwrap(), None).expect("打开镜像");
    let mut b = Backend::Local(s);
    let msg = dmde_gui_lib::commands::do_export_with(
        &mut b,
        "/sub",
        out.to_str().unwrap(),
        &std::sync::atomic::AtomicBool::new(false),
        Conflict::Skip,
        &dmde_gui_lib::skip::SkipRules::defaults(),
        &mut |_, _| {},
    )
    .unwrap();

    assert!(msg.contains("跳过"), "应报告跳过的数量，实际: {msg}");
    assert_eq!(
        std::fs::read_to_string(out.join("sub/deeper/n.txt")).unwrap(),
        "user edited\n",
        "跳过策略绝不能覆盖用户已有的文件"
    );

    let _ = std::fs::remove_dir_all(&out);
}

#[test]
fn rename_creates_numbered_copy_and_keeps_original() {
    let Some(_) = image() else { return };
    let out = std::env::temp_dir().join(format!("dmde-rename-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&out);
    prepare_existing(&out);

    let (s, _) = dmrpc::open_local(image().unwrap(), None).expect("打开镜像");
    let mut b = Backend::Local(s);
    dmde_gui_lib::commands::do_export_with(
        &mut b,
        "/sub",
        out.to_str().unwrap(),
        &std::sync::atomic::AtomicBool::new(false),
        Conflict::Rename,
        &dmde_gui_lib::skip::SkipRules::defaults(),
        &mut |_, _| {},
    )
    .unwrap();

    // 原文件保持用户改过的内容；新文件带序号且是源内容
    assert_eq!(
        std::fs::read_to_string(out.join("sub/deeper/n.txt")).unwrap(),
        "user edited\n"
    );
    let renamed = out.join("sub/deeper/n (1).txt");
    assert!(renamed.exists(), "应生成带序号的副本：{}", renamed.display());
    assert_eq!(std::fs::read_to_string(&renamed).unwrap(), "nested\n");

    let _ = std::fs::remove_dir_all(&out);
}

#[test]
fn overwrite_replaces_existing_content() {
    let Some(_) = image() else { return };
    let out = std::env::temp_dir().join(format!("dmde-over-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&out);
    prepare_existing(&out);

    let (s, _) = dmrpc::open_local(image().unwrap(), None).expect("打开镜像");
    let mut b = Backend::Local(s);
    dmde_gui_lib::commands::do_export_with(
        &mut b,
        "/sub",
        out.to_str().unwrap(),
        &std::sync::atomic::AtomicBool::new(false),
        Conflict::Overwrite,
        &dmde_gui_lib::skip::SkipRules::defaults(),
        &mut |_, _| {},
    )
    .unwrap();

    assert_eq!(
        std::fs::read_to_string(out.join("sub/deeper/n.txt")).unwrap(),
        "nested\n",
        "覆盖策略应写回源内容"
    );
    assert!(
        !out.join("sub/deeper/n (1).txt").exists(),
        "覆盖不应产生副本"
    );

    let _ = std::fs::remove_dir_all(&out);
}

/// 跳过规则必须在**读盘之前**生效：被跳过的目录连建都不建、内容一个字节都不读。
///
/// 这里用测试镜像里的 `/sub` 当"假 node_modules"：规则命中它，
/// 导出根目录后 `sub` 应当整体消失，且汇总里明确报告跳过数量。
#[test]
fn export_skips_dirs_matching_rules() {
    let Some(img) = image() else {
        eprintln!("跳过：测试镜像不存在");
        return;
    };
    let (s, _) = dmrpc::open_local(img, None).expect("打开镜像");
    let mut b = Backend::Local(s);

    // 目录名必须和已有的 skip 测试区分开：同一进程里测试是并行跑的，
    // 用同一个临时目录会互相删掉对方的结果
    let out = std::env::temp_dir().join(format!("dmde-skiprules-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&out);

    let rules = dmde_gui_lib::skip::SkipRules::parse("sub");
    let msg = dmde_gui_lib::commands::do_export_with(
        &mut b,
        "/",
        out.to_str().unwrap(),
        &std::sync::atomic::AtomicBool::new(false),
        Conflict::Skip,
        &rules,
        &mut |_, _| {},
    )
    .unwrap();

    assert!(
        !out.join("root/sub").exists(),
        "命中规则的目录不该出现在输出里：{}",
        out.join("root/sub").display()
    );
    assert!(out.join("root/readme.txt").exists(), "没命中的条目要正常导出");
    assert!(
        msg.contains("按规则跳过"),
        "汇总里要说清按规则跳过了多少，实际: {msg}"
    );

    let _ = std::fs::remove_dir_all(&out);
}

/// 单文件导出（按会话 id）：界面上选中一个文件后点「导出」走的就是这条。
///
/// 与按路径导出的关键差别：这条不依赖路径能解析，所以勾了「显示已删除」
/// 之后选中的条目（目录里已经查不到）也能导出。
#[test]
fn export_single_file_by_id() {
    use dmde_gui_lib::commands::{do_export_entry, Conflict};
    use std::sync::atomic::AtomicBool;

    let Some(img) = image() else {
        eprintln!("跳过：测试镜像不存在");
        return;
    };
    let (s, _) = dmrpc::open_local(img, None).expect("打开镜像");
    let mut b = Backend::Local(s);

    let items = b.list_dir("/", false).expect("列根目录");
    let e = items
        .iter()
        .find(|e| e.name == "readme.txt")
        .expect("镜像根目录里应有 readme.txt");
    assert!(e.id > 0, "list_dir 必须带回会话 id，否则按 id 导出无从谈起");

    let out = std::env::temp_dir().join(format!("dmde-export-id-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&out);

    let (st, dest) = do_export_entry(
        &mut b,
        e.id,
        &e.name,
        out.to_str().unwrap(),
        &AtomicBool::new(false),
        Conflict::Skip,
        &mut |_, _| {},
    )
    .expect("按 id 导出应成功");

    assert_eq!(st.files, 1, "只该导这一个文件");
    assert_eq!(st.bytes, "apfs hello\n".len() as u64);
    assert_eq!(dest.file_name().unwrap(), "readme.txt");
    assert_eq!(std::fs::read_to_string(&dest).unwrap(), "apfs hello\n");

    let _ = std::fs::remove_dir_all(&out);
}

/// 目标已存在 + 跳过策略：不能覆盖，也不能报成失败。
#[test]
fn export_single_file_respects_conflict() {
    use dmde_gui_lib::commands::{do_export_entry, Conflict};
    use std::sync::atomic::AtomicBool;

    let Some(img) = image() else {
        return;
    };
    let (s, _) = dmrpc::open_local(img, None).expect("打开镜像");
    let mut b = Backend::Local(s);
    let items = b.list_dir("/", false).expect("列根目录");
    let e = items.iter().find(|e| e.name == "readme.txt").unwrap();

    let out = std::env::temp_dir().join(format!("dmde-export-id-cf-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&out);
    std::fs::create_dir_all(&out).unwrap();
    std::fs::write(out.join("readme.txt"), "原有的内容，别动").unwrap();

    let (st, _) = do_export_entry(
        &mut b,
        e.id,
        &e.name,
        out.to_str().unwrap(),
        &AtomicBool::new(false),
        Conflict::Skip,
        &mut |_, _| {},
    )
    .expect("跳过不是错误");

    assert_eq!(st.skipped, 1);
    assert_eq!(st.files, 0);
    assert_eq!(
        std::fs::read_to_string(out.join("readme.txt")).unwrap(),
        "原有的内容，别动",
        "Skip 策略绝不能动目标目录里的旧文件"
    );

    let _ = std::fs::remove_dir_all(&out);
}

/// 多选导出：文件与目录可以混在一起，一次导出。
/// 这是界面上「导出选中的 N 项」的底层实现——通常用来只捞几台虚拟机目录，
/// 而不是整个父目录。
#[test]
fn export_selected_items_mixes_files_and_dirs() {
    use dmde_gui_lib::commands::{do_export_items, Conflict, ExportItem};
    use dmde_gui_lib::skip::SkipRules;
    use std::sync::atomic::AtomicBool;

    let Some(img) = image() else {
        eprintln!("跳过：测试镜像不存在");
        return;
    };
    let (s, _) = dmrpc::open_local(img, None).expect("打开镜像");
    let mut b = Backend::Local(s);
    let all = b.list_dir("/", false).expect("列根目录");
    let file = all.iter().find(|e| e.name == "readme.txt").expect("应有 readme.txt");
    let dir = all.iter().find(|e| e.name == "sub").expect("应有 sub 目录");

    let out = std::env::temp_dir().join(format!("dmde-export-items-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&out);

    let items = vec![
        ExportItem {
            id: file.id,
            path: file.path.clone(),
            name: file.name.clone(),
            is_dir: false,
        },
        ExportItem {
            id: dir.id,
            path: dir.path.clone(),
            name: dir.name.clone(),
            is_dir: true,
        },
    ];
    let msg = do_export_items(
        &mut b,
        &items,
        out.to_str().unwrap(),
        &AtomicBool::new(false),
        Conflict::Skip,
        &SkipRules::parse(""),
        &mut |_, _| {},
    )
    .expect("混合导出应成功");
    eprintln!("{msg}");

    assert_eq!(
        std::fs::read_to_string(out.join("readme.txt")).unwrap(),
        "apfs hello\n",
        "选中的文件要原样导出"
    );
    assert!(
        out.join("sub/deeper/n.txt").exists(),
        "选中的目录要保留子树结构"
    );
    // readme.txt + sub/keep.bin + sub/deeper/n.txt
    assert!(msg.contains("3 个文件"), "汇总应统计到 3 个文件: {msg}");

    let _ = std::fs::remove_dir_all(&out);
}
