# 嵌入式设备按键打断播报设计

## 1. 背景

当前设备语音链路支持普通语音轮次和基于 `interrupt_audio_segment` 的打断词识别，但后者仍依赖麦克风音频、云端 IAT 和固定打断词。嵌入式设备已有实体按钮，期望用户在播报期间按下按钮后立即停止当前播报，并自动进入下一轮拾音。

按键打断不应伪造音频，也不应复用语音打断识别协议。它需要独立的控制事件，使设备可以本地立即停播，同时通知服务端停止该轮剩余回复和 TTS 输出。

## 2. 目标

- 播报期间按下实体按钮后，设备立即停止解码和 DAC 播放。
- 设备清空该轮所有待播音频，并丢弃之后到达的同一 `turn_id` 音频。
- 设备在播放链路排空后自动恢复普通拾音，无需再次按键。
- 服务端收到控制事件后停止该轮尚未完成的 LLM 回复和 TTS 输出。
- 打断只影响回复和播报，不取消已经启动的商品分析、订单创建、退款等业务动作。
- 同一 WebSocket、`conversation_id` 和设备身份保持不变，下一轮语音可以继续当前会话。
- 重复按键、过期 `turn_id`、非播报状态按键均可安全幂等处理。

## 3. 非目标

- 本次不实现“检测到任意人声就打断”。
- 本次不实现播报期间的回声消除、双工普通 ASR 或本地关键词唤醒。
- 本次不改变音频格式、编解码方式或 WebSocket 鉴权协议。
- 本仓库不包含具体开发板的 GPIO、按键消抖、MP3 decoder 或 DAC HAL 实现；只提供服务端协议、SDK 接口、状态机要求和自动化验证。

## 4. 方案选择

### 方案 A：只在设备本地停播

按键后设备停止播放器并忽略旧轮次音频，服务端继续生成和发送剩余内容。优点是设备响应最快、服务端无需改动；缺点是继续消耗模型、TTS 和网络资源，服务端状态也不知道用户已经打断。

### 方案 B：设备本地停播并通知服务端取消（采用）

设备先本地立即停播，再通过原 WebSocket 发送独立控制事件。服务端取消该轮回复/TTS 输出并返回确认。该方案兼顾按键响应速度、服务端状态一致性和资源释放。

### 方案 C：断开 WebSocket 后重连

断开连接通常能终止旧输出，但会引入重新鉴权、音频能力协商、会话恢复和错误重试，且无法精确表达“只打断播报”。不采用。

## 5. 线级协议

设备发送：

```json
{
  "type": "tts_interrupt",
  "conversation_id": "device-session-001",
  "turn_id": "当前正在播报的 turn_id",
  "source": "button"
}
```

字段要求：

- `conversation_id` 必须属于当前设备连接。
- `turn_id` 必须来自当前连接已经收到的服务端事件，不能由设备猜测。
- `source` 本期只接受 `button`，为后续其他本地控制来源留出扩展空间。
- 事件不携带音频，不进入 IAT，不触发新的用户消息和订单意图。

服务端确认：

```json
{
  "event_type": "tts_interrupted",
  "conversation_id": "device-session-001",
  "turn_id": "被打断的 turn_id",
  "payload": {
    "source": "button",
    "status": "interrupted"
  }
}
```

若目标轮次已经结束或已被打断，服务端仍返回 `tts_interrupted`，但 `payload.status` 为 `already_finished` 或 `already_interrupted`。缺少字段、连接归属不符或非法来源返回 `bad_request`，且不得影响其他轮次。

## 6. 设备状态机

设备至少维护以下状态：

```text
LISTENING -> WAITING_REPLY -> TTS_PREBUFFER -> PLAYING_TTS -> LISTENING
```

按键处理只在 `TTS_PREBUFFER` 或 `PLAYING_TTS` 生效：

1. GPIO ISR 完成消抖后只向音频/网络 owner task 投递 `BUTTON_INTERRUPT`，不得在 ISR 内执行 WebSocket、内存释放或 decoder 操作。
2. owner task 原子记录 `interrupted_turn_id`。
3. 立即停止 decoder 和 DAC，清空当前音频、预缓冲、待播 seq 和播放队列。
4. 后续收到相同 `turn_id` 的 `tts_audio_chunk`、`reply_sentence` 和 `voice_done` 时，不得重新启动旧播报或覆盖当前拾音状态。
5. 向服务端发送 `tts_interrupt`。网络发送失败不回滚本地停播；设备记录错误并继续恢复拾音。
6. decoder/DAC 确认排空后立即切回 `LISTENING`。不设置固定 500ms 延迟；若具体硬件存在明显声学拖尾，应以实测最短 guard time 作为板端参数，而不是协议要求。
7. 非播报状态按键按产品既有语义处理，本协议不赋予其新的对话动作。

