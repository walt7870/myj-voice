# 讯飞原生压缩音频档位设计

## 状态

已确认设计，待实施计划。

## 背景

现有语音 WebSocket 已按连接区分输入、输出格式，支持 `mp3` 和固定 16kHz 的 `pcm16k`。当前实现仍有两个限制：

- 采样率不能由调用方选择，超级拟人 TTS 的 MP3 下行默认仍可能为 24kHz。
- 没有面向带宽敏感设备的 Opus、Speex 原生压缩档位。

目标设备之一是杰理 AC7911BA，其硬件 DAC 使用 16kHz；服务端同时需要兼容其他芯片和场景，不能把能力写死为单一设备型号。

## 目标

- 输入和输出分别通过连接参数选择格式与采样率。
- 格式缺省为 MP3，采样率缺省为 16000Hz。
- 支持 8000Hz、16000Hz，并为讯飞原生支持的场景保留 24000Hz。
- 在讯飞当前接口和账号原生支持时，开放 Opus、Speex 上下行压缩。
- 每条连接建立后固定一种输入档位和一种输出档位。
- 服务端不做解码、编码、重采样或攒包，不增加整体链路等待时间。
- 通过能力发现只向客户端公布真实可用的组合。

## 核心决策

采用讯飞原生直通方案：设备产生的编码数据由现有 JSON + Base64 通道送达服务端，服务端完成连接级校验后立即转发给讯飞；讯飞下发的编码数据保持原始字节和包边界转发给设备。

服务端不得为了“支持”某个档位而加入 PCM 中转、转码、重采样或 Ogg 封装。如果讯飞端点要求与设备标准包不兼容的私有帧头，该档位在当前 provider 下视为不支持。

## 连接协议

设备和浏览器语音 WebSocket 使用相同的四个查询参数：

```text
/api/device/voice?device_id=...&token=...&in_format=opus&in_rate=16000&out_format=opus&out_rate=16000
/api/chat/voice?in_format=pcm&in_rate=16000&out_format=mp3&out_rate=16000
```

| 参数 | 可选值 | 默认值 | 含义 |
| --- | --- | --- | --- |
| `in_format` | `mp3`, `pcm`, `opus`, `speex` | `mp3` | 客户端上行编码 |
| `in_rate` | `8000`, `16000`, `24000` | `16000` | 上行采样率，单位 Hz |
| `out_format` | `mp3`, `pcm`, `opus`, `speex` | `mp3` | 服务端下行编码 |
| `out_rate` | `8000`, `16000`, `24000` | `16000` | 下行采样率，单位 Hz |

参数仅接受精确小写格式值和十进制采样率。连接升级后不允许通过事件修改档位。

项目不存在需要兼容的历史客户端，因此 `pcm16k` 不再作为正式协议值；原来的 `pcm16k` 表达改为 `pcm&in_rate=16000` 或 `pcm&out_rate=16000`。PCM 固定为 signed 16-bit little-endian、单声道，采样率由对应 rate 参数决定。

未传四个参数时，连接档位为：

```text
input:  mp3 / 16000Hz / mono
output: mp3 / 16000Hz / mono
```

这会把当前超级拟人 TTS 的默认 MP3 下行从 24kHz 统一调整为 16kHz。调用方显式传 `out_rate=24000` 且当前 provider 支持时，仍可请求 24kHz。

## 音频档位模型

服务端将格式与采样率组合为类型化连接档位，而不是分别散落判断：

```text
AudioProfile {
    format,
    sample_rate,
    channels: 1,
    bit_depth: 16 | none,
}

VoiceConnectionAudio {
    input: AudioProfile,
    output: AudioProfile,
}
```

`bit_depth=16` 只用于 PCM 元数据；压缩格式不声明 bit depth。连接档位在握手时解析一次，随后显式传给 IAT、TTS 和下行事件构造逻辑，异步任务不得重新读取全局默认值。

## 能力矩阵

协议可识别的值不等于当前 provider 必然支持。每个讯飞 provider 分别声明输入和输出能力，服务端只开放经过真实接口验证的组合。

| 格式 | 协议候选采样率 | 限制 |
| --- | --- | --- |
| PCM | 8k, 16k, 24k | 仅开放讯飞端点原生支持的 raw/L16 档位 |
| MP3 | 8k, 16k, 24k | 仅开放讯飞端点原生支持的 lame 档位 |
| Opus | 8k, 16k | 讯飞标准 TTS 支持下行；标准 IAT 未声明 Opus 上行能力 |
| Speex | 8k, 16k | 8k 为窄带、16k 为宽带，不定义 24k 档位 |

