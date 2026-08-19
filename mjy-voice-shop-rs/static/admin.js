const adminState = {
  config: null,
  products: [],
  miniProgram: {
    interfaces: [],
    missing: [],
    activeId: "",
    drafts: {},
  },
  conversations: {
    page: 1,
    pageSize: 8,
    total: 0,
    totalPages: 1,
  },
  orders: {
    items: [],
    activeId: "",
  },
};

const $ = (id) => document.getElementById(id);
const PUBLIC_BASE_PATH = detectPublicBasePath();
const {
  formatShanghaiTime,
  formatTriggerTime,
  formatOrderTriggerTime,
  formatJsonForDisplay,
} = globalThis.AdminTime;
const orderMcpToolDefinitions = [
  { key: "resolve_context", label: "解析上下文", description: "兼容扩展 · 本地设备绑定上下文", fallback: "resolveUserContext" },
  { key: "authorize_member", label: "会员授权", description: "PDF 主规范 · 所有点餐工具调用前执行", fallback: "authorizeMember" },
  { key: "preview_order", label: "订单预览", description: "PDF 主规范 · 确认金额和预计取餐时间", fallback: "previewOrder" },
  { key: "create_order", label: "创建订单", description: "PDF 主规范 · 用户确认后创建订单", fallback: "createOrder" },
  { key: "list_orders", label: "查询订单列表", description: "Apifox 兼容扩展 · PDF 未定义列表工具", fallback: "listUserOrders" },
  { key: "get_order_detail", label: "查询订单详情", description: "PDF 主规范 · 按 orderId 查询订单", fallback: "queryOrderDetailInfo" },
  { key: "refund_order", label: "退单/取消", description: "待确认扩展 · PDF 未定义退单工具", fallback: "refundOrder" },
];

function detectPublicBasePath() {
  const markers = ["/myj-voice-shop", "/mjy-voice-shop"];
  return markers.find((marker) => location.pathname === marker || location.pathname.startsWith(`${marker}/`)) || "";
}

function apiUrl(path) {
  return `${PUBLIC_BASE_PATH}${path}`;
}

class InternalAccessError extends Error {}

function enterInternalOnlyMode() {
  adminState.internalAccessDenied = true;
  document.body.dataset.internalAccess = "denied";
  document.querySelectorAll("main input, main textarea, main select, main button").forEach((control) => {
    control.disabled = true;
  });
  ["adminStatus", "adminOrderMcpSaveStatus", "orderAdminStatus", "miniStatus", "miniResultSummary"].forEach((id) => {
    setText(id, "仅限服务器本机管理");
  });
  ["dashboardRecentOrders", "dashboardRecentConversations"].forEach((id) => {
    renderDashboardState(id, "denied", '<div class="dashboard-data-empty">仅限服务器本机管理</div>');
  });
  const host = document.querySelector(".admin-shell")
    || document.querySelector(".admin-main")
    || document.querySelector("main");
  if (host && !$("internalAccessNotice")) {
    const notice = document.createElement("p");
    notice.id = "internalAccessNotice";
    notice.setAttribute("role", "status");
    notice.textContent = "当前页面仅可在服务器本机或 SSH 隧道中管理，公网访问已锁定编辑功能。";
    host.prepend(notice);
  }
}

async function fetchInternalJson(path, options) {
  const response = window.adminFetch
    ? await window.adminFetch(path, options)
    : await fetch(apiUrl(path), options);
  if (response.status === 401) throw new Error("login required");
  if (response.status === 403) {
    enterInternalOnlyMode();
    throw new InternalAccessError("internal access required");
  }
  if (!response.ok) throw new Error(`HTTP ${response.status}`);
  return response.json();
}

function runAdminTask(task, errorTargets = []) {
  Promise.resolve()
    .then(task)
    .then(() => {
      if (adminState.internalAccessDenied) enterInternalOnlyMode();
    })
    .catch((error) => {
      if (error instanceof InternalAccessError) return;
      ["adminStatus", "adminOrderMcpSaveStatus", "orderAdminStatus"].forEach((id) => {
        setText(id, "管理数据加载失败");
      });
      errorTargets.forEach((id) => {
        renderDashboardState(id, "error", '<div class="dashboard-data-empty">管理数据加载失败</div>');
      });
    });
}

function setValue(id, value) {
  const element = $(id);
  if (element) element.value = value ?? "";
}

function setChecked(id, value) {
  const element = $(id);
  if (element) element.checked = Boolean(value);
}

function getValue(id, fallback = "") {
  return $(id)?.value?.trim() || fallback;
}

function getChecked(id, fallback = false) {
  const element = $(id);
  return element ? element.checked : fallback;
}

function getJsonValue(id, fallback = {}) {
  const element = $(id);
  if (!element || !element.value.trim()) return fallback;
  try {
    return JSON.parse(element.value);
  } catch {
    const label = id === "adminOrderMcpTools" ? "能力映射" : "门店绑定上下文";
    setAdminConfigStatus(`${label}不是合法 JSON`);
    throw new Error("invalid order context json");
  }
}

function setText(id, value) {
  const element = $(id);
  if (element) element.textContent = value;
}

function renderDashboardState(id, state, html) {
  const container = $(id);
  if (!container) return;
  container.dataset.state = state;
  container.innerHTML = html;
}

function setAdminConfigStatus(value) {
  setText($("adminOrderMcpSaveStatus") ? "adminOrderMcpSaveStatus" : "adminStatus", value);
}

function escapeAttr(value) {
  return String(value ?? "")
    .replaceAll("&", "&amp;")
    .replaceAll('"', "&quot;")
    .replaceAll("<", "&lt;")
    .replaceAll(">", "&gt;");
}

function renderOrderMcpToolRows(config = {}) {
  const container = $("adminOrderMcpToolRows");
  if (!container) return;
  const tools = config.order_mcp_tools || {};
  container.innerHTML = orderMcpToolDefinitions
    .map((item) => {
      const value = tools[item.key] || item.fallback;
      return `
        <label class="mapping-row">
          <span>
            <strong>${item.label}</strong>
            <small>${item.key} · ${item.description}</small>
          </span>
          <input data-order-mcp-tool="${item.key}" type="text" value="${escapeAttr(value)}" placeholder="${escapeAttr(item.fallback)}" autocomplete="off" />
        </label>
      `;
    })
    .join("");
  syncOrderMcpToolsField();
  container.querySelectorAll("[data-order-mcp-tool]").forEach((input) => {
    input.addEventListener("input", () => {
      syncOrderMcpToolsField();
      setAdminConfigStatus("有未保存修改");
    });
  });
}

