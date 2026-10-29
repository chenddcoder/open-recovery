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
| APFS | ✅ 已验证 | 容器/检查点/对象映射/fs-tree；多卷（`--volume`）、两种 drec 键格式 |
| APFS 删除恢复 | 🚧 部分 | `.Trashes` + 孤儿 inode；快照 diff 与雕刻待做 |
| 签名雕刻（carving） | ⏳ 规划 | HFS+/APFS 深删后只能靠它 |
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

## APFS：已经能读，恢复靠快照（重点）

**现状：容器 → 卷 → 目录 → 文件全链路打通**，`ls / tree / cat / extract` 与 FAT/NTFS 同一套命令。
这是本轮的主要工作量：APFS 不是「超级块 + 目录项」，而是**对象化、写时复制、多层 B-tree** 的系统。

### 实测出来的落盘布局（macOS 26）

| 结构 | 位置 / 要点 |
|---|---|
| `NXSB` 容器超级块 | 块 0；`magic@0x20`、`block_size@0x24`、`block_count@0x28`、`omap_oid@0xa0`、`fs_oid[]@0xb8`、检查点描述区 `desc_base@0x70` / `desc_blocks@0x68` |
| 检查点 | 每写一次事务新增一份 NXSB 副本；**必须取 xid 最大且校验通过的那份**，否则读到旧树 |
| `omap_phys` | `omap_oid` 是**物理** oid（就是块号）；`om_tree_oid@0x30` |
| `btree_node_phys` | 头 0x38；`flags@0x20`（ROOT/LEAF/FIXED_KV）、`level@0x22`、`nkeys@0x24`、`table_space@0x28` |
| `APSB` 卷超级块 | `omap_oid@0x80`、`root_tree_oid@0x88`、`snap_meta_tree_oid@0x98`、`num_files@0xb8`、`num_snapshots@0xd8`、`fs_flags@0x108`（bit0=未加密）、卷名 `@0x2c0` |

### 三个把解析卡住、最后靠实测钉死的细节

1. **值区偏移**：TOC 里的 `v` 是「从值区末尾往回数」的偏移，
   值起点 = `块尾 − (root ? 40 : 0) − v`。root 节点末尾有 40 字节 `btree_info_t`，
   不减掉它，所有记录都会整体错位 40 字节——表现得像「记录解析全错」。
2. **Fletcher64 是变体**：`hi = s2`、`lo = 0xFFFFFFFF − (s1+s2) mod`，等价于
   「整块（含 cksum 字段）的 s1 ≡ 0 (mod 2³²−1)」。按教科书 Fletcher 写会「高 32 位对、低 32 位错」。
3. **目录项名字有两种键格式**：`j_drec_hashed_key_t`（`u32 name_len_and_hash`，**低 10 位**才是名字长度，
   名字从 12 字节开始）与 `j_drec_key_t`（`u16 name_len`，名字从 10 字节开始）。
   触发条件是「大小写不敏感 **或** 规范化不敏感」，两者不等价：
   `newfs_apfs -e` 造的卷是「大小写敏感 + 规范化不敏感 + hashed 键」。
   代码不靠标志硬猜，而是用「名字长度必须等于键剩余长度」自校验，两种格式都能吃。

另外 inode 值里 xfields 的起点实测是 **92**（apfs.h 写 86，多 6 字节填充），同样按内容自校验定位。

### 删除恢复：APFS 上能救的与救不了的

| 途径 | 有效性 | 说明 |
|---|---|---|
| `/.Trashes/<uid>/` | ✅ 最实用 | macOS 删除只是移动过去；清空废纸篓前文件完整。`ls /.Trashes/501` 直接导出即可 |
| 本地快照 | ✅ 最强，**本版未做** | 快照里保留旧 fs-tree，按旧 xid 读出再与当前树 diff。下一版目标 |
| 孤儿 inode 扫描 | ⚠️ 少见 | `recover` 会做；真删后通常连 inode 一起回收，扫不到属正常 |
| 签名雕刻 | ⏳ 规划 | 兜底手段，不依赖目录结构 |

`recover` 在 APFS 上会先打印上面几条路径，再跑一次孤儿扫描（全树遍历，卷越大越慢）。

### 已验证范围（真镜像，不是「应该能跑」）

- `hdiutil create -fs APFS`：大小写不敏感卷，2 级目录、中文名、符号链接、硬链接、5 MB 多 extent 文件
  → `cat` 内容 md5 与原文件**逐字节一致**，全量 `extract` 206 个文件 0 失败
- `newfs_apfs -e`：大小写敏感卷，`ABC.txt` 与 `abc.txt` 共存且都能列出、读到正确内容
- 合成容器测试（`crates/dmfs/tests/apfs_synthetic.rs`）：64 块最小容器、**两个卷**（验证 `--volume`）、
  两种 drec 键格式、Fletcher 校验、坏块检测，6 项断言全过

### 限制

