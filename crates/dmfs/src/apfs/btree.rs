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

pub struct Tree<'r> {
    pub reader: &'r Reader,
    pub bs: u32,
    /// 定长树的键长度（变长树为 None）
    pub key_len: Option<usize>,
    pub val_len: Option<usize>,
}

impl<'r> Tree<'r> {
    /// 变长键值树（fs-tree 等）
    pub fn variable(reader: &'r Reader, bs: u32) -> Self {
        Self { reader, bs, key_len: None, val_len: None }
    }

    /// 定长键值树（omap：键 16B / 值 16B）
    pub fn fixed(reader: &'r Reader, bs: u32, key_len: usize, val_len: usize) -> Self {
        Self { reader, bs, key_len: Some(key_len), val_len: Some(val_len) }
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
            let idx = child_index(&node, key);
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
                if k.is_empty() || k.as_slice() < start {
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
        let first = child_index_floor(&node, start);
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
fn child_index(node: &BNode, target: &[u8]) -> usize {
    match node
        .entries
        .binary_search_by(|(k, _)| k.as_slice().cmp(target))
    {
        Ok(i) => i,
        Err(0) => 0,
        Err(i) => i - 1,
    }
}

/// 范围扫描起点：命中时从该项开始，未命中时从前一项开始（覆盖间隙）。
fn child_index_floor(node: &BNode, target: &[u8]) -> usize {
    child_index(node, target)
}
