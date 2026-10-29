//! 前端可调用的命令。全部围绕"当前会话"这一个状态。
//!
//! 会话有两种后端：
//! * `Local`  —— 镜像文件 / 已有权限的设备，本进程直接读；
//! * `Remote` —— `/dev/rdisk*`，经系统授权弹窗拉起的只读 helper 读。
//! 前端不用关心是哪一种，命令签名完全一样。

use crate::{priv_helper, AppState};
use dmrpc::{open_local, Backend, EntryDto, PreviewDto, RecoverDto, SourceDto};
use dmfs::sanitize_name;
use std::fs;
use std::path::{Path, PathBuf};

fn not_open() -> String {
    "还没有打开任何卷".to_string()
}

/// 列出本机物理磁盘（`diskutil` 查询，不需要 root，也不会碰设备）。
#[tauri::command]
pub fn list_disks() -> Result<Vec<dmrpc::DiskDto>, String> {
    let disks = dmdisk::list_disks().map_err(|e| e.to_string())?;
    Ok(disks
        .into_iter()
        .map(|d| dmrpc::DiskDto {
            dev_node: d.dev_node,
            bsd: d.bsd,
            size: d.size,
            size_human: dmdisk::human_size(d.size),
            model: d.model,
            bus: d.bus,
            internal: d.internal,
            removable: d.removable,
        })
        .collect())
}

/// 有权限就直接开；只有"权限不足"才值得弹授权窗——其它错误（路径不存在、
/// 格式不认识）弹密码窗纯属骚扰用户。
#[tauri::command]
pub fn open_source(
    state: tauri::State<AppState>,
    path: String,
    partition: Option<usize>,
) -> Result<SourceDto, String> {
    let mut guard = state.backend.lock().map_err(|_| not_open())?;
    match open_local(&path, partition) {
        Ok((s, dto)) => {
            *guard = Some(Backend::Local(s));
            Ok(dto)
        }
        Err(e) if e.permission => {
            let mut client = priv_helper::connect()?;
            let dto = client.open(&path, partition)?;
            *guard = Some(Backend::Remote(client));
            Ok(dto)
        }
        Err(e) => Err(e.message),
    }
}

#[tauri::command]
pub fn list_dir(
    state: tauri::State<AppState>,
    path: String,
    show_deleted: bool,
) -> Result<Vec<EntryDto>, String> {
    let mut guard = state.backend.lock().map_err(|_| not_open())?;
    let b = guard.as_mut().ok_or_else(not_open)?;
    b.list_dir(&path, show_deleted)
}

#[tauri::command]
pub fn preview(
    state: tauri::State<AppState>,
    id: u64,
    path: String,
    max: Option<usize>,
) -> Result<PreviewDto, String> {
    let limit = max.unwrap_or(256 * 1024);
    let mut guard = state.backend.lock().map_err(|_| not_open())?;
    let b = guard.as_mut().ok_or_else(not_open)?;
    if id > 0 {
        return b.preview_id(id, limit);
    }
    b.preview(&path, limit)
}

#[tauri::command]
pub fn scan_deleted(
    state: tauri::State<AppState>,
    limit: Option<usize>,
) -> Result<Vec<EntryDto>, String> {
    let mut guard = state.backend.lock().map_err(|_| not_open())?;
    let b = guard.as_mut().ok_or_else(not_open)?;
    b.deleted(limit.unwrap_or(0))
}

fn unique_path(dir: &Path, name: &str) -> PathBuf {
    let mut target = dir.join(name);
    let mut n = 1;
    while target.exists() {
        target = dir.join(format!("{name}.{n}"));
        n += 1;
    }
    target
}

/// 读一个条目并写到本地。写盘由 GUI 自己完成（非 root），属主、TCC、
/// 目录权限都天然正确；helper 只负责把字节吐回来。
fn dump_to_dir(
    b: &mut Backend,
    id: u64,
    name: &str,
    out_dir: &str,
) -> Result<(PathBuf, u64), String> {
    fs::create_dir_all(out_dir).map_err(|e| format!("创建 {out_dir} 失败: {e}"))?;
    let target = unique_path(Path::new(out_dir), &sanitize_name(name));
    let file = fs::File::create(&target).map_err(|e| format!("创建 {} 失败: {e}", target.display()))?;
    let mut w = std::io::BufWriter::new(file);
    match b.read_id(id, &mut w) {
        Ok(n) if n > 0 => {
            use std::io::Write;
            w.flush().ok();
            Ok((target, n))
        }
        Ok(_) => {
            let _ = fs::remove_file(&target);
            Err("读到 0 字节".into())
        }
        Err(e) => {
            // 半成品文件必须删掉，否则恢复目录里混着截断文件最坑人
            let _ = fs::remove_file(&target);
            Err(e)
        }
    }
}

