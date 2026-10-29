//! 前端可调用的命令：全部围绕"当前挂载的卷"这个唯一的会话状态。

use crate::AppState;
use dmdisk::{human_size, Source};
use dmfs::{resolve, sanitize_name, AnyFs, Entry, FileSystem, FsKind};
use serde::Serialize;
use std::fs;
use std::io::Write;
use std::path::{Path, PathBuf};

fn not_open() -> String {
    "还没有打开任何卷".to_string()
}

// ---------------- DTO ----------------

#[derive(Serialize)]
pub struct DiskDto {
    pub dev_node: String,
    pub bsd: String,
    pub size: u64,
    pub size_human: String,
    pub model: String,
    pub bus: String,
    pub internal: bool,
    pub removable: bool,
}

#[derive(Serialize)]
pub struct PartDto {
    pub index: usize,
    pub type_name: String,
    pub size: u64,
    pub size_human: String,
    pub offset: u64,
    pub fs_kind: String,
    pub mountable: bool,
}

#[derive(Serialize)]
pub struct SourceDto {
    pub desc: String,
    pub scheme: String,
    pub verified: bool,
    pub notes: Vec<String>,
    pub partitions: Vec<PartDto>,
    pub fs_kind: String,
    pub label: String,
    pub uuid: String,
    pub block_size: u32,
    pub total_bytes: u64,
    pub total_human: String,
    pub free_bytes: Option<u64>,
    pub details: Vec<(String, String)>,
}

#[derive(Serialize)]
pub struct EntryDto {
    pub name: String,
    pub path: String,
    pub id: u64,
    pub is_dir: bool,
    pub size: u64,
    pub size_human: String,
    pub deleted: bool,
    pub mtime: Option<i64>,
    pub name_lost: bool,
}

#[derive(Serialize)]
pub struct PreviewDto {
    pub text: String,
    pub mode: String,
    pub size: u64,
    pub truncated: bool,
}

#[derive(Serialize)]
pub struct RecoverDto {
    pub ok: u64,
    pub failed: u64,
    pub bytes: u64,
    pub bytes_human: String,
    pub out_dir: String,
    pub saved: Vec<String>,
}

// ---------------- 命令 ----------------

#[tauri::command]
pub fn list_disks() -> Result<Vec<DiskDto>, String> {
    let disks = dmdisk::list_disks().map_err(|e| e.to_string())?;
    Ok(disks
        .into_iter()
        .map(|d| DiskDto {
            size_human: human_size(d.size),
            dev_node: d.dev_node,
            bsd: d.bsd,
            size: d.size,
            model: d.model,
            bus: d.bus,
            internal: d.internal,
            removable: d.removable,
        })
        .collect())
}

/// 打开源；`partition` 为 None 时自动挑选唯一可挂载分区。
#[tauri::command]
pub fn open_source(
    state: tauri::State<AppState>,
    path: String,
    partition: Option<usize>,
) -> Result<SourceDto, String> {
    let src = Source::open(&path, 0).map_err(|e| e.to_string())?;
    let table = src.table().map_err(|e| e.to_string())?;

    let mut partitions = Vec::new();
    for p in &table.partitions {
        let kind = dmfs::probe(&src.volume(p.offset, p.size));
        partitions.push(PartDto {
            index: p.index,
            type_name: p.type_name.clone(),
            size: p.size,
            size_human: human_size(p.size),
            offset: p.offset,
            fs_kind: kind.as_str().to_string(),
            mountable: kind != FsKind::Unknown && kind != FsKind::Apfs,
        });
    }

    let (reader, desc) = match partition {
        Some(idx) => {
            let p = table
                .partitions
                .get(idx.saturating_sub(1))
                .ok_or_else(|| format!("分区 {idx} 不存在"))?;
            (
                src.volume(p.offset, p.size),
                format!("{path} 分区{idx}（{}）", p.type_name),
            )
        }
        None => {
            if dmfs::probe(&src.reader) != FsKind::Unknown {
                (src.reader.slice(0, src.size), path.clone())
            } else {
                let cands: Vec<_> = table
                    .partitions
                    .iter()
                    .filter(|p| {
                        let k = dmfs::probe(&src.volume(p.offset, p.size));
                        k != FsKind::Unknown && k != FsKind::Apfs
                    })
                    .collect();
                match cands.len() {
                    0 => {
                        return Err(format!(
                            "{path} 上没有可挂载的文件系统（分区表 {}）",
                            table.scheme.as_str()
                        ))
                    }
                    1 => {
                        let p = cands[0];
                        (
                            src.volume(p.offset, p.size),
                            format!("{path} 分区{}（{}）", p.index, p.type_name),
                        )
                    }
                    _ => return Err("该源有多个可挂载分区，请先选择分区".to_string()),
                }
            }
        }
    };

    let fs = AnyFs::open(reader).map_err(|e| e.to_string())?;
    let info = fs.info().clone();
    let dto = SourceDto {
        desc: desc.clone(),
        scheme: table.scheme.as_str().to_string(),
        verified: table.verified,
        notes: table.notes.clone(),
        partitions,
        fs_kind: info.kind.as_str().to_string(),
        label: info.label.clone(),
        uuid: info.uuid.clone().unwrap_or_default(),
        block_size: info.block_size,
        total_bytes: info.total_bytes,
        total_human: human_size(info.total_bytes),
        free_bytes: info.free_bytes,
        details: info.details.clone(),
    };

    {
        let mut guard = state.session.lock().map_err(|e| e.to_string())?;
        *guard = Some(crate::Session {
            source: path,
            desc,
            fs,
        });
    }
    Ok(dto)
}

