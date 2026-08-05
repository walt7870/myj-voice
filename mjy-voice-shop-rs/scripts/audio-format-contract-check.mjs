#!/usr/bin/env node
import { readFile } from "node:fs/promises";
import path from "node:path";

const root = path.resolve(import.meta.dirname, "..");

const paths = {
  browser: "static/app.js",
  python: "SDKs/python/device_client.py",
  cpp: "SDKs/cpp/device_client.cpp",
  pythonPcmDemo: "SDKs/python/run_pcm_demo.sh",
  pythonTextDemo: "SDKs/python/run_text_demo.sh",
  cppPcmDemo: "SDKs/cpp/run_pcm_demo.sh",
  cppTextDemo: "SDKs/cpp/run_text_demo.sh",
  sdkReadme: "SDKs/README.md",
  pythonReadme: "SDKs/python/README.md",
  cppReadme: "SDKs/cpp/README.md",
  interfaceDoc: "docs/接口接入说明.md",
};

const files = Object.fromEntries(
  await Promise.all(
    Object.entries(paths).map(async ([name, relativePath]) => [
      name,
      await readFile(path.join(root, relativePath), "utf8"),
    ]),
  ),
);

const browserProfile = "in_format=pcm&in_rate=16000&out_format=mp3&out_rate=16000";
const clientAndExampleText = [
  files.browser,
  files.python,
  files.cpp,
  files.pythonPcmDemo,
  files.pythonTextDemo,
  files.cppPcmDemo,
  files.cppTextDemo,
  files.sdkReadme,
  files.pythonReadme,
  files.cppReadme,
].join("\n");
const deprecatedMentions = files.interfaceDoc.match(/pcm16k/g) ?? [];

const assertions = [
  ["体验页固定 PCM/16k 上行、MP3/16k 下行四参数", files.browser.includes(browserProfile)],
  ["体验页声明统一拾音门控纯函数", /function canCaptureAudio\(.*\)/.test(files.browser)],
  ["体验页声明统一语音 UI 派生函数", /function deriveVoiceUiState\(.*\)/.test(files.browser)],
  ["体验页发送音频前复用统一门控", /if \(!canCaptureAudio\(state\)\) return;/.test(files.browser)],
  ["Python 暴露 --in-format", files.python.includes('"--in-format"')],
  ["Python 暴露 --in-rate", files.python.includes('"--in-rate"')],
  ["Python 暴露 --out-format", files.python.includes('"--out-format"')],
  ["Python 暴露 --out-rate", files.python.includes('"--out-rate"')],
  ["Python 使用 generic --audio", files.python.includes('"--audio"')],
  ["Python 握手传四参数", ["in_format", "in_rate", "out_format", "out_rate"].every((key) => files.python.includes(`"${key}":`))],
  ["C++ 暴露 --in-format", files.cpp.includes('key == "--in-format"')],
  ["C++ 暴露 --in-rate", files.cpp.includes('key == "--in-rate"')],
  ["C++ 暴露 --out-format", files.cpp.includes('key == "--out-format"')],
  ["C++ 暴露 --out-rate", files.cpp.includes('key == "--out-rate"')],
  ["C++ 使用 generic --audio", files.cpp.includes('key == "--audio"')],
  ["C++ 握手传四参数", ["in_format", "in_rate", "out_format", "out_rate"].every((key) => files.cpp.includes(`"&${key}="`))],
  ["客户端代码、示例和 SDK 文档不再使用 pcm16k", !clientAndExampleText.includes("pcm16k")],
  ["接口文档仅允许一处 pcm16k 废弃迁移说明", deprecatedMentions.length <= 1],
];

const failures = assertions.filter(([, ok]) => !ok).map(([name]) => name);
if (failures.length) {
  console.error(`音频格式调用契约检查失败（${failures.length} 项）：\n- ${failures.join("\n- ")}`);
  process.exit(1);
}

console.log(`音频格式调用契约检查通过：${assertions.length} 项`);