function collectOrderMcpTools(currentTools = {}) {
  const inputs = document.querySelectorAll("[data-order-mcp-tool]");
  if (!inputs.length) return currentTools;
  const tools = {};
  inputs.forEach((input) => {
    const definition = orderMcpToolDefinitions.find((item) => item.key === input.dataset.orderMcpTool);
    tools[input.dataset.orderMcpTool] = input.value.trim() || definition?.fallback || "";
  });
  return tools;
}

function syncOrderMcpToolsField() {
  const field = $("adminOrderMcpTools");
  if (field) field.value = JSON.stringify(collectOrderMcpTools({}), null, 2);
}

function superSmartVoices(config) {
  const voices = config?.available_super_smart_voices || [];
  return voices.length
    ? voices
    : [
        { name: "聆小璇", code: "x6_lingxiaoxuan_pro" },
        { name: "聆飞瀚", code: "x6_lingfeihan_pro" },
      ];
}

function renderAdminSuperSmartVoiceControls(config) {
  const voices = superSmartVoices(config);
  const selectedCode = voices.some((voice) => voice.code === config.tts_voice)
    ? config.tts_voice
    : voices[0].code;
  const nameSelect = $("adminVoiceName");
  const codeSelect = $("adminVoiceCode");
  if (nameSelect) {
    nameSelect.innerHTML = voices
      .map((voice) => `<option value="${voice.code}" ${voice.code === selectedCode ? "selected" : ""}>${voice.name}</option>`)
      .join("");
  }
  if (codeSelect) {
    codeSelect.innerHTML = voices
      .map((voice) => `<option value="${voice.code}" ${voice.code === selectedCode ? "selected" : ""}>${voice.code}</option>`)
      .join("");
  }
}

function syncAdminVoiceSelects(sourceId) {
  const value = $(sourceId)?.value;
  if (!value) return;
  setValue("adminVoiceName", value);
  setValue("adminVoiceCode", value);
}

async function loadAdminConfig() {
  const config = await fetchInternalJson("/api/admin/config");
  adminState.config = config;
  setValue("adminAppId", config.app_id);
  setValue("adminIatEndpoint", config.iat_endpoint);
  setValue("adminTtsProvider", config.tts_provider || "super_smart");
  setValue("adminTtsEndpoint", config.tts_endpoint);
  setValue("adminStandardTtsEndpoint", config.tts_standard_endpoint || "wss://tts-api.xfyun.cn/v2/tts");
  setValue("adminStandardTtsVoice", config.tts_standard_voice || "x4_lingxiaolu_em_v2");
  setValue("adminLlmEndpoint", config.llm_endpoint);
  renderAdminSuperSmartVoiceControls(config);
  setValue("adminTemperature", config.temperature ?? 0.4);
  setValue("adminMaxTokens", config.max_tokens ?? 1024);
  setValue("adminRolePrompt", config.role_prompt);
  setValue("adminAnalysisPrompt", config.analysis_prompt);
  setChecked("adminOrderMcpEnabled", config.order_mcp_enabled);
  setValue("adminOrderMcpUrl", config.order_mcp_url || "http://127.0.0.1:8765/mcp");
  setValue("adminOrderContext", JSON.stringify(config.order_context || {}, null, 2));
  renderOrderMcpToolRows(config);
  renderOrderMcpMode(config);
  setText("dashboardModel", config.llm_model || "-");
  setText("dashboardVoice", config.tts_voice_name || "聆小玥");
  setText("dashboardIat", config.iat_endpoint || "-");
  if ($("adminAppId") || $("adminOrderMcpEnabled")) setAdminConfigStatus("配置已加载");

  const modelSelect = $("adminModel");
  if (modelSelect) {
    modelSelect.innerHTML = config.available_models
      .map((model) => `<option value="${model}" ${model === config.llm_model ? "selected" : ""}>${model}</option>`)
      .join("");
  }
}

async function saveAdminConfig() {
  const current = adminState.config || await fetchInternalJson("/api/admin/config");
  const selectedSuperVoiceCode = getValue("adminVoiceCode", current.tts_voice || "x6_lingxiaoxuan_pro");
  const selectedSuperVoice = superSmartVoices(current).find((voice) => voice.code === selectedSuperVoiceCode) || superSmartVoices(current)[0];
  const next = {
    ...current,
    app_id: getValue("adminAppId", current.app_id),
    api_key: getValue("adminApiKey"),
    api_secret: getValue("adminApiSecret"),
    iat_endpoint: getValue("adminIatEndpoint", current.iat_endpoint),
    tts_provider: getValue("adminTtsProvider", current.tts_provider || "super_smart"),
    tts_endpoint: getValue("adminTtsEndpoint", current.tts_endpoint),
    tts_standard_endpoint: getValue("adminStandardTtsEndpoint", current.tts_standard_endpoint || "wss://tts-api.xfyun.cn/v2/tts"),
    tts_standard_voice: getValue("adminStandardTtsVoice", current.tts_standard_voice || "x4_lingxiaolu_em_v2"),
    llm_endpoint: getValue("adminLlmEndpoint", current.llm_endpoint),
    tts_voice_name: selectedSuperVoice.name,
    tts_voice: selectedSuperVoice.code,
    llm_model: getValue("adminModel", current.llm_model),
    temperature: Number(getValue("adminTemperature", current.temperature ?? 0.4)),
    max_tokens: Number(getValue("adminMaxTokens", current.max_tokens ?? 1024)),
    role_prompt: $("adminRolePrompt") ? $("adminRolePrompt").value : current.role_prompt,
    analysis_prompt: $("adminAnalysisPrompt") ? $("adminAnalysisPrompt").value : current.analysis_prompt,
    order_mcp_enabled: getChecked("adminOrderMcpEnabled", current.order_mcp_enabled),
    order_mcp_url: getValue("adminOrderMcpUrl", current.order_mcp_url || "http://127.0.0.1:8765/mcp"),
    order_mcp_token: getValue("adminOrderMcpToken"),
    order_context: getJsonValue("adminOrderContext", current.order_context || {}),
    order_mcp_tools: collectOrderMcpTools(current.order_mcp_tools || {}),
    mock_providers: false,
  };
  const saved = await fetchInternalJson("/api/admin/config", {
    method: "PUT",
    headers: { "Content-Type": "application/json" },
    body: JSON.stringify(next),
  });
  adminState.config = saved;
  setValue("adminApiKey", "");
  setValue("adminApiSecret", "");
  await loadAdminConfig();
  setAdminConfigStatus("已保存");
}

