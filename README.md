# dmde-rs

DMDE（DM Disk Editor and Data Recovery）的**开源替代实现**：磁盘/分区解析 + 多文件系统只读浏览 + 删除文件恢复。
Rust 核心**零第三方依赖**，命令行与图形界面共用同一套引擎。

> 设计前提：恢复工具的第一原则是**不要动现场**。本项目所有打开路径都是只读，
> 任何写操作只能发生在你指定的输出目录里。

---

## 当前状态（v0.1）

| 能力 | 状态 | 说明 |
|---|---|---|
| 物理盘/分区枚举 | ✅ macOS、Linux | Windows 预留（`CreateFileW` + `DeviceIoControl`） |
| 镜像文件读取 | ✅ | `.img/.dd/.raw`，`.dmg` 需先 `hdiutil convert -format UDRW` |
| 只读块设备访问 | ✅ | macOS 走 `/dev/rdisk*` + `F_NOCACHE`，4K 扇区自适应 |
| 扇区级读写 | ⏳ 规划 | 本版只读，不提供写盘 |
| 分区表 MBR / EBR | ✅ | 含扩展分区链、环检测 |
| 分区表 GPT | ✅ | 头/分区项 CRC32 校验、备份表提示、4K 扇区识别 |
| 分区表 APM | ✅ | 老 Mac / 部分装机 U 盘 |
| FAT12/16/32 | ✅ 已验证 | 簇链遍历、LFN 还原、删除项恢复 |
| exFAT | ✅ 已验证 | File/Stream/FileName 三元组、NoFatChain |
| NTFS | ✅ 已验证 | 引导扇区、MFT、属性、runlist、`$INDEX_ROOT`、删除记录恢复 |
| ext2/3/4 | ✅ 已验证 | 超级块、块组、inode、extent 树 + 间接块 |
| HFS+ / HFSX | ✅ 已验证 | 卷头、catalog B-tree、fork extents |
| APFS | 🚧 仅容器识别 | 能认出 APFS 容器；卷/目录/文件解析见下节路线 |
| 签名雕刻（carving） | ⏳ 规划 | HFS+/APFS 删除后只能靠它 |
| GUI | ✅ Tauri v2 | 磁盘→分区→目录→预览→导出/恢复 |
| 提权读取设备 | ✅ macOS | 系统授权窗 → 只读 helper，GUI 本体不以 root 运行 |

"已验证"= 用真实镜像或字节级构造的镜像做过端到端断言（见「测试」一节）。

---

## 权限：GUI 为什么需要一个 helper

macOS 上打开 `/dev/rdisk*` 必须是 root。把整个 App（含 WebView）以 root 重启是最省事也最糟的做法：
破坏 TCC、窗口服务和用户偏好，还把全部解析代码都放进特权进程。

这里的取舍是**只把"读"交出去**：

```
GUI (你的身份)  ── Unix socket + token ──▶  dmde-rs-helper (root, 只读)
   │                                              │
   └── 写恢复文件（你的身份、你的目录）            └── 列目录 / 预览 / 吐字节
```

* 点 `/dev/rdisk*` 时如果权限不足，自动弹 **macOS 标准授权窗**（`do shell script ... with administrator privileges`）；
  取消就报"已取消管理员授权"，不做任何事；
* helper 的 socket 由 helper `chown` 回当前用户、权限 `0600`，放在 `~/Library/Application Support/dmde-rs/`，
  其它本地用户连不上；连接必须先 `hello` 带一次性随机 token；
* helper **没有任何写接口**——恢复出来的文件由 GUI 以你的身份写盘，属主/TCC/权限都正确；
* helper 空闲 900 秒自动退出并删除 socket；点"管理员权限"按钮也可手动请求，`drop_admin` 立即放弃；
* helper 日志在 `~/Library/Application Support/dmde-rs/helper.log`。

端口/协议是行式 JSON（`crates/dmrpc`），文件内容用 `chunk…end` 分块流，恒定内存。

---

## APFS：现状与路线（重点）

**现状：只做到"认得出"。** 打开 APFS 卷会明确告诉你还不能列目录，而不是假装挂载成功。
这不是疏忽，是分层没做完：APFS 不是"超级块 + 目录项"的结构，而是一套**对象化、带事务的 B-tree 系统**。

已经用真实 APFS 镜像（`hdiutil create -fs APFS`）确认的落盘布局（macOS 26 生成的卷）：

