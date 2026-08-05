#!/usr/bin/env python3
import argparse
import asyncio
import base64
from collections import deque
import inspect
import ipaddress
import json
import shutil
import struct
import subprocess
import time
import urllib.parse
import urllib.request
from pathlib import Path

import websockets


AUDIO_FORMATS = ("mp3", "pcm", "opus", "speex")
AUDIO_RATES = (8000, 16000, 24000)
OUTPUT_SUFFIXES = {"mp3": ".mp3", "pcm": ".pcm", "opus": ".opuspack", "speex": ".speex"}


def websocket_connect_options(connect_callable) -> dict:
    options = {"max_size": None}
    if "proxy" in inspect.signature(connect_callable).parameters:
        options["proxy"] = None
    return options


def post_json(url: str, payload: dict) -> dict:
    data = json.dumps(payload).encode("utf-8")
    request = urllib.request.Request(url, data=data, headers={"content-type": "application/json"}, method="POST")
    with urllib.request.urlopen(request, timeout=10) as response:
        return json.loads(response.read().decode("utf-8"))


def api_url(base_url: str, path: str) -> str:
    parsed = urllib.parse.urlparse(base_url)
    return urllib.parse.urlunparse((parsed.scheme, parsed.netloc, f"{parsed.path.rstrip('/')}{path}", "", "", ""))


def resolve_device_secret(base_url: str, device_id: str, device_secret: str | None) -> str:
    host = urllib.parse.urlparse(base_url).hostname or ""
    try:
        is_local = host.lower() == "localhost" or ipaddress.ip_address(host).is_loopback
    except ValueError:
        is_local = host.lower() == "localhost"
    if device_id == "DOLL-0001" and not is_local:
        raise SystemExit("DOLL-0001 is local-only; provision a separate device and pass --device-secret")
    if device_secret:
        return device_secret
    if is_local and device_id == "DOLL-0001":
        return "demo-secret"
    raise SystemExit("--device-secret is required for non-local or independently provisioned devices")


def ws_url(base_url: str, device_id: str, token: str, in_format: str, in_rate: int,
           out_format: str, out_rate: int) -> str:
    parsed = urllib.parse.urlparse(base_url)
    scheme = "wss" if parsed.scheme == "https" else "ws"
    query = urllib.parse.urlencode({
        "device_id": device_id,
        "token": token,
        "in_format": in_format,
        "in_rate": in_rate,
        "out_format": out_format,
        "out_rate": out_rate,
    })
    return f"{scheme}://{parsed.netloc}{parsed.path.rstrip('/')}/api/device/voice?{query}"


def validate_profile(audio_format: str, sample_rate: int, direction: str) -> None:
    if audio_format == "speex" and sample_rate == 24000:
        raise SystemExit(f"--{direction}-rate 24000 is unsupported for Speex; use 8000 or 16000")


def output_path_for_profile(value: str | None, audio_format: str) -> Path:
    path = Path(value or f"/tmp/mjy-device-reply{OUTPUT_SUFFIXES[audio_format]}")
    return path.with_suffix(OUTPUT_SUFFIXES[audio_format])


def validate_tts_metadata(payload: dict, out_format: str, out_rate: int) -> None:
    actual = (payload.get("format"), payload.get("sample_rate"), payload.get("channels"))
    expected = (out_format, out_rate, 1)
    if actual != expected:
        raise RuntimeError(
            f"tts_audio_chunk metadata mismatch: expected {out_format}/{out_rate}/mono, "
            f"received {actual[0]}/{actual[1]}/{actual[2]}ch"
        )
    if out_format == "pcm" and payload.get("bit_depth") != 16:
        raise RuntimeError("tts_audio_chunk metadata mismatch: PCM bit_depth must be 16")


class TtsSequenceValidator:
    def __init__(self) -> None:
        self.closed: set[int] = set()

    def validate(self, payload: dict) -> None:
        seq = payload.get("seq")
        is_last = payload.get("is_last")
        if not isinstance(seq, int) or seq < 0 or not isinstance(is_last, bool):
            raise RuntimeError("tts_audio_chunk seq must be non-negative int and is_last must be boolean")
        if seq in self.closed:
            raise RuntimeError(f"tts_audio_chunk sequence out of order: seq={seq}")
        if is_last:
            self.closed.add(seq)


