# C++ 设备模拟 SDK

示例使用 POSIX socket 手写明文 HTTP/WebSocket，便于迁移到嵌入式 Linux；不包含 TLS 或 codec 依赖。

## 编译和运行

```bash
SDKs/cpp/build.sh
SDKs/cpp/run_text_demo.sh
SDKs/cpp/run_pcm_demo.sh
```

完整文本命令：

```bash
SDKs/cpp/device_client --host 127.0.0.1 --port 8787 \
  --text "我要一瓶可口可乐" \
  --in-format mp3 --in-rate 16000 \
  --out-format mp3 --out-rate 16000 --output /tmp/reply.mp3
```

在第一段可播放 TTS 到达后模拟一次消抖完成的实体按钮：

```bash
SDKs/cpp/device_client --host 127.0.0.1 --port 8787 \
  --text "介绍一下这个商品" \
  --interrupt-after-first-chunk
```

demo 会先停止本地播放器并清空当前轮次的 seq/预缓冲，再在原 WebSocket 上发送：

```json
{"type":"tts_interrupt","conversation_id":"当前会话","turn_id":"当前播报轮次","source":"button"}
```

同一 `conversation_id + turn_id` 的迟到 `llm_delta`、`reply_sentence`、`tts_audio_chunk` 和 `voice_done` 会在 base64 解码及 seq 校验前丢弃；新的 `turn_id` 会重新建立播放状态并可正常播放。`tts_interrupted` 只是当前播报的终止确认，不会关闭 WebSocket。自动打断 demo 会等待该轮 `tts_interrupted` 和 `latency_metrics` 业务尾都到达后退出，因此不依赖已取消轮次不会再发送的 `voice_done`。若控制帧发送失败，demo 会保留本地停播结果并明确报错退出，不会等待不可能到达的确认。

POSIX demo 不再用同步 `pclose` 实现紧急停播：播放器通过独立子进程组运行，并分别保存 shell leader PID 与 PGID。即使播放命令把 decoder 放到后台后由 shell 先退出，`stop_now` 仍会关闭输入并立即终止整个进程组；owner task 只执行有界的 `WNOHANG` 回收，未在时限内收敛的 leader PID + PGID 会完整移交后台 reaper。reaper 独立执行阻塞 `waitpid` 并持续确认旧 PGID 消失，因此不会把 D-state 等内核等待带回按钮/音频 owner task，也不会丢失 zombie/orphan 的所有权。正常 `close` 同样有界：先用 EOF 给播放器优雅退出时间，超时后按 `SIGTERM`、`SIGKILL` 升级，必要时移交 reaper。移交成功后新轮播放器可以立即启动，旧组仍由 reaper 单独追踪。socket 写入在 Linux 使用 `MSG_NOSIGNAL`、macOS 使用 `SO_NOSIGPIPE`，并在 `EINTR` 时重试，断链不会因 `SIGPIPE` 直接杀死设备进程。

## 实体 GPIO 按钮接入

`device_client.cpp` 中的 `PlaybackState` 和 `interrupt_tts_from_button` 展示了固件 owner task 应复用的状态机。真实板端应保持如下线程边界：

```text
GPIO ISR（只消抖/投递 BUTTON_INTERRUPT）
  -> 音频/网络 owner task
     1. 原子记录并清空当前 conversation_id + turn_id
     2. decoder_stop / dac_stop / 清 DMA、I2S、预缓冲和待播 seq
     3. 发送 tts_interrupt 控制帧
     4. 立即恢复 LISTENING
```

不要在 GPIO ISR 中直接操作 WebSocket、decoder、动态内存或等待锁。ISR 只应使用平台的 ISR-safe queue/notification 把事件交给唯一拥有播放器与 WebSocket 的 task；这样按钮与最后一个 TTS 包会按 owner task 的串行顺序收敛到“旧轮队列为空、正在拾音”。网络发送失败时也不要恢复旧音频，本地停播仍然生效；owner task 应记录发送失败并退出当前自动演示或进入既有的有界重连流程，不得继续等待 `tts_interrupted`。重复按钮或没有活跃播报时为 no-op。

在实际固件中，将 demo 的两个回调分别替换为：

- `stop_and_clear_playback`：停止 MP3/PCM decoder 与 DAC，清空 DMA/I2S 和软件播放队列；
- `send_control`：通过现有长连接发送状态机生成的 JSON。不得由调用方猜测 `turn_id`。

播放器排空后直接恢复麦克风拾音，不增加固定 500ms 延迟。若硬件存在声学拖尾，只配置实测所需的最短板端 guard time。

完整 PCM 命令：

```bash
SDKs/cpp/device_client --host 127.0.0.1 --port 8787 \
  --audio /tmp/input.pcm --in-format pcm --in-rate 16000 \
  --out-format mp3 --out-rate 16000 --output /tmp/reply.mp3
```

format choices 为 `mp3/pcm/opus/speex`，rate choices 为 `8000/16000/24000`，默认 `mp3/16000`。Speex 仅支持 8k/16k。

客户端不做编码。PCM 是 S16LE mono，40ms 分片随 `in_rate` 计算；Speex 输入是已编码 quality-7 包（8k 38 bytes、16k 60 bytes）；MP3 是连续字节流。Opus 平面文件没有可靠的可变包边界，demo 明确拒绝，而不是错误切片。

MP3/PCM 支持 `--play`，PCM 播放命令使用 `out_rate`。Speex 固定包保存为 raw `.speex`；Opus 保存为 `.opuspack`，每包是 `uint32 little-endian 长度 + 包字节`，不是 Ogg/Opus 容器也不能直接播放。收到每个 TTS event 都校验 `format/sample_rate`，并将交错 seq 按序落盘。Opus/Speex 下行事件始终携带一个非空、完整的真实 packet，最后一个真实 packet 携带 `is_last=true`，不会额外收到空压缩 final。

上下文路径示例：

```bash
HOST=127.0.0.1 PORT=8787 BASE_PATH=/myj-voice-shop SDKs/cpp/run_text_demo.sh
```

`DOLL-0001 / demo-secret` 只在 `127.0.0.1` / `localhost` / `::1` 自动使用。连接非本机服务时，必须使用已线下配置的设备，并显式传入 `--device-id` 和 `--device-secret`（脚本对应 `DEVICE_ID` / `DEVICE_SECRET`）。客户端会在连网前拒绝将 DOLL 演示凭据用于非本机地址。

直接连接 HTTPS/WSS 需要集成 TLS/WebSocket 库；demo 不会关闭证书校验或伪装安全传输。
