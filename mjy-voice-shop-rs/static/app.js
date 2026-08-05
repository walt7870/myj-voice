const state = {
  ws: null,
  diagWs: null,
  conversationId: null,
  conversationEnded: false,
  activeTraceId: null,
  draft: null,
  lastOrder: null,
  config: null,
  matches: [],
  ttsBuffers: new Map(),
  ttsStreams: new Map(),
  ttsTurnOrdering: new Map(),
  audioQueue: [],
  audioPlaying: false,
  currentAudio: null,
  currentAudioItem: null,
  currentPlaybackTurnId: null,
  interruptedTurnIds: new Set(),
  latencyStages: new Map(),
  recorder: {
    stream: null,
    context: null,
    processor: null,
    source: null,
    active: false,
    permissionGranted: false,
    pickupAllowed: true,
    sendLocked: false,
    speaking: false,
    streamingStarted: false,
    streamTrace: null,
    chunks: [],
    preBuffer: [],
    startedAt: 0,
    lastVoiceAt: 0,
    utteranceTimeoutId: null,
    asrResponseTimeoutId: null,
    inputSampleRate: 48000,
    targetSampleRate: 16000,
    threshold: 0.018,
    silenceMs: 320,
    minSpeechMs: 350,
    maxSpeechMs: 10000,
    preSpeechMs: 250,
    ttsNoInterrupt: true,
    ttsInterruptWord: "停一下",
    ttsAsrBlocked: false,
    ttsAsrResumeAt: 0,
    interruptSpeaking: false,
    interruptChunks: [],
    interruptStartedAt: 0,
    interruptLastVoiceAt: 0,
    interruptCheckInFlight: false,
  },
};

const $ = (id) => document.getElementById(id);
const STREAMING_TTS_PREBUFFER_BYTES = 4096;
const ASR_RESPONSE_TIMEOUT_MS = 8000;
const MESSAGE_HISTORY_LIMIT = 100;
const PUBLIC_BASE_PATH = detectPublicBasePath();

function detectPublicBasePath() {
  const markers = ["/myj-voice-shop", "/mjy-voice-shop"];
  return markers.find((marker) => location.pathname === marker || location.pathname.startsWith(`${marker}/`)) || "";
}

function apiUrl(path) {
  return `${PUBLIC_BASE_PATH}${path}`;
}

function wsUrl(path) {
  const proto = location.protocol === "https:" ? "wss" : "ws";
  return `${proto}://${location.host}${PUBLIC_BASE_PATH}${path}`;
}

const latencyStageLabels = {
  speech_sent: "开始推流",
  audio_received: "服务端接收",
  audio_input_done: "音频输入结束",
  asr_done: "转写完成",
  llm_first_token: "模型首字",
  llm_sentence_ready: "首段回复",
  tts_start: "语音合成开始",
  tts_first_chunk: "TTS 首包",
  tts_done: "语音合成完成",
  playback_start: "开始播报",
};

function connect() {
  const existing = state.ws;
  if (existing && [WebSocket.CONNECTING, WebSocket.OPEN].includes(existing.readyState)) return existing;
  const socket = new WebSocket(wsUrl("/api/chat/voice?in_format=pcm&in_rate=16000&out_format=mp3&out_rate=16000"));
  state.ws = socket;
  socket.onopen = () => {
    if (state.recorder.active) showMicAvailability();
    else setStatus("待体验");
  };
  socket.onclose = () => {
    handleVoiceSocketClose(socket);
  };
  socket.onmessage = (event) => {
    if (state.ws === socket) handleEvent(JSON.parse(event.data));
  };
  return socket;
}

function handleVoiceSocketClose(socket) {
  if (state.ws !== socket) return false;
  clearAsrResponseTimeout();
  state.recorder.sendLocked = false;
  discardRecordingWindow();
  if (state.recorder.active) showMicAvailability();
  else setStatus("待体验");
  return true;
}

function startDiagnosticsForTrace(traceId) {
  if (PUBLIC_BASE_PATH) return null;
  if (state.diagWs && [WebSocket.CONNECTING, WebSocket.OPEN].includes(state.diagWs.readyState)) return state.diagWs;
  const socket = new WebSocket(wsUrl("/api/diagnostics/latency"));
  state.diagWs = socket;
  socket.onmessage = (event) => handleDiagnosticEvent(JSON.parse(event.data));
  socket.onclose = () => {
    if (state.diagWs === socket) state.diagWs = null;
  };
  return socket;
}

function sendWs(payload) {
  const socket = connect();
  const body = JSON.stringify({ conversation_id: state.conversationId, ...payload });
  const requiresCapture = ["audio_stream_start", "audio_stream_chunk", "audio_segment"].includes(payload.type);
  if (requiresCapture && !canCaptureAudio(state)) return false;
  if (["text", "audio_stream_end", "audio_segment"].includes(payload.type)) {
    if (payload.type === "text") clearAsrResponseTimeout();
    state.recorder.sendLocked = true;
    showMicAvailability();
  }
  if (["audio_stream_end", "audio_segment"].includes(payload.type)) {
    scheduleAsrResponseTimeout();
  }
  if (socket.readyState === WebSocket.OPEN) {
    socket.send(body);
    return true;
  }
  socket.addEventListener("open", () => {
    if (!requiresCapture || canCaptureAudio(state)) socket.send(body);
  }, { once: true });
  return false;
}

function handleDiagnosticEvent(event) {
  if (!state.activeTraceId || event.trace_id !== state.activeTraceId) return;
  if (!event.stage) return;
  upsertLatencyStage(event.stage, event.elapsed_ms, event.detail || {});
}

function resetLatencyStages(traceId) {
  state.activeTraceId = traceId;
  state.latencyStages = new Map();
  $("latencyTraceId").textContent = traceId ? `本次 ${traceId.slice(-6)}` : "等待语音";
  renderLatencyStages();
}

function createTurnTrace() {
  return `${Date.now().toString(36)}-${Math.random().toString(36).slice(2, 8)}`;
}

function beginTurnTrace() {
  const traceId = createTurnTrace();
  const clientSentMs = Date.now();
  resetLatencyStages(traceId);
  upsertLatencyStage("speech_sent", 0, { client_ms: clientSentMs });
  startDiagnosticsForTrace(traceId);
  return { traceId, clientSentMs };
}

async function prepareTurnTrace() {
  return beginTurnTrace();
}

function upsertLatencyStage(stage, elapsedMs, detail = {}) {
  state.latencyStages.set(stage, {
    label: latencyStageLabels[stage] || stage,
    elapsedMs,
    detail,
  });
  renderLatencyStages();
}

function renderLatencyStages() {
  const preferred = [
    "speech_sent",
    "audio_received",
    "audio_input_done",
    "asr_done",
    "llm_first_token",
    "llm_sentence_ready",
    "tts_start",
    "tts_first_chunk",
    "playback_start",
    "tts_done",
  ];
  $("latencyStages").innerHTML = preferred
    .map((stage) => {
      const item = state.latencyStages.get(stage);
      const value = item?.elapsedMs == null ? "--" : `${Math.max(0, Math.round(item.elapsedMs))}ms`;
      const detail = formatLatencyDetail(stage, item?.detail || {});
      return `
        <div class="latency-stage ${item ? "active" : "idle"}">
          <span>${latencyStageLabels[stage] || stage}</span>
          <strong>${value}</strong>
          ${detail ? `<small>${detail}</small>` : ""}
        </div>
      `;
    })
    .join("");
}

function formatLatencyDetail(stage, detail) {
  if (stage === "audio_received" && detail.mode === "streaming") return "流式音频";
  if (stage === "audio_received" && detail.audio_duration_ms) return `音频 ${detail.audio_duration_ms}ms`;
  if (stage === "audio_input_done" && detail.audio_duration_ms) return `音频 ${detail.audio_duration_ms}ms`;
  if (stage === "asr_done" && detail.audio_duration_ms) return `音频 ${detail.audio_duration_ms}ms`;
  if (stage === "tts_start" && detail.mode === "streaming_text") return "流式文本";
  if (stage === "tts_start" && detail.chars) return `${detail.chars} 字`;
  if (stage === "tts_first_chunk" && detail.bytes) return `${detail.bytes} bytes`;
  if (stage === "tts_done" && detail.chunks) return `${detail.chunks} 包 · ${detail.bytes || 0} bytes`;
  if (stage === "llm_sentence_ready" && detail.mode === "streaming_text") return `${detail.chars || 0} 字 · 流式合成`;
  if (stage === "llm_sentence_ready" && detail.chars) return `${detail.chars} 字`;
  return "";
}

function setStatus(text) {
  $("status").textContent = text;
}

function setMicButtonLabel(text) {
  const label = $("micBtn").querySelector(".mic-label");
  if (label) label.textContent = text;
}

function canCaptureAudio(appState) {
  const recorder = appState.recorder;
  return Boolean(
    !appState.conversationEnded
      && recorder.permissionGranted
      && recorder.active
      && appState.ws?.readyState === 1
      && recorder.pickupAllowed
      && !recorder.sendLocked
      && !recorder.interruptCheckInFlight
      && !recorder.ttsAsrBlocked
      && recorder.ttsAsrResumeAt === 0
      && !appState.audioPlaying
      && appState.audioQueue.length === 0
      && appState.ttsBuffers.size === 0
      && appState.ttsStreams.size === 0
      && ![...(appState.ttsTurnOrdering?.values() || [])].some((ordering) => ordering.ready.size > 0),
  );
}

function deriveVoiceUiState(appState) {
  if (appState.conversationEnded) {
    return { available: false, label: "☎ 开始持续监听", status: "对话已结束" };
  }
  if (canCaptureAudio(appState)) {
    return { available: true, label: "持续监听中，点击停止", status: "持续监听中，说完一句会自动发送" };
  }
  const recorder = appState.recorder;
  if (!recorder.permissionGranted) return { available: false, label: "等待麦克风权限", status: "等待麦克风权限" };
  if (!recorder.active) return { available: false, label: "☎ 开始持续监听", status: "监听已停止" };
  if (appState.ws?.readyState !== 1) return { available: false, label: "连接中，暂停收音", status: "语音连接未就绪" };
  if (!recorder.pickupAllowed) return { available: false, label: "暂停收音", status: "当前不可拾音" };
  if (recorder.sendLocked || recorder.interruptCheckInFlight) {
    return { available: false, label: "发送中，暂停收音", status: "发送中，暂停收音" };
  }
  return { available: false, label: "播报中，暂停收音", status: "播报中，暂停收音" };
}

function showMicAvailability(readyStatus = "持续监听中，说完一句会自动发送") {
  const ui = deriveVoiceUiState(state);
  document.body.classList.toggle("recording", ui.available);
  document.body.classList.toggle("asr-blocked", state.recorder.active && !ui.available);
  if (!ui.available) document.body.classList.remove("speaking");
  setMicButtonLabel(ui.label);
  setStatus(ui.available ? readyStatus : ui.status);
  return ui.available;
}