class TtsOrderedAudio:
    def __init__(self) -> None:
        self.next_seq = 0
        self.buffered: dict[int, list[bytes]] = {}
        self.closed: set[int] = set()
        self.seen: set[int] = set()

    def accept(self, payload: dict, chunk: bytes) -> list[bytes]:
        seq = payload["seq"]
        self.seen.add(seq)
        if seq in self.closed:
            raise RuntimeError(f"tts_audio_chunk sequence out of order: seq={seq}")
        ready: list[bytes] = []
        if seq == self.next_seq:
            if chunk:
                ready.append(chunk)
        else:
            if chunk:
                self.buffered.setdefault(seq, []).append(chunk)
        if payload["is_last"]:
            self.closed.add(seq)
        while self.next_seq in self.closed:
            self.next_seq += 1
            ready.extend(self.buffered.pop(self.next_seq, []))
        return ready

    def finish(self) -> None:
        if self.buffered or self.seen != self.closed:
            raise RuntimeError("tts_audio_chunk sequence incomplete at voice_done")


class PlaybackState:
    DROPPABLE_EVENT_TYPES = {"llm_delta", "reply_sentence", "tts_audio_chunk", "voice_done"}
    INTERRUPTED_TURN_LIMIT = 64

    def __init__(self) -> None:
        self.conversation_id: str | None = None
        self.turn_id: str | None = None
        self.interrupted: set[tuple[str, str]] = set()
        self.interrupted_order: deque[tuple[str, str]] = deque()
        self.sequence = TtsSequenceValidator()
        self.ordered = TtsOrderedAudio()
        self.audio_chunks: list[bytes] = []

    def reset_playback_buffers(self) -> None:
        self.sequence = TtsSequenceValidator()
        self.ordered = TtsOrderedAudio()
        self.audio_chunks.clear()

    def observe(self, event: dict) -> None:
        event_type = event.get("event_type")
        turn_id = event.get("turn_id")
        conversation_id = event.get("conversation_id")
        if event_type == "tts_audio_chunk":
            if not isinstance(turn_id, str) or not isinstance(conversation_id, str):
                return
            if (conversation_id, turn_id) in self.interrupted:
                return
            if (conversation_id, turn_id) != (self.conversation_id, self.turn_id):
                self.reset_playback_buffers()
                self.conversation_id = conversation_id
                self.turn_id = turn_id
            return
        if event_type not in {"tts_interrupted", "voice_done", "conversation_ended"}:
            return
        current_playback = (
            (self.conversation_id, self.turn_id)
            if isinstance(self.conversation_id, str) and isinstance(self.turn_id, str)
            else None
        )
        terminal_playback = (
            (conversation_id, turn_id)
            if isinstance(conversation_id, str) and isinstance(turn_id, str)
            else None
        )
        if event_type == "conversation_ended":
            if current_playback is not None and terminal_playback != current_playback:
                return
        elif current_playback is None or terminal_playback != current_playback:
            return
        self.conversation_id = None
        self.turn_id = None
        self.reset_playback_buffers()

    def should_drop(self, event: dict) -> bool:
        if event.get("event_type") not in self.DROPPABLE_EVENT_TYPES:
            return False
        turn_id = event.get("turn_id")
        conversation_id = event.get("conversation_id")
        return (
            isinstance(conversation_id, str)
            and isinstance(turn_id, str)
            and (conversation_id, turn_id) in self.interrupted
        )

    def interrupt_payload(self, source: str = "button") -> dict | None:
        conversation_id = self.conversation_id
        turn_id = self.turn_id
        if not conversation_id or not turn_id:
            return None
        playback_key = (conversation_id, turn_id)
        if playback_key in self.interrupted:
            return None
        self.interrupted.add(playback_key)
        self.interrupted_order.append(playback_key)
        while len(self.interrupted_order) > self.INTERRUPTED_TURN_LIMIT:
            self.interrupted.discard(self.interrupted_order.popleft())
        self.conversation_id = None
        self.turn_id = None
        self.reset_playback_buffers()
        return {
            "type": "tts_interrupt",
            "conversation_id": conversation_id,
            "turn_id": turn_id,
            "source": source,
        }


