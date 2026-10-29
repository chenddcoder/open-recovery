#!/usr/bin/env python3
"""只读探测磁盘：GPT 主/备份头 + 丢失分区的文件系统签名。

用法: sudo python3 probe-disk.py /dev/rdisk4 [--full]
  默认只扫前 256MB + 末尾 256MB（秒级）；--full 扫全盘（慢，用于确认分区位置）
绝对只读：仅 os.open(O_RDONLY) + pread，不写一个字节。
"""
import os
import struct
import sys

def u16(b, o): return struct.unpack_from('<H', b, o)[0]
def u32(b, o): return struct.unpack_from('<I', b, o)[0]
def u64(b, o): return struct.unpack_from('<Q', b, o)[0]

class Dev:
    def __init__(self, path):
        self.fd = os.open(path, os.O_RDONLY)
        self.size = _device_size(self.fd, path)
    def read(self, off, n):
        if off < 0 or off >= self.size:
            return b''
        n = min(n, self.size - off)
        try:
            return os.pread(self.fd, n, off)
        except OSError:
            # USB 桥接盘偶发尾部读失败，退回小块读
            try:
                return os.pread(self.fd, min(n, 4096), off)
            except OSError:
                return b''
    def close(self):
        os.close(self.fd)

def _device_size(fd, path):
    """raw 字符设备（/dev/rdisk*）的 lseek(END) 返回 0，必须走 ioctl。"""
    try:
        n = os.lseek(fd, 0, os.SEEK_END)
        if n > 0:
            os.lseek(fd, 0, os.SEEK_SET)
            return n
    except OSError:
        pass
    import fcntl
    # macOS <sys/disk.h>: DKIOCGETBLOCKSIZE=0x40046418, DKIOCGETBLOCKCOUNT=0x40086419
    try:
        bs = struct.unpack('<I', fcntl.ioctl(fd, 0x40046418, b'\0' * 4))[0]
        bc = struct.unpack('<Q', fcntl.ioctl(fd, 0x40086419, b'\0' * 8))[0]
        if bs and bc:
            return bs * bc
    except OSError:
        pass
    # 兜底：问 diskutil
    import subprocess, re
    try:
        out = subprocess.run(['diskutil', 'info', path], capture_output=True, text=True).stdout
        m = re.search(r'Disk Size:\s+[\d.]+\s+\w+\s+\((\d+)\s+Bytes\)', out)
        if m:
            return int(m.group(1))
    except Exception:
        pass
    return 0

def gpt_header(dev, lba, sector, where):
    d = dev.read(lba * sector, 512)
    if len(d) < 92:
        print(f"  {where}: 读不到（{len(d)} 字节）")
        return None
    if d[:8] != b'EFI PART':
        print(f"  {where}: 无 GPT 签名，前 8 字节 = {d[:8]!r}")
        return None
    hdr = dict(
        rev=u32(d, 8), hsize=u32(d, 12),
        my_lba=u64(d, 24), alt_lba=u64(d, 32),
        first_usable=u64(d, 40), last_usable=u64(d, 48),
        entries_lba=u64(d, 72), n_entries=u32(d, 80), esize=u32(d, 84),
    )
    print(f"  {where}: 有效  rev={hdr['rev']:#x}  自身LBA={hdr['my_lba']}  另一份LBA={hdr['alt_lba']}")
    print(f"        可用区间 LBA {hdr['first_usable']}..{hdr['last_usable']}"
          f"  条目表 @LBA{hdr['entries_lba']} × {hdr['n_entries']}（每条 {hdr['esize']}B）")
    return hdr

def show_entries(dev, hdr, sector, where):
    if not hdr:
        return []
    raw = dev.read(hdr['entries_lba'] * sector, hdr['n_entries'] * hdr['esize'])
    out = []
    for i in range(hdr['n_entries']):
        e = raw[i * hdr['esize']:(i + 1) * hdr['esize']]
        if len(e) < 128 or e[:16] == b'\0' * 16:
            continue
        out.append(dict(i=i, type_guid=e[:16].hex(), first=u64(e, 32), last=u64(e, 40),
                        name=e[56:128].decode('utf-16-le', 'replace').rstrip('\0')))
    if out:
        for e in out:
            print(f"  条目{e['i']}: 类型 {e['type_guid'][:8]}…  LBA {e['first']}..{e['last']}"
                  f"  名字 {e['name']!r}")
    else:
        print(f"  {where}: 条目表 {hdr['n_entries']} 条**全为空** ← 分区就是这样消失的")
    return out

