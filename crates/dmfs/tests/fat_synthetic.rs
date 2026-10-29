//! 手工构造 FAT16 卷，验证 BPB 解析、簇链读取、LFN 还原与删除恢复。
//!
//! 不依赖 hdiutil/newfs，跑在任何平台都能验证核心逻辑。

use dmfs::{FileSystem, NodeKind};

const SECTOR: usize = 512;
const SECTORS_PER_CLUSTER: usize = 4;
const CLUSTER: usize = SECTOR * SECTORS_PER_CLUSTER;
const RESERVED: usize = 1;
const NUM_FATS: usize = 2;
const FAT_SECTORS: usize = 20;
const ROOT_ENTRIES: usize = 512;
const TOTAL_SECTORS: usize = 20_480; // 10 MiB

const ROOT_DIR_SECTOR: usize = RESERVED + NUM_FATS * FAT_SECTORS;
const ROOT_DIR_SECTORS: usize = ROOT_ENTRIES * 32 / SECTOR;
const FIRST_DATA_SECTOR: usize = ROOT_DIR_SECTOR + ROOT_DIR_SECTORS;

fn cluster_offset(cluster: u32) -> usize {
    (FIRST_DATA_SECTOR + (cluster as usize - 2) * SECTORS_PER_CLUSTER) * SECTOR
}

/// 设置 FAT16 表项（两份表同时写）
fn set_fat(img: &mut [u8], cluster: u32, value: u16) {
    for f in 0..NUM_FATS {
        let base = (RESERVED + f * FAT_SECTORS) * SECTOR;
        let off = base + cluster as usize * 2;
        img[off..off + 2].copy_from_slice(&value.to_le_bytes());
    }
}

/// 32 字节短目录项
fn dirent(name: &str, ext: &str, attr: u8, cluster: u32, size: u32, deleted: bool) -> Vec<u8> {
    let mut e = vec![0u8; 32];
    let n = format!("{name:<8}");
    let x = format!("{ext:<3}");
    e[0..8].copy_from_slice(&n.as_bytes()[..8]);
    e[8..11].copy_from_slice(&x.as_bytes()[..3]);
    e[11] = attr;
    e[26..28].copy_from_slice(&(cluster as u16).to_le_bytes());
    e[28..32].copy_from_slice(&size.to_le_bytes());
    // 时间：2026-10-29 13:45
    let date: u16 = ((2026 - 1980) << 9) | (10 << 5) | 29;
    let time: u16 = (13 << 11) | (45 << 5);
    e[22..24].copy_from_slice(&time.to_le_bytes());
    e[24..26].copy_from_slice(&date.to_le_bytes());
    if deleted {
        e[0] = 0xE5;
    }
    e
}

/// 长文件名条目（顺序与目录中一致：从最后一段到第一段）
fn lfn_entries(name: &str) -> Vec<Vec<u8>> {
    let units: Vec<u16> = name.encode_utf16().collect();
    let mut chunks: Vec<Vec<u16>> = units.chunks(13).map(|c| c.to_vec()).collect();
    let count = chunks.len() as u8;
    let mut out = Vec::new();
    for i in (0..chunks.len()).rev() {
        let mut e = vec![0u8; 32];
        e[0] = (i as u8 + 1) | if i == chunks.len() - 1 { 0x40 } else { 0 };
        e[11] = 0x0F;
        e[13] = 0; // 校验和：本工具不做校验，置 0 即可
        let mut c = chunks[i].clone();
        while c.len() < 13 {
            c.push(if c.is_empty() { 0 } else { 0xFFFF });
        }
        for (k, unit) in c.iter().enumerate() {
            let pos = match k {
                0..=4 => 1 + k * 2,
                5..=10 => 14 + (k - 5) * 2,
                _ => 28 + (k - 11) * 2,
            };
            e[pos..pos + 2].copy_from_slice(&unit.to_le_bytes());
        }
        out.push(e);
    }
    chunks.clear();
    let _ = count;
    out
}

