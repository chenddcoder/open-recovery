//! 通用 B-Tree 遍历：点查、范围扫描、节点解析。
//!
//! APFS 里有两类树：
//! * **物理树**（容器 omap、卷 omap）：索引节点的 value 就是子节点的块号；
//! * **虚拟树**（fs-tree）：索引节点的 value 是子节点的 oid，必须先经卷 omap
//!   映射成块号才能读。差异用 [`Resolve`] 抽象掉，同一套遍历代码通吃。

use super::bnode::{BNode, BTreeInfo};
use super::obj;
use dmdisk::{Error, Result, Reader};

/// 递归深度上限：APFS 树高一般 ≤ 5，这里给足余量，主要是防坏节点造成死循环。
const MAX_DEPTH: u32 = 12;
/// 单次扫描最多读多少个节点，防止损坏的树把磁盘读穿。
const MAX_NODES: usize = 2_000_000;

/// oid → 块号。
pub trait Resolve {
    fn node_paddr(&mut self, oid: u64) -> Result<u64>;
}

/// 物理树：oid 就是块号。
pub struct PhysicalOid;

impl Resolve for PhysicalOid {
    fn node_paddr(&mut self, oid: u64) -> Result<u64> {
        Ok(oid)
    }
}

fn u64le(b: &[u8], o: usize) -> u64 {
    u64::from_le_bytes([
        b[o],
        b[o + 1],
        b[o + 2],
        b[o + 3],
        b[o + 4],
        b[o + 5],
        b[o + 6],
        b[o + 7],
    ])
}

/// 键的比较方式。APFS 里两种树的键语义不同，**不能统一按字节序比**：
///
/// * fs-tree 的 `j_key_t` 是**大端** `(obj_id << 60) | type`，字节序比较恰好等于数值比较；
/// * omap 的 `omap_key_t` 是 `{u64 oid; u64 xid}` **小端**，字节序比较会把
///   `oid=1051`（字节 `1b 04`）判成大于 `oid=178945`（字节 `01 bb 02`），
///   于是起始键一跳过，整张表就"查不到对象"。
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum KeyOrder {
    /// 逐字节字典序（适用于大端整数键与变长键）
    Lexicographic,
    /// omap 专用：前 8 字节 oid、后 8 字节 xid，均按小端数值比较
    OmapKey,
    /// fs-tree 专用：`j_key` 是 `(type << 60) | obj_id` 的小端 u64，
    /// 但排序语义是**先 obj_id（低 60 位）、再 type（高 4 位）**，最后比剩余字节。
    JKey,
}

impl KeyOrder {
    pub fn cmp(self, a: &[u8], b: &[u8]) -> std::cmp::Ordering {
        match self {
            KeyOrder::Lexicographic => a.cmp(b),
            KeyOrder::OmapKey => {
                if a.len() >= 16 && b.len() >= 16 {
                    let ao = u64::from_le_bytes(a[0..8].try_into().unwrap());
                    let bo = u64::from_le_bytes(b[0..8].try_into().unwrap());
                    ao.cmp(&bo).then_with(|| {
                        let ax = u64::from_le_bytes(a[8..16].try_into().unwrap());
                        let bx = u64::from_le_bytes(b[8..16].try_into().unwrap());
                        ax.cmp(&bx)
                    })
                } else {
                    a.cmp(b)
                }
            }
            KeyOrder::JKey => {
                if a.len() >= 8 && b.len() >= 8 {
                    let av = u64::from_le_bytes(a[0..8].try_into().unwrap());
                    let bv = u64::from_le_bytes(b[0..8].try_into().unwrap());
                    // 低 60 位是 obj_id，高 4 位是类型
                    let aid = av & 0x0fff_ffff_ffff_ffff;
                    let bid = bv & 0x0fff_ffff_ffff_ffff;
                    aid.cmp(&bid)
                        .then_with(|| (av >> 60).cmp(&(bv >> 60)))
                        .then_with(|| a[8..].cmp(&b[8..]))
                } else {
                    a.cmp(b)
                }
            }
        }
    }
}

pub struct Tree<'r> {
    pub reader: &'r Reader,
    pub bs: u32,
    /// 定长树的键长度（变长树为 None）
    pub key_len: Option<usize>,
    pub val_len: Option<usize>,
    pub order: KeyOrder,
}

