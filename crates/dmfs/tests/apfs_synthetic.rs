//! 合成 APFS 容器端到端测试。
//!
//! 为什么要自己造镜像：`hdiutil`/`newfs_apfs` 造不出"多卷 + 大小写敏感 + 指定
//! xid"这类组合，而这几项恰好是解析器最容易写错的地方。这里按 APFS 规范逐字节
//! 构造一个 64 块（256 KiB）的最小容器，覆盖：
//!
//! * 容器 omap（物理树、定长键值）→ APSB 的对象映射链
//! * 两个卷（验证 `--volume` 选卷，且卷 1 用 plain drec 键格式）
//! * fs-tree 的 DIR_REC / INODE / FILE_EXTENT 三种记录
//! * inode 的 xfields（NAME + DSTREAM），以及 92 字节头部的实际布局
//! * Fletcher64 校验和（写完后必须自校验通过）

use dmfs::apfs::{self, bnode, obj, record};
use dmfs::FileSystem;
use std::io::Write;
use std::path::PathBuf;

const BS: usize = 4096;
const NBLK: u64 = 64;

const B_NX: u64 = 0;
const B_COMAP: u64 = 4;
const B_COMAP_TREE: u64 = 5;
const B_V0_OMAP: u64 = 8;
const B_V0_TREE: u64 = 9;
const B_V0_APSB: u64 = 20;
const B_V1_OMAP: u64 = 22;
const B_V1_TREE: u64 = 23;
const B_V1_APSB: u64 = 24;
const B_FSTREE0: u64 = 30;
const B_FSTREE1: u64 = 31;
const B_DATA0: u64 = 40;
const B_DATA1: u64 = 41;

const OID_V0: u64 = 7000;
const OID_V1: u64 = 7001;
const OID_ROOT0: u64 = 7100;
const OID_ROOT1: u64 = 7101;

const CONTENT0: &str = "hello apfs\n";
const CONTENT1: &str = "plain\n";
const CONTENT_V1: &str = "second volume\n";

// ---------------- 字节级工具 ----------------

fn put_u16(b: &mut [u8], o: usize, v: u16) {
    b[o..o + 2].copy_from_slice(&v.to_le_bytes());
}

fn put_u32(b: &mut [u8], o: usize, v: u32) {
    b[o..o + 4].copy_from_slice(&v.to_le_bytes());
}

fn put_u64(b: &mut [u8], o: usize, v: u64) {
    b[o..o + 8].copy_from_slice(&v.to_le_bytes());
}

fn align8(n: usize) -> usize {
    (n + 7) & !7
}

/// 对象头（32 字节），校验和最后统一回填。
fn obj_header(oid: u64, xid: u64, otype: u32, subtype: u32) -> Vec<u8> {
    let mut b = vec![0u8; 32];
    put_u64(&mut b, 8, oid);
    put_u64(&mut b, 16, xid);
    put_u32(&mut b, 24, otype);
    put_u32(&mut b, 28, subtype);
    b
}

/// 回填 Fletcher64 校验和。
fn seal(block: &mut [u8]) {
    let ck = obj::checksum(block);
    put_u64(block, 0, ck);
}

struct Img {
    blocks: Vec<Vec<u8>>,
}

impl Img {
    fn new() -> Self {
        Self { blocks: vec![vec![0u8; BS]; NBLK as usize] }
    }

    fn put(&mut self, n: u64, data: &[u8]) {
        let d = &mut self.blocks[n as usize];
        d[..data.len().min(BS)].copy_from_slice(&data[..data.len().min(BS)]);
    }

    fn write(&self, name: &str) -> PathBuf {
        let path = std::env::temp_dir().join(format!("dmde-apfs-{name}.img"));
        let mut f = std::fs::File::create(&path).unwrap();
        for b in &self.blocks {
            f.write_all(b).unwrap();
        }
        f.flush().unwrap();
        path
    }
}

// ---------------- 各类结构 ----------------

