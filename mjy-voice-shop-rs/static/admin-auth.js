(() => {
  const markers = ["/myj-voice-shop", "/mjy-voice-shop"];
  const base = markers.find((marker) => location.pathname === marker || location.pathname.startsWith(`${marker}/`)) || "";
  const api = (path) => `${base}${path}`;
  const loginUrl = () => `${base}/admin-login.html?next=${encodeURIComponent(location.pathname + location.search)}`;

  window.adminFetch = async (path, options = {}) => {
    const response = await fetch(path.startsWith("/") ? api(path) : path, {
      credentials: "same-origin",
      ...options,
    });
    if (response.status === 401) location.replace(loginUrl());
    return response;
  };

  async function initialize() {
    const response = await fetch(api("/api/admin/auth/me"), { credentials: "same-origin" });
    if (response.status === 401) {
      location.replace(loginUrl());
      return;
    }
    if (!response.ok) return;
    const actions = document.querySelector(".header-actions");
    if (actions && !actions.querySelector("[data-admin-logout]")) {
      const button = document.createElement("button");
      button.type = "button";
      button.dataset.adminLogout = "";
      button.textContent = "退出登录";
      button.addEventListener("click", async () => {
        await fetch(api("/api/admin/auth/logout"), {
          method: "POST",
          credentials: "same-origin",
          headers: { "content-type": "application/json" },
          body: "{}",
        });
        location.replace(`${base}/admin-login.html`);
      });
      actions.append(button);
    }
  }

  window.adminSessionReady = initialize();
})();
