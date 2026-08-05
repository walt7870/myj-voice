#!/usr/bin/env python3
import asyncio
import importlib.util
import json
import struct
import tempfile
import urllib.parse
from pathlib import Path

MODULE = Path(__file__).with_name("device_client.py")
spec = importlib.util.spec_from_file_location("mjy_device_client", MODULE)
client = importlib.util.module_from_spec(spec)
spec.loader.exec_module(client)


def must_fail(fn, text: str) -> None:
    try:
        fn()
    except (RuntimeError, SystemExit, ValueError) as error:
        assert text in str(error), (text, error)
    else:
        raise AssertionError(f"expected failure containing: {text}")


def legacy_connect(uri, *, max_size=1024, **kwargs):
    del uri, max_size, kwargs


def modern_connect(uri, *, max_size=1024, proxy=True, **kwargs):
    del uri, max_size, proxy, kwargs


assert client.websocket_connect_options(legacy_connect) == {"max_size": None}
assert client.websocket_connect_options(modern_connect) == {"max_size": None, "proxy": None}


url = client.ws_url("https://example.test/base", "DOLL A/1", "tok+en=?", "pcm", 8000, "mp3", 24000)
query = urllib.parse.parse_qs(urllib.parse.urlparse(url).query)
assert query == {
    "device_id": ["DOLL A/1"], "token": ["tok+en=?"],
    "in_format": ["pcm"], "in_rate": ["8000"],
    "out_format": ["mp3"], "out_rate": ["24000"],
}

must_fail(lambda: client.validate_profile("speex", 24000, "in"), "unsupported for Speex")
must_fail(lambda: list(client.audio_chunks(b"opus", "opus", 16000)), "packet boundaries")
must_fail(lambda: list(client.audio_chunks(b"x", "pcm", 16000)), "complete signed 16-bit")
must_fail(lambda: list(client.audio_chunks(bytes(39), "speex", 8000)), "whole quality-7 packets")
assert [len(x) for x in client.audio_chunks(bytes(76), "speex", 8000)] == [38, 38]
assert [len(x) for x in client.audio_chunks(bytes(120), "speex", 16000)] == [60, 60]

sequence = client.TtsSequenceValidator()
base = {"format": "mp3", "sample_rate": 24000, "channels": 1, "seq": 0, "is_last": False, "audio": "AA=="}
assert client.decode_tts_audio(base, "mp3", 24000, sequence) == b"\x00"
must_fail(lambda: client.decode_tts_audio({**base, "seq": 1, "audio": "***"}, "mp3", 24000, sequence), "Only base64")
must_fail(lambda: client.validate_tts_metadata({**base, "channels": 2}, "mp3", 24000), "metadata mismatch")
must_fail(lambda: client.validate_tts_metadata({**base, "format": "pcm", "bit_depth": 8}, "pcm", 24000), "bit_depth")
sequence.validate({"seq": 0, "is_last": True})
must_fail(lambda: sequence.validate({"seq": 0, "is_last": False}), "out of order")

interleaved = client.TtsOrderedAudio()
assert interleaved.accept({"seq": 1, "is_last": False}, b"B") == []
assert interleaved.accept({"seq": 0, "is_last": False}, b"A") == [b"A"]
assert interleaved.accept({"seq": 1, "is_last": True}, b"b") == []
assert interleaved.accept({"seq": 0, "is_last": True}, b"a") == [b"a", b"B", b"b"]
interleaved.finish()
must_fail(lambda: interleaved.accept({"seq": 1, "is_last": False}, b"!"), "out of order")
incomplete = client.TtsOrderedAudio()
incomplete.accept({"seq": 2, "is_last": True}, b"lost")
must_fail(incomplete.finish, "incomplete")
packed = client.encode_output_chunk("opus", b"abc") + client.encode_output_chunk("opus", b"de")
assert struct.unpack_from("<I", packed, 0)[0] == 3 and packed[4:7] == b"abc"
assert struct.unpack_from("<I", packed, 7)[0] == 2 and packed[11:13] == b"de"
assert client.output_path_for_profile("/tmp/a.b/reply.bin", "opus") == Path("/tmp/a.b/reply.opuspack")