fn nx_superblock(xid: u64, omap_oid: u64, fs_oids: &[u64]) -> Vec<u8> {
    let mut b = vec![0u8; BS];
    b[..32].copy_from_slice(&obj_header(1, xid, 0x8000_0001, 0));
    b[0x20..0x24].copy_from_slice(obj::NX_MAGIC);
    put_u32(&mut b, 0x24, BS as u32);
    put_u64(&mut b, 0x28, NBLK);
    for (i, x) in b[0x48..0x58].iter_mut().enumerate() {
        *x = (0xA0 + i) as u8;
    }
    put_u32(&mut b, 0x68, 8); // nx_xp_desc_blocks
    put_u64(&mut b, 0x70, 1); // nx_xp_desc_base
    put_u64(&mut b, 0xa0, omap_oid);
    for (i, oid) in fs_oids.iter().enumerate() {
        put_u64(&mut b, 0xb8 + i * 8, *oid);
    }
    seal(&mut b);
    b
}

fn omap_obj(oid: u64, xid: u64, tree_paddr: u64) -> Vec<u8> {
    let mut b = vec![0u8; BS];
    b[..32].copy_from_slice(&obj_header(oid, xid, obj::OBJ_FLAG_PHYSICAL | obj::OBJ_OMAP, 0));
    put_u32(&mut b, 0x28, obj::OBJ_FLAG_PHYSICAL | obj::OBJ_BTREE);
    put_u64(&mut b, 0x30, tree_paddr);
    seal(&mut b);
    b
}

/// 单节点（root=leaf）B-Tree。
///
/// 值区从「块尾 − 40（btree_info）」往回长，`v` 存的是「值末尾相对值区末尾」的偏移。
fn btree_root(
    oid: u64,
    xid: u64,
    etype: u32,
    fixed: bool,
    entries: &[(Vec<u8>, Vec<u8>)],
) -> Vec<u8> {
    let mut b = vec![0u8; BS];
    b[..32].copy_from_slice(&obj_header(oid, xid, obj::OBJ_FLAG_PHYSICAL | etype, 0));
    let mut flags = bnode::BTNODE_ROOT | bnode::BTNODE_LEAF;
    if fixed {
        flags |= bnode::BTNODE_FIXED_KV;
    }
    put_u16(&mut b, 0x20, flags);
    put_u16(&mut b, 0x22, 0); // level
    put_u32(&mut b, 0x24, entries.len() as u32);
    let step = if fixed { 4 } else { 8 };
    let tlen = entries.len() * step;
    put_u16(&mut b, 0x28, 0); // table_space.off
    put_u16(&mut b, 0x2a, tlen as u16);

    let key_base = 0x38 + tlen;
    let val_base = BS - bnode::BTREE_INFO_SIZE;

    let mut koff = 0usize;
    let mut cum = 0usize;
    for (i, (k, v)) in entries.iter().enumerate() {
        let v_off = cum + v.len();
        let t = 0x38 + i * step;
        if fixed {
            put_u16(&mut b, t, koff as u16);
            put_u16(&mut b, t + 2, v_off as u16);
        } else {
            put_u16(&mut b, t, koff as u16);
            put_u16(&mut b, t + 2, k.len() as u16);
            put_u16(&mut b, t + 4, v_off as u16);
            put_u16(&mut b, t + 6, v.len() as u16);
        }
        b[key_base + koff..key_base + koff + k.len()].copy_from_slice(k);
        let vs = val_base - v_off;
        b[vs..vs + v.len()].copy_from_slice(v);
        koff += align8(k.len());
        cum += v.len();
    }
    assert!(key_base + koff <= val_base - cum, "合成节点装不下");

    // btree_info_t（40 字节）
    let f = BS - bnode::BTREE_INFO_SIZE;
    let info_flags = if fixed {
        bnode::BTREE_PHYSICAL | bnode::BTREE_SEQUENTIAL_INSERT
    } else {
        0
    };
    put_u32(&mut b, f, info_flags);
    put_u32(&mut b, f + 4, BS as u32);
    let (ks, vs) = if fixed {
        (entries.first().map(|(k, _)| k.len()).unwrap_or(0), entries.first().map(|(_, v)| v.len()).unwrap_or(0))
    } else {
        (0, 0)
    };
    put_u32(&mut b, f + 8, ks as u32);
    put_u32(&mut b, f + 12, vs as u32);
    put_u64(&mut b, f + 24, entries.len() as u64);
    put_u64(&mut b, f + 32, 1);
    seal(&mut b);
    b
}