def encode_output_chunk(audio_format: str, chunk: bytes) -> bytes:
    if audio_format == "opus":
        return struct.pack("<I", len(chunk)) + chunk
    return chunk


def decode_tts_audio(payload: dict, out_format: str, out_rate: int,
                     sequence: TtsSequenceValidator) -> bytes:
    validate_tts_metadata(payload, out_format, out_rate)
    decoded = base64.b64decode(payload.get("audio") or "", validate=True)
    sequence.validate(payload)
    return decoded


class StreamPlayer:
    def __init__(self, audio_format: str, sample_rate: int, enabled: bool) -> None:
        self.audio_format = audio_format
        self.sample_rate = sample_rate
        self.enabled = enabled
        self.process: subprocess.Popen | None = None

    def start(self) -> None:
        if not self.enabled:
            return
        if self.audio_format in {"opus", "speex"}:
            raise RuntimeError(
                f"--play is unsupported for raw {self.audio_format} packets; "
                "save them or send each packet to the device decoder"
            )
        players = ("ffplay",) if self.audio_format == "pcm" else ("mpg123", "ffplay")
        for player in players:
            binary = shutil.which(player)
            if not binary:
                continue
            if self.audio_format == "pcm":
                args = [binary, "-nodisp", "-autoexit", "-loglevel", "quiet", "-f", "s16le",
                        "-ar", str(self.sample_rate), "-ac", "1", "-i", "pipe:0"]
            elif player == "mpg123":
                args = [binary, "-q", "-"]
            else:
                args = [binary, "-nodisp", "-autoexit", "-loglevel", "quiet", "-i", "pipe:0"]
            self.process = subprocess.Popen(args, stdin=subprocess.PIPE)
            print(f"stream_player={player}")
            return
        print(f"no stdin {self.audio_format} player found; audio will only be saved")

    def write(self, chunk: bytes) -> None:
        if not self.process or not self.process.stdin:
            return
        try:
            self.process.stdin.write(chunk)
            self.process.stdin.flush()
        except BrokenPipeError:
            self.close()

    def close(self) -> None:
        if not self.process:
            return
        if self.process.stdin:
            try:
                self.process.stdin.close()
            except BrokenPipeError:
                pass
        try:
            self.process.wait(timeout=10)
        except subprocess.TimeoutExpired:
            self.process.terminate()
        self.process = None

    def stop_now(self) -> None:
        if not self.process:
            return
        process = self.process
        self.process = None
        if process.poll() is None:
            process.kill()
        if process.stdin:
            try:
                process.stdin.close()
            except BrokenPipeError:
                pass
        try:
            process.wait(timeout=1)
        except subprocess.TimeoutExpired:
            process.kill()
            process.wait(timeout=1)


async def interrupt_tts_from_button(socket, playback: PlaybackState,
                                    stream_player: StreamPlayer) -> bool:
    payload = playback.interrupt_payload("button")
    if payload is None:
        return False
    stream_player.stop_now()
    try:
        await socket.send(json.dumps(payload))
    except Exception as error:
        print(f"tts_interrupt_send_failed={error}")
    return True


