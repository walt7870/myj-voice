# Python 设备模拟 SDK

## 安装和运行

```bash
SDKs/python/setup.sh
SDKs/python/run_text_demo.sh
SDKs/python/run_pcm_demo.sh
```

完整文本命令：

```bash
SDKs/python/.venv/bin/python SDKs/python/device_client.py \
  --base-url http://127.0.0.1:8787 --text "我要一瓶可口可乐" \
  --in-format mp3 --in-rate 16000 \
  --out-format mp3 --out-rate 16000 --output /tmp/reply.mp3
```

模拟实体按键在首个可播放 TTS 包后打断：

```bash
SDKs/python/.venv/bin/python SDKs/python/device_client.py \
  --base-url http://127.0.0.1:8787 --text "我要一瓶可口可乐" --play \
  --interrupt-after-first-chunk
```

## 实体按键接入

板端完成按键消抖后，在音频/网络 owner task 中调用 `interrupt_tts_from_button(socket, playback, stream_player)`。不要在 GPIO ISR 中直接操作 WebSocket、decoder 或内存；ISR 只投递一个按键事件给 owner task。

该接口只在正在预缓冲或播放 TTS 时生效。首次调用会先在本地强制停止播放器、清空乱序缓冲和待播数据，再通过当前 WebSocket 发送：

```json
{"type":"tts_interrupt","conversation_id":"当前会话","turn_id":"当前播报轮次","source":"button"}
```

无活跃播报或按键抖动导致的重复调用会直接返回 `False`，不会重复停播或发包。网络发送失败也不会恢复旧音频；本地仍保持停播状态。SDK 以 `(conversation_id, turn_id)` 二元组维护最多 64 个近期被打断轮次，会在 base64 解码和 seq 校验之前丢弃匹配二元组的迟到 `llm_delta`、`reply_sentence`、`tts_audio_chunk` 和 `voice_done`；另一个会话即使复用了相同 `turn_id` 也不会被误丢。收到 `tts_interrupted` 只结束该播放轮次，不关闭 WebSocket。下一轮播报的首个音频包会重新启动播放器。

SDK 示例中的 `PlaybackState` 和 `StreamPlayer` 应由同一个 owner task 持有；真实固件可把 `StreamPlayer.stop_now()` 替换为 decoder/DAC 的立即停止与队列清空实现。

命令行 demo 使用 `receive_events(..., one_shot=True)`：普通轮次在 `voice_done` 返回；打断轮不再等待不会到达的 `voice_done`，而是在收到该 turn 的 `tts_interrupted` 和业务尾事件 `latency_metrics` 后返回，因此订单/分析事件不会被提前截断。常驻设备连接应使用 `one_shot=False`，这样每轮 `voice_done` 或打断确认都不会关闭接收循环，可继续在同一 WebSocket 上发起下一轮；收到 `conversation_ended` 时才结束会话接收。

完整 PCM 命令：

```bash
SDKs/python/.venv/bin/python SDKs/python/device_client.py \
  --base-url http://127.0.0.1:8787 --audio /tmp/input.pcm --stream \
  --in-format pcm --in-rate 16000 \
  --out-format mp3 --out-rate 16000 --output /tmp/reply.mp3
```

format choices 为 `mp3/pcm/opus/speex`，rate choices 为 `8000/16000/24000`，双向默认均为 `mp3/16000`。Speex 不支持 24k。

客户端同时兼容 legacy 和 modern `websockets` 连接签名：只在当前 `websockets.connect` 显式支持 `proxy` 参数时才传 `proxy=None`，因此 Python 3.13 + websockets 11.x 不会把该参数误传给 event loop，新版仍会禁用环境代理以保持直连行为。

demo 不编码音频。PCM 按协商 rate 的 40ms S16LE mono 分片；Speex 文件必须是 quality-7 固定包（8k 38 bytes，16k 60 bytes）；MP3 按连续 bytes 发送。Opus 是可变长度 packet，普通文件无法表达边界，因此文件上行不支持；设备实时接入时应直接把每个编码包作为一个 chunk。

`--play` 仅支持 MP3/PCM；PCM 使用 `--out-rate`。Speex 固定包保存为 raw `.speex`；Opus 保存为 `.opuspack`，每包是 `uint32 little-endian 长度 + 包字节`，不是 Ogg/Opus 容器也不能直接播放。客户端逐个校验 TTS `format/sample_rate`，并将交错 seq 按序落盘。Opus/Speex 下行事件始终携带一个非空、完整的真实 packet，最后一个真实 packet 携带 `is_last=true`，不会额外收到空压缩 final。

公网示例：

```bash
DEVICE_ID='<已配置设备 ID>' \
DEVICE_SECRET='<独立设备密钥>' \
BASE_URL=https://www.niuwancheng.cn/myj-voice-shop \
SDKs/python/run_text_demo.sh
```

`DOLL-0001 / demo-secret` 只会在 loopback 地址上自动使用。公网或其他设备必须显式传 `--device-secret`（脚本对应 `DEVICE_SECRET`）；客户端会在发起网络请求前拒绝公网 DOLL 凭据。