function renderOrderMcpMode(config = adminState.config || {}) {
  const enabled = Boolean(config.order_mcp_enabled);
  setText("orderMcpModeBadge", enabled ? "MCP 已启用" : "本地验证");
  setText("orderMcpStatusText", enabled ? "当前订单请求优先调用 MCP Server" : "当前使用本地订单链路");
  $("orderMcpModeBadge")?.classList.toggle("enabled", enabled);
}

async function loadConversations(page = adminState.conversations.page) {
  const list = $("conversationList");
  const dashboard = $("dashboardRecentConversations");
  if (!list && !dashboard) return;
  const pageSize = list ? adminState.conversations.pageSize : 5;
  const data = await fetchInternalJson(`/api/admin/conversations?page=${page}&page_size=${pageSize}`);
  const conversations = data.items || [];
  adminState.conversations = {
    page: data.page || 1,
    pageSize: data.page_size || pageSize,
    total: data.total || 0,
    totalPages: data.total_pages || 1,
  };
  renderDashboardConversations(conversations);
  if (!list) return;
  renderConversationPager();
  if (!conversations.length) {
    list.innerHTML = `<div class="conversation-empty">暂无历史对话</div>`;
    $("conversationDetail")?.classList.add("empty");
    if ($("conversationDetail")) {
      $("conversationDetail").innerHTML = `<h3>选择一轮对话</h3><p>右侧会展示这一轮的用户输入和系统回复。</p>`;
    }
    setText("adminStatus", "暂无数据");
    return;
  }
  list.innerHTML = conversations.map(renderConversationRow).join("");
  setText("adminStatus", `已加载 ${conversations.length} / ${adminState.conversations.total} 轮`);
  document.querySelectorAll("[data-conversation-id]").forEach((row) => {
    row.addEventListener("click", () => runAdminTask(() => loadConversationDetail(row.dataset.conversationId)));
  });
  loadConversationDetail(conversations[0].conversation_id);
}

function renderConversationPager() {
  const { page, total, totalPages } = adminState.conversations;
  setText("conversationPageInfo", `共 ${total} 轮`);
  setText("conversationPageText", `第 ${page} / ${totalPages} 页`);
  const prev = $("conversationPrev");
  const next = $("conversationNext");
  if (prev) prev.disabled = page <= 1;
  if (next) next.disabled = page >= totalPages;
}

function renderConversationRow(row) {
  const id = escapeHtml(row.conversation_id);
  const lastText = escapeHtml(row.last_user_text || "暂无用户输入");
  const triggerTime = escapeHtml(formatTriggerTime(row.last_message_at || row.created_at));
  const source = escapeHtml(conversationSourceLabel(row.device_id));
  return `
    <button class="conversation-row" data-conversation-id="${id}" type="button">
      <strong>${id.slice(0, 8)}</strong>
      <span>${lastText}</span>
      <small>${source} · ${triggerTime} · ${row.message_count || 0} 条</small>
    </button>
  `;
}

function renderDashboardConversations(conversations) {
  const recent = conversations.slice(0, 5);
  if (!recent.length) {
    renderDashboardState("dashboardRecentConversations", "empty", '<div class="dashboard-data-empty">暂无对话</div>');
    return;
  }
  renderDashboardState("dashboardRecentConversations", "ready", recent.map((row) => {
    const id = String(row.conversation_id || "");
    const lastText = row.last_user_text || "暂无用户输入";
    return `
      <a class="dashboard-data-row" href="admin-conversations.html">
        <span>
          <strong>${escapeHtml(id.slice(0, 8) || "未命名会话")}</strong>
          <small title="${escapeAttr(lastText)}">${escapeHtml(lastText)}</small>
        </span>
        <span>
          <strong>${escapeHtml(String(row.message_count || 0))} 条</strong>
          <small>${escapeHtml(formatTriggerTime(row.created_at))}</small>
        </span>
      </a>
    `;
  }).join(""));
}

function conversationSourceLabel(deviceId) {
  return deviceId ? `设备 ${deviceId}` : "体验页/历史对话";
}

async function loadConversationDetail(conversationId) {
  document.querySelectorAll(".conversation-row").forEach((row) => {
    row.classList.toggle("active", row.dataset.conversationId === conversationId);
  });
  const detail = await fetchInternalJson(`/api/admin/conversations/${encodeURIComponent(conversationId)}`);
  const container = $("conversationDetail");
  const messages = detail.messages || [];
  const orders = detail.orders || [];
  const events = detail.events || [];
  container.classList.remove("empty");
  container.innerHTML = `
    <div class="conversation-detail-head">
      <h3>轮次 ${escapeHtml(conversationId.slice(0, 8))}</h3>
      <span>${messages.length} 条消息 · ${orders.length} 个订单 · ${events.length} 个事件</span>
    </div>
    ${renderOrderRouteSummary(events, orders)}
    ${orders.length ? `
      <section class="conversation-section">
        <h4>关联订单</h4>
        <div class="conversation-orders">
          ${orders.map(renderConversationOrder).join("")}
        </div>
      </section>
    ` : ""}
    ${events.length ? `
      <section class="conversation-section">
        <h4>链路事件</h4>
        <div class="conversation-events">
          ${events.map(renderConversationEvent).join("")}
        </div>
      </section>
    ` : ""}
    <section class="conversation-section">
      <h4>消息记录</h4>
    <div class="conversation-messages">
        ${messages.map(renderConversationMessage).join("")}
    </div>
    </section>
  `;
}

