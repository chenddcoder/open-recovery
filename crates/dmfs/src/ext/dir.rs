//! ext2/3/4 目录块解析。
//!
//! 两种格式：老格式 name_len 是 u16；启用 filetype 特性后是 name_len(u8)+file_type(u8)。
//! 两种都能按"inode + rec_len"定位下一条，所以可以统一处理。

#[derive(Debug, Clone)]
pub struct ExtDirEntry {
    pub inode: u32,
    pub name: String,
    pub file_type: u8,
}

pub const FT_REG: u8 = 1;
pub const FT_DIR: u8 = 2;
pub const FT_SYMLINK: u8 = 7;

pub fn parse_dir_block(data: &[u8], filetype_feature: bool) -> Vec<ExtDirEntry> {
    let mut out = Vec::new();
    let mut off = 0usize;
    while off + 8 <= data.len() {
        let inode = u32::from_le_bytes([data[off], data[off + 1], data[off + 2], data[off + 3]]);
        let rec_len = u16::from_le_bytes([data[off + 4], data[off + 5]]) as usize;
        if rec_len < 8 || off + rec_len > data.len() {
            break;
        }
        let (name_len, ftype) = if filetype_feature {
            (data[off + 6] as usize, data[off + 7])
        } else {
            (
                u16::from_le_bytes([data[off + 6], data[off + 7]]) as usize,
                0u8,
            )
        };
        if name_len > 0 && name_len <= rec_len.saturating_sub(8) {
            let name = String::from_utf8_lossy(&data[off + 8..off + 8 + name_len]).to_string();
            if name != "." && name != ".." {
                out.push(ExtDirEntry {
                    inode,
                    name,
                    file_type: ftype,
                });
            }
        }
        off += rec_len;
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn entry(inode: u32, name: &str, rec_len: u16, ftype: u8) -> Vec<u8> {
        let mut v = vec![0u8; rec_len as usize];
        v[0..4].copy_from_slice(&inode.to_le_bytes());
        v[4..6].copy_from_slice(&rec_len.to_le_bytes());
        v[6] = name.len() as u8;
        v[7] = ftype;
        v[8..8 + name.len()].copy_from_slice(name.as_bytes());
        v
    }

    #[test]
    fn parses_entries_and_skips_dots() {
        let mut data = entry(2, ".", 12, FT_DIR);
        data.extend(entry(2, "..", 12, FT_DIR));
        data.extend(entry(13, "hello.txt", 1012, FT_REG));
        let e = parse_dir_block(&data, true);
        assert_eq!(e.len(), 1);
        assert_eq!(e[0].name, "hello.txt");
        assert_eq!(e[0].inode, 13);
    }

    #[test]
    fn keeps_orphan_entries_with_zero_inode() {
        let mut data = entry(0, "deleted.bin", 1024, FT_REG);
        data[0] = 0; // inode 已清零
        let e = parse_dir_block(&data, true);
        assert_eq!(e.len(), 1);
        assert_eq!(e[0].name, "deleted.bin");
        assert_eq!(e[0].inode, 0);
    }
}