| 结构 | 位置 / 要点 |
|---|---|
| `NXSB` 容器超级块 | 容器第 0 块；`magic@0x20`、`block_size@0x24`、`block_count@0x28`、`omap_oid@0xa0`、`fs_oid[]@0xb8` |
| 检查点 | `nx_xp_desc_base/desc_blocks` 指向连续的 NXSB 副本（每 2 块一份） |
| `omap_phys` | `nx_omap_oid` 是**物理** oid（就是块号）；`om_tree_oid@0x30` 指向 omap B-tree |
| `btree_node_phys` | 头 0x38；`flags@0x20`（ROOT/LEAF/FIXED_KV）、`level@0x22`、`nkeys@0x24`、`table_space@0x28` |
| TOC / 键 / 值 | TOC 条目 `{u16 k; u16 v}`；键区起点 = `0x38 + table_space.off + table_space.len`；值从块尾向前放 |
| `APSB` 卷超级块 | `magic@0x20`、`features@0x28`、`alloc_count@0x58`、`meta_crypto@0x60`（`cpflags&1`=加密）、`omap_oid@0x80`、`root_tree_oid@0x88`、`snap_meta_tree_oid@0x98`、卷名 `@0x2c0`（256 字节） |

在 256 MB 测试卷上实测：容器 4096 B/块、65526 块；`fs_oid=[1026]`；container omap 在第 405 块 →
tree 在 406 块；**omap 里 `(1026, xid=6) → paddr=404`，第 404 块正是 `APSB`（oid 1026, xid 6）**；
卷 omap 在 399 块，`(1028, 5) → 392`，第 392 块的对象头 `oid=1028`——即卷的 fs-tree 根。
也就是说对象映射这条链路已经跑通，剩下的是 fs-tree 的记录解析。

**接下来分三步（按依赖顺序）：**

1. **B-tree 通用层 + 对象映射**：`kvoff` 值偏移规则、Fletcher64 校验、多级节点下降；
   `oid → paddr`（按 xid 取最新可见版本）。产出：能定位任意卷的 fs-tree 根。
2. **fs-tree 记录解析**：`j_key`（`obj_id | type`）、`j_drec_key/val`（目录项 + xfields）、
   `j_inode_val`（大小、时间）、`j_file_extent_val`（逻辑地址 → 物理块，读文件内容）。
   产出：APFS 卷可以像 FAT/NTFS 一样 `ls / cat / extract`。
3. **恢复**：APFS 删除会直接抹掉目录记录，按目录项恢复通常没戏——真正的抓手是
   ① **快照**（`snap_meta_tree`，本地 Time Machine 快照里保留旧 fs-tree，按 xid 读旧版本再 diff）；
   ② **extent 残留扫描**（已释放但未覆盖的块，按 inode/extent 特征重建）；
   ③ 最后兜底才是签名雕刻。这三条都要在第 2 步之上做。

**FileVault 加密卷**是另一条独立战线：`meta_crypto.cpflags&1` 为真时，fs-tree 的键值是加密的，
必须先拿到卷密钥（keybag + AES-XTS 解包）。本版对此只做识别与提示，不做解密；
实务上可以先在系统里解锁卷再处理其**未加密的容器元数据**，或对硬件加密盘做镜像后离线分析。

---

---

## 构建

```bash
# 命令行（零依赖，秒级编译）
cargo build --release
./target/release/dmde-rs help

# 图形界面（Tauri v2）
cd gui/src-tauri
cargo build --release          # 产物：target/release/dmde-gui
cd ../.. && cargo build --release -p dmhelper   # GUI 提权用的只读 helper
./scripts/bundle-macos.sh     # 打成 dist/dmde-rs.app（含 helper，ad-hoc 签名，双击即用）
```

打包说明：`cargo tauri build` 需要 **tauri-cli v2**。如果本机装的是 v1（`cargo tauri --version` 显示 1.x），
它会用 v1 schema 校验 `tauri.conf.json` 并报 `frontendDist was unexpected`——这时用上面的
`bundle-macos.sh` 手工组装 bundle（不依赖 CLI 版本）。

拉不动 crates.io 时：`~/.cargo/config.toml` 里的 tuna 镜像其 `dl` 仍指向 `static.crates.io`，
国内会 502。换成带下载地址的完整镜像即可，例如：

```toml
[source.crates-io]
replace-with = "ustc"
[source.ustc]
registry = "sparse+https://mirrors.ustc.edu.cn/crates.io-index/"
```

核心三个 crate 是零依赖的，不配镜像也能 `cargo build`；只有 GUI 需要拉 Tauri。

---

## 快速上手

### 列出磁盘

```bash
$ sudo ./dmde-rs devices -v
#   设备              容量  型号                          总线   类型
1   /dev/rdisk4     2.00 TiB  My Passport 2627            USB    外置
  分区表: GPT
    #1  EFI 系统分区              200.0 MiB  FAT32
    #2  Apple APFS                1.99 TiB  APFS
```

`/dev/rdisk*` 没有 root 权限打不开——这是内核的限制，不是工具的。

### 看分区表与文件系统