function handleEvent(event) {
  if (!isEventForCurrentConversation(event)) return;
  if (event.conversation_id && !state.conversationId) state.conversationId = event.conversation_id;
  if (event.event_type === "tts_interrupt_detected") {
    state.recorder.interruptCheckInFlight = false;
    stopTtsPlayback(event.payload?.text || state.recorder.ttsInterruptWord);
    return;
  }
  if (event.event_type === "tts_interrupt_ignored") {
    state.recorder.interruptCheckInFlight = false;
    if (state.recorder.active && state.audioPlaying) setStatus("播报中，暂停收音");
    return;
  }
  if (event.event_type === "asr_ignored") {
    clearAsrResponseTimeout();
    state.recorder.sendLocked = false;
    if (state.recorder.active) {
      const canListen = showMicAvailability();
      $("latency").textContent = canListen ? "继续监听" : "播放中";
    }
    return;
  }
  if (event.event_type === "asr_partial" && !isAsrBlockedByTts()) {
    setStatus(event.payload.text || "正在识别");
  }
  if (event.event_type === "asr_final") {
    clearAsrResponseTimeout();
    addMessage("user", event.payload.text);
    $("latency").textContent = "模型回复中";
  }
  if (event.event_type === "llm_delta") appendAssistant(event.payload.content);
  if (event.event_type === "reply_sentence") {
    addAnalysis(`播报：${event.payload.text}`);
    $("latency").textContent = "语音合成中";
  }
  if (event.event_type === "tts_audio_chunk") {
    if (event.turn_id && state.interruptedTurnIds.has(event.turn_id)) return;
    if (!validateTtsAudioMetadata(event.payload)) return;
    state.currentPlaybackTurnId = event.turn_id || state.currentPlaybackTurnId;
    try {
      enqueueTtsAudio(event.payload, event.turn_id);
    } catch {
      failTtsPlayback("TTS 音频数据无效，已停止本轮播报");
      return;
    }
    $("latency").textContent = "播放中";
  }
  if (event.event_type === "intent_analysis") addAnalysis(`意图：${event.payload.intent} · 置信度 ${event.payload.confidence}`);
  if (event.event_type === "product_matches") renderMatches(event.payload.items || []);
  if (event.event_type === "order_draft") renderDraft(event.payload);
  if (event.event_type === "order_submit_started") addAnalysis("正在下发订单");
  if (event.event_type === "order_refund_started") addAnalysis("正在退单");
  if (event.event_type === "order_created") {
    state.lastOrder = event.payload;
    addAnalysis(`订单已下发：${event.payload.order_id || event.payload.saleOrderId || "已提交"}`);
    renderShoppingPanel();
  }
  if (event.event_type === "order_refunded") {
    state.lastOrder = event.payload;
    addAnalysis(`订单已退：${event.payload.order_id || event.payload.saleOrderId || "已处理"}`);
    renderShoppingPanel();
  }
  if (event.event_type === "conversation_ended") {
    endConversationFromServer();
  }
  if (event.event_type === "latency_metrics") $("latency").textContent = `${event.payload.total_ms}ms`;
  if (event.event_type === "voice_done") {
    clearAsrResponseTimeout();
    state.recorder.sendLocked = false;
    if (state.recorder.active || state.conversationEnded) {
      const canListen = showMicAvailability();
      $("latency").textContent = state.conversationEnded ? "已结束" : (canListen ? "继续监听" : "播放中");
    } else {
      setStatus("本轮完成");
      $("latency").textContent = "本轮完成";
    }
  }
  if (event.event_type === "error") {
    clearAsrResponseTimeout();
    state.recorder.sendLocked = false;
    if (state.recorder.active) showMicAvailability();
    const message = formatEventError(event.payload);
    if (isEmptyAsrMessage(event.payload)) {
      if (state.recorder.active) {
        const canListen = showMicAvailability();
        $("latency").textContent = canListen ? "继续监听" : "播放中";
      }
      return;
    }
    const terminalTtsError = isTerminalTtsError(event.payload, state);
    if (terminalTtsError) clearTtsPlaybackState({ resumeImmediately: true });
    if (terminalTtsError) {
      addAnalysis(message);
    } else {
      addMessage("event", message);
    }
    setStatus(message);
  }
}

function validateTtsAudioMetadata(payload) {
  if (payload?.format === "mp3" && payload?.sample_rate === 16000 && payload?.channels === 1) return true;
  const actualFormat = payload?.format ?? "missing";
  const actualRate = payload?.sample_rate ?? "missing";
  const actualChannels = payload?.channels ?? "missing";
  const message = `拒绝播放不匹配的 TTS 音频：期望 mp3/16000/mono，收到 ${actualFormat}/${actualRate}/${actualChannels}ch`;
  clearTtsPlaybackState({ resumeImmediately: true });
  addAnalysis(message);
  setStatus(message);
  return false;
}

function hasTtsPlaybackWork(voiceState) {
  const recorder = voiceState.recorder;
  return Boolean(
    voiceState.ttsStreams.size > 0
      || voiceState.ttsBuffers.size > 0
      || [...(voiceState.ttsTurnOrdering?.values() || [])].some((ordering) => ordering.ready.size > 0)
      || voiceState.audioQueue.length > 0
      || voiceState.currentAudio
      || voiceState.currentAudioItem
      || voiceState.audioPlaying
      || recorder.ttsAsrBlocked
      || recorder.ttsAsrResumeAt !== 0,
  );
}

function isTerminalTtsError(payload, voiceState) {
  if (payload?.code === "tts_failed") return true;
  if (payload?.code !== "upstream_audio_profile_rejected") return false;
  const direction = String(payload.direction || "").trim().toLowerCase();
  if (direction) return ["tts", "output", "downlink"].includes(direction);
  return hasTtsPlaybackWork(voiceState);
}

function isEventForCurrentConversation(event) {
  if (!event?.conversation_id || !state.conversationId) return true;
  return event.conversation_id === state.conversationId;
}

function isEmptyAsrMessage(payload) {
  return payload?.code === "asr_failed" && (
    payload.message === "IAT returned empty text" ||
    payload.message === "没有识别到有效语音，请再说一遍"
  );
}

function formatEventError(payload) {
  if (!payload) return "服务暂不可用";
  if (payload.code === "asr_failed" && payload.message === "IAT returned empty text") {
    return "没有识别到有效语音，请再说一遍";
  }
  return payload.message || "服务暂不可用";
}

function addMessage(kind, text) {
  const messages = $("messages");
  const last = messages.lastElementChild;
  if (kind === "event" && last?.classList.contains("event") && last.textContent === text) return;
  const stickToBottom = shouldStickMessagesToBottom(messages);
  const div = document.createElement("div");
  div.className = `msg ${kind}`;
  div.textContent = text;
  messages.appendChild(div);
  trimMessages();
  if (stickToBottom) messages.scrollTop = messages.scrollHeight;
}

function appendAssistant(text) {
  const messages = $("messages");
  const stickToBottom = shouldStickMessagesToBottom(messages);
  let last = messages.lastElementChild;
  if (!last || !last.classList.contains("assistant-live")) {
    last = document.createElement("div");
    last.className = "msg assistant-live";
    messages.appendChild(last);
  }
  last.textContent += text;
  trimMessages();
  if (stickToBottom) messages.scrollTop = messages.scrollHeight;
}

function shouldStickMessagesToBottom(messages) {
  return messages.scrollHeight - messages.clientHeight - messages.scrollTop <= 24;
}

function trimMessages() {
  const messages = $("messages");
  while (messages.children.length > MESSAGE_HISTORY_LIMIT) {
    messages.removeChild(messages.firstElementChild);
  }
}

function addAnalysis(text) {
  const span = document.createElement("span");
  span.className = "pill";
  span.textContent = text;
  $("analysisLog").appendChild(span);
  while ($("analysisLog").children.length > 8) {
    $("analysisLog").removeChild($("analysisLog").firstElementChild);
  }
}

function renderMatches(items) {
  state.matches = items;
  if (!items.length) {
    addAnalysis("未命中商品");
    renderShoppingPanel();
    return;
  }
  addAnalysis(`命中 ${items.length} 个商品`);
  renderShoppingPanel();
}

function renderDraft(draft) {
  state.draft = draft;
  renderShoppingPanel();
}

function renderShoppingPanel() {
  const matches = state.matches || [];
  const draft = state.draft;
  const order = state.lastOrder;
  const items = draft?.items || matches;
  if (!items.length) {
    $("draft").innerHTML = `
      <div class="shopping-panel empty">
        <h2>购物识别</h2>
        <p>暂未识别到商品，可以继续对话。</p>
      </div>
    `;
    return;
  }
  const orderId = currentOrderId();
  const orderRefunded = isOrderRefunded(order);
  const statusText = order
    ? orderRefunded
      ? "已退"
      : "已下发"
    : draft?.status === "submitting"
      ? "下发中"
      : draft
        ? "待语音确认"
        : "已识别";
  $("draft").innerHTML = `
    <div class="shopping-panel">
      <div class="shopping-head">
        <h2>${order ? (orderRefunded ? "已退订单" : "已下发订单") : draft ? "下单意向" : "商品意向"}</h2>
        <span>${statusText} · ${items.length} 项</span>
      </div>
      <div class="shopping-items">
        ${items
          .map(
            (item) => `
              <div class="shopping-item">
                <div>
                  <strong>${item.name}</strong>
                  <small>${item.spec || "默认规格"}</small>
                </div>
                <b>x ${item.quantity}</b>
              </div>
            `,
          )
          .join("")}
      </div>
      ${draft && !order ? `
        <div class="voice-order-hint">
          <strong>等待语音确认</strong>
          <span>系统会播报本轮识别到的商品，用户说“确认下单”后自动调用下单服务。</span>
        </div>
      ` : ""}
      ${order ? `
        <div class="order-result">
          <span>${orderRefunded ? "订单已退" : "订单已下发"}</span>
          <strong>${orderId || "已提交"}</strong>
          <small>${order.status || order.displayStatus || order.data?.displayStatus || "created"}</small>
        </div>
      ` : ""}
    </div>
  `;
}

function currentOrderId() {
  return state.lastOrder?.saleOrderId || state.lastOrder?.order_id || state.lastOrder?.orderId || "";
}

function isOrderRefunded(order) {
  if (!order) return false;
  const status = String(
    order.status || order.displayStatus || order.data?.status || order.data?.displayStatus || "",
  ).toLowerCase();
  return status.includes("refund") || status.includes("cancel") || status.includes("已退") || status.includes("已取消");
}

function ttsSequenceKey(turnId, seq) {
  return JSON.stringify([turnId || "", String(seq ?? 0)]);
}

function enqueueTtsAudio(payload, turnId = null) {
  const bytes = base64ToBytes(payload.audio);
  if (!bytes.length) {
    if (payload.is_last) finishTtsAudioInput(payload, turnId);
    return;
  }
  beginTtsAsrBlock();
  if (canStreamMp3()) {
    enqueueStreamingTtsAudio(payload, bytes, turnId);
    return;
  }
  enqueueBufferedTtsAudio(payload, bytes, turnId);
}

function finishTtsAudioInput(payload, turnId) {
  const key = ttsSequenceKey(turnId, payload.seq);
  const player = state.ttsStreams.get(key);
  if (player) {
    player.ending = true;
    maybeQueueStreamingTtsPlayer(key, player);
    pumpTtsStream(player);
    return;
  }
  const chunks = state.ttsBuffers.get(key);
  if (chunks?.length) {
    state.ttsBuffers.delete(key);
    const blob = new Blob(chunks, { type: "audio/mpeg" });
    const url = URL.createObjectURL(blob);
    queueTtsAudioItem(turnId, payload.seq, { url, revoke: () => URL.revokeObjectURL(url) });
    return;
  }
  queueTtsAudioItem(turnId, payload.seq, null);
}

function canStreamMp3() {
  return Boolean(window.MediaSource?.isTypeSupported?.("audio/mpeg"));
}

function enqueueStreamingTtsAudio(payload, bytes, turnId) {
  const key = ttsSequenceKey(turnId, payload.seq);
  let player = state.ttsStreams.get(key);
  if (!player) {
    const mediaSource = new MediaSource();
    const url = URL.createObjectURL(mediaSource);
    player = {
      key,
      turnId,
      seq: Number(payload.seq ?? 0),
      mediaSource,
      sourceBuffer: null,
      pending: [],
      ending: false,
      queued: false,
      bufferedBytes: 0,
      url,
      released: false,
    };
    state.ttsStreams.set(key, player);
    mediaSource.addEventListener("sourceopen", () => {
      if (player.released) return;
      try {
        player.sourceBuffer = mediaSource.addSourceBuffer("audio/mpeg");
        player.sourceBuffer.mode = "sequence";
        player.sourceBuffer.addEventListener("updateend", () => pumpTtsStream(player));
        pumpTtsStream(player);
      } catch {
        failTtsPlayback("TTS 播放器初始化失败，已停止本轮播报");
      }
    }, { once: true });
  }
  player.pending.push(bytes);
  player.bufferedBytes += bytes.length;
  if (payload.is_last) player.ending = true;
  maybeQueueStreamingTtsPlayer(key, player);
  pumpTtsStream(player);
}