fn apsb(
    oid: u64,
    xid: u64,
    name: &str,
    incompat: u64,
    omap_paddr: u64,
    root_tree_oid: u64,
    files: u64,
    dirs: u64,
) -> Vec<u8> {
    let mut b = vec![0u8; BS];
    b[..32].copy_from_slice(&obj_header(oid, xid, obj::OBJ_FS, 0x4000_0002));
    b[0x20..0x24].copy_from_slice(obj::APSB_MAGIC);
    put_u64(&mut b, 0x28, 0x2); // features
    put_u64(&mut b, 0x38, incompat);
    put_u64(&mut b, 0x58, 32); // alloc_count
    put_u32(&mut b, 0x74, obj::OBJ_FLAG_PHYSICAL | obj::OBJ_BTREE); // root_tree_type
    put_u64(&mut b, 0x80, omap_paddr);
    put_u64(&mut b, 0x88, root_tree_oid);
    put_u64(&mut b, 0xb0, 9000); // next_obj_id
    put_u64(&mut b, 0xb8, files);
    put_u64(&mut b, 0xc0, dirs);
    for (i, x) in b[0xf0..0x100].iter_mut().enumerate() {
        *x = 0x30 + i as u8;
    }
    put_u64(&mut b, 0x108, 1); // APFS_FS_UNENCRYPTED
    let nb = name.as_bytes();
    b[0x2c0..0x2c0 + nb.len()].copy_from_slice(nb);
    seal(&mut b);
    b
}

struct Ino {
    id: u64,
    parent: u64,
    mode: u16,
    nlink: u32,
    size: u64,
    name: Option<&'static str>,
}

fn inode_val(i: &Ino) -> Vec<u8> {
    // 实测 macOS 26 的 xfields 起点是 92（apfs.h 写的是 86，多 6 字节填充）
    let mut v = vec![0u8; 92];
    put_u64(&mut v, 0, i.parent);
    put_u64(&mut v, 8, i.id);
    put_u64(&mut v, 16, 1_793_000_000_000_000_000);
    put_u64(&mut v, 24, 1_793_000_001_000_000_000);
    put_u64(&mut v, 32, 1_793_000_001_000_000_000);
    put_u64(&mut v, 40, 1_793_000_001_000_000_000);
    put_u32(&mut v, 56, i.nlink);
    put_u32(&mut v, 72, 501);
    put_u32(&mut v, 76, 20);
    put_u16(&mut v, 80, i.mode);

    let mut fields: Vec<(u8, Vec<u8>)> = Vec::new();
    if let Some(nm) = i.name {
        let mut d = nm.as_bytes().to_vec();
        d.push(0);
        fields.push((record::INO_EXT_TYPE_NAME, d));
    }
    if i.size > 0 {
        let mut d = vec![0u8; 40];
        put_u64(&mut d, 0, i.size);
        put_u64(&mut d, 8, align8(i.size as usize) as u64);
        put_u64(&mut d, 16, 0);
        put_u64(&mut d, 24, i.size);
        fields.push((record::INO_EXT_TYPE_DSTREAM, d));
    }

    let n = fields.len();
    let mut xf = Vec::new();
    xf.extend_from_slice(&(n as u16).to_le_bytes());
    // used_data（位图）+ 奇偶填充
    xf.extend(std::iter::repeat(0u8).take(n + (n & 1)));
    for (t, d) in &fields {
        xf.push(*t);
        xf.push(0);
        xf.extend_from_slice(&(d.len() as u16).to_le_bytes());
    }
    let mut data = Vec::new();
    for (_, d) in &fields {
        data.extend_from_slice(d);
        while data.len() % 8 != 0 {
            data.push(0);
        }
    }
    xf.extend_from_slice(&data);
    v.extend_from_slice(&xf);
    v
}

fn drec_key(parent: u64, name: &str, hashed: bool) -> Vec<u8> {
    let hdr = ((record::T_DIR_REC as u64) << 60) | parent;
    let n = name.len() + 1;
    let mut k = hdr.to_le_bytes().to_vec();
    if hashed {
        // 低 10 位是名字长度，高位是名字哈希
        let mixed = (n as u32) | (0x2A_5C31 << 10);
        k.extend_from_slice(&mixed.to_le_bytes());
    } else {
        k.extend_from_slice(&(n as u16).to_le_bytes());
    }
    k.extend_from_slice(name.as_bytes());
    k.push(0);
    k
}

fn drec_val(file_id: u64, flags: u16) -> Vec<u8> {
    let mut v = vec![0u8; 18];
    put_u64(&mut v, 0, file_id);
    put_u64(&mut v, 8, 1_793_000_002_000_000_000);
    put_u16(&mut v, 16, flags);
    v
}