fn entry_dto(base: &str, e: &Entry) -> EntryDto {
    let path = if base == "/" || base.is_empty() {
        format!("/{}", e.name)
    } else {
        format!("{}/{}", base.trim_end_matches('/'), e.name)
    };
    EntryDto {
        name: e.name.clone(),
        path,
        id: e.id.0,
        is_dir: e.is_dir(),
        size: e.size,
        size_human: human_size(e.size),
        deleted: e.deleted,
        mtime: e.mtime,
        name_lost: e.name_lost,
    }
}

#[tauri::command]
pub fn list_dir(
    state: tauri::State<AppState>,
    path: String,
    show_deleted: bool,
) -> Result<Vec<EntryDto>, String> {
    let guard = state.session.lock().map_err(|e| e.to_string())?;
    let s = guard.as_ref().ok_or_else(not_open)?;
    let node = if path == "/" || path.is_empty() {
        s.fs.root()
    } else {
        resolve(&s.fs, &path)
            .map_err(|e| e.to_string())?
            .ok_or_else(|| format!("路径不存在: {path}"))?
            .id
    };
    let mut entries = s.fs.list(node).map_err(|e| e.to_string())?;
    if !show_deleted {
        entries.retain(|e| !e.deleted);
    }
    entries.sort_by_key(|e| (!e.is_dir(), e.name.to_lowercase()));
    let base = if path.is_empty() { "/" } else { &path };
    Ok(entries.iter().map(|e| entry_dto(base, e)).collect())
}

#[tauri::command]
pub fn preview(
    state: tauri::State<AppState>,
    path: String,
    max: Option<usize>,
) -> Result<PreviewDto, String> {
    let limit = max.unwrap_or(256 * 1024);
    let guard = state.session.lock().map_err(|e| e.to_string())?;
    let s = guard.as_ref().ok_or_else(not_open)?;
    let entry = resolve(&s.fs, &path)
        .map_err(|e| e.to_string())?
        .ok_or_else(|| format!("路径不存在: {path}"))?;
    if entry.is_dir() {
        return Err("目录不能预览".into());
    }
    let mut buf = Vec::new();
    s.fs.read(&entry, &mut buf).map_err(|e| e.to_string())?;
    let truncated = buf.len() > limit;
    if truncated {
        buf.truncate(limit);
    }
    let (text, mode) = match String::from_utf8(buf.clone()) {
        Ok(t) => {
            let ctrl = t
                .chars()
                .filter(|c| c.is_control() && *c != '\n' && *c != '\r' && *c != '\t')
                .count();
            if ctrl * 20 > t.chars().count().max(1) {
                (hex_dump(&buf), "hex".to_string())
            } else {
                (t, "text".to_string())
            }
        }
        Err(_) => (hex_dump(&buf), "hex".to_string()),
    };
    Ok(PreviewDto {
        text,
        mode,
        size: entry.size,
        truncated,
    })
}