fn build() -> Vec<u8> {
    let mut img = vec![0u8; TOTAL_SECTORS * SECTOR];

    // ---- 引导扇区 ----
    let mut bs = vec![0u8; SECTOR];
    bs[0..3].copy_from_slice(&[0xEB, 0x3C, 0x90]);
    bs[3..11].copy_from_slice(b"MSDOS5.0");
    bs[11..13].copy_from_slice(&(SECTOR as u16).to_le_bytes());
    bs[13] = SECTORS_PER_CLUSTER as u8;
    bs[14..16].copy_from_slice(&(RESERVED as u16).to_le_bytes());
    bs[16] = NUM_FATS as u8;
    bs[17..19].copy_from_slice(&(ROOT_ENTRIES as u16).to_le_bytes());
    bs[19..21].copy_from_slice(&0u16.to_le_bytes()); // total16 = 0（用 total32）
    bs[21] = 0xF8;
    bs[22..24].copy_from_slice(&(FAT_SECTORS as u16).to_le_bytes());
    bs[32..36].copy_from_slice(&(TOTAL_SECTORS as u32).to_le_bytes());
    bs[38] = 0x29;
    bs[39..43].copy_from_slice(&0xDEAD_BEEFu32.to_le_bytes());
    bs[43..54].copy_from_slice(b"TESTFAT    ");
    bs[54..62].copy_from_slice(b"FAT16   ");
    bs[510] = 0x55;
    bs[511] = 0xAA;
    img[..SECTOR].copy_from_slice(&bs);

    // ---- FAT：媒体描述符 + 簇链 ----
    set_fat(&mut img, 0, 0xFFF8);
    set_fat(&mut img, 1, 0xFFFF);

    // cluster 2: 根目录之外的子目录 docs 不需要；这里只放文件
    // hello.txt -> cluster 2（1 个簇）
    set_fat(&mut img, 2, 0xFFFF);
    // dead.txt（已删除）-> 原 cluster 3，FAT 已清空（0）
    // big.bin -> cluster 4..6（3 簇链）
    set_fat(&mut img, 4, 5);
    set_fat(&mut img, 5, 6);
    set_fat(&mut img, 6, 0xFFFF);

    // ---- 数据 ----
    let hello = b"hello from synthetic fat\n";
    img[cluster_offset(2)..cluster_offset(2) + hello.len()].copy_from_slice(hello);

    let dead = b"dead file payload";
    img[cluster_offset(3)..cluster_offset(3) + dead.len()].copy_from_slice(dead);

    // big.bin 3 簇 = 6144 字节，最后一簇只用到 1000 字节
    let big_size = CLUSTER * 2 + 1000;
    for i in 0..big_size {
        img[cluster_offset(4) + i] = (i % 241) as u8;
    }

    // ---- 根目录 ----
    let root = ROOT_DIR_SECTOR * SECTOR;
    let mut off = root;

    // hello.txt（8.3 短名）
    let e = dirent("HELLO", "TXT", 0x20, 2, hello.len() as u32, false);
    img[off..off + 32].copy_from_slice(&e);
    off += 32;

    // 已删除 + 长文件名：LFN 条目全部标 0xE5，字符内容保留
    let long_name = "a very long filename example.txt";
    for mut lfn in lfn_entries(long_name) {
        lfn[0] = 0xE5; // 删除后序号丢失
        img[off..off + 32].copy_from_slice(&lfn);
        off += 32;
    }
    let e = dirent("AVERYLON", "TXT", 0x20, 3, dead.len() as u32, true);
    img[off..off + 32].copy_from_slice(&e);
    off += 32;

    // big.bin（3 簇链）
    let e = dirent("BIG", "BIN", 0x20, 4, big_size as u32, false);
    img[off..off + 32].copy_from_slice(&e);

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
fn parses_synthetic_fat16() {
    let path = write_temp("synthetic_fat16.img", &build());
    let dev = dmdisk::open_source(path.to_str().unwrap(), 0).unwrap();
    let reader = dmdisk::Reader::new(dev);
    assert_eq!(dmfs::probe(&reader), dmfs::FsKind::Fat16);

    let fs = dmfs::AnyFs::open(reader).unwrap();
    assert_eq!(fs.info().kind, dmfs::FsKind::Fat16);
    assert_eq!(fs.info().label, "TESTFAT");
    assert_eq!(fs.info().block_size, CLUSTER as u32);

    let entries = fs.list(fs.root()).unwrap();
    let names: Vec<&str> = entries.iter().map(|e| e.name.as_str()).collect();
    assert!(names.contains(&"HELLO.TXT"), "names={names:?}");
    assert!(names.contains(&"BIG.BIN"), "names={names:?}");

    // 未删除文件：按簇链读
    let hello = entries.iter().find(|e| e.name == "HELLO.TXT").unwrap();
    let mut buf = Vec::new();
    fs.read(hello, &mut buf).unwrap();
    assert_eq!(buf, b"hello from synthetic fat\n");

    let big = entries.iter().find(|e| e.name == "BIG.BIN").unwrap();
    assert_eq!(big.size, (CLUSTER * 2 + 1000) as u64);
    let mut buf = Vec::new();
    fs.read(big, &mut buf).unwrap();
    assert_eq!(buf.len(), CLUSTER * 2 + 1000);
    assert_eq!(buf[0], 0);
    assert_eq!(buf[240], 240);
    assert_eq!(buf[241], 0);

    // 多簇链的第三段
    assert_eq!(buf[CLUSTER * 2], (CLUSTER * 2 % 241) as u8);

    // 删除恢复：长文件名应从 LFN 还原出来，内容按连续簇读
    let deleted = fs.deleted().unwrap();
    assert_eq!(deleted.len(), 1, "实际: {:?}", deleted.iter().map(|e| &e.name).collect::<Vec<_>>());
    let d = &deleted[0];
    assert_eq!(d.name, "a very long filename example.txt");
    assert!(d.deleted);
    assert_eq!(d.kind, NodeKind::File);
    let mut buf = Vec::new();
    fs.read(d, &mut buf).unwrap();
    assert_eq!(buf, b"dead file payload");
}
