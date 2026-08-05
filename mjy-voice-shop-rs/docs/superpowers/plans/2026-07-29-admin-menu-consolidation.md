# 管理后台菜单合并 Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** 将管理后台从 10 个同级菜单收敛为 5 个一级入口，并用可访问、可深链的页内标签合并相关页面。

**Architecture:** 保留无构建步骤的静态 HTML/CSS/JavaScript 架构。聚合页继续复用 `static/admin.js` 的按 DOM 存在性加载机制，新增一套基于 `data-admin-tab` / `data-admin-panel` 和 URL hash 的通用标签控制；服务端 API 与权限边界不变。

**Tech Stack:** HTML5、CSS、原生 JavaScript、Playwright UI 验收、Rust/Axum 静态资源服务。

旧后台页面不保留兼容地址或重定向，删除后只验收新的 5 个入口。

---

## 文件结构与职责

- Modify: `scripts/ui-acceptance.mjs`：定义 5 个新页面，并验证菜单、标签、hash、响应式和 403 降级。
- Modify: `static/admin.js`：增加通用标签初始化与 hash/键盘交互；保留当前未提交的时间格式和 403 修复。
- Modify: `static/styles.css`：增加聚合页标签和标签面板布局。
- Modify: `static/admin.html`：将菜单和概览链接改为新信息架构。
- Modify: `static/admin-voice.html`：合并能力授权、声音模型和角色分析。
- Create: `static/admin-commerce.html`：合并商品库、订单列表和订单接入。
- Modify: `static/admin-conversations.html`：改为 5 项菜单并统一“会话记录”命名。
- Create: `static/admin-integrations.html`：合并设备接入和小程序 C 端。
- Delete: `static/admin-capability.html`、`static/admin-prompts.html`、`static/admin-products.html`、`static/admin-orders.html`、`static/admin-order-mcp.html`、`static/admin-devices.html`、`static/admin-miniprogram-c.html`。
- Modify: `docs/规划迭代记录.md`：记录后台信息架构收敛和验收结果。

仓库中 `static/admin.js`、`scripts/ui-acceptance.mjs`、`docs/规划迭代记录.md` 已有用户改动。实施时只追加本计划相关内容，不回退现有差异；因这些文件存在重叠未提交工作，本计划不自动创建实现提交。

### Task 1: 先把新信息架构写入 UI 验收

**Files:**
- Modify: `scripts/ui-acceptance.mjs:10-22`
- Modify: `scripts/ui-acceptance.mjs:124-535`

- [ ] **Step 1: 把页面清单改成 5 个后台入口并声明标签契约**

用以下结构替换旧的 10 页定义；保留体验页：

```js
const pages = [
  { id: "experience", path: "/", title: "体验页", required: [".app-header", ".studio", ".config-column", ".voice-column", ".cart-column", "#micBtn", "#draft", "#latencyStages"] },
  { id: "admin-overview", path: "/admin.html", title: "概览", activeMenu: "概览", required: [".admin-menu", ".dashboard-grid", ".metric-card"] },
  {
    id: "admin-voice", path: "/admin-voice.html", title: "语音配置", activeMenu: "语音配置",
    required: ["#adminAppId", "#adminVoiceName", "#adminModel", "#adminRolePrompt"],
    tabs: ["capability", "model", "prompts"],
  },
  {
    id: "admin-commerce", path: "/admin-commerce.html", title: "商品与订单", activeMenu: "商品与订单",
    required: ["#adminProducts", "#orderList", "#adminOrderMcpEnabled"],
    tabs: ["products", "orders", "order-mcp"],
  },
  { id: "admin-conversations", path: "/admin-conversations.html", title: "会话记录", activeMenu: "会话记录", required: ["#conversationList", "#conversationDetail", "#conversationPager"] },
  {
    id: "admin-integrations", path: "/admin-integrations.html", title: "接入管理", activeMenu: "接入管理",
    required: ["#adminDeviceConfig", "#miniInterfaces", "#miniRunAll"],
    tabs: ["devices", "miniprogram"],
  },
];
```

- [ ] **Step 2: 在页面 audit 返回值中记录菜单数与标签状态**

