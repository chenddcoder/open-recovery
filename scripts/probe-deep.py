#!/usr/bin/env python3
"""深度探测：GPT CRC 校验 + 直接读某个 LBA 附近的内容。

用法: sudo python3 probe-deep.py /dev/rdisk4
绝对只读。
"""
import os
import struct
import sys
import zlib

def u32(b, o): return struct.unpack_from('<I', b, o)[0]
def u64(b, o): return struct.unpack_from('<Q', b, o)[0]
def u16(b, o): return struct.unpack_from('<H', b, o)[0]

def dev_size(fd, path):
    try:
        n = os.lseek(fd, 0, os.SEEK_END)
        if n > 0:
            os.lseek(fd, 0, os.SEEK_SET)
            return n
    except OSError:
        pass
    import fcntl
    try:
        bs = struct.unpack('<I', fcntl.ioctl(fd, 0x40046418, b'\0' * 4))[0]
        bc = struct.unpack('<Q', fcntl.ioctl(fd, 0x40086419, b'\0' * 8))[0]
        if bs and bc:
            return bs * bc
    except OSError:
        pass
    return 0

def pread(fd, off, n, size):
    if off < 0 or off >= size:
        return b''
    n = min(n, size - off)
    try:
        return os.pread(fd, n, off)
    except OSError:
        try:
            return os.pread(fd, min(n, 4096), off)
        except OSError:
            return b''

def check_gpt(fd, size, sector, lba, label):
    d = pread(fd, lba * sector, 512, size)
    if len(d) < 92 or d[:8] != b'EFI PART':
        print(f"\n[{label}] LBA{lba}: 无 GPT 签名（{d[:8]!r}）")
        return None
    hsize = u32(d, 12)
    stored_hcrc = u32(d, 16)
    entries_lba = u64(d, 72)
    n = u32(d, 80)
    esize = u32(d, 84)
    stored_ecrc = u32(d, 88)

    # 头 CRC：把 CRC 字段清零后对 hsize 字节求 CRC32
    head = bytearray(d[:hsize])
    head[16:20] = b'\0\0\0\0'
    calc_hcrc = zlib.crc32(bytes(head)) & 0xffffffff

    # 条目 CRC
    raw = pread(fd, entries_lba * sector, n * esize, size)
    calc_ecrc = zlib.crc32(raw) & 0xffffffff

    print(f"\n[{label}] LBA{lba}  hsize={hsize}  条目 {n}×{esize}")
    print(f"  头 CRC   存储 {stored_hcrc:#010x}  计算 {calc_hcrc:#010x}  "
          f"{'✓ 一致' if stored_hcrc == calc_hcrc else '✗ 不一致（结构被改过）'}")
    print(f"  条目 CRC 存储 {stored_ecrc:#010x}  计算 {calc_ecrc:#010x}  "
          f"{'✓ 一致' if stored_ecrc == calc_ecrc else '✗ 不一致（条目被改过）'}")

    # 解析条目
    for i in range(n):
        e = raw[i * esize:(i + 1) * esize]
        if len(e) < 128 or e[:16] == b'\0' * 16:
            continue
        name = e[56:128].decode('utf-16-le', 'replace').rstrip('\0')
        print(f"  条目{i}: 类型 {e[:16].hex()}  LBA {u64(e,32)}..{u64(e,40)}  名 {name!r}")
    return dict(entries_lba=entries_lba, n=n, esize=esize)

def dump(fd, size, sector, lba, count, label):
    off = lba * sector
    print(f"\n[{label}] LBA{lba} = 偏移 {off:,}，读 {count} 个扇区")
    d = pread(fd, off, count * sector, size)
    if not d:
        print("  读不到")
        return
    if all(b == 0 for b in d):
        print("  ⚠ 全是 0（该区域被清零 = 已被擦除/格式化）")
        return
    if all(b == 0xFF for b in d):
        print("  ⚠ 全是 0xFF（未写入/坏区）")
        return
    print(f"  前 256 字节: {d[:256].hex()}")
    print(f"  ASCII: {''.join(chr(c) if 32 <= c < 127 else '.' for c in d[:256])}")
    # 找魔数
    for sig, name in [(b'NXSB', 'APFS 容器'), (b'APSB', 'APFS 卷'), (b'H+', 'HFS+'),
                      (b'HX', 'HFSX'), (b'NTFS    ', 'NTFS'), (b'EXFAT   ', 'exFAT')]:
        p = d.find(sig)
        if p >= 0:
            print(f"  ✓ 找到 {name} 签名 @ 块内偏移 {p:#x}（绝对 {off+p:,}）")

def main():
    path = sys.argv[1] if len(sys.argv) > 1 else '/dev/rdisk4'
    fd = os.open(path, os.O_RDONLY)
    size = dev_size(fd, path)
    sector = 512
    print(f"设备 {path}  容量 {size:,} 字节（{size//sector:,} 扇区）")

    check_gpt(fd, size, sector, 1, "主 GPT")
    check_gpt(fd, size, sector, size // sector - 1, "备份 GPT")

    # 分区 1（EFI）起点
    dump(fd, size, sector, 40, 8, "EFI 分区起点")
    # 分区 2（疑似 APFS）起点，多读几个位置确认头在哪
    dump(fd, size, sector, 409640, 16, "分区2 起点（GPT 声明的 APFS 起点）")
    # APFS 容器头也可能在起点后 1MB / 4K
    dump(fd, size, sector, 409640 + 8, 4, "分区2 +4KB")
    dump(fd, size, sector, 409640 + 2048, 4, "分区2 +1MB")

    # 在分区 2 前 64MB 里找 NXSB
    print("\n[在分区2 前 64MB 内搜 NXSB]")
    base = 409640 * sector
    chunk = 4 * 1024 * 1024
    found = 0
    off = base
    while off < base + 64 * 1024 * 1024 and off < size:
        buf = pread(fd, off, chunk, size)
        if not buf:
            break
        p = buf.find(b'NXSB')
        while p >= 0 and found < 10:
            print(f"  ✓ NXSB @ {off+p:,} (LBA {(off+p)//sector})")
            found += 1
            p = buf.find(b'NXSB', p + 1)
        off += chunk - 32
    if not found:
        print("  ✗ 前 64MB 内没有 NXSB —— APFS 容器头已不在原位")
    os.close(fd)

if __name__ == '__main__':
    main()