function renderOrderRouteSummary(events, orders) {
  const orderEvents = events.filter((event) => event.event_type.startsWith("order_"));
  if (!orderEvents.length && !orders.length) return "";
  const createCall = [...orderEvents].reverse().find((event) => event.event_type === "order_create_call");
  const fallback = [...orderEvents].reverse().find((event) => event.event_type.endsWith("_fallback"));
  const persisted = [...orderEvents].reverse().find((event) => event.event_type === "order_persisted");
  const mcpCalled = Boolean(createCall && createCall.payload?.mcp_enabled !== false);
  const sourcePayload = persisted?.payload || orders.at(-1)?.payload || {};
  const localResult = sourcePayload.mock === true;
  const mcpResult = sourcePayload.mock === false;
  const reason = fallback?.payload?.reason || {};
  let mode = "本轮未触发订单调用";
  let modeClass = "idle";
  let gateway = "未触发";
  let result = "无订单结果";
  if (fallback && mcpCalled) {
    mode = "MCP 调用失败，已自动转本地 Mock";
    modeClass = "fallback";
    gateway = `调用失败${createCall?.payload?.tool ? ` · ${createCall.payload.tool}` : ""}`;
    result = "本地 Mock 已接管";
  } else if (fallback || createCall?.payload?.mcp_enabled === false) {
    mode = "MCP 未启用，本轮直接使用本地 Mock";
    modeClass = "local";
    gateway = "未发起 MCP 请求";
    result = localResult ? "本地 Mock 订单" : "本地链路";
  } else if (mcpResult || mcpCalled) {
    mode = mcpResult ? "MCP Server 调用成功" : "已调用 MCP Server";
    modeClass = "mcp";
    gateway = `已调用${createCall?.payload?.tool ? ` · ${createCall.payload.tool}` : ""}`;
    result = mcpResult ? "MCP 订单" : "等待结果";
  } else if (localResult) {
    mode = "本地 Mock 订单";
    modeClass = "local";
    gateway = "未发现 MCP 调用";
    result = "本地 Mock 订单";
  }
  const reasonText = reason.code || reason.message
    ? `${reason.code || ""}${reason.code && reason.message ? " · " : ""}${reason.message || ""}`
    : "";
  return `
    <section class="order-route-summary ${modeClass}">
      <header>
        <div><span>本轮订单链路</span><strong>${escapeHtml(mode)}</strong></div>
        ${reasonText ? `<small>${escapeHtml(reasonText)}</small>` : ""}
      </header>
      <div class="order-route-flow">
        <div><span>01</span><strong>用户确认</strong><small>订单动作触发</small></div>
        <i>→</i>
        <div><span>02</span><strong>MCP 网关</strong><small>${escapeHtml(gateway)}</small></div>
        <i>→</i>
        <div><span>03</span><strong>订单结果</strong><small>${escapeHtml(result)}</small></div>
      </div>
    </section>
  `;
}

function renderConversationOrder(order) {
  const payload = order.payload || {};
  const items = payload.items || payload.data?.goodses || [];
  const total = payload.total_amount
    ?? payload.payAmt
    ?? payload.data?.payAmt
    ?? payload.data?.orderPayAmount
    ?? payload.data?.discountPrice
    ?? "-";
  const status = payload.status || payload.displayStatus || payload.data?.displayStatus || "已创建";
  const orderId = payload.saleOrderId || payload.order_id || payload.orderId || order.order_id || "-";
  return `
    <article class="conversation-order-card">
      <header>
        <div>
          <strong>${escapeHtml(orderId)}</strong>
          <span>${escapeHtml(status)} · ${escapeHtml(formatShanghaiTime(order.created_at))}</span>
        </div>
        <b>￥${escapeHtml(String(total))}</b>
      </header>
      <ul>
        ${items.map((item) => `
          <li>
            <span>${escapeHtml(item.name || item.goodsName || "-")}</span>
            <em>x ${escapeHtml(String(item.quantity || item.qty || 1))}</em>
            <small>${escapeHtml(item.spec || "")}</small>
          </li>
        `).join("")}
      </ul>
    </article>
  `;
}

function renderConversationEvent(event) {
  const payload = event.payload || {};
  const summary = summarizeConversationEvent(event.event_type, payload);
  const meta = conversationEventMeta(event.event_type, payload);
  return `
    <article class="conversation-event-card ${meta.className}">
      <header>
        <div>
          <strong>${escapeHtml(labelConversationEvent(event.event_type, payload))}</strong>
          ${meta.badge ? `<em>${escapeHtml(meta.badge)}</em>` : ""}
        </div>
        <span>${escapeHtml(formatShanghaiTime(event.created_at))}</span>
      </header>
      <p>${escapeHtml(summary)}</p>
    </article>
  `;
}

function labelConversationEvent(type, payload = {}) {
  const labels = {
    product_matches: "商品识别",
    order_draft: "待确认订单",
    order_submit_started: "开始下单",
    order_mcp_authorize_call: "调用会员授权",
    order_mcp_authorize_result: "会员授权结果",
    order_mcp_preview_call: "调用订单预览",
    order_mcp_preview_result: "订单预览结果",
    order_create_fallback: "切换本地兜底",
    order_persisted: "订单已保存",
    order_created: "下单成功",
    order_failed: "下单失败",
    intent_analysis: "意图识别",
  };
  if (type === "order_create_call") {
    return payload.mcp_enabled === false ? "跳过 MCP 调用" : "调用 MCP 创建订单";
  }
  return labels[type] || type;
}

function conversationEventMeta(type, payload) {
  if (["order_mcp_authorize_call", "order_mcp_authorize_result", "order_mcp_preview_call", "order_mcp_preview_result"].includes(type)) {
    return { className: payload.success === false ? "route-fallback" : "route-mcp", badge: "MCP" };
  }
  if (type === "order_create_call") {
    return payload.mcp_enabled === false
      ? { className: "route-local", badge: "未调用 MCP" }
      : { className: "route-mcp", badge: "MCP" };
  }
  if (type.endsWith("_fallback")) return { className: "route-fallback", badge: "本地兜底" };
  if (["order_persisted", "order_created", "order_refunded"].includes(type)) {
    return payload.mock === true
      ? { className: "route-local", badge: "本地 Mock" }
      : { className: "route-mcp", badge: "MCP 结果" };
  }
  return { className: "", badge: "" };
}

function summarizeConversationEvent(type, payload) {
  if (type === "product_matches") return summarizeItems(payload.items || []);
  if (type === "order_draft") return `待确认：${summarizeItems(payload.items || [])}`;
  if (type === "order_submit_started") return `准备下单：${summarizeItems(payload.items || [])}`;
  if (type === "order_mcp_authorize_call") return `工具：${payload.tool || "authorizeMember"}`;
  if (type === "order_mcp_authorize_result") return payload.success ? "会员授权通过" : `授权失败：${payload.code || payload.message || "未知原因"}`;
  if (type === "order_mcp_preview_call") return `工具：${payload.tool || "previewOrder"}`;
  if (type === "order_mcp_preview_result") return payload.success
    ? `预览成功${payload.discountPrice != null ? ` · 折后价 ￥${payload.discountPrice}` : ""}`
    : `预览失败：${payload.code || payload.message || "未知原因"}`;
  if (type === "order_create_call") return payload.mcp_enabled === false
    ? `MCP 开关关闭 · ${summarizeItems(payload.items || [])}`
    : `${payload.tool || "createOrder"} · ${summarizeItems(payload.items || [])}`;
  if (type.endsWith("_fallback")) {
    const reason = payload.reason || {};
    return `${reason.code || "MCP 调用失败"}${reason.message ? ` · ${reason.message}` : ""}，已切到本地 Mock`;
  }
  if (type === "order_persisted") return payload.mock ? "本地 Mock 订单已保存" : "MCP 订单已保存";
  if (type === "order_persisted" || type === "order_created") {
    const orderId = payload.saleOrderId || payload.order_id || payload.orderId || "已提交";
    return `${orderId} · ${payload.status || "created"}`;
  }
  if (type === "order_failed") return payload.message || "订单接口调用失败";
  if (type === "intent_analysis") return `${payload.intent || "-"} · ${payload.text || ""}`;
  return formatJsonForDisplay(payload, 0);
}