assert client.resolve_device_secret("http://127.0.0.1:8787", "DOLL-0001", None) == "demo-secret"
must_fail(
    lambda: client.resolve_device_secret("https://example.test/myj-voice-shop", "DOLL-0001", None),
    "--device-secret",
)
must_fail(
    lambda: client.resolve_device_secret(
        "https://example.test/myj-voice-shop", "DOLL-0001", "demo-secret"
    ),
    "local-only",
)
assert client.resolve_device_secret(
    "https://example.test/myj-voice-shop", "DEVICE-PROD-0001", "independent-secret"
) == "independent-secret"


playback = client.PlaybackState()
assert playback.interrupt_payload() is None
playback.observe({
    "event_type": "tts_audio_chunk",
    "conversation_id": "conversation-1",
    "turn_id": "turn-1",
    "payload": {},
})
payload = playback.interrupt_payload()
assert payload == {
    "type": "tts_interrupt",
    "conversation_id": "conversation-1",
    "turn_id": "turn-1",
    "source": "button",
}
assert playback.should_drop({
    "event_type": "reply_sentence", "conversation_id": "conversation-1", "turn_id": "turn-1"
})
assert playback.should_drop({
    "event_type": "tts_audio_chunk", "conversation_id": "conversation-1", "turn_id": "turn-1"
})
assert playback.should_drop({
    "event_type": "voice_done", "conversation_id": "conversation-1", "turn_id": "turn-1"
})
assert not playback.should_drop({
    "event_type": "tts_interrupted", "conversation_id": "conversation-1", "turn_id": "turn-1"
})
assert not playback.should_drop({"event_type": "tts_audio_chunk", "turn_id": "turn-1"})
assert not playback.should_drop({
    "event_type": "tts_audio_chunk", "conversation_id": "conversation-1", "turn_id": "turn-2"
})
assert playback.interrupt_payload() is None

pair_playback = client.PlaybackState()
pair_playback.observe({
    "event_type": "tts_audio_chunk",
    "conversation_id": "conversation-a",
    "turn_id": "shared-turn",
    "payload": {},
})
assert pair_playback.interrupt_payload() == {
    "type": "tts_interrupt",
    "conversation_id": "conversation-a",
    "turn_id": "shared-turn",
    "source": "button",
}
assert pair_playback.should_drop({
    "event_type": "tts_audio_chunk",
    "conversation_id": "conversation-a",
    "turn_id": "shared-turn",
})
assert not pair_playback.should_drop({
    "event_type": "tts_audio_chunk",
    "conversation_id": "conversation-b",
    "turn_id": "shared-turn",
})
pair_playback.observe({
    "event_type": "tts_audio_chunk",
    "conversation_id": "conversation-b",
    "turn_id": "shared-turn",
    "payload": {},
})
assert pair_playback.interrupt_payload() == {
    "type": "tts_interrupt",
    "conversation_id": "conversation-b",
    "turn_id": "shared-turn",
    "source": "button",
}

terminal_playback = client.PlaybackState()
terminal_playback.observe({
    "event_type": "tts_audio_chunk",
    "conversation_id": "terminal-a",
    "turn_id": "shared-terminal-turn",
    "payload": {},
})
for unrelated_terminal in (
    {
        "event_type": "conversation_ended",
        "conversation_id": "terminal-b",
        "turn_id": "shared-terminal-turn",
        "payload": {},
    },
    {
        "event_type": "conversation_ended",
        "conversation_id": "terminal-b",
        "turn_id": "other-terminal-turn",
        "payload": {},
    },
    {"event_type": "conversation_ended", "payload": {}},
):
    terminal_playback.observe(unrelated_terminal)
    assert (terminal_playback.conversation_id, terminal_playback.turn_id) == (
        "terminal-a", "shared-terminal-turn"
    )
terminal_playback.observe({
    "event_type": "conversation_ended",
    "conversation_id": "terminal-a",
    "turn_id": "shared-terminal-turn",
    "payload": {},
})
assert terminal_playback.conversation_id is None and terminal_playback.turn_id is None

bounded_playback = client.PlaybackState()
for index in range(66):
    bounded_playback.observe({
        "event_type": "tts_audio_chunk",
        "conversation_id": f"bounded-conversation-{index}",
        "turn_id": "shared-bounded-turn",
        "payload": {},
    })
    assert bounded_playback.interrupt_payload() is not None
assert len(bounded_playback.interrupted_order) == 64
assert not bounded_playback.should_drop({
    "event_type": "tts_audio_chunk",
    "conversation_id": "bounded-conversation-0",
    "turn_id": "shared-bounded-turn",
})
assert bounded_playback.should_drop({
    "event_type": "tts_audio_chunk",
    "conversation_id": "bounded-conversation-65",
    "turn_id": "shared-bounded-turn",
})