设备必须按 `conversation_id + turn_id + seq` 管理下行数据。不能只设置一个全局“忽略所有 TTS”标志，否则会误丢下一轮播报。

## 7. 服务端并发与取消边界

每条设备 WebSocket 维护连接级活动轮次表。ASR final 移交业务轮次前生成 `turn_id`，并登记一个仅控制回复/TTS 的取消句柄。WebSocket reader 始终可以继续接收控制事件，不能被正在运行的业务轮次阻塞。

收到合法 `tts_interrupt` 后：

- 将目标轮次标记为 interrupted，触发回复/TTS cancellation token。
- 停止继续读取 LLM delta，终止尚未完成的 TTS producer，过滤已经排队但尚未发送的 `reply_sentence` 和 `tts_audio_chunk`。
- 不撤销已经发送的音频；设备本地丢弃逻辑是即时停播的最终保障。
- 不取消独立运行的订单分析任务，并继续发送 `intent_analysis`、`product_matches`、`order_draft`、`order_created`、`order_refunded`、`analysis_done` 等业务事件。
- 被打断轮次不再发送普通 `voice_done` 来驱动设备恢复；以 `tts_interrupted` 作为该轮播放终止确认。设备本地恢复不依赖确认事件。
- 轮次完成、连接关闭或取消确认后清理活动轮次表，防止句柄和 `turn_id` 无限累积。

同一会话的新 ASR 轮次可以在旧播报取消后立即开始。业务分析保持现有事件语义；如果前一轮仍有订单动作在执行，服务端必须保证同一 `conversation_id` 的订单分析/写动作按 turn 顺序提交，避免按钮打断造成并发订单状态竞争。

## 8. SDK 接口

Python 与 C++ SDK 增加等价方法：

```text
interrupt_tts_from_button()
```

调用方无需构造 JSON。SDK 负责读取当前播放 `conversation_id/turn_id`、执行本地队列清理、发送控制事件和维护 interrupted turn 集合。

C++ demo 增加可测试的命令或 stdin 控制入口模拟实体按钮；Python demo提供对应参数/交互入口。真实固件只需在消抖后的按键 task 中调用同一状态机动作。

## 9. 错误处理

- WebSocket 未连接：本地仍停播并恢复拾音，记录一次可观测错误，不循环重发旧轮次事件。
- 服务端确认超时：不恢复旧音频；下一轮仍可进行，连接健康由既有心跳/重连逻辑判断。
- `turn_id` 不匹配：服务端返回 `bad_request`，不得取消当前真实轮次。
- 按键抖动或重复点击：首次调用执行取消，后续调用幂等返回，不重复清空下一轮状态。
- TTS 与按键同时结束：设备以本地播放 owner 的串行事件顺序处理；两种顺序最终都必须得到空队列和 `LISTENING`。
- 订单事件晚于打断确认：设备继续处理订单状态，但不重新播放旧轮次音频。

## 10. 验证标准

服务端自动化：

- 合法按键事件返回 `tts_interrupted`，后续不再输出目标轮次的播报事件。
- 过期、重复、跨会话和非法来源事件按协议处理。
- 打断后订单分析与订单创建仍能完成。
- 打断后同一连接可开始新 ASR 轮次并正常收到下一轮 TTS。
- 并发边界测试覆盖“最后一个 TTS 包与按键同时到达”。

SDK 自动化：

- 按键后 decoder/DAC stop 只执行一次，所有队列清空。
- 相同 `turn_id` 的迟到包全部丢弃，下一 `turn_id` 正常播放。
- 断网、重复按键和确认超时不会恢复旧播报。

设备人工验收：

- 播报过程中任意时刻按键，听感上立即停止。
- 按键后直接说下一句话，无需再次按键，能够进入下一轮 ASR。
- 连续执行 20 次打断，无旧播报复活、无播放重叠、无 WebSocket 重连风暴。
- 在确认下单回复期间按键，订单仍按原意图完成且后台状态正确。
- 记录按钮按下到 DAC 静音、按钮按下到恢复拾音、服务端收到取消到停止输出的 P50/P95。

## 11. 交付边界

本仓库交付 Rust 服务、Python/C++ SDK、协议文档和测试。本仓库当前没有实际板端 GPIO/decoder/DAC 固件源码，因此实体按钮最终接线、GPIO 消抖和音频 HAL 调用需要在设备固件仓库完成；服务端和 SDK 完成后可提供明确的调用入口与事件时序供板端接入。