async def receive_events(socket, output: Path, play: bool, out_format: str, out_rate: int,
                         interrupt_after_first_chunk: bool = False,
                         one_shot: bool = True) -> None:
    playback = PlaybackState()
    completed_audio_chunks: list[bytes] = []
    stream_player = StreamPlayer(out_format, out_rate, play)
    stream_player.start()
    interrupt_sent = False
    one_shot_turn: tuple[str, str] | None = None
    interrupted_acknowledged = False
    interrupted_turn_finished = False
    conversation_ended_turn: tuple[str, str] | None = None
    last_completed_turn: tuple[str, str] | None = None
    try:
        while True:
            event = json.loads(await socket.recv())
            event_type = event.get("event_type")
            event_turn_id = event.get("turn_id")
            event_conversation_id = event.get("conversation_id")
            event_playback_key = (
                (event_conversation_id, event_turn_id)
                if isinstance(event_conversation_id, str) and isinstance(event_turn_id, str)
                else None
            )
            payload = event.get("payload") or {}
            if playback.should_drop(event):
                print(f"dropped_interrupted_event={event_type} turn_id={event.get('turn_id')}")
                continue
            if event_type == "tts_audio_chunk":
                playback.observe(event)
                if one_shot_turn is None:
                    one_shot_turn = event_playback_key
                chunk = decode_tts_audio(payload, out_format, out_rate, playback.sequence)
                ready_chunks = playback.ordered.accept(payload, chunk)
                if ready_chunks and stream_player.process is None:
                    stream_player.start()
                for ready in ready_chunks:
                    playback.audio_chunks.append(encode_output_chunk(out_format, ready))
                    stream_player.write(ready)
                print(f"tts_audio_chunk format={payload.get('format')} sample_rate={payload.get('sample_rate')} "
                      f"seq={payload.get('seq')} bytes={len(chunk)} last={payload.get('is_last')}")
                if interrupt_after_first_chunk and ready_chunks and not interrupt_sent:
                    interrupt_sent = await interrupt_tts_from_button(socket, playback, stream_player)
            elif event_type in {"asr_partial", "asr_final", "reply_sentence", "order_draft", "order_created", "error"}:
                print(event_type, json.dumps(payload, ensure_ascii=False))
            elif event_type == "tts_interrupted":
                playback.observe(event)
                print("tts_interrupted", json.dumps(payload, ensure_ascii=False))
                if one_shot and event_playback_key == one_shot_turn:
                    interrupted_acknowledged = True
                    if interrupted_turn_finished:
                        break
            elif event_type == "voice_done":
                playback.ordered.finish()
                completed_audio_chunks = list(playback.audio_chunks)
                playback.observe(event)
                last_completed_turn = event_playback_key
                print("voice_done")
                if one_shot and (one_shot_turn is None or event_playback_key == one_shot_turn):
                    break
            elif event_type == "latency_metrics":
                print("latency_metrics", json.dumps(payload, ensure_ascii=False))
                if one_shot and event_playback_key == one_shot_turn and interrupt_sent:
                    interrupted_turn_finished = True
                    if interrupted_acknowledged:
                        break
                if event_playback_key == conversation_ended_turn:
                    break
            elif event_type == "conversation_ended":
                active_playback_key = (
                    (playback.conversation_id, playback.turn_id)
                    if isinstance(playback.conversation_id, str)
                    and isinstance(playback.turn_id, str)
                    else None
                )
                playback.observe(event)
                print("conversation_ended")
                expected_playback_key = active_playback_key or last_completed_turn
                if expected_playback_key is None and interrupt_sent:
                    expected_playback_key = one_shot_turn
                if expected_playback_key is not None and event_playback_key != expected_playback_key:
                    continue
                if event_playback_key is not None:
                    conversation_ended_turn = event_playback_key
                else:
                    break
    finally:
        stream_player.close()
    if completed_audio_chunks:
        output.write_bytes(b"".join(completed_audio_chunks))
        print(f"saved_tts={output}")


def audio_chunks(data: bytes, audio_format: str, sample_rate: int):
    if audio_format == "opus":
        raise SystemExit(
            "Opus file upload is unsupported: a flat file does not preserve variable packet boundaries; "
            "send one complete device-encoded Opus packet per audio_stream_chunk"
        )
    if audio_format == "pcm":
        if len(data) % 2:
            raise SystemExit("PCM input must contain complete signed 16-bit little-endian samples")
        frame_bytes = sample_rate * 2 * 40 // 1000
    elif audio_format == "speex":
        frame_bytes = 38 if sample_rate == 8000 else 60
        if len(data) % frame_bytes:
            raise SystemExit(
                f"Speex input must contain whole quality-7 packets: {frame_bytes} bytes per 20ms packet"
            )
    else:
        frame_bytes = 4096
    for offset in range(0, len(data), frame_bytes):
        yield data[offset:offset + frame_bytes]


