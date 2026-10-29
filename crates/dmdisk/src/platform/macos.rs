//! macOS 磁盘枚举：解析 `diskutil list -plist` 与 `diskutil info -plist`。
//!
//! 不直接上 IOKit——那需要 linking framework 和大量 FFI；`diskutil` 是系统自带的
//! 事实标准接口，输出稳定且不需要提权。

use super::{DiskInfo, PartInfo};
use crate::error::{Error, Result};
use crate::plist::{self, Value};
use std::process::Command;

fn run(args: &[&str]) -> Result<Value> {
    let out = Command::new("diskutil")
        .args(args)
        .output()
        .map_err(|e| Error::Io(e))?;
    if !out.status.success() {
        return Err(Error::Format(format!(
            "diskutil {} 失败: {}",
            args.join(" "),
            String::from_utf8_lossy(&out.stderr).trim()
        )));
    }
    let text = String::from_utf8_lossy(&out.stdout).to_string();
    plist::parse(&text)
}

/// 列出物理盘（过滤掉 APFS 合成盘、磁盘映像等虚拟设备）。
pub fn list_disks() -> Result<Vec<DiskInfo>> {
    let root = run(&["list", "-plist"])?;
    let mut ids: Vec<String> = Vec::new();

    for key in ["AllDisksAndPartitions", "AllDisks"] {
        if let Some(arr) = root.get(key).and_then(|v| v.as_array()) {
            for item in arr {
                match item {
                    Value::Dict(_) => {
                        if let Some(id) = item.get("DeviceIdentifier").and_then(|v| v.as_str()) {
                            ids.push(id.to_string());
                        }
                    }
                    Value::Str(s) => ids.push(s.clone()),
                    _ => {}
                }
            }
        }
    }
    ids.sort();
    ids.dedup();

    let mut out = Vec::new();
    for id in ids {
        // 只处理整盘（disk0 / disk4），分区节点（disk0s1）跳过。
        // 注意不能简单用 contains('s') 判断——"disk" 自己就带 s。
        if !is_whole_disk(&id) {
            continue;
        }
        let info = match run(&["info", "-plist", &id]) {
            Ok(v) => v,
            Err(_) => continue,
        };
        // 注意：Apple Silicon 内置 SSD 这项返回 "Unknown"，只有明确 Virtual 才排除，
        // 否则内置盘会凭空消失。
        let virtual_disk = info
            .get("VirtualOrPhysical")
            .and_then(|v| v.as_str())
            .map(|s| s.eq_ignore_ascii_case("Virtual"))
            .unwrap_or(false);
        if virtual_disk {
            continue;
        }
        let size = info
            .get("TotalSize")
            .or_else(|| info.get("Size"))
            .and_then(|v| v.as_u64())
            .unwrap_or(0);
        if size == 0 {
            continue;
        }
        let model = info
            .get("MediaName")
            .or_else(|| info.get("DeviceModel"))
            .and_then(|v| v.as_str())
            .unwrap_or("未知型号")
            .trim()
            .to_string();
        let disk = DiskInfo {
            dev_node: format!("/dev/r{id}"),
            removable: info
                .get("RemovableMedia")
                .and_then(|v| v.as_bool())
                .unwrap_or(false),
            internal: info.get("Internal").and_then(|v| v.as_bool()).unwrap_or(false),
            bus: info
                .get("BusProtocol")
                .and_then(|v| v.as_str())
                .unwrap_or("")
                .to_string(),
            content: info
                .get("Content")
                .and_then(|v| v.as_str())
                .unwrap_or("")
                .to_string(),
            bsd: id.clone(),
            size,
            model,
            partitions: Vec::new(),
        };
        out.push(disk);
    }
    Ok(out)
}

fn is_whole_disk(id: &str) -> bool {
    let rest = match id.strip_prefix("disk") {
        Some(r) => r,
        None => return false,
    };
    !rest.is_empty() && rest.bytes().all(|b| b.is_ascii_digit())
}

/// 读取某个盘的详细分区列表（用于 `devices -v`）。
pub fn disk_partitions(bsd: &str) -> Result<Vec<PartInfo>> {
    let v = run(&["list", "-plist", bsd])?;
    let mut out = Vec::new();
    let arr = match v.get("AllDisksAndPartitions").and_then(|x| x.as_array()) {
        Some(a) => a,
        None => return Ok(out),
    };
    for disk in arr {
        if let Some(parts) = disk.get("Partitions").and_then(|x| x.as_array()) {
            for p in parts {
                out.push(PartInfo {
                    bsd: p
                        .get("DeviceIdentifier")
                        .and_then(|x| x.as_str())
                        .unwrap_or("")
                        .to_string(),
                    name: p
                        .get("VolumeName")
                        .and_then(|x| x.as_str())
                        .unwrap_or("")
                        .to_string(),
                    size: p.get("Size").and_then(|x| x.as_u64()).unwrap_or(0),
                    content: p
                        .get("Content")
                        .and_then(|x| x.as_str())
                        .unwrap_or("")
                        .to_string(),
                });
            }
        }
    }
    Ok(out)
}
