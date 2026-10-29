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
use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};

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
pub async fn open_source(
    state: tauri::State<'_, AppState>,
    path: String,
    partition: Option<usize>,
) -> Result<SourceDto, String> {
    // 这里最长要等系统授权窗 180 秒，再读几 MB 分区元数据。
    // 同步命令跑在 Tauri 主线程上，一旦阻塞窗口连重绘都做不到，
    // 用户看到的就是"点完授权，整个应用卡死"。
    let p = path.clone();
    let opened = tauri::async_runtime::spawn_blocking(
        move || -> Result<(Backend, SourceDto), String> {
            match open_local(&p, partition) {
                Ok((s, dto)) => Ok((Backend::Local(s), dto)),
                Err(e) if e.permission => {
                    let mut client = priv_helper::connect()?;
                    let dto = client.open(&p, partition)?;
                    Ok((Backend::Remote(client), dto))
                }
                Err(e) => Err(e.message),
            }
        },
    )
    .await
    .map_err(|e| format!("打开任务异常: {e}"))?;

    let (backend, dto) = opened?;
    let mut guard = state.backend.lock().map_err(|_| not_open())?;
    *guard = Some(backend);
    Ok(dto)
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
pub async fn scan_deleted(
    state: tauri::State<'_, AppState>,
    limit: Option<usize>,
) -> Result<Vec<EntryDto>, String> {
    // APFS 上这一步要遍历整棵 fs-tree（几千万条），必须离开主线程
    let mut backend = {
        let mut g = state.backend.lock().map_err(|_| not_open())?;
        g.take().ok_or_else(not_open)?
    };
    let joined = tauri::async_runtime::spawn_blocking(move || {
        let r = backend.deleted(limit.unwrap_or(0));
        (backend, r)
    })
    .await;
    match joined {
        Ok((b, r)) => {
            let mut g = state.backend.lock().map_err(|_| not_open())?;
            *g = Some(b);
            r
        }
        Err(e) => Err(format!("扫描任务异常: {e}")),
    }
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
pub async fn admin_state(state: tauri::State<'_, AppState>) -> Result<AdminState, String> {
    let backend = state
        .backend
        .lock()
        .ok()
        .and_then(|g| g.as_ref().map(|b| b.kind().to_string()))
        .unwrap_or_else(|| "none".into());
    // alive() 会真的去连一次 socket，可能阻塞几秒 —— 放到后台线程
    let helper = tauri::async_runtime::spawn_blocking(priv_helper::alive)
        .await
        .unwrap_or(false);
    Ok(AdminState {
        active: backend == "admin",
        helper,
        backend,
    })
}

/// 主动请求管理员权限（会弹出 macOS 标准授权窗）。
#[tauri::command]
pub async fn request_admin() -> Result<String, String> {
    tauri::async_runtime::spawn_blocking(|| {
        let mut c = priv_helper::connect()?;
        c.ping()?;
        Ok("已获得管理员权限（只读）".to_string())
    })
    .await
    .map_err(|e| format!("提权任务异常: {e}"))?
}

/// 放弃管理员权限：helper 收到 shutdown 后自行退出。
#[tauri::command]
pub async fn drop_admin(state: tauri::State<'_, AppState>) -> Result<String, String> {
    {
        let mut guard = state.backend.lock().map_err(|_| not_open())?;
        if matches!(guard.as_ref(), Some(Backend::Remote(_))) {
            *guard = None;
        }
    }
    let _ = tauri::async_runtime::spawn_blocking(priv_helper::shutdown).await;
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
pub struct ExportStats {
    files: u64,
    dirs: u64,
    failed: u64,
    bytes: u64,
    /// 用户主动暂停时为 true（此时的计数是「暂停前已完成的部分」）
    cancelled: bool,
}

/// 导出进度事件（每 ~120ms 最多一帧，避免刷屏拖慢导出本身）。
#[derive(Default, Clone, serde::Serialize)]
pub struct ExportProgress {
    pub files: u64,
    pub dirs: u64,
    pub failed: u64,
    pub bytes: u64,
    pub current: String,
    pub done: bool,
    pub cancelled: bool,
}

/// 包一层写盘：每次 `write` 前查一次暂停标志。
///
/// 只在「文件之间」检查是不够的——单个十几 GB 的文件要写好几分钟，
/// 这期间用户点了暂停却毫无反应，体验上就是「停不下来」。
/// `read_id` / `read_path` 是分块写进来的，所以在文件内部也能立刻中断。
struct CancelWriter<'a, W: std::io::Write> {
    inner: W,
    cancel: &'a AtomicBool,
}

impl<W: std::io::Write> std::io::Write for CancelWriter<'_, W> {
    fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
        if self.cancel.load(Ordering::Relaxed) {
            // 必须用「不可重试」的错误类型。
            // std 的 write_all 对 ErrorKind::Interrupted 是**静默重试**的：
            //     Err(ref e) if e.is_interrupted() => {}   // 继续循环
            // 用它当暂停信号会让 write_all 无限重试、导出线程永远退不出来
            // —— 表现就是"点了暂停没反应，也停不下来"。
            return Err(std::io::Error::other("用户已暂停导出"));
        }
        self.inner.write(buf)
    }
    fn flush(&mut self) -> std::io::Result<()> {
        self.inner.flush()
    }
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
    cancel: &AtomicBool,
    on_progress: &mut dyn FnMut(&ExportStats, &str),
) {
    if cancel.load(Ordering::Relaxed) {
        st.cancelled = true;
        return;
    }
    if let Some(par) = dest.parent() {
        let _ = fs::create_dir_all(par);
    }
    let f = match fs::File::create(dest) {
        Ok(f) => f,
        Err(_) => {
            st.failed += 1;
            return;
        }
    };
    // 用 id 读：路径可能含非法字符或已变化，id 是会话里的权威句柄。
    // 单文件兜底（id=0）时退回按路径读。
    let mut w = CancelWriter { inner: f, cancel };
    let r = if entry.id > 0 {
        b.read_id(entry.id, &mut w)
    } else {
        b.read_path(&entry.path, &mut w)
    };
    let _ = w.flush();
    drop(w);

    match r {
        Ok(n) => {
            st.files += 1;
            st.bytes += n;
            on_progress(st, &entry.path);
        }
        // 被中断的文件不能冒充完整文件：改名 .partial，
        // 既保留已恢复的字节，又不至于让人误以为拿到了完整数据。
        Err(_) if cancel.load(Ordering::Relaxed) => {
            st.cancelled = true;
            let mut tmp = dest.as_os_str().to_owned();
            tmp.push(".partial");
            let _ = fs::rename(dest, PathBuf::from(tmp));
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
    cancel: &AtomicBool,
    on_progress: &mut dyn FnMut(&ExportStats, &str),
) {
    // 防符号链接自指或异常深目录把栈打爆
    if depth > 64 {
        st.failed += 1;
        return;
    }
    if cancel.load(Ordering::Relaxed) {
        st.cancelled = true;
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
    on_progress(st, src);
    for e in entries {
        // 每处理一个条目就查一次：目录里几千个文件时，靠递归入口检查
        // 要等整层遍历完才停，实际可能已经写了几百个文件
        if cancel.load(Ordering::Relaxed) {
            st.cancelled = true;
            return;
        }
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
            export_dir(b, &e.path, &p, st, depth + 1, cancel, on_progress);
        } else {
            export_file(b, &e, &p, st, cancel, on_progress);
        }
        if st.cancelled {
            return;
        }
    }
}

/// 暂停正在进行的导出。
///
/// 只置一个标志位、不碰 backend：导出线程正独占 backend，
/// 任何去抢那把锁的写法都会死锁（表现就是「点暂停整个应用卡住」）。
#[tauri::command]
pub fn cancel_export(state: tauri::State<AppState>) -> Result<bool, String> {
    state.export_cancel.store(true, Ordering::SeqCst);
    Ok(true)
}

/// 导出当前路径（目录递归或单文件）到 `out_dir`，边导边通过事件上报进度。
#[tauri::command]
pub async fn export_path(
    app: tauri::AppHandle,
    state: tauri::State<'_, AppState>,
    path: String,
    out_dir: String,
) -> Result<String, String> {
    // 把后端独占取出：既避免跨线程借用，也保证导出期间不会被别的命令插队
    let backend = {
        let mut g = state.backend.lock().map_err(|_| not_open())?;
        g.take().ok_or_else(not_open)?
    };

    // 清掉上一轮可能残留的暂停标志，否则这次一开始就会立刻"暂停"
    state.export_cancel.store(false, Ordering::SeqCst);
    let flag = state.export_cancel.clone();

    let handle = app.clone();
    let joined = tauri::async_runtime::spawn_blocking(move || {
        let mut b = backend;
        let mut last = std::time::Instant::now();
        let res = do_export_with(&mut b, &path, &out_dir, &flag, &mut |st, cur| {
            // 节流：导出几万个小文件时，每个都发事件会把 WebView 拖死
            if last.elapsed().as_millis() >= 120 {
                last = std::time::Instant::now();
                let _ = tauri::Emitter::emit(
                    &handle,
                    "export-progress",
                    ExportProgress {
                        files: st.files,
                        dirs: st.dirs,
                        failed: st.failed,
                        bytes: st.bytes,
                        current: cur.to_string(),
                        done: false,
                        cancelled: st.cancelled,
                    },
                );
            }
        });
        let cancelled = flag.load(Ordering::SeqCst);
        (b, res, cancelled)
    })
    .await;

    match joined {
        Ok((b, res, cancelled)) => {
            let mut g = state.backend.lock().map_err(|_| not_open())?;
            *g = Some(b);
            let _ = tauri::Emitter::emit(
                &app,
                "export-progress",
                ExportProgress { done: true, cancelled, ..Default::default() },
            );
            res
        }
        Err(e) => {
            let _ = tauri::Emitter::emit(
                &app,
                "export-progress",
                ExportProgress { done: true, ..Default::default() },
            );
            Err(format!("导出任务异常: {e}"))
        }
    }
}

/// 导出核心（与 Tauri 解耦，便于直接测试）。
pub fn do_export(b: &mut Backend, path: &str, out_dir: &str) -> Result<String, String> {
    do_export_with(b, path, out_dir, &AtomicBool::new(false), &mut |_, _| {})
}

pub fn do_export_with(
    b: &mut Backend,
    path: &str,
    out_dir: &str,
    cancel: &AtomicBool,
    on_progress: &mut dyn FnMut(&ExportStats, &str),
) -> Result<String, String> {
    let root = PathBuf::from(out_dir);
    fs::create_dir_all(&root).map_err(|e| format!("无法创建输出目录 {out_dir}: {e}"))?;

    // 必须先 stat：`list_dir` 对普通文件也会返回空列表，
    // 靠"列不出东西"猜类型会把文件当空目录建出来。
    let entry = b.stat(path)?.ok_or_else(|| format!("路径不存在: {path}"))?;

    let name = sanitize_name(&path_base_name(path));
    let dest = root.join(&name);
    let mut st = ExportStats::default();

    if entry.is_dir {
        fs::create_dir_all(&dest).map_err(|e| format!("创建 {} 失败: {e}", dest.display()))?;
        st.dirs += 1;
        on_progress(&st, path);
        let entries = b.list_dir(path, false)?;
        for e in entries {
            if cancel.load(Ordering::Relaxed) {
                st.cancelled = true;
                break;
            }
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
                export_dir(b, &e.path, &p, &mut st, 1, cancel, on_progress);
            } else {
                export_file(b, &e, &p, &mut st, cancel, on_progress);
            }
            if st.cancelled {
                break;
            }
        }
    } else {
        export_file(b, &entry, &dest, &mut st, cancel, on_progress);
    }

    let tail = if st.cancelled {
        "；已按你的要求暂停（未写完的文件改名为 .partial）"
    } else {
        ""
    };
    Ok(format!(
        "{} {}：{} 个文件、{} 个目录、{} 个失败，共 {}{}",
        if st.cancelled { "已暂停导出到" } else { "已导出到" },
        dest.display(),
        st.files,
        st.dirs,
        st.failed,
        dmdisk::human_size(st.bytes),
        tail
    ))
}


#[cfg(test)]
mod tests {
    use super::{CancelWriter, ExportStats};
    use std::sync::atomic::AtomicBool;
    use std::sync::Arc;

    /// 暂停的核心保证：**文件内部**也能中断。
    ///
    /// 如果只在文件之间检查，一个 30 GB 的视频要写十几分钟，
    /// 用户点暂停后毫无反应 —— 这正是"停不下来"的体验来源。
    #[test]
    fn cancel_writer_interrupts_mid_file() {
        eprintln!("[t] 开始");
        let flag = Arc::new(AtomicBool::new(false));
        let mut sink: Vec<u8> = Vec::new();
        {
            let mut w = CancelWriter { inner: &mut sink, cancel: &flag };
            use std::io::Write;
            w.write_all(b"first chunk").unwrap();
            // 模拟用户在写第一个文件的过程中点了暂停
            flag.store(true, std::sync::atomic::Ordering::SeqCst);
            let e = w.write_all(b"second chunk").unwrap_err();
            // 关键：不能是 Interrupted —— write_all 会把 Interrupted 当"稍后重试"，
            // 在循环里反复调用 write，暂停就变成了死循环。
            assert_ne!(
                e.kind(),
                std::io::ErrorKind::Interrupted,
                "暂停信号必须是不可重试的错误，否则 write_all 会无限重试"
            );
        }
        // 暂停前写下的字节保留，暂停后的一个字节都不写
        assert_eq!(sink, b"first chunk");
    }

    #[test]
    fn cancel_writer_passes_through_when_not_cancelled() {
        let flag = AtomicBool::new(false);
        let mut sink: Vec<u8> = Vec::new();
        {
            let mut w = CancelWriter { inner: &mut sink, cancel: &flag };
            use std::io::Write;
            w.write_all(b"hello").unwrap();
        }
        assert_eq!(sink, b"hello");
    }

    /// 统计字段的默认值必须是「未暂停」，否则正常导出会被误报成暂停。
    #[test]
    fn export_stats_default_is_not_cancelled() {
        assert!(!ExportStats::default().cancelled);
    }
}