```bash
$ ./dmde-rs volumes /tmp/fat16raw.dmg
源      : /tmp/fat16raw.dmg
容量    : 24.00 MiB

分区表  : MBR
#   类型     起始偏移        大小  格式      分区标识       文件系统
1   主分区        512   24.00 MiB  FAT16     FAT32 (CHS)   可挂载为 FAT16
```

注意这里 `分区标识` 写的是 macOS 自己打错的类型码，实际以簇数推导（12255 簇 → FAT16 规范定义）为准。

### 浏览目录（含已删除项）

```bash
$ ./dmde-rs ls backup.dmg -l
T           大小  修改时间                 起始         名称
------------------------------------------------------------------------
d          0 B  2026-10-29 13:45:02  2          docs/
-         18 B  2026-10-29 13:45:02  3          hello.txt
-         27 B  2026-10-29 13:45:02  8          a very long filename example.txt  [已删除]
```

FAT 删除时会把文件首字节改成 `0xE5`、长文件名条目也一起标记。本工具会把 LFN 字符按原顺序拼回去，
所以能恢复出**原本的长文件名**，而不是 `?ILE.TXT` 这种短名残骸。

### 恢复

```bash
$ ./dmde-rs recover backup.dmg --dry-run          # 先看清单，不写盘
$ ./dmde-rs recover backup.dmg -o ~/rescued       # 真正落盘
完成：成功 2 个，失败 0 个，共 53 B
```

### 导出 / 查看

```bash
$ ./dmde-rs cat backup.dmg /hello.txt
$ ./dmde-rs extract backup.dmg /photos -o ~/rescued
$ ./dmde-rs info /dev/rdisk4 -p 2
$ ./dmde-rs tree backup.dmg / --depth 3
```

### 图形界面

左侧选磁盘 → 选分区 → 中间浏览（可勾选「显示已删除」）→ 右侧预览（文本/十六进制自动切换）
→ 单个「保存」或「批量恢复已删除」到输出目录。

---

## 架构

```
crates/dmdisk     只读块设备 + 分区表
├─ device.rs      BlockDevice trait、ImageFile、RawDevice（含 macOS F_NOCACHE / 4K 扇区探测）
├─ reader.rs      64 KiB 页缓存，子读取器共享同一份缓存（整盘扫描 + 卷解析不互相冷启）
├─ scheme/        mbr.rs / gpt.rs / apm.rs + 自动识别顺序 GPT → APM → MBR
└─ plist.rs       极简 XML plist 解析（枚举 macOS 磁盘用，避免引 IOKit）

crates/dmfs       文件系统解析 + 恢复
├─ fat/           boot / table(分块 LRU) / dir(LFN 还原) / fs
├─ exfat/         boot / dir(三元组) / fs
├─ ntfs/          boot / fixup / attr / record / runlist / index / fs
├─ ext/           superblock / inode / extent(树+间接块) / dir / fs
├─ hfsplus/       volume / catalog(B-tree 线性扫描) / fs
└─ detect.rs      签名探测 + AnyFs 门面

crates/dmcli      命令行
gui/              Tauri v2（Rust 后端直调 dmdisk/dmfs，前端纯 HTML/JS）

```

### 几个刻意的设计决定

**1. 零第三方依赖。**
核心不引 `anyhow/clap/serde`，理由是：恢复工具会被拿去跑在**陌生机器、离线环境、老系统**上；
依赖越少，越可能在现场编译成功；同时也没有供应链攻击面（恢复工具有读全盘权限）。

**2. 页缓存必须共享。**
列一个目录可能触发几万次 4 KiB 随机读。`Reader::slice()` 让分区读取器复用整盘读取器的缓存，
否则"整盘扫分区表 + 解析卷"会重复读同一批扇区。

**3. NTFS 列目录走索引，恢复才扫 MFT。**
索引是 O(目录项) 的；扫 MFT 是 O(全卷记录)，百万文件的大盘上要几十秒到几分钟。
两者用途不同，不能混用。

**4. FAT 表分块 LRU，不整表载入。**
32 GiB 卷的 FAT 约 4 MiB 还行，2 TiB 卷能到 256 MiB——整表载入在低配机器上会直接 OOM。

**5. 越界读取补零而不是报错。**
恢复对象常常是被 dd 截断的镜像/坏盘，解析器必须能优雅退化，把能读的部分读出来。

---

## 恢复原理与局限（务必先读）