function maybeQueueStreamingTtsPlayer(key, player) {
  if (player.queued) return;
  if (!player.ending && player.bufferedBytes < STREAMING_TTS_PREBUFFER_BYTES) return;
  player.queued = true;
  queueTtsAudioItem(player.turnId, player.seq, {
    url: player.url,
    revoke: () => {
      releaseTtsStreamPlayer(key, player);
    },
  });
}

function pumpTtsStream(player) {
  if (player.released) return;
  if (!player.sourceBuffer || player.sourceBuffer.updating) return;
  if (player.pending.length) {
    try {
      player.sourceBuffer.appendBuffer(player.pending.shift());
    } catch {
      failTtsPlayback("TTS 音频追加失败，已停止本轮播报");
    }
    return;
  }
  if (player.ending && player.mediaSource.readyState === "open") {
    try {
      player.mediaSource.endOfStream();
    } catch {
      // The audio element may already have torn down the stream.
    }
  }
}

function enqueueBufferedTtsAudio(payload, bytes, turnId) {
  const key = ttsSequenceKey(turnId, payload.seq);
  const chunks = state.ttsBuffers.get(key) || [];
  chunks.push(bytes);
  if (!payload.is_last) {
    state.ttsBuffers.set(key, chunks);
    return;
  }
  state.ttsBuffers.delete(key);
  const blob = new Blob(chunks, { type: "audio/mpeg" });
  const url = URL.createObjectURL(blob);
  queueTtsAudioItem(turnId, payload.seq, { url, revoke: () => URL.revokeObjectURL(url) });
}

function queueTtsAudioItem(turnId, seqValue, item) {
  if (!turnId) {
    state.audioQueue.push(item);
    drainAudioQueue();
    return;
  }
  const seq = Number(seqValue ?? 0);
  const ordering = state.ttsTurnOrdering.get(turnId) || { nextSeq: 0, ready: new Map() };
  ordering.ready.set(seq, item);
  state.ttsTurnOrdering.set(turnId, ordering);
  while (ordering.ready.has(ordering.nextSeq)) {
    const readyItem = ordering.ready.get(ordering.nextSeq);
    if (readyItem) state.audioQueue.push(readyItem);
    ordering.ready.delete(ordering.nextSeq);
    ordering.nextSeq += 1;
  }
  drainAudioQueue();
}

function releaseAudioQueueItem(item) {
  if (!item || item.released) return false;
  item.released = true;
  item.revoke?.();
  return true;
}

function releaseTtsStreamPlayer(key, player) {
  if (!player || player.released) return false;
  player.released = true;
  player.pending = [];
  URL.revokeObjectURL(player.url);
  state.ttsStreams.delete(key);
  return true;
}

function cleanupAudioPlayback(audio, item, { drainQueue = true, scheduleResume = true } = {}) {
  if (state.currentAudio !== audio || state.currentAudioItem !== item) return false;
  audio.onended = null;
  audio.onerror = null;
  try {
    audio.pause();
    audio.removeAttribute("src");
    audio.load();
  } catch {
    // The browser may reject teardown after a media initialization failure.
  }
  releaseAudioQueueItem(item);
  state.currentAudio = null;
  state.currentAudioItem = null;
  state.audioPlaying = false;
  if (drainQueue) drainAudioQueue();
  if (scheduleResume) {
    scheduleTtsAsrResume();
    showMicAvailability();
  }
  return true;
}

function clearTtsPlaybackState({ resumeImmediately = false } = {}) {
  if (state.currentAudio && state.currentAudioItem) {
    cleanupAudioPlayback(state.currentAudio, state.currentAudioItem, {
      drainQueue: false,
      scheduleResume: false,
    });
  }
  state.audioQueue.forEach(releaseAudioQueueItem);
  state.ttsTurnOrdering.forEach((ordering) => ordering.ready.forEach(releaseAudioQueueItem));
  [...state.ttsStreams.entries()].forEach(([seq, player]) => releaseTtsStreamPlayer(seq, player));
  state.ttsBuffers.clear();
  state.ttsStreams.clear();
  state.ttsTurnOrdering.clear();
  state.audioQueue = [];
  state.audioPlaying = false;
  state.currentAudio = null;
  state.currentAudioItem = null;
  if (resumeImmediately) {
    const recorder = state.recorder;
    recorder.ttsAsrBlocked = false;
    recorder.ttsAsrResumeAt = 0;
    recorder.interruptCheckInFlight = false;
    recorder.sendLocked = false;
    document.body.classList.remove("asr-blocked");
    if (recorder.active) {
      showMicAvailability();
      $("latency").textContent = "继续监听";
    }
    return;
  }
  scheduleTtsAsrResume();
  showMicAvailability();
}

function failTtsPlayback(message) {
  clearTtsPlaybackState({ resumeImmediately: true });
  addAnalysis(message);
  setStatus(message);
}

function drainAudioQueue() {
  if (state.audioPlaying || state.audioQueue.length === 0) return;
  state.audioPlaying = true;
  beginTtsAsrBlock();
  const item = state.audioQueue.shift();
  let audio;
  try {
    audio = new Audio(item.url);
  } catch {
    releaseAudioQueueItem(item);
    failTtsPlayback("TTS 播放器创建失败，已停止本轮播报");
    return;
  }
  state.currentAudio = audio;
  state.currentAudioItem = item;
  audio.onended = () => cleanupAudioPlayback(audio, item);
  audio.onerror = () => cleanupAudioPlayback(audio, item);
  let playPromise;
  try {
    playPromise = audio.play();
  } catch {
    cleanupAudioPlayback(audio, item);
    return;
  }
  Promise.resolve(playPromise)
    .then(() => {
      if (!state.latencyStages.has("playback_start")) {
        const sent = state.latencyStages.get("speech_sent")?.detail?.client_ms;
        upsertLatencyStage("playback_start", sent ? Date.now() - sent : null, {});
      }
    })
    .catch(() => cleanupAudioPlayback(audio, item));
}

function stopTtsPlayback(text) {
  if (state.currentPlaybackTurnId) state.interruptedTurnIds.add(state.currentPlaybackTurnId);
  clearTtsPlaybackState();
  addAnalysis(`已打断播报：${text}`);
  showMicAvailability("持续监听中，说完一句会自动发送");
  $("latency").textContent = "已打断";
}

function beginTtsAsrBlock() {
  const recorder = state.recorder;
  if (recorder.streamingStarted && state.ws?.readyState === 1) {
    sendWs({
      type: "audio_stream_end",
      trace_id: recorder.streamTrace?.traceId,
      client_sent_ms: recorder.streamTrace?.clientSentMs,
    });
  }
  recorder.ttsAsrBlocked = true;
  recorder.ttsAsrResumeAt = Number.POSITIVE_INFINITY;
  discardRecordingWindow();
  showMicAvailability();
}

function scheduleTtsAsrResume() {
  const recorder = state.recorder;
  if (state.audioPlaying || state.audioQueue.length > 0) return;
  recorder.ttsAsrBlocked = false;
  recorder.ttsAsrResumeAt = 0;
  document.body.classList.remove("asr-blocked");
  if (recorder.active) {
    showMicAvailability();
    $("latency").textContent = "继续监听";
  }
}

function isAsrBlockedByTts() {
  const recorder = state.recorder;
  return recorder.ttsAsrBlocked || performance.now() < recorder.ttsAsrResumeAt;
}

function discardRecordingWindow() {
  const recorder = state.recorder;
  clearUtteranceTimeout();
  recorder.speaking = false;
  recorder.streamingStarted = false;
  recorder.streamTrace = null;
  recorder.chunks = [];
  recorder.preBuffer = [];
  recorder.interruptSpeaking = false;
  recorder.interruptChunks = [];
  document.body.classList.remove("speaking");
}

function resetExperience(label) {
  $("latency").textContent = label;
  $("analysisLog").innerHTML = "";
  $("messages").innerHTML = "";
  state.matches = [];
  state.draft = null;
  state.lastOrder = null;
  renderShoppingPanel();
}

function endConversationFromServer() {
  addAnalysis("已结束本轮对话");
  state.conversationEnded = true;
  if (state.recorder.active) stopContinuousRecording();
  setStatus("对话已结束");
  $("latency").textContent = "已结束";
  setMicButtonLabel("☎ 开始持续监听");
}

async function startNewConversation() {
  if (state.recorder.active) stopContinuousRecording();
  const res = await fetch(apiUrl("/api/conversations/new"), { method: "POST" });
  const body = await res.json();
  state.conversationId = body.conversation_id;
  state.conversationEnded = false;
  resetExperience("等待输入");
  const roundText = $("roundText");
  if (roundText) roundText.textContent = `本轮 ${state.conversationId.slice(0, 8)}`;
  setStatus("新一轮已开启");
}

$("sendText").addEventListener("click", async () => {
  const text = $("textInput").value.trim();
  if (!text) return;
  state.conversationEnded = false;
  $("textInput").value = "";
  $("latency").textContent = "处理中";
  setStatus("文字已发送");
  const { traceId, clientSentMs } = await prepareTurnTrace();
  sendWs({ type: "text", text, trace_id: traceId, client_sent_ms: clientSentMs });
});

const newRoundButton = $("newRoundBtn");
if (newRoundButton) newRoundButton.addEventListener("click", startNewConversation);

["silenceMsControl", "minSpeechMsControl", "thresholdControl", "preSpeechMsControl"].forEach((id) => {
  const control = $(id);
  if (!control) return;
  control.addEventListener("input", applyRecorderTuning);
  control.addEventListener("change", applyRecorderTuning);
});

["ttsProviderControl", "voiceNameControl", "voiceCodeControl", "modelControl", "ttsNoInterruptControl", "ttsInterruptWordControl"].forEach((id) => {
  const control = $(id);
  if (!control) return;
  control.addEventListener("input", () => setQuickConfigStatus("有未应用修改"));
  control.addEventListener("change", () => {
    if (id === "ttsProviderControl") syncVoiceCodeForProvider();
    if (id === "voiceNameControl" || id === "voiceCodeControl") syncSelectedVoice(id);
    if (id === "ttsNoInterruptControl") {
      state.recorder.ttsNoInterrupt = true;
      control.checked = true;
    }
    if (id === "ttsInterruptWordControl") {
      state.recorder.ttsInterruptWord = control.value.trim();
    }
    setQuickConfigStatus("有未应用修改");
  });
});

$("quickConfigSave").addEventListener("click", saveQuickConfig);
$("promptConfigOpen").addEventListener("click", openPromptDialog);
$("promptConfigClose").addEventListener("click", closePromptDialog);
$("promptConfigSave").addEventListener("click", savePromptConfig);

$("micBtn").addEventListener("click", async () => {
  if (state.recorder.active) {
    stopContinuousRecording();
    return;
  }
  await startContinuousRecording();
});

$("keyboardBtn").addEventListener("click", () => $("textInput").focus());