- **FileVault 加密卷**：`fs_flags` 未置「未加密」时直接报错并说明，不做解密（需要 keybag + AES-XTS）。
- **decmpfs 透明压缩文件**：读会明确报错并提示用 `ditto` / `afsctool -d` 解压，不会吐出压缩流冒充原文件。
- **sealed 系统卷**：只读访问没问题，但不校验哈希树。
- **快照**：只显示数量，尚未按快照读旧树。
- **目录大小**：APFS 若没有 `j_dir_stats_val_t` 记录（实测 hdiutil 造的卷、以及这块 WD 盘都没有），
  只能递归累加，因此设了 8 秒预算；超大目录会显示 `≥` 下界而不是精确值。

---

---

## 实战：一块 2 TB Mac 盘的分区"消失"了

这不是构造出来的案例，是真实救回来的一块 WD 移动硬盘（`/dev/rdisk4`，RTL9210B 桥接）。

### 现象

```
diskutil list
/dev/disk4 (external, physical):
   0:      GUID_partition_scheme                        *2.0 TB     disk4
```

`diskutil` 只显示分区表，**一个分区都不列**，`/dev/disk4s1`、`disk4s2` 设备节点根本不存在，
系统里既看不到盘也挂不动。表面看像"分区表被抹了"。

### 实际诊断（`scripts/probe-disk.py`）

直接按字节读，结论和表面完全相反：

| 检查项 | 结果 |
|---|---|
| 保护性 MBR | 正常（类型 0xEE） |
| GPT 主头 CRC32 | **通过**（`存储 0x86fc13ae = 计算 0x86fc13ae`）|
| GPT 备份头 CRC32 | **通过** |
| 分区条目 | **2 条都在**：`EFI 200MB` + `Apple APFS 1.82TB`（LBA 409640..3907029127）|
| APFS 容器块 0 | **被随机字节覆盖**（正常应是 `NXSB`）|
| 检查点副本 | 块 2、4、6…280 共 140+ 份，**全部校验通过** |

也就是说：分区表没丢，数据没丢，**只是容器块 0 被打坏了**，而系统在分区扫描阶段就放弃，
连设备节点都不建 —— 于是看起来"整块盘没了"。

### 修好的三个深层 bug

这三处都是小镜像碰巧能跑、真盘一上就彻底读不出的类型：

1. **omap 键不能按字节比**。`omap_key_t` 是 `{u64 oid; u64 xid}` 小端，
   字节序比较会把 `oid=1051`（字节 `1b 04`）判成**大于** `oid=178945`（`01 bb 02`），
   起始键一跳过就报"omap 里找不到对象"。
2. **fs-tree 的 `j_key` 排序语义是"先 obj_id 再 type"**。它是 `(type<<60)|obj_id` 的小端 u64，
   用字节序比较时，obj_id 一旦跨过 256 / 65536 的进位边界就整棵树错位。
   症状极具迷惑性：**根目录能正常列出，所有子目录却都显示"空目录"**。
3. **索引节点（level>0）的值固定 8 字节**（子节点 oid），不能按叶子的 `val_size`(16) 取，
   否则值区顶出块外 8 字节，报「第 0 项值越界（4048..4064）」。

修完之后同一块盘：

```
open  : /dev/rdisk4 分区2（Apple APFS） · 卷「WD - Data」   (0.9s)
卷标  : WD - Data        容量 1.82 TiB
fs-tree: 35,721,962 条键 / 953,023 个节点（树高 4）
/Users/chendd          : 80 项
/Users/chendd/Documents: games, huan, learn, self, whisky, 图片 …
```

`cat .gitconfig` 读出的内容与原文逐字节一致，导出到本机也验证通过。

### 这类盘怎么救（操作顺序）

```bash
# 1) 只读诊断，确认分区表与文件系统头还在
sudo python3 scripts/probe-disk.py /dev/rdisk4

# 2) 直接按偏移打开 APFS 分区（分区表在，但系统不认的情况）
./target/release/dmde-rs ls /dev/rdisk4 /Users \
    --offset 209735680 --size 2000189177856

# 3) 挑目录导出
./target/release/dmde-rs extract /dev/rdisk4 /Users/chendd/Documents \
    --offset 209735680 --size 2000189177856 -o /Volumes/目标盘/rescued
```

GUI 里更简单：点这块盘 → 自动跳过 EFI、选中 APFS 分区和 `- Data` 卷 → 浏览目录 →
「导出当前目录（含子目录）」。全程只读，不需要修分区表，也不写原盘。

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
→ 单个「保存」或「导出当前目录（含子目录）」到输出目录。

**导出前可以先看多大**：右侧「当前目录大小 → 计算大小」。带 8 秒时间预算，
算不完就明确显示 `≥ 3.2 GB（未完成，实际更大）`，绝不把半截统计当精确值。

**同名文件**下拉框决定目标已存在时怎么办：

| 策略 | 行为 | 适用 |
|---|---|---|
| 跳过（默认） | 已存在就不读不写，只计数 | 断点续导、重复导出同一目录 |
| 保留两份 | 新文件改名 `名字 (1).扩展名` | 怕覆盖，想都留着对比 |
| 覆盖 | 直接替换 | 确认上次导坏、要重来 |

批量恢复（「恢复已删除」）固定用「保留两份」，不会覆盖你在输出目录里的旧结果。

