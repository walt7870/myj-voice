const authApi = (path, options) => window.adminFetch(path, options);
const list = document.getElementById("authorizationList");
const statusNode = document.getElementById("authorizationStatus");

async function jsonRequest(path, options) {
  const response = await authApi(path, options);
  const body = await response.json().catch(() => ({}));
  if (!response.ok) throw new Error(body.error || `HTTP ${response.status}`);
  return body;
}

function showSecret(secret) {
  const dialog = document.getElementById("secretDialog");
  document.getElementById("generatedSecret").textContent = secret;
  dialog.hidden = false;
}

async function runAction(action, pendingMessage) {
  statusNode.textContent = pendingMessage;
  try {
    await action();
  } catch (error) {
    statusNode.textContent = error instanceof Error ? error.message : "操作失败，请重试";
  }
}

async function loadAuthorizations() {
  const devices = await jsonRequest("/api/admin/device-authorizations");
  list.innerHTML = devices.map((device) => `<article class="authorization-row" data-device-id="${escapeHtml(device.device_id)}"><div><strong>${escapeHtml(device.name)}</strong><small>${escapeHtml(device.device_id)} · ${device.enabled ? "已启用" : "已停用"} · ${lastConversationLabel(device.last_conversation_at)}</small></div><input value="${escapeHtml(device.name)}" aria-label="设备名称" /><label><input type="checkbox" ${device.enabled ? "checked" : ""} />启用</label><button data-save>保存</button><button data-reset>重置密钥</button></article>`).join("");
  statusNode.textContent = `共 ${devices.length} 个授权`;
}

function lastConversationLabel(value) {
  if (!value) return "暂无设备对话";
  const date = new Date(value);
  return Number.isNaN(date.getTime()) ? "有设备对话" : `最近对话 ${date.toLocaleString("zh-CN", { hour12: false })}`;
}

function escapeHtml(value) {
  return String(value).replaceAll("&", "&amp;").replaceAll('"', "&quot;").replaceAll("<", "&lt;").replaceAll(">", "&gt;");
}

document.getElementById("createAuthorizationForm").addEventListener("submit", async (event) => {
  event.preventDefault();
  await runAction(async () => {
    const body = await jsonRequest("/api/admin/device-authorizations", { method: "POST", headers: { "content-type": "application/json" }, body: JSON.stringify({ device_id: document.getElementById("newDeviceId").value.trim(), name: document.getElementById("newDeviceName").value.trim() }) });
    showSecret(body.device_secret);
    event.target.reset();
    await loadAuthorizations();
  }, "正在创建授权…");
});

list.addEventListener("click", async (event) => {
  const row = event.target.closest(".authorization-row");
  if (!row) return;
  const saving = event.target.matches("[data-save]");
  const resetting = event.target.matches("[data-reset]");
  if (!saving && !resetting) return;
  const id = encodeURIComponent(row.dataset.deviceId);
  await runAction(async () => {
    if (saving) await jsonRequest(`/api/admin/device-authorizations/${id}`, { method: "PUT", headers: { "content-type": "application/json" }, body: JSON.stringify({ name: row.querySelector('input[aria-label="设备名称"]').value.trim(), enabled: row.querySelector('input[type="checkbox"]').checked }) });
    if (resetting && confirm("重置后旧密钥立即失效，确认继续？")) {
      const body = await jsonRequest(`/api/admin/device-authorizations/${id}/reset-secret`, { method: "POST", headers: { "content-type": "application/json" }, body: JSON.stringify({ confirm: true }) });
      showSecret(body.device_secret);
    }
    await loadAuthorizations();
  }, "正在保存…");
});

document.getElementById("copySecret").addEventListener("click", () => navigator.clipboard.writeText(document.getElementById("generatedSecret").textContent));
document.getElementById("closeSecret").addEventListener("click", () => { document.getElementById("generatedSecret").textContent = ""; document.getElementById("secretDialog").hidden = true; });
window.adminSessionReady.then(loadAuthorizations).catch(() => { statusNode.textContent = "授权加载失败"; });