async function startContinuousRecording() {
  state.conversationEnded = false;
  connect();
  if (!state.conversationId) await startNewConversation();
  $("latency").textContent = "监听中";
  const recorder = state.recorder;
  try {
    recorder.stream = await navigator.mediaDevices.getUserMedia({
      audio: {
        channelCount: 1,
        sampleRate: recorder.targetSampleRate,
        echoCancellation: true,
        noiseSuppression: true,
        autoGainControl: true,
      },
    });
  } catch {
    recorder.active = false;
    recorder.permissionGranted = false;
    recorder.stream = null;
    document.body.classList.remove("recording", "speaking", "asr-blocked");
    setMicButtonLabel("☎ 开始持续监听");
    setStatus("无法访问麦克风，请检查浏览器权限");
    $("latency").textContent = "麦克风权限未授予";
    return false;
  }
  recorder.permissionGranted = true;
  recorder.stream.getTracks().forEach((track) => track.addEventListener("ended", handleMicrophoneTrackEnded));
  recorder.context = new AudioContext({ sampleRate: recorder.targetSampleRate });
  recorder.inputSampleRate = recorder.context.sampleRate;
  recorder.source = recorder.context.createMediaStreamSource(recorder.stream);
  recorder.processor = recorder.context.createScriptProcessor(4096, 1, 1);
  recorder.processor.onaudioprocess = onAudioProcess;
  recorder.source.connect(recorder.processor);
  recorder.processor.connect(recorder.context.destination);
  recorder.active = true;
  showMicAvailability();
  return true;
}

function loadRecorderTuning() {
  const recorder = state.recorder;
  $("silenceMsControl").value = recorder.silenceMs;
  $("minSpeechMsControl").value = recorder.minSpeechMs;
  $("thresholdControl").value = recorder.threshold;
  $("preSpeechMsControl").value = recorder.preSpeechMs;
}

function readNumber(id, fallback, min, max) {
  const value = Number($(id)?.value);
  if (!Number.isFinite(value)) return fallback;
  return Math.min(max, Math.max(min, value));
}

function applyRecorderTuning() {
  const recorder = state.recorder;
  recorder.silenceMs = readNumber("silenceMsControl", recorder.silenceMs, 250, 400);
  recorder.minSpeechMs = readNumber("minSpeechMsControl", recorder.minSpeechMs, 300, 500);
  recorder.threshold = readNumber("thresholdControl", recorder.threshold, 0.006, 0.05);
  recorder.preSpeechMs = readNumber("preSpeechMsControl", recorder.preSpeechMs, 200, 300);
  setQuickConfigStatus("断句参数已生效");
}

function handleMicrophoneTrackEnded() {
  if (!state.recorder.active) return;
  stopContinuousRecording({ stopTracks: false, finalize: false, status: "麦克风已断开" });
  $("latency").textContent = "麦克风不可用";
}

function stopContinuousRecording({ stopTracks = true, finalize = true, status = "监听已停止" } = {}) {
  const recorder = state.recorder;
  if (finalize) finalizeUtterance(true);
  recorder.sendLocked = false;
  recorder.active = false;
  recorder.permissionGranted = false;
  recorder.speaking = false;
  recorder.streamingStarted = false;
  recorder.streamTrace = null;
  clearUtteranceTimeout();
  clearAsrResponseTimeout();
  recorder.interruptSpeaking = false;
  recorder.interruptChunks = [];
  recorder.interruptCheckInFlight = false;
  recorder.chunks = [];
  recorder.preBuffer = [];
  if (recorder.processor) recorder.processor.disconnect();
  if (recorder.source) recorder.source.disconnect();
  if (recorder.context) recorder.context.close();
  if (stopTracks && recorder.stream) recorder.stream.getTracks().forEach((track) => track.stop());
  recorder.processor = null;
  recorder.source = null;
  recorder.context = null;
  recorder.stream = null;
  setMicButtonLabel("☎ 开始持续监听");
  document.body.classList.remove("recording", "speaking", "asr-blocked");
  setStatus(status);
}

function onAudioProcess(event) {
  const recorder = state.recorder;
  if (!canCaptureAudio(state)) {
    discardRecordingWindow();
    return;
  }
  const input = event.inputBuffer.getChannelData(0);
  const frame = downsample(input, recorder.inputSampleRate, recorder.targetSampleRate);
  const rms = calcRms(frame);
  const now = performance.now();
  const isVoice = rms >= recorder.threshold;

  if (!recorder.speaking) {
    pushPreBuffer(frame);
    if (!isVoice) return;
    recorder.speaking = true;
    recorder.streamingStarted = false;
    recorder.streamTrace = null;
    recorder.startedAt = now;
    recorder.lastVoiceAt = now;
    scheduleUtteranceTimeout();
    recorder.chunks = recorder.preBuffer.slice();
    recorder.chunks.push(frame);
    recorder.preBuffer = [];
    document.body.classList.add("speaking");
    setStatus("正在收音");
    return;
  }

  recorder.chunks.push(frame);
  const wasStreaming = recorder.streamingStarted;
  maybeStartStreamingUtterance();
  if (wasStreaming && recorder.streamingStarted) sendAudioStreamChunk(frame);
  if (isVoice) recorder.lastVoiceAt = now;
  const silentLongEnough = now - recorder.lastVoiceAt >= recorder.silenceMs;
  const tooLong = now - recorder.startedAt >= recorder.maxSpeechMs;
  if (silentLongEnough || tooLong) finalizeUtterance(false);
}

function pushPreBuffer(frame) {
  const recorder = state.recorder;
  recorder.preBuffer.push(frame);
  const maxSamples = Math.floor((recorder.targetSampleRate * recorder.preSpeechMs) / 1000);
  let total = recorder.preBuffer.reduce((sum, item) => sum + item.length, 0);
  while (total > maxSamples && recorder.preBuffer.length > 1) {
    total -= recorder.preBuffer.shift().length;
  }
}

function maybeStartStreamingUtterance() {
  const recorder = state.recorder;
  if (!canCaptureAudio(state)) return;
  if (!recorder.speaking || recorder.streamingStarted) return;
  const duration = performance.now() - recorder.startedAt;
  if (duration < recorder.minSpeechMs) return;
  recorder.streamTrace = beginTurnTrace();
  recorder.streamingStarted = true;
  sendWs({
    type: "audio_stream_start",
    trace_id: recorder.streamTrace.traceId,
    client_sent_ms: recorder.streamTrace.clientSentMs,
  });
  recorder.chunks.forEach(sendAudioStreamChunk);
}

function sendAudioStreamChunk(frame) {
  const recorder = state.recorder;
  if (!canCaptureAudio(state)) return;
  if (!recorder.streamingStarted) return;
  const pcm = floatToPcm16(frame);
  if (pcm.byteLength === 0) return;
  sendWs({
    type: "audio_stream_chunk",
    audio: bytesToBase64(pcm),
    trace_id: recorder.streamTrace?.traceId,
    client_sent_ms: recorder.streamTrace?.clientSentMs,
  });
}

async function finalizeUtterance(force) {
  const recorder = state.recorder;
  if (!recorder.speaking || recorder.chunks.length === 0) return;
  clearUtteranceTimeout();
  const duration = performance.now() - recorder.startedAt;
  const frames = recorder.chunks;
  recorder.speaking = false;
  recorder.chunks = [];
  recorder.preBuffer = [];
  document.body.classList.remove("speaking");

  if (!canCaptureAudio(state)) return;

  if (!force && duration < recorder.minSpeechMs) {
    setStatus("持续监听中");
    return;
  }
  if (recorder.streamingStarted) {
    sendWs({
      type: "audio_stream_end",
      trace_id: recorder.streamTrace?.traceId,
      client_sent_ms: recorder.streamTrace?.clientSentMs,
    });
    recorder.streamingStarted = false;
    recorder.streamTrace = null;
    setStatus("一句话结束，等待识别结果");
    $("latency").textContent = "ASR 收尾中";
    return;
  }
  const samples = concatFloat32(frames);
  const pcm = floatToPcm16(samples);
  if (pcm.byteLength < 3200) {
    setStatus("持续监听中");
    return;
  }
  setStatus("一句话结束，正在发送识别");
  $("latency").textContent = "ASR 处理中";
  const { traceId, clientSentMs } = await prepareTurnTrace();
  sendWs({
    type: "audio_segment",
    audio: bytesToBase64(pcm),
    trace_id: traceId,
    client_sent_ms: clientSentMs,
  });
}

function clearUtteranceTimeout() {
  const recorder = state.recorder;
  if (recorder.utteranceTimeoutId == null) return;
  window.clearTimeout(recorder.utteranceTimeoutId);
  recorder.utteranceTimeoutId = null;
}

function scheduleUtteranceTimeout() {
  const recorder = state.recorder;
  clearUtteranceTimeout();
  const timeoutId = window.setTimeout(() => {
    if (recorder.utteranceTimeoutId !== timeoutId) return;
    recorder.utteranceTimeoutId = null;
    finalizeUtterance(false);
  }, recorder.maxSpeechMs);
  recorder.utteranceTimeoutId = timeoutId;
}

function clearAsrResponseTimeout() {
  const recorder = state.recorder;
  if (recorder.asrResponseTimeoutId == null) return;
  window.clearTimeout(recorder.asrResponseTimeoutId);
  recorder.asrResponseTimeoutId = null;
}

function scheduleAsrResponseTimeout() {
  const recorder = state.recorder;
  clearAsrResponseTimeout();
  const timeoutId = window.setTimeout(() => {
    if (recorder.asrResponseTimeoutId !== timeoutId) return;
    recorder.asrResponseTimeoutId = null;
    recorder.sendLocked = false;
    discardRecordingWindow();
    if (recorder.active) showMicAvailability("识别超时，请再说一遍");
    else setStatus("识别超时，请再说一遍");
    $("latency").textContent = "ASR 超时";
  }, ASR_RESPONSE_TIMEOUT_MS);
  recorder.asrResponseTimeoutId = timeoutId;
}

function calcRms(frame) {
  let sum = 0;
  for (let i = 0; i < frame.length; i += 1) sum += frame[i] * frame[i];
  return Math.sqrt(sum / frame.length);
}

function downsample(input, inputRate, outputRate) {
  if (inputRate === outputRate) return new Float32Array(input);
  const ratio = inputRate / outputRate;
  const length = Math.floor(input.length / ratio);
  const output = new Float32Array(length);
  for (let i = 0; i < length; i += 1) {
    output[i] = input[Math.floor(i * ratio)] || 0;
  }
  return output;
}

function concatFloat32(chunks) {
  const length = chunks.reduce((sum, chunk) => sum + chunk.length, 0);
  const result = new Float32Array(length);
  let offset = 0;
  chunks.forEach((chunk) => {
    result.set(chunk, offset);
    offset += chunk.length;
  });
  return result;
}

function floatToPcm16(samples) {
  const bytes = new Uint8Array(samples.length * 2);
  const view = new DataView(bytes.buffer);
  for (let i = 0; i < samples.length; i += 1) {
    const sample = Math.max(-1, Math.min(1, samples[i]));
    view.setInt16(i * 2, sample < 0 ? sample * 0x8000 : sample * 0x7fff, true);
  }
  return bytes;
}

function bytesToBase64(bytes) {
  let binary = "";
  const step = 0x8000;
  for (let i = 0; i < bytes.length; i += step) {
    binary += String.fromCharCode(...bytes.subarray(i, i + step));
  }
  return btoa(binary);
}

function base64ToBytes(value) {
  const binary = atob(value);
  const bytes = new Uint8Array(binary.length);
  for (let i = 0; i < binary.length; i += 1) bytes[i] = binary.charCodeAt(i);
  return bytes;
}

async function loadConfig() {
  const response = await fetch(apiUrl("/api/public/config"));
  if (!response.ok) throw new Error(`配置加载失败：HTTP ${response.status}`);
  const config = await response.json();
  state.config = config;
  updatePromptControls(config);
  state.recorder.ttsNoInterrupt = true;
  state.recorder.ttsInterruptWord = config.tts_interrupt_word || "停一下";
  $("ttsNoInterruptControl").checked = true;
  $("ttsInterruptWordControl").value = state.recorder.ttsInterruptWord;
  $("ttsProviderControl").value = config.tts_provider || "super_smart";
  renderVoiceControls(config);
  $("modelControl").innerHTML = (config.available_models || [])
    .map((model) => `<option value="${model}" ${model === config.llm_model ? "selected" : ""}>${model}</option>`)
    .join("");
  setQuickConfigStatus("参数已加载");
  setStatus("待体验");
}

