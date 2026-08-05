#!/usr/bin/env node
import fs from "node:fs/promises";
import path from "node:path";
import { fileURLToPath } from "node:url";

const ROOT = path.resolve(path.dirname(fileURLToPath(import.meta.url)), "..");
const BASE_URL = process.env.UI_BASE_URL || "http://127.0.0.1:8787";
const OUT_DIR = process.env.UI_ACCEPTANCE_OUT || path.join(ROOT, "ui-report");

const pages = [
  { id: "experience", path: "/", title: "体验页", required: [".app-header", ".studio", ".config-column", ".voice-column", ".cart-column", "#micBtn", "#draft", "#latencyStages"] },
  { id: "admin-login", path: "/admin-login.html", title: "管理员登录", required: ["#loginForm", "#username", "#password", "#loginButton"] },
  {
    id: "admin-overview",
    path: "/admin.html",
    title: "概览",
    activeMenu: "概览",
    required: [".admin-menu", ".dashboard-grid", ".metric-card", "#dashboardRecentOrders", "#dashboardRecentConversations"],
  },
  {
    id: "admin-voice",
    path: "/admin-voice.html",
    title: "语音配置",
    activeMenu: "语音配置",
    required: ["#adminAppId", "#adminVoiceName", "#adminModel", "#adminRolePrompt"],
    tabs: ["capability", "model", "prompts"],
  },
  {
    id: "admin-commerce",
    path: "/admin-commerce.html",
    title: "商品与订单",
    activeMenu: "商品与订单",
    required: ["#adminProducts", "#adminStatus", "#orderList", "#adminOrderMcpEnabled", "#adminOrderMcpSaveStatus"],
    tabs: ["products", "orders", "order-mcp"],
  },
  { id: "admin-conversations", path: "/admin-conversations.html", title: "会话记录", activeMenu: "会话记录", required: ["#conversationList", "#conversationDetail", "#conversationPager"] },
  { id: "admin-authorizations", path: "/admin-authorizations.html", title: "授权管理", activeMenu: "接入管理", required: ["#createAuthorizationForm", "#authorizationList", "#secretDialog"] },
  {
    id: "admin-integrations",
    path: "/admin-integrations.html",
    title: "接入管理",
    activeMenu: "接入管理",
    required: ["#adminDeviceConfig", "#manageDeviceAuthorizations", "#miniInterfaces", "#miniRunAll", "#miniResult", "#miniMissingInterfaces"],
    tabs: ["devices", "miniprogram"],
  },
];

const viewports = [
  { id: "desktop", width: 1440, height: 1000 },
  { id: "tablet", width: 900, height: 1500 },
  { id: "mobile", width: 390, height: 844 },
];

const bannedCopy = [
  "自主规划",
  "agent_",
  "真实讯飞链路",
  "未发布",
  "IAT returned empty text",
  "missing iat payload.result.text",
];

const failures = [];
const warnings = [];
const results = [];

function addFailure(page, viewport, message) {
  failures.push({ page, viewport, message });
}

function addWarning(page, viewport, message) {
  warnings.push({ page, viewport, message });
}

async function ensurePlaywright() {
  try {
    return await import("playwright");
  } catch (error) {
    console.error("缺少 Playwright 依赖。先执行：npm install");
    console.error(error.message);
    process.exit(2);
  }
}

async function ensureServer() {
  try {
    const res = await fetch(`${BASE_URL}/api/health`);
    if (!res.ok) throw new Error(`HTTP ${res.status}`);
  } catch (error) {
    console.error(`本地服务不可访问：${BASE_URL}`);
    console.error("先执行：scripts/start-dev.sh --foreground");
    console.error(error.message);
    process.exit(2);
  }
}