fn extent_key(id: u64, logical: u64) -> Vec<u8> {
    let hdr = ((record::T_FILE_EXTENT as u64) << 60) | id;
    let mut k = hdr.to_le_bytes().to_vec();
    k.extend_from_slice(&logical.to_le_bytes());
    k
}

fn extent_val(len: u64, phys: u64) -> Vec<u8> {
    let mut v = vec![0u8; 24];
    put_u64(&mut v, 0, len);
    put_u64(&mut v, 8, phys);
    v
}

fn inode_key(id: u64) -> Vec<u8> {
    record::make_key(id, record::T_INODE)
}

/// 卷 0 的 fs-tree：/hello.txt 与 /sub/Plain.TXT
fn fs_tree0() -> Vec<u8> {
    let e = vec![
        (drec_key(1, "root", true), drec_val(2, 4)),
        (
            inode_key(2),
            inode_val(&Ino { id: 2, parent: 1, mode: 0o40755, nlink: 3, size: 0, name: Some("root") }),
        ),
        (drec_key(2, "gap.bin", true), drec_val(6, 8)),
        (drec_key(2, "hello.txt", true), drec_val(3, 8)),
        (drec_key(2, "hole.bin", true), drec_val(7, 8)),
        (drec_key(2, "sub", true), drec_val(4, 4)),
        (
            inode_key(3),
            inode_val(&Ino { id: 3, parent: 2, mode: 0o100644, nlink: 1, size: CONTENT0.len() as u64, name: Some("hello.txt") }),
        ),
        (extent_key(3, 0), extent_val(CONTENT0.len() as u64, B_DATA0)),
        (
            inode_key(4),
            inode_val(&Ino { id: 4, parent: 2, mode: 0o40755, nlink: 2, size: 0, name: Some("sub") }),
        ),
        (drec_key(4, "Plain.TXT", true), drec_val(5, 8)),
        (
            inode_key(5),
            inode_val(&Ino { id: 5, parent: 4, mode: 0o100644, nlink: 1, size: CONTENT1.len() as u64, name: Some("Plain.TXT") }),
        ),
        (extent_key(5, 0), extent_val(CONTENT1.len() as u64, B_DATA1)),
        // 6: 两段数据之间有 4 字节缝隙（没有 extent 覆盖）
        (
            inode_key(6),
            inode_val(&Ino { id: 6, parent: 2, mode: 0o100644, nlink: 1, size: 14, name: Some("gap.bin") }),
        ),
        (extent_key(6, 0), extent_val(4, B_DATA0)),
        (extent_key(6, 8), extent_val(6, B_DATA1)),
        // 7: 开头是一段 phys=0 的空洞
        (
            inode_key(7),
            inode_val(&Ino { id: 7, parent: 2, mode: 0o100644, nlink: 1, size: 10, name: Some("hole.bin") }),
        ),
        (extent_key(7, 0), extent_val(4, 0)),
        (extent_key(7, 4), extent_val(6, B_DATA1)),
    ];
    btree_root(OID_ROOT0, 5, obj::OBJ_BTREE, false, &e)
}

/// 卷 1 的 fs-tree：只有一个 /only.txt，且用 plain drec 键
fn fs_tree1() -> Vec<u8> {
    let e = vec![
        (drec_key(1, "root", false), drec_val(2, 4)),
        (
            inode_key(2),
            inode_val(&Ino { id: 2, parent: 1, mode: 0o40755, nlink: 2, size: 0, name: Some("root") }),
        ),
        (drec_key(2, "only.txt", false), drec_val(3, 8)),
        (
            inode_key(3),
            inode_val(&Ino { id: 3, parent: 2, mode: 0o100644, nlink: 1, size: CONTENT_V1.len() as u64, name: Some("only.txt") }),
        ),
        (extent_key(3, 0), extent_val(CONTENT_V1.len() as u64, B_DATA1 + 1)),
    ];
    btree_root(OID_ROOT1, 5, obj::OBJ_BTREE, false, &e)
}

