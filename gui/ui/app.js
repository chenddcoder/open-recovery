// dmde-rs 前端：只用 Tauri 的 invoke，不引任何框架。
const invoke = window.__TAURI__.core.invoke;

const state = {
  source: null,
  path: "/",
  showDeleted: false,
  entries: [],
  selected: null,
  outDir: "",
};

const $ = (id) => document.getElementById(id);

function setStatus(msg, kind = "info") {
  const el = $("status");
  el.textContent = msg;
  el.style.color = kind === "error" ? "var(--danger)" : kind === "ok" ? "var(--ok)" : "";
}

function fmtSize(n) {
  if (n === 0) return "0 B";
  const units = ["B", "KiB", "MiB", "GiB", "TiB"];
  let v = n, i = 0;
  while (v >= 1024 && i + 1 < units.length) { v /= 1024; i++; }
  return `${v >= 100 ? v.toFixed(1) : v.toFixed(2)} ${units[i]}`;
}

function fmtTime(ts) {
  if (!ts) return "-";
  const d = new Date(ts * 1000);
  const p = (x) => String(x).padStart(2, "0");
  return `${d.getFullYear()}-${p(d.getMonth() + 1)}-${p(d.getDate())} ${p(d.getHours())}:${p(d.getMinutes())}`;
}

// ---------------- 磁盘与分区 ----------------

async function loadDisks() {
  const box = $("disks");
  box.innerHTML = `<p class="hint">读取中…</p>`;
  try {
    const disks = await invoke("list_disks");
    if (!disks.length) {
      box.innerHTML = `<p class="hint">没有找到物理磁盘（或平台暂不支持枚举）</p>`;
      return;
    }
    box.innerHTML = "";
    for (const d of disks) {
      const el = document.createElement("div");
      el.className = "item";
      el.innerHTML = `<span class="name">${d.dev_node}</span>
        <span class="meta">${d.size_human}</span>`;
      el.title = `${d.model} · ${d.bus || "未知总线"}${d.internal ? " · 内置" : " · 外置"}`;
      el.onclick = () => openSource(d.dev_node);
      box.appendChild(el);
    }
  } catch (e) {
    box.innerHTML = `<p class="hint" style="color:var(--danger)">读取失败：${e}<br/>物理盘需要管理员权限。</p>`;
  }
}

// WebView 的 console 外面看不到，关键路径写文件（~/Library/Application Support/dmde-rs/ui.log）
function tlog(msg) {
  try {
    invoke("ui_log", { msg: String(msg) });
  } catch (_) {}
}

let openingSource = false;

async function openSource(path, partition = null) {
  // 连点两下会起两个打开流程：一个在等授权窗、另一个已经成功，
  // 状态栏就会卡在"正在打开"，界面也不再刷新。这里直接挡住重复进入。
  if (openingSource) {
    tlog(`open ignored (already opening): ${path}`);
    return;
  }
  openingSource = true;
  const needsAdmin = path.startsWith("/dev/");
  setStatus(needsAdmin ? `正在打开 ${path}，若需要管理员权限会弹出系统授权窗…` : `正在打开 ${path} …`);
  tlog(`open start ${path}`);
  try {
    const src = await invoke("open_source", { path, partition });
    state.source = src;
    renderPartitions();
    renderVolume(src);
    tlog(`open ok fs=${src.fs_kind} label=${src.label}`);
    setStatus(`已挂载 ${src.desc} · ${src.fs_kind}`, "ok");
    // 不要 await：admin_state 要连一次 helper socket，慢的时候会把"列出目录"拖后几秒
    updateAdmin();
    await navigate("/");
    tlog("navigate / done");
  } catch (e) {
    tlog(`open FAILED: ${e}`);
    setStatus(String(e), "error");
    updateAdmin();
  } finally {
    openingSource = false;
  }
}

// ---------------- 权限 ----------------

async function updateAdmin() {
  const pill = $("priv-pill");
  try {
    const st = await invoke("admin_state");
    if (st.active) {
      pill.className = "pill admin";
      pill.textContent = "只读 · 管理员权限";
      pill.title = "当前会话由只读 helper 提供（root 读、本进程写）";
    } else if (st.helper) {
      pill.className = "pill warn";
      pill.textContent = "只读 · 已获权限（未使用）";
      pill.title = "helper 在线，但当前卷不需要提权";
    } else {
      pill.className = "pill safe";
      pill.textContent = "只读模式 · 未提权";
      pill.title = "读取原始磁盘需要管理员权限；helper 只做读";
    }
  } catch (_) {
    pill.className = "pill safe";
    pill.textContent = "只读模式";
  }
}

