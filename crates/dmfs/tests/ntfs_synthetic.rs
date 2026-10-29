//! 手工构造一个最小 NTFS 卷，端到端验证解析器。
//!
//! 为什么不用 mkntfs：macOS 上没有 NTFS 写入工具。与其"假设能跑"，
//! 不如在测试里按微软规范把引导扇区、MFT、属性、索引全部写出来——
//! 这样解析器的每个偏移都被真实字节校验过，也能当回归测试长期用。

use dmfs::{FileSystem, NodeKind};

const SECTOR: usize = 512;
const CLUSTER: usize = 4096;
const RECORD: usize = 1024;
const TOTAL_SECTORS: u32 = 16384; // 8 MiB
const MFT_LCN: u64 = 4;
const MFT_RECORDS: usize = 96;
const DATA_LCN: u64 = 40;

// 记录号
const R_MFT: usize = 0;
const R_SYSTEM: usize = 6;
const R_ROOT: usize = 5;
const R_HELLO: usize = 64;
const R_BIG: usize = 65;
const R_SUB: usize = 66;
const R_DELETED: usize = 67;
const R_INNER: usize = 68;

fn put_u16(b: &mut [u8], off: usize, v: u16) {
    b[off..off + 2].copy_from_slice(&v.to_le_bytes());
}
fn put_u32(b: &mut [u8], off: usize, v: u32) {
    b[off..off + 4].copy_from_slice(&v.to_le_bytes());
}
fn put_u64(b: &mut [u8], off: usize, v: u64) {
    b[off..off + 8].copy_from_slice(&v.to_le_bytes());
}

/// UTF-16LE 文件名
fn utf16(s: &str) -> Vec<u8> {
    let mut v = Vec::new();
    for u in s.encode_utf16() {
        v.extend_from_slice(&u.to_le_bytes());
    }
    v
}

/// $FILE_NAME 属性值：66 字节头 + 名字
fn file_name_value(parent: u64, name: &str, size: u64, is_dir: bool) -> Vec<u8> {
    let n = utf16(name);
    let mut v = vec![0u8; 66 + n.len()];
    put_u64(&mut v, 0, parent | (1u64 << 48)); // 父引用低 48 位 + 序列号 1
    // 时间：2026-01-01 之外的任意固定值即可
    for i in 0..4 {
        put_u64(&mut v, 8 + i * 8, 0x01D0_0000_0000_0000);
    }
    put_u64(&mut v, 40, size); // allocated
    put_u64(&mut v, 48, size); // real size
    put_u32(&mut v, 56, if is_dir { 0x1000_0000 } else { 0x20 });
    v[64] = name.encode_utf16().count() as u8;
    v[65] = 1; // WIN32
    v[66..].copy_from_slice(&n);
    v
}

/// 常驻属性
fn resident_attr(type_id: u32, name: &str, value: &[u8], attr_id: u16) -> Vec<u8> {
    let name_bytes = utf16(name);
    let header = 24;
    let name_off = header;
    let value_off = align8(name_off + name_bytes.len());
    let total = align8(value_off + value.len());
    let mut a = vec![0u8; total];
    put_u32(&mut a, 0, type_id);
    put_u32(&mut a, 4, total as u32);
    a[8] = 0;
    a[9] = if name.is_empty() { 0 } else { name.encode_utf16().count() as u8 };
    put_u16(&mut a, 10, if name.is_empty() { 0 } else { name_off as u16 });
    put_u16(&mut a, 12, 0);
    put_u16(&mut a, 14, attr_id);
    put_u32(&mut a, 16, value.len() as u32);
    put_u16(&mut a, 20, value_off as u16);
    a[22] = 0;
    if !name.is_empty() {
        a[name_off..name_off + name_bytes.len()].copy_from_slice(&name_bytes);
    }
    a[value_off..value_off + value.len()].copy_from_slice(value);
    a
}

/// 编码单条 data run。长度/偏移字段宽度必须与实际字节数一致，
/// 否则解析端会错位（这是手写 runlist 最容易犯的错）。
fn encode_run(lcn: u64, len: u64) -> Vec<u8> {
    let need = |mut v: u64| -> usize {
        let mut n = 1;
        while v > 0xFF {
            v >>= 8;
            n += 1;
        }
        n
    };
    let len_bytes = need(len);
    let off_bytes = need(lcn);
    let mut out = vec![((off_bytes as u8) << 4) | len_bytes as u8];
    for i in 0..len_bytes {
        out.push((len >> (8 * i)) as u8);
    }
    for i in 0..off_bytes {
        out.push((lcn >> (8 * i)) as u8);
    }
    out.push(0); // 结束
    out
}

