# 语音连接输入输出格式参数设计

## 目标

为浏览器和设备语音 WebSocket 增加连接级输入、输出音频格式参数。每条连接固定一种输入格式和一种输出格式，允许不同连接并发选择不同组合。格式不写入全局配置或数据库。

## 协议

两条语音 WebSocket 使用相同查询参数：

```text
/api/device/voice?device_id=...&token=...&in_format=pcm16k&out_format=mp3
/api/chat/voice?in_format=pcm16k&out_format=mp3
```

参数规则：

| 参数 | 可选值 | 默认值 | 作用 |
| --- | --- | --- | --- |
| `in_format` | `mp3`, `pcm16k` | `mp3` | 客户端上行音频格式 |
| `out_format` | `mp3`, `pcm16k` | `mp3` | 服务端下行 TTS 音频格式 |

`pcm16k` 固定表示 PCM signed 16-bit little-endian、16000Hz、单声道。连接升级后不允许切换格式。只接受精确小写值 `mp3`、`pcm16k`，不接受大小写变体或别名，避免同一能力出现多套协议名称。

现有体验页显式使用：

```text
in_format=pcm16k&out_format=mp3
```

项目不存在需要兼容的历史客户端，因此未传 `in_format` 时按 MP3 解释，不再沿用当前隐式 PCM 行为。

## 服务端结构

新增类型化音频格式模型：

```text
AudioFormat::Mp3
AudioFormat::Pcm16k
```

格式模型负责：

- 解析查询值和默认值。
- 提供讯飞协议所需的 encoding、sample rate、channels 和 bit depth。
- 生成 `tts_audio_chunk` 元数据。
- 生成 `/api/device/config` 的格式能力描述。

WebSocket 握手解析得到 `VoiceConnectionFormats { input, output }`，随后显式传入语音接收、ASR、对话处理和 TTS 任务。异步 TTS 任务必须复制当前连接格式，不能在任务执行期间重新读取全局值。

## 输入数据流

客户端继续使用现有 JSON 事件和 base64 音频正文：

```text
audio_stream_start -> audio_stream_chunk -> audio_stream_end
audio_segment
```

输入格式只在连接查询参数声明，不在每个事件重复。

- `in_format=mp3`：服务端将音频按讯飞 IAT 的 `lame` 编码发送。
- `in_format=pcm16k`：服务端将音频按 `raw`、16000Hz、16bit、mono 发送。
- 服务端不执行 MP3 解码或 PCM 转码。
- 流式 MP3 与流式 PCM 都沿用当前 base64 JSON 分片通道。

如果客户端声明格式与实际音频不一致，服务端不做格式猜测，由 IAT 错误映射为现有 `asr_failed` 事件。

## 输出数据流

服务端根据 `out_format` 构造讯飞 TTS 请求：

- `out_format=mp3`：请求 `lame` 编码，保持各 TTS provider 当前支持的 MP3 采样率。
- `out_format=pcm16k`：请求 `raw`、16000Hz、16bit、mono。
- 服务端不做编码、解码或重采样。

两种格式都使用现有事件：

```json
{
  "event_type": "tts_audio_chunk",
  "payload": {
    "audio": "base64 audio bytes",
    "seq": 0,
    "is_last": false,
    "format": "pcm16k",
    "sample_rate": 16000,
    "channels": 1,
    "bit_depth": 16
  }
}
```

MP3 事件包含 `format`、实际 `sample_rate` 和 `channels`；`bit_depth` 只用于 PCM。Mock provider 也必须按连接格式返回一致的元数据和可区分的测试音频内容。

## 能力发现和客户端

`GET /api/device/config` 公布：

- 输入默认格式 `mp3`。
- 输出默认格式 `mp3`。
- 输入和输出支持 `mp3`、`pcm16k`。
- `pcm16k` 的完整物理参数。
- WebSocket 查询参数名 `in_format`、`out_format`。

体验页固定请求 PCM 输入、MP3 输出，继续使用现有 MP3 MediaSource/Audio 播放链路，不增加浏览器 PCM 播放器。

Python 和 C++ SDK 增加连接参数及命令行选项：

```text
--in-format mp3|pcm16k
--out-format mp3|pcm16k
```

SDK 根据输出元数据保存正确扩展名；实时播放时，MP3 使用现有播放器，PCM 使用明确的 16kHz/16bit/mono 原始音频参数。示例脚本在上传 PCM 文件时显式传 `--in-format pcm16k`。

## 错误处理

- 不支持的 `in_format` 或 `out_format` 在 WebSocket 升级前返回 HTTP 400，错误码为 `unsupported_audio_format`。
- 缺省参数使用 MP3，不返回错误。
- 上游不接受所选格式时返回 `tts_failed` 或 `asr_failed`，不静默切换格式。
- PCM 音频长度不是 16bit 样本边界时返回 `bad_request`，不向 IAT 发送损坏数据。
- 连接期间事件不能覆盖握手格式。

## 测试和验收

实施采用测试驱动开发，先验证失败再实现。自动化覆盖：

1. 查询参数默认值、两种合法格式和非法格式。
2. IAT payload 在 MP3/PCM 下分别使用 `lame`/`raw`。
3. 两种 TTS provider 的 MP3/PCM payload 参数。
4. `tts_audio_chunk` 的格式和物理参数元数据。
5. `/api/device/config` 的默认值、支持列表和查询参数说明。
6. `/api/device/voice` 与 `/api/chat/voice` 的连接格式隔离。
7. 体验页显式 PCM 输入、MP3 输出且现有语音门控回归通过。
8. Python/C++ SDK 对查询参数、保存格式和播放命令的验收。

交付前执行：

```bash
cargo test
npm run voice:check
npm run sdk:check
git diff --check
```

有真实讯飞凭据时增加云端烟测，分别验证 MP3 输入、PCM 输入、MP3 输出和 PCM 输出。任何原生格式被上游拒绝，都视为功能未完成，不能用静默转码或降级掩盖。

## 非目标

- 不增加全局音频格式配置。
- 不支持连接中途切换格式。
- 不增加 WebSocket binary frame。
- 不引入服务端音频转码库。
- 不扩展 `mp3`、`pcm16k` 以外的格式。