async function requestAdmin() {
  setStatus("等待 macOS 授权窗…（密码只用于启动只读 helper，不写任何设备）");
  try {
    const msg = await invoke("request_admin");
    setStatus(msg, "ok");
  } catch (e) {
    setStatus(String(e), "error");
  }
  await updateAdmin();
}

function renderPartitions() {
  const wrap = $("partitions-wrap");
  const box = $("partitions");
  box.innerHTML = "";
  const parts = state.source?.partitions || [];
  if (!parts.length) { wrap.classList.add("hidden"); return; }
  wrap.classList.remove("hidden");
  for (const p of parts) {
    const el = document.createElement("div");
    el.className = "item" + (p.mountable ? "" : " disabled");
    el.innerHTML = `<span class="name">#${p.index} ${p.fs_kind}</span>
      <span class="meta">${p.size_human}</span>`;
    el.title = p.type_name;
    if (p.mountable) {
      el.onclick = () => openSource(state.source.desc.split(" 分区")[0], p.index);
    }
    box.appendChild(el);
  }
}

function renderVolume(src) {
  const card = $("volume-card");
  card.classList.remove("hidden");
  const rows = [
    ["文件系统", src.fs_kind],
    ["卷标", src.label || "（无）"],
    ["总容量", src.total_human],
    ["块大小", src.block_size + " B"],
    ["分区表", src.scheme + (src.verified ? " ✓" : "（校验未通过）")],
  ];
  if (src.uuid) rows.push(["UUID", src.uuid]);
  $("volume-info").innerHTML = rows
    .map(([k, v]) => `<dt>${k}</dt><dd>${v}</dd>`)
    .join("");
}

// ---------------- 浏览 ----------------

async function navigate(path) {
  state.path = path;
  clearSelection();
  renderCrumbs();
  await refresh();
}

function clearSelection() {
  state.selected = null;
  document.querySelectorAll(".row.selected").forEach((r) => r.classList.remove("selected"));
  $("preview-meta").textContent = "未选择文件";
  $("preview-body").textContent = "";
  updateExportTarget();
}

/// 一个「导出」按钮，按选中项决定导什么。之前只有「导出当前目录」，
/// 想单独捞一个文件只能点「保存选中文件」——而那条路是同步读写，
/// 几 GB 的文件会把窗口卡死，也不支持暂停。现在统一成一个入口。
function updateExportTarget() {
  const b = $("btn-export");
  if (!b) return;
  const sel = state.selected;
  if (sel && sel.is_dir) {
    b.textContent = "导出选中目录（含子目录）";
    b.title = `${sel.path} → 整棵子树`;
  } else if (sel) {
    b.textContent = `导出选中文件：${sel.name}`;
    b.title = `${sel.path} → 只导这一个文件`;
  } else {
    b.textContent = "导出当前目录（含子目录）";
    b.title = `${state.path} → 整棵子树`;
  }
}

async function refresh() {
  const box = $("entries");
  // 列表要重建，之前的行元素会消失；选中状态跟着作废，
  // 否则按钮还停在"导出选中文件：xxx"上，实际那个文件已经不在列表里了。
  clearSelection();
  box.innerHTML = `<p class="empty">读取中…</p>`;
  try {
    const items = await invoke("list_dir", {
      path: state.path,
      showDeleted: state.showDeleted,
    });
    state.entries = items;
    tlog(`ls ${state.path} -> ${items.length} 项`);
    renderEntries(items);
  } catch (e) {
    tlog(`ls FAIL ${state.path}: ${e}`);
    box.innerHTML = `<p class="empty" style="color:var(--danger)">${e}</p>`;
  }
}

function renderCrumbs() {
  const box = $("crumbs");
  box.innerHTML = "";
  const parts = state.path.split("/").filter(Boolean);
  const root = document.createElement("button");
  root.textContent = "/";
  root.onclick = () => navigate("/");
  box.appendChild(root);
  let acc = "";
  parts.forEach((p, i) => {
    const sep = document.createElement("span");
    sep.className = "sep";
    sep.textContent = "/";
    box.appendChild(sep);
    acc += "/" + p;
    const btn = document.createElement("button");
    btn.textContent = p;
    const target = acc;
    btn.onclick = () => navigate(target);
    box.appendChild(btn);
  });
}