impl<'r> Tree<'r> {
    /// 变长键值树（fs-tree 等）
    pub fn variable(reader: &'r Reader, bs: u32) -> Self {
        Self { reader, bs, key_len: None, val_len: None, order: KeyOrder::Lexicographic }
    }

    /// fs-tree：键是比较语义特殊的 `j_key`（见 [`KeyOrder::JKey`]）。
    pub fn fs_tree(reader: &'r Reader, bs: u32) -> Self {
        Self { reader, bs, key_len: None, val_len: None, order: KeyOrder::JKey }
    }

    /// 定长键值树（omap：键 16B / 值 16B）
    pub fn fixed(reader: &'r Reader, bs: u32, key_len: usize, val_len: usize) -> Self {
        Self {
            reader,
            bs,
            key_len: Some(key_len),
            val_len: Some(val_len),
            order: KeyOrder::Lexicographic,
        }
    }

    /// 定长 omap 树：键按 `(oid, xid)` 小端数值比较。
    pub fn fixed_omap(reader: &'r Reader, bs: u32, key_len: usize, val_len: usize) -> Self {
        Self {
            reader,
            bs,
            key_len: Some(key_len),
            val_len: Some(val_len),
            order: KeyOrder::OmapKey,
        }
    }

    pub fn node(&self, paddr: u64) -> Result<BNode> {
        let buf = obj::read_block(self.reader, self.bs, paddr)?;
        BNode::parse(paddr, &buf, self.key_len, self.val_len)
    }

    /// 读根节点的 `btree_info_t`，用来确认树类型（定长/物理/允许 ghost）。
    pub fn root_info(&self, paddr: u64) -> Result<Option<BTreeInfo>> {
        let buf = obj::read_block(self.reader, self.bs, paddr)?;
        let flags = u16::from_le_bytes([buf[0x20], buf[0x21]]);
        let is_root = flags & super::bnode::BTNODE_ROOT != 0;
        Ok(BNode::btree_info(&buf, is_root))
    }

    /// 点查：返回与 `key` 完全相等的值。
    /// `root_paddr` 必须是**已解析的物理块号**（调用方通过 omap 解好），
    /// 本函数只负责往下层解索引项里的 oid。
    pub fn lookup<R: Resolve>(
        &self,
        root_paddr: u64,
        res: &mut R,
        key: &[u8],
    ) -> Result<Option<Vec<u8>>> {
        let mut paddr = root_paddr;
        let mut depth = 0;
        loop {
            if depth > MAX_DEPTH {
                return Err(Error::Format("APFS B-Tree 层级过深".into()));
            }
            depth += 1;
            let node = self.node(paddr)?;
            if node.is_leaf() {
                return Ok(node
                    .entries
                    .into_iter()
                    .find(|(k, _)| k.as_slice() == key)
                    .map(|(_, v)| v));
            }
            let idx = child_index(&node, key, self.order);
            paddr = res.node_paddr(u64le(&node.entries[idx].1, 0))?;
        }
    }

    /// 范围扫描：从 `start`（含）之后按升序回调，`cb` 返回 false 即停止。
    /// 返回实际回调的条目数。
    pub fn scan<R: Resolve>(
        &self,
        root_paddr: u64,
        res: &mut R,
        start: &[u8],
        cb: &mut dyn FnMut(&[u8], &[u8]) -> bool,
    ) -> Result<usize> {
        let mut state = ScanState { budget: MAX_NODES, hits: 0 };
        self.scan_node(root_paddr, res, start, cb, &mut state, 0)?;
        Ok(state.hits)
    }

    fn scan_node<R: Resolve>(
        &self,
        paddr: u64,
        res: &mut R,
        start: &[u8],
        cb: &mut dyn FnMut(&[u8], &[u8]) -> bool,
        st: &mut ScanState,
        depth: u32,
    ) -> Result<bool> {
        if depth > MAX_DEPTH {
            return Err(Error::Format("APFS B-Tree 层级过深".into()));
        }
        if st.budget == 0 {
            return Ok(false);
        }
        st.budget -= 1;

        let node = self.node(paddr)?;
        if node.is_leaf() {
            for (k, v) in &node.entries {
                // ghost 记录（键长 0）是删除标记，不参与结果
                if k.is_empty() || self.order.cmp(k, start).is_lt() {
                    continue;
                }
                st.hits += 1;
                if !cb(k, v) {
                    return Ok(false);
                }
            }
            return Ok(true);
        }

        // 可能包含 start 的子节点：最后一个「首键 ≤ start」的项。
        // 再往前一个也无害（子节点内会再按 start 过滤），但不能跳过它，
        // 因为索引键存的是子节点首键，start 可能落在两键之间的空隙里。
        let first = child_index_floor(&node, start, self.order);
        for entry in node.entries.iter().skip(first) {
            let child = u64le(&entry.1, 0);
            let cp = res.node_paddr(child)?;
            if !self.scan_node(cp, res, start, cb, st, depth + 1)? {
                return Ok(false);
            }
        }
        Ok(true)
    }
}

