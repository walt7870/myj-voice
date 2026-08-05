# Device Button TTS Interrupt Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Add a hardware-button control path that stops the current device TTS turn immediately, notifies the server, preserves order/business analysis, and lets the same connection start listening again.

**Architecture:** Add a connection-scoped turn playback registry backed by `tokio::sync::watch` cancellation. Allocate `turn_id` before downstream handoff, keep the WebSocket reader responsive, and pass cancellation only through reply/LLM/TTS work while the existing analysis task completes. SDKs maintain the current playback identity and a bounded interrupted-turn set so local playback stops before the server acknowledgment and late packets cannot revive it.

**Tech Stack:** Rust 2021, Axum WebSocket, Tokio channels/tasks, serde JSON, SQLite integration tests, Python 3 asyncio/websockets, C++17 POSIX WebSocket demo.

---

## File map

- Create `src/web/turn_interrupt.rs`: connection-scoped active/recent turn registry, cancellation receiver, interrupt status, bounded cleanup.
- Modify `src/web/mod.rs`: parse `turn_id/source`, register turns before handoff, handle `tts_interrupt`, make reply/TTS work cancellation-aware, advertise the protocol.
- Modify `tests/app_tests.rs`: real WebSocket tests for interrupt acknowledgment, late-audio cutoff, next-turn reuse, validation, and business-event preservation.
- Modify `SDKs/python/device_client.py`: device playback state, button interrupt message, late-turn filtering, interactive interrupt hook.
- Modify `SDKs/python/protocol_self_test.py`: deterministic Python SDK state and payload tests.
- Modify `SDKs/cpp/device_client.cpp`: equivalent playback state and `--interrupt-after-first-chunk` demo hook plus self-tests.
- Modify `SDKs/python/README.md`, `SDKs/cpp/README.md`, `SDKs/README.md`, `docs/接口接入说明.md`, and `docs/voice-integrity-test-cases.md`: document the new control event and device acceptance flow.

### Task 1: Build the connection-scoped interrupt registry

**Files:**
- Create: `src/web/turn_interrupt.rs`
- Modify: `src/web/mod.rs:64-68`
- Test: `src/web/turn_interrupt.rs`

- [ ] **Step 1: Write registry tests first**

Create unit tests that register one turn, cancel it, verify the receiver changes, verify a duplicate returns `AlreadyInterrupted`, verify completion returns `AlreadyFinished`, and verify the same `turn_id` cannot be interrupted through a different conversation:

```rust
#[tokio::test]
async fn interrupt_is_scoped_idempotent_and_observable() {
    let registry = TurnInterruptRegistry::default();
    let mut cancellation = registry.register("conversation-a", "turn-1").await;

    assert_eq!(
        registry.interrupt("conversation-a", "turn-1").await,
        InterruptStatus::Interrupted
    );
    cancellation.changed().await.unwrap();
    assert!(*cancellation.borrow());
    assert_eq!(
        registry.interrupt("conversation-a", "turn-1").await,
        InterruptStatus::AlreadyInterrupted
    );
    assert_eq!(
        registry.interrupt("conversation-b", "turn-1").await,
        InterruptStatus::ConversationMismatch
    );

    registry.finish("conversation-a", "turn-1").await;
    assert_eq!(
        registry.interrupt("conversation-a", "turn-1").await,
        InterruptStatus::AlreadyFinished
    );
}
```

- [ ] **Step 2: Run the focused test and confirm RED**

Run: `cargo test web::turn_interrupt::tests::interrupt_is_scoped_idempotent_and_observable -- --exact`

Expected: compilation fails because `TurnInterruptRegistry` and `InterruptStatus` do not exist.

- [ ] **Step 3: Implement the registry with a bounded recent-status cache**

Implement these public interfaces in `src/web/turn_interrupt.rs`:

