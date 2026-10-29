//! 导出时的「跳过规则」。
//!
//! 从一块坏盘里捞数据，时间基本都花在**读盘**上：一个 `node_modules` 动辄几万个
//! 小文件、几百 MB 到几 GB，而它的内容全都能用 `npm install` 重新装回来。
//! 恢复时把它连同 `.git`、`__pycache__` 这类派生目录一起跳过，
//! 省下的是实打实的读盘时间和目标盘空间。
//!
//! 规则本身很简单，就两条匹配方式：
//!
//! * 不含 `/` 的模式 → 只和**条目的名字**比（`node_modules`、`*.log`）；
//! * 含 `/` 的模式   → 和**源路径**比（`*/Library/Caches/*`），用来精确排除某棵子树。
//!
//! 一律大小写不敏感（APFS 默认就是大小写不敏感，`Node_Modules` 和 `node_modules`
//! 是同一个东西，规则要认出它们）。通配符只支持 `*`（任意长）和 `?`（单个字符），
//! 不引第三方 crate —— 这个 crate 里连正则都没有，一个手写匹配足够。

/// 默认跳过的名字。挑的原则：**删掉也能重新生成**的依赖/缓存/系统元数据。
///
/// 故意不放进去的：`dist`、`build`、`target`、`vendor`、`Caches`。
/// 前几个在不少项目里是产物，但也有人的 `target` 里就放着自己的资料；
/// `Caches` 更是常常装着想留的东西。这类边界交给用户自己在界面上加。
pub const DEFAULT_SPEC: &str = concat!(
    // 前端/后端依赖
    "node_modules, bower_components, jspm_packages, ",
    // 版本库元数据（体积大、且能重新 clone）
    ".git, .svn, .hg, ",
    // Python
    "__pycache__, .pytest_cache, .mypy_cache, .ruff_cache, .tox, .venv, ",
    // 前端构建缓存
    ".next, .nuxt, .svelte-kit, .parcel-cache, .turbo, .cache, .eslintcache, ",
    // 构建工具缓存
    ".gradle, .idea, DerivedData, ",
    // 系统垃圾与本卷快照索引
    ".Trash, .Spotlight-V100, .fseventsd, .DocumentRevisions-V100, .TemporaryItems, ",
    ".com.apple.timemachine.donotpresent, ",
    // 系统垃圾文件（macOS / Windows）
    ".DS_Store, ._*, .AppleDouble, Thumbs.db, desktop.ini"
);

/// 一组跳过规则。
#[derive(Debug, Clone, Default)]
pub struct SkipRules {
    patterns: Vec<String>,
}

impl SkipRules {
    /// 用默认规则集。
    pub fn defaults() -> Self {
        Self::parse(DEFAULT_SPEC)
    }

    /// 解析用户给的规格串：逗号 / 分号 / 空白都能当分隔符。
    ///
    /// 空串 = 一条规则都没有（用户把列表清空就是想「全都恢复」，
    /// 这时不能再偷偷把默认值套回去）。
    pub fn parse(spec: &str) -> Self {
        let patterns = spec
            .split([',', ';', '\n', '\r', '\t', ' '])
            .map(|s| s.trim().trim_start_matches("./"))
            .filter(|s| !s.is_empty())
            .map(|s| s.to_lowercase())
            .collect();
        Self { patterns }
    }

    /// 兼容「没传」和「传了空串」两种语义。
    pub fn from_opt(spec: Option<&str>) -> Self {
        match spec {
            Some(s) => Self::parse(s),
            None => Self::defaults(),
        }
    }

    pub fn is_empty(&self) -> bool {
        self.patterns.is_empty()
    }

    pub fn len(&self) -> usize {
        self.patterns.len()
    }

    /// `name` 是条目名，`path` 是它在源盘里的完整路径（用于含 `/` 的规则）。
    pub fn matches(&self, name: &str, path: &str) -> bool {
        if self.patterns.is_empty() {
            return false;
        }
        let name = name.to_lowercase();
        let path = path.to_lowercase();
        self.patterns.iter().any(|p| {
            if p.contains('/') {
                glob_match(p, &path)
            } else {
                glob_match(p, &name)
            }
        })
    }
}