function iconFor(e) {
  if (e.is_dir) return "▸";
  return "·";
}

function renderEntries(items) {
  const box = $("entries");
  box.innerHTML = "";

  if (!items.length) {
    box.innerHTML = `<p class="empty">（空目录）</p>`;
    updateStats();
    return;
  }
  for (const e of items) {
    const row = document.createElement("div");
    row.className = "row" + (e.is_dir ? " is-dir" : "");
    row.innerHTML = `
      <span class="col-name"><span class="icon">${iconFor(e)}</span>${e.name}</span>
      <span class="col-size">${e.is_dir ? "-" : fmtSize(e.size)}</span>
      <span class="col-time">${fmtTime(e.mtime)}</span>
      <span class="col-tag">${e.deleted ? '<span class="tag deleted">已删除</span>' : e.is_dir ? '<span class="tag dir">目录</span>' : ""}</span>`;
    row.onclick = () => selectEntry(e, row);
    row.ondblclick = () => { if (e.is_dir) navigate(e.path); };
    box.appendChild(row);
  }
  updateStats();
}

function updateStats() {
  const files = state.entries.filter((e) => !e.is_dir);
  const total = files.reduce((s, e) => s + e.size, 0);
  $("stats").textContent = `${state.entries.length} 项 · ${files.length} 个文件 · ${fmtSize(total)}`;
}

async function selectEntry(e, row) {
  document.querySelectorAll(".row.selected").forEach((r) => r.classList.remove("selected"));
  row?.classList.add("selected");
  state.selected = e;
  updateExportTarget();
  $("preview-meta").textContent = `${e.path} · ${e.is_dir ? "目录" : fmtSize(e.size)}`;
  if (e.is_dir) {
    $("preview-body").textContent = "目录：双击进入";
    return;
  }
  $("preview-body").textContent = "读取中…";
  try {
    const p = await invoke("preview", { id: e.id, path: e.path, max: 256 * 1024 });
    const head = p.truncated ? `（仅显示前 256 KiB，文件共 ${fmtSize(p.size)}）\n\n` : "";
    $("preview-body").textContent = head + p.text;
  } catch (err) {
    $("preview-body").textContent = "预览失败：" + err;
  }
}

// ---------------- 动作 ----------------

async function recoverAll() {
  if (!state.source) { setStatus("先打开一个源", "error"); return; }
  const outDir = $("input-out").value.trim();
  if (!outDir) { setStatus("请先填写输出目录", "error"); return; }
  setStatus("正在恢复…（要先把已删除条目扫一遍，APFS 上比较慢）");
  try {
    const r = await invoke("recover_all", { outDir, minSize: 0, limit: 0 });
    setStatus(`恢复完成：成功 ${r.ok} 个，失败 ${r.failed} 个，共 ${r.bytes_human} → ${r.out_dir}`, "ok");
  } catch (e) {
    setStatus(String(e), "error");
  }
}

// ---------------- 事件绑定 ----------------

$("btn-refresh").onclick = loadDisks;

// ---------------- 导出进度 ----------------
// 后端每 120ms 最多发一帧，避免几万个小文件把 WebView 拖死
let exportListener = null;

async function ensureExportListener() {
  if (exportListener) return;
  const ev = window.__TAURI__ && window.__TAURI__.event;
  if (!ev) return; // 老版本没有 event API 时静默降级
  exportListener = await ev.listen("export-progress", (e) => {
    const p = e.payload || {};
    const box = $("export-progress");
    if (p.done) {
      box.hidden = true;
      return;
    }
    box.hidden = false;
    const parts = [`${p.files} 个文件`, `${p.dirs} 个目录`, fmtSize(p.bytes || 0)];
    if (p.skipped) parts.push(`跳过 ${p.skipped}`);
    if (p.skipped_rule) parts.push(`按规则跳过 ${p.skipped_rule}`);
    if (p.failed) parts.push(`失败 ${p.failed}`);
    $("export-progress-text").textContent = `${parts.join(" · ")}\n${p.current || ""}`;
  });
}