能力判断包含四个维度：方向、provider、格式、采样率。例如某个 TTS provider 支持 `mp3/24k`，不能据此推导 IAT 也支持该档位。

讯飞标准 WebAPI 的方向能力如下：

- 标准 IAT：PCM、MP3、Speex 8k、Speex-WB 16k；官方未声明 Opus 输入。
- 标准 TTS：PCM、MP3、Opus 8k、Opus-WB 16k、标准开源 Speex 8k/16k。
- 标准 TTS 的开源 Speex 参数使用 `speex-org-nb;7` 和 `speex-org-wb;7`。
- 标准 IAT 使用开源 Speex 质量等级 7 时，8k/16k 的 `speex_size` 分别为 38/60 bytes。

当前私有大模型 IAT 和超级拟人 TTS 仍按各自实际验证结果声明能力，不能照搬标准 WebAPI 矩阵。Opus、Speex 必须在当前讯飞端点和账号上完成原生包实测后，才进入对外能力列表。若讯飞只在私有协议中支持某种压缩格式，而其帧结构不能由杰理标准编码器直接产生，则保持禁用。

## 能力发现

`GET /api/device/config` 返回默认档位、查询参数名和按方向分组的可用档位。建议结构：

```json
{
  "audio_profiles": {
    "input": {
      "default": { "format": "mp3", "sample_rate": 16000 },
      "supported": [
        { "format": "mp3", "sample_rates": [16000] },
        { "format": "pcm", "sample_rates": [16000] }
      ]
    },
    "output": {
      "default": { "format": "mp3", "sample_rate": 16000 },
      "supported": [
        { "format": "mp3", "sample_rates": [8000, 16000, 24000] },
        { "format": "pcm", "sample_rates": [16000] }
      ]
    },
    "query": ["in_format", "in_rate", "out_format", "out_rate"]
  }
}
```

示例中的支持列表不是静态承诺，实际响应必须由当前 IAT/TTS provider 的能力矩阵生成。客户端应先读取配置，再选择档位。

## 分包和数据流

继续使用现有事件协议：

```text
audio_stream_start -> audio_stream_chunk -> audio_stream_end
audio_segment
tts_audio_chunk
```

输入格式只在连接查询参数中声明，不在每个事件重复。

- PCM 沿用连续字节流，调用方宜按 20ms 至 40ms 分片；服务端不等待凑满固定时长。
- MP3 沿用连续编码字节流。
- Opus、Speex 的每个 `audio_stream_chunk` 必须包含一个完整的 20ms 标准编码包。
- 服务端保留 Opus、Speex 的事件边界和顺序，一包到达即一包上送。
- 下行 `tts_audio_chunk` 保留讯飞原生编码包边界，一包收到即一包下发。
- 只有在讯飞响应能够提供兼容的完整包边界时，才开放相应 Opus、Speex 下行档位。

压缩包只做传输层校验：Base64 合法、非空、未超过单包上限、序号有序。服务端不通过解码来验证音频内容或实际时长；设备负责按协商档位产生合法的 20ms 包，讯飞负责完成编码语义校验。

`tts_audio_chunk` 必须携带实际档位：

```json
{
  "event_type": "tts_audio_chunk",
  "payload": {
    "audio": "base64 audio bytes",
    "seq": 0,
    "is_last": false,
    "format": "opus",
    "sample_rate": 16000,
    "channels": 1
  }
}
```

PCM 事件额外包含 `"bit_depth": 16`。

## 杰理设备接入档位

AC7911BA 的推荐连接参数为：

```text
in_rate=16000&out_rate=16000
```

具体格式根据设备 SDK 已启用的编码器选择 `opus`、`speex`、`mp3` 或 `pcm`。设备负责将压缩下行解码为 16kHz、16bit、单声道 PCM 后送入硬件 DAC。

该推荐值不进入服务端硬编码。其他芯片可从能力发现结果中选择 8kHz 或 24kHz 档位。

## 讯飞适配边界

IAT 和 TTS provider 适配器负责把 `AudioProfile` 映射为讯飞当前接口的准确字段，但不得改变音频字节：

- raw PCM 映射到讯飞的 raw/L16 参数。
- MP3 映射到讯飞的 lame 参数。
- Opus、Speex 使用经当前端点验证的原生编码标识和标准包约束。
- 不同 provider 的字段和能力分别实现，不用一个通用字符串假定所有端点行为相同。