SIGS = [
    (b'NXSB', 'APFS 容器超级块'),
    (b'APSB', 'APFS 卷超级块（次级，非分区起点）'),
    (b'H+\x00', 'HFS+ 卷头'),
    (b'HX\x00', 'HFSX 卷头'),
    (b'NTFS    ', 'NTFS 引导扇区'),
    (b'EXFAT   ', 'exFAT 引导扇区'),
]

def scan(dev, start, length, sector, label, block=4 * 1024 * 1024, hits=None):
    """顺序大块读，内存里找签名。块间重叠 16 字节防止签名跨块。"""
    if hits is None:
        hits = []
    end = min(dev.size, start + length)
    off = max(0, start)
    overlap = 16
    while off < end:
        n = min(block, end - off)
        buf = dev.read(off, n)
        if not buf:
            break
        for sig, name in SIGS:
            p = buf.find(sig)
            while p >= 0:
                abs_off = off + p
                # APFS NXSB / HFS+ 头都在对象偏移 0x20 处，用魔数位置反推块起点
                hits.append((abs_off, name))
                p = buf.find(sig, p + 1)
        # ext 超级块魔数在超级块 +0x38
        p = buf.find(b'\x53\xef')
        while p >= 0:
            if p >= 0x38:
                hits.append((off + p - 0x38, 'ext2/3/4 超级块'))
            p = buf.find(b'\x53\xef', p + 1)
        off += max(1, n - overlap)
    print(f"  [{label}] 扫 {start:,}..{end:,}（{(end-start)/1e6:.0f} MB）完成，累计命中 {len(hits)}")
    return hits

def dedup_report(hits, sector):
    """同一文件系统会被多次命中（NXSB 在容器头与检查点各有一份），按 256MB 窗口聚合。"""
    if not hits:
        print("  未发现已知文件系统签名")
        return
    hits.sort()
    print("\n  ── 命中位置（已按 256MB 窗口去重）──")
    last_win = None
    shown = 0
    for off, name in hits:
        win = off // (256 * 1024 * 1024)
        if win == last_win:
            continue
        last_win = win
        print(f"   偏移 {off:>15,}  (LBA {off // sector:>12,})  {name}")
        shown += 1
        if shown >= 40:
            print("   … 还有更多，省略")
            break

def main():
    dev_path = sys.argv[1] if len(sys.argv) > 1 else '/dev/rdisk4'
    full = '--full' in sys.argv
    d = Dev(dev_path)
    sector = 512
    print(f"设备 {dev_path}")
    print(f"容量 {d.size:,} 字节 = {d.size/1e12:.3f} TB = {d.size//sector:,} 个 {sector}B 扇区")

    print("\n=== 1. LBA0（保护性 MBR） ===")
    mbr = d.read(0, 512)
    if len(mbr) == 512:
        print(f"  引导签名 {mbr[510:512].hex()}  磁盘签名 {mbr[440:444].hex()}")
        for i in range(4):
            e = mbr[446 + i * 16: 462 + i * 16]
            if e[4] == 0:
                continue
            print(f"  MBR 分区{i}: 类型 {e[4]:#04x}  起始LBA {u32(e,8)}  扇区数 {u32(e,12)}")

    print("\n=== 2. GPT 主头（LBA1） ===")
    h1 = gpt_header(d, 1, sector, "LBA1")
    show_entries(d, h1, sector, "主条目表")

    print("\n=== 3. GPT 备份头（最后一个扇区） ===")
    h2 = gpt_header(d, d.size // sector - 1, sector, f"LBA{d.size//sector - 1}")
    show_entries(d, h2, sector, "备份条目表")

    print("\n=== 4. 文件系统签名扫描 ===")
    hits = []
    head = 256 * 1024 * 1024
    tail = 256 * 1024 * 1024
    hits = scan(d, 0, head, sector, "开头", hits=hits)
    hits = scan(d, max(0, d.size - tail), tail, sector, "末尾", hits=hits)
    if full:
        mid_start = head
        mid_len = max(0, d.size - head - tail)
        if mid_len > 0:
            hits = scan(d, mid_start, mid_len, sector, "中段全盘", hits=hits)
    dedup_report(hits, sector)
    d.close()

if __name__ == '__main__':
    main()