在 `page.evaluate` 返回对象前计算：

```js
const adminMenuItems = document.querySelectorAll(".admin-menu a").length;
const tabs = [...document.querySelectorAll("[data-admin-tab]")].map((tab) => ({
  name: tab.dataset.adminTab,
  selected: tab.getAttribute("aria-selected"),
  controls: tab.getAttribute("aria-controls"),
}));
const visiblePanels = [...document.querySelectorAll("[data-admin-panel]")]
  .filter((panel) => !panel.hidden)
  .map((panel) => panel.dataset.adminPanel);
```

并把 `adminMenuItems`、`tabs`、`visiblePanels` 加进返回对象。后台页断言：

```js
if (target.activeMenu && audit.adminMenuItems !== 5) {
  addFailure(target.id, viewport.id, `后台一级菜单数量异常：${audit.adminMenuItems}`);
}
if (target.tabs) {
  if (audit.tabs.map((item) => item.name).join(",") !== target.tabs.join(",")) {
    addFailure(target.id, viewport.id, `标签集合异常：${JSON.stringify(audit.tabs)}`);
  }
  if (audit.tabs.filter((item) => item.selected === "true").length !== 1
    || audit.visiblePanels.length !== 1
    || audit.visiblePanels[0] !== target.tabs[0]) {
    addFailure(target.id, viewport.id, `默认标签状态异常：${JSON.stringify({ tabs: audit.tabs, visiblePanels: audit.visiblePanels })}`);
  }
}
```

- [ ] **Step 3: 添加 hash、点击和前进/后退验收**

在常规页面循环后、403 验收前追加：