```rust
use std::{
    collections::{HashMap, VecDeque},
    sync::Arc,
};
use tokio::sync::{watch, Mutex};

const RECENT_TURN_LIMIT: usize = 64;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum InterruptStatus {
    Interrupted,
    AlreadyInterrupted,
    AlreadyFinished,
    ConversationMismatch,
    UnknownTurn,
}

#[derive(Clone, Default)]
pub struct TurnInterruptRegistry {
    inner: Arc<Mutex<RegistryState>>,
}

#[derive(Default)]
struct RegistryState {
    active: HashMap<String, ActiveTurn>,
    recent: VecDeque<RecentTurn>,
}

struct ActiveTurn {
    conversation_id: String,
    interrupted: bool,
    cancellation: watch::Sender<bool>,
}

struct RecentTurn {
    conversation_id: String,
    turn_id: String,
}

impl TurnInterruptRegistry {
    pub async fn register(&self, conversation_id: &str, turn_id: &str) -> watch::Receiver<bool> {
        let (cancellation, receiver) = watch::channel(false);
        self.inner.lock().await.active.insert(
            turn_id.to_string(),
            ActiveTurn {
                conversation_id: conversation_id.to_string(),
                interrupted: false,
                cancellation,
            },
        );
        receiver
    }

    pub async fn interrupt(&self, conversation_id: &str, turn_id: &str) -> InterruptStatus {
        let mut state = self.inner.lock().await;
        if let Some(turn) = state.active.get_mut(turn_id) {
            if turn.conversation_id != conversation_id {
                return InterruptStatus::ConversationMismatch;
            }
            if turn.interrupted {
                return InterruptStatus::AlreadyInterrupted;
            }
            turn.interrupted = true;
            let _ = turn.cancellation.send(true);
            return InterruptStatus::Interrupted;
        }
        if let Some(turn) = state.recent.iter().rev().find(|turn| turn.turn_id == turn_id) {
            return if turn.conversation_id == conversation_id {
                InterruptStatus::AlreadyFinished
            } else {
                InterruptStatus::ConversationMismatch
            };
        }
        InterruptStatus::UnknownTurn
    }

    pub async fn finish(&self, conversation_id: &str, turn_id: &str) {
        let mut state = self.inner.lock().await;
        if state.active.remove(turn_id).is_none() {
            return;
        }
        state.recent.push_back(RecentTurn {
            conversation_id: conversation_id.to_string(),
            turn_id: turn_id.to_string(),
        });
        while state.recent.len() > RECENT_TURN_LIMIT {
            state.recent.pop_front();
        }
    }
}
```

Keep active entries in a `HashMap<String, ActiveTurn>`. Keep at most 64 recent terminal entries in a `VecDeque`; evict the oldest entry when adding the 65th. Store conversation ownership in both collections so a turn from another conversation never receives a cancellation signal.

- [ ] **Step 4: Expose the module and run its tests GREEN**

Add `mod turn_interrupt;` beside `mod admin;` in `src/web/mod.rs`.

Run: `cargo test web::turn_interrupt::tests -- --nocapture`

Expected: all turn interrupt registry tests pass.

- [ ] **Step 5: Commit the isolated registry**

```bash
git add src/web/turn_interrupt.rs src/web/mod.rs
git commit -m "功能：增加播报轮次取消注册表"
```

### Task 2: Add the WebSocket control event and cancellation boundary

**Files:**
- Modify: `src/web/mod.rs:747-759,864-924,1015-1420,1689-2004,2010-2340,2342-2620,3438-3500`
- Test: `tests/app_tests.rs`

- [ ] **Step 1: Write failing WebSocket protocol tests**

Add a test that connects to `/api/device/voice` with mock providers, starts a streamed turn, captures its first `tts_audio_chunk.turn_id`, sends the button event, and asserts the acknowledgment and business events:

```rust
let interrupt = json!({
    "type": "tts_interrupt",
    "conversation_id": conversation_id,
    "turn_id": active_turn_id,
    "source": "button"
});
socket.send(Message::Text(interrupt.to_string().into())).await.unwrap();

let events = receive_until(&mut socket, "analysis_done").await;
let interrupted = events.iter().find(|event| event["event_type"] == "tts_interrupted").unwrap();
assert_eq!(interrupted["payload"]["source"], "button");
assert_eq!(interrupted["payload"]["status"], "interrupted");
assert!(events.iter().any(|event| event["event_type"] == "order_draft"));
assert!(!events.iter().skip_while(|event| event["event_type"] != "tts_interrupted")
    .skip(1).any(|event| event["event_type"] == "tts_audio_chunk"));
```

Add separate assertions for duplicate interrupt (`already_interrupted`), finished turn (`already_finished`), wrong conversation (`bad_request`), missing `turn_id` (`bad_request`), and unsupported `source` (`bad_request`). Add a second streamed turn on the same socket and assert it receives a different `turn_id` and normal TTS. In an order-specific test, interrupt the first turn's confirmation reply, immediately send the next purchase turn, and assert the stored/event order remains first-turn `order_created` before second-turn `order_draft`.

- [ ] **Step 2: Run the new integration tests and confirm RED**

Run: `cargo test --test app_tests device_button_interrupt -- --nocapture`