如果讯飞返回不支持编码、采样率或帧结构的错误，服务端不得静默切换为 MP3/PCM，也不得临时启动转码。

## 错误处理

握手阶段返回 HTTP 400 JSON 错误，不创建讯飞连接：

| 错误码 | 条件 |
| --- | --- |
| `unsupported_audio_format` | 格式值不是 `mp3`, `pcm`, `opus`, `speex` |
| `unsupported_audio_rate` | 采样率不是 8000、16000、24000，或 Speex 使用 24000 |
| `unsupported_audio_profile` | 值本身有效，但当前方向/provider 不支持该组合 |

连接建立后使用现有错误事件通道：

| 错误码 | 条件 |
| --- | --- |
| `invalid_audio_packet` | Base64、空包、单包大小或事件顺序不合法 |
| `upstream_audio_profile_rejected` | 讯飞运行时拒绝已配置档位 |

运行时上游拒绝必须记录 provider、方向、格式、采样率和讯飞错误码，但日志不得包含音频正文、签名 URL 或密钥。

## 延迟约束和可观测性

本次改造不得增加任何音频处理阶段：

- 不解码、不编码、不重采样。
- 不增加等待队列、攒包窗口或按时长聚合。
- 不为每个连接启动额外的音频处理任务。
- 能力校验是连接建立时的内存查表，音频包路径只做常数时间的传输层检查。

增加两个不含音频正文的转发耗时指标：

```text
voice_audio_uplink_relay_duration
voice_audio_downlink_relay_duration
```

分别测量服务端收到完整事件到提交讯飞发送、收到讯飞音频到提交客户端发送的耗时，并以 format、sample_rate、provider 为标签。与现有 MP3 链路 A/B 对比，服务端 P95 转发耗时相对增加不得超过 2ms，且测试中不得出现累计多个压缩包后再发送。

## 测试和验收

实施使用测试驱动方式，覆盖：

1. 四个查询参数的默认值、合法值、精确小写解析和非法值。
2. 格式、采样率、方向、provider 的允许和拒绝组合。
3. PCM 物理参数以及 Speex 8k/16k 限制。
4. IAT/TTS 请求参数快照，确认各 provider 使用准确的讯飞字段。
5. `/api/device/config` 只公布当前 provider 已启用的真实档位。
6. Opus、Speex 上下行逐包转发，验证字节完全一致、边界不变、顺序不变。
7. 非法 Base64、空包、超大包、乱序、上游拒绝和中途断流。
8. `tts_audio_chunk` 的格式、采样率、声道和 PCM bit depth 元数据。
9. 多条并发连接使用不同档位时互不影响。
10. 与 MP3 基线进行服务端上下行 P95 延迟对比。
11. 浏览器体验页、Python SDK、C++ SDK 和现有语音门控回归。

Opus、Speex 上线前必须使用真实讯飞账号分别完成 IAT 和 TTS 烟测。某个方向或采样率未通过，不影响其他已通过档位，但该组合不得出现在能力发现结果中。

## 发布策略

1. 先上线新档位模型、默认 16kHz、能力发现和拒绝逻辑。
2. 保持 raw/lame 已验证档位可用。
3. 对 Opus、Speex 逐个 provider、方向、采样率完成真实讯飞验证；标准 IAT 不开放 Opus 上行。
4. 只有验证通过的组合才加入当前部署的能力配置。
5. 在 JD 服务器部署后执行公网 WebSocket、默认 MP3/16kHz 和已启用压缩档位烟测。

## 非目标

- 不支持连接中途切换格式或采样率。
- 不引入 Ogg 容器或 WebSocket binary frame。
- 不实现服务端音频转码、重采样和设备专用编解码。
- 不保证协议列出的每个候选档位在所有讯飞 provider 上可用。
- 不把 AC7911BA 或任何单一芯片能力写死进服务端业务逻辑。

## 官方依据

- [讯飞语音听写流式 WebAPI](https://www.xfyun.cn/doc/asr/voicedictation/API.html)：声明 8k/16k PCM、Speex/Speex-WB 和 MP3 输入能力。
- [讯飞语音听写音频格式说明](https://www.xfyun.cn/doc/asr/voicedictation/Audio.html)：声明标准开源 Speex 的 `speex_size` 与压缩等级对应关系。
- [讯飞在线语音合成 WebAPI](https://www.xfyun.cn/doc/tts/online_tts/API.html)：声明 raw、lame、Opus/Opus-WB、标准开源 Speex 及 8k/16k 采样率参数。