```js
for (const target of pages.filter((item) => item.tabs)) {
  const tabPage = await browser.newPage({ viewport: { width: 900, height: 1000 } });
  const second = target.tabs[1];
  await tabPage.goto(`${BASE_URL}${target.path}#${second}`, { waitUntil: "networkidle" });
  const hashState = await tabPage.evaluate(() => ({
    hash: location.hash,
    selected: document.querySelector('[data-admin-tab][aria-selected="true"]')?.dataset.adminTab,
    visible: document.querySelector('[data-admin-panel]:not([hidden])')?.dataset.adminPanel,
  }));
  if (hashState.hash !== `#${second}` || hashState.selected !== second || hashState.visible !== second) {
    addFailure(target.id, "hash", `hash 初始化异常：${JSON.stringify(hashState)}`);
  }
  await tabPage.locator(`[data-admin-tab="${target.tabs[2] || target.tabs[0]}"]`).click();
  await tabPage.goBack();
  const backSelected = await tabPage.locator('[data-admin-tab][aria-selected="true"]').getAttribute("data-admin-tab");
  if (backSelected !== second) addFailure(target.id, "history", `返回后标签异常：${backSelected}`);
  await tabPage.goto(`${BASE_URL}${target.path}#invalid`, { waitUntil: "networkidle" });
  const fallback = await tabPage.locator('[data-admin-tab][aria-selected="true"]').getAttribute("data-admin-tab");
  if (fallback !== target.tabs[0]) addFailure(target.id, "hash", `非法 hash 未回退：${fallback}`);
  await tabPage.close();
}
```

- [ ] **Step 4: 更新 403 页面目标**

```js
for (const deniedTarget of ["admin-voice.html", "admin-commerce.html"]) {
```

保留现有 `noticeInsideShell` 和禁用控件断言，不重写该段。

- [ ] **Step 5: 运行 UI 验收并确认 RED**

Run:

```bash
scripts/start-dev.sh
npm run ui:check
```

Expected: FAIL，至少报告 `/admin-commerce.html` 或 `/admin-integrations.html` 缺失，以及后台菜单数量仍为 10。失败原因必须是新结构尚未实现，不是脚本语法错误。

### Task 2: 实现通用标签行为与样式

**Files:**
- Modify: `static/admin.js:1090-1139`
- Modify: `static/styles.css:1655-1725`

- [ ] **Step 1: 在 `static/admin.js` 增加通用标签控制**

在 `bindAdminEvents()` 前加入：

```js
function initAdminTabs() {
  const tabList = document.querySelector("[data-admin-tabs]");
  if (!tabList) return;
  const tabs = [...tabList.querySelectorAll("[data-admin-tab]")]
    .filter((tab) => document.querySelector(`[data-admin-panel="${tab.dataset.adminTab}"]`));
  if (!tabs.length) return;

  function activateTab(name, { updateHash = false, focus = false } = {}) {
    const active = tabs.find((tab) => tab.dataset.adminTab === name) || tabs[0];
    tabs.forEach((tab) => {
      const selected = tab === active;
      tab.setAttribute("aria-selected", String(selected));
      tab.tabIndex = selected ? 0 : -1;
      document.querySelector(`[data-admin-panel="${tab.dataset.adminTab}"]`).hidden = !selected;
    });
    if (updateHash && location.hash !== `#${active.dataset.adminTab}`) {
      history.pushState(null, "", `#${active.dataset.adminTab}`);
    }
    if (focus) active.focus();
  }

  tabs.forEach((tab, index) => {
    tab.addEventListener("click", () => activateTab(tab.dataset.adminTab, { updateHash: true }));
    tab.addEventListener("keydown", (event) => {
      if (!["ArrowLeft", "ArrowRight", "Home", "End"].includes(event.key)) return;
      event.preventDefault();
      const nextIndex = event.key === "Home" ? 0
        : event.key === "End" ? tabs.length - 1
          : (index + (event.key === "ArrowRight" ? 1 : -1) + tabs.length) % tabs.length;
      activateTab(tabs[nextIndex].dataset.adminTab, { updateHash: true, focus: true });
    });
  });
  window.addEventListener("popstate", () => activateTab(location.hash.slice(1)));
  activateTab(location.hash.slice(1));
}
```

并在现有初始化调用前执行：

```js
initAdminTabs();
bindAdminEvents();
```

- [ ] **Step 2: 在 `static/styles.css` 增加标签布局**

```css
.admin-tabs {
  display: flex;
  gap: 8px;
  overflow-x: auto;
  border-bottom: 1px solid var(--line);
  padding: 0 2px 12px;
}

.admin-tab {
  flex: 0 0 auto;
  min-height: 40px;
  border: 1px solid var(--line);
  border-radius: 8px;
  padding: 0 14px;
  background: #fff;
  color: #657089;
  font-weight: 900;
}

.admin-tab[aria-selected="true"] {
  border-color: #b8c9ff;
  background: #eef3ff;
  color: var(--blue);
}

.admin-tab-panel {
  display: grid;
  gap: 18px;
}

.admin-tab-panel[hidden] {
  display: none;
}
```

- [ ] **Step 3: 做静态语法检查**

Run:

```bash
node --check static/admin.js
```

Expected: exit 0，无输出。

### Task 3: 合并语音配置页面

**Files:**
- Modify: `static/admin-voice.html`
- Delete: `static/admin-capability.html`
- Delete: `static/admin-prompts.html`

- [ ] **Step 1: 把 `static/admin-voice.html` 改成聚合页骨架**

页面必须使用以下唯一菜单和标签骨架；三个 panel 内分别原样迁入旧页面的字段区块，且全页只保留一组 `adminSaveTop`、`adminSaveBottom`、`adminStatus`：

```html
<title>语音配置 - 管理后台</title>
<header class="app-header admin-header">
  <div class="brand-block">
    <div class="brand-mark">声</div>
    <div><h1>语音配置</h1><p>统一管理能力授权、播报模型和对话策略</p></div>
  </div>
  <div class="header-actions"><a href="./">体验页</a><button id="adminSaveTop">保存配置</button></div>
</header>
<aside class="admin-menu" aria-label="后台菜单">
  <div class="menu-title">管理菜单</div>
  <a href="admin.html">概览</a>
  <a class="active" href="admin-voice.html">语音配置</a>
  <a href="admin-commerce.html">商品与订单</a>
  <a href="admin-conversations.html">会话记录</a>
  <a href="admin-integrations.html">接入管理</a>
</aside>
<div class="admin-shell single-column">
  <nav class="admin-tabs" data-admin-tabs role="tablist" aria-label="语音配置分类">
    <button class="admin-tab" id="tab-capability" data-admin-tab="capability" aria-controls="panel-capability" role="tab">能力授权</button>
    <button class="admin-tab" id="tab-model" data-admin-tab="model" aria-controls="panel-model" role="tab">声音与模型</button>
    <button class="admin-tab" id="tab-prompts" data-admin-tab="prompts" aria-controls="panel-prompts" role="tab">角色与分析</button>
  </nav>
  <div class="admin-tab-panel" id="panel-capability" data-admin-panel="capability" role="tabpanel" aria-labelledby="tab-capability"></div>
  <div class="admin-tab-panel" id="panel-model" data-admin-panel="model" role="tabpanel" aria-labelledby="tab-model" hidden></div>
  <div class="admin-tab-panel" id="panel-prompts" data-admin-panel="prompts" role="tabpanel" aria-labelledby="tab-prompts" hidden></div>
  <div class="admin-actions"><span id="adminStatus">待保存</span><button id="adminSaveBottom" class="publish-btn">保存配置</button></div>
</div>
```

面板内容按以下确定映射迁入，不改字段 ID 或文案：

```text
panel-capability <- static/admin-capability.html 的“能力授权”与“授权与 Endpoint” section
panel-model      <- static/admin-voice.html 的“播报与推理”“声音配置”“模型配置” section
panel-prompts    <- static/admin-prompts.html 的“对话策略”与“提示词配置” section
```

- [ ] **Step 2: 删除两个已合并页面**

Delete exactly:

```text
static/admin-capability.html
static/admin-prompts.html
```

- [ ] **Step 3: 运行语音配置页面验收**

Run:

```bash
npm run ui:check
```

Expected: `admin-voice` 不再报告菜单、标签或关键字段错误；整体仍因 commerce/integrations 未实现而失败。

### Task 4: 合并商品与订单页面

**Files:**
- Create: `static/admin-commerce.html`
- Delete: `static/admin-products.html`
- Delete: `static/admin-orders.html`
- Delete: `static/admin-order-mcp.html`

- [ ] **Step 1: 创建商品与订单聚合页**

使用与 Task 3 完全相同的 5 项菜单，`商品与订单` 为 active。标签和面板契约必须是：

```html
<title>商品与订单 - 管理后台</title>
<nav class="admin-tabs" data-admin-tabs role="tablist" aria-label="商品与订单分类">
  <button class="admin-tab" id="tab-products" data-admin-tab="products" aria-controls="panel-products" role="tab">商品库</button>
  <button class="admin-tab" id="tab-orders" data-admin-tab="orders" aria-controls="panel-orders" role="tab">订单列表</button>
  <button class="admin-tab" id="tab-order-mcp" data-admin-tab="order-mcp" aria-controls="panel-order-mcp" role="tab">订单接入</button>
</nav>
<div class="admin-tab-panel" id="panel-products" data-admin-panel="products" role="tabpanel" aria-labelledby="tab-products"></div>
<div class="admin-tab-panel" id="panel-orders" data-admin-panel="orders" role="tabpanel" aria-labelledby="tab-orders" hidden></div>
<div class="admin-tab-panel" id="panel-order-mcp" data-admin-panel="order-mcp" role="tabpanel" aria-labelledby="tab-order-mcp" hidden></div>
```

面板内容按以下确定映射迁入，保持业务区块和关键 ID 不变：

```text
panel-products  <- static/admin-products.html 的“商品管理”与“商品列表” section，并把 adminAddProduct 移到“商品管理” section
panel-orders    <- static/admin-orders.html 的“语音下单记录”与“最近订单” section，并把 adminRefreshOrders 移到“语音下单记录” section
panel-order-mcp <- static/admin-order-mcp.html 内 admin-shell 的全部 section
```

全页只保留一个 `id="adminStatus"`；`adminSaveTop` 不创建，订单接入标签内保留唯一的 `adminSaveBottom`。

- [ ] **Step 2: 删除三个旧业务页面**

Delete exactly:

```text
static/admin-products.html
static/admin-orders.html
static/admin-order-mcp.html
```

- [ ] **Step 3: 运行商品与订单页面验收**

Run: `npm run ui:check`

Expected: `admin-commerce` 不再报告标签、关键元素、MCP 映射或菜单错误；整体仍可能因 integrations 未实现而失败。

### Task 5: 合并接入管理并收敛所有导航

**Files:**
- Create: `static/admin-integrations.html`
- Modify: `static/admin.html`
- Modify: `static/admin-conversations.html`
- Delete: `static/admin-devices.html`
- Delete: `static/admin-miniprogram-c.html`

- [ ] **Step 1: 创建接入管理聚合页**

使用同一 5 项菜单，`接入管理` 为 active。标签骨架：

```html
<title>接入管理 - 管理后台</title>
<nav class="admin-tabs" data-admin-tabs role="tablist" aria-label="接入管理分类">
  <button class="admin-tab" id="tab-devices" data-admin-tab="devices" aria-controls="panel-devices" role="tab">设备接入</button>
  <button class="admin-tab" id="tab-miniprogram" data-admin-tab="miniprogram" aria-controls="panel-miniprogram" role="tab">小程序 C 端</button>
</nav>
<div class="admin-tab-panel" id="panel-devices" data-admin-panel="devices" role="tabpanel" aria-labelledby="tab-devices"></div>
<div class="admin-tab-panel" id="panel-miniprogram" data-admin-panel="miniprogram" role="tabpanel" aria-labelledby="tab-miniprogram" hidden></div>
```

面板内容按以下确定映射迁入：

```text
panel-devices     <- static/admin-devices.html 内 admin-shell 的全部 section
panel-miniprogram <- static/admin-miniprogram-c.html 内 admin-shell 的全部 section
```

迁入时保留 `miniInterfaces`、`miniRunAll`、`miniRequestDetail`、`miniResult` 和 `miniMissingInterfaces` 等全部现有 ID。

- [ ] **Step 2: 把概览与会话页菜单改成 5 项**

两页均使用以下菜单；仅相应页面添加 `active`：

```html
<a href="admin.html">概览</a>
<a href="admin-voice.html">语音配置</a>
<a href="admin-commerce.html">商品与订单</a>
<a href="admin-conversations.html">会话记录</a>
<a href="admin-integrations.html">接入管理</a>
```

概览链接映射：

```text
开始配置、大模型、声音 -> admin-voice.html（声音卡片可用 #model）
商品库、订单 -> admin-commerce.html#products / #orders
设备音频 -> admin-integrations.html#devices
```

- [ ] **Step 3: 删除两个旧接入页面并扫描旧链接**

Delete exactly:

```text
static/admin-devices.html
static/admin-miniprogram-c.html
```

Run:

```bash
rg -n 'admin-(capability|prompts|products|orders|order-mcp|devices|miniprogram-c)\.html' static scripts tests
```

Expected: 无匹配。

- [ ] **Step 4: 运行 UI 验收并确认 GREEN**

Run: `npm run ui:check`

Expected: exit 0，5 个后台入口在 desktop/tablet/mobile 均通过；标签 hash 与 403 降级通过。

### Task 6: 文档与完整回归

**Files:**
- Modify: `docs/规划迭代记录.md`

- [ ] **Step 1: 在迭代记录追加完成项**

记录：10 个同级菜单收敛为 5 个入口；语音、商品订单、终端接入改为标签聚合；旧静态页移除；URL hash、键盘标签、三视口、403 权限降级均已验收。

- [ ] **Step 2: 运行项目回归**

Run:

```bash
cargo test
npm run time:check
npm run voice:check
npm run ui:check
git diff --check
```

Expected: 所有命令 exit 0；`git diff --check` 无输出。

- [ ] **Step 3: 审阅最终差异与工作树边界**

Run:

```bash
git diff --stat
git diff -- static scripts/ui-acceptance.mjs docs/规划迭代记录.md
git status -sb
```

Expected: 仅出现本计划的页面、样式、标签行为、验收和迭代记录改动，以及开始前已存在的用户改动；不得出现 `.env`、数据库、日志、`ui-report` 或 `.superpowers` 内容。
