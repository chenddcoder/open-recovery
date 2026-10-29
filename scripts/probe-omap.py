#!/usr/bin/env python3
"""只读诊断 APFS 容器的 omap 树：NXSB 字段、树层级、条目数、目标 oid 是否存在、坏节点。

用法: sudo python3 probe-omap.py /dev/rdisk4 <容器起始字节偏移> [目标oid]
"""
import os
import struct
import sys

def u32(b, o): return struct.unpack_from('<I', b, o)[0]
def u16(b, o): return struct.unpack_from('<H', b, o)[0]
def u64(b, o): return struct.unpack_from('<Q', b, o)[0]

class Dev:
    def __init__(self, path, base):
        self.fd = os.open(path, os.O_RDONLY)
        self.base = base
        self.size = _size(self.fd, path)
    def close(self):
        try:
            os.close(self.fd)
        except OSError:
            pass
    def blk(self, bs, n):
        off = self.base + n * bs
        if off + bs > self.size:
            return b''
        try:
            return os.pread(self.fd, bs, off)
        except OSError:
            return b''

def _size(fd, path):
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

M = 0xFFFF_FFFF

def fletcher(data):
    s1 = s2 = 0
    for i in range(0, len(data) - 3, 4):
        w = struct.unpack_from('<I', data, i)[0]
        s1 = (s1 + w) % M
        s2 = (s2 + s1) % M
    return s1, s2

def cksum_ok(b):
    return len(b) >= 32 and len(b) % 4 == 0 and fletcher(b)[0] == 0

def main():
    path = sys.argv[1]
    base = int(sys.argv[2])
    target = int(sys.argv[3]) if len(sys.argv) > 3 else None
    d = Dev(path, base)
    bs = 4096
    print(f"容器基点 {base:,}  设备 {path}")

    # 扫前 4 MiB 找 NXSB 副本
    print("\n=== NXSB 副本 ===")
    found = []
    for n in range(0, 1024):
        b = d.blk(bs, n)
        if not b or len(b) < 0x100:
            break
        if b[0x20:0x24] == b'NXSB':
            hdr_xid = u64(b, 0x10)
            nx_xid = u64(b, 0x10)
            ok = cksum_ok(b)
            omap = u64(b, 0xa0)
            cnt = u64(b, 0x28)
            fs = [u64(b, 0xb8 + i * 8) for i in range(4)]
            found.append((n, hdr_xid, ok, omap, cnt, fs))
            print(f"  块{n:5d} xid={hdr_xid:>10} 校验={'✓' if ok else '✗'} omap_paddr={omap} blocks={cnt} fs_oid={fs}")
    if not found:
        print("  未找到 NXSB")
        return

    good = [f for f in found if f[2]]
    if not good:
        print("  没有校验通过的 NXSB")
        return
    n, xid, _, omap_paddr, block_count, fs_oids = max(good, key=lambda x: x[1])
    print(f"\n选用 块{n} xid={xid} omap_paddr={omap_paddr}")

    # omap 物理对象
    om = d.blk(bs, omap_paddr)
    print(f"\n=== OMAP @块{omap_paddr} ===")
    if not om:
        print("  读不到")
        return
    print(f"  oid={u64(om,8)} xid={u64(om,0x10)} type={u32(om,0x18):#x} 校验={'✓' if cksum_ok(om) else '✗'}")
    tree_oid = u64(om, 0x30)
    print(f"  tree_oid={tree_oid} flags@0x28={u32(om,0x28):#x}")

    # 遍历 omap btree（物理树：索引值前 8 字节是子节点块号）
    print("\n=== omap 树遍历 ===")
    stack = [(tree_oid, 0)]
    seen = set()
    total = 0
    bad = 0
    hit = None
    max_xid_seen = 0
    while stack:
        paddr, depth = stack.pop()
        if paddr in seen or len(seen) > 20000:
            continue
        seen.add(paddr)
        nd = d.blk(bs, paddr)
        if not nd:
            print(f"  块{paddr}: 读不到")
            bad += 1
            continue
        if nd[0x20:0x24] != b'NXSB' and u32(nd, 0x18) & 0xFFFF not in (2, 3):
            pass  # 有些节点无魔数，仅用 type 判断
        ok = cksum_ok(nd)
        flags = u16(nd, 0x20)
        level = u16(nd, 0x22)
        nkeys = u32(nd, 0x24)
        toff = u16(nd, 0x28)
        tlen = u16(nd, 0x2A)
        if nkeys == 0 or toff + tlen > bs:
            print(f"  块{paddr}: 头异常 flags={flags:#x} level={level} nkeys={nkeys} toff={toff} tlen={tlen}")
            bad += 1
            continue
        fixed = bool(flags & 4)
        root = bool(flags & 1)
        kb = 0x38 + toff + tlen
        vbase = bs - (40 if root else 0)
        if not ok:
            bad += 1
        if depth <= 1:
            print(f"  块{paddr} depth={depth} level={level} nkeys={nkeys} fixed={fixed} root={root} 校验={'✓' if ok else '✗'}")
        for i in range(nkeys):
            p = 0x38 + toff + i * (4 if fixed else 8)
            if p + 8 > bs:
                break
            ko = u16(nd, p)
            vo = u16(nd, p + 2)
            k = nd[kb + ko:]
            v = nd[vbase - vo:]
            if level > 0:
                # 索引节点：值里前 8 字节是子块号
                child = u64(v, 0)
                if 0 < child < block_count:
                    stack.append((child, depth + 1))
            else:
                total += 1
                if len(k) >= 16 and len(v) >= 16:
                    oid = u64(k, 0)
                    kxid = u64(k, 8)
                    max_xid_seen = max(max_xid_seen, kxid)
                    if target is not None and oid == target:
                        hit = (oid, kxid, u64(v, 8))
    print(f"  叶子条目 {total} 条，节点 {len(seen)} 个，异常 {bad} 个，最大条目 xid={max_xid_seen}")
    if target is not None:
        if hit:
            print(f"  ✓ 找到 oid={target}: xid={hit[1]} paddr={hit[2]}")
        else:
            print(f"  ✗ 没有 oid={target}")
    d.close()

if __name__ == '__main__':
    main()