#[tauri::command]
pub fn save_file(
    state: tauri::State<AppState>,
    id: u64,
    name: String,
    out_dir: String,
) -> Result<String, String> {
    let mut guard = state.backend.lock().map_err(|_| not_open())?;
    let b = guard.as_mut().ok_or_else(not_open)?;
    let (target, n) = dump_to_dir(b, id, &name, &out_dir)?;
    Ok(format!("已保存 {}（{n} 字节）", target.display()))
}

#[tauri::command]
pub fn recover_all(
    state: tauri::State<AppState>,
    out_dir: String,
    min_size: u64,
    limit: usize,
) -> Result<RecoverDto, String> {
    let mut guard = state.backend.lock().map_err(|_| not_open())?;
    let b = guard.as_mut().ok_or_else(not_open)?;
    fs::create_dir_all(&out_dir).map_err(|e| format!("创建 {out_dir} 失败: {e}"))?;
    let mut items = b.deleted(0)?;
    items.retain(|e| !e.is_dir && e.size >= min_size);
    if limit > 0 {
        items.truncate(limit);
    }
    let mut ok = 0u64;
    let mut failed = 0u64;
    let mut bytes = 0u64;
    let mut saved = Vec::new();
    for e in &items {
        match dump_to_dir(b, e.id, &e.name, &out_dir) {
            Ok((p, n)) => {
                ok += 1;
                bytes += n;
                saved.push(p.display().to_string());
            }
            Err(_) => failed += 1,
        }
    }
    Ok(RecoverDto {
        ok,
        failed,
        bytes,
        bytes_human: dmdisk::human_size(bytes),
        out_dir,
        saved,
    })
}

/// 默认输出目录：`~/Downloads/dmde-rs-recovered`
#[tauri::command]
pub fn default_out_dir() -> String {
    let home = std::env::var("HOME").unwrap_or_else(|_| ".".to_string());
    format!("{home}/Downloads/dmde-rs-recovered")
}

#[derive(serde::Serialize)]
pub struct AdminState {
    /// 当前会话是否由特权 helper 提供
    pub active: bool,
    /// 提权通道是否可用（helper 活着）
    pub helper: bool,
    /// 当前会话后端：local / admin
    pub backend: String,
}

#[tauri::command]
pub fn admin_state(state: tauri::State<AppState>) -> AdminState {
    let backend = state
        .backend
        .lock()
        .ok()
        .and_then(|g| g.as_ref().map(|b| b.kind().to_string()))
        .unwrap_or_else(|| "none".into());
    AdminState {
        active: backend == "admin",
        helper: priv_helper::alive(),
        backend,
    }
}

/// 主动请求管理员权限（会弹出 macOS 标准授权窗）。
#[tauri::command]
pub fn request_admin() -> Result<String, String> {
    let mut c = priv_helper::connect()?;
    c.ping()?;
    Ok("已获得管理员权限（只读）".to_string())
}

/// 放弃管理员权限：helper 收到 shutdown 后自行退出。
#[tauri::command]
pub fn drop_admin(state: tauri::State<AppState>) -> Result<String, String> {
    {
        let mut guard = state.backend.lock().map_err(|_| not_open())?;
        if matches!(guard.as_ref(), Some(Backend::Remote(_))) {
            *guard = None;
        }
    }
    priv_helper::shutdown();
    Ok("已退出提权模式".into())
}

#[tauri::command]
pub fn current_source(state: tauri::State<AppState>) -> Result<String, String> {
    let guard = state.backend.lock().map_err(|_| not_open())?;
    let b = guard.as_ref().ok_or_else(not_open)?;
    Ok(b.desc())
}

// ---------------------------------------------------------------------------
// 导出（"挑着恢复"的核心）：把一个目录或单个文件写到本地磁盘。
// 读字节始终由只读后端完成，写盘始终以用户身份进行，权限不混。
// ---------------------------------------------------------------------------