function summarizeItems(items) {
  if (!items.length) return "无商品";
  return items.map((item) => {
    const name = item.name || item.goodsName || "-";
    const qty = item.quantity || item.qty || 1;
    const spec = item.spec ? `（${item.spec}）` : "";
    return `${name} x ${qty}${spec}`;
  }).join("、");
}

function renderConversationMessage(message) {
  return `
    <article class="conversation-message ${message.role}">
      <header>
        <strong>${message.role === "user" ? "用户" : "系统"}</strong>
        <span>${escapeHtml(formatShanghaiTime(message.created_at))}</span>
      </header>
      <p>${escapeHtml(message.content || "")}</p>
    </article>
  `;
}

async function loadAdminOrders() {
  const container = $("orderList");
  const dashboard = $("dashboardOrders");
  const recentDashboard = $("dashboardRecentOrders");
  if (!container && !dashboard && !recentDashboard) return;
  const data = await fetchInternalJson("/api/orders/list", {
    method: "POST",
    headers: { "Content-Type": "application/json" },
    body: JSON.stringify({}),
  });
  const orders = data.orders || data.data?.items || data.items || [];
  adminState.orders.items = orders;
  if (dashboard) setText("dashboardOrders", String(orders.length));
  renderDashboardOrders(orders);
  if (!container) return;
  setText("orderCount", `${orders.length} 个订单`);
  setText("orderAdminStatus", data.mock
    ? (orders.length ? "本地订单" : "本地订单 · 暂无")
    : (orders.length ? "订单服务" : "订单服务 · 暂无"));
  if (!orders.length) {
    container.innerHTML = `<div class="conversation-empty">暂无订单</div>`;
    const detail = $("orderDetail");
    if (detail) {
      detail.classList.add("empty");
      detail.innerHTML = `<h3>暂无订单</h3><p>体验页语音确认下单后，这里会出现订单记录。</p>`;
    }
    return;
  }
  container.innerHTML = orders.map(renderOrderRow).join("");
  document.querySelectorAll("[data-order-id]").forEach((row) => {
    row.addEventListener("click", () => runAdminTask(() => selectOrder(row.dataset.orderId)));
  });
  await selectOrder(adminState.orders.activeId || orderIdentity(orders[0]));
}

function renderDashboardOrders(orders) {
  const recent = orders.slice(0, 5);
  if (!recent.length) {
    renderDashboardState("dashboardRecentOrders", "empty", '<div class="dashboard-data-empty">暂无订单</div>');
    return;
  }
  renderDashboardState("dashboardRecentOrders", "ready", recent.map((order) => {
    const id = orderIdentity(order);
    const items = orderItems(order);
    return `
      <a class="dashboard-data-row" href="admin-commerce.html#orders">
        <span>
          <strong title="${escapeAttr(id || "未返回订单号")}">${escapeHtml(shortOrderId(id))}</strong>
          <small>${escapeHtml(orderStatus(order))}</small>
        </span>
        <span>
          <strong>￥${escapeHtml(String(orderTotal(order)))}</strong>
          <small>${items.length} 项 · ${escapeHtml(formatOrderTriggerTime(order))}</small>
        </span>
      </a>
    `;
  }).join(""));
}

function orderIdentity(order) {
  return order?.saleOrderId || order?.order_id || order?.orderId || order?.data?.saleOrderId || "";
}

function orderStatus(order) {
  return order?.displayStatus || order?.statusDesc || order?.status || order?.data?.displayStatus || order?.data?.statusDesc || "created";
}

function orderItems(order) {
  return order?.items || order?.goodses || order?.data?.goodses || [];
}

function orderTotal(order) {
  return order?.total_amount ?? order?.payAmt ?? order?.data?.payAmt ?? order?.totalAmount ?? "-";
}

function renderOrderRow(order) {
  const id = orderIdentity(order);
  const items = orderItems(order);
  const triggerTime = formatOrderTriggerTime(order);
  return `
    <button class="order-row" data-order-id="${escapeHtml(id)}" type="button">
      <span>
        <strong title="${escapeHtml(id || "未返回订单号")}">${escapeHtml(shortOrderId(id))}</strong>
        <small>${escapeHtml(orderStatus(order))}</small>
        <small>${escapeHtml(triggerTime)}</small>
      </span>
      <em>${items.length || 0} 项 · ￥${escapeHtml(String(orderTotal(order)))}</em>
    </button>
  `;
}

function shortOrderId(id) {
  if (!id) return "未返回订单号";
  if (id.length <= 12) return id;
  return `${id.slice(0, 6)}...${id.slice(-4)}`;
}

async function selectOrder(orderId) {
  if (!orderId) return;
  adminState.orders.activeId = orderId;
  document.querySelectorAll(".order-row").forEach((row) => {
    row.classList.toggle("active", row.dataset.orderId === orderId);
  });
  const detail = await fetchInternalJson("/api/orders/detail", {
    method: "POST",
    headers: { "Content-Type": "application/json" },
    body: JSON.stringify({ saleOrderId: orderId }),
  });
  renderOrderDetail(detail.ok === false ? adminState.orders.items.find((item) => orderIdentity(item) === orderId) || detail : detail);
}

function renderOrderDetail(order) {
  const container = $("orderDetail");
  if (!container) return;
  const id = orderIdentity(order);
  const items = orderItems(order);
  container.classList.remove("empty");
  container.innerHTML = `
    <div class="order-detail-head">
      <div>
        <h3>${escapeHtml(id || "订单详情")}</h3>
        <span>${escapeHtml(orderStatus(order))}</span>
      </div>
      <button id="adminRefundOrder" type="button">退单</button>
    </div>
    <div class="order-detail-summary">
      <div><span>商品数</span><strong>${items.length}</strong></div>
      <div><span>金额</span><strong>￥${escapeHtml(String(orderTotal(order)))}</strong></div>
      <div><span>来源</span><strong>${order.mock ? "本地 Mock" : "MCP Server"}</strong></div>
    </div>
    <div class="order-item-list">
      ${items.map(renderOrderItem).join("") || "<p>暂无商品明细</p>"}
    </div>
    <pre class="order-raw">${escapeHtml(formatJsonForDisplay(order))}</pre>
  `;
  $("adminRefundOrder")?.addEventListener("click", () => runAdminTask(() => refundAdminOrder(id)));
}