playback.observe({
    "event_type": "tts_audio_chunk",
    "conversation_id": "conversation-2",
    "turn_id": "turn-new",
    "payload": {},
})
playback.observe({
    "event_type": "tts_interrupted",
    "conversation_id": "conversation-1",
    "turn_id": "turn-66",
    "payload": {"source": "button", "status": "interrupted"},
})
assert playback.turn_id == "turn-new"
playback.observe({
    "event_type": "tts_interrupted",
    "conversation_id": "conversation-2",
    "turn_id": "turn-new",
    "payload": {"source": "button", "status": "already_finished"},
})
assert playback.turn_id is None and playback.conversation_id is None


class FakeSocket:
    def __init__(self) -> None:
        self.sent: list[dict] = []

    async def send(self, raw: str) -> None:
        self.sent.append(json.loads(raw))


class FakePlayer:
    def __init__(self) -> None:
        self.stop_count = 0

    def stop_now(self) -> None:
        self.stop_count += 1


async def verify_button_interrupt() -> None:
    socket = FakeSocket()
    player = FakePlayer()
    state = client.PlaybackState()
    state.observe({
        "event_type": "tts_audio_chunk",
        "conversation_id": "conversation-button",
        "turn_id": "turn-button",
        "payload": {},
    })
    assert await client.interrupt_tts_from_button(socket, state, player)
    assert not await client.interrupt_tts_from_button(socket, state, player)
    assert player.stop_count == 1
    assert socket.sent == [{
        "type": "tts_interrupt",
        "conversation_id": "conversation-button",
        "turn_id": "turn-button",
        "source": "button",
    }]

    class FailingSocket:
        async def send(self, raw: str) -> None:
            del raw
            raise ConnectionError("offline")

    offline_state = client.PlaybackState()
    offline_state.observe({
        "event_type": "tts_audio_chunk",
        "conversation_id": "conversation-offline",
        "turn_id": "turn-offline",
        "payload": {},
    })
    offline_state.audio_chunks.append(b"queued")
    offline_state.ordered.accept({"seq": 1, "is_last": False}, b"buffered")
    offline_player = FakePlayer()
    assert await client.interrupt_tts_from_button(FailingSocket(), offline_state, offline_player)
    assert offline_player.stop_count == 1
    assert offline_state.turn_id is None and offline_state.audio_chunks == []
    assert offline_state.ordered.buffered == {}


class FakeEventSocket:
    def __init__(self, events: list[dict]) -> None:
        self.events = iter(events)
        self.sent: list[dict] = []
        self.received_count = 0

    async def recv(self) -> str:
        event = next(self.events)
        self.received_count += 1
        return json.dumps(event)

    async def send(self, raw: str) -> None:
        self.sent.append(json.loads(raw))


async def verify_late_packet_is_dropped_before_decode() -> None:
    audio_payload = {
        "format": "mp3",
        "sample_rate": 16000,
        "channels": 1,
        "seq": 0,
        "is_last": True,
        "audio": "Qg==",
    }
    socket = FakeEventSocket([
        {
            "event_type": "tts_audio_chunk",
            "conversation_id": "conversation-receive",
            "turn_id": "turn-old",
            "payload": {**audio_payload, "is_last": False, "audio": "QQ=="},
        },
        {
            "event_type": "tts_audio_chunk",
            "conversation_id": "conversation-receive",
            "turn_id": "turn-old",
            "payload": {**audio_payload, "audio": "not-base64"},
        },
        {
            "event_type": "tts_interrupted",
            "conversation_id": "conversation-receive",
            "turn_id": "turn-old",
            "payload": {"source": "button", "status": "interrupted"},
        },
        {
            "event_type": "analysis_done",
            "conversation_id": "conversation-receive",
            "turn_id": "turn-old",
            "payload": {},
        },
        {
            "event_type": "latency_metrics",
            "conversation_id": "conversation-receive",
            "turn_id": "turn-old",
            "payload": {"total_ms": 10},
        },
        {
            "event_type": "tts_audio_chunk",
            "conversation_id": "conversation-receive",
            "turn_id": "turn-new",
            "payload": audio_payload,
        },
        {
            "event_type": "voice_done",
            "conversation_id": "conversation-receive",
            "turn_id": "turn-new",
            "payload": {},
        },
        {
            "event_type": "conversation_ended",
            "conversation_id": "conversation-receive",
            "turn_id": "turn-new",
            "payload": {},
        },
        {
            "event_type": "latency_metrics",
            "conversation_id": "conversation-receive",
            "turn_id": "turn-new",
            "payload": {"total_ms": 20},
        },
    ])
    with tempfile.TemporaryDirectory() as directory:
        output = Path(directory) / "reply.mp3"
        await client.receive_events(
            socket, output, False, "mp3", 16000,
            interrupt_after_first_chunk=True,
            one_shot=False,
        )
        assert output.read_bytes() == b"B"
    assert socket.sent == [{
        "type": "tts_interrupt",
        "conversation_id": "conversation-receive",
        "turn_id": "turn-old",
        "source": "button",
    }]


