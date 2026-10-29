//! NTFS 数据运行（data run）解码。
//!
//! 每个 run 用 1 个头部字节 + 变长字段描述：低 4 位是长度字段字节数，
//! 高 4 位是起始 LCN 偏移字段字节数，偏移是相对上一个 run 的有符号值。

use dmdisk::{Error, Result};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Run {
    /// 起始 VCN（虚拟簇号）
    pub vcn: u64,
    /// 起始 LCN（物理簇号）；稀疏 run 为 None
    pub lcn: Option<u64>,
    /// 簇数
    pub len: u64,
}

impl Run {
    pub fn is_sparse(&self) -> bool {
        self.lcn.is_none()
    }
}

pub fn decode(data: &[u8], start_vcn: u64) -> Result<Vec<Run>> {
    let mut runs = Vec::new();
    let mut i = 0usize;
    let mut vcn = start_vcn;
    let mut prev_lcn: i64 = 0;
    while i < data.len() {
        let header = data[i];
        if header == 0 {
            break;
        }
        i += 1;
        let len_bytes = (header & 0x0F) as usize;
        let off_bytes = (header >> 4) as usize;
        if len_bytes == 0 || len_bytes > 8 || off_bytes > 8 {
            return Err(Error::Format(format!("runlist 头部非法: {header:#04x}")));
        }
        if i + len_bytes + off_bytes > data.len() {
            break;
        }
        let mut len = 0u64;
        for k in 0..len_bytes {
            len |= (data[i + k] as u64) << (8 * k);
        }
        i += len_bytes;

        if off_bytes == 0 {
            // 稀疏 run
            runs.push(Run { vcn, lcn: None, len });
            vcn += len;
            continue;
        }
        let mut delta = 0i64;
        for k in 0..off_bytes {
            delta |= (data[i + k] as i64) << (8 * k);
        }
        // 符号扩展
        let shift = 64 - 8 * off_bytes;
        let delta = (delta << shift) >> shift;
        i += off_bytes;

        let lcn = prev_lcn + delta;
        if lcn < 0 {
            return Err(Error::Format("runlist LCN 为负".into()));
        }
        prev_lcn = lcn;
        runs.push(Run { vcn, lcn: Some(lcn as u64), len });
        vcn += len;
    }
    Ok(runs)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn decodes_single_run() {
        // len=0x21, lcn=0x1000
        let data = [0x21u8, 0x21, 0x00, 0x10, 0x00];
        let runs = decode(&data, 0).unwrap();
        assert_eq!(runs.len(), 1);
        assert_eq!(runs[0].lcn, Some(0x1000));
        assert_eq!(runs[0].len, 0x21);
    }

    #[test]
    fn decodes_relative_second_run() {
        // 第二个 run 的偏移是相对值
        let data = [0x11u8, 0x10, 0x20, 0x11, 0x10, 0x10];
        let runs = decode(&data, 0).unwrap();
        assert_eq!(runs.len(), 2);
        assert_eq!(runs[0].lcn, Some(0x20));
        assert_eq!(runs[1].lcn, Some(0x30));
    }

    #[test]
    fn decodes_sparse_run() {
        let data = [0x01u8, 0x08, 0x11, 0x08, 0x33];
        let runs = decode(&data, 0).unwrap();
        assert_eq!(runs.len(), 2);
        assert!(runs[0].is_sparse());
        assert_eq!(runs[1].lcn, Some(0x33));
    }
}