/// 通配符匹配：`*` 任意长度（含空）、`?` 恰好一个字符。
///
/// 回溯版：两个指针 + 记住最近一个 `*` 的位置，最坏 O(n·m)、实际接近线性。
/// 递归写法遇到 `"*a*a*a*..."` 这种长模式会爆栈，这里不用递归。
fn glob_match(pat: &str, text: &str) -> bool {
    let p: Vec<char> = pat.chars().collect();
    let t: Vec<char> = text.chars().collect();
    let (mut pi, mut ti) = (0usize, 0usize);
    let (mut star, mut mark) = (usize::MAX, 0usize);

    while ti < t.len() {
        if pi < p.len() && (p[pi] == '?' || p[pi] == t[ti]) {
            pi += 1;
            ti += 1;
        } else if pi < p.len() && p[pi] == '*' {
            star = pi;
            mark = ti;
            pi += 1;
        } else if star != usize::MAX {
            // 上一次的 `*` 多吃一个字符，再试
            pi = star + 1;
            mark += 1;
            ti = mark;
        } else {
            return false;
        }
    }
    while pi < p.len() && p[pi] == '*' {
        pi += 1;
    }
    pi == p.len()
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 用户诉求的原点：`node_modules` 必须被默认规则拦下。
    #[test]
    fn default_rules_cover_node_modules() {
        let r = SkipRules::defaults();
        assert!(r.matches("node_modules", "/Users/x/proj/node_modules"));
        // APFS 大小写不敏感，换个写法也得认出来
        assert!(r.matches("Node_Modules", "/x/Node_Modules"));
        assert!(r.matches(".git", "/repo/.git"));
        assert!(r.matches("__pycache__", "/py/__pycache__"));
        assert!(r.matches(".DS_Store", "/Users/x/.DS_Store"));
        // 正常目录一个都不能误伤
        for n in ["Documents", "node_modules_old", "src", "Photo", "gin", "gits"] {
            assert!(!r.matches(n, &format!("/Users/x/{n}")), "{n} 不该被跳过");
        }
    }

    /// 清空列表 = 全都要，不能再偷偷套默认值。
    #[test]
    fn empty_spec_skips_nothing() {
        let r = SkipRules::from_opt(Some(""));
        assert!(r.is_empty());
        assert!(!r.matches("node_modules", "/x/node_modules"));
    }

    /// 没传 spec 才用默认值。
    #[test]
    fn none_means_defaults() {
        assert!(!SkipRules::from_opt(None).is_empty());
        assert!(SkipRules::from_opt(None).matches("node_modules", "/x/node_modules"));
    }

    /// 用户自定义：逗号/空格混用、大小写随意、支持通配符。
    #[test]
    fn custom_spec_parsing() {
        let r = SkipRules::parse(" build ,  *.log;Logs,\n.cache ");
        assert_eq!(r.len(), 4);
        assert!(r.matches("build", "/p/build"));
        assert!(r.matches("a.log", "/p/a.log"));
        assert!(r.matches("Logs", "/p/Logs"));
        assert!(!r.matches("a.log.bak", "/p/a.log.bak"));
        assert!(!r.matches("builder", "/p/builder"));
    }

    /// 含 `/` 的规则匹配整条源路径，用来排除某棵子树。
    #[test]
    fn path_patterns_match_full_path() {
        let r = SkipRules::parse("*/Library/Caches/*");
        assert!(r.matches("com.apple.Safari", "/Users/x/Library/Caches/com.apple.Safari"));
        assert!(!r.matches("Caches", "/Users/x/Library/Caches"));
        assert!(!r.matches("Documents", "/Users/x/Documents"));
    }

    /// `?` 只吃一个字符，`*` 可以吃零个。
    #[test]
    fn glob_edge_cases() {
        assert!(glob_match("a?c", "abc"));
        assert!(!glob_match("a?c", "abbc"));
        assert!(glob_match("*.tmp", "x.tmp"));
        assert!(glob_match("a*", "a"));
        assert!(glob_match("a*b*c", "axxbyyc"));
        assert!(!glob_match("a*b*c", "axxbyy"));
        assert!(glob_match("._*", "._foo"));
        assert!(glob_match("*", "任意中文"));
    }
}