async def send_audio_stream(socket, conversation_id: str, audio_path: Path,
                            in_format: str, in_rate: int) -> None:
    chunks = list(audio_chunks(audio_path.read_bytes(), in_format, in_rate))
    if not chunks:
        raise SystemExit("audio input file is empty")
    trace_id = f"py-{int(time.time() * 1000)}"
    client_sent_ms = int(time.time() * 1000)
    await socket.send(json.dumps({"type": "audio_stream_start", "conversation_id": conversation_id,
                                  "trace_id": trace_id, "client_sent_ms": client_sent_ms}))
    for chunk in chunks:
        await socket.send(json.dumps({
            "type": "audio_stream_chunk",
            "audio": base64.b64encode(chunk).decode("ascii"),
        }))
        await asyncio.sleep(0.02 if in_format == "speex" else 0.04)
    await socket.send(json.dumps({"type": "audio_stream_end", "conversation_id": conversation_id}))


async def send_audio_segment(socket, conversation_id: str, audio_path: Path,
                             in_format: str) -> None:
    if in_format in {"opus", "speex"}:
        raise SystemExit(f"{in_format} input is packetized; use --stream so each chunk contains exactly one packet")
    data = audio_path.read_bytes()
    await socket.send(json.dumps({
        "type": "audio_segment",
        "conversation_id": conversation_id,
        "audio": base64.b64encode(data).decode("ascii"),
        "trace_id": f"py-{int(time.time() * 1000)}",
        "client_sent_ms": int(time.time() * 1000),
    }))


async def run(args) -> None:
    validate_profile(args.in_format, args.in_rate, "in")
    validate_profile(args.out_format, args.out_rate, "out")
    if bool(args.text) == bool(args.audio):
        raise SystemExit("choose exactly one of --text or --audio")
    args.device_secret = resolve_device_secret(args.base_url, args.device_id, args.device_secret)
    auth = post_json(api_url(args.base_url, "/api/device/auth"),
                     {"device_id": args.device_id, "device_secret": args.device_secret})
    output = output_path_for_profile(args.output, args.out_format)
    output.parent.mkdir(parents=True, exist_ok=True)
    url = ws_url(args.base_url, args.device_id, auth["token"], args.in_format, args.in_rate,
                 args.out_format, args.out_rate)
    async with websockets.connect(url, **websocket_connect_options(websockets.connect)) as socket:
        conversation_id = args.conversation_id or f"device-{int(time.time() * 1000)}"
        receiver = asyncio.create_task(receive_events(
            socket, output, args.play, args.out_format, args.out_rate,
            args.interrupt_after_first_chunk, one_shot=True,
        ))
        if args.text:
            await socket.send(json.dumps({"type": "text", "conversation_id": conversation_id, "text": args.text}))
        elif args.stream:
            await send_audio_stream(socket, conversation_id, Path(args.audio), args.in_format, args.in_rate)
        else:
            await send_audio_segment(socket, conversation_id, Path(args.audio), args.in_format)
        await receiver


def parse_args():
    parser = argparse.ArgumentParser(description="MJY voice shop embedded-device client")
    parser.add_argument("--base-url", default="http://127.0.0.1:8787")
    parser.add_argument("--device-id", default="DOLL-0001")
    parser.add_argument("--device-secret")
    parser.add_argument("--conversation-id")
    parser.add_argument("--text")
    parser.add_argument("--audio", help="pre-encoded input file; the SDK does not encode codecs")
    parser.add_argument("--stream", action="store_true")
    parser.add_argument("--in-format", choices=AUDIO_FORMATS, default="mp3")
    parser.add_argument("--in-rate", type=int, choices=AUDIO_RATES, default=16000)
    parser.add_argument("--out-format", choices=AUDIO_FORMATS, default="mp3")
    parser.add_argument("--out-rate", type=int, choices=AUDIO_RATES, default=16000)
    parser.add_argument("--output", help="response path; suffix is normalized to the output format")
    parser.add_argument("--play", action="store_true", help="play MP3/PCM response chunks immediately")
    parser.add_argument(
        "--interrupt-after-first-chunk",
        action="store_true",
        help="simulate a debounced hardware button after the first playable TTS chunk",
    )
    return parser.parse_args()


if __name__ == "__main__":
    asyncio.run(run(parse_args()))