fn build() -> Img {
    let mut img = Img::new();
    img.put(B_NX, &nx_superblock(5, B_COMAP, &[OID_V0, OID_V1]));
    img.put(B_COMAP, &omap_obj(500, 5, B_COMAP_TREE));
    img.put(
        B_COMAP_TREE,
        &btree_root(
            500,
            5,
            obj::OBJ_BTREE,
            true,
            &[
                (omap_key(OID_V0, 5), omap_val(B_V0_APSB)),
                (omap_key(OID_V1, 5), omap_val(B_V1_APSB)),
            ],
        ),
    );

    // 卷 0：大小写不敏感（incompat = 1）→ hashed drec
    img.put(B_V0_APSB, &apsb(OID_V0, 5, "APFSVOL", 1, B_V0_OMAP, OID_ROOT0, 4, 2));
    img.put(B_V0_OMAP, &omap_obj(B_V0_OMAP, 5, B_V0_TREE));
    img.put(
        B_V0_TREE,
        &btree_root(B_V0_OMAP, 5, obj::OBJ_BTREE, true, &[(omap_key(OID_ROOT0, 5), omap_val(B_FSTREE0))]),
    );
    img.put(B_FSTREE0, &fs_tree0());

    // 卷 1：大小写敏感（incompat = 0）→ plain drec
    img.put(B_V1_APSB, &apsb(OID_V1, 5, "Second", 0, B_V1_OMAP, OID_ROOT1, 1, 1));
    img.put(B_V1_OMAP, &omap_obj(B_V1_OMAP, 5, B_V1_TREE));
    img.put(
        B_V1_TREE,
        &btree_root(B_V1_OMAP, 5, obj::OBJ_BTREE, true, &[(omap_key(OID_ROOT1, 5), omap_val(B_FSTREE1))]),
    );
    img.put(B_FSTREE1, &fs_tree1());

    img.put(B_DATA0, CONTENT0.as_bytes());
    img.put(B_DATA1, CONTENT1.as_bytes());
    img.put(B_DATA1 + 1, CONTENT_V1.as_bytes());
    img
}

fn omap_key(oid: u64, xid: u64) -> Vec<u8> {
    let mut k = Vec::with_capacity(16);
    k.extend_from_slice(&oid.to_le_bytes());
    k.extend_from_slice(&xid.to_le_bytes());
    k
}

fn omap_val(paddr: u64) -> Vec<u8> {
    let mut v = vec![0u8; 16];
    put_u32(&mut v, 4, BS as u32);
    put_u64(&mut v, 8, paddr);
    v
}

fn open(path: &std::path::Path) -> dmdisk::Reader {
    let dev = dmdisk::open_source(path.to_str().unwrap(), 0).unwrap();
    dmdisk::Reader::new(dev)
}

// ---------------- 测试 ----------------

#[test]
fn container_and_checksums() {
    let path = build().write("container");
    let r = open(&path);

    let c = apfs::Container::open(&r).unwrap();
    assert_eq!(c.bs, BS as u32);
    assert_eq!(c.block_count, NBLK);
    assert_eq!(c.xid, 5, "应选 xid 最大的超级块");
    assert_eq!(c.fs_oids, vec![OID_V0, OID_V1], "两个卷都应被发现");
    assert_eq!(c.cksum_bad, 0);

    // 校验和自洽：整块 Fletcher 和为 0
    for b in [B_NX, B_COMAP, B_COMAP_TREE, B_V0_APSB, B_FSTREE0] {
        let buf = obj::read_block(&r, BS as u32, b).unwrap();
        assert_eq!(obj::checksum(&buf), obj::parse_header(&buf).unwrap().cksum, "块 {b} 校验和不符");
        assert!(obj::parse_header(&buf).unwrap().cksum_ok, "块 {b} 自校验失败");
    }
}

#[test]
fn volume_selection_and_names() {
    let path = build().write("vols");
    let r = open(&path);
    let c = apfs::Container::open(&r).unwrap();

    let v0 = c.volume(&r, 0).unwrap();
    assert_eq!(v0.name, "APFSVOL");
    assert!(v0.case_insensitive());
    assert!(v0.hashed_drec());

    let v1 = c.volume(&r, 1).unwrap();
    assert_eq!(v1.name, "Second");
    assert!(!v1.case_insensitive());
    assert!(!v1.hashed_drec(), "incompat=0 的卷应使用 plain drec 键");

    assert!(c.volume(&r, 2).is_err(), "越界卷号应报错");
}