function renderOrderItem(item) {
  const name = item.name || item.goodsName || "-";
  const spec = item.spec || item.specName || "";
  const qty = item.quantity || item.qty || 1;
  const price = item.unit_price ?? item.salePrice ?? item.payAmt ?? "";
  return `
    <article class="order-item-row">
      <div>
        <strong>${escapeHtml(name)}</strong>
        <span>${escapeHtml(spec || "默认规格")}</span>
      </div>
      <em>x ${escapeHtml(String(qty))}</em>
      <b>${price === "" ? "" : `￥${escapeHtml(String(price))}`}</b>
    </article>
  `;
}

async function refundAdminOrder(orderId) {
  if (!orderId) return;
  setText("orderAdminStatus", "正在退单");
  const result = await fetchInternalJson("/api/orders/refund", {
    method: "POST",
    headers: { "Content-Type": "application/json" },
    body: JSON.stringify({ saleOrderId: orderId, reason: "后台订单管理退单" }),
  });
  if (result.ok === false) {
    setText("orderAdminStatus", result.message || "退单失败");
    renderOrderDetail(result);
    return;
  }
  setText("orderAdminStatus", "退单已提交");
  await loadAdminOrders();
  renderOrderDetail(result);
}

async function loadAdminProducts() {
  adminState.products = await fetchInternalJson("/api/admin/products");
  renderAdminProducts();
  setText("adminStatus", "商品库已加载");
}

function renderAdminProducts() {
  setText("dashboardProducts", String(adminState.products.length));
  const container = $("adminProducts");
  if (!container) return;
  container.innerHTML = adminState.products.map(renderProductRow).join("");
  document.querySelectorAll("[data-save-product]").forEach((button) => {
    button.addEventListener("click", () => runAdminTask(() => saveProduct(button.dataset.saveProduct)));
  });
}

async function syncAdminProducts() {
  if (!window.confirm("将从当前 MCP 门店同步商品，并清理此前的演示商品。确定继续吗？")) return;
  setText("adminStatus", "正在从 MCP 同步商品");
  const result = await fetchInternalJson("/api/admin/products/sync", {
    method: "POST",
    headers: { "Content-Type": "application/json" },
    body: "{}",
  });
  adminState.products = result.products || [];
  renderAdminProducts();
  setText(
    "adminStatus",
    `已同步 ${result.synced || 0} 个实际商品，清理 ${result.removed_legacy || 0} 个演示商品`,
  );
}

function renderProductRow(product) {
  return `
    <div class="product-edit" data-product="${product.id}">
      <label>
        <span>商品名</span>
        <input data-field="name" value="${escapeHtml(product.name)}" />
      </label>
      <label>
        <span>别名</span>
        <input data-field="aliases" value="${escapeHtml(product.aliases.join(" / "))}" />
      </label>
      <label>
        <span>规格</span>
        <input data-field="spec" value="${escapeHtml(product.spec)}" />
      </label>
      <label>
        <span>价格</span>
        <input data-field="price" type="number" step="0.01" value="${product.price}" />
      </label>
      <button data-save-product="${product.id}">保存</button>
    </div>
  `;
}

async function saveProduct(id) {
  const row = document.querySelector(`[data-product="${id}"]`);
  const field = (name) => row.querySelector(`[data-field="${name}"]`).value.trim();
  await fetchInternalJson(`/api/admin/products/${encodeURIComponent(id)}`, {
    method: "PUT",
    headers: { "Content-Type": "application/json" },
    body: JSON.stringify({
      id,
      name: field("name"),
      aliases: field("aliases").split(/[\\/，,]/).map((v) => v.trim()).filter(Boolean),
      spec: field("spec"),
      price: Number(field("price") || 0),
    }),
  });
  await loadAdminProducts();
  setText("adminStatus", "商品已保存");
}

async function addProduct() {
  const id = `sku-${Date.now()}`;
  await fetchInternalJson("/api/admin/products", {
    method: "POST",
    headers: { "Content-Type": "application/json" },
    body: JSON.stringify({ id, name: "新商品", aliases: ["新品"], spec: "默认规格", price: 1 }),
  });
  await loadAdminProducts();
  setText("adminStatus", "商品已新增");
}

function escapeHtml(value) {
  return String(value).replace(/[&<>"']/g, (ch) => ({
    "&": "&amp;",
    "<": "&lt;",
    ">": "&gt;",
    '"': "&quot;",
    "'": "&#39;",
  })[ch]);
}

function summarizeDeviceAudioProfiles(deviceConfig) {
  const profiles = deviceConfig?.audio_profiles;
  const inputDefault = profiles?.input?.default;
  const outputDefault = profiles?.output?.default;
  if (!inputDefault?.format || !inputDefault?.sample_rate
    || !outputDefault?.format || !outputDefault?.sample_rate) {
    return "音频能力未提供";
  }
  const formatProfile = (profile) => `${String(profile.format).toUpperCase()}/${profile.sample_rate / 1000}k`;
  const formatSupported = (direction) => (Array.isArray(direction?.supported) ? direction.supported : [])
    .filter((profile) => profile && typeof profile.format === "string" && Array.isArray(profile.sample_rates))
    .map((profile) => {
      const rates = profile.sample_rates
        .filter((rate) => Number.isFinite(rate) && rate > 0)
        .map((rate) => rate / 1000).join("/");
      return rates ? `${String(profile.format).toUpperCase()} ${rates}k` : "";
    })
    .filter(Boolean)
    .join("、") || "未提供";
  return `上行 ${formatProfile(inputDefault)} · 下行 ${formatProfile(outputDefault)}`
    + ` · 支持 上行 ${formatSupported(profiles.input)}；下行 ${formatSupported(profiles.output)}`;
}

async function loadDeviceConfig() {
  let deviceConfig;
  try {
    const response = await fetch(apiUrl("/api/device/config"));
    if (!response.ok) throw new Error(`HTTP ${response.status}`);
    deviceConfig = await response.json();
  } catch {
    setText("adminDeviceConfig", "设备音频配置暂不可用，请稍后重试。");
    setText("dashboardDevice", "音频能力未提供");
    return;
  }
  setText("adminDeviceConfig", formatJsonForDisplay(deviceConfig));
  const summary = summarizeDeviceAudioProfiles(deviceConfig);
  const [defaults, supported = ""] = summary.split(" · 支持 ");
  setText("dashboardDevice", defaults);
  const dashboardDevice = $("dashboardDevice");
  if (dashboardDevice) {
    dashboardDevice.title = summary;
    if (dashboardDevice.nextElementSibling && supported) {
      dashboardDevice.nextElementSibling.textContent = `支持 ${supported}`;
    }
  }
}

