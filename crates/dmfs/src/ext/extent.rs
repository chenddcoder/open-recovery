//! ext4 extent 树遍历 + ext2/3 间接块。

use dmdisk::{Error, Result, Reader};

#[derive(Debug, Clone, Copy)]
pub struct Extent {
    /// 逻辑块号
    pub logical: u64,
    /// 物理块号（0 或未写入视为空洞）
    pub physical: u64,
    pub len: u64,
    pub uninit: bool,
}

const EXTENT_MAGIC: u16 = 0xF30A;

/// 遍历一个 extent 节点（叶子或索引），把结果追加到 `out`。
pub fn walk(reader: &Reader, node: &[u8], out: &mut Vec<Extent>, guard: u32, block_size: u32) -> Result<()> {
    if guard > 8 {
        return Err(Error::Format("extent 树深度异常".into()));
    }
    if node.len() < 12 {
        return Ok(());
    }
    let magic = u16::from_le_bytes([node[0], node[1]]);
    if magic != EXTENT_MAGIC {
        return Err(Error::Format(format!("extent 头魔数错误: {magic:#06x}")));
    }
    let entries = u16::from_le_bytes([node[2], node[3]]) as usize;
    let depth = u16::from_le_bytes([node[6], node[7]]) as u32;
    let max_entries = ((node.len() - 12) / 12).min(entries);

    if depth == 0 {
        for i in 0..max_entries {
            let off = 12 + i * 12;
            let logical = u32::from_le_bytes(node[off..off + 4].try_into().unwrap()) as u64;
            let raw_len = u16::from_le_bytes([node[off + 4], node[off + 5]]);
            let uninit = raw_len > 32768;
            let len = if uninit {
                (raw_len - 32768) as u64
            } else {
                raw_len as u64
            };
            let hi = u16::from_le_bytes([node[off + 6], node[off + 7]]) as u64;
            let lo = u32::from_le_bytes(node[off + 8..off + 12].try_into().unwrap()) as u64;
            out.push(Extent {
                logical,
                physical: (hi << 32) | lo,
                len,
                uninit,
            });
        }
    } else {
        for i in 0..max_entries {
            let off = 12 + i * 12;
            let lo = u32::from_le_bytes(node[off + 4..off + 8].try_into().unwrap()) as u64;
            let hi = u16::from_le_bytes([node[off + 8], node[off + 9]]) as u64;
            let child = (hi << 32) | lo;
            let child_off = child * block_size as u64;
            if child_off >= reader.len() {
                continue;
            }
            let buf = reader.read_vec(child_off, block_size as usize)?;
            walk(reader, &buf, out, guard + 1, block_size)?;
        }
    }
    Ok(())
}

/// ext2/3 的块映射：直接块 + 一级 + 二级间接。
pub fn indirect_blocks(
    reader: &Reader,
    i_block: &[u8],
    block_size: u32,
    file_blocks: u64,
    out: &mut Vec<Extent>,
) -> Result<()> {
    let bs = block_size as u64;
    let per_block = bs / 4;
    let read_ptrs = |blk: u64| -> Vec<u32> {
        let mut v = Vec::new();
        if blk == 0 || blk * bs >= reader.len() {
            return v;
        }
        if let Ok(buf) = reader.read_vec(blk * bs, block_size as usize) {
            for c in buf.chunks_exact(4) {
                v.push(u32::from_le_bytes([c[0], c[1], c[2], c[3]]));
            }
        }
        v
    };

    // 12 个直接块
    for i in 0..12usize {
        if (i as u64) >= file_blocks {
            return Ok(());
        }
        let b = u32::from_le_bytes([
            i_block[i * 4],
            i_block[i * 4 + 1],
            i_block[i * 4 + 2],
            i_block[i * 4 + 3],
        ]);
        if b != 0 {
            out.push(Extent { logical: i as u64, physical: b as u64, len: 1, uninit: false });
        }
    }
    // 一级间接
    let single = u32::from_le_bytes([i_block[48], i_block[49], i_block[50], i_block[51]]) as u64;
    let mut logical = 12u64;
    if single != 0 && logical < file_blocks {
        for b in read_ptrs(single) {
            if logical >= file_blocks {
                break;
            }
            if b != 0 {
                out.push(Extent { logical, physical: b as u64, len: 1, uninit: false });
            }
            logical += 1;
        }
    }
    // 二级间接
    let double = u32::from_le_bytes([i_block[52], i_block[53], i_block[54], i_block[55]]) as u64;
    if double != 0 && logical < file_blocks {
        for l1 in read_ptrs(double) {
            if logical >= file_blocks {
                break;
            }
            if l1 == 0 {
                logical += per_block;
                continue;
            }
            for b in read_ptrs(l1 as u64) {
                if logical >= file_blocks {
                    break;
                }
                if b != 0 {
                    out.push(Extent { logical, physical: b as u64, len: 1, uninit: false });
                }
                logical += 1;
            }
        }
    }
    Ok(())
}