#[test]
fn browse_and_read_files() {
    let path = build().write("browse");
    let fs = apfs::open_volume(open(&path), 0).unwrap();
    assert_eq!(fs.info().kind, dmfs::FsKind::Apfs);
    assert_eq!(fs.info().label, "APFSVOL");
    assert_eq!(fs.info().block_size, BS as u32);

    let root = fs.root();
    let entries = fs.list(root).unwrap();
    let names: Vec<&str> = entries.iter().map(|e| e.name.as_str()).collect();
    assert_eq!(
        names,
        vec!["sub", "gap.bin", "hello.txt", "hole.bin"],
        "目录应排在前，且名字正确：{names:?}"
    );

    let by_name = |n: &str| entries.iter().find(|e| e.name == n).cloned().unwrap();
    let hello = &by_name("hello.txt");
    assert_eq!(hello.size, CONTENT0.len() as u64);
    assert_eq!(hello.kind, dmfs::NodeKind::File);
    assert!(hello.mtime.is_some(), "纳秒时间戳应换算成 Unix 秒");

    let mut buf = Vec::new();
    let n = fs.read(hello, &mut buf).unwrap();
    assert_eq!(n, CONTENT0.len() as u64);
    assert_eq!(String::from_utf8(buf).unwrap(), CONTENT0);

    // 路径解析 + 二级目录里的文件
    let e = dmfs::resolve(&fs, "/sub/Plain.TXT").unwrap().expect("应能找到 /sub/Plain.TXT");
    let mut buf = Vec::new();
    fs.read(&e, &mut buf).unwrap();
    assert_eq!(String::from_utf8(buf).unwrap(), CONTENT1);
}

#[test]
fn sparse_gaps_and_holes_are_zero_filled() {
    let path = build().write("sparse");
    let fs = apfs::open_volume(open(&path), 0).unwrap();
    let entries = fs.list(fs.root()).unwrap();
    let pick = |n: &str| entries.iter().find(|e| e.name == n).cloned().unwrap();

    // 缝隙：0..4 有数据，4..8 无 extent，8..14 有数据 → 中间必须是 0
    let gap = pick("gap.bin");
    let mut buf = Vec::new();
    let n = fs.read(&gap, &mut buf).unwrap();
    assert_eq!(n, 14);
    assert_eq!(&buf[..4], &CONTENT0.as_bytes()[..4]);
    assert_eq!(&buf[4..8], &[0, 0, 0, 0], "缝隙必须补零，否则数据会前移");
    assert_eq!(&buf[8..], CONTENT1.as_bytes(), "后半段数据必须落在逻辑偏移 8");

    // 空洞：phys=0 视为全零
    let hole = pick("hole.bin");
    let mut buf = Vec::new();
    let n = fs.read(&hole, &mut buf).unwrap();
    assert_eq!(n, 10);
    assert_eq!(&buf[..4], &[0, 0, 0, 0], "phys=0 的 extent 是空洞");
    assert_eq!(&buf[4..], CONTENT1.as_bytes());
}

#[test]
fn second_volume_reads_plain_keys() {
    let path = build().write("second");
    let fs = apfs::open_volume(open(&path), 1).unwrap();
    assert_eq!(fs.info().label, "Second");

    let entries = fs.list(fs.root()).unwrap();
    assert_eq!(entries.len(), 1);
    assert_eq!(entries[0].name, "only.txt");

    let mut buf = Vec::new();
    fs.read(&entries[0], &mut buf).unwrap();
    assert_eq!(String::from_utf8(buf).unwrap(), CONTENT_V1);
}

#[test]
fn omap_returns_newest_xid_and_reports_missing() {
    let path = build().write("omap");
    let r = open(&path);
    let c = apfs::Container::open(&r).unwrap();
    let om = c.omap(&r).unwrap();

    let v = om.lookup(&r, BS as u32, OID_V0, 5).unwrap().unwrap();
    assert_eq!(v.paddr, B_V0_APSB);
    assert_eq!(v.size, BS as u32);

    // xid 早于对象创建 → 看不到
    assert!(om.lookup(&r, BS as u32, OID_V0, 4).unwrap().is_none());
    // 不存在的 oid
    assert!(om.lookup(&r, BS as u32, 99_999, 5).unwrap().is_none());
}

#[test]
fn corrupt_checksum_is_detected() {
    let mut img = build();
    // 破坏 APSB 中间一个字节（不动头部）
    img.blocks[B_V0_APSB as usize][0x300] ^= 0xFF;
    let path = img.write("corrupt");
    let r = open(&path);
    let buf = obj::read_block(&r, BS as u32, B_V0_APSB).unwrap();
    assert!(!obj::parse_header(&buf).unwrap().cksum_ok, "改过字节后校验必须失败");
}