struct ScanState {
    budget: usize,
    hits: usize,
}

/// 索引节点下降：最后一个键 ≤ target 的项（二分）。
fn child_index(node: &BNode, target: &[u8], order: KeyOrder) -> usize {
    match node
        .entries
        .binary_search_by(|(k, _)| order.cmp(k, target))
    {
        Ok(i) => i,
        Err(0) => 0,
        Err(i) => i - 1,
    }
}

/// 范围扫描起点：命中时从该项开始，未命中时从前一项开始（覆盖间隙）。
fn child_index_floor(node: &BNode, target: &[u8], order: KeyOrder) -> usize {
    child_index(node, target, order)
}

#[cfg(test)]
mod tests {
    use super::KeyOrder;
    use std::cmp::Ordering;

    fn be(v: u64) -> Vec<u8> {
        v.to_be_bytes().to_vec()
    }

    /// 这两组比较曾经都是"字节序比一遍"了事，小镜像碰巧正确，
    /// 真盘上 obj_id / oid 一跨进位边界就整棵树的查询错位。
    #[test]
    fn omap_key_compares_numerically_not_by_bytes() {
        let k = KeyOrder::OmapKey;
        let a = {
            let mut v = 1051u64.to_le_bytes().to_vec();
            v.extend_from_slice(&1u64.to_le_bytes());
            v
        };
        let b = {
            let mut v = 178_945u64.to_le_bytes().to_vec();
            v.extend_from_slice(&0u64.to_le_bytes());
            v
        };
        assert_eq!(k.cmp(&a, &b), Ordering::Less, "1051 必须小于 178945");
        // 字节序下首字节 0x1b > 0x01，会得出相反结论
        assert_eq!(a.cmp(&b), Ordering::Greater, "这正是原来的 bug");
    }

    #[test]
    fn omap_key_orders_by_xid_within_same_oid() {
        let k = KeyOrder::OmapKey;
        let mut a = 7u64.to_le_bytes().to_vec();
        a.extend_from_slice(&1u64.to_le_bytes());
        let mut b = 7u64.to_le_bytes().to_vec();
        b.extend_from_slice(&2u64.to_le_bytes());
        assert_eq!(k.cmp(&a, &b), Ordering::Less);
    }

    #[test]
    fn jkey_orders_by_obj_id_across_boundaries() {
        let k = KeyOrder::JKey;
        // (type<<60)|obj_id，小端存储
        let key = |obj: u64, ty: u64| ((ty << 60) | obj).to_le_bytes().to_vec();
        assert_eq!(k.cmp(&key(255, 9), &key(256, 9)), Ordering::Less, "跨 256 边界");
        assert_eq!(k.cmp(&key(65535, 9), &key(65536, 9)), Ordering::Less, "跨 65536 边界");
        assert_eq!(k.cmp(&key(2, 9), &key(3, 9)), Ordering::Less);
    }

    #[test]
    fn jkey_orders_by_type_within_same_obj() {
        let k = KeyOrder::JKey;
        let key = |obj: u64, ty: u64| ((ty << 60) | obj).to_le_bytes().to_vec();
        assert_eq!(k.cmp(&key(5, 3), &key(5, 9)), Ordering::Less, "同 id 下 inode(3) 在 drec(9) 前");
    }

    #[test]
    fn jkey_falls_back_to_tail_bytes() {
        let k = KeyOrder::JKey;
        let mut a = ((9u64 << 60) | 5).to_le_bytes().to_vec();
        a.extend_from_slice(b"abc");
        let mut b = ((9u64 << 60) | 5).to_le_bytes().to_vec();
        b.extend_from_slice(b"abd");
        assert_eq!(k.cmp(&a, &b), Ordering::Less);
    }

    #[test]
    fn lexicographic_still_used_for_big_endian() {
        let k = KeyOrder::Lexicographic;
        assert_eq!(k.cmp(&be(1), &be(2)), Ordering::Less);
    }
}