function updatePromptControls(config) {
  $("rolePrompt").textContent = config.role_prompt || "";
  $("rolePromptControl").value = config.role_prompt || "";
  $("analysisPromptControl").value = config.analysis_prompt || "";
  $("promptConfigStatus").textContent = "配置已加载";
}

function getActiveVoiceCode(config) {
  return (config.tts_provider === "standard"
    ? config.tts_standard_voice
    : config.tts_voice) || (config.tts_provider === "standard" ? "x4_lingxiaolu_em_v2" : "x6_lingxiaoxuan_pro");
}

function syncVoiceCodeForProvider() {
  if (!state.config) return;
  renderVoiceControls({ ...state.config, tts_provider: $("ttsProviderControl").value });
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

function renderVoiceControls(config) {
  const provider = config.tts_provider || "super_smart";
  const voices = provider === "standard"
    ? [{ name: "在线语音合成", code: config.tts_standard_voice || "x4_lingxiaolu_em_v2" }]
    : superSmartVoices(config);
  const selectedCode = voices.some((voice) => voice.code === getActiveVoiceCode(config))
    ? getActiveVoiceCode(config)
    : voices[0].code;
  $("voiceNameControl").innerHTML = voices
    .map((voice) => `<option value="${voice.code}" ${voice.code === selectedCode ? "selected" : ""}>${voice.name}</option>`)
    .join("");
  $("voiceCodeControl").innerHTML = voices
    .map((voice) => `<option value="${voice.code}" ${voice.code === selectedCode ? "selected" : ""}>${voice.code}</option>`)
    .join("");
}

function syncSelectedVoice(sourceId) {
  const value = $(sourceId).value;
  $("voiceNameControl").value = value;
  $("voiceCodeControl").value = value;
}

function setQuickConfigStatus(text) {
  $("quickConfigStatus").textContent = text;
}

async function saveQuickConfig() {
  try {
    const current = state.config;
    const payload = buildConfigPayload(current);
    setQuickConfigStatus("应用中");
    const saved = await saveConfigPayload(payload);
    state.config = saved;
    $("ttsProviderControl").value = saved.tts_provider || "super_smart";
    state.recorder.ttsNoInterrupt = true;
    state.recorder.ttsInterruptWord = saved.tts_interrupt_word || "停一下";
    $("ttsNoInterruptControl").checked = true;
    $("ttsInterruptWordControl").value = state.recorder.ttsInterruptWord;
    renderVoiceControls(saved);
    $("modelControl").value = saved.llm_model;
    updatePromptControls(saved);
    setQuickConfigStatus("已应用");
  } catch (error) {
    setQuickConfigStatus(error instanceof Error ? error.message : "应用失败");
  }
}

function buildConfigPayload(current, overrides = {}) {
  const provider = $("ttsProviderControl").value;
  const voiceCode = $("voiceCodeControl").value;
  const voice = superSmartVoices(current).find((item) => item.code === voiceCode) || superSmartVoices(current)[0];
  return {
    app_id: current.app_id,
    api_key: "",
    api_secret: "",
    iat_endpoint: current.iat_endpoint,
    tts_provider: provider,
    tts_endpoint: current.tts_endpoint,
    tts_standard_endpoint: current.tts_standard_endpoint,
    tts_standard_voice: provider === "standard" ? voiceCode : current.tts_standard_voice,
    tts_voice_name: voice.name,
    tts_voice: voice.code,
    tts_no_interrupt: true,
    tts_interrupt_word: $("ttsInterruptWordControl").value.trim() || "停一下",
    llm_endpoint: current.llm_endpoint,
    llm_model: $("modelControl").value || current.llm_model,
    temperature: current.temperature,
    max_tokens: current.max_tokens,
    role_prompt: $("rolePromptControl").value || current.role_prompt,
    analysis_prompt: $("analysisPromptControl").value || current.analysis_prompt,
    order_mcp_url: current.order_mcp_url || "http://127.0.0.1:8765/mcp",
    order_mcp_enabled: current.order_mcp_enabled || false,
    order_mcp_token: "",
    order_mcp_tools: current.order_mcp_tools || {},
    mock_providers: current.mock_providers,
    ...overrides,
  };
}

async function saveConfigPayload(payload) {
  const response = await fetch(apiUrl("/api/admin/config"), {
    method: "PUT",
    headers: { "Content-Type": "application/json" },
    body: JSON.stringify(payload),
  });
  if (response.status === 401) throw new Error("请先登录管理后台");
  if (!response.ok) throw new Error(`保存失败：HTTP ${response.status}`);
  return response.json();
}

function openPromptDialog() {
  if (state.config) updatePromptControls(state.config);
  $("promptDialog").showModal();
}

function closePromptDialog() {
  $("promptDialog").close();
}

async function savePromptConfig() {
  try {
    const current = state.config;
    $("promptConfigStatus").textContent = "保存中";
    const saved = await saveConfigPayload(buildConfigPayload(current));
    state.config = saved;
    updatePromptControls(saved);
    setQuickConfigStatus("已应用");
    $("promptConfigStatus").textContent = "已保存";
    closePromptDialog();
  } catch (error) {
    $("promptConfigStatus").textContent = error instanceof Error ? error.message : "保存失败";
  }
}

function installVoiceTestHooks() {
  if (!new URLSearchParams(location.search).has("voiceTest")) return;

  const makeBytes = (length) => {
    let binary = "";
    for (let i = 0; i < length; i += 1) binary += String.fromCharCode(65 + (i % 26));
    return btoa(binary);
  };

  const makeFrame = (length, value) => {
    const frame = new Float32Array(length);
    frame.fill(value);
    return frame;
  };

  const makeAudioEvent = (frame) => ({
    inputBuffer: { getChannelData: () => frame },
  });

  const resetPlaybackState = () => {
    if (state.currentAudio) {
      state.currentAudio.onended = null;
      state.currentAudio.onerror = null;
      state.currentAudio.pause?.();
      state.currentAudio.removeAttribute?.("src");
      state.currentAudio.load?.();
    }
    if (state.currentAudioItem) state.currentAudioItem.revoke?.();
    state.audioQueue.forEach((item) => item.revoke?.());
    state.ttsStreams.forEach((player) => URL.revokeObjectURL(player.url));
    state.ttsBuffers.clear();
    state.ttsStreams.clear();
    state.ttsTurnOrdering.clear();
    state.audioQueue = [];
    state.audioPlaying = false;
    state.currentAudio = null;
    state.currentAudioItem = null;
    state.currentPlaybackTurnId = null;
    state.interruptedTurnIds.clear();
    state.recorder.ttsAsrBlocked = false;
    state.recorder.ttsAsrResumeAt = 0;
    document.body.classList.remove("asr-blocked");
  };

  const withFakeAudio = (run) => {
    const originalAudio = window.Audio;
    let audioStarts = 0;
    window.Audio = function FakeAudio() {
      audioStarts += 1;
      return {
        onended: null,
        onerror: null,
        play: () => new Promise(() => {}),
        pause: () => {},
        removeAttribute: () => {},
        load: () => {},
      };
    };
    try {
      return run(() => audioStarts);
    } finally {
      window.Audio = originalAudio;
      resetPlaybackState();
    }
  };

  const auditTerminalTtsHandlerCleanup = (failureEvent) => {
    const originalRevoke = URL.revokeObjectURL;
    const originalWs = state.ws;
    const recorder = state.recorder;
    const originalRecorder = {
      active: recorder.active,
      permissionGranted: recorder.permissionGranted,
      pickupAllowed: recorder.pickupAllowed,
      sendLocked: recorder.sendLocked,
      interruptCheckInFlight: recorder.interruptCheckInFlight,
      ttsAsrBlocked: recorder.ttsAsrBlocked,
      ttsAsrResumeAt: recorder.ttsAsrResumeAt,
    };
    let revokeCount = 0;
    try {
      resetPlaybackState();
      state.ws = { readyState: 1 };
      Object.assign(recorder, {
        active: true,
        permissionGranted: true,
        pickupAllowed: true,
        sendLocked: false,
        interruptCheckInFlight: false,
        ttsAsrBlocked: false,
        ttsAsrResumeAt: 0,
      });
      URL.revokeObjectURL = () => { revokeCount += 1; };
      handleEvent({
        event_type: "tts_audio_chunk",
        turn_id: "terminal-failure-turn",
        payload: {
          seq: 969,
          format: "mp3",
          sample_rate: 16000,
          channels: 1,
          audio: makeBytes(256),
          is_last: false,
        },
      });
      const prebufferCreated = state.ttsStreams.size === 1 || state.ttsBuffers.size === 1;
      handleEvent(failureEvent);
      handleEvent(failureEvent);
      handleEvent({ event_type: "voice_done", payload: {} });
      return {
        prebufferCreated,
        revokeCount,
        streams: state.ttsStreams.size,
        buffers: state.ttsBuffers.size,
        queueLength: state.audioQueue.length,
        currentCleared: state.currentAudio === null && state.currentAudioItem === null && !state.audioPlaying,
        locksCleared: !recorder.ttsAsrBlocked
          && recorder.ttsAsrResumeAt === 0
          && !recorder.interruptCheckInFlight,
        canCaptureAfterVoiceDone: canCaptureAudio(state),
      };
    } finally {
      URL.revokeObjectURL = originalRevoke;
      resetPlaybackState();
      state.ws = originalWs;
      Object.assign(recorder, originalRecorder);
    }
  };

  const auditUpstreamRejectionPreservation = ({ direction, withPrebuffer }) => {
    const originalRevoke = URL.revokeObjectURL;
    const originalWs = state.ws;
    const recorder = state.recorder;
    const originalRecorder = {
      active: recorder.active,
      permissionGranted: recorder.permissionGranted,
      pickupAllowed: recorder.pickupAllowed,
      sendLocked: recorder.sendLocked,
      interruptCheckInFlight: recorder.interruptCheckInFlight,
      ttsAsrBlocked: recorder.ttsAsrBlocked,
      ttsAsrResumeAt: recorder.ttsAsrResumeAt,
    };
    let revokeCount = 0;
    try {
      resetPlaybackState();
      state.ws = { readyState: 1 };
      Object.assign(recorder, {
        active: true,
        permissionGranted: true,
        pickupAllowed: true,
        sendLocked: false,
        interruptCheckInFlight: false,
        ttsAsrBlocked: false,
        ttsAsrResumeAt: 0,
      });
      URL.revokeObjectURL = () => { revokeCount += 1; };
      if (withPrebuffer) {
        handleEvent({
          event_type: "tts_audio_chunk",
          turn_id: "preserved-upstream-turn",
          payload: {
            seq: 974,
            format: "mp3",
            sample_rate: 16000,
            channels: 1,
            audio: makeBytes(256),
            is_last: false,
          },
        });
      } else {
        recorder.interruptCheckInFlight = true;
      }
      const prebufferCreated = state.ttsStreams.size === 1 || state.ttsBuffers.size === 1;
      handleEvent({
        event_type: "error",
        payload: {
          code: "upstream_audio_profile_rejected",
          message: "voice-test upstream profile rejected",
          ...(direction ? { direction } : {}),
        },
      });
      return {
        prebufferCreated,
        revokeCount,
        streams: state.ttsStreams.size,
        interruptLockPreserved: recorder.interruptCheckInFlight,
        ttsLockPreserved: recorder.ttsAsrBlocked && recorder.ttsAsrResumeAt !== 0,
      };
    } finally {
      URL.revokeObjectURL = originalRevoke;
      resetPlaybackState();
      state.ws = originalWs;
      Object.assign(recorder, originalRecorder);
    }
  };

  window.__voiceTest = {
    configSnapshot() {
      return {
        ttsNoInterrupt: state.recorder.ttsNoInterrupt,
        ttsInterruptWord: state.recorder.ttsInterruptWord,
        streamingPrebufferBytes: STREAMING_TTS_PREBUFFER_BYTES,
      };
    },

    auditConnectingSocketReuse() {
      const OriginalWebSocket = window.WebSocket;
      const originalWs = state.ws;
      const originalSendLocked = state.recorder.sendLocked;
      const sockets = [];
      let decoySendCount = 0;
      class FakeWebSocket {
        static CONNECTING = 0;
        static OPEN = 1;

        constructor() {
          this.readyState = FakeWebSocket.CONNECTING;
          this.sent = [];
          this.listeners = { open: [] };
          sockets.push(this);
        }

        addEventListener(type, listener) {
          this.listeners[type] ||= [];
          this.listeners[type].push(listener);
        }

        send(body) {
          this.sent.push(body);
        }

        open() {
          this.readyState = FakeWebSocket.OPEN;
          this.onopen?.();
          this.listeners.open.splice(0).forEach((listener) => listener());
        }
      }
      try {
        window.WebSocket = FakeWebSocket;
        state.ws = null;
        sendWs({ type: "text", text: "first" });
        sendWs({ type: "text", text: "second" });
        const createdSockets = sockets.length;
        state.ws = { readyState: FakeWebSocket.OPEN, send: () => { decoySendCount += 1; } };
        sockets.forEach((socket) => socket.open());
        return {
          createdSockets,
          sentOnConnectingSocket: sockets.reduce((total, socket) => total + socket.sent.length, 0),
          decoySendCount,
        };
      } finally {
        window.WebSocket = OriginalWebSocket;
        state.ws = originalWs;
        state.recorder.sendLocked = originalSendLocked;
      }
    },

    auditDiagnosticSocketReuse() {
      const OriginalWebSocket = window.WebSocket;
      const originalDiagWs = state.diagWs;
      const originalTraceId = state.activeTraceId;
      const sockets = [];
      class FakeDiagnosticWebSocket {
        static CONNECTING = 0;
        static OPEN = 1;

        constructor() {
          this.readyState = FakeDiagnosticWebSocket.CONNECTING;
          this.closeCount = 0;
          sockets.push(this);
        }

        close() {
          this.closeCount += 1;
          this.readyState = 3;
        }
      }
      try {
        window.WebSocket = FakeDiagnosticWebSocket;
        state.diagWs = null;
        state.activeTraceId = "voice-test-trace-a";
        startDiagnosticsForTrace(state.activeTraceId);
        const first = state.diagWs;
        first.readyState = FakeDiagnosticWebSocket.OPEN;
        state.activeTraceId = "voice-test-trace-b";
        startDiagnosticsForTrace(state.activeTraceId);
        return {
          createdSockets: sockets.length,
          firstSocketStayedOpen: first.closeCount === 0 && state.diagWs === first,
          activeTraceId: state.activeTraceId,
        };
      } finally {
        state.diagWs?.close?.();
        window.WebSocket = OriginalWebSocket;
        state.diagWs = originalDiagWs;
        state.activeTraceId = originalTraceId;
      }
    },

    auditTurnScopedTtsSequence() {
      return withFakeAudio((audioStarts) => {
        handleEvent({
          event_type: "tts_audio_chunk",
          turn_id: "voice-test-turn-a",
          payload: {
            seq: 0,
            format: "mp3",
            sample_rate: 16000,
            channels: 1,
            audio: makeBytes(STREAMING_TTS_PREBUFFER_BYTES + 128),
            is_last: true,
          },
        });
        const [oldKey, oldPlayer] = state.ttsStreams.entries().next().value || [];
        const activeAudio = state.currentAudio;
        let oldAppendCount = 0;
        if (oldPlayer) {
          oldPlayer.pending = [];
          oldPlayer.ending = true;
          oldPlayer.sourceBuffer = {
            updating: false,
            appendBuffer: () => { oldAppendCount += 1; },
          };
        }
        handleEvent({
          event_type: "tts_audio_chunk",
          turn_id: "voice-test-turn-b",
          payload: {
            seq: 0,
            format: "mp3",
            sample_rate: 16000,
            channels: 1,
            audio: makeBytes(256),
            is_last: false,
          },
        });
        return {
          oldKey,
          keys: [...state.ttsStreams.keys()],
          streamCount: state.ttsStreams.size,
          oldAppendCount,
          oldAudioStillPlaying: Boolean(activeAudio) && state.currentAudio === activeAudio,
          audioStarts: audioStarts(),
        };
      });
    },

    auditInterleavedTtsOrdering() {
      const originalAudio = window.Audio;
      const audios = [];
      try {
        resetPlaybackState();
        window.Audio = function OrderedAudio(url) {
          const audio = {
            url, onended: null, onerror: null,
            play: () => new Promise(() => {}), pause: () => {},
            removeAttribute: () => {}, load: () => {},
          };
          audios.push(audio);
          return audio;
        };
        handleEvent({ event_type: "tts_audio_chunk", turn_id: "interleaved-turn", payload: {
          seq: 1, format: "mp3", sample_rate: 16000, channels: 1,
          audio: makeBytes(STREAMING_TTS_PREBUFFER_BYTES + 128), is_last: true,
        } });
        const seq1Url = [...state.ttsStreams.values()].find((player) => player.seq === 1)?.url;
        const startsAfterSeq1 = audios.length;
        handleEvent({ event_type: "tts_audio_chunk", turn_id: "interleaved-turn", payload: {
          seq: 0, format: "mp3", sample_rate: 16000, channels: 1,
          audio: makeBytes(STREAMING_TTS_PREBUFFER_BYTES + 128), is_last: true,
        } });
        const seq0Url = [...state.ttsStreams.values()].find((player) => player.seq === 0)?.url;
        const firstUrl = audios[0]?.url;
        audios[0]?.onended?.();
        return {
          startsAfterSeq1,
          firstWasSeq0: firstUrl === seq0Url,
          secondWasSeq1: audios[1]?.url === seq1Url,
          audioStarts: audios.length,
        };
      } finally {
        window.Audio = originalAudio;
        resetPlaybackState();
      }
    },

    auditEmptySequenceOrderingMarker() {
      return withFakeAudio((audioStarts) => {
        handleEvent({ event_type: "tts_audio_chunk", turn_id: "empty-marker-turn", payload: {
          seq: 1, format: "mp3", sample_rate: 16000, channels: 1,
          audio: makeBytes(STREAMING_TTS_PREBUFFER_BYTES + 128), is_last: true,
        } });
        const startsBeforeMarker = audioStarts();
        handleEvent({ event_type: "tts_audio_chunk", turn_id: "empty-marker-turn", payload: {
          seq: 0, format: "mp3", sample_rate: 16000, channels: 1, audio: "", is_last: true,
        } });
        return { startsBeforeMarker, startsAfterMarker: audioStarts() };
      });
    },

    auditStreamingPrebuffer() {
      return withFakeAudio((audioStarts) => {
        enqueueTtsAudio({ seq: 910, audio: makeBytes(256), is_last: false });
        const firstTinyChunkAudioStarts = audioStarts();
        enqueueTtsAudio({ seq: 910, audio: makeBytes(STREAMING_TTS_PREBUFFER_BYTES), is_last: false });
        const afterThresholdAudioStarts = audioStarts();
        enqueueTtsAudio({ seq: 910, audio: makeBytes(1024), is_last: false });
        return {
          firstTinyChunkAudioStarts,
          afterThresholdAudioStarts,
          afterExtraChunkAudioStarts: audioStarts(),
        };
      });
    },

    auditEmptyAsrSuppression() {
      const messages = $("messages");
      const original = messages.innerHTML;
      messages.innerHTML = "";
      handleEvent({
        event_type: "error",
        payload: {
          code: "asr_failed",
          message: "没有识别到有效语音，请再说一遍",
        },
      });
      const result = {
        messageDelta: messages.children.length,
        leakedText: messages.textContent.includes("没有识别到有效语音"),
      };
      messages.innerHTML = original;
      return result;
    },

    auditPlaybackInterrupt() {
      return withFakeAudio((audioStarts) => {
        handleEvent({
          event_type: "tts_audio_chunk",
          turn_id: "voice-test-turn",
          payload: { seq: 920, format: "mp3", sample_rate: 16000, channels: 1, audio: makeBytes(STREAMING_TTS_PREBUFFER_BYTES + 512), is_last: false },
        });
        const audioStartsBeforeInterrupt = audioStarts();
        handleEvent({
          event_type: "tts_interrupt_detected",
          payload: { text: state.recorder.ttsInterruptWord },
        });
        const audioStartsAfterInterrupt = audioStarts();
        handleEvent({
          event_type: "tts_audio_chunk",
          turn_id: "voice-test-turn",
          payload: { seq: 920, format: "mp3", sample_rate: 16000, channels: 1, audio: makeBytes(STREAMING_TTS_PREBUFFER_BYTES + 512), is_last: false },
        });
        return {
          audioStartsBeforeInterrupt,
          audioStartsAfterInterrupt,
          audioStartsAfterOldChunk: audioStarts(),
          interruptedTurnTracked: state.interruptedTurnIds.has("voice-test-turn"),
        };
      });
    },

    auditRecordingGateDuringPlayback() {
      const sentTypes = [];
      const originalSendWs = sendWs;
      sendWs = (payload) => sentTypes.push(payload.type);
      const recorder = state.recorder;
      const original = {
        active: recorder.active,
        permissionGranted: recorder.permissionGranted,
        pickupAllowed: recorder.pickupAllowed,
        sendLocked: recorder.sendLocked,
        inputSampleRate: recorder.inputSampleRate,
        targetSampleRate: recorder.targetSampleRate,
        threshold: recorder.threshold,
        ttsNoInterrupt: recorder.ttsNoInterrupt,
        ttsAsrBlocked: recorder.ttsAsrBlocked,
        ttsAsrResumeAt: recorder.ttsAsrResumeAt,
        interruptCheckInFlight: recorder.interruptCheckInFlight,
      };
      const originalStatus = $("status").textContent;
      const originalMicLabel = $("micBtn").querySelector(".mic-label")?.textContent || "";
      const originallyBlocked = document.body.classList.contains("asr-blocked");
      const originalWs = state.ws;
      const originalAudioPlaying = state.audioPlaying;
      try {
        recorder.active = true;
        recorder.permissionGranted = true;
        recorder.pickupAllowed = true;
        recorder.sendLocked = false;
        recorder.inputSampleRate = 16000;
        recorder.targetSampleRate = 16000;
        recorder.threshold = 0.01;
        recorder.ttsNoInterrupt = true;
        recorder.ttsAsrBlocked = false;
        recorder.ttsAsrResumeAt = 0;
        recorder.interruptCheckInFlight = false;
        state.ws = { readyState: 1 };
        state.audioPlaying = true;
        showMicAvailability();
        onAudioProcess(makeAudioEvent(makeFrame(4096, 0.04)));
        recorder.interruptLastVoiceAt = performance.now() - Math.max(300, recorder.silenceMs + 10);
        onAudioProcess(makeAudioEvent(makeFrame(4096, 0)));
        onAudioProcess(makeAudioEvent(makeFrame(4096, 0)));
        handleEvent({ event_type: "voice_done", payload: {} });
        return {
          normalAudioStarts: sentTypes.filter((type) => type === "audio_stream_start" || type === "audio_segment").length,
          interruptSegments: sentTypes.filter((type) => type === "interrupt_audio_segment").length,
          status: $("status").textContent,
          micLabel: $("micBtn").querySelector(".mic-label")?.textContent || "",
          blockedClass: document.body.classList.contains("asr-blocked"),
          sentTypes,
        };
      } finally {
        sendWs = originalSendWs;
        state.ws = originalWs;
        state.audioPlaying = originalAudioPlaying;
        Object.assign(recorder, original);
        recorder.interruptSpeaking = false;
        recorder.interruptChunks = [];
        setStatus(originalStatus);
        setMicButtonLabel(originalMicLabel);
        document.body.classList.toggle("asr-blocked", originallyBlocked);
      }
    },

    auditUtteranceHardTimeout() {
      const recorder = state.recorder;
      const originalSendWs = sendWs;
      const originalSetTimeout = window.setTimeout;
      const originalClearTimeout = window.clearTimeout;
      const originalWs = state.ws;
      const originalRecorder = { ...recorder };
      const sentTypes = [];
      let scheduled = null;
      try {
        sendWs = (payload) => {
          sentTypes.push(payload.type);
          return true;
        };
        window.setTimeout = (callback, delay) => {
          scheduled = { callback, delay };
          return 4242;
        };
        window.clearTimeout = () => {};
        state.ws = { readyState: 1 };
        Object.assign(recorder, {
          active: true,
          permissionGranted: true,
          pickupAllowed: true,
          sendLocked: false,
          speaking: false,
          streamingStarted: false,
          inputSampleRate: 16000,
          targetSampleRate: 16000,
          threshold: 0.01,
          ttsAsrBlocked: false,
          ttsAsrResumeAt: 0,
          interruptCheckInFlight: false,
          chunks: [],
          preBuffer: [],
        });
        onAudioProcess(makeAudioEvent(makeFrame(4096, 0.04)));
        recorder.startedAt = performance.now() - recorder.minSpeechMs - 10;
        onAudioProcess(makeAudioEvent(makeFrame(4096, 0.04)));
        scheduled?.callback();
        return {
          scheduledDelay: scheduled?.delay ?? null,
          maxSpeechMs: recorder.maxSpeechMs,
          sentTypes,
          speakingAfterTimeout: recorder.speaking,
        };
      } finally {
        sendWs = originalSendWs;
        window.setTimeout = originalSetTimeout;
        window.clearTimeout = originalClearTimeout;
        state.ws = originalWs;
        Object.assign(recorder, originalRecorder);
      }
    },

    auditAsrResponseTimeout() {
      const recorder = state.recorder;
      const originalSetTimeout = window.setTimeout;
      const originalClearTimeout = window.clearTimeout;
      const originalWs = state.ws;
      const originalRecorder = { ...recorder };
      const originalStatus = $("status").textContent;
      const originalLatency = $("latency").textContent;
      let scheduled = null;
      try {
        window.setTimeout = (callback, delay) => {
          scheduled = { callback, delay };
          return 4343;
        };
        window.clearTimeout = () => {};
        state.ws = { readyState: 1, send: () => {} };
        Object.assign(recorder, {
          active: true,
          permissionGranted: true,
          pickupAllowed: true,
          sendLocked: false,
          asrResponseTimeoutId: null,
          interruptCheckInFlight: false,
          ttsAsrBlocked: false,
          ttsAsrResumeAt: 0,
        });
        sendWs({ type: "audio_stream_end" });
        const scheduledDelay = scheduled?.delay ?? null;
        const lockedBeforeTimeout = recorder.sendLocked;
        scheduled?.callback();
        const lockedAfterTimeout = recorder.sendLocked;
        const canCaptureAfterTimeout = canCaptureAudio(state);
        const status = $("status").textContent;
        const latency = $("latency").textContent;
        sendWs({ type: "audio_stream_end" });
        handleEvent({ event_type: "asr_final", payload: { text: "测试" } });
        return {
          scheduledDelay,
          lockedBeforeTimeout,
          lockedAfterTimeout,
          canCaptureAfterTimeout,
          status,
          latency,
          timeoutClearedByFinal: recorder.asrResponseTimeoutId == null,
        };
      } finally {
        clearAsrResponseTimeout();
        window.setTimeout = originalSetTimeout;
        window.clearTimeout = originalClearTimeout;
        state.ws = originalWs;
        Object.assign(recorder, originalRecorder);
        setStatus(originalStatus);
        $("latency").textContent = originalLatency;
      }
    },

    auditPickupStateMatrix() {
      const base = {
        ws: { readyState: 1 },
        audioPlaying: false,
        audioQueue: [],
        ttsBuffers: new Map(),
        ttsStreams: new Map(),
        recorder: {
          active: true,
          permissionGranted: true,
          pickupAllowed: true,
          sendLocked: false,
          interruptCheckInFlight: false,
          ttsAsrBlocked: false,
          ttsAsrResumeAt: 0,
        },
      };
      const cases = [
        ["permission", { recorder: { permissionGranted: false } }],
        ["capture", { recorder: { active: false } }],
        ["websocket", { ws: { readyState: 0 } }],
        ["pickup policy", { recorder: { pickupAllowed: false } }],
        ["send lock", { recorder: { sendLocked: true } }],
        ["interrupt send lock", { recorder: { interruptCheckInFlight: true } }],
        ["TTS prebuffer", { recorder: { ttsAsrBlocked: true } }],
        ["player", { audioPlaying: true }],
        ["play queue", { audioQueue: [{}] }],
        ["stream prebuffer", { ttsStreams: new Map([["1", {}]]) }],
        ["buffered prebuffer", { ttsBuffers: new Map([["1", []]]) }],
      ];
      const makeState = (override) => ({
        ...base,
        ...override,
        recorder: { ...base.recorder, ...(override.recorder || {}) },
      });
      return {
        base: canCaptureAudio(base),
        blocked: cases.map(([name, override]) => ({ name, allowed: canCaptureAudio(makeState(override)) })),
      };
    },

    auditTtsMetadataValidation() {
      return withFakeAudio((audioStarts) => {
        const originalStatus = $("status").textContent;
        handleEvent({
          event_type: "tts_audio_chunk",
          turn_id: "wrong-profile-turn",
          payload: { seq: 930, format: "mp3", sample_rate: 24000, audio: makeBytes(8192), is_last: true },
        });
        const result = { audioStarts: audioStarts(), status: $("status").textContent };
        setStatus(originalStatus);
        return result;
      });
    },

    async auditPlaybackRejectionCleanup() {
      const originalAudio = window.Audio;
      const originalWs = state.ws;
      const recorder = state.recorder;
      const originalRecorder = {
        active: recorder.active,
        permissionGranted: recorder.permissionGranted,
        pickupAllowed: recorder.pickupAllowed,
        sendLocked: recorder.sendLocked,
        interruptCheckInFlight: recorder.interruptCheckInFlight,
        ttsAsrBlocked: recorder.ttsAsrBlocked,
        ttsAsrResumeAt: recorder.ttsAsrResumeAt,
      };
      let revokeCount = 0;
      try {
        resetPlaybackState();
        state.ws = { readyState: 1 };
        Object.assign(recorder, {
          active: true,
          permissionGranted: true,
          pickupAllowed: true,
          sendLocked: false,
          interruptCheckInFlight: false,
          ttsAsrBlocked: false,
          ttsAsrResumeAt: 0,
        });
        window.Audio = function RejectingAudio() {
          return {
            onended: null,
            onerror: null,
            play: () => Promise.reject(new Error("voice-test autoplay rejected")),
            pause: () => {},
            removeAttribute: () => {},
            load: () => {},
          };
        };
        state.ttsStreams.set("reject-only", { url: "blob:reject-only" });
        state.audioQueue.push({
          url: "blob:reject-only",
          revoke: () => {
            revokeCount += 1;
            state.ttsStreams.delete("reject-only");
          },
        });
        drainAudioQueue();
        await new Promise((resolve) => window.setTimeout(resolve, 0));
        const immediate = {
          revokeCount,
          streams: state.ttsStreams.size,
          audioPlaying: state.audioPlaying,
          queueLength: state.audioQueue.length,
          currentCleared: state.currentAudio === null && state.currentAudioItem === null,
        };
        await new Promise((resolve) => window.setTimeout(resolve, 560));
        return { ...immediate, canCaptureAfterResume: canCaptureAudio(state) };
      } finally {
        window.Audio = originalAudio;
        resetPlaybackState();
        state.ws = originalWs;
        Object.assign(recorder, originalRecorder);
      }
    },

    async auditPlaybackRejectionWithNextItem() {
      const originalAudio = window.Audio;
      const originalWs = state.ws;
      const recorder = state.recorder;
      const originalRecorder = {
        active: recorder.active,
        permissionGranted: recorder.permissionGranted,
        pickupAllowed: recorder.pickupAllowed,
        sendLocked: recorder.sendLocked,
        interruptCheckInFlight: recorder.interruptCheckInFlight,
        ttsAsrBlocked: recorder.ttsAsrBlocked,
        ttsAsrResumeAt: recorder.ttsAsrResumeAt,
      };
      const revokeCounts = [0, 0];
      const audios = [];
      try {
        resetPlaybackState();
        state.ws = { readyState: 1 };
        Object.assign(recorder, {
          active: true,
          permissionGranted: true,
          pickupAllowed: true,
          sendLocked: false,
          interruptCheckInFlight: false,
          ttsAsrBlocked: false,
          ttsAsrResumeAt: 0,
        });
        window.Audio = function QueueAudio() {
          const index = audios.length;
          const audio = {
            onended: null,
            onerror: null,
            play: () => index === 0
              ? Promise.reject(new Error("voice-test first item rejected"))
              : new Promise(() => {}),
            pause: () => {},
            removeAttribute: () => {},
            load: () => {},
          };
          audios.push(audio);
          return audio;
        };
        ["first", "second"].forEach((seq, index) => {
          state.ttsStreams.set(seq, { url: `blob:${seq}` });
          state.audioQueue.push({
            url: `blob:${seq}`,
            revoke: () => {
              revokeCounts[index] += 1;
              state.ttsStreams.delete(seq);
            },
          });
        });
        drainAudioQueue();
        await new Promise((resolve) => window.setTimeout(resolve, 0));
        const afterFirst = {
          audioStartsAfterFirstRejection: audios.length,
          firstRevokeCount: revokeCounts[0],
          streamsAfterFirstRejection: state.ttsStreams.size,
          audioPlayingAfterFirstRejection: state.audioPlaying,
          canCaptureWhileNextPlaying: canCaptureAudio(state),
        };
        audios[1]?.onerror?.();
        await new Promise((resolve) => window.setTimeout(resolve, 560));
        return {
          ...afterFirst,
          secondRevokeCount: revokeCounts[1],
          streamsAfterAll: state.ttsStreams.size,
          audioPlayingAfterAll: state.audioPlaying,
          canCaptureAfterAll: canCaptureAudio(state),
        };
      } finally {
        window.Audio = originalAudio;
        resetPlaybackState();
        state.ws = originalWs;
        Object.assign(recorder, originalRecorder);
      }
    },

    auditTtsFailureCleanup() {
      return auditTerminalTtsHandlerCleanup({
        event_type: "error",
        payload: { code: "tts_failed", message: "voice-test TTS failure" },
      });
    },

    auditMetadataFailureCleanup() {
      return auditTerminalTtsHandlerCleanup({
        event_type: "tts_audio_chunk",
        turn_id: "metadata-failure-turn",
        payload: {
          seq: 970,
          format: "mp3",
          sample_rate: 24000,
          channels: 1,
          audio: makeBytes(256),
          is_last: false,
        },
      });
    },

    auditInvalidTtsAudioCleanup() {
      return auditTerminalTtsHandlerCleanup({
        event_type: "tts_audio_chunk",
        turn_id: "invalid-audio-turn",
        payload: {
          seq: 972,
          format: "mp3",
          sample_rate: 16000,
          channels: 1,
          audio: "not-valid-base64***",
          is_last: false,
        },
      });
    },

    auditDirectionlessUpstreamCleanup() {
      return auditTerminalTtsHandlerCleanup({
        event_type: "error",
        payload: {
          code: "upstream_audio_profile_rejected",
          message: "voice-test real upstream rejection shape",
        },
      });
    },

    auditDirectionlessUpstreamWithoutTts() {
      return auditUpstreamRejectionPreservation({ withPrebuffer: false });
    },

    auditExplicitIatUpstreamPreservesTts() {
      return auditUpstreamRejectionPreservation({ direction: "iat", withPrebuffer: true });
    },

    auditActiveTtsFailureCleanup() {
      const originalAudio = window.Audio;
      const originalRevoke = URL.revokeObjectURL;
      const originalWs = state.ws;
      const recorder = state.recorder;
      const originalRecorder = {
        active: recorder.active,
        permissionGranted: recorder.permissionGranted,
        pickupAllowed: recorder.pickupAllowed,
        sendLocked: recorder.sendLocked,
        interruptCheckInFlight: recorder.interruptCheckInFlight,
        ttsAsrBlocked: recorder.ttsAsrBlocked,
        ttsAsrResumeAt: recorder.ttsAsrResumeAt,
      };
      let audioStarted = false;
      let pauseCount = 0;
      let revokeCount = 0;
      try {
        resetPlaybackState();
        state.ws = { readyState: 1 };
        Object.assign(recorder, {
          active: true,
          permissionGranted: true,
          pickupAllowed: true,
          sendLocked: false,
          interruptCheckInFlight: false,
          ttsAsrBlocked: false,
          ttsAsrResumeAt: 0,
        });
        URL.revokeObjectURL = () => { revokeCount += 1; };
        window.Audio = function PendingAudio() {
          audioStarted = true;
          return {
            onended: null,
            onerror: null,
            play: () => new Promise(() => {}),
            pause: () => { pauseCount += 1; },
            removeAttribute: () => {},
            load: () => {},
          };
        };
        handleEvent({
          event_type: "tts_audio_chunk",
          turn_id: "active-failure-turn",
          payload: {
            seq: 0,
            format: "mp3",
            sample_rate: 16000,
            channels: 1,
            audio: makeBytes(STREAMING_TTS_PREBUFFER_BYTES + 512),
            is_last: false,
          },
        });
        handleEvent({
          event_type: "error",
          payload: { code: "asr_failed", message: "voice-test ordinary ASR failure" },
        });
        const nonTtsErrorPreservedPlayback = state.currentAudio !== null
          && state.audioPlaying
          && state.ttsStreams.size === 1
          && pauseCount === 0
          && revokeCount === 0;
        handleEvent({
          event_type: "error",
          payload: {
            code: "upstream_audio_profile_rejected",
            message: "voice-test directionless active upstream failure",
          },
        });
        handleEvent({
          event_type: "error",
          payload: {
            code: "upstream_audio_profile_rejected",
            message: "voice-test repeated directionless upstream failure",
          },
        });
        return {
          audioStarted,
          nonTtsErrorPreservedPlayback,
          pauseCount,
          revokeCount,
          currentCleared: state.currentAudio === null && state.currentAudioItem === null && !state.audioPlaying,
          streams: state.ttsStreams.size,
          canCapture: canCaptureAudio(state),
        };
      } finally {
        window.Audio = originalAudio;
        URL.revokeObjectURL = originalRevoke;
        resetPlaybackState();
        state.ws = originalWs;
        Object.assign(recorder, originalRecorder);
      }
    },

    async auditEmptyFinalCleanup() {
      const originalAudio = window.Audio;
      const originalWs = state.ws;
      const recorder = state.recorder;
      const originalRecorder = {
        active: recorder.active,
        permissionGranted: recorder.permissionGranted,
        pickupAllowed: recorder.pickupAllowed,
        sendLocked: recorder.sendLocked,
        interruptCheckInFlight: recorder.interruptCheckInFlight,
        ttsAsrBlocked: recorder.ttsAsrBlocked,
        ttsAsrResumeAt: recorder.ttsAsrResumeAt,
      };
      let fakeAudio;
      let pauseCount = 0;
      try {
        resetPlaybackState();
        state.ws = { readyState: 1 };
        Object.assign(recorder, { active: true, permissionGranted: true, pickupAllowed: true, sendLocked: false });
        window.Audio = function EmptyFinalAudio() {
          fakeAudio = {
            onended: null, onerror: null,
            play: () => new Promise(() => {}),
            pause: () => { pauseCount += 1; }, removeAttribute: () => {}, load: () => {},
          };
          return fakeAudio;
        };
        handleEvent({ event_type: "tts_audio_chunk", payload: {
          seq: 980, format: "mp3", sample_rate: 16000, channels: 1,
          audio: makeBytes(STREAMING_TTS_PREBUFFER_BYTES + 128), is_last: false,
        } });
        const audioStarted = Boolean(fakeAudio);
        handleEvent({ event_type: "tts_audio_chunk", payload: {
          seq: 980, format: "mp3", sample_rate: 16000, channels: 1,
          audio: "", is_last: true,
        } });
        const before = {
          audioStarted,
          pauseCountBeforeEnded: pauseCount,
          playingBeforeEnded: state.audioPlaying,
          canCaptureBeforeEnded: canCaptureAudio(state),
        };
        fakeAudio?.onended?.();
        const canCaptureImmediatelyAfterEnded = canCaptureAudio(state);
        await new Promise((resolve) => window.setTimeout(resolve, 560));
        return { ...before, canCaptureImmediatelyAfterEnded, canCaptureAfterEnded: canCaptureAudio(state) };
      } finally {
        window.Audio = originalAudio;
        resetPlaybackState();
        state.ws = originalWs;
        Object.assign(recorder, originalRecorder);
      }
    },

    auditSendLockLifecycle() {
      const originalWs = state.ws;
      const recorder = state.recorder;
      const original = {
        active: recorder.active, permissionGranted: recorder.permissionGranted,
        pickupAllowed: recorder.pickupAllowed, sendLocked: recorder.sendLocked,
        interruptCheckInFlight: recorder.interruptCheckInFlight,
        ttsAsrBlocked: recorder.ttsAsrBlocked, ttsAsrResumeAt: recorder.ttsAsrResumeAt,
      };
      try {
        resetPlaybackState();
        state.ws = { readyState: 1, send: () => {} };
        Object.assign(recorder, { active: true, permissionGranted: true, pickupAllowed: true, sendLocked: false,
          interruptCheckInFlight: false, ttsAsrBlocked: false, ttsAsrResumeAt: 0 });
        sendWs({ type: "text", text: "test" });
        const lockedAfterText = recorder.sendLocked;
        const blockedWhileWaiting = !canCaptureAudio(state);
        handleEvent({ event_type: "voice_done", payload: {} });
        const unlockedAfterVoiceDone = !recorder.sendLocked;
        sendWs({ type: "audio_stream_end" });
        const lockedAfterAudioEnd = recorder.sendLocked;
        handleEvent({ event_type: "error", payload: { code: "asr_failed", message: "failed" } });
        const unlockedAfterError = !recorder.sendLocked;
        recorder.sendLocked = true;
        handleEvent({ event_type: "asr_ignored", payload: {} });
        const unlockedAfterAsrIgnored = !recorder.sendLocked;
        recorder.sendLocked = true;
        const activeSocket = state.ws;
        handleVoiceSocketClose({ readyState: 3 });
        const staleWsClosePreservedLock = recorder.sendLocked;
        handleVoiceSocketClose(activeSocket);
        const unlockedAfterWsClose = !recorder.sendLocked;
        recorder.active = true;
        recorder.permissionGranted = true;
        recorder.sendLocked = true;
        stopContinuousRecording({ finalize: false });
        return { lockedAfterText, blockedWhileWaiting, unlockedAfterVoiceDone,
          lockedAfterAudioEnd, unlockedAfterError, unlockedAfterAsrIgnored,
          staleWsClosePreservedLock, unlockedAfterWsClose,
          unlockedAfterManualStop: !recorder.sendLocked };
      } finally {
        state.ws = originalWs;
        Object.assign(recorder, original);
      }
    },

    auditConversationHistoryScroll() {
      const messages = $("messages");
      const original = messages.innerHTML;
      const originalScrollTop = messages.scrollTop;
      try {
        messages.innerHTML = "";
        for (let index = 1; index <= 20; index += 1) {
          addMessage(index % 2 === 0 ? "user" : "assistant-live", `第 ${index} 条对话，用于验证历史消息的滚动与保留。`);
        }
        const style = window.getComputedStyle(messages);
        const retainedMessages = messages.children.length;
        const scrollHeight = messages.scrollHeight;
        const clientHeight = messages.clientHeight;
        const startedAtBottom = Math.abs(scrollHeight - clientHeight - messages.scrollTop) <= 1;
        messages.scrollTop = 0;
        addMessage("assistant-live", "用户查看旧消息时，新消息不得强制拉回底部。");
        return {
          retainedMessages,
          overflowY: style.overflowY,
          scrollHeight,
          clientHeight,
          startedAtBottom,
          userScrollPositionPreserved: messages.scrollTop === 0,
        };
      } finally {
        messages.innerHTML = original;
        messages.scrollTop = originalScrollTop;
      }
    },

    async auditMicrophonePermissionFailure() {
      const originalGetUserMedia = navigator.mediaDevices.getUserMedia;
      const recorder = state.recorder;
      try {
        navigator.mediaDevices.getUserMedia = () => Promise.reject(new Error("permission denied"));
        let threw = false;
        try { await startContinuousRecording(); } catch { threw = true; }
        return { threw, active: recorder.active, permissionGranted: recorder.permissionGranted,
          status: $("status").textContent };
      } finally {
        navigator.mediaDevices.getUserMedia = originalGetUserMedia;
      }
    },

    auditMicrophoneTrackEnded() {
      const recorder = state.recorder;
      recorder.active = true;
      recorder.permissionGranted = true;
      recorder.stream = { getTracks: () => [] };
      if (typeof handleMicrophoneTrackEnded === "function") handleMicrophoneTrackEnded();
      return { active: recorder.active, permissionGranted: recorder.permissionGranted,
        streamCleared: recorder.stream === null };
    },

    auditConversationEventIsolation() {
      const originalConversationId = state.conversationId;
      const messages = $("messages");
      const originalMessages = messages.innerHTML;
      try {
        state.conversationId = "current-round";
        messages.innerHTML = "";
        handleEvent({
          conversation_id: "old-round",
          turn_id: "old-turn",
          event_type: "asr_final",
          payload: { text: "旧会话内容不应出现" },
        });
        const afterOld = messages.children.length;
        handleEvent({
          conversation_id: "current-round",
          turn_id: "current-turn",
          event_type: "asr_final",
          payload: { text: "当前会话内容" },
        });
        return {
          afterOld,
          afterCurrent: messages.children.length,
          text: messages.textContent,
        };
      } finally {
        state.conversationId = originalConversationId;
        messages.innerHTML = originalMessages;
      }
    },

    auditConversationEndPersistence() {
      const recorder = state.recorder;
      const originalRecorder = { ...recorder };
      const originalEnded = state.conversationEnded;
      const originalStatus = $("status").textContent;
      const originalMicLabel = $("micBtn").querySelector(".mic-label")?.textContent || "";
      try {
        Object.assign(recorder, {
          active: true,
          permissionGranted: true,
          pickupAllowed: true,
          sendLocked: false,
          speaking: false,
          ttsAsrBlocked: false,
          ttsAsrResumeAt: 0,
          stream: null,
          context: null,
          processor: null,
          source: null,
        });
        state.conversationEnded = false;
        handleEvent({ event_type: "conversation_ended", payload: { reason: "user_end_intent" } });
        beginTtsAsrBlock();
        clearTtsPlaybackState({ resumeImmediately: true });
        handleEvent({ event_type: "voice_done", payload: {} });
        return {
          recorderActive: recorder.active,
          canCapture: canCaptureAudio(state),
          status: $("status").textContent,
          micLabel: $("micBtn").querySelector(".mic-label")?.textContent || "",
        };
      } finally {
        state.conversationEnded = originalEnded;
        Object.assign(recorder, originalRecorder);
        setStatus(originalStatus);
        setMicButtonLabel(originalMicLabel);
      }
    },
  };
}

installVoiceTestHooks();
startDiagnosticsForTrace(null);
connect();
loadConfig();
loadRecorderTuning();
startNewConversation();