async function run() {
  if (process.argv.includes("--help")) {
    console.log(`Usage: npm run ui:check

Environment:
  UI_BASE_URL=http://127.0.0.1:8787
  UI_ACCEPTANCE_OUT=ui-report

Checks:
  - 页面可访问与关键元素存在
  - 控制台 error
  - 横向溢出
  - 顶部/侧栏/菜单 active 一致性
  - 按钮与卡片尺寸协调
  - 长文本异常断裂
  - 技术/编辑器残留文案
  - 桌面和移动端截图`);
    return;
  }

  const adminSource = await fs.readFile(path.join(ROOT, "static/admin.js"), "utf8");
  if (adminSource.includes(".audio.sample_rate") || adminSource.includes(".audio.format")) {
    addFailure("admin-device-config-contract", "static", "仍在访问旧 deviceConfig.audio 音频结构");
  }
  if (!adminSource.includes("function summarizeDeviceAudioProfiles")) {
    addFailure("admin-device-config-contract", "static", "缺少新 audio_profiles 安全摘要函数");
  }
  if (!adminSource.includes("function conversationSourceLabel")
    || !adminSource.includes("conversationSourceLabel(row.device_id)")) {
    addFailure("admin-conversation-owner", "static", "历史对话列表未展示设备或体验页来源");
  }
  if (!adminSource.includes("function enterInternalOnlyMode")
    || !adminSource.includes("async function fetchInternalJson")) {
    addFailure("admin-internal-access", "static", "管理页缺少公网 403 的仅限本机降级处理");
  }
  if (!adminSource.includes('fetchInternalJson("/api/orders/list"')
    || !adminSource.includes('fetchInternalJson("/api/orders/detail"')
    || !adminSource.includes('fetchInternalJson("/api/orders/refund"')
    || !adminSource.includes("await selectOrder(")) {
    addFailure("admin-orders-internal-access", "static", "后台订单请求或详情异步链路未纳入仅限本机门禁");
  }
  const loginSource = await fs.readFile(path.join(ROOT, "static/admin-login.html"), "utf8");
  if (!loginSource.includes('value="myjadmin"')
    || !loginSource.includes('class="auth-intro"')
    || !loginSource.includes('class="auth-security-note"')) {
    addFailure("admin-login-contract", "static", "登录页用户名或居中卡片结构未更新");
  }
  const publicPathSources = {
    "static/app.js": await fs.readFile(path.join(ROOT, "static/app.js"), "utf8"),
    "static/admin.js": adminSource,
    "static/admin-auth.js": await fs.readFile(path.join(ROOT, "static/admin-auth.js"), "utf8"),
    "static/admin-login.html": loginSource,
  };
  for (const [file, source] of Object.entries(publicPathSources)) {
    const canonicalIndex = source.indexOf('"/myj-voice-shop"');
    const legacyIndex = source.indexOf('"/mjy-voice-shop"');
    if (canonicalIndex < 0 || legacyIndex < 0 || canonicalIndex > legacyIndex) {
      addFailure("public-base-path-contract", "static", `${file} 未将 /myj-voice-shop 设为主路径并兼容 /mjy-voice-shop`);
    }
  }
  const nginxLocationSource = await fs.readFile(
    path.join(ROOT, "deploy/mjy-voice-shop-nginx.locations.conf"),
    "utf8",
  ).catch(() => "");
  if (!nginxLocationSource.includes("location ^~ /myj-voice-shop/")
    || !nginxLocationSource.includes("location ^~ /mjy-voice-shop/")) {
    addFailure("public-base-path-contract", "static", "Nginx 路由模板未同时提供新主路径和旧兼容路径");
  }

  await ensureServer();
  const { chromium } = await ensurePlaywright();
  await fs.rm(OUT_DIR, { recursive: true, force: true });
  await fs.mkdir(path.join(OUT_DIR, "screenshots"), { recursive: true });

  const browser = await chromium.launch({ headless: true });
  const page = await browser.newPage();
  const consoleErrors = [];
  page.on("console", (msg) => {
    if (msg.type() === "error") consoleErrors.push(msg.text());
  });
  page.on("pageerror", (error) => consoleErrors.push(error.message));

  for (const target of pages) {
    for (const viewport of viewports) {
      consoleErrors.length = 0;
      await page.setViewportSize({ width: viewport.width, height: viewport.height });
      const url = `${BASE_URL}${target.path}`;
      const response = await page.goto(url, { waitUntil: "networkidle" });
      if (!response || !response.ok()) {
        addFailure(target.id, viewport.id, `页面访问失败：${response?.status() || "no response"}`);
        continue;
      }

      const screenshotName = `${target.id}-${viewport.id}.png`;
      await page.screenshot({
        path: path.join(OUT_DIR, "screenshots", screenshotName),
        fullPage: true,
      });

      const runPlaybackPrebufferAudit = target.id === "experience" && viewport.id === "desktop";
      const audit = await page.evaluate(({ target, bannedCopy, runPlaybackPrebufferAudit }) => {
        const rectOf = (selector) => {
          const el = document.querySelector(selector);
          if (!el) return null;
          const rect = el.getBoundingClientRect();
          return { width: rect.width, height: rect.height, top: rect.top, left: rect.left };
        };

        const visible = (el) => {
          const rect = el.getBoundingClientRect();
          const style = getComputedStyle(el);
          return rect.width > 0 && rect.height > 0 && style.visibility !== "hidden" && style.display !== "none";
        };

        const textOverflow = [...document.querySelectorAll("button, a, h1, h2, h3, strong, .metric-card strong, .brand-block p, .section-head span")]
          .filter(visible)
          .map((el) => {
            const rect = el.getBoundingClientRect();
            const text = (el.textContent || "").trim();
            const lines = el.getClientRects().length;
            return {
              text,
              selector: el.tagName.toLowerCase() + (el.className ? `.${String(el.className).split(" ").join(".")}` : ""),
              width: Math.round(rect.width),
              height: Math.round(rect.height),
              scrollWidth: el.scrollWidth,
              clientWidth: el.clientWidth,
              lines,
            };
          })
          .filter((item) => {
            if (!item.text) return false;
            if (item.scrollWidth > item.clientWidth + 2) return true;
            if (/^[A-Za-z0-9_-]{12,}$/.test(item.text) && item.lines > 2) return true;
            if (item.text.length <= 8) return false;
            return item.width < 96 && item.lines > 2;
          })
          .slice(0, 20);

        const narrowCards = [...document.querySelectorAll(".metric-card, .admin-panel, .workflow-lane")]
          .filter(visible)
          .map((el) => {
            const rect = el.getBoundingClientRect();
            return {
              className: el.className,
              title: el.querySelector("h2,h3,strong,span")?.textContent?.trim() || "",
              width: Math.round(rect.width),
              height: Math.round(rect.height),
            };
          })
          .filter((item) => item.width < 180);

        const requiredMissing = target.required.filter((selector) => !document.querySelector(selector));
        const overviewStates = target.id === "admin-overview"
          ? ["dashboardRecentOrders", "dashboardRecentConversations"].map((id) => {
              const container = document.getElementById(id);
              return {
                id,
                state: container?.dataset.state || "missing",
                rows: container?.querySelectorAll(".dashboard-data-row").length || 0,
              };
            })
          : [];
        const activeMenu = document.querySelector(".admin-menu a.active")?.textContent?.trim() || "";
        const activeCount = document.querySelectorAll(".admin-menu a.active").length;
        const adminMenuItems = document.querySelectorAll(".admin-menu a").length;
        const tabs = [...document.querySelectorAll("[data-admin-tab]")].map((tab) => ({
          name: tab.dataset.adminTab,
          selected: tab.getAttribute("aria-selected"),
          controls: tab.getAttribute("aria-controls"),
        }));
        const visiblePanels = [...document.querySelectorAll("[data-admin-panel]")]
          .filter((panel) => !panel.hidden)
          .map((panel) => panel.dataset.adminPanel);
        const bodyText = document.body.textContent || "";
        const voiceEditor = document.querySelector(".voice-editor");
        const modelSummary = [...document.querySelectorAll(".summary-card")]
          .find((el) => (el.textContent || "").includes("模型与断句"));
        const voiceConfigLayout = target.id === "experience" && voiceEditor && modelSummary
          ? (() => {
              const voiceRect = voiceEditor.getBoundingClientRect();
              const modelRect = modelSummary.getBoundingClientRect();
              const controls = [...voiceEditor.querySelectorAll("input, select")].map((el) => {
                const rect = el.getBoundingClientRect();
                return {
                  id: el.id,
                  top: Math.round(rect.top),
                  bottom: Math.round(rect.bottom),
                  clipped: rect.top < voiceRect.top - 1 || rect.bottom > voiceRect.bottom + 1,
                };
              });
              return {
                gapToNext: Math.round(modelRect.top - voiceRect.bottom),
                voiceHeight: Math.round(voiceRect.height),
                clippedControls: controls.filter((item) => item.clipped),
              };
            })()
          : null;
        const latencyLayout = target.id === "experience"
          ? (() => {
              const stages = document.getElementById("latencyStages");
              if (!stages) return null;
              const original = stages.innerHTML;
              stages.innerHTML = Array.from({ length: 9 }, (_, index) => `
                <div class="latency-stage active">
                  <span>${["开始推流", "服务端接收", "转写完成", "模型首字", "首段回复", "语音合成开始", "TTS 首包", "开始播报", "语音合成完成"][index]}</span>
                  <strong>${index ? `${index * 731}ms` : "--"}</strong>
                  <small>${index % 2 ? "1120 bytes · 流式文本" : "14 字 · 流式合成"}</small>
                </div>
              `).join("");
              const stageRects = [...stages.querySelectorAll(".latency-stage")]
                .map((el) => el.getBoundingClientRect());
              const strongRects = [...stages.querySelectorAll(".latency-stage strong")]
                .map((el) => el.getBoundingClientRect());
              const containerRect = stages.getBoundingClientRect();
              const style = getComputedStyle(stages);
              const minRightGap = Math.min(...strongRects.map((rect) => containerRect.right - rect.right));
              const clippedStages = stageRects.filter((rect) => rect.right > containerRect.right - 8).length;
              const scrollable = stages.scrollHeight > stages.clientHeight + 2;
              stages.innerHTML = original;
              return {
                minRightGap: Math.round(minRightGap),
                clippedStages,
                scrollable,
                paddingRight: style.paddingRight,
                overflowY: style.overflowY,
              };
            })()
          : null;
        const asrEmptyHandling = target.id === "experience"
          ? (() => {
              const messages = document.getElementById("messages");
              if (!messages || typeof window.handleEvent !== "function") return null;
              const before = messages.children.length;
              window.handleEvent({
                event_type: "error",
                payload: {
                  code: "asr_failed",
                  message: "没有识别到有效语音，请再说一遍",
                },
              });
              return {
                before,
                after: messages.children.length,
                leakedText: messages.textContent.includes("没有识别到有效语音"),
              };
            })()
          : null;

        return {
          title: document.title,
          requiredMissing,
          overviewStates,
          activeMenu,
          activeCount,
          adminMenuItems,
          tabs,
          visiblePanels,
          bannedHits: bannedCopy.filter((word) => bodyText.includes(word)),
          docWidth: document.documentElement.clientWidth,
          scrollWidth: document.documentElement.scrollWidth,
          bodyWidth: document.body.scrollWidth,
          header: rectOf(".app-header"),
          adminLayout: rectOf(".admin-layout"),
          adminMenu: rectOf(".admin-menu"),
          adminShell: rectOf(".admin-shell"),
          authCard: rectOf(".auth-card"),
          loginUsername: target.id === "admin-login"
            ? document.getElementById("username")?.value || ""
            : "",
          controlsOnExperience: target.id === "experience"
            ? [...document.querySelectorAll(".config-column input, .config-column textarea, .config-column select, .voice-column input, .voice-column select, .voice-column textarea, .cart-column input, .cart-column textarea, .cart-column select")]
                .filter((el) => ![
                  "textInput",
                  "ttsProviderControl",
                  "voiceNameControl",
                  "voiceCodeControl",
                  "modelControl",
                  "silenceMsControl",
                  "minSpeechMsControl",
                  "thresholdControl",
                  "preSpeechMsControl",
                  "ttsNoInterruptControl",
                  "ttsInterruptWordControl",
                ].includes(el.id))
                .map((el) => el.id || el.tagName.toLowerCase())
            : [],
          experienceHasLeftProductList: target.id === "experience"
            ? Boolean(document.querySelector("#productList, #productCount"))
            : false,
          experienceTuningControls: target.id === "experience"
            ? ["silenceMsControl", "minSpeechMsControl", "thresholdControl", "preSpeechMsControl", "ttsNoInterruptControl", "ttsInterruptWordControl"]
                .filter((id) => document.getElementById(id))
            : [],
          orderMcpMapping: target.id === "admin-commerce"
            ? [...document.querySelectorAll("[data-order-mcp-tool]")].map((input) => ({
                key: input.dataset.orderMcpTool,
                value: input.value,
                disabled: input.disabled,
                readOnly: input.readOnly,
              }))
            : [],
          deviceAudioSummary: target.id === "admin-integrations" && typeof window.summarizeDeviceAudioProfiles === "function"
            ? {
                missing: window.summarizeDeviceAudioProfiles({}),
                current: window.summarizeDeviceAudioProfiles({
                  audio_profiles: {
                    input: {
                      default: { format: "mp3", sample_rate: 16000 },
                      supported: [
                        { format: "mp3", sample_rates: [16000] },
                        { format: "pcm", sample_rates: [16000] },
                      ],
                    },
                    output: {
                      default: { format: "mp3", sample_rate: 16000 },
                      supported: [
                        { format: "mp3", sample_rates: [8000, 16000, 24000] },
                        { format: "pcm", sample_rates: [16000] },
                      ],
                    },
                  },
                }),
                malformed: (() => {
                  try {
                    return window.summarizeDeviceAudioProfiles({ audio_profiles: {
                      input: { default: { format: "mp3", sample_rate: 16000 }, supported: {} },
                      output: { default: { format: "mp3", sample_rate: 16000 }, supported: [{ format: "pcm", sample_rates: "bad" }] },
                    } });
                  } catch { return "THREW"; }
                })(),
              }
            : null,
          orderRouteExamples: target.id === "admin-conversations" && typeof window.renderOrderRouteSummary === "function"
            ? {
                local: window.renderOrderRouteSummary([
                  { event_type: "order_create_call", payload: { mcp_enabled: false } },
                  { event_type: "order_create_fallback", payload: { reason: { code: "ORDER_MCP_DISABLED" } } },
                  { event_type: "order_persisted", payload: { mock: true } },
                ], []),
                mcp: window.renderOrderRouteSummary([
                  { event_type: "order_create_call", payload: { tool: "createOrder" } },
                  { event_type: "order_persisted", payload: { mock: false } },
                ], []),
                fallback: window.renderOrderRouteSummary([
                  { event_type: "order_create_call", payload: { tool: "createOrder" } },
                  { event_type: "order_create_fallback", payload: { reason: { code: "ORDER_MCP_UNAVAILABLE" } } },
                  { event_type: "order_persisted", payload: { mock: true } },
                ], []),
              }
            : null,
          experienceScrollers: target.id === "experience"
            ? [...document.querySelectorAll(".column")].map((el) => ({
                className: el.className,
                scrollHeight: el.scrollHeight,
                clientHeight: el.clientHeight,
                overflowY: getComputedStyle(el).overflowY,
                isConfig: el.classList.contains("config-column"),
              }))
            : [],
          micButton: target.id === "experience" ? rectOf("#micBtn") : null,
          textInput: target.id === "experience" ? rectOf("#textInput") : null,
          voiceConfigLayout,
          latencyLayout,
          asrEmptyHandling,
          textOverflow,
          narrowCards,
          primaryButtons: [...document.querySelectorAll("button, .primary-link, .header-actions a")]
            .filter(visible)
            .map((el) => {
              const rect = el.getBoundingClientRect();
              return { text: el.textContent.trim(), width: Math.round(rect.width), height: Math.round(rect.height) };
            }),
          playbackPrebuffer: runPlaybackPrebufferAudit
            ? (() => {
                if (!window.MediaSource?.isTypeSupported?.("audio/mpeg")) {
                  return { skipped: true, reason: "media-source-unsupported" };
                }
                const originalAudio = window.Audio;
                let audioConstructorCalls = 0;
                window.Audio = function FakeAudio() {
                  audioConstructorCalls += 1;
                  return {
                    play: () => new Promise(() => {}),
                    set onended(_handler) {},
                    set onerror(_handler) {},
                  };
                };
                try {
                  window.enqueueTtsAudio({ seq: 901, audio: btoa("abc"), is_last: false });
                  return { skipped: false, audioConstructorCalls };
                } finally {
                  window.Audio = originalAudio;
                }
              })()
            : null,
        };
      }, { target, bannedCopy, runPlaybackPrebufferAudit });

      if (consoleErrors.length) addFailure(target.id, viewport.id, `控制台错误：${consoleErrors.join(" | ")}`);
      if (audit.requiredMissing.length) addFailure(target.id, viewport.id, `关键元素缺失：${audit.requiredMissing.join(", ")}`);
      if (audit.bannedHits.length) addFailure(target.id, viewport.id, `残留不应出现的文案：${audit.bannedHits.join(", ")}`);
      if (audit.scrollWidth > audit.docWidth + 2 || audit.bodyWidth > audit.docWidth + 2) {
        addFailure(target.id, viewport.id, `页面横向溢出：doc=${audit.docWidth}, scroll=${audit.scrollWidth}, body=${audit.bodyWidth}`);
      }
      if (target.activeMenu && audit.activeMenu !== target.activeMenu) {
        addFailure(target.id, viewport.id, `菜单 active 错误：期望 ${target.activeMenu}，实际 ${audit.activeMenu || "无"}`);
      }
      if (target.activeMenu && audit.activeCount !== 1) {
        addFailure(target.id, viewport.id, `菜单 active 数量异常：${audit.activeCount}`);
      }
      if (target.activeMenu && audit.adminMenuItems !== 5) {
        addFailure(target.id, viewport.id, `后台一级菜单数量异常：${audit.adminMenuItems}`);
      }
      if (target.id === "admin-overview") {
        const invalidStates = audit.overviewStates.filter((item) => !["ready", "empty"].includes(item.state));
        const oversizedStates = audit.overviewStates.filter((item) => item.rows > 5);
        if (invalidStates.length) {
          addFailure(
            target.id,
            viewport.id,
            `概览数据区域状态异常：${invalidStates.map((item) => `${item.id}=${item.state}`).join(", ")}`,
          );
        }
        if (oversizedStates.length) {
          addFailure(
            target.id,
            viewport.id,
            `概览数据区域行数超过 5：${oversizedStates.map((item) => `${item.id}=${item.rows}`).join(", ")}`,
          );
        }
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
      if (target.id === "admin-login") {
        const centerX = audit.authCard.left + audit.authCard.width / 2;
        const centerY = audit.authCard.top + audit.authCard.height / 2;
        if (Math.abs(centerX - viewport.width / 2) > 3
          || Math.abs(centerY - viewport.height / 2) > 40
          || audit.loginUsername !== "myjadmin") {
          addFailure(target.id, viewport.id, "登录卡片未居中或默认用户名错误");
        }
      }
      if (target.id === "admin-commerce") {
        const expectedKeys = ["resolve_context", "authorize_member", "preview_order", "create_order", "list_orders", "get_order_detail", "refund_order"];
        const keys = audit.orderMcpMapping.map((item) => item.key);
        const missingKeys = expectedKeys.filter((key) => !keys.includes(key));
        const lockedKeys = audit.orderMcpMapping.filter((item) => item.disabled || item.readOnly).map((item) => item.key);
        if (audit.orderMcpMapping.length < expectedKeys.length || missingKeys.length) {
          addFailure(target.id, viewport.id, `订单能力映射输入缺失：${missingKeys.join(", ") || `${audit.orderMcpMapping.length}/${expectedKeys.length}`}`);
        }
        if (lockedKeys.length) {
          addFailure(target.id, viewport.id, `订单能力映射不应只读：${lockedKeys.join(", ")}`);
        }
      }
      if (target.id === "admin-conversations") {
        const examples = audit.orderRouteExamples;
        if (!examples
          || !examples.local.includes("MCP 未启用")
          || !examples.mcp.includes("MCP Server 调用成功")
          || !examples.fallback.includes("MCP 调用失败")) {
          addFailure(target.id, viewport.id, "订单链路未清晰区分本地、MCP 成功和失败兜底");
        }
      }
      if (target.id === "admin-integrations") {
        if (!audit.deviceAudioSummary
          || !audit.deviceAudioSummary.missing.includes("未提供")
          || !audit.deviceAudioSummary.current.includes("上行 MP3/16k")
          || !audit.deviceAudioSummary.current.includes("下行 MP3/16k")
          || !audit.deviceAudioSummary.current.includes("MP3 8/16/24k")
          || audit.deviceAudioSummary.malformed === "THREW") {
          addFailure(target.id, viewport.id, "新 audio_profiles 默认档位、支持档位或缺失 fallback 摘要错误");
        }
      }
      if (target.id === "experience" && viewport.id === "desktop") {
        if (audit.playbackPrebuffer && !audit.playbackPrebuffer.skipped && audit.playbackPrebuffer.audioConstructorCalls !== 0) {
          addFailure(target.id, viewport.id, "TTS 流式播放缺少预缓冲：首个小音频包已经进入播放队列");
        }
        if (audit.experienceHasLeftProductList) {
          addFailure(target.id, viewport.id, "体验页左侧不应再出现商品库列表");
        }
        if (audit.experienceTuningControls.length !== 6) {
          addFailure(target.id, viewport.id, `体验配置控件缺失：${audit.experienceTuningControls.length}/6`);
        }
        if (audit.controlsOnExperience.length) {
          addFailure(target.id, viewport.id, `体验页出现配置控件：${audit.controlsOnExperience.join(", ")}`);
        }
        const badScroller = audit.experienceScrollers.find((item) => {
          if (item.isConfig) {
            return item.scrollHeight > item.clientHeight + 2 && !["auto", "scroll"].includes(item.overflowY);
          }
          return (item.overflowY !== "visible" && item.overflowY !== "hidden") || item.scrollHeight > item.clientHeight + 2;
        });
        if (badScroller) {
          addFailure(target.id, viewport.id, `体验页列滚动异常：${badScroller.className} ${badScroller.scrollHeight}/${badScroller.clientHeight} overflow=${badScroller.overflowY}`);
        }
        if (audit.micButton && audit.micButton.top + audit.micButton.height > viewport.height - 12) {
          addFailure(target.id, viewport.id, `语音按钮超出首屏：top=${Math.round(audit.micButton.top)}, height=${Math.round(audit.micButton.height)}, viewport=${viewport.height}`);
        }
        if (audit.textInput && audit.micButton && audit.micButton.top < audit.textInput.top) {
          addFailure(target.id, viewport.id, "语音按钮位置异常：按钮跑到输入框上方");
        }
      }
      if (target.id === "experience" && audit.voiceConfigLayout) {
        if (audit.voiceConfigLayout.clippedControls.length) {
          addFailure(
            target.id,
            viewport.id,
            `语音配置控件被裁切：${audit.voiceConfigLayout.clippedControls.map((item) => item.id).join(", ")}`,
          );
        }
        if (audit.voiceConfigLayout.gapToNext < 18) {
          addFailure(target.id, viewport.id, `语音配置卡片和下一模块间距不足：${audit.voiceConfigLayout.gapToNext}px`);
        }
      }
      if (target.id === "experience" && audit.latencyLayout) {
        if (audit.latencyLayout.clippedStages) {
          addFailure(target.id, viewport.id, `链路诊断条目贴到滚动条：${audit.latencyLayout.clippedStages} 项`);
        }
        if (audit.latencyLayout.minRightGap < 12) {
          addFailure(target.id, viewport.id, `链路诊断耗时列右侧留白不足：${audit.latencyLayout.minRightGap}px`);
        }
      }
      if (target.id === "experience" && audit.asrEmptyHandling) {
        if (audit.asrEmptyHandling.after !== audit.asrEmptyHandling.before || audit.asrEmptyHandling.leakedText) {
          addFailure(target.id, viewport.id, "短时空 ASR 错误不应显示为聊天消息");
        }
      }
      if (viewport.id === "desktop") {
        if (audit.header && (audit.header.height < 56 || audit.header.height > 92)) {
          addWarning(target.id, viewport.id, `顶部高度不协调：${Math.round(audit.header.height)}px`);
        }
        if (audit.adminMenu && (audit.adminMenu.width < 190 || audit.adminMenu.width > 280)) {
          addWarning(target.id, viewport.id, `后台菜单宽度不协调：${Math.round(audit.adminMenu.width)}px`);
        }
        if (audit.narrowCards.length) {
          addFailure(target.id, viewport.id, `卡片过窄：${audit.narrowCards.map((c) => `${c.title || c.className} ${c.width}px`).join("; ")}`);
        }
      }
      if (audit.textOverflow.length) {
        addFailure(target.id, viewport.id, `文字溢出/异常断裂：${audit.textOverflow.map((t) => `${t.text}(${t.width}px/${t.lines}行)`).join("; ")}`);
      }
      const badButton = audit.primaryButtons.find((button) => button.height < 36 || button.height > 58 || button.width < 40);
      if (badButton) addWarning(target.id, viewport.id, `按钮尺寸异常：${badButton.text} ${badButton.width}x${badButton.height}`);

      results.push({ page: target.id, viewport: viewport.id, screenshot: `screenshots/${screenshotName}`, audit });
    }
  }

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
    const alternate = target.tabs[2] || target.tabs[0];
    await tabPage.locator(`[data-admin-tab="${alternate}"]`).click();
    await tabPage.goBack();
    const backSelected = await tabPage.locator('[data-admin-tab][aria-selected="true"]').getAttribute("data-admin-tab");
    if (backSelected !== second) addFailure(target.id, "history", `返回后标签异常：${backSelected}`);
    await tabPage.goto(`${BASE_URL}${target.path}#invalid`, { waitUntil: "networkidle" });
    const fallback = await tabPage.locator('[data-admin-tab][aria-selected="true"]').getAttribute("data-admin-tab");
    if (fallback !== target.tabs[0]) addFailure(target.id, "hash", `非法 hash 未回退：${fallback}`);
    await tabPage.close();
  }

  for (const deniedTarget of ["admin-voice.html", "admin-commerce.html"]) {
    const deniedPage = await browser.newPage({ viewport: { width: 1440, height: 1000 } });
    const deniedPageErrors = [];
    deniedPage.on("pageerror", (error) => deniedPageErrors.push(error.message));
    await deniedPage.route(/\/api\/(admin|debug|orders)\//, (route) => route.fulfill({
      status: 403,
      contentType: "application/json",
      body: JSON.stringify({ error: "forbidden" }),
    }));
    await deniedPage.goto(`${BASE_URL}/${deniedTarget}`, { waitUntil: "networkidle" });
    const deniedAudit = await deniedPage.evaluate(() => ({
      denied: document.body.dataset.internalAccess === "denied",
      notice: document.getElementById("internalAccessNotice")?.textContent || "",
      noticeInsideShell: Boolean(document.querySelector(".admin-shell > #internalAccessNotice")),
      editableControls: [...document.querySelectorAll("main input, main textarea, main select, main button")]
        .filter((control) => !control.disabled).length,
    }));
    if (deniedPageErrors.length) {
      addFailure("admin-internal-access", deniedTarget, `403 降级触发 pageerror：${deniedPageErrors.join(" | ")}`);
    }
    if (!deniedAudit.denied
      || deniedAudit.editableControls !== 0
      || !deniedAudit.notice.includes("仅可")
      || !deniedAudit.noticeInsideShell) {
      addFailure("admin-internal-access", deniedTarget, `403 降级未锁定编辑态：${JSON.stringify(deniedAudit)}`);
    }
    await deniedPage.close();
  }

  await browser.close();
  await writeReport();

  if (failures.length) {
    console.error(`UI 验收失败：${failures.length} 个问题。报告：${path.join(OUT_DIR, "ui-acceptance.md")}`);
    process.exit(1);
  }
  console.log(`UI 验收通过。报告：${path.join(OUT_DIR, "ui-acceptance.md")}`);
}

