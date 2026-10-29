#!/usr/bin/env python3
"""只读：按父目录精确查询 APFS fs-tree 的目录项（范围扫描，只读少量节点）。

用法: sudo python3 query-dir.py /dev/rdisk4 <容器基点> <目录id|root>
"""
import os
import struct
import sys

def u16(b, o): return struct.unpack_from('<H', b, o)[0]
def u32(b, o): return struct.unpack_from('<I', b, o)[0]
def u64(b, o): return struct.unpack_from('<Q', b, o)[0]

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
def cksum_ok(b):
    if len(b) < 32 or len(b) % 4:
        return False
    s1 = s2 = 0
    for i in range(0, len(b) - 3, 4):
        w = struct.unpack_from('<I', b, i)[0]
        s1 = (s1 + w) % M
        s2 = (s2 + s1) % M
    return s1 == 0

class FS:
    def __init__(self, path, base):
        self.fd = os.open(path, os.O_RDONLY)
        self.base = base
        self.size = _size(self.fd, path)
        self.bs = 4096
        self.vmap = {}
        self.root = None
        self.reads = 0
        self._boot()
    def blk(self, n):
        off = self.base + n * self.bs
        if off < 0 or off + self.bs > self.size:
            return b''
        self.reads += 1
        try:
            return os.pread(self.fd, self.bs, off)
        except OSError:
            return b''
    def entries(self, nd):
        flags = u16(nd, 0x20); level = u16(nd, 0x22)
        nkeys = u32(nd, 0x24)
        toff = u16(nd, 0x28); tlen = u16(nd, 0x2A)
        fixed = bool(flags & 4); root = bool(flags & 1)
        kb = 0x38 + toff + tlen
        vbase = self.bs - (40 if root else 0)
        out = []
        if nkeys == 0 or kb > self.bs:
            return level, out
        step = 4 if fixed else 8
        for i in range(nkeys):
            p = 0x38 + toff + i * step
            if p + step > self.bs:
                break
            if fixed:
                ko = u16(nd, p); klen = 16; vo = u16(nd, p+2)
                vlen = 8 if level > 0 else 16
            else:
                ko = u16(nd, p); klen = u16(nd, p+2)
                vo = u16(nd, p+4); vlen = u16(nd, p+6)
            ks = kb + ko; ke = ks + klen
            vs = vbase - vo; ve = vs + vlen
            if ks < 0 or ke > self.bs or vs < 0 or ve > self.bs or ks >= ke:
                continue
            out.append((nd[ks:ke], nd[vs:ve]))
        return level, out
    def omap_map(self, omap_paddr):
        om = self.blk(omap_paddr)
        if not om or u32(om, 0x18) & 0xffff != 0x0b:
            return {}
        tree = u64(om, 0x30)
        res = {}
        stack = [tree]; seen = set()
        while stack and len(seen) < 50000:
            p = stack.pop()
            if p in seen: continue
            seen.add(p)
            nd = self.blk(p)
            if not nd: continue
            lvl, ents = self.entries(nd)
            if lvl > 0:
                for k, v in ents:
                    stack.append(u64(v, 0))
            else:
                for k, v in ents:
                    if len(k) >= 16 and len(v) >= 16:
                        oid = u64(k, 0); xid = u64(k, 8); pa = u64(v, 8)
                        if oid not in res or xid > res[oid][0]:
                            res[oid] = (xid, pa)
        return {k: v[1] for k, v in res.items()}
    def _boot(self):
        nx = None
        for n in range(0, 600, 2):
            b = self.blk(n)
            if b and b[0x20:0x24] == b'NXSB' and cksum_ok(b):
                if nx is None or u64(b, 0x10) > u64(nx[1], 0x10):
                    nx = (n, b)
        if not nx:
            raise SystemExit("找不到 NXSB")
        n, b = nx
        cmap = self.omap_map(u64(b, 0xa0))
        apsb = cmap.get(u64(b, 0xb8))
        ap = self.blk(apsb)
        vmap = self.omap_map(u64(ap, 0x80))
        self.vmap = vmap
        self.root = vmap.get(u64(ap, 0x88))
        self.volname = ap[0x2c0:0x2c0+64].split(b'\0')[0].decode('utf8','replace')
    def scan_dir(self, dir_id, max_hits=500):
        start = struct.pack('>Q', (dir_id << 60) | 9)
        stop_oid = dir_id + 1
        out = []
        self._scan(self.root, start, stop_oid, out, max_hits, 0, set())
        return out
    def _scan(self, paddr, start, stop_oid, out, max_hits, depth, seen):
        if depth > 8 or paddr in seen or len(out) >= max_hits:
            return False
        seen.add(paddr)
        nd = self.blk(paddr)
        if not nd:
            return False
        lvl, ents = self.entries(nd)
        if os.environ.get('DBG'):
            print(f"  [scan] 块{paddr} level={lvl} nkeys={len(ents)}")
        if lvl == 0:
            for k, v in ents:
                if len(k) < 8:
                    continue
                if k[:8] < start[:8]:
                    continue
                hdr = struct.unpack_from('>Q', k, 0)[0]
                oid = hdr & 0x0fffffffffffffff
                if oid > stop_oid - 1:
                    return True
                if oid < stop_oid:
                    out.append((k, v))
                    if len(out) >= max_hits:
                        return True
            return False
        # 索引节点：二分找起点
        i = 0
        for idx, (k, _v) in enumerate(ents):
            if k[:8] <= start[:8]:
                i = idx
        for idx in range(i, len(ents)):
            k, v = ents[idx]
            if k[:8] > struct.pack('>Q', (stop_oid << 60)):
                return True
            child = self.vmap.get(u64(v, 0))
            if child and self._scan(child, start, stop_oid, out, max_hits, depth + 1, seen):
                return True
        return False

def drec_name(k):
    # j_drec_hashed_key: u64 hdr + u32 name_len_and_hash + name
    if len(k) >= 12:
        nl = u32(k, 8) & 0x3FF
        if 12 + nl <= len(k):
            return k[12:12+nl].decode('utf8', 'replace')
    if len(k) >= 10:
        nl = u16(k, 8)
        if 10 + nl <= len(k):
            return k[10:10+nl].decode('utf8', 'replace')
    return "<无法解析>"

def main():
    path = sys.argv[1]; base = int(sys.argv[2]); arg = sys.argv[3]
    fs = FS(path, base)
    print(f"卷 {fs.volname!r} fs-tree 根块 {fs.root}")
    if arg == 'root':
        did = 2
    else:
        did = int(arg)
    rows = fs.scan_dir(did)
    print(f"目录 {did} 的目录项：{len(rows)} 条（读了 {fs.reads} 个块）")
    for k, v in rows:
        fid = u64(v, 0)
        flags = u16(v, 16)
        print(f"  {drec_name(k)!r:<40} id={fid:<10} flags={flags:#06x}")
    os.close(fs.fd)

if __name__ == '__main__':
    main()
