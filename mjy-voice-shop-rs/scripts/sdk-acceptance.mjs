#!/usr/bin/env node
import { mkdir, readFile, stat, writeFile } from "node:fs/promises";
import { spawn } from "node:child_process";
import path from "node:path";
import process from "node:process";

const ROOT = path.resolve(import.meta.dirname, "..");
const OUT_DIR = path.join(ROOT, "ui-report");
const BASE_URL = process.env.BASE_URL || "http://127.0.0.1:8787";
const HOST = process.env.HOST || new URL(BASE_URL).hostname;
const PORT = process.env.PORT || new URL(BASE_URL).port || "80";
const TEXT = process.env.SDK_TEST_TEXT || "SDK严格测试：我要买一瓶可口可乐";
const RECORDING_AIFF = process.env.SDK_TEST_AIFF || "/tmp/mjy-sdk-acceptance-recording.aiff";
const RECORDING_PCM = process.env.SDK_TEST_PCM || "/tmp/mjy-sdk-acceptance-recording.pcm";

const cases = [
  {
    name: "python_text",
    command: "SDKs/python/run_text_demo.sh",
    env: {
      BASE_URL,
      TEXT,
      OUTPUT: "/tmp/mjy-sdk-acceptance-python-text.mp3",
    },
    expectText: true,
  },
  {
    name: "python_pcm",
    command: "SDKs/python/run_pcm_demo.sh",
    env: {
      BASE_URL,
      PCM_FILE: RECORDING_PCM,
      OUTPUT: "/tmp/mjy-sdk-acceptance-python-pcm.mp3",
    },
    expectText: false,
  },
  {
    name: "python_text_8k",
    command: "SDKs/python/run_text_demo.sh",
    env: {
      BASE_URL,
      TEXT,
      OUT_FORMAT: "mp3",
      OUT_RATE: "8000",
      OUTPUT: "/tmp/mjy-sdk-acceptance-python-text-8k.bin",
    },
    outputPath: "/tmp/mjy-sdk-acceptance-python-text-8k.mp3",
    expectedRate: 8000,
    expectText: true,
  },
  {
    name: "python_text_24k",
    command: "SDKs/python/run_text_demo.sh",
    env: {
      BASE_URL,
      TEXT,
      OUT_FORMAT: "mp3",
      OUT_RATE: "24000",
      OUTPUT: "/tmp/mjy-sdk-acceptance-python-text-24k.bin",
    },
    outputPath: "/tmp/mjy-sdk-acceptance-python-text-24k.mp3",
    expectedRate: 24000,
    expectText: true,
  },
  {
    name: "cpp_text",
    command: "SDKs/cpp/run_text_demo.sh",
    env: {
      HOST,
      PORT,
      TEXT,
      OUTPUT: "/tmp/mjy-sdk-acceptance-cpp-text.mp3",
    },
    expectText: true,
  },
  {
    name: "cpp_pcm",
    command: "SDKs/cpp/run_pcm_demo.sh",
    env: {
      HOST,
      PORT,
      PCM_FILE: RECORDING_PCM,
      OUTPUT: "/tmp/mjy-sdk-acceptance-cpp-pcm.mp3",
    },
    expectText: false,
  },
];

async function main() {
  await ensureServer();
  const protocolChecks = await runProtocolChecks();
  const handshakeChecks = await runHandshakeChecks();
  await ensureRecording();
  const results = [];
  for (const testCase of cases) {
    const startedAt = Date.now();
    const runResult = await runScript(testCase.command, testCase.env);
    const outputPath = testCase.outputPath || testCase.env.OUTPUT;
    const checks = await validateCase(testCase, runResult, outputPath);
    results.push({
      name: testCase.name,
      command: testCase.command,
      outputPath,
      durationMs: Date.now() - startedAt,
      exitCode: runResult.code,
      checks,
      ok: runResult.code === 0 && checks.every((check) => check.ok),
      stdoutTail: tail(runResult.stdout),
      stderrTail: tail(runResult.stderr),
    });
  }

  const failures = results.filter((item) => !item.ok);
  const report = {
    baseUrl: BASE_URL,
    host: HOST,
    port: PORT,
    text: TEXT,
    recordingPcm: RECORDING_PCM,
    protocolChecks,
    handshakeChecks,
    results,
    failures,
  };
  await mkdir(OUT_DIR, { recursive: true });
  await writeFile(path.join(OUT_DIR, "sdk-acceptance.json"), JSON.stringify(report, null, 2));

  if (failures.length) {
    console.error(`SDK 接入验收失败：${failures.map((item) => item.name).join(", ")}`);
    console.error(`报告：${path.join(OUT_DIR, "sdk-acceptance.json")}`);
    process.exit(1);
  }
  console.log(`SDK 接入验收通过：${results.map((item) => item.name).join(", ")}`);
  console.log(`报告：${path.join(OUT_DIR, "sdk-acceptance.json")}`);
}