async def verify_one_shot_interrupt_finishes_without_voice_done() -> None:
    socket = FakeEventSocket([
        {
            "event_type": "tts_audio_chunk",
            "conversation_id": "conversation-one-shot",
            "turn_id": "turn-one-shot",
            "payload": {
                "format": "mp3",
                "sample_rate": 16000,
                "channels": 1,
                "seq": 0,
                "is_last": False,
                "audio": "QQ==",
            },
        },
        {
            "event_type": "tts_interrupted",
            "conversation_id": "conversation-one-shot",
            "turn_id": "turn-one-shot",
            "payload": {"source": "button", "status": "interrupted"},
        },
        {
            "event_type": "conversation_ended",
            "conversation_id": "conversation-one-shot",
            "turn_id": "turn-one-shot",
            "payload": {},
        },
        {
            "event_type": "analysis_done",
            "conversation_id": "conversation-one-shot",
            "turn_id": "turn-one-shot",
            "payload": {},
        },
        {
            "event_type": "latency_metrics",
            "conversation_id": "conversation-one-shot",
            "turn_id": "turn-one-shot",
            "payload": {"total_ms": 10},
        },
    ])
    with tempfile.TemporaryDirectory() as directory:
        await asyncio.wait_for(client.receive_events(
            socket,
            Path(directory) / "reply.mp3",
            False,
            "mp3",
            16000,
            interrupt_after_first_chunk=True,
            one_shot=True,
        ), timeout=0.2)
    assert socket.sent == [{
        "type": "tts_interrupt",
        "conversation_id": "conversation-one-shot",
        "turn_id": "turn-one-shot",
        "source": "button",
    }]
    assert socket.received_count == 5


async def verify_unrelated_conversation_terminal_does_not_stop_receiver() -> None:
    audio_payload = {
        "format": "mp3",
        "sample_rate": 16000,
        "channels": 1,
        "seq": 0,
        "is_last": False,
        "audio": "QQ==",
    }
    socket = FakeEventSocket([
        {
            "event_type": "tts_audio_chunk",
            "conversation_id": "active-conversation",
            "turn_id": "shared-receive-turn",
            "payload": audio_payload,
        },
        {
            "event_type": "conversation_ended",
            "conversation_id": "late-conversation",
            "turn_id": "shared-receive-turn",
            "payload": {},
        },
        {
            "event_type": "latency_metrics",
            "conversation_id": "late-conversation",
            "turn_id": "shared-receive-turn",
            "payload": {"total_ms": 1},
        },
        {
            "event_type": "tts_audio_chunk",
            "conversation_id": "active-conversation",
            "turn_id": "shared-receive-turn",
            "payload": {**audio_payload, "is_last": True},
        },
        {
            "event_type": "voice_done",
            "conversation_id": "active-conversation",
            "turn_id": "shared-receive-turn",
            "payload": {},
        },
        {
            "event_type": "conversation_ended",
            "conversation_id": "active-conversation",
            "turn_id": "shared-receive-turn",
            "payload": {},
        },
        {
            "event_type": "latency_metrics",
            "conversation_id": "active-conversation",
            "turn_id": "shared-receive-turn",
            "payload": {"total_ms": 2},
        },
    ])
    with tempfile.TemporaryDirectory() as directory:
        output = Path(directory) / "reply.mp3"
        await client.receive_events(socket, output, False, "mp3", 16000, one_shot=False)
        assert output.read_bytes() == b"AA"
    assert socket.received_count == 7


asyncio.run(verify_button_interrupt())
asyncio.run(verify_late_packet_is_dropped_before_decode())
asyncio.run(verify_one_shot_interrupt_finishes_without_voice_done())
asyncio.run(verify_unrelated_conversation_terminal_does_not_stop_receiver())

print("Python SDK protocol self-test: PASS")
