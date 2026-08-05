# 设备接入 SDK 示例

Python 与 C++ demo 用于验证设备鉴权、四参数音频档位、音频上行和 TTS 下行。它们不包含编解码器，不会把 PCM 转成 MP3、Opus 或 Speex。

## 连接协议

设备先读 `GET /api/device/config` 的动态 `audio_profiles`，再连接：

```text
WS /api/device/voice?device_id=...&token=...&in_format=...&in_rate=...&out_format=...&out_rate=...
```

- format：`mp3 | pcm | opus | speex`，默认 `mp3`。
- rate：`8000 | 16000 | 24000`，默认 `16000`；Speex 仅 8000/16000。
- 档位在连接期间固定。合法值不代表当前 provider 一定支持，实际以 `audio_profiles` 为准。

## 一键运行

```bash
SDKs/python/run_text_demo.sh
SDKs/python/run_pcm_demo.sh
SDKs/cpp/run_text_demo.sh
SDKs/cpp/run_pcm_demo.sh
```

四个脚本都显式传递 format 和 rate。PCM 脚本使用 `IN_FORMAT=pcm IN_RATE=16000`，上传参数是 generic `--audio`。

完整命令示例：

```bash
SDKs/python/.venv/bin/python SDKs/python/device_client.py \
  --base-url http://127.0.0.1:8787 --text "买一瓶可乐" \
  --in-format mp3 --in-rate 16000 --out-format mp3 --out-rate 16000

SDKs/cpp/device_client --host 127.0.0.1 --port 8787 \
  --audio /tmp/input.pcm --in-format pcm --in-rate 16000 \
  --out-format mp3 --out-rate 16000 --output /tmp/reply.mp3
```

## 公网测试环境

当前测试环境：

```text
https://www.niuwancheng.cn/myj-voice-shop
```

公网必须使用已线下配置的真实设备凭据；`DOLL-0001 / demo-secret` 只允许本机或 SSH 隧道调试。Python demo 支持 HTTPS/WSS 和上下文路径，可直接用于公网验收：

```bash
DEVICE_ID='<已配置设备 ID>' \
DEVICE_SECRET='<独立设备密钥>' \
BASE_URL=https://www.niuwancheng.cn/myj-voice-shop \
SDKs/python/run_text_demo.sh

DEVICE_ID='<已配置设备 ID>' \
DEVICE_SECRET='<独立设备密钥>' \
BASE_URL=https://www.niuwancheng.cn/myj-voice-shop \
SDKs/python/.venv/bin/python SDKs/python/device_client.py \
  --base-url "$BASE_URL" --device-id "$DEVICE_ID" --device-secret "$DEVICE_SECRET" \
  --text "请简短介绍一下可口可乐" --play --interrupt-after-first-chunk
```

C++ demo 为嵌入式迁移参考，只实现明文 HTTP/WS。直接连接公网 HTTPS/WSS 时，需要板端接入 TLS/WebSocket 库；也可以在可信内网网关后使用 `HOST/PORT/BASE_PATH` 连接转发后的明文服务。

## 实体按键打断播报

Python 与 C++ demo 均提供 `interrupt_tts_from_button` 状态机和 `--interrupt-after-first-chunk` 测试入口。真实板端的 GPIO ISR 只做消抖并向唯一的音频/网络 owner task 投递 `BUTTON_INTERRUPT`；不得在 ISR 中直接发送 WebSocket、停止 decoder、释放内存或等待锁。

owner task 收到事件后必须先本地停播：原子记录当前 `conversation_id + turn_id`，立即停止 decoder/DAC，清空 DMA/I2S、预缓冲、乱序 seq 和软件播放队列，然后再通过原 WebSocket 发送：

```json
{"type":"tts_interrupt","conversation_id":"当前会话","turn_id":"当前播报轮次","source":"button"}
```

服务端确认格式为：

```json
{
  "event_type": "tts_interrupted",
  "conversation_id": "当前会话",
  "turn_id": "被打断轮次",
  "payload": {"source":"button","status":"interrupted"}
}
```

`payload.status` 还可能是 `already_interrupted` 或 `already_finished`；未知轮次、会话不匹配、缺少字段或非 `button` 来源返回 `error.code=bad_request`。重复按钮和非播报状态按钮为 no-op。本地停播不等待确认，控制帧发送失败也不恢复旧音频；播放器排空后立即回到 `LISTENING`，不额外固定等待 500ms。

SDK 使用 `conversation_id + turn_id` 标识播放轮次，并保留最多 64 个近期被打断轮次。相同 pair 的迟到 `llm_delta`、`reply_sentence`、`tts_audio_chunk`、`voice_done` 必须在 base64 解码和 seq 校验前丢弃，新的 pair 正常建立播放器。被打断轮不会再收到普通 `voice_done`，终止信号是 `tts_interrupted`；`intent_analysis`、`product_matches`、`order_draft`、`order_created`、`order_refunded`、`analysis_done`、`latency_metrics` 等业务尾事件仍会继续，同一 WebSocket 可立即上送下一轮语音。

服务端取消仅覆盖旧轮的 LLM/reply/TTS producer，并主动终止仍在等待的 provider 工作；已经启动的分析与订单动作不取消，并按同一会话的轮次顺序提交。仓库当前只包含 Rust 服务和 Python/C++ POSIX demo，不包含真实开发板 GPIO、decoder、DAC、DMA/I2S HAL 源码；真实按键接线、ISR-safe queue 和音频 HAL 收敛仍需在板端固件仓库实现与实测。

本地协议状态机验证：

```bash
SDKs/python/.venv/bin/python SDKs/python/protocol_self_test.py
SDKs/cpp/build.sh
SDKs/cpp/device_client --self-test
```

## 分包与播放边界

- PCM：S16LE、mono、无 WAV 头；40ms 大小为 `rate * 2 * 40 / 1000` bytes。
- MP3：连续编码字节流，demo 按合理字节块顺序发送。
- Speex：输入必须已由设备编码为 quality 7；每个 20ms 包为 38 bytes（8k）或 60 bytes（16k）。
- Opus：必须每个 `audio_stream_chunk` 恰好一个完整包。普通平面文件不保留可变包边界，因此 demo 明确拒绝 Opus 文件上行。

MP3 与 PCM 可用 `--play` 本机播放，PCM 播放命令使用协商的 `out_rate`。Speex 固定长包保存为 raw `.speex`；Opus 可变长包保存为 `.opuspack`，每包是 `uint32 little-endian 长度 + 包字节`。`.opuspack` 是诊断分包文件，不是 Ogg/Opus 容器，不能直接播放；真实板端应按每个 `tts_audio_chunk` 实时交给 decoder。指定 `--play` 处理 Opus/Speex 会明确失败。

每个 `tts_audio_chunk` 的 `format` 和 `sample_rate` 都会与连接的 output profile 比较，不匹配立即报错；输出扩展名按 format 归一化。Opus/Speex 下行的每个 chunk 都是非空、完整的真实 packet，最后一个真实 packet 携带 `is_last=true`；服务端不会发送空压缩 chunk。

标准 TTS 可并行合成多句，不同 `seq` 的 chunk 可交错到达。SDK 保留 `seq=0` 的边收边播，对后续 seq 预缓冲并按 seq 顺序写入/播放；`voice_done` 时仍有缺口或未结束 seq 会明确报错。

Python 支持 HTTPS/WSS 和上下文路径。C++ 示例为便于嵌入式迁移只实现明文 HTTP/WS；公网 HTTPS/WSS 需要板端 TLS/WebSocket 库或可信内网反向代理。