async function writeReport() {
  const lines = [
    "# UI 验收报告",
    "",
    `Base URL: ${BASE_URL}`,
    `生成时间: ${new Date().toLocaleString("zh-CN", { hour12: false })}`,
    "",
    `结果: ${failures.length ? "失败" : "通过"}`,
    "",
    "## 失败项",
    failures.length ? failures.map((f) => `- [${f.page}/${f.viewport}] ${f.message}`).join("\n") : "- 无",
    "",
    "## 警告项",
    warnings.length ? warnings.map((w) => `- [${w.page}/${w.viewport}] ${w.message}`).join("\n") : "- 无",
    "",
    "## 页面截图",
    ...results.map((r) => `- ${r.page} / ${r.viewport}: ${r.screenshot}`),
    "",
    "## 检查范围",
    "- 页面可访问与关键元素存在",
    "- 控制台 error / pageerror",
    "- 横向溢出",
    "- 后台菜单 active 状态",
    "- 顶部、菜单、卡片和按钮尺寸协调",
    "- 长文本溢出或异常断裂",
    "- 编辑器/技术状态残留文案",
  ];

  await fs.writeFile(path.join(OUT_DIR, "ui-acceptance.json"), JSON.stringify({ failures, warnings, results }, null, 2));
  await fs.writeFile(path.join(OUT_DIR, "ui-acceptance.md"), `${lines.join("\n")}\n`);
}

run().catch((error) => {
  console.error(error);
  process.exit(1);
});