async function loadMiniProgramDebug() {
  const container = $("miniInterfaces");
  if (!container) return;
  const meta = await fetchInternalJson("/api/debug/miniprogram-c/interfaces");
  adminState.miniProgram.interfaces = meta.interfaces || [];
  adminState.miniProgram.missing = meta.missing_interfaces || [];
  adminState.miniProgram.activeId = adminState.miniProgram.activeId || adminState.miniProgram.interfaces[0]?.id || "";
  adminState.miniProgram.drafts = Object.fromEntries(adminState.miniProgram.interfaces.map((item) => [
    item.id,
    {
      query: item.default_query || {},
      body: item.default_body || {},
    },
  ]));
  const mockCount = adminState.miniProgram.interfaces.filter((item) => item.path_status).length;
  setText("miniTotalCount", String(adminState.miniProgram.interfaces.length));
  setText("miniMockCount", String(mockCount));
  setText("miniInterfaceCount", `${adminState.miniProgram.interfaces.length} 个接口`);
  renderMiniProgramInterfaces();
  renderMiniProgramRequestDetail();
  const missing = $("miniMissingInterfaces");
  if (missing) missing.innerHTML = adminState.miniProgram.missing.map(renderMiniProgramMissing).join("");
  setText("miniStatus", "文档已加载");
}

function renderMiniProgramInterfaces() {
  const container = $("miniInterfaces");
  if (!container) return;
  container.innerHTML = adminState.miniProgram.interfaces.map(renderMiniProgramInterface).join("");
  document.querySelectorAll("[data-mini-select]").forEach((selectable) => {
    selectable.addEventListener("click", () => selectMiniProgramInterface(selectable.dataset.miniSelect));
    selectable.addEventListener("keydown", (event) => {
      if (event.key !== "Enter" && event.key !== " ") return;
      event.preventDefault();
      selectMiniProgramInterface(selectable.dataset.miniSelect);
    });
  });
  document.querySelectorAll("[data-mini-run]").forEach((button) => {
    button.addEventListener("click", (event) => {
      event.stopPropagation();
      selectMiniProgramInterface(button.dataset.miniRun);
      runMiniProgramInterface(button.dataset.miniRun);
    });
  });
}

function renderMiniProgramInterface(item) {
  const active = item.id === adminState.miniProgram.activeId;
  const status = item.path_status ? "预置 Mock" : "正式文档";
  return `
    <article class="mini-interface-card ${active ? "active" : ""}" data-mini-interface="${escapeHtml(item.id)}">
      <div class="mini-interface-select" role="button" tabindex="0" data-mini-select="${escapeHtml(item.id)}">
        <span class="mini-method ${escapeHtml(item.method.toLowerCase())}">${escapeHtml(item.method)}</span>
        <span class="mini-interface-main">
          <strong>${escapeHtml(item.name)}</strong>
          <small>${escapeHtml(item.path)}</small>
        </span>
        <em class="${item.path_status ? "pending" : "confirmed"}">${escapeHtml(status)}</em>
      </div>
      <p>${escapeHtml(item.description || "")}</p>
      <div class="mini-card-footer">
        <div class="mini-focus">
          ${(item.response_focus || []).slice(0, 3).map((focus) => `<span>${escapeHtml(focus)}</span>`).join("")}
        </div>
        <button type="button" data-mini-run="${escapeHtml(item.id)}">运行</button>
      </div>
    </article>
  `;
}

function selectMiniProgramInterface(interfaceId) {
  persistMiniProgramDraft();
  adminState.miniProgram.activeId = interfaceId;
  renderMiniProgramInterfaces();
  renderMiniProgramRequestDetail();
}

function activeMiniProgramInterface() {
  return adminState.miniProgram.interfaces.find((entry) => entry.id === adminState.miniProgram.activeId);
}

function renderMiniProgramRequestDetail() {
  const item = activeMiniProgramInterface();
  const container = $("miniRequestDetail");
  if (!item || !container) return;
  const draft = adminState.miniProgram.drafts[item.id] || { query: item.default_query || {}, body: item.default_body || {} };
  setText("miniRequestSummary", `${item.method} · ${item.path_status ? "预置 Mock" : "正式文档"}`);
  container.innerHTML = `
    <div class="mini-request-head">
      <div>
        <span class="mini-method ${escapeHtml(item.method.toLowerCase())}">${escapeHtml(item.method)}</span>
        <strong>${escapeHtml(item.name)}</strong>
      </div>
      <button type="button" id="miniRunCurrent">运行当前接口</button>
    </div>
    <dl class="mini-request-paths">
      <div><dt>接口路径</dt><dd>${escapeHtml(item.path)}</dd></div>
      <div><dt>Mock 路径</dt><dd>${escapeHtml(item.mock_path)}</dd></div>
    </dl>
    <label>
      <span>Query JSON</span>
      <textarea id="miniActiveQuery">${escapeHtml(JSON.stringify(draft.query || {}, null, 2))}</textarea>
    </label>
    ${item.method === "POST" ? `
      <label>
        <span>Body JSON</span>
        <textarea id="miniActiveBody">${escapeHtml(JSON.stringify(draft.body || {}, null, 2))}</textarea>
      </label>
    ` : ""}
    <div class="mini-focus expanded">
      ${(item.response_focus || []).map((focus) => `<span>${escapeHtml(focus)}</span>`).join("")}
    </div>
  `;
  $("miniRunCurrent")?.addEventListener("click", () => runMiniProgramInterface(item.id));
  ["miniActiveQuery", "miniActiveBody"].forEach((id) => {
    $(id)?.addEventListener("input", persistMiniProgramDraft);
  });
}

function persistMiniProgramDraft() {
  const item = activeMiniProgramInterface();
  if (!item) return;
  const query = $("miniActiveQuery")?.value;
  const body = $("miniActiveBody")?.value;
  const draft = adminState.miniProgram.drafts[item.id] || {};
  try {
    if (query != null) draft.query = JSON.parse(query || "{}");
    if (body != null) draft.body = JSON.parse(body || "{}");
    adminState.miniProgram.drafts[item.id] = draft;
  } catch {
    // Keep the text in place; validation happens when the user runs the request.
  }
}

function renderMiniProgramMissing(item) {
  const reason = miniProgramCoverageReason(item.name, item.reason);
  return `
    <article class="mini-missing-card">
      <header>
        <strong>${escapeHtml(item.name)}</strong>
        <em>待正式文档确认</em>
      </header>
      <p>${escapeHtml(reason)}</p>
    </article>
  `;
}

