#!/usr/bin/env python3
"""只读扫描真盘 APFS fs-tree：统计 drec 按父目录分布 + dump inode 原始字节。

用法: sudo python3 fs-scan.py /dev/rdisk4 <容器基点>
"""
import os
import struct
import sys

def u16(b, o): return struct.unpack_from('<H', b, o)[0]
def u32(b, o): return struct.unpack_from('<I', b, o)[0]
def u64(b, o): return struct.unpack_from('<Q', b, o)[0]

class Dev:
    def __init__(self, path, base):
        self.fd = os.open(path, os.O_RDONLY)
        self.base = base
        self.size = _size(self.fd, path)
    def blk(self, bs, n):
        off = self.base + n * bs
        if off < 0 or off + bs > self.size:
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

def node_entries(nd, bs):
    flags = u16(nd, 0x20)
    level = u16(nd, 0x22)
    nkeys = u32(nd, 0x24)
    toff = u16(nd, 0x28)
    tlen = u16(nd, 0x2A)
    fixed = bool(flags & 4)
    root = bool(flags & 1)
    kb = 0x38 + toff + tlen
    vbase = bs - (40 if root else 0)
    out = []
    step = 4 if fixed else 8
    if nkeys == 0 or kb > bs or kb < 0x38:
        return flags, level, out
    for i in range(nkeys):
        p = 0x38 + toff + i * step
        if p + step > bs:
            break
        if fixed:
            ko = u16(nd, p); vo = u16(nd, p + 2)
            klen = 16
            vlen = 8 if level > 0 else 16
        else:
            ko = u16(nd, p); klen = u16(nd, p + 2)
            vo = u16(nd, p + 4); vlen = u16(nd, p + 6)
        ks = kb + ko
        ke = ks + klen
        vs = vbase - vo
        ve = vs + vlen
        if ks < 0 or ke > bs or vs < 0 or ve > bs or ks >= ke:
            continue
        out.append((nd[ks:ke], nd[vs:ve]))
    return flags, level, out

