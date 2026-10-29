//! 源解析：把 `<源> + --partition/--offset` 解析成一个可用的文件系统。

use dmdisk::{human_size, Partition, Source};
use dmfs::{probe, AnyFs, FsKind};

pub struct Opened {
    pub fs: AnyFs,
    /// 描述，给用户看的（`/dev/rdisk4s2` 或 `img@2`）
    pub desc: String,
    pub partition: Option<Partition>,
    pub read_only: bool,
}

/// 打开源并按需定位到某个卷，挂载文件系统。
pub fn open_fs(
    source: &str,
    partition: Option<usize>,
    offset: Option<u64>,
    size: u64,
    volume: usize,
) -> Result<Opened, String> {
    let src = Source::open(source, size).map_err(|e| e.to_string())?;
    let read_only = src.read_only;

    // 1) 手动偏移
    if let Some(off) = offset {
        let len = if size > 0 { size } else { src.size.saturating_sub(off) };
        let reader = src.volume(off, len);
        let fs = AnyFs::open_volume(reader, volume).map_err(|e| {
            format!(
                "偏移 {off} 处无法识别文件系统: {e}（先用 `volumes` 看看有哪些分区）"
            )
        })?;
        return Ok(Opened {
            fs,
            desc: format!("{source}@{off}"),
            partition: None,
            read_only,
        });
    }

    // 2) 显式分区号
    if let Some(p) = partition {
        let table = src.table().map_err(|e| e.to_string())?;
        let part = table
            .partitions
            .get(p.saturating_sub(1))
            .ok_or_else(|| format!("分区 {p} 不存在（共 {} 个）", table.partitions.len()))?;
        let reader = src.volume(part.offset, part.size);
        let fs = AnyFs::open_volume(reader, volume).map_err(|e| {
            format!(
                "分区 {p}（{}，{}）挂载失败: {e}",
                part.type_name,
                human_size(part.size)
            )
        })?;
        return Ok(Opened {
            fs,
            desc: format!("{source} 分区{p}"),
            partition: Some(part.clone()),
            read_only,
        });
    }

    // 3) 整盘就是一个文件系统
    if probe(&src.reader) != FsKind::Unknown {
        let fs = AnyFs::open_volume(src.reader, volume).map_err(|e| e.to_string())?;
        return Ok(Opened { fs, desc: source.to_string(), partition: None, read_only });
    }

    // 4) 有分区表：唯一可挂载分区就自动选，多个则提示
    let table = src.table().map_err(|e| e.to_string())?;
    let mut candidates: Vec<(usize, &Partition)> = Vec::new();
    for (i, p) in table.partitions.iter().enumerate() {
        let r = src.volume(p.offset, p.size);
        if probe(&r) != FsKind::Unknown {
            candidates.push((i + 1, p));
        }
    }
    match candidates.len() {
        1 => {
            let (idx, part) = candidates[0];
            let reader = src.volume(part.offset, part.size);
            let fs = AnyFs::open_volume(reader, volume).map_err(|e| e.to_string())?;
            Ok(Opened {
                fs,
                desc: format!("{source} 分区{idx}"),
                partition: Some(part.clone()),
                read_only,
            })
        }
        0 => Err(format!(
            "{} 上没有可识别的文件系统（分区表: {}）",
            source,
            table.scheme.as_str()
        )),
        _ => {
            let mut msg = String::from("该源有多个可挂载分区，请用 -p/--partition 指定：\n");
            for (idx, p) in candidates {
                let kind = probe(&src.volume(p.offset, p.size));
                msg.push_str(&format!(
                    "  -p {idx}  {:<28} {:>10}  {}\n",
                    p.type_name,
                    human_size(p.size),
                    kind.as_str()
                ));
            }
            Err(msg)
        }
    }
}