/// 非常驻属性（一个 run）
fn nonresident_attr(
    type_id: u32,
    lcn: u64,
    len_clusters: u64,
    real_size: u64,
    attr_id: u16,
) -> Vec<u8> {
    let header = 64;
    let runs = encode_run(lcn, len_clusters);
    let total = align8(header + runs.len());
    let mut a = vec![0u8; total];
    put_u32(&mut a, 0, type_id);
    put_u32(&mut a, 4, total as u32);
    a[8] = 1;
    a[9] = 0;
    put_u16(&mut a, 10, 0);
    put_u16(&mut a, 12, 0);
    put_u16(&mut a, 14, attr_id);
    put_u64(&mut a, 16, 0); // start vcn
    put_u64(&mut a, 24, len_clusters - 1); // last vcn
    put_u16(&mut a, 32, header as u16); // run offset
    put_u16(&mut a, 34, 0);
    put_u64(&mut a, 40, len_clusters * CLUSTER as u64);
    put_u64(&mut a, 48, real_size);
    put_u64(&mut a, 56, real_size);
    a[header..header + runs.len()].copy_from_slice(&runs);
    a
}

fn align8(n: usize) -> usize {
    (n + 7) & !7
}

/// 索引项：16 字节头 + $FILE_NAME key
fn index_entry(ref_id: u64, parent: u64, name: &str, size: u64, is_dir: bool, last: bool) -> Vec<u8> {
    if last {
        let mut e = vec![0u8; 16];
        put_u16(&mut e, 8, 16);
        put_u16(&mut e, 10, 0);
        put_u16(&mut e, 12, 2); // 最后一项
        return e;
    }
    let key = file_name_value(parent, name, size, is_dir);
    let len = align8(16 + key.len());
    let mut e = vec![0u8; len];
    put_u64(&mut e, 0, ref_id | (1u64 << 48));
    put_u16(&mut e, 8, len as u16);
    put_u16(&mut e, 10, key.len() as u16);
    put_u16(&mut e, 12, 0);
    e[16..16 + key.len()].copy_from_slice(&key);
    e
}

/// $INDEX_ROOT 值
fn index_root_value(entries: &[Vec<u8>]) -> Vec<u8> {
    let body: Vec<u8> = entries.iter().flatten().copied().collect();
    let mut v = vec![0u8; 32 + body.len()];
    put_u32(&mut v, 0, 0x30); // 索引的属性类型：$FILE_NAME
    put_u32(&mut v, 4, 1); // COLLATION_FILE_NAME
    put_u32(&mut v, 8, CLUSTER as u32);
    v[12] = (CLUSTER / SECTOR) as u8;
    // IndexHeader 从 16 开始
    put_u32(&mut v, 16, 16); // 首项相对 IndexHeader 的偏移
    put_u32(&mut v, 20, body.len() as u32);
    put_u32(&mut v, 24, body.len() as u32);
    v[28] = 0; // 叶子
    v[32..].copy_from_slice(&body);
    v
}

fn build_record(number: usize, in_use: bool, is_dir: bool, attrs: Vec<Vec<u8>>) -> Vec<u8> {
    let mut rec = vec![0u8; RECORD];
    rec[0..4].copy_from_slice(b"FILE");
    put_u16(&mut rec, 4, 48); // USA 偏移
    put_u16(&mut rec, 6, (RECORD / SECTOR + 1) as u16);
    put_u16(&mut rec, 16, 1); // 序列号
    put_u16(&mut rec, 18, 1); // 链接数
    let first_attr = 56usize;
    put_u16(&mut rec, 20, first_attr as u16);
    let flags = (if in_use { 1u16 } else { 0 }) | (if is_dir { 2 } else { 0 });
    put_u16(&mut rec, 22, flags);
    put_u32(&mut rec, 28, RECORD as u32);

    let mut off = first_attr;
    for a in attrs {
        rec[off..off + a.len()].copy_from_slice(&a);
        off += a.len();
    }
    put_u32(&mut rec, off, 0xFFFF_FFFF); // 属性结束
    let used = off + 8;
    put_u32(&mut rec, 24, used as u32);
    put_u32(&mut rec, 44, 0); // next attr id

    // fixup：把每个扇区末尾 2 字节的真实内容存进 USA，再用 USN 覆盖。
    // 这正是 NTFS 的做法——不这么做，解析器还原 fixup 时会把真实数据抹掉。
    let usn: u16 = 0x0001;
    put_u16(&mut rec, 48, usn);
    for i in 1..(RECORD / SECTOR + 1) {
        let pos = i * SECTOR;
        let orig = u16::from_le_bytes([rec[pos - 2], rec[pos - 1]]);
        put_u16(&mut rec, 48 + i * 2, orig);
        put_u16(&mut rec, pos - 2, usn);
    }
    let _ = number;
    rec
}

