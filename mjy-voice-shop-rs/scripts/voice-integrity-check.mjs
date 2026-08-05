#!/usr/bin/env node
import { writeFile, mkdir } from "node:fs/promises";
import path from "node:path";

const BASE_URL = process.env.UI_BASE_URL || "http://127.0.0.1:8787";
const OUT_DIR = process.env.VOICE_CHECK_OUT || "ui-report";

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

async function run() {
  await ensureServer();
  const { chromium } = await ensurePlaywright();
  const browser = await chromium.launch({ headless: true });
  const page = await browser.newPage();
  const consoleErrors = [];
  page.on("console", (msg) => {
    if (msg.type() === "error") consoleErrors.push(msg.text());
  });
  page.on("pageerror", (error) => consoleErrors.push(error.message));

  const testUrl = new URL(BASE_URL.endsWith("/") ? BASE_URL : `${BASE_URL}/`);
  testUrl.searchParams.set("voiceTest", "1");
  await page.goto(testUrl.toString(), { waitUntil: "networkidle" });

  const audit = await page.evaluate(async () => {
    const api = window.__voiceTest;
    if (!api) return { missingHook: true };
    return {
      missingHook: false,
      config: api.configSnapshot(),
      connectingSocketReuse: api.auditConnectingSocketReuse(),
      diagnosticSocketReuse: api.auditDiagnosticSocketReuse(),
      turnScopedTtsSequence: api.auditTurnScopedTtsSequence(),
      interleavedTtsOrdering: api.auditInterleavedTtsOrdering(),
      emptySequenceOrderingMarker: api.auditEmptySequenceOrderingMarker(),
      prebuffer: api.auditStreamingPrebuffer(),
      emptyAsr: api.auditEmptyAsrSuppression(),
      interrupt: api.auditPlaybackInterrupt(),
      recordingGate: api.auditRecordingGateDuringPlayback(),
      utteranceHardTimeout: api.auditUtteranceHardTimeout(),
      asrResponseTimeout: api.auditAsrResponseTimeout(),
      pickupStateMatrix: api.auditPickupStateMatrix(),
      ttsMetadata: api.auditTtsMetadataValidation(),
      playbackRejection: await api.auditPlaybackRejectionCleanup(),
      playbackRejectionQueue: await api.auditPlaybackRejectionWithNextItem(),
      ttsFailureCleanup: api.auditTtsFailureCleanup(),
      metadataFailureCleanup: api.auditMetadataFailureCleanup(),
      invalidTtsAudioCleanup: api.auditInvalidTtsAudioCleanup(),
      directionlessUpstreamCleanup: api.auditDirectionlessUpstreamCleanup(),
      directionlessUpstreamWithoutTts: api.auditDirectionlessUpstreamWithoutTts(),
      explicitIatUpstreamPreservesTts: api.auditExplicitIatUpstreamPreservesTts(),
      activeTtsFailureCleanup: api.auditActiveTtsFailureCleanup(),
      emptyFinalCleanup: await api.auditEmptyFinalCleanup(),
      conversationHistoryScroll: api.auditConversationHistoryScroll(),
      sendLockLifecycle: api.auditSendLockLifecycle(),
      permissionFailure: await api.auditMicrophonePermissionFailure(),
      trackEnded: api.auditMicrophoneTrackEnded(),
      conversationIsolation: api.auditConversationEventIsolation(),
      conversationEndPersistence: api.auditConversationEndPersistence(),
    };
  });

  assertCase("测试钩子已暴露", !audit.missingHook, audit);
  if (!audit.missingHook) {
    assertCase("播报静音固定开启", audit.config.ttsNoInterrupt === true, audit.config);
    assertCase(
      "CONNECTING WebSocket 被复用且 open 回调发送到触发时 socket",
      audit.connectingSocketReuse.createdSockets === 1
        && audit.connectingSocketReuse.sentOnConnectingSocket === 2
        && audit.connectingSocketReuse.decoySendCount === 0,
      audit.connectingSocketReuse,
    );
    assertCase(
      "诊断 WebSocket 跨轮复用，避免首个服务端事件在握手期间丢失",
      audit.diagnosticSocketReuse.createdSockets === 1
        && audit.diagnosticSocketReuse.firstSocketStayedOpen
        && audit.diagnosticSocketReuse.activeTraceId === "voice-test-trace-b",
      audit.diagnosticSocketReuse,
    );
    assertCase(
      "TTS 序列按 turn_id+seq 隔离且新轮不追加旧 SourceBuffer",
      audit.turnScopedTtsSequence.streamCount === 2
        && new Set(audit.turnScopedTtsSequence.keys).size === 2
        && audit.turnScopedTtsSequence.oldAppendCount === 0
        && audit.turnScopedTtsSequence.oldAudioStillPlaying,
      audit.turnScopedTtsSequence,
    );
    assertCase(
      "同一 turn 的交错 TTS 必须按 seq 播放且 seq0 仍可边收边播",
      audit.interleavedTtsOrdering.startsAfterSeq1 === 0
        && audit.interleavedTtsOrdering.firstWasSeq0
        && audit.interleavedTtsOrdering.secondWasSeq1
        && audit.interleavedTtsOrdering.audioStarts === 2,
      audit.interleavedTtsOrdering,
    );
    assertCase(
      "空 seq0 final 作为排序 marker 放行已缓冲的 seq1",
      audit.emptySequenceOrderingMarker.startsBeforeMarker === 0
        && audit.emptySequenceOrderingMarker.startsAfterMarker === 1,
      audit.emptySequenceOrderingMarker,
    );
    assertCase("TTS 首个小包不会立即开播", audit.prebuffer.firstTinyChunkAudioStarts === 0, audit.prebuffer);
    assertCase("达到预缓冲阈值后只启动一次播放", audit.prebuffer.afterThresholdAudioStarts === 1, audit.prebuffer);
    assertCase("短时空 ASR 不进入聊天气泡", audit.emptyAsr.messageDelta === 0 && !audit.emptyAsr.leakedText, audit.emptyAsr);
    assertCase("打断后停止当前播报并忽略旧 TTS 包", audit.interrupt.audioStartsAfterOldChunk === audit.interrupt.audioStartsAfterInterrupt, audit.interrupt);
    assertCase(
      "播报期间完全停止拾音且页面不显示可拾音",
      audit.recordingGate.normalAudioStarts === 0
        && audit.recordingGate.interruptSegments === 0
        && audit.recordingGate.status === "播报中，暂停收音"
        && audit.recordingGate.micLabel === "播报中，暂停收音"
        && audit.recordingGate.blockedClass,
      audit.recordingGate,
    );
    assertCase(
      "单句录音有独立硬截止，不依赖后续音频回调才能收尾",
      audit.utteranceHardTimeout.scheduledDelay === audit.utteranceHardTimeout.maxSpeechMs
        && audit.utteranceHardTimeout.sentTypes.includes("audio_stream_start")
        && audit.utteranceHardTimeout.sentTypes.includes("audio_stream_end")
        && !audit.utteranceHardTimeout.speakingAfterTimeout,
      audit.utteranceHardTimeout,
    );
    assertCase(
      "ASR 收尾超时后自动解锁并允许重说",
      audit.asrResponseTimeout.scheduledDelay === 8000
        && audit.asrResponseTimeout.lockedBeforeTimeout
        && !audit.asrResponseTimeout.lockedAfterTimeout
        && audit.asrResponseTimeout.canCaptureAfterTimeout
        && audit.asrResponseTimeout.status === "识别超时，请再说一遍"
        && audit.asrResponseTimeout.latency === "ASR 超时"
        && audit.asrResponseTimeout.timeoutClearedByFinal,
      audit.asrResponseTimeout,
    );
    assertCase(
      "拾音状态矩阵仅允许全部前置条件满足的状态",
      audit.pickupStateMatrix.base === true
        && audit.pickupStateMatrix.blocked.every((item) => item.allowed === false),
      audit.pickupStateMatrix,
    );
    assertCase(
      "服务端结束事件在 TTS 收尾后仍保持停止拾音和结束态",
      audit.conversationEndPersistence.recorderActive === false
        && audit.conversationEndPersistence.canCapture === false
        && audit.conversationEndPersistence.status === "对话已结束"
        && audit.conversationEndPersistence.micLabel === "☎ 开始持续监听",
      audit.conversationEndPersistence,
    );
    assertCase(
      "TTS 元数据不匹配时安全拒绝播放",
      audit.ttsMetadata.audioStarts === 0
        && audit.ttsMetadata.status.includes("期望 mp3/16000"),
      audit.ttsMetadata,
    );
    assertCase(
      "播放器拒绝播放时幂等释放当前 stream 并恢复拾音",
      audit.playbackRejection.revokeCount === 1
        && audit.playbackRejection.streams === 0
        && audit.playbackRejection.audioPlaying === false
        && audit.playbackRejection.queueLength === 0
        && audit.playbackRejection.currentCleared
        && audit.playbackRejection.canCaptureAfterResume === true,
      audit.playbackRejection,
    );
    assertCase(
      "播放器拒绝首项后继续下一项且不提前恢复拾音",
      audit.playbackRejectionQueue.audioStartsAfterFirstRejection === 2
        && audit.playbackRejectionQueue.firstRevokeCount === 1
        && audit.playbackRejectionQueue.streamsAfterFirstRejection === 1
        && audit.playbackRejectionQueue.audioPlayingAfterFirstRejection === true
        && audit.playbackRejectionQueue.canCaptureWhileNextPlaying === false
        && audit.playbackRejectionQueue.secondRevokeCount === 1
        && audit.playbackRejectionQueue.streamsAfterAll === 0
        && audit.playbackRejectionQueue.audioPlayingAfterAll === false
        && audit.playbackRejectionQueue.canCaptureAfterAll === true,
      audit.playbackRejectionQueue,
    );
    assertCase(
      "TTS 失败事件清理 partial prebuffer、静音锁且保持幂等",
      audit.ttsFailureCleanup.prebufferCreated
        && audit.ttsFailureCleanup.revokeCount === 1
        && audit.ttsFailureCleanup.streams === 0
        && audit.ttsFailureCleanup.buffers === 0
        && audit.ttsFailureCleanup.queueLength === 0
        && audit.ttsFailureCleanup.currentCleared
        && audit.ttsFailureCleanup.locksCleared
        && audit.ttsFailureCleanup.canCaptureAfterVoiceDone,
      audit.ttsFailureCleanup,
    );
    assertCase(
      "TTS metadata 错配清理已有 prebuffer、静音锁且保持幂等",
      audit.metadataFailureCleanup.prebufferCreated
        && audit.metadataFailureCleanup.revokeCount === 1
        && audit.metadataFailureCleanup.streams === 0
        && audit.metadataFailureCleanup.buffers === 0
        && audit.metadataFailureCleanup.queueLength === 0
        && audit.metadataFailureCleanup.currentCleared
        && audit.metadataFailureCleanup.locksCleared
        && audit.metadataFailureCleanup.canCaptureAfterVoiceDone,
      audit.metadataFailureCleanup,
    );
    assertCase(
      "TTS 失败会立即暂停并释放正在播放的 Audio",
      audit.activeTtsFailureCleanup.nonTtsErrorPreservedPlayback
        && audit.activeTtsFailureCleanup.audioStarted
        && audit.activeTtsFailureCleanup.pauseCount === 1
        && audit.activeTtsFailureCleanup.revokeCount === 1
        && audit.activeTtsFailureCleanup.currentCleared
        && audit.activeTtsFailureCleanup.streams === 0
        && audit.activeTtsFailureCleanup.canCapture,
      audit.activeTtsFailureCleanup,
    );
    assertCase(
      "非法 TTS base64 清理已有 prebuffer 且保持幂等",
      audit.invalidTtsAudioCleanup.prebufferCreated
        && audit.invalidTtsAudioCleanup.revokeCount === 1
        && audit.invalidTtsAudioCleanup.streams === 0
        && audit.invalidTtsAudioCleanup.queueLength === 0
        && audit.invalidTtsAudioCleanup.locksCleared
        && audit.invalidTtsAudioCleanup.canCaptureAfterVoiceDone,
      audit.invalidTtsAudioCleanup,
    );
    assertCase(
      "无 direction 的真实 upstream rejection 在 TTS 活跃时清理",
      audit.directionlessUpstreamCleanup.prebufferCreated
        && audit.directionlessUpstreamCleanup.revokeCount === 1
        && audit.directionlessUpstreamCleanup.streams === 0
        && audit.directionlessUpstreamCleanup.locksCleared
        && audit.directionlessUpstreamCleanup.canCaptureAfterVoiceDone,
      audit.directionlessUpstreamCleanup,
    );
    assertCase(
      "无 TTS 活动时 upstream rejection 不触发多余清理",
      audit.directionlessUpstreamWithoutTts.revokeCount === 0
        && audit.directionlessUpstreamWithoutTts.streams === 0
        && audit.directionlessUpstreamWithoutTts.interruptLockPreserved,
      audit.directionlessUpstreamWithoutTts,
    );
    assertCase(
      "明确 direction=iat 优先且不清理活跃 TTS",
      audit.explicitIatUpstreamPreservesTts.prebufferCreated
        && audit.explicitIatUpstreamPreservesTts.revokeCount === 0
        && audit.explicitIatUpstreamPreservesTts.streams === 1
        && audit.explicitIatUpstreamPreservesTts.ttsLockPreserved,
      audit.explicitIatUpstreamPreservesTts,
    );
    assertCase(
      "空 final 只终结对应 stream，当前播放器自然结束后才恢复拾音",
      audit.emptyFinalCleanup.audioStarted
        && audit.emptyFinalCleanup.pauseCountBeforeEnded === 0
        && audit.emptyFinalCleanup.playingBeforeEnded
        && !audit.emptyFinalCleanup.canCaptureBeforeEnded
        && audit.emptyFinalCleanup.canCaptureImmediatelyAfterEnded
        && audit.emptyFinalCleanup.canCaptureAfterEnded,
      audit.emptyFinalCleanup,
    );
    assertCase(
      "对话历史保留且消息区可纵向滚动",
      audit.conversationHistoryScroll.retainedMessages >= 20
        && audit.conversationHistoryScroll.overflowY === "auto"
        && audit.conversationHistoryScroll.scrollHeight > audit.conversationHistoryScroll.clientHeight
        && audit.conversationHistoryScroll.startedAtBottom
        && audit.conversationHistoryScroll.userScrollPositionPreserved,
      audit.conversationHistoryScroll,
    );
    assertCase(
      "文本/音频结束发送锁持续到 voice_done 或终止错误",
      audit.sendLockLifecycle.lockedAfterText
        && audit.sendLockLifecycle.blockedWhileWaiting
        && audit.sendLockLifecycle.unlockedAfterVoiceDone
        && audit.sendLockLifecycle.lockedAfterAudioEnd
        && audit.sendLockLifecycle.unlockedAfterError
        && audit.sendLockLifecycle.unlockedAfterAsrIgnored
        && audit.sendLockLifecycle.staleWsClosePreservedLock
        && audit.sendLockLifecycle.unlockedAfterWsClose
        && audit.sendLockLifecycle.unlockedAfterManualStop,
      audit.sendLockLifecycle,
    );
    assertCase(
      "麦克风权限拒绝被捕获并刷新不可拾音 UI",
      !audit.permissionFailure.threw
        && !audit.permissionFailure.active
        && !audit.permissionFailure.permissionGranted
        && audit.permissionFailure.status.includes("麦克风"),
      audit.permissionFailure,
    );
    assertCase(
      "麦克风 track ended 会关闭 capture 和权限状态",
      !audit.trackEnded.active && !audit.trackEnded.permissionGranted && audit.trackEnded.streamCleared,
      audit.trackEnded,
    );
    assertCase(
      "旧会话事件不会污染当前页面",
      audit.conversationIsolation.afterOld === 0
        && audit.conversationIsolation.afterCurrent === 1
        && !audit.conversationIsolation.text.includes("旧会话内容不应出现"),
      audit.conversationIsolation,
    );
  }
  assertCase("控制台无错误", consoleErrors.length === 0, { consoleErrors });

  await browser.close();
  await writeReport(audit, consoleErrors);

  if (failures.length) {
    console.error(`语音完整性验收失败：${failures.length} 个问题。报告：${path.resolve(OUT_DIR, "voice-integrity.json")}`);
    process.exit(1);
  }
  console.log(`语音完整性验收通过。报告：${path.resolve(OUT_DIR, "voice-integrity.json")}`);
}

async function writeReport(audit, consoleErrors) {
  await mkdir(OUT_DIR, { recursive: true });
  await writeFile(
    path.join(OUT_DIR, "voice-integrity.json"),
    JSON.stringify({ baseUrl: BASE_URL, results, failures, audit, consoleErrors }, null, 2),
  );
}

run().catch((error) => {
  console.error(error);
  process.exit(1);
});