fn hex_dump(buf: &[u8]) -> String {
    let mut out = String::with_capacity(buf.len() * 4);
    for (i, chunk) in buf.chunks(16).enumerate() {
        out.push_str(&format!("{:08x}  ", i * 16));
        for b in chunk {
            out.push_str(&format!("{b:02x} "));
        }
        for _ in chunk.len()..16 {
            out.push_str("   ");
        }
        out.push_str(" |");
        for b in chunk {
            let c = *b as char;
            out.push(if c.is_ascii_graphic() || c == ' ' { c } else { '.' });
        }
        out.push_str("|\n");
    }
    out
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

#[tauri::command]
pub fn save_file(
    state: tauri::State<AppState>,
    path: String,
    out_dir: String,
) -> Result<String, String> {
    let guard = state.session.lock().map_err(|e| e.to_string())?;
    let s = guard.as_ref().ok_or_else(not_open)?;
    let entry = resolve(&s.fs, &path)
        .map_err(|e| e.to_string())?
        .ok_or_else(|| format!("路径不存在: {path}"))?;
    if entry.is_dir() {
        return Err("暂不支持整目录导出，请逐个文件保存".into());
    }
    fs::create_dir_all(&out_dir).map_err(|e| format!("创建 {out_dir} 失败: {e}"))?;
    let target = unique_path(Path::new(&out_dir), &sanitize_name(&entry.name));
    let file = fs::File::create(&target).map_err(|e| e.to_string())?;
    let mut w = std::io::BufWriter::new(file);
    let n = s.fs.read(&entry, &mut w).map_err(|e| e.to_string())?;
    w.flush().ok();
    Ok(format!("已保存 {}（{} 字节）", target.display(), n))
}

#[tauri::command]
pub fn scan_deleted(
    state: tauri::State<AppState>,
    limit: Option<usize>,
) -> Result<Vec<EntryDto>, String> {
    let guard = state.session.lock().map_err(|e| e.to_string())?;
    let s = guard.as_ref().ok_or_else(not_open)?;
    if !s.fs.supports_deleted() {
        return Err(format!(
            "{} 删除时会销毁目录记录，需要用签名雕刻恢复（规划中）",
            s.fs.info().kind.as_str()
        ));
    }
    let mut items = s.fs.deleted().map_err(|e| e.to_string())?;
    if let Some(l) = limit {
        items.truncate(l);
    }
    Ok(items.iter().map(|e| entry_dto("", e)).collect())
}

#[tauri::command]
pub fn recover_all(
    state: tauri::State<AppState>,
    out_dir: String,
    min_size: u64,
    limit: usize,
) -> Result<RecoverDto, String> {
    let guard = state.session.lock().map_err(|e| e.to_string())?;
    let s = guard.as_ref().ok_or_else(not_open)?;
    fs::create_dir_all(&out_dir).map_err(|e| format!("创建 {out_dir} 失败: {e}"))?;
    let mut items = s.fs.deleted().map_err(|e| e.to_string())?;
    items.retain(|e| e.size >= min_size);
    if limit > 0 {
        items.truncate(limit);
    }
    let mut ok = 0u64;
    let mut failed = 0u64;
    let mut bytes = 0u64;
    let mut saved = Vec::new();
    for e in &items {
        let target = unique_path(Path::new(&out_dir), &sanitize_name(&e.name));
        let file = match fs::File::create(&target) {
            Ok(f) => f,
            Err(_) => {
                failed += 1;
                continue;
            }
        };
        let mut w = std::io::BufWriter::new(file);
        match s.fs.read(e, &mut w) {
            Ok(n) if n > 0 => {
                w.flush().ok();
                ok += 1;
                bytes += n;
                saved.push(target.display().to_string());
            }
            _ => {
                failed += 1;
                let _ = fs::remove_file(&target);
            }
        }
    }
    Ok(RecoverDto {
        ok,
        failed,
        bytes,
        bytes_human: human_size(bytes),
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

#[tauri::command]
pub fn current_source(state: tauri::State<AppState>) -> Result<String, String> {
    let guard = state.session.lock().map_err(|e| e.to_string())?;
    let s = guard.as_ref().ok_or_else(not_open)?;
    let _ = &s.source;
    Ok(s.desc.clone())
}