def main():
    path = sys.argv[1]
    base = int(sys.argv[2])
    d = Dev(path, base)
    bs = 4096
    print(f"设备 {path} 容器基点 {base:,}")

    # 1) NXSB
    nxsb = None
    for n in range(0, 600, 2):
        b = d.blk(bs, n)
        if b[0x20:0x24] == b'NXSB' and cksum_ok(b):
            if nxsb is None or u64(b, 0x10) > u64(nxsb[1], 0x10):
                nxsb = (n, b)
    _n, nx = nxsb
    print(f"NXSB 块{_n} xid={u64(nx,0x10)}")
    fs_oid = u64(nx, 0xb8)
    container_omap = u64(nx, 0xa0)

    # 2) 容器 omap → APSB
    def omap_map(omap_paddr, limit=100000):
        om = d.blk(bs, omap_paddr)
        if not om or u32(om, 0x18) & 0xffff != 0x0b:
            print(f"  omap@{omap_paddr} 不是 omap")
            return {}
        tree = u64(om, 0x30)
        res = {}
        stack = [tree]
        seen = set()
        while stack and len(seen) < 20000:
            p = stack.pop()
            if p in seen:
                continue
            seen.add(p)
            nd = d.blk(bs, p)
            if not nd:
                continue
            fl, lvl, ents = node_entries(nd, bs)
            if lvl > 0:
                for k, v in ents:
                    stack.append(u64(v, 0))
            else:
                for k, v in ents:
                    if len(k) >= 16 and len(v) >= 16:
                        oid = u64(k, 0); xid = u64(k, 8)
                        pa = u64(v, 8)
                        if oid not in res or xid > res[oid][0]:
                            res[oid] = (xid, pa)
        return {k: v[1] for k, v in res.items()}

    cmap = omap_map(container_omap)
    print(f"容器 omap 条目 {len(cmap)} 个")
    apsb_pa = cmap.get(fs_oid)
    print(f"fs_oid={fs_oid} → APSB 块 {apsb_pa}")
    ap = d.blk(bs, apsb_pa)
    vol_omap = u64(ap, 0x80)
    root_tree_oid = u64(ap, 0x88)
    volname = ap[0x2c0:0x2c0+64].split(b'\0')[0].decode('utf8','replace')
    print(f"卷名 {volname!r} 卷omap={vol_omap} root_tree_oid={root_tree_oid}")

    vmap = omap_map(vol_omap)
    root_pa = vmap.get(root_tree_oid)
    print(f"fs-tree 根 oid={root_tree_oid} → 块 {root_pa}")
    if root_pa is None:
        print("找不到 fs-tree 根，退出")
        return

    # 3) 遍历 fs-tree，统计 drec
    parent_count = {}
    inodes = []
    total_drec = 0
    total_nodes = 0
    stack = [root_pa]
    seen = set()
    while stack:
        p = stack.pop()
        if p in seen or len(seen) > 60000:
            continue
        seen.add(p)
        total_nodes += 1
        nd = d.blk(bs, p)
        if not nd:
            continue
        fl, lvl, ents = node_entries(nd, bs)
        if lvl > 0:
            for k, v in ents:
                child_oid = u64(v, 0)
                cp = vmap.get(child_oid)
                if cp:
                    stack.append(cp)
            continue
        for k, v in ents:
            if len(k) < 8:
                continue
            hdr = u64(k, 0)
            oid = hdr & 0x0fffffffffffffff
            ty = hdr >> 60
            if ty == 9:      # DIR_REC
                total_drec += 1
                parent_count[oid] = parent_count.get(oid, 0) + 1
            elif ty == 3 and len(inodes) < 40:
                inodes.append((oid, v))
    print(f"\n遍历节点 {total_nodes}，drec {total_drec} 条")
    print("\n=== 子项最多的目录（parent_id → 子项数）===")
    for pid, c in sorted(parent_count.items(), key=lambda x: -x[1])[:20]:
        print(f"  目录 {pid:>8} : {c} 项")
    print(f"\n目录总数 {len(parent_count)}")

    # 4) dump 几个 inode 的原始结构
    def xf_off(v):
        def valid(off):
            if off + 4 > len(v):
                return False
            n = u16(v, off)
            if n == 0 or n > 16:
                return False
            hdr = off + 2 + n + (n & 1)
            if hdr + n * 4 > len(v):
                return False
            data = hdr + n * 4
            for i in range(n):
                p = hdr + i * 4
                if p + 4 > len(v):
                    return False
                ty = v[p]; sz = u16(v, p + 2)
                if ty == 0 or ty > 20 or sz > 4096:
                    return False
                data += (sz + 7) & ~7
                if data > len(v):
                    return False
            return True
        for cand in (92, 86, 88, 90):
            if valid(cand):
                return cand
        return None

    print("\n=== inode 样本（前 8 个）===")
    for oid, v in inodes[:8]:
        xf = xf_off(v)
        mode = u16(v, 80) if len(v) >= 82 else 0
        print(f"  inode {oid}: vlen={len(v)} mode={mode:06o} xf_off={xf} "
              f"v88..96={v[88:96].hex() if len(v)>=96 else '-'}")
        if xf is not None:
            n = u16(v, xf)
            hdr = xf + 2 + n + (n & 1)
            data = hdr + n * 4
            for i in range(n):
                p = hdr + i * 4
                ty = v[p]; sz = u16(v, p + 2)
                seg = v[data:data+sz]
                extra = ""
                if ty == 8 and len(seg) >= 16:
                    extra = f" size={u64(seg,0)} alloced={u64(seg,8)}"
                print(f"      xf[{i}] type={ty} size={sz}{extra}")
                data += (sz + 7) & ~7

if __name__ == '__main__':
    main()