async function runHandshakeChecks() {
  const base = BASE_URL.replace(/\/$/, "");
  const authResponse = await fetch(`${base}/api/device/auth`, {
    method: "POST",
    headers: { "content-type": "application/json" },
    body: JSON.stringify({ device_id: "DOLL-0001", device_secret: "demo-secret" }),
  });
  if (!authResponse.ok) throw new Error(`SDK 握手负例鉴权失败：HTTP ${authResponse.status}`);
  const { token } = await authResponse.json();
  const cases = [
    { name: "speex_24k", profile: ["speex", "24000", "mp3", "16000"], expected: "unsupported_audio_rate" },
    { name: "unsupported_opus_input", profile: ["opus", "16000", "mp3", "16000"], expected: "unsupported_audio_profile" },
  ];
  const results = [];
  for (const item of cases) {
    const url = new URL(`${base}/api/device/voice`);
    const [inFormat, inRate, outFormat, outRate] = item.profile;
    Object.entries({ device_id: "DOLL-0001", token, in_format: inFormat, in_rate: inRate,
      out_format: outFormat, out_rate: outRate }).forEach(([key, value]) => url.searchParams.set(key, value));
    for (const key of ["in_format", "in_rate", "out_format", "out_rate"]) {
      if (!url.searchParams.has(key)) throw new Error(`握手缺少 ${key}`);
    }
    const response = await fetch(url);
    const body = await response.json();
    if (response.status !== 400 || body.error !== item.expected) {
      throw new Error(`${item.name} 期望 HTTP400/${item.expected}，实际 HTTP${response.status}/${body.error}`);
    }
    const query = Object.fromEntries(url.searchParams);
    query.token = "[redacted]";
    results.push({ name: item.name, status: response.status, error: body.error, query });
  }
  return results;
}

async function runProtocolChecks() {
  const checks = [
    ["python_protocol", "python3", ["SDKs/python/protocol_self_test.py"]],
    ["cpp_build", "SDKs/cpp/build.sh", []],
    ["cpp_protocol", "SDKs/cpp/device_client", ["--self-test"]],
  ];
  const results = [];
  for (const [name, command, args] of checks) {
    const result = await run(command, args);
    results.push({ name, exitCode: result.code, stdout: result.stdout, stderr: result.stderr });
    if (result.code !== 0) throw new Error(`SDK 协议检查失败：${name}\n${result.stderr || result.stdout}`);
  }
  return results;
}

async function ensureServer() {
  try {
    const response = await fetch(`${BASE_URL}/api/health`);
    if (!response.ok) throw new Error(`HTTP ${response.status}`);
  } catch (error) {
    throw new Error(`本地服务不可访问：${BASE_URL}。请先执行 scripts/start-dev.sh。${error.message}`);
  }
}

async function ensureRecording() {
  await run("say", [
    "-v",
    "Eddy (中文（中国大陆）)",
    "-o",
    RECORDING_AIFF,
    "我要买一瓶可口可乐",
  ]);
  await run("ffmpeg", [
    "-y",
    "-loglevel",
    "error",
    "-i",
    RECORDING_AIFF,
    "-ac",
    "1",
    "-ar",
    "16000",
    "-f",
    "s16le",
    RECORDING_PCM,
  ]);
  const pcmStat = await stat(RECORDING_PCM);
  if (pcmStat.size < 16000) {
    throw new Error(`测试录音过短：${RECORDING_PCM} (${pcmStat.size} bytes)`);
  }
}

async function runScript(command, extraEnv) {
  return run(command, [], { ...process.env, ...extraEnv, PLAY: "0" });
}

function run(command, args, env = process.env) {
  return new Promise((resolve) => {
    const child = spawn(command, args, {
      cwd: ROOT,
      env,
      shell: false,
    });
    let stdout = "";
    let stderr = "";
    child.stdout.on("data", (data) => {
      stdout += data.toString();
      process.stdout.write(data);
    });
    child.stderr.on("data", (data) => {
      stderr += data.toString();
      process.stderr.write(data);
    });
    child.on("close", (code) => resolve({ code, stdout, stderr }));
    child.on("error", (error) => resolve({ code: 127, stdout, stderr: `${stderr}${error.message}` }));
  });
}

async function validateCase(testCase, runResult, outputPath) {
  const combined = `${runResult.stdout}\n${runResult.stderr}`;
  const checks = [
    {
      name: "进程退出成功",
      ok: runResult.code === 0,
      detail: `exit=${runResult.code}`,
    },
    {
      name: "收到 TTS 分片",
      ok: combined.includes("tts_audio_chunk"),
    },
    {
      name: "收到 voice_done",
      ok: combined.includes("voice_done"),
    },
    {
      name: "没有错误事件",
      ok: !combined.includes('"event_type":"error"') && !combined.includes("\nerror "),
    },
  ];
  if (testCase.expectText) {
    checks.push({
      name: "文字输入进入 ASR final",
      ok: combined.includes(TEXT),
    });
  } else {
    checks.push({
      name: "录音读取进入 ASR final",
      ok: combined.includes("asr_final") && combined.includes("可口可乐"),
    });
  }
  if (testCase.expectedRate) {
    checks.push({
      name: `TTS metadata 为 ${testCase.expectedRate}Hz`,
      ok: combined.includes(`sample_rate=${testCase.expectedRate}`),
    });
  }

  try {
    const mp3Stat = await stat(outputPath);
    const head = await readFile(outputPath).then((buffer) => buffer.subarray(0, 3).toString("latin1"));
    checks.push({
      name: "生成可播放 MP3 数据",
      ok: mp3Stat.size > 8192 && (head === "ID3" || mp3Stat.size > 16000),
      detail: `${mp3Stat.size} bytes`,
    });
  } catch (error) {
    checks.push({
      name: "生成可播放 MP3 数据",
      ok: false,
      detail: error.message,
    });
  }
  return checks;
}

function tail(value) {
  return value.split("\n").filter(Boolean).slice(-30).join("\n");
}

main().catch((error) => {
  console.error(error.message);
  process.exit(1);
});