导出期间右下角显示滚动进度（文件数 / 目录数 / 已写字节 / 当前路径），旁边出现
**「暂停导出」**。暂停是**文件内部也生效**的：写盘时每个分块都查一次标志位，
所以几十 GB 的单文件也能立刻停下，不用等它写完。停下时：

* 已完整写完的文件全部保留；
* 正在写的那一个改名为 `<原名>.partial`（保留已恢复的字节，但绝不冒充完整文件）；
* 后端会话归还、按钮恢复，可以直接换个目录接着导。

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
cargo test          # 单元测试 + NTFS/FAT/APFS 合成镜像端到端测试
```

NTFS 没有可用的 mkntfs，APFS 更没法用 `hdiutil` 造出「多卷 + 大小写敏感 + 指定 xid」
这类组合，所以这两者都在测试里**按规范逐字节构造**最小卷：

- `crates/dmfs/tests/ntfs_synthetic.rs`：引导扇区、MFT、`$FILE_NAME`、`$INDEX_ROOT`、
  常驻/非常驻 `$DATA`、已删除记录、fixup；
- `crates/dmfs/tests/apfs_synthetic.rs`：64 块容器、NXSB + 检查点、容器/卷两张 omap、
  两个卷（不同 drec 键格式）、fs-tree 的 drec/inode/extent、Fletcher64 自校验与坏块检测；
- `crates/dmfs/tests/fat_synthetic.rs`：FAT 目录项与 LFN。

这样每个偏移都被真实字节校验过，也能长期当回归测试用（改解析代码时立刻能发现偏移写错）。

真实镜像验证脚本（需要 macOS 的 `hdiutil`、`newfs_msdos`、`newfs_hfs`）：

```bash
./scripts/make-test-images.sh          # 生成 FAT16/FAT32/exFAT/HFS+ 镜像并写入+删除测试文件
./target/debug/dmde-rs ls -l /tmp/dmde-test/hfsraw.dmg
```

已验证的实测结果（本仓库开发时跑过）：

- FAT16：删除的 27 字节长文件名文件被完整恢复，内容与原文一致；
- exFAT：删除的 `gone.txt` 名字、大小、内容全部还原；
- ext4（`mke2fs -d` 生成）：目录树遍历 + 文件导出后 `cmp` 与原文件字节一致；
- HFS+：catalog 解析、目录树、文件读取正常（删除恢复按设计返回空并说明原因）；
- APFS（`hdiutil create -fs APFS`）：5 MB 多 extent 文件 `cat` 后 md5 与源文件一致，
  全量导出 206 个文件 0 失败；中文名、符号链接、硬链接、`.Trashes` 均正确；
- APFS（`newfs_apfs -e`）：大小写敏感卷，`ABC.txt`/`abc.txt` 共存且各自内容正确。

---

## 已知限制

1. **只读**：没有扇区编辑/写回，也不打算在 v0.1 提供（写盘要配套"写前二次确认 + 备份"）。
2. **APFS 恢复能力有限**：能完整浏览/导出存活文件，但清空废纸篓后的文件只能靠快照；
   加密卷、decmpfs 压缩文件不支持。
3. **RAID / LVM / LDM** 未实现，动态盘、软 RAID 需要用镜像重组。
4. ext4 的 `meta_bg`、超大卷的块组定位可能不准；NTFS 的 `$ATTRIBUTE_LIST` 未展开。
5. HFS+ 的 extents overflow 文件未解析，单文件超过 8 个 extent 时会提示"extents 溢出"。
6. 无进度条细粒度回调（大卷扫描时只有开始/结束提示）。

## Roadmap

- [ ] 扇区十六进制查看/搜索（`hex` / `find` 命令）
- [ ] 签名雕刻（规则文件 + 头尾匹配 + 碎片拼接）
- [ ] 镜像/克隆（坏道跳过与重试、速度曲线）
- [ ] GPT 备份表恢复、丢失分区扫描重建
- [x] APFS 容器、多卷、fs-tree 浏览与导出
- [ ] APFS 快照列表与按旧 xid 恢复删除文件
- [ ] APFS 加密卷（keybag + AES-XTS）
- [ ] Windows 后端（`PhysicalDrive`）
- [ ] 分区表/GPT 导出与写回（带二次确认）

## 与 DMDE 的差异

| | dmde-rs | DMDE |
|---|---|---|
| 授权 | MIT | 免费版有限制，专业版收费 |
| 平台 | macOS / Linux（Windows 规划） | Windows / macOS / Linux / DOS |
| 扇区编辑 | 未实现 | 完整 |
| 文件系统覆盖 | FAT/exFAT/NTFS/ext/HFS+ | 更多（ReFS、UFS、Btrfs、XFS…） |
| APFS | 浏览/导出（不含快照恢复） | 支持 |
| RAID 重建 | 未实现 | 支持 |
| 代码可审计 | 是（零依赖核心） | 闭源 |

目标不是一比一复刻，而是把「最常用的 80%」做成可审计、可自己改的开源工具。

## 许可

MIT，见 [LICENSE](LICENSE)。