fn build_image() -> Vec<u8> {
    let mut img = vec![0u8; TOTAL_SECTORS as usize * SECTOR];

    // ---- 引导扇区 ----
    let mut b = vec![0u8; SECTOR];
    b[0..3].copy_from_slice(&[0xEB, 0x52, 0x90]);
    b[3..11].copy_from_slice(b"NTFS    ");
    put_u16(&mut b, 11, SECTOR as u16);
    b[13] = (CLUSTER / SECTOR) as u8;
    put_u64(&mut b, 40, TOTAL_SECTORS as u64);
    put_u64(&mut b, 48, MFT_LCN);
    put_u64(&mut b, 56, MFT_LCN + 32);
    b[64] = 0xF6; // 记录大小 = 2^10 = 1024（i8 负数）
    b[65] = 0xF6; // 索引块 1024
    put_u64(&mut b, 69, 0x1122_3344_5566_7788);
    b[510] = 0x55;
    b[511] = 0xAA;
    img[0..SECTOR].copy_from_slice(&b);

    // ---- 数据区（非驻留文件）----
    let data_off = DATA_LCN as usize * CLUSTER;
    for i in 0..5000usize {
        img[data_off + i] = (i % 251) as u8;
    }

    // ---- 记录 ----
    let mft_off = MFT_LCN as usize * CLUSTER;

    let mut recs: Vec<Vec<u8>> = vec![Vec::new(); MFT_RECORDS];

    // 0: $MFT 自身
    recs[R_MFT] = build_record(
        R_MFT,
        true,
        false,
        vec![
            resident_attr(0x10, "", &vec![0u8; 72], 0),
            resident_attr(0x30, "", &file_name_value(5, "$MFT", 0, false), 1),
            nonresident_attr(0x80, MFT_LCN, (MFT_RECORDS * RECORD / CLUSTER) as u64, (MFT_RECORDS * RECORD) as u64, 2),
        ],
    );

    // 3: $Volume（卷标）
    recs[3] = build_record(
        3,
        true,
        false,
        vec![
            resident_attr(0x10, "", &vec![0u8; 72], 0),
            resident_attr(0x30, "", &file_name_value(5, "$Volume", 0, false), 1),
            resident_attr(0x60, "", &utf16("TESTNTFS"), 2),
        ],
    );

    // 系统记录占位（避免解析到空记录时误判）
    for i in 1..=R_SYSTEM {
        if recs[i].is_empty() {
            recs[i] = build_record(i, true, false, vec![resident_attr(0x10, "", &vec![0u8; 72], 0)]);
        }
    }

    // 5: 根目录
    let root_entries = vec![
        index_entry(R_HELLO as u64, 5, "hello.txt", 11, false, false),
        index_entry(R_BIG as u64, 5, "big.bin", 5000, false, false),
        index_entry(R_SUB as u64, 5, "sub", 0, true, false),
        index_entry(0, 0, "", 0, false, true),
    ];
    recs[R_ROOT] = build_record(
        R_ROOT,
        true,
        true,
        vec![
            resident_attr(0x10, "", &vec![0u8; 72], 0),
            resident_attr(0x30, "", &file_name_value(5, ".", 0, true), 1),
            resident_attr(0x90, "$I30", &index_root_value(&root_entries), 2),
        ],
    );

    // 64: hello.txt（常驻数据）
    recs[R_HELLO] = build_record(
        R_HELLO,
        true,
        false,
        vec![
            resident_attr(0x10, "", &vec![0u8; 72], 0),
            resident_attr(0x30, "", &file_name_value(5, "hello.txt", 11, false), 1),
            resident_attr(0x80, "", b"hello ntfs\n", 2),
        ],
    );

    // 65: big.bin（非常驻，2 个簇，真实 5000 字节）
    recs[R_BIG] = build_record(
        R_BIG,
        true,
        false,
        vec![
            resident_attr(0x10, "", &vec![0u8; 72], 0),
            resident_attr(0x30, "", &file_name_value(5, "big.bin", 5000, false), 1),
            nonresident_attr(0x80, DATA_LCN, 2, 5000, 2),
        ],
    );

    // 66: sub 目录
    let sub_entries = vec![
        index_entry(R_INNER as u64, R_SUB as u64, "inner.txt", 7, false, false),
        index_entry(0, 0, "", 0, false, true),
    ];
    recs[R_SUB] = build_record(
        R_SUB,
        true,
        true,
        vec![
            resident_attr(0x10, "", &vec![0u8; 72], 0),
            resident_attr(0x30, "", &file_name_value(5, "sub", 0, true), 1),
            resident_attr(0x90, "$I30", &index_root_value(&sub_entries), 2),
        ],
    );

    // 67: 已删除文件（in_use = false）
    recs[R_DELETED] = build_record(
        R_DELETED,
        false,
        false,
        vec![
            resident_attr(0x10, "", &vec![0u8; 72], 0),
            resident_attr(0x30, "", &file_name_value(5, "deleted.txt", 12, false), 1),
            resident_attr(0x80, "", b"deleted data", 2),
        ],
    );

    // 68: inner.txt
    recs[R_INNER] = build_record(
        R_INNER,
        true,
        false,
        vec![
            resident_attr(0x10, "", &vec![0u8; 72], 0),
            resident_attr(0x30, "", &file_name_value(R_SUB as u64, "inner.txt", 7, false), 1),
            resident_attr(0x80, "", b"nested\n", 2),
        ],
    );

    for (i, r) in recs.iter().enumerate() {
        if r.is_empty() {
            continue;
        }
        let off = mft_off + i * RECORD;
        img[off..off + RECORD].copy_from_slice(r);
    }
    img
}

