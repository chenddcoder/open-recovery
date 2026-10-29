// dmde-rs 前端：只用 Tauri 的 invoke，不引任何框架。
const invoke = window.__TAURI__.core.invoke;

const state = {
  source: null,
  path: "/",
  showDeleted: false,
  entries: [],
  selected: null,
  mode: "browse", // browse | deleted
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

async function openSource(path, partition = null) {
  const needsAdmin = path.startsWith("/dev/");
  setStatus(needsAdmin ? `正在打开 ${path}，若需要管理员权限会弹出系统授权窗…` : `正在打开 ${path} …`);
  try {
    const src = await invoke("open_source", { path, partition });
    state.source = src;
    state.mode = "browse";
    renderPartitions();
    renderVolume(src);
    setStatus(`已挂载 ${src.desc} · ${src.fs_kind}`, "ok");
    await updateAdmin();
    await navigate("/");
  } catch (e) {
    setStatus(String(e), "error");
    await updateAdmin();
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
  state.selected = null;
  renderCrumbs();
  await refresh();
}

async function refresh() {
  if (state.mode === "deleted") return scanDeleted();
  const box = $("entries");
  box.innerHTML = `<p class="empty">读取中…</p>`;
  try {
    const items = await invoke("list_dir", {
      path: state.path,
      showDeleted: state.showDeleted,
    });
    state.entries = items;
    renderEntries(items);
  } catch (e) {
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
    box.innerHTML = `<p class="empty">${state.mode === "deleted" ? "没有扫描到可恢复文件" : "（空目录）"}</p>`;
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
  $("btn-save").disabled = e.is_dir;
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

async function scanDeleted() {
  state.mode = "deleted";
  const box = $("entries");
  const kind = (state.source && state.source.fs_kind) || "";
  const slow = kind === "APFS";
  box.innerHTML = `<p class="empty">${
    slow ? "APFS 要遍历整棵目录树（你这盘约 3500 万条记录），可能需要几分钟，请勿关闭窗口…"
         : "正在扫描目录项，请稍候…"
  }</p>`;
  setStatus(slow ? "正在全树扫描（APFS），这一步比较慢…" : "正在扫描…");
  try {
    const items = await invoke("scan_deleted", { limit: 2000 });
    state.entries = items;
    renderEntries(items);
    if (!items.length && slow) {
      setStatus("没扫到孤儿 inode —— APFS 真删除会连 inode 一起回收，属正常；可先看 .Trashes 或快照", "ok");
    } else {
      setStatus(`扫描完成：${items.length} 个可恢复文件（列表最多显示 2000 个）`, "ok");
    }
  } catch (e) {
    // 出错时不要把人卡在错误页：直接退回浏览模式并重新列目录
    state.mode = "browse";
    setStatus(String(e), "error");
    await refresh();
  }
}

// ---------------- 动作 ----------------

async function saveSelected() {
  if (!state.selected || state.selected.is_dir) return;
  const outDir = $("input-out").value.trim();
  try {
    const msg = await invoke("save_file", {
      id: state.selected.id,
      name: state.selected.name,
      outDir,
    });
    setStatus(msg, "ok");
  } catch (e) {
    setStatus(String(e), "error");
  }
}

async function recoverAll() {
  const outDir = $("input-out").value.trim();
  if (!outDir) { setStatus("请先填写输出目录", "error"); return; }
  setStatus("正在恢复…");
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
    if (p.failed) parts.push(`失败 ${p.failed}`);
    $("export-progress-text").textContent = `${parts.join(" · ")}\n${p.current || ""}`;
  });
}

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
  const p = state.path || "/";
  setStatus(`正在导出 ${p} …`);
  const btn = $("btn-export");
  const up = $("btn-up");
  btn.disabled = true;
  if (up) up.disabled = true;
  await ensureExportListener();
  const box = $("export-progress");
  box.hidden = false;
  $("export-progress-text").textContent = "正在准备…";
  try {
    const msg = await invoke("export_path", { path: p, outDir });
    setStatus(msg, "ok");
  } catch (e) {
    setStatus(String(e), "error");
  } finally {
    // done 事件一般已经隐藏了，这里兜底（比如事件丢失）
    box.hidden = true;
    btn.disabled = false;
    if (up) up.disabled = false;
  }
};
$("btn-admin").onclick = requestAdmin;
$("open-form").onsubmit = (ev) => {
  ev.preventDefault();
  const p = $("input-path").value.trim();
  if (p) openSource(p);
};
$("chk-deleted").onchange = (ev) => {
  state.showDeleted = ev.target.checked;
  if (state.mode === "deleted") { state.mode = "browse"; }
  refresh();
};
$("btn-up").onclick = () => {
  const parts = state.path.split("/").filter(Boolean);
  parts.pop();
  navigate("/" + parts.join("/"));
};
$("btn-scan").onclick = scanDeleted;
$("btn-save").onclick = saveSelected;
$("btn-recover").onclick = recoverAll;

(async () => {
  try {
    state.outDir = await invoke("default_out_dir");
    $("input-out").value = state.outDir;
  } catch (_) {
    $("input-out").value = "./dmde-rs-recovered";
  }
  await loadDisks();
  await updateAdmin();
})();