| 格式 | 删除后残留 | 能恢复什么 | 局限 |
|---|---|---|---|
| FAT12/16/32 | 目录项首字节被抹成 `0xE5`，LFN 条目字符仍在 | 文件名（含长名）、大小、起始簇；数据按**连续簇**读取 | 簇链已清空，**碎片化文件会读坏** |
| exFAT | 只清"在用位"，File/Stream/Name 全部保留 | 文件名、大小、时间、起始簇；数据按连续或 NoFatChain 读 | 同上，碎片化需雕刻 |
| NTFS | MFT 记录只清"在用位"，runlist 通常完整 | 文件名、大小、时间、$DATA 运行列表 → **可精确恢复** | 压缩/EFS 加密文件、`$ATTRIBUTE_LIST` 分散的极端情况不支持 |
| ext2/3/4 | 目录项 inode 清零（名字残留）+ 释放 inode（块映射残留） | 名字（无数据）或数据（名为 `inoN.rec`） | 两者不保证能对应上；日志回放未实现 |
| HFS+ | catalog 记录被直接移除 | 只能列出存活文件 | **无法按目录项恢复**，要靠雕刻（规划中） |

另外两个实用提示：

- macOS 的 HFS+/APFS 删除文件通常先落到卷根的 `.Trashes/`，清空废纸篓前直接用 `ls .Trashes` 就能捞回来。
- 恢复目标是**同一块盘**时请立刻停止使用该盘；每写一个新文件都可能覆盖待恢复数据。

---

## 安全模型

- 设备只读打开（`OpenOptions::new().read(true)`），代码里没有写设备的分支；
- 所有写入都经过 `-o` 指定的输出目录，文件名经 `sanitize_name()` 去掉路径分隔符，避免目录穿越；
- 同名文件自动追加 `.1/.2` 后缀，绝不覆盖既有文件；
- 只读打开失败（macOS 权限）会明确提示用 `sudo`，而不是静默降级。

---

## 测试

```bash
cargo test          # 单元测试 + NTFS 合成镜像端到端测试
```

NTFS 没有可用的 mkntfs，所以测试里**按规范逐字节构造**了一个最小 NTFS 卷
（`crates/dmfs/tests/ntfs_synthetic.rs`）：引导扇区、MFT、`$FILE_NAME`、`$INDEX_ROOT`、
常驻/非常驻 `$DATA`、已删除记录、fixup 全部手写。这样每个偏移都被真实字节校验过，
也能长期当回归测试用。

真实镜像验证脚本（需要 macOS 的 `hdiutil`、`newfs_msdos`、`newfs_hfs`）：

```bash
./scripts/make-test-images.sh          # 生成 FAT16/FAT32/exFAT/HFS+ 镜像并写入+删除测试文件
./target/debug/dmde-rs ls -l /tmp/dmde-test/hfsraw.dmg
```

已验证的实测结果（本仓库开发时跑过）：

- FAT16：删除的 27 字节长文件名文件被完整恢复，内容与原文一致；
- exFAT：删除的 `gone.txt` 名字、大小、内容全部还原；
- ext4（`mke2fs -d` 生成）：目录树遍历 + 文件导出后 `cmp` 与原文件字节一致；
- HFS+：catalog 解析、目录树、文件读取正常（删除恢复按设计返回空并说明原因）。

---

## 已知限制

1. **只读**：没有扇区编辑/写回，也不打算在 v0.1 提供（写盘要配套"写前二次确认 + 备份"）。
2. **APFS 未解析**：macOS 系统盘目前只能看分区表。
3. **RAID / LVM / LDM** 未实现，动态盘、软 RAID 需要用镜像重组。
4. ext4 的 `meta_bg`、超大卷的块组定位可能不准；NTFS 的 `$ATTRIBUTE_LIST` 未展开。
5. HFS+ 的 extents overflow 文件未解析，单文件超过 8 个 extent 时会提示"extents 溢出"。
6. 无进度条细粒度回调（大卷扫描时只有开始/结束提示）。

## Roadmap

- [ ] 扇区十六进制查看/搜索（`hex` / `find` 命令）
- [ ] 签名雕刻（规则文件 + 头尾匹配 + 碎片拼接）
- [ ] 镜像/克隆（坏道跳过与重试、速度曲线）
- [ ] GPT 备份表恢复、丢失分区扫描重建
- [ ] APFS 容器与卷（含加密卷口令）
- [ ] Windows 后端（`PhysicalDrive`）
- [ ] 分区表/GPT 导出与写回（带二次确认）

## 与 DMDE 的差异

| | dmde-rs | DMDE |
|---|---|---|
| 授权 | MIT | 免费版有限制，专业版收费 |
| 平台 | macOS / Linux（Windows 规划） | Windows / macOS / Linux / DOS |
| 扇区编辑 | 未实现 | 完整 |
| 文件系统覆盖 | FAT/exFAT/NTFS/ext/HFS+ | 更多（ReFS、UFS、Btrfs、XFS…） |
| APFS | 仅识别 | 支持 |
| RAID 重建 | 未实现 | 支持 |
| 代码可审计 | 是（零依赖核心） | 闭源 |

目标不是一比一复刻，而是把「最常用的 80%」做成可审计、可自己改的开源工具。

## 许可

MIT，见 [LICENSE](LICENSE)。
