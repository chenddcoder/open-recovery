//! 前端可调用的命令。全部围绕"当前会话"这一个状态。
//!
//! 会话有两种后端：
//! * `Local`  —— 镜像文件 / 已有权限的设备，本进程直接读；
//! * `Remote` —— `/dev/rdisk*`，经系统授权弹窗拉起的只读 helper 读。
//! 前端不用关心是哪一种，命令签名完全一样。

use crate::skip::SkipRules;
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

/// backend 被长任务取走时给出的说明（比"还没有打开任何卷"诚实）。
fn busy_note(state: &AppState) -> Option<String> {
    state
        .busy
        .lock()
        .ok()
        .and_then(|g| g.clone())
        .map(|t| format!("正在{t}，请稍候（界面会在这期间暂时列不出目录）"))
}

fn set_busy(state: &AppState, what: Option<&str>) {
    if let Ok(mut g) = state.busy.lock() {
        *g = what.map(|s| s.to_string());
    }
}

/// 调试用：`DMDE_OPEN_ON_START=<镜像路径>` 时启动即打开该源。
///
/// 只是为了能在没有鼠标自动化的情况下复现"打开后列表不刷新"这类前端问题。
#[tauri::command]
pub fn boot_path() -> Option<String> {
    std::env::var("DMDE_OPEN_ON_START").ok().filter(|s| !s.is_empty())
}

/// 前端关键路径日志。
///
/// WebView 的 console 我们在外面看不到，出了问题只能猜。
/// 这个命令把事件追加到 `~/Library/Application Support/dmde-rs/ui.log`，
/// 排查"打开了但列表不刷新"这类问题时能直接看时间线。
#[tauri::command]
pub fn ui_log(msg: String) {
    let dir = std::env::var("HOME")
        .map(|h| std::path::PathBuf::from(h).join("Library/Application Support/dmde-rs"))
        .unwrap_or_else(|_| std::env::temp_dir());
    let _ = std::fs::create_dir_all(&dir);
    let line = format!(
        "[{}] {}\n",
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_secs())
            .unwrap_or(0),
        msg
    );
    use std::io::Write;
    if let Ok(mut f) = fs::OpenOptions::new().create(true).append(true).open(dir.join("ui.log")) {
        let _ = f.write_all(line.as_bytes());
    }
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
    let b = match guard.as_mut() {
        Some(b) => b,
        None => return Err(busy_note(&state).unwrap_or_else(not_open)),
    };
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
    set_busy(&state, Some("扫描可恢复文件"));
    let joined = tauri::async_runtime::spawn_blocking(move || {
        let r = backend.deleted(limit.unwrap_or(0));
        (backend, r)
    })
    .await;
    set_busy(&state, None);
    match joined {
        Ok((b, r)) => {
            let mut g = state.backend.lock().map_err(|_| not_open())?;
            *g = Some(b);
            r
        }
        Err(e) => Err(format!("扫描任务异常: {e}")),
    }
}