Expected: failure because `tts_interrupt` is not handled and `tts_interrupted` is never emitted.

- [ ] **Step 3: Extend the input schema and allocate turn IDs before handoff**

Extend `WsInput`:

```rust
struct WsInput {
    #[serde(alias = "type")]
    event_type: String,
    conversation_id: Option<String>,
    turn_id: Option<String>,
    source: Option<String>,
    text: Option<String>,
    audio: Option<String>,
    trace_id: Option<String>,
    client_sent_ms: Option<i64>,
}
```

Create one `TurnInterruptRegistry` and one `Arc<tokio::sync::Mutex<()>>` business sequencer inside `handle_ws`. Generate the `turn_id` in `RecognizedTurn::new`, register it before `handoff`, and change `run_turn_to_channel` to accept the allocated ID instead of generating a second value. Pass the sequencer to every turn and acquire it inside the spawned `analysis` future immediately before `analyze_turn`; release it after the complete analysis/order event vector is produced. Tokio's FIFO mutex acquisition then preserves turn order without holding the lock during LLM or TTS. Apply the same allocation path to text and whole-segment turns so the reader loop never awaits a complete reply/TTS turn.

- [ ] **Step 4: Handle `tts_interrupt` without entering IAT**

Before audio event dispatch, validate `conversation_id`, `turn_id`, and `source == "button"`, then call the registry. Emit:

```rust
StreamEvent::new(
    "tts_interrupted",
    json!({"source": "button", "status": status.as_str()}),
)
.with_context(conversation_id, turn_id)
```

Map `ConversationMismatch`, `UnknownTurn`, missing fields, and unsupported source to explicit messages such as `StreamEvent::error("bad_request", "tts_interrupt turn_id does not belong to this conversation")`. Do not append a user message and do not call `run_turn`.

- [ ] **Step 5: Make only reply and TTS work cancellable**

Pass a cloned `watch::Receiver<bool>` into `run_turn`, `run_llm_with_streaming_super_smart_tts`, and `emit_reply_sentence`. In every LLM/TTS wait loop, select cancellation alongside provider output:

```rust
tokio::select! {
    changed = playback_cancel.changed() => {
        if changed.is_ok() && *playback_cancel.borrow() {
            break;
        }
    }
    maybe_audio = tts_audio_rx.recv() => {
        let Some(chunk) = maybe_audio else {
            tts_audio_done = true;
            continue;
        };
        let chunk = match chunk {
            Ok(chunk) => chunk,
            Err(error) => {
                let code = classify_tts_error(&error);
                emit(StreamEvent::error(code, &friendly_error_message(code, &error.to_string()))
                    .with_context(conversation_id, turn_id)).await;
                tts_audio_done = true;
                continue;
            }
        };
        emit(StreamEvent::tts_audio_chunk(
            STANDARD.encode(chunk.audio),
            0,
            chunk.is_last,
            audio_context.audio.output,
        ).with_context(conversation_id, turn_id)).await;
    }
}
```

For non-streaming synthesis, wrap the synthesis future in the same `tokio::select!`. After cancellation, skip remaining `llm_delta`, `reply_sentence`, `tts_audio_chunk`, assistant-message persistence, and normal `voice_done`; then await the existing `analysis` task and emit its business events plus `latency_metrics`. Always call `registry.finish(conversation_id, turn_id)` through a single cleanup path before the downstream task exits.

- [ ] **Step 6: Advertise the new protocol event**

Add `tts_interrupt` to `voice_ws.client_events` and `tts_interrupted` plus `conversation_ended` to `voice_ws.server_events` in `/api/device/config`.

- [ ] **Step 7: Run focused and full server tests GREEN**

Run:

```bash
cargo test --test app_tests device_button_interrupt -- --nocapture
cargo test
```

Expected: focused button tests pass; the complete Rust suite reports zero failures.

- [ ] **Step 8: Commit the server protocol**

```bash
git add src/web/mod.rs tests/app_tests.rs
git commit -m "功能：支持设备按键打断播报"
```

### Task 3: Add Python SDK button interruption

**Files:**
- Modify: `SDKs/python/device_client.py`
- Modify: `SDKs/python/protocol_self_test.py`
- Modify: `SDKs/python/README.md`

- [ ] **Step 1: Write failing Python state tests**

Add tests for the exact control payload and late-packet filtering:

```python
state = PlaybackState()
state.observe({"event_type": "tts_audio_chunk", "conversation_id": "c1", "turn_id": "t1", "payload": {}})
payload = state.interrupt_payload("button")
assert payload == {
    "type": "tts_interrupt", "conversation_id": "c1", "turn_id": "t1", "source": "button"
}
assert state.should_drop({"event_type": "tts_audio_chunk", "turn_id": "t1"})
assert not state.should_drop({"event_type": "tts_audio_chunk", "turn_id": "t2"})
assert state.interrupt_payload("button") is None
```

- [ ] **Step 2: Run the Python self-test and confirm RED**

Run: `SDKs/python/.venv/bin/python SDKs/python/protocol_self_test.py`

Expected: import or name failure for `PlaybackState`.

- [ ] **Step 3: Implement `PlaybackState` and the button API**

Add a bounded 64-entry interrupted-turn set/order, current `conversation_id/turn_id`, and these methods:

```python
from collections import deque

class PlaybackState:
    def __init__(self) -> None:
        self.conversation_id: str | None = None
        self.turn_id: str | None = None
        self.interrupted: set[str] = set()
        self.interrupted_order: deque[str] = deque()

    def observe(self, event: dict) -> None:
        if event.get("event_type") != "tts_audio_chunk":
            return
        turn_id = event.get("turn_id")
        conversation_id = event.get("conversation_id")
        if isinstance(turn_id, str) and isinstance(conversation_id, str):
            self.turn_id = turn_id
            self.conversation_id = conversation_id

    def should_drop(self, event: dict) -> bool:
        turn_id = event.get("turn_id")
        return isinstance(turn_id, str) and turn_id in self.interrupted

    def interrupt_payload(self, source: str = "button") -> dict | None:
        if not self.turn_id or not self.conversation_id or self.turn_id in self.interrupted:
            return None
        self.interrupted.add(self.turn_id)
        self.interrupted_order.append(self.turn_id)
        while len(self.interrupted_order) > 64:
            self.interrupted.discard(self.interrupted_order.popleft())
        return {
            "type": "tts_interrupt",
            "conversation_id": self.conversation_id,
            "turn_id": self.turn_id,
            "source": source,
        }

async def interrupt_tts_from_button(socket, playback: PlaybackState,
                                    stream_player: StreamPlayer) -> bool:
    payload = playback.interrupt_payload("button")
    if payload is None:
        return False
    stream_player.stop_now()
    await socket.send(json.dumps(payload))
    return True
```

Add `StreamPlayer.stop_now()` as `self.close()` while retaining the `enabled` flag. Check `should_drop` before decoding or validating TTS sequence data. Treat `tts_interrupted` as a terminal playback event, but do not terminate the connection. When a non-dropped TTS chunk from a new turn arrives and the player process is absent, call `stream_player.start()` again before writing. Add `--interrupt-after-first-chunk` as a demo/test hook that invokes the same method after the first playable chunk.

- [ ] **Step 4: Run Python checks GREEN**

Run:

```bash
SDKs/python/.venv/bin/python SDKs/python/protocol_self_test.py
SDKs/python/.venv/bin/python -m py_compile SDKs/python/device_client.py
```

Expected: self-test passes and compilation exits zero.

- [ ] **Step 5: Commit the Python SDK change**

```bash
git add SDKs/python/device_client.py SDKs/python/protocol_self_test.py SDKs/python/README.md
git commit -m "功能：增加 Python 设备按键打断接口"
```

### Task 4: Add C++ SDK button interruption

**Files:**
- Modify: `SDKs/cpp/device_client.cpp`
- Modify: `SDKs/cpp/README.md`

- [ ] **Step 1: Add failing C++ self-test assertions**

Extend `run_self_test()` to require a `PlaybackState` that produces the exact JSON payload, drops the interrupted turn, accepts the next turn, and treats a repeated button event as a no-op:

```cpp
PlaybackState playback;
playback.observe("{\"event_type\":\"tts_audio_chunk\",\"conversation_id\":\"c1\",\"turn_id\":\"t1\",\"payload\":{}}");
if (playback.interrupt_payload() != "{\"type\":\"tts_interrupt\",\"conversation_id\":\"c1\",\"turn_id\":\"t1\",\"source\":\"button\"}")
    throw std::runtime_error("button interrupt payload self-test failed");
if (!playback.should_drop("t1") || playback.should_drop("t2") || !playback.interrupt_payload().empty())
    throw std::runtime_error("button interrupt state self-test failed");
```

- [ ] **Step 2: Run the C++ self-test and confirm RED**

Run: `SDKs/cpp/build.sh && SDKs/cpp/device_client --self-test`

Expected: compilation fails because `PlaybackState` does not exist.

- [ ] **Step 3: Implement the C++ playback state and demo trigger**

