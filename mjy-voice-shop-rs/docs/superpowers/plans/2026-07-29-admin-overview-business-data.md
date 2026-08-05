# 管理后台概览业务数据 Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** 在管理后台概览页直接展示最近五条订单和最近五轮对话，同时保留现有聚合菜单与内部访问边界。

**Architecture:** 复用 `admin.js` 已有订单、对话请求和字段兼容函数，在现有加载函数中增加概览专用渲染分支。`admin.html` 只提供语义化容器和初始状态，`styles.css` 提供紧凑双列布局，现有 UI 验收脚本覆盖结构、最终状态与响应式边界。

**Tech Stack:** 原生 HTML/CSS/JavaScript、Axum 静态资源、Playwright UI 验收脚本

---

### Task 1: 建立概览业务数据失败验收

**Files:**
- Modify: `scripts/ui-acceptance.mjs:12`
- Modify: `scripts/ui-acceptance.mjs:140-240`

- [ ] **Step 1: 扩展概览页结构与状态断言**

将概览页必需选择器改为：

```js
required: [
  ".admin-menu",
  ".dashboard-grid",
  ".metric-card",
  "#dashboardRecentOrders",
  "#dashboardRecentConversations",
]
```

在页面审计结果中增加：

```js
overviewStates: target.id === "admin-overview"
  ? ["dashboardRecentOrders", "dashboardRecentConversations"].map((id) => ({
      id,
      state: document.getElementById(id)?.dataset.state || "missing",
      rows: document.querySelectorAll(`#${id} .dashboard-data-row`).length,
    }))
  : [],
```

概览页本机验收要求每个区域状态为 `ready` 或 `empty`，且行数不超过 5；`missing`、`loading`、`error` 都记为失败。

- [ ] **Step 2: 运行测试并确认按预期失败**

Run: `npm run ui:check`

Expected: FAIL，明确报告概览缺少 `#dashboardRecentOrders` 和 `#dashboardRecentConversations`，证明测试覆盖了当前缺陷。

- [ ] **Step 3: 提交失败验收**

```bash
git add scripts/ui-acceptance.mjs
git commit -m "测试：覆盖概览订单与对话数据"
```

### Task 2: 增加概览业务数据结构与渲染

**Files:**
- Modify: `static/admin.html:68-75`
- Modify: `static/admin.js:82-94`
- Modify: `static/admin.js:293-365`
- Modify: `static/admin.js:590-625`

- [ ] **Step 1: 在概览页增加两个独立数据面板**

在摘要卡片后增加：

```html
<section class="dashboard-business wide" aria-labelledby="dashboardBusinessTitle">
  <div class="section-head dashboard-business-head">
    <div>
      <h2 id="dashboardBusinessTitle">业务动态</h2>
      <span>最近订单与最近对话</span>
    </div>
  </div>
  <div class="dashboard-business-grid">
    <section class="admin-panel dashboard-data-panel" aria-labelledby="dashboardOrdersTitle">
      <div class="section-head">
        <h3 id="dashboardOrdersTitle">最近订单</h3>
        <a href="admin-commerce.html#orders">查看全部</a>
      </div>
      <div id="dashboardRecentOrders" class="dashboard-data-list" data-state="loading">正在加载</div>
    </section>
    <section class="admin-panel dashboard-data-panel" aria-labelledby="dashboardConversationsTitle">
      <div class="section-head">
        <h3 id="dashboardConversationsTitle">最近对话</h3>
        <a href="admin-conversations.html">查看全部</a>
      </div>
      <div id="dashboardRecentConversations" class="dashboard-data-list" data-state="loading">正在加载</div>
    </section>
  </div>
</section>
```

- [ ] **Step 2: 增加通用状态和最近订单渲染**

新增小型状态辅助函数，并在 `loadAdminOrders` 取得 `orders` 后调用：