/// 读一个条目并写到本地。写盘由 GUI 自己完成（非 root），属主、TCC、
/// 目录权限都天然正确；helper 只负责把字节吐回来。
fn dump_to_dir(
    b: &mut Backend,
    id: u64,
    name: &str,
    out_dir: &str,
    conflict: Conflict,
) -> Result<Option<(PathBuf, u64)>, String> {
    fs::create_dir_all(out_dir).map_err(|e| format!("创建 {out_dir} 失败: {e}"))?;
    let wanted = Path::new(out_dir).join(sanitize_name(name));
    // 这里拿不到源文件大小（批量恢复只传 id），交给 exists() 判断
    let target = match resolve_conflict(&wanted, conflict, None) {
        Some(p) => p,
        None => return Ok(None), // 已存在且策略为跳过
    };
    let file = fs::File::create(&target).map_err(|e| format!("创建 {} 失败: {e}", target.display()))?;
    let mut w = std::io::BufWriter::new(file);
    match b.read_id(id, &mut w) {
        Ok(n) if n > 0 => {
            use std::io::Write;
            w.flush().ok();
            Ok(Some((target, n)))
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

/// 单文件导出核心（与 Tauri 解耦，便于直接测试）。
///
/// 和 `do_export_with` 的区别只有两点，但都很关键：
/// 1. **按 id 读**。「已删除」条目在目录里已经查不到，`stat(path)` 会直接报
///    "路径不存在"——单文件导出如果走路径，勾了「显示已删除」就一个都导不出来。
/// 2. **不套跳过规则**。规则是给整棵目录树省时间的；用户已经明确点了这个文件，
///    哪怕它叫 `x.log`、规则里写了 `*.log`，也该照导。
pub fn do_export_entry(
    b: &mut Backend,
    id: u64,
    name: &str,
    out_dir: &str,
    cancel: &AtomicBool,
    conflict: Conflict,
    on_progress: &mut dyn FnMut(&ExportStats, &str),
) -> Result<(ExportStats, PathBuf), String> {
    let safe = sanitize_name(name);
    if safe.is_empty() {
        return Err(format!("文件名不能作为本地文件名：{name}"));
    }
    if out_dir.trim().is_empty() {
        return Err("请先填写输出目录".into());
    }
    fs::create_dir_all(out_dir).map_err(|e| format!("创建 {out_dir} 失败: {e}"))?;
    let dest = Path::new(out_dir).join(&safe);
    // 只填 id：export_file 见到 id>0 就走 read_id，path/name 仅在提示与 id=0 兜底时用
    let entry = EntryDto {
        name: name.to_string(),
        path: name.to_string(),
        id,
        is_dir: false,
        size: 0,
        size_human: String::new(),
        deleted: false,
        mtime: None,
        name_lost: false,
    };
    let mut st = ExportStats::default();
    export_file(b, &entry, &dest, &mut st, cancel, conflict, on_progress);
    Ok((st, dest))
}

/// 单文件导出的结果文案（成功 / 暂停 / 跳过 / 失败各有说法）。
fn single_export_result(st: &ExportStats, name: &str, dest: &Path) -> Result<String, String> {
    if st.cancelled {
        return Ok(format!(
            "已暂停导出 {name}（未写完的部分改名为 {}.partial）",
            dest.display()
        ));
    }
    if st.skipped > 0 {
        return Ok(format!("{name} 已存在于输出目录，按当前策略跳过"));
    }
    if st.files == 0 {
        return Err(format!("导出失败：{name}（读盘出错或目标不可写）"));
    }
    Ok(format!("已导出 {} → {}（{}）", name, dest.display(), dmdisk::human_size(st.bytes)))
}

/// 导出单个文件（列表行内「导出」按钮走这里）。
///
/// 单文件也可能是几个 GB，所以和目录导出一样 async + spawn_blocking；
/// 冲突策略、暂停、`.partial` 语义全部复用 `export_file`，行为与目录导出一致。
#[tauri::command]
pub async fn export_entry(
    app: tauri::AppHandle,
    state: tauri::State<'_, AppState>,
    id: u64,
    name: String,
    out_dir: String,
    on_conflict: Option<String>,
) -> Result<String, String> {
    let conflict = Conflict::parse(on_conflict.as_deref());
    run_export_task(app, state, "导出文件", move |b, cancel, progress| {
        let (st, dest) = do_export_entry(b, id, &name, &out_dir, cancel, conflict, progress)?;
        single_export_result(&st, &name, &dest)
    })
    .await
}

/// 批量恢复：先把已删除条目全扫一遍，再逐个读出来写盘。
/// 全程可能要几分钟（APFS 要遍历整棵 fs-tree），所以和 scan_deleted 一样
/// 放到阻塞线程池里跑，否则 Tauri 主线程会被卡住、窗口直接无响应。
#[tauri::command]
pub async fn recover_all(
    state: tauri::State<'_, AppState>,
    out_dir: String,
    min_size: u64,
    limit: usize,
) -> Result<RecoverDto, String> {
    let mut backend = {
        let mut g = state.backend.lock().map_err(|_| not_open())?;
        g.take().ok_or_else(not_open)?
    };
    fs::create_dir_all(&out_dir).map_err(|e| format!("创建 {out_dir} 失败: {e}"))?;
    set_busy(&state, Some("批量恢复已删除"));
    let joined = tauri::async_runtime::spawn_blocking(move || {
        let r = (|| -> Result<RecoverDto, String> {
            let mut items = backend.deleted(0)?;
            items.retain(|e| !e.is_dir && e.size >= min_size);
            if limit > 0 {
                items.truncate(limit);
            }
            let mut ok = 0u64;
            let mut failed = 0u64;
            let mut bytes = 0u64;
            let mut saved = Vec::new();
            for e in &items {
                // 批量恢复一律「改名保留两份」：这里的文件都是刚从盘里捞出来的，
                // 覆盖掉输出目录里同名的旧结果风险太大
                match dump_to_dir(&mut backend, e.id, &e.name, &out_dir, Conflict::Rename) {
                    Ok(Some((p, n))) => {
                        ok += 1;
                        bytes += n;
                        saved.push(p.display().to_string());
                    }
                    Ok(None) | Err(_) => failed += 1,
                }
            }
            Ok(RecoverDto {
                ok,
                failed,
                bytes,
                bytes_human: dmdisk::human_size(bytes),
                out_dir: out_dir.clone(),
                saved,
            })
        })();
        (backend, r)
    })
    .await;
    set_busy(&state, None);
    match joined {
        Ok((b, r)) => {
            let mut g = state.backend.lock().map_err(|_| not_open())?;
            *g = Some(b);
            r
        }
        Err(e) => Err(format!("恢复任务异常: {e}")),
    }
}

/// 默认输出目录：`~/Downloads/dmde-rs-recovered`
#[tauri::command]
pub fn default_out_dir() -> String {
    let home = std::env::var("HOME").unwrap_or_else(|_| ".".to_string());
    format!("{home}/Downloads/dmde-rs-recovered")
}

/// 默认跳过规则串，给界面预填用。
///
/// 规则表放在 Rust 侧当权威来源：默认值只有一份，界面只是展示和编辑它，
/// 免得前后端各写一份、改了一边忘了另一边。
#[tauri::command]
pub fn default_skip_rules() -> String {
    crate::skip::DEFAULT_SPEC.to_string()
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

/// 目标位置已有同名文件时的处理方式。
///
/// 默认 **Skip**：恢复几 TB 的目录本来就慢，重复导出一遍再全量覆盖，
/// 既浪费几个小时，也可能把上一轮已经确认无误的结果盖掉。
#[derive(Clone, Copy, PartialEq, Eq, Debug, Default)]
pub enum Conflict {
    /// 已存在就跳过（不读源、不写盘，最快）
    #[default]
    Skip,
    /// 保留已有文件，新文件改名为 `名字 (1).扩展名`
    Rename,
    /// 直接覆盖。注意：这是唯一会动到你已有文件的策略
    Overwrite,
}

impl Conflict {
    /// 前端传字符串；未知值一律按最安全的 Skip 处理，不做"猜"。
    pub fn parse(s: Option<&str>) -> Self {
        match s.unwrap_or("").trim().to_ascii_lowercase().as_str() {
            "overwrite" => Conflict::Overwrite,
            "rename" => Conflict::Rename,
            _ => Conflict::Skip,
        }
    }
}

/// 给同名文件找一个新的兄弟名字：`a.txt` → `a (1).txt`。
///
/// 必须是 `名字 (n).扩展名`，不能是 `a.txt.1`：后者双击时系统不认识，
/// 恢复出来的东西还得手动改回来。
fn unique_sibling(path: &Path) -> PathBuf {
    let parent = path.parent().unwrap_or_else(|| Path::new("."));
    let stem = path
        .file_stem()
        .map(|s| s.to_string_lossy().to_string())
        .unwrap_or_default();
    let ext = path.extension().map(|e| e.to_string_lossy().to_string());
    for n in 1..=9999u32 {
        let cand = match &ext {
            Some(e) => parent.join(format!("{stem} ({n}).{e}")),
            None => parent.join(format!("{stem} ({n})")),
        };
        if !cand.exists() {
            return cand;
        }
    }
    // 一万个同名文件还没找到空位就别再数了，加进程号保证不撞
    parent.join(format!("{stem}-{}-{}", std::process::id(), ext.unwrap_or_default()))
}

/// 按策略决定这次实际写到哪里；`None` 表示「跳过」。
/// 目标位置能写吗、该写到哪。
///
/// `expect` 是源文件大小（不知道时传 None）。三条规矩：
///
/// 1. 同名位置是**目录**时文件写不进去，只能按策略退让。
/// 2. **0 字节不算"已存在"**。以前写盘是直接 `File::create` 正式名，读盘一失败
///    就留下 0 字节空壳；而 Skip 只看 `exists()`，于是空壳被当成"已导出"永久跳过。
///    Parallels 的 `CentOS Linux-0.hdd` 里那个 `.hds`（整块磁盘镜像）就是这么
///    变成 0 字节、且反复重导都补不上的。空文件不可能是用户的资料，放心重写。
/// 3. 但**非空且大小不同**时仍然跳过：那可能是你自己放的同名文件，
///    Skip 的承诺就是"不碰已有的东西"。要强制补齐请把策略换成"覆盖"。
fn resolve_conflict(dest: &Path, mode: Conflict, expect: Option<u64>) -> Option<PathBuf> {
    let meta = match fs::symlink_metadata(dest) {
        Ok(m) => m,
        // 不存在（或父目录都还没有）就是要写的位置
        Err(_) => return Some(dest.to_path_buf()),
    };
    if meta.is_dir() {
        return match mode {
            Conflict::Rename => Some(unique_sibling(dest)),
            _ => None,
        };
    }
    if let Some(want) = expect {
        if want > 0 && meta.len() == 0 {
            return Some(dest.to_path_buf());
        }
    }
    match mode {
        Conflict::Skip => None,
        Conflict::Overwrite => Some(dest.to_path_buf()),
        Conflict::Rename => Some(unique_sibling(dest)),
    }
}

#[derive(Default)]
pub struct ExportStats {
    pub files: u64,
    pub dirs: u64,
    pub failed: u64,
    /// 因为目标已存在而跳过的条目数（策略为 Skip 时）
    pub skipped: u64,
    /// 被跳过规则挡下的条目数（node_modules 之类），**不读盘**
    pub skipped_rule: u64,
    pub bytes: u64,
    /// 用户主动暂停时为 true（此时的计数是「暂停前已完成的部分」）
    pub cancelled: bool,
}

/// 导出进度事件（每 ~120ms 最多一帧，避免刷屏拖慢导出本身）。
#[derive(Default, Clone, serde::Serialize)]
pub struct ExportProgress {
    pub files: u64,
    pub dirs: u64,
    pub failed: u64,
    pub skipped: u64,
    pub skipped_rule: u64,
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
    conflict: Conflict,
    on_progress: &mut dyn FnMut(&ExportStats, &str),
) {
    if cancel.load(Ordering::Relaxed) {
        st.cancelled = true;
        return;
    }
    // 同名策略在这里生效：Skip 且目标已完整时直接返回，连源都不读（几万个文件时快很多）。
    // 已存在但大小对不上，说明是上次失败留下的残骸，必须重读，不能算"已导出"。
    let actual = match resolve_conflict(dest, conflict, Some(entry.size)) {
        Some(p) => p,
        None => {
            st.skipped += 1;
            // 正式文件是完整的，暂停留下的 .partial 就是过时垃圾，顺手清掉
            let mut stale = dest.as_os_str().to_owned();
            stale.push(".partial");
            let _ = fs::remove_file(PathBuf::from(stale));
            return;
        }
    };
    let dest = actual.as_path();
    if let Some(par) = dest.parent() {
        let _ = fs::create_dir_all(par);
    }

    // 先写 `<名字>.partial`，整份读完才改名到正式名。
    //
    // 这样"正式名存在 == 内容完整"才有保证：中途读盘失败、本地盘满、用户暂停，
    // 都只会在 .partial 上留下半截，不会把正式文件截成 0 字节。
    // 以前直接 create 正式名，一次失败就把好文件变成空壳，而 Skip 又只看 exists()，
    // 于是那块盘上 Parallels 的 .hds 永远导不出来。
    let mut part = dest.as_os_str().to_owned();
    part.push(".partial");
    let part = PathBuf::from(part);

    let f = match fs::File::create(&part) {
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
        Ok(n) => match fs::rename(&part, dest) {
            Ok(()) => {
                st.files += 1;
                st.bytes += n;
                on_progress(st, &entry.path);
            }
            // 改名失败（目标被占用、跨卷等）算失败，但 .partial 里是完整数据，
            // 留在原地比删掉强
            Err(_) => st.failed += 1,
        },
        // 被中断：文件本来就叫 .partial，不用改名，保留已恢复的字节
        Err(_) if cancel.load(Ordering::Relaxed) => st.cancelled = true,
        // 读盘失败：同样只留 .partial，正式名不会出现半个文件
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
    conflict: Conflict,
    rules: &SkipRules,
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

    // 目录已存在是正常的（增量导出、重复导出），直接复用；
    // 但如果同名位置是个**文件**，就得按策略处理——
    // 注意 Overwrite 也不删它：为了建目录而删掉你的文件，代价太大。
    // 唯一例外是 0 字节的空壳：那是之前某次失败留下的垃圾，里面没有任何数据，
    // 挡着一整棵目录不导才是真的坑（.hdd 这类 bundle 目录最容易被它卡住）。
    let empty_shell = dest.symlink_metadata().map(|m| !m.is_dir() && m.len() == 0).unwrap_or(false);
    let actual = if dest.exists() && !dest.is_dir() && !empty_shell {
        match conflict {
            Conflict::Skip => {
                st.skipped += 1;
                return;
            }
            Conflict::Rename => unique_sibling(dest),
            Conflict::Overwrite => {
                st.failed += 1;
                return;
            }
        }
    } else {
        dest.to_path_buf()
    };
    let dest = actual.as_path();

    // 0 字节空壳占着目录的位置，先清掉才能建目录
    if empty_shell {
        let _ = fs::remove_file(dest);
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
        // 规则命中就整棵跳过：连目录都不建、文件也不读。
        // 这一步必须在 create_dir_all / read 之前——跳过的意义就是省掉读盘。
        if rules.matches(&e.name, &e.path) {
            st.skipped_rule += 1;
            continue;
        }
        let p = dest.join(&name);
        if e.is_dir {
            export_dir(b, &e.path, &p, st, depth + 1, cancel, conflict, rules, on_progress);
        } else {
            export_file(b, &e, &p, st, cancel, conflict, on_progress);
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

/// 前端提交的一个待导出条目。
///
/// 用 id 而不是路径定位：已删除条目在目录树里已经查不到，只有会话 id 可靠。
/// `name` 决定输出文件名，`is_dir` 决定走递归还是单文件。
#[derive(Debug, Clone, serde::Deserialize)]
pub struct ExportItem {
    pub id: u64,
    pub path: String,
    pub name: String,
    pub is_dir: bool,
}

/// 导出统计的收尾文案（三处导出共用，避免各写一份、文案互相对不上）。
fn export_summary(st: &ExportStats, dest: &Path) -> String {
    let mut extra = String::new();
    if st.skipped > 0 {
        extra.push_str(&format!("、跳过 {} 个（目标已存在）", st.skipped));
    }
    if st.skipped_rule > 0 {
        extra.push_str(&format!("、按规则跳过 {} 个（依赖/缓存目录，未读盘）", st.skipped_rule));
    }
    if st.cancelled {
        extra.push_str("；已按你的要求暂停（未写完的文件改名为 .partial）");
    }
    format!(
        "{} {}：{} 个文件、{} 个目录、{} 个失败{}，共 {}",
        if st.cancelled { "已暂停导出到" } else { "已导出到" },
        dest.display(),
        st.files,
        st.dirs,
        st.failed,
        extra,
        dmdisk::human_size(st.bytes)
    )
}

/// 把一组选中项导出到 `out_dir`：目录按子树递归，文件只导自己。
///
/// 这是「挑着恢复」的真正主路径——用户经常只想捞两三个目录（比如几台虚拟机），
/// 而不是整个父目录。所有条目共用一份统计、一个暂停标志、一套冲突策略；
/// 暂停后已经导完的部分完整保留。
pub fn do_export_items(
    b: &mut Backend,
    items: &[ExportItem],
    out_dir: &str,
    cancel: &AtomicBool,
    conflict: Conflict,
    rules: &SkipRules,
    on_progress: &mut dyn FnMut(&ExportStats, &str),
) -> Result<String, String> {
    if items.is_empty() {
        return Err("没有选中任何条目".into());
    }
    let root = PathBuf::from(out_dir);
    fs::create_dir_all(&root).map_err(|e| format!("无法创建输出目录 {out_dir}: {e}"))?;
    let mut st = ExportStats::default();
    for it in items {
        if cancel.load(Ordering::Relaxed) {
            st.cancelled = true;
            break;
        }
        let name = sanitize_name(&it.name);
        if name.is_empty() {
            st.failed += 1;
            continue;
        }
        let dest = root.join(&name);
        if it.is_dir {
            // export_dir 自己建目录、自己 +1 目录数，这里不要重复统计
            export_dir(b, &it.path, &dest, &mut st, 1, cancel, conflict, rules, on_progress);
        } else {
            // 用户已经明确点了这个文件：不套跳过规则，哪怕它命中 *.log
            let entry = EntryDto {
                name: it.name.clone(),
                path: it.path.clone(),
                id: it.id,
                is_dir: false,
                size: 0,
                size_human: String::new(),
                deleted: false,
                mtime: None,
                name_lost: false,
            };
            export_file(b, &entry, &dest, &mut st, cancel, conflict, on_progress);
        }
        if st.cancelled {
            break;
        }
    }
    Ok(export_summary(&st, &root))
}

/// 导出任务的统一外壳：独占 backend、跑在阻塞线程、节流发进度、结束后归位。
///
/// 三种导出（当前目录 / 选中项 / 单个文件）只差中间「具体导什么」，
/// 其余全一样——抽出来，免得三份代码各自演变出不同的暂停、异常行为。
async fn run_export_task<F>(
    app: tauri::AppHandle,
    state: tauri::State<'_, AppState>,
    busy: &'static str,
    job: F,
) -> Result<String, String>
where
    F: FnOnce(&mut Backend, &AtomicBool, &mut dyn FnMut(&ExportStats, &str)) -> Result<String, String>
        + Send
        + 'static,
{
    // 把后端独占取出：既避免跨线程借用，也保证导出期间不会被别的命令插队
    let backend = {
        let mut g = state.backend.lock().map_err(|_| not_open())?;
        g.take().ok_or_else(not_open)?
    };
    // 清掉上一轮可能残留的暂停标志，否则这次一开始就会立刻"暂停"
    state.export_cancel.store(false, Ordering::SeqCst);
    set_busy(&state, Some(busy));
    let flag = state.export_cancel.clone();
    let handle = app.clone();

    let joined = tauri::async_runtime::spawn_blocking(move || {
        let mut b = backend;
        let mut last = std::time::Instant::now();
        let res = job(&mut b, &flag, &mut |st: &ExportStats, cur: &str| {
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
                        skipped: st.skipped,
                        skipped_rule: st.skipped_rule,
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

    set_busy(&state, None);
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

/// 导出当前路径（目录递归或单文件）到 `out_dir`，边导边通过事件上报进度。
#[tauri::command]
pub async fn export_path(
    app: tauri::AppHandle,
    state: tauri::State<'_, AppState>,
    path: String,
    out_dir: String,
    on_conflict: Option<String>,
    skip_rules: Option<String>,
) -> Result<String, String> {
    let conflict = Conflict::parse(on_conflict.as_deref());
    // `skip_rules` 不传 = 用默认规则（跳过 node_modules 等）；
    // 传空串 = 用户把列表清空了，表示「全都要」，一条也不跳。
    let rules = SkipRules::from_opt(skip_rules.as_deref());
    run_export_task(app, state, "导出文件", move |b, cancel, progress| {
        do_export_with(b, &path, &out_dir, cancel, conflict, &rules, progress)
    })
    .await
}

/// 导出选中的多个条目（文件与目录可以混在一起）。
#[tauri::command]
pub async fn export_items(
    app: tauri::AppHandle,
    state: tauri::State<'_, AppState>,
    items: Vec<ExportItem>,
    out_dir: String,
    on_conflict: Option<String>,
    skip_rules: Option<String>,
) -> Result<String, String> {
    let conflict = Conflict::parse(on_conflict.as_deref());
    let rules = SkipRules::from_opt(skip_rules.as_deref());
    run_export_task(app, state, "导出选中项", move |b, cancel, progress| {
        do_export_items(b, &items, &out_dir, cancel, conflict, &rules, progress)
    })
    .await
}

/// 导出核心（与 Tauri 解耦，便于直接测试）。默认冲突策略：跳过已存在。
pub fn do_export(b: &mut Backend, path: &str, out_dir: &str) -> Result<String, String> {
    do_export_with(
        b,
        path,
        out_dir,
        &AtomicBool::new(false),
        Conflict::default(),
        &SkipRules::defaults(),
        &mut |_, _| {},
    )
}

pub fn do_export_with(
    b: &mut Backend,
    path: &str,
    out_dir: &str,
    cancel: &AtomicBool,
    conflict: Conflict,
    rules: &SkipRules,
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
            if rules.matches(&e.name, &e.path) {
                st.skipped_rule += 1;
                continue;
            }
            let p = dest.join(&n);
            if e.is_dir {
                export_dir(b, &e.path, &p, &mut st, 1, cancel, conflict, rules, on_progress);
            } else {
                export_file(b, &e, &p, &mut st, cancel, conflict, on_progress);
            }
            if st.cancelled {
                break;
            }
        }
    } else {
        export_file(b, &entry, &dest, &mut st, cancel, conflict, on_progress);
    }

    Ok(export_summary(&st, &dest))
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

    /// 未知的策略字符串必须退回最安全的「跳过」，绝不能猜成覆盖。
    #[test]
    fn conflict_parse_falls_back_to_skip() {
        use super::Conflict;
        assert_eq!(Conflict::parse(None), Conflict::Skip);
        assert_eq!(Conflict::parse(Some("")), Conflict::Skip);
        assert_eq!(Conflict::parse(Some("随便什么")), Conflict::Skip);
        assert_eq!(Conflict::parse(Some("overwrite")), Conflict::Overwrite);
        assert_eq!(Conflict::parse(Some("RENAME")), Conflict::Rename);
        assert_eq!(Conflict::parse(Some(" rename ")), Conflict::Rename);
    }

    /// 重命名要保住扩展名（`a.txt` → `a (1).txt`），
    /// 否则恢复出来的文件双击打不开，还得手动改回来。
    #[test]
    fn unique_sibling_keeps_extension() {
        use super::unique_sibling;
        let dir = std::env::temp_dir().join(format!("dmde-uniq-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();

        let a = dir.join("a.txt");
        std::fs::write(&a, b"x").unwrap();
        let n1 = unique_sibling(&a);
        assert_eq!(n1.file_name().unwrap().to_string_lossy(), "a (1).txt");
        std::fs::write(&n1, b"y").unwrap();
        let n2 = unique_sibling(&a);
        assert_eq!(n2.file_name().unwrap().to_string_lossy(), "a (2).txt");

        // 无扩展名
        let b = dir.join("noext");
        std::fs::write(&b, b"z").unwrap();
        assert_eq!(
            unique_sibling(&b).file_name().unwrap().to_string_lossy(),
            "noext (1)"
        );

        // 隐藏文件不能被当成"扩展名为 gitconfig"
        let h = dir.join(".gitconfig");
        std::fs::write(&h, b"z").unwrap();
        assert_eq!(
            unique_sibling(&h).file_name().unwrap().to_string_lossy(),
            ".gitconfig (1)"
        );

        let _ = std::fs::remove_dir_all(&dir);
    }
}