Implement `PlaybackState` with current IDs, a `std::set` plus `std::deque` bounded to 64 interrupted turns, `observe`, `should_drop`, and `interrupt_payload`. Add `--interrupt-after-first-chunk`; after receiving the first TTS chunk, call `player.close()`, send the control JSON once, and continue reading until `tts_interrupted` plus required business events. Before writing the first non-dropped chunk of a later turn, when `--play` is enabled call `player.start(args.play_cmd.empty() ? default_play_command(args.out_format, args.out_rate) : args.play_cmd)` again. Never pass a dropped chunk to `TtsSequenceValidator` or `TtsOrderedAudio`.

Use this concrete state object:

```cpp
#include <deque>

class PlaybackState {
  public:
    void observe(const std::string& message) {
        if (message.find("\"event_type\":\"tts_audio_chunk\"") == std::string::npos) return;
        conversation_id_ = extract_json_string(message, "conversation_id");
        turn_id_ = extract_json_string(message, "turn_id");
    }

    bool should_drop(const std::string& turn_id) const {
        return interrupted_.count(turn_id) != 0;
    }

    std::string interrupt_payload() {
        if (conversation_id_.empty() || turn_id_.empty() || should_drop(turn_id_)) return {};
        interrupted_.insert(turn_id_);
        interrupted_order_.push_back(turn_id_);
        while (interrupted_order_.size() > 64) {
            interrupted_.erase(interrupted_order_.front());
            interrupted_order_.pop_front();
        }
        return "{\"type\":\"tts_interrupt\",\"conversation_id\":\"" +
               json_escape(conversation_id_) + "\",\"turn_id\":\"" +
               json_escape(turn_id_) + "\",\"source\":\"button\"}";
    }

  private:
    std::string conversation_id_;
    std::string turn_id_;
    std::set<std::string> interrupted_;
    std::deque<std::string> interrupted_order_;
};
```

- [ ] **Step 4: Run the C++ SDK checks GREEN**

Run: `SDKs/cpp/build.sh && SDKs/cpp/device_client --self-test`

Expected: build succeeds and prints `C++ SDK protocol self-test: PASS`.

- [ ] **Step 5: Commit the C++ SDK change**

```bash
git add SDKs/cpp/device_client.cpp SDKs/cpp/README.md
git commit -m "功能：增加 C++ 设备按键打断接口"
```

### Task 5: Complete contract documentation and end-to-end verification

**Files:**
- Modify: `SDKs/README.md`
- Modify: `docs/接口接入说明.md`
- Modify: `docs/voice-integrity-test-cases.md`
- Modify: `docs/规划迭代记录.md`

- [ ] **Step 1: Add the exact device contract to documentation**

Document the request/ack JSON, button ISR rule, local-first stop behavior, `turn_id` late-packet filter, `voice_done` difference, automatic pickup recovery, order-event preservation, and absence of board HAL source in this repository. Add an acceptance case: interrupt during an order confirmation reply, speak a second request immediately, and verify both the order event and next ASR turn.

- [ ] **Step 2: Run static contract and formatting checks**

Run:

```bash
rg -n 'tts_interrupt|tts_interrupted|interrupt_tts_from_button' src SDKs docs
git diff --check
```

Expected: all three contract names appear in implementation, SDKs, and documentation; `git diff --check` prints nothing.

- [ ] **Step 3: Run all local verification gates**

Run:

```bash
cargo test
npm run voice:check
SDKs/python/.venv/bin/python SDKs/python/protocol_self_test.py
SDKs/cpp/build.sh
SDKs/cpp/device_client --self-test
curl -fsS http://127.0.0.1:8787/api/health
```

Expected: all Rust tests and voice integrity checks pass; both SDK self-tests pass; local health returns `{"service":"mjy-voice-shop-rs","status":"ok"}`.

- [ ] **Step 4: Exercise the running local WebSocket**

Restart only the local service after a successful build. Run the Python demo with `--interrupt-after-first-chunk`, verify `tts_interrupted.status=interrupted`, then reuse the same conversation/socket for a second text or PCM turn. Confirm no interrupted-turn audio is written after the button event and the second turn reaches `voice_done`.

- [ ] **Step 5: Commit documentation and verification updates**

```bash
git add SDKs/README.md docs/接口接入说明.md docs/voice-integrity-test-cases.md docs/规划迭代记录.md
git commit -m "文档：补充设备按键打断接入说明"
```

No production deployment is part of this plan. Publishing requires a separate explicit user request and the project deployment gate.