```js
function renderDashboardState(id, state, html) {
  const container = $(id);
  if (!container) return;
  container.dataset.state = state;
  container.innerHTML = html;
}

function renderDashboardOrders(orders) {
  const recent = orders.slice(0, 5);
  if (!recent.length) {
    renderDashboardState("dashboardRecentOrders", "empty", '<div class="dashboard-data-empty">暂无订单</div>');
    return;
  }
  renderDashboardState("dashboardRecentOrders", "ready", recent.map((order) => `
    <a class="dashboard-data-row" href="admin-commerce.html#orders">
      <span><strong>${escapeHtml(shortOrderId(orderIdentity(order)))}</strong><small>${escapeHtml(orderStatus(order))}</small></span>
      <span><strong>￥${escapeHtml(String(orderTotal(order)))}</strong><small>${escapeHtml(formatOrderTriggerTime(order))}</small></span>
    </a>
  `).join(""));
}
```

- [ ] **Step 3: 让对话请求在概览页执行并渲染五条**

将 `loadConversations` 的入口条件改为完整列表或概览列表任一存在，并增加：

```js
function renderDashboardConversations(conversations) {
  const recent = conversations.slice(0, 5);
  if (!recent.length) {
    renderDashboardState("dashboardRecentConversations", "empty", '<div class="dashboard-data-empty">暂无对话</div>');
    return;
  }
  renderDashboardState("dashboardRecentConversations", "ready", recent.map((row) => `
    <a class="dashboard-data-row" href="admin-conversations.html">
      <span><strong>${escapeHtml(row.conversation_id.slice(0, 8))}</strong><small>${escapeHtml(row.last_user_text || "暂无用户输入")}</small></span>
      <span><strong>${row.message_count || 0} 条</strong><small>${escapeHtml(formatTriggerTime(row.created_at))}</small></span>
    </a>
  `).join(""));
}
```

概览请求使用 `page_size=5`；完整会话页继续使用原分页大小并且只在完整列表存在时加载首条详情。

- [ ] **Step 4: 覆盖失败与内部权限状态**

扩展 `runAdminTask(task, errorTargets = [])`，错误时将目标容器设置为 `error`；调用订单和对话加载时分别传入对应目标。`enterInternalOnlyMode` 将两个区域设置为 `denied` 并显示“仅限服务器本机管理”。

- [ ] **Step 5: 运行 UI 验收并确认数据断言通过**

Run: `npm run ui:check`

Expected: PASS；本机概览两个区域均为 `ready` 或 `empty`，每区不超过 5 行。

- [ ] **Step 6: 提交结构与渲染**

```bash
git add static/admin.html static/admin.js scripts/ui-acceptance.mjs
git commit -m "修复：恢复概览订单与对话数据"
```

### Task 3: 补齐响应式样式与完整回归

**Files:**
- Modify: `static/styles.css`

- [ ] **Step 1: 增加紧凑业务数据布局**

```css
.dashboard-business-grid {
  display: grid;
  grid-template-columns: repeat(2, minmax(0, 1fr));
  gap: 16px;
}

.dashboard-data-list {
  display: grid;
  min-height: 180px;
}

.dashboard-data-row {
  display: grid;
  grid-template-columns: minmax(0, 1fr) auto;
  gap: 16px;
  align-items: center;
  padding: 12px 0;
  border-bottom: 1px solid var(--border);
}

@media (max-width: 900px) {
  .dashboard-business-grid { grid-template-columns: 1fr; }
}
```

同时为长文本增加 `min-width: 0`、单行截断和悬停态，保持现有后台视觉语言。

- [ ] **Step 2: 运行前端与后端回归**

Run: `npm run ui:check`

Expected: PASS，桌面、平板、移动端无横向溢出。

Run: `npm run voice:check && npm run miniprogram:check && npm run time:check`

Expected: 全部 PASS。

Run: `DEVELOPER_DIR=/Library/Developer/CommandLineTools cargo test`

Expected: 全部 Rust 测试 PASS。

- [ ] **Step 3: 在真实浏览器验证**

打开 `http://127.0.0.1:8787/admin.html`，确认最近订单显示真实数据或空状态，最近对话显示最多五条真实记录；分别点击“查看全部”，确认进入 `admin-commerce.html#orders` 和 `admin-conversations.html`。

- [ ] **Step 4: 检查差异并提交样式**

```bash
git diff --check
git status -sb
git add static/styles.css
git commit -m "样式：完善概览业务动态布局"
```
