#!/usr/bin/env node
import assert from "node:assert/strict";

await import("../static/admin-time.js");

const {
  formatShanghaiTime,
  formatTriggerTime,
  formatOrderTriggerTime,
  formatJsonForDisplay,
} = globalThis.AdminTime;

assert.equal(typeof formatTriggerTime, "function", "后台应提供统一的触发时间文案格式");
assert.equal(
  formatTriggerTime("2026-07-15T07:41:50Z"),
  "触发时间：2026-07-15 15:41:50",
  "触发时间应转换为上海时区并精确到秒",
);
assert.equal(formatTriggerTime(""), "触发时间：-", "缺少触发时间时应明确显示占位符");
assert.equal(typeof formatOrderTriggerTime, "function", "订单时间应兼容本地和正式订单字段");
assert.equal(
  formatOrderTriggerTime({ created_at: "2026-07-15T07:41:50Z" }),
  "触发时间：2026-07-15 15:41:50",
  "本地订单应读取 created_at",
);
assert.equal(
  formatOrderTriggerTime({ creationTime: "2026-07-15 15:41:50" }),
  "触发时间：2026-07-15 15:41:50",
  "正式订单应读取 creationTime",
);
assert.equal(
  formatOrderTriggerTime({ data: { creationTime: "2026-07-15 15:41:50" } }),
  "触发时间：2026-07-15 15:41:50",
  "订单详情应兼容 data.creationTime",
);

assert.equal(
  formatShanghaiTime("2026-07-15T07:41:50.797727907+00:00"),
  "2026-07-15 15:41:50",
  "UTC 时间应转换为上海时区，并移除 T",
);
assert.equal(
  formatShanghaiTime("2026-07-15T15:41:50+08:00"),
  "2026-07-15 15:41:50",
  "东八区时间应保持本地时刻",
);
assert.equal(
  formatShanghaiTime("2026-07-15 15:41:50.123456"),
  "2026-07-15 15:41:50",
  "无时区的本地时间应按上海时间展示",
);
assert.equal(formatShanghaiTime("created"), "created", "非时间字符串不应被修改");

const displayedJson = formatJsonForDisplay({
  created_at: "2026-07-15T07:41:50Z",
  nested: { refundedAt: "2026-07-15T08:00:00+00:00" },
});
assert.match(displayedJson, /"created_at": "2026-07-15 15:41:50"/);
assert.match(displayedJson, /"refundedAt": "2026-07-15 16:00:00"/);
assert.doesNotMatch(displayedJson, /T\d{2}:/);

console.log("后台时间格式检查通过");