function miniProgramCoverageReason(name, fallback) {
  if (name === "创建订单") {
    return "已接入本地 Mock 下单流程，可验证语音识别、多商品确认、订单落库与会话关联。";
  }
  if (name === "取消订单") {
    return "已保留取消分支调试入口；正式接口确认后替换请求路径、字段和状态映射。";
  }
  if (name === "支付/退款申请") {
    return "已拆分为发起支付和申请退款两个 Mock 入口，用于覆盖支付态与售后态联调。";
  }
  return fallback || "当前使用本地 Mock 完成联调，待正式文档补齐后替换。";
}

function miniProgramHeaders() {
  return {
    "__app": getValue("miniHeaderApp", "mjy-miniapp"),
    "__appver": getValue("miniHeaderAppVer", "1.0.0"),
    "__company": getValue("miniHeaderCompany", "CC"),
    "__store": getValue("miniHeaderStore", "999006940"),
    "__storeno": getValue("miniHeaderStoreNo", "6634"),
    "__src_channel": getValue("miniHeaderSrcChannel", "2"),
    "CompanyCode": getValue("miniHeaderCompanyCode", "CC"),
    "Authorization": getValue("miniHeaderAuthorization", "Bearer mock-token"),
    "debug": getValue("miniHeaderDebug", "true"),
  };
}

function readMiniProgramQuery(interfaceId) {
  persistMiniProgramDraft();
  const draft = adminState.miniProgram.drafts[interfaceId];
  return draft?.query || {};
}

function readMiniProgramBody(interfaceId) {
  persistMiniProgramDraft();
  const draft = adminState.miniProgram.drafts[interfaceId];
  return draft?.body || {};
}

async function runMiniProgramInterface(interfaceId) {
  if (adminState.miniProgram.activeId !== interfaceId) {
    adminState.miniProgram.activeId = interfaceId;
    renderMiniProgramInterfaces();
    renderMiniProgramRequestDetail();
  }
  const item = adminState.miniProgram.interfaces.find((entry) => entry.id === interfaceId);
  if (!item) return null;
  setText("miniStatus", `调试中：${item.name}`);
  try {
    const response = await fetchInternalJson("/api/debug/miniprogram-c/call", {
      method: "POST",
      headers: { "Content-Type": "application/json" },
      body: JSON.stringify({
        interface_id: interfaceId,
        query: readMiniProgramQuery(interfaceId),
        body: readMiniProgramBody(interfaceId),
        headers: miniProgramHeaders(),
      }),
    });
    const ok = response.ok && response.response?.code === 0;
    renderMiniProgramResult(item, response, ok);
    setText("miniStatus", ok ? `已通过：${item.name}` : `调试失败：${item.name}`);
    return { item, response, ok };
  } catch (error) {
    const response = { ok: false, message: error.message };
    renderMiniProgramResult(item, response, false);
    setText("miniStatus", `调试失败：${item.name}`);
    return { item, response, ok: false };
  }
}

async function runAllMiniProgramInterfaces() {
  persistMiniProgramDraft();
  const previous = adminState.miniProgram.activeId;
  const results = [];
  for (const item of adminState.miniProgram.interfaces) {
    results.push(await runMiniProgramInterface(item.id));
  }
  adminState.miniProgram.activeId = previous || adminState.miniProgram.interfaces[0]?.id || "";
  renderMiniProgramInterfaces();
  renderMiniProgramRequestDetail();
  const okCount = results.filter((item) => item?.ok).length;
  setText("miniStatus", `已完成 ${okCount}/${results.length}`);
  setText("miniResultSummary", `全部调试：${okCount}/${results.length} 通过`);
  setText("miniResult", formatJsonForDisplay(results));
}

function renderMiniProgramResult(item, response, ok) {
  setText("miniResultSummary", `${item.name} · ${ok ? "通过" : "失败"}`);
  setText("miniResult", formatJsonForDisplay(response));
}

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
      const nextIndex = event.key === "Home"
        ? 0
        : event.key === "End"
          ? tabs.length - 1
          : (index + (event.key === "ArrowRight" ? 1 : -1) + tabs.length) % tabs.length;
      activateTab(tabs[nextIndex].dataset.adminTab, { updateHash: true, focus: true });
    });
  });
  window.addEventListener("popstate", () => activateTab(location.hash.slice(1)));
  activateTab(location.hash.slice(1));
}

function bindAdminEvents() {
  ["adminSaveTop", "adminSaveBottom"].forEach((id) => {
    const button = $(id);
    if (button) button.addEventListener("click", () => runAdminTask(saveAdminConfig));
  });
  const addButton = $("adminAddProduct");
  if (addButton) addButton.addEventListener("click", () => runAdminTask(addProduct));
  const syncProductsButton = $("adminSyncProducts");
  if (syncProductsButton) syncProductsButton.addEventListener("click", () => runAdminTask(syncAdminProducts));
  const conversationPrev = $("conversationPrev");
  const conversationNext = $("conversationNext");
  if (conversationPrev) {
    conversationPrev.addEventListener("click", () => runAdminTask(() => loadConversations(adminState.conversations.page - 1)));
  }
  if (conversationNext) {
    conversationNext.addEventListener("click", () => runAdminTask(() => loadConversations(adminState.conversations.page + 1)));
  }
  const refreshOrders = $("adminRefreshOrders");
  if (refreshOrders) refreshOrders.addEventListener("click", () => runAdminTask(loadAdminOrders));
  const miniRunAll = $("miniRunAll");
  if (miniRunAll) miniRunAll.addEventListener("click", runAllMiniProgramInterfaces);
  ["adminVoiceName", "adminVoiceCode"].forEach((id) => {
    const select = $(id);
    if (select) select.addEventListener("change", () => syncAdminVoiceSelects(id));
  });
  const orderMcpEnabled = $("adminOrderMcpEnabled");
  if (orderMcpEnabled) {
    orderMcpEnabled.addEventListener("change", () => {
      renderOrderMcpMode({ order_mcp_enabled: orderMcpEnabled.checked });
      setAdminConfigStatus("有未保存修改");
    });
  }

  document.querySelectorAll(".admin-panel input, .admin-panel textarea, .admin-panel select").forEach((input) => {
    input.addEventListener("input", () => {
      if (input.dataset.orderMcpTool) syncOrderMcpToolsField();
      if (input.closest('[data-admin-panel="order-mcp"]')) {
        setAdminConfigStatus("有未保存修改");
      } else {
        setText("adminStatus", "有未保存修改");
      }
    });
  });
}

initAdminTabs();
bindAdminEvents();
runAdminTask(loadAdminConfig);
runAdminTask(loadAdminProducts);
runAdminTask(loadAdminOrders, ["dashboardRecentOrders"]);
runAdminTask(loadDeviceConfig);
runAdminTask(loadConversations, ["dashboardRecentConversations"]);
runAdminTask(loadMiniProgramDebug);