// 暂停正在进行的导出：只置后端标志位，导出线程会在下一个分块处退出
$("btn-cancel-export").onclick = async () => {
  const b = $("btn-cancel-export");
  b.disabled = true;
  b.textContent = "正在暂停…";
  try {
    await invoke("cancel_export");
    setStatus("已请求暂停，正在收尾（当前文件会改名为 .partial）…");
  } catch (e) {
    setStatus(String(e), "error");
    b.disabled = false;
    b.textContent = "暂停导出";
  }
};

// 导出当前浏览的目录（递归）；这是"挑着恢复"的主入口
$("btn-export").onclick = async () => {
  if (!state.source) {
    setStatus("先打开一个源", "error");
    return;
  }
  const outDir = ($("input-out").value || "").trim() || state.outDir;
  if (!outDir) {
    setStatus("请先填写输出目录", "error");
    return;
  }
  const sel = $("sel-conflict");
  const onConflict = sel ? sel.value : "skip";
  // 跳过规则：清空表示全都要恢复；规则本身按名字匹配，命中的目录整棵不读
  const skipInput = $("input-skip");
  const skipRules = skipInput ? skipInput.value.trim() : "";
  if (skipInput) {
    try {
      localStorage.setItem("dmde.skipRules", skipRules);
    } catch (_) {}
  }
  // 选中文件 → 只导这一个（按 id 读，已删除条目也能导出；不套跳过规则）；
  // 选中目录 / 未选中 → 走整棵子树导出（有遍历、有跳过规则）
  const picked = state.selected;
  const single = picked && !picked.is_dir ? picked : null;
  const p = single ? single.path : (picked && picked.is_dir ? picked.path : state.path || "/");
  setStatus(single ? `正在导出文件 ${p} …` : `正在导出 ${p} …`);
  const btn = $("btn-export");
  const up = $("btn-up");
  const stop = $("btn-cancel-export");
  btn.disabled = true;
  if (sel) sel.disabled = true;
  if (up) up.disabled = true;
  stop.hidden = false;
  stop.disabled = false;
  stop.textContent = "暂停导出";
  await ensureExportListener();
  const box = $("export-progress");
  box.hidden = false;
  $("export-progress-text").textContent = "正在准备…";
  try {
    const msg = single
      ? await invoke("export_entry", {
          id: single.id,
          name: single.name,
          outDir,
          onConflict,
        })
      : await invoke("export_path", { path: p, outDir, onConflict, skipRules });
    // 后端会在消息里写明是"已暂停"还是"已导出"，直接透出
    setStatus(msg, msg.startsWith("已暂停") ? "" : "ok");
  } catch (e) {
    setStatus(String(e), "error");
  } finally {
    // done 事件一般已经隐藏了，这里兜底（比如事件丢失）
    box.hidden = true;
    btn.disabled = false;
    if (up) up.disabled = false;
    if (sel) sel.disabled = false;
    stop.hidden = true;
  }
};
// 覆盖是唯一会动到已有文件的策略，选中时把警告显出来
$("sel-conflict").onchange = (ev) => {
  $("conflict-hint").hidden = ev.target.value !== "overwrite";
};
$("btn-admin").onclick = requestAdmin;
$("open-form").onsubmit = (ev) => {
  ev.preventDefault();
  const p = $("input-path").value.trim();
  if (p) openSource(p);
};
$("chk-deleted").onchange = (ev) => {
  state.showDeleted = ev.target.checked;
  refresh();
};
$("btn-up").onclick = () => {
  const parts = state.path.split("/").filter(Boolean);
  parts.pop();
  navigate("/" + parts.join("/"));
};
$("btn-recover").onclick = recoverAll;

(async () => {
  try {
    state.outDir = await invoke("default_out_dir");
    $("input-out").value = state.outDir;
  } catch (_) {
    $("input-out").value = "./dmde-rs-recovered";
  }
  // 跳过规则：用户改过的优先，否则用后端给的默认值（node_modules 等）
  try {
    let saved = "";
    try {
      saved = localStorage.getItem("dmde.skipRules") || "";
    } catch (_) {}
    $("input-skip").value = saved || (await invoke("default_skip_rules")) || "node_modules, .git";
  } catch (_) {
    $("input-skip").value = "node_modules, .git";
  }
  await loadDisks();
  await updateAdmin();
  // 调试入口：DMDE_OPEN_ON_START 指定的源自动打开（无鼠标自动化时用来复现问题）
  try {
    const bp = await invoke("boot_path");
    if (bp) {
      tlog(`boot auto-open ${bp}`);
      await openSource(bp);
    }
  } catch (_) {}
})();