fn write_temp(name: &str, data: &[u8]) -> std::path::PathBuf {
    let dir = std::env::temp_dir().join("dmde-rs-tests");
    std::fs::create_dir_all(&dir).unwrap();
    let p = dir.join(name);
    std::fs::write(&p, data).unwrap();
    p
}

#[test]
fn parses_synthetic_ntfs_volume() {
    let path = write_temp("synthetic_ntfs.img", &build_image());
    let dev = dmdisk::open_source(path.to_str().unwrap(), 0).unwrap();
    let reader = dmdisk::Reader::new(dev);
    assert_eq!(dmfs::probe(&reader), dmfs::FsKind::Ntfs);

    let fs = dmfs::AnyFs::open(reader).expect("挂载 NTFS 失败");
    assert_eq!(fs.info().kind, dmfs::FsKind::Ntfs);
    assert_eq!(fs.info().label, "TESTNTFS");
    assert_eq!(fs.info().block_size, CLUSTER as u32);

    let mut names: Vec<String> = fs
        .list(fs.root())
        .unwrap()
        .into_iter()
        .map(|e| e.name)
        .collect();
    names.sort();
    assert_eq!(names, vec!["big.bin", "hello.txt", "sub"]);

    // 常驻数据
    let hello = fs
        .list(fs.root())
        .unwrap()
        .into_iter()
        .find(|e| e.name == "hello.txt")
        .unwrap();
    assert_eq!(hello.size, 11);
    let mut buf = Vec::new();
    fs.read(&hello, &mut buf).unwrap();
    assert_eq!(buf, b"hello ntfs\n");

    // 非常驻数据 + runlist
    let big = fs
        .list(fs.root())
        .unwrap()
        .into_iter()
        .find(|e| e.name == "big.bin")
        .unwrap();
    let mut buf = Vec::new();
    let n = fs.read(&big, &mut buf).unwrap();
    assert_eq!(n, 5000);
    assert_eq!(buf.len(), 5000);
    assert_eq!(buf[0], 0);
    assert_eq!(buf[250], 250);
    assert_eq!(buf[251], 0);

    // 子目录
    let sub = fs
        .list(fs.root())
        .unwrap()
        .into_iter()
        .find(|e| e.name == "sub")
        .unwrap();
    assert_eq!(sub.kind, NodeKind::Dir);
    let inner = fs.list(sub.id).unwrap();
    assert_eq!(inner.len(), 1);
    assert_eq!(inner[0].name, "inner.txt");
    let mut buf = Vec::new();
    fs.read(&inner[0], &mut buf).unwrap();
    assert_eq!(buf, b"nested\n");

    // 删除恢复
    let deleted = fs.deleted().unwrap();
    assert_eq!(deleted.len(), 1, "应只找到 deleted.txt，实际 {:?}", deleted.iter().map(|e| &e.name).collect::<Vec<_>>());
    assert_eq!(deleted[0].name, "deleted.txt");
    assert!(deleted[0].deleted);
    let mut buf = Vec::new();
    fs.read(&deleted[0], &mut buf).unwrap();
    assert_eq!(buf, b"deleted data");
}