#[derive(Default)]
struct ExportStats {
    files: u64,
    dirs: u64,
    failed: u64,
    bytes: u64,
}

fn path_base_name(path: &str) -> String {
    let t = path.trim_end_matches('/');
    t.rsplit('/').next().filter(|s| !s.is_empty()).unwrap_or("root").to_string()
}

fn export_file(
    b: &mut Backend,
    entry: &EntryDto,
    dest: &Path,
    st: &mut ExportStats,
) {
    if let Some(par) = dest.parent() {
        let _ = fs::create_dir_all(par);
    }
    let mut f = match fs::File::create(dest) {
        Ok(f) => f,
        Err(_) => {
            st.failed += 1;
            return;
        }
    };
    // 用 id 读：路径可能含非法字符或已变化，id 是会话里的权威句柄。
    // 单文件兜底（id=0）时退回按路径读。
    let r = if entry.id > 0 {
        b.read_id(entry.id, &mut f)
    } else {
        b.read_path(&entry.path, &mut f)
    };
    match r {
        Ok(n) => {
            st.files += 1;
            st.bytes += n;
        }
        Err(_) => st.failed += 1,
    }
}

fn export_dir(
    b: &mut Backend,
    src: &str,
    dest: &Path,
    st: &mut ExportStats,
    depth: usize,
) {
    // 防符号链接自指或异常深目录把栈打爆
    if depth > 64 {
        st.failed += 1;
        return;
    }
    if fs::create_dir_all(dest).is_err() {
        st.failed += 1;
        return;
    }
    st.dirs += 1;
    let entries = match b.list_dir(src, false) {
        Ok(v) => v,
        Err(_) => {
            st.failed += 1;
            return;
        }
    };
    for e in entries {
        if e.name == "." || e.name == ".." {
            continue;
        }
        let name = sanitize_name(&e.name);
        if name.is_empty() {
            st.failed += 1;
            continue;
        }
        let p = dest.join(&name);
        if e.is_dir {
            export_dir(b, &e.path, &p, st, depth + 1);
        } else {
            export_file(b, &e, &p, st);
        }
    }
}

/// 导出当前路径（目录递归或单文件）到 `out_dir`，保持原有目录结构。
#[tauri::command]
pub fn export_path(
    state: tauri::State<AppState>,
    path: String,
    out_dir: String,
) -> Result<String, String> {
    let mut guard = state.backend.lock().map_err(|_| not_open())?;
    let b = guard.as_mut().ok_or_else(not_open)?;
    do_export(b, &path, &out_dir)
}

/// 导出核心（与 Tauri 解耦，便于直接测试）。
pub fn do_export(b: &mut Backend, path: &str, out_dir: &str) -> Result<String, String> {
    let root = PathBuf::from(out_dir);
    fs::create_dir_all(&root).map_err(|e| format!("无法创建输出目录 {out_dir}: {e}"))?;

    // 必须先 stat：`list_dir` 对普通文件也会返回空列表，
    // 靠"列不出东西"猜类型会把文件当空目录建出来。
    let entry = b
        .stat(path)?
        .ok_or_else(|| format!("路径不存在: {path}"))?;

    let name = sanitize_name(&path_base_name(path));
    let dest = root.join(&name);
    let mut st = ExportStats::default();

    if entry.is_dir {
        fs::create_dir_all(&dest).map_err(|e| format!("创建 {} 失败: {e}", dest.display()))?;
        st.dirs += 1;
        let entries = b.list_dir(path, false)?;
        for e in entries {
            if e.name == "." || e.name == ".." {
                continue;
            }
            let n = sanitize_name(&e.name);
            if n.is_empty() {
                st.failed += 1;
                continue;
            }
            let p = dest.join(&n);
            if e.is_dir {
                export_dir(b, &e.path, &p, &mut st, 1);
            } else {
                export_file(b, &e, &p, &mut st);
            }
        }
    } else {
        export_file(b, &entry, &dest, &mut st);
    }

    Ok(format!(
        "已导出到 {}：{} 个文件、{} 个目录、{} 个失败，共 {}",
        dest.display(),
        st.files,
        st.dirs,
        st.failed,
        dmdisk::human_size(st.bytes)
    ))
}
