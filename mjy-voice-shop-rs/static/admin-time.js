(function installAdminTime(global) {
  const shanghaiFormatter = new Intl.DateTimeFormat("en-CA", {
    timeZone: "Asia/Shanghai",
    year: "numeric",
    month: "2-digit",
    day: "2-digit",
    hour: "2-digit",
    minute: "2-digit",
    second: "2-digit",
    hourCycle: "h23",
  });

  function formatShanghaiTime(value) {
    const text = String(value ?? "").trim();
    if (!text) return "";

    const localTime = text.match(/^(\d{4}-\d{2}-\d{2})[T ](\d{2}:\d{2}:\d{2})(?:\.\d+)?$/);
    if (localTime) return `${localTime[1]} ${localTime[2]}`;

    const zonedTime = text.match(/^\d{4}-\d{2}-\d{2}T\d{2}:\d{2}:\d{2}(?:\.\d+)?(?:Z|[+-]\d{2}:?\d{2})$/i);
    if (!zonedTime) return text;

    const date = new Date(text);
    if (Number.isNaN(date.getTime())) return text.replace("T", " ");

    const parts = Object.fromEntries(
      shanghaiFormatter.formatToParts(date)
        .filter((part) => part.type !== "literal")
        .map((part) => [part.type, part.value]),
    );
    return `${parts.year}-${parts.month}-${parts.day} ${parts.hour}:${parts.minute}:${parts.second}`;
  }

  function formatTriggerTime(value) {
    return `触发时间：${formatShanghaiTime(value) || "-"}`;
  }

  function formatOrderTriggerTime(order) {
    const value = order?.created_at
      || order?.creationTime
      || order?.data?.created_at
      || order?.data?.creationTime;
    return formatTriggerTime(value);
  }

  function formatJsonForDisplay(value, spacing = 2) {
    return JSON.stringify(value, (_key, item) => (
      typeof item === "string" ? formatShanghaiTime(item) : item
    ), spacing);
  }

  global.AdminTime = {
    formatShanghaiTime,
    formatTriggerTime,
    formatOrderTriggerTime,
    formatJsonForDisplay,
  };
})(globalThis);
