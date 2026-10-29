//! Linux 磁盘枚举：直接读 `/sys/block`，无需 udev。

use super::{DiskInfo, PartInfo};
use crate::error::Result;
use std::fs;
use std::path::Path;

fn read_trim(p: &Path) -> String {
    fs::read_to_string(p).unwrap_or_default().trim().to_string()
}

pub fn list_disks() -> Result<Vec<DiskInfo>> {
    let mut out = Vec::new();
    let rd = match fs::read_dir("/sys/block") {
        Ok(r) => r,
        Err(_) => return Ok(out),
    };
    for entry in rd.flatten() {
        let name = entry.file_name().to_string_lossy().to_string();
        // loop/ram/zram/dm 等虚拟设备对恢复无意义
        if name.starts_with("loop")
            || name.starts_with("ram")
            || name.starts_with("zram")
            || name.starts_with("dm-")
            || name.starts_with("sr")
        {
            continue;
        }
        let base = entry.path();
        let sectors: u64 = read_trim(&base.join("size")).parse().unwrap_or(0);
        if sectors == 0 {
            continue;
        }
        let size = sectors * 512;
        let model = read_trim(&base.join("device/model"));
        let removable: bool = read_trim(&base.join("removable")) == "1";

        let mut partitions = Vec::new();
        if let Ok(children) = fs::read_dir(&base) {
            for c in children.flatten() {
                let cname = c.file_name().to_string_lossy().to_string();
                if !cname.starts_with(&name) {
                    continue;
                }
                let secs: u64 = read_trim(&c.path().join("size")).parse().unwrap_or(0);
                if secs == 0 {
                    continue;
                }
                partitions.push(PartInfo {
                    bsd: cname.clone(),
                    name: String::new(),
                    size: secs * 512,
                    content: String::new(),
                });
            }
        }
        partitions.sort_by(|a, b| a.bsd.cmp(&b.bsd));

        out.push(DiskInfo {
            bsd: name.clone(),
            dev_node: format!("/dev/{name}"),
            size,
            model: if model.is_empty() { "未知型号".into() } else { model },
            bus: "".into(),
            internal: !removable,
            removable,
            content: "".into(),
            partitions,
        });
    }
    out.sort_by(|a, b| a.bsd.cmp(&b.bsd));
    Ok(out)
}
