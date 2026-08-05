# Embedded Client Latency P0 Implementation Plan

> **For Codex:** REQUIRED SUB-SKILL: Use superpowers:executing-plans to implement this plan task-by-task.

**Goal:** Reduce the embedded demo's measured key-up-to-audio latency from about 6.2 seconds by removing avoidable client-side stalls while preserving the current PCM/JSON/WSS protocol.

**Architecture:** Keep the existing synchronous WebSocket writer for this first low-risk patch, but limit each blocking TLS read slice to 20ms so the writer can acquire the shared TLS mutex promptly. Stop the recorder before draining a bounded tail snapshot, send 80ms PCM packets, and begin long TTS playback after 4KB. Add timing logs so board tests can separate mutex wait, socket write, tail drain, and decoder start.

**Tech Stack:** Embedded C, mbedTLS WebSocket transport, JieLi audio/net_buf APIs, host-side C11 policy contract test.

---

### Task 1: Add a failing latency-policy contract

**Files:**
- Create: `client/tests/latency_policy_test.c`
- Create: `client/tests/test_latency_policy.sh`
- Test: `client/tests/test_latency_policy.sh`

**Steps:**
1. Add compiler-independent source assertions plus optional C11 compile-time assertions for input sample rate, two-frame PCM aggregation, 20ms TLS read slices, three-frame tail drain, and 4KB TTS prebuffer.
2. Run `sh client/tests/test_latency_policy.sh` and confirm it fails against the current constants. Run the optional C11 assertions with `VSHOP_RUN_C_CONTRACT=1 CC=<available compiler>` when the local toolchain is usable.

### Task 2: Apply the P0 device policy

**Files:**
- Modify: `client/voice_shop_config.h`
- Modify: `client/voice_shop_ws.c`
- Modify: `client/voice_shop_demo.c`
- Modify: `client/voice_shop_tts.c`
- Test: `client/tests/test_latency_policy.sh`

**Steps:**
1. Bind recorder initialization to `VOICE_SHOP_IN_RATE`, aggregate two 40ms PCM frames, define a 20ms runtime TLS read slice and a three-frame tail limit, and set both TTS thresholds to 4KB.
2. Apply the runtime TLS read timeout after WebSocket upgrade and record mutex-wait/write timing for slow sends.
3. On key-up, snapshot pending PCM, stop the producer, drain at most the configured snapshot limit, then send `audio_stream_end`.
4. Change `client_sent_ms` to 64-bit and add end-to-end client phase logs.
5. Run `sh client/tests/test_latency_policy.sh` and confirm it passes.

### Task 3: Document behavior and expected impact

**Files:**
- Modify: `mjy-voice-shop-rs/docs/接口接入说明.md`
- Modify: `mjy-voice-shop-rs/docs/规划迭代记录.md`

**Steps:**
1. Document the P0 embedded parameters, compatibility boundary, board validation metrics, and fallback values.
2. Record that this is the mutex-slice transition rather than the final single-I/O-owner transport architecture.

### Task 4: Verify the review build

**Files:**
- Verify: `client/voice_shop_config.h`
- Verify: `client/voice_shop_ws.c`
- Verify: `client/voice_shop_demo.c`
- Verify: `client/voice_shop_tts.c`
- Verify: the two documentation files above

**Steps:**
1. Run `sh client/tests/test_latency_policy.sh`.
2. Run source checks for recorder-stop ordering, runtime timeout application, timing logs, and 64-bit timestamp formatting.
3. Run `git diff --check` for tracked docs and inspect all modified/untracked files without staging or committing.
4. Report expected board effect separately from measured results; do not claim a device latency number until firmware is flashed and logs are collected.
