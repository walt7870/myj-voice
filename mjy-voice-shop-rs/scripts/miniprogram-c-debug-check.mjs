#!/usr/bin/env node
import { mkdir, writeFile } from "node:fs/promises";
import path from "node:path";

const BASE_URL = process.env.UI_BASE_URL || "http://127.0.0.1:8787";
const OUT_DIR = process.env.MINI_C_CHECK_OUT || "ui-report";

const headers = {
  "__app": "mjy-miniapp",
  "__appver": "1.0.0",
  "__company": "CC",
  "__store": "999006940",
  "__storeno": "6634",
  "__src_channel": "2",
  "CompanyCode": "CC",
  "Authorization": "Bearer mock-token",
  "debug": "true",
};

const failures = [];
const results = [];

function assertCase(name, condition, detail = {}) {
  results.push({ name, ok: Boolean(condition), detail });
  if (!condition) failures.push({ name, detail });
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

async function jsonFetch(url, options = {}) {
  const res = await fetch(url, options);
  const body = await res.json();
  return { status: res.status, body };
}

async function ensureServer() {
  try {
    const res = await fetch(`${BASE_URL}/api/health`);
    if (!res.ok) throw new Error(`HTTP ${res.status}`);
  } catch (error) {
    console.error(`本地服务不可访问：${BASE_URL}`);
    console.error("先执行：scripts/start-dev.sh");
    console.error(error.message);
    process.exit(2);
  }
}

async function runApiChecks() {
  const meta = await jsonFetch(`${BASE_URL}/api/debug/miniprogram-c/interfaces`);
  assertCase("调试接口清单可读取", meta.status === 200, meta);
  const interfaces = meta.body.interfaces || [];
  assertCase(
    "暴露两个已确认读取接口和四个预置写接口 mock",
    interfaces.length === 6
      && interfaces.some((item) => item.id === "get-user-sale-orders")
      && interfaces.some((item) => item.id === "get-user-sale-order-detail")
      && interfaces.some((item) => item.id === "create-order")
      && interfaces.some((item) => item.id === "cancel-sale-order")
      && interfaces.some((item) => item.id === "pay-order")
      && interfaces.some((item) => item.id === "apply-refund"),
    interfaces,
  );
  assertCase(
    "写操作接口明确标记为预置 mock 待改造",
    interfaces.filter((item) => item.path_status === "待 Apifox 确认").length === 4,
    meta.body.missing_interfaces,
  );

  const list = await jsonFetch(`${BASE_URL}/mock/app-catering/api/app/saleorder/get-user-sale-orders?pageIndex=1&pageSize=2&status=102`, { headers });
  assertCase("订单列表 mock 服务可调用", list.status === 200 && list.body.code === 0, list.body);
  assertCase("订单列表返回分页和订单摘要", list.body.data?.pageIndex === 1 && list.body.data?.items?.length === 2, list.body.data);
  assertCase("订单列表 Header 完整时无缺失提示", list.body._debug?.missingHeaders?.length === 0, list.body._debug);

  const detail = await jsonFetch(`${BASE_URL}/mock/app-catering/api/app/saleorder/get-user-sale-order-detail?saleOrderId=mock-sale-order-002&srcChannel=2`, { headers });
  assertCase("订单详情 mock 服务可调用", detail.status === 200 && detail.body.code === 0, detail.body);
  assertCase("订单详情返回指定 saleOrderId", detail.body.data?.saleOrderId === "mock-sale-order-002", detail.body.data);
  assertCase("订单详情包含语音播报关键字段", Boolean(detail.body.data?.displayStatus && detail.body.data?.goodses?.length), detail.body.data);

  const create = await jsonFetch(`${BASE_URL}/mock/app-catering/api/app/saleorder/create-order?srcChannel=2`, {
    method: "POST",
    headers: { ...headers, "Content-Type": "application/json" },
    body: JSON.stringify({
      storeId: "999006940",
      storeNo: "6634",
      goodses: [{ goodsId: "cola-500", goodsName: "可口可乐", qty: 2, salePrice: 3.5 }],
    }),
  });
  assertCase("创建订单预置 mock 可调用", create.status === 200 && create.body.data?.displayStatus === "待支付", create.body);

  const cancel = await jsonFetch(`${BASE_URL}/mock/app-catering/api/app/saleorder/cancel-sale-order?srcChannel=2`, {
    method: "POST",
    headers: { ...headers, "Content-Type": "application/json" },
    body: JSON.stringify({ saleOrderId: "mock-sale-order-001", cancelReason: "测试取消" }),
  });
  assertCase("取消订单预置 mock 可调用", cancel.status === 200 && cancel.body.data?.displayStatus === "已取消", cancel.body);

  const pay = await jsonFetch(`${BASE_URL}/mock/app-catering/api/app/saleorder/pay-order?srcChannel=2`, {
    method: "POST",
    headers: { ...headers, "Content-Type": "application/json" },
    body: JSON.stringify({ saleOrderId: "mock-sale-order-001", payType: 1 }),
  });
  assertCase("发起支付预置 mock 可调用", pay.status === 200 && Boolean(pay.body.data?.payment?.prepayId), pay.body);

  const refund = await jsonFetch(`${BASE_URL}/mock/app-catering/api/app/saleorder/apply-refund?srcChannel=2`, {
    method: "POST",
    headers: { ...headers, "Content-Type": "application/json" },
    body: JSON.stringify({ saleOrderId: "mock-sale-order-003", refundAmt: 16.8 }),
  });
  assertCase("申请退款预置 mock 可调用", refund.status === 200 && refund.body.data?.refundStatus === 1, refund.body);

  const debugCall = await jsonFetch(`${BASE_URL}/api/debug/miniprogram-c/call`, {
    method: "POST",
    headers: { "Content-Type": "application/json" },
    body: JSON.stringify({
      interface_id: "get-user-sale-order-detail",
      query: { saleOrderId: "mock-sale-order-001", srcChannel: "2" },
      headers,
    }),
  });
  assertCase("调试封装接口可调用详情 mock", debugCall.status === 200 && debugCall.body.ok === true, debugCall.body);
}

async function runPageChecks() {
  const { chromium } = await ensurePlaywright();
  const browser = await chromium.launch({ headless: true });
  const page = await browser.newPage({ viewport: { width: 1440, height: 1000 } });
  const consoleErrors = [];
  page.on("console", (msg) => {
    if (msg.type() === "error") consoleErrors.push(msg.text());
  });
  page.on("pageerror", (error) => consoleErrors.push(error.message));
  const response = await page.goto(`${BASE_URL}/admin-integrations.html#miniprogram`, { waitUntil: "networkidle" });
  assertCase("小程序C端调试页可访问", Boolean(response?.ok()), { status: response?.status() });
  await page.click("#miniRunAll");
  await page.waitForFunction(() => document.getElementById("miniResultSummary")?.textContent?.includes("6/6 通过"));
  const pageState = await page.evaluate(() => ({
    activeMenu: document.querySelector(".admin-menu a.active")?.textContent?.trim(),
    cards: document.querySelectorAll(".mini-interface-card").length,
    missing: document.querySelectorAll(".mini-missing-card").length,
    status: document.getElementById("miniStatus")?.textContent,
    summary: document.getElementById("miniResultSummary")?.textContent,
    hasListPath: document.body.textContent.includes("/app-catering/api/app/saleorder/get-user-sale-orders"),
    hasDetailPath: document.body.textContent.includes("/app-catering/api/app/saleorder/get-user-sale-order-detail"),
  }));
  assertCase("调试页菜单和卡片渲染正确", pageState.activeMenu === "接入管理" && pageState.cards === 6, pageState);
  assertCase("调试页展示待补充接口", pageState.missing >= 1, pageState);
  assertCase("调试页全部调试通过", pageState.summary?.includes("6/6 通过"), pageState);
  assertCase("控制台无错误", consoleErrors.length === 0, { consoleErrors });
  await mkdir(path.join(OUT_DIR, "screenshots"), { recursive: true });
  await page.screenshot({ path: path.join(OUT_DIR, "screenshots", "admin-integrations-miniprogram-desktop.png"), fullPage: true });
  await browser.close();
}

async function writeReport() {
  await mkdir(OUT_DIR, { recursive: true });
  await writeFile(
    path.join(OUT_DIR, "miniprogram-c-debug.json"),
    JSON.stringify({ baseUrl: BASE_URL, results, failures }, null, 2),
  );
}

async function run() {
  await ensureServer();
  await runApiChecks();
  await runPageChecks();
  await writeReport();
  if (failures.length) {
    console.error(`小程序C端接口调试验收失败：${failures.length} 个问题。报告：${path.resolve(OUT_DIR, "miniprogram-c-debug.json")}`);
    process.exit(1);
  }
  console.log(`小程序C端接口调试验收通过。报告：${path.resolve(OUT_DIR, "miniprogram-c-debug.json")}`);
}

run().catch((error) => {
  console.error(error);
  process.exit(1);
});
