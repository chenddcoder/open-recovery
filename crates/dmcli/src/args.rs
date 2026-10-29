//! 极简参数解析（零依赖）。

#[derive(Debug, Default)]
pub struct Opts {
    pub cmd: String,
    pub positional: Vec<String>,
    pub partition: Option<usize>,
    pub offset: Option<u64>,
    pub size: u64,
    pub out: Option<String>,
    pub long: bool,
    pub deleted: bool,
    pub depth: usize,
    pub min_size: u64,
    pub limit: usize,
    pub dry_run: bool,
    pub verbose: bool,
    pub json: bool,
    /// APFS 容器内的卷序号（0 起）
    pub volume: usize,
    pub help: bool,
    pub version: bool,
}

pub const HELP: &str = r#"dmde-rs —— 开源磁盘编辑器与数据恢复工具（只读）

用法:
  dmde-rs <命令> <源> [参数...]

命令:
  devices                     列出本机物理磁盘
  volumes    <源>             解析分区表，并探测每个分区的文件系统
  info       <源>             显示文件系统详细信息
  ls         <源> [路径]      列目录（支持已删除项）
  tree       <源> [路径]      递归列出目录树
  cat        <源> <路径>      输出文件内容到标准输出
  extract    <源> [路径]      导出目录/文件到本地
  recover    <源>             批量恢复已删除文件
  help                        显示本帮助

源 (source):
  物理盘   /dev/rdisk4          （macOS 需要 sudo）
  分区     /dev/rdisk4s1
  镜像     disk.img / disk.dmg  （.dmg 请先 hdiutil convert -format UDRW）

通用参数:
  -p, --partition <N>   指定第 N 个分区（从 1 开始）
      --volume <N>      APFS 容器内的卷序号（0 起；info 会列出每个卷）
      --offset <字节>   手动指定卷起始偏移
      --size <字节>     手动指定卷长度
  -v, --verbose         输出更多细节
  -V, --version         显示版本
      --json            以 JSON 输出（部分命令支持）

命令参数:
  ls:      -l/--long 详细列表   --deleted 只显示已删除项
  tree:    --depth <N> 最大深度（默认 3）
  cat:     -o <文件> 写入文件而不是标准输出
  extract: -o <目录> 输出目录（必填）  --deleted 同时导出已删除文件
  recover: -o <目录> 输出目录（必填）  --min-size <字节>  --limit <N>
           --dry-run 只列出不写盘

示例:
  sudo dmde-rs devices -v
  sudo dmde-rs volumes /dev/rdisk4
  sudo dmde-rs ls /dev/rdisk4 -p 2
  dmde-rs ls backup.img --deleted
  dmde-rs extract backup.img /photos -o ~/rescued
  sudo dmde-rs recover /dev/rdisk4 -p 1 -o ~/rescued --dry-run
"#;

pub fn parse(args: Vec<String>) -> Result<Opts, String> {
    let mut o = Opts { depth: 3, limit: 0, size: 0, ..Default::default() };
    let mut it = args.into_iter().peekable();
    // 第一个非选项参数是命令
    while let Some(a) = it.next() {
        if a.starts_with('-') {
            return Err(format!("未知选项出现在命令之前: {a}"));
        }
        o.cmd = a;
        break;
    }
    while let Some(a) = it.next() {
        match a.as_str() {
            "-h" | "--help" => o.help = true,
            "-l" | "--long" => o.long = true,
            "--deleted" => o.deleted = true,
            "--dry-run" => o.dry_run = true,
            "-V" | "--version" => o.version = true,
            "-v" | "--verbose" => o.verbose = true,
            "--json" => o.json = true,
            "-p" | "--partition" => {
                let v = it.next().ok_or("--partition 缺少参数")?;
                o.partition = Some(v.parse().map_err(|_| format!("分区号非法: {v}"))?);
            }
            "-o" | "--out" => {
                o.out = Some(it.next().ok_or("-o 缺少参数")?);
            }
            "--volume" => {
                let v = it.next().ok_or("--volume 缺少参数")?;
                o.volume = v.parse().map_err(|_| format!("卷序号非法: {v}"))?;
            }
            "--offset" => {
                let v = it.next().ok_or("--offset 缺少参数")?;
                o.offset = Some(parse_num(&v)?);
            }
            "--size" => {
                let v = it.next().ok_or("--size 缺少参数")?;
                o.size = parse_num(&v)?;
            }
            "--depth" => {
                let v = it.next().ok_or("--depth 缺少参数")?;
                o.depth = v.parse().map_err(|_| format!("深度非法: {v}"))?;
            }
            "--min-size" => {
                let v = it.next().ok_or("--min-size 缺少参数")?;
                o.min_size = parse_num(&v)?;
            }
            "--limit" => {
                let v = it.next().ok_or("--limit 缺少参数")?;
                o.limit = v.parse().map_err(|_| format!("数量非法: {v}"))?;
            }
            _ if a.starts_with("--") => return Err(format!("未知选项: {a}")),
            _ if a.starts_with('-') && a.len() > 1 => return Err(format!("未知选项: {a}")),
            _ => o.positional.push(a),
        }
    }
    Ok(o)
}

/// 支持 1K / 2M / 3G 这种带单位的数字
pub fn parse_num(s: &str) -> Result<u64, String> {
    let t = s.trim();
    if t.is_empty() {
        return Err("空数字".to_string());
    }
    let (num, mul) = match t.chars().last().unwrap().to_ascii_uppercase() {
        'K' => (&t[..t.len() - 1], 1024u64),
        'M' => (&t[..t.len() - 1], 1024 * 1024),
        'G' => (&t[..t.len() - 1], 1024 * 1024 * 1024),
        'T' => (&t[..t.len() - 1], 1024u64 * 1024 * 1024 * 1024),
        _ => (t, 1),
    };
    num.trim()
        .parse::<u64>()
        .map(|v| v * mul)
        .map_err(|_| format!("数字非法: {s}"))
}
