#!/usr/bin/env python3
"""Minimal Wyoming device emulator for end-to-end testing an Anamanti Core.

Acts like the Echo Show for one voice turn: streams a 16 kHz mono s16le WAV
(real-time paced), keeps sending silence until the Core's VAD finalizes, then
collects every frame the Core sends back until the turn's final `audio-stop`
after TTS audio (or a timeout).

Usage:
    e2e_wyoming_client.py HOST PORT WAV [--timeout 90]

Make a test WAV on macOS:
    say -o /tmp/q.wav --data-format=LEI16@16000 "what is two plus two"

Prints a JSON summary: transcript, reply text, non-audio frames (display
actions, listen windows, ...), TTS byte count and frame-type counts. Exit code 0
when the turn completed with a transcript, a reply and TTS audio; 1 otherwise.
"""
import json
import socket
import sys
import threading
import time
import wave

RATE, WIDTH, CHANNELS = 16000, 2, 1
CHUNK_SAMPLES = 1024


def frame(event_type, data=None, payload=None):
    header = {"type": event_type}
    data_bytes = json.dumps(data).encode() if data is not None else b""
    if data_bytes:
        header["data_length"] = len(data_bytes)
    if payload:
        header["payload_length"] = len(payload)
    return json.dumps(header).encode() + b"\n" + data_bytes + (payload or b"")


def read_exact(f, n):
    buf = b""
    while len(buf) < n:
        chunk = f.read(n - len(buf))
        if not chunk:
            raise EOFError("socket closed mid-frame")
        buf += chunk
    return buf


def reader(sock, out, done):
    f = sock.makefile("rb")
    try:
        while True:
            line = f.readline()
            if not line:
                break
            hdr = json.loads(line)
            data = {}
            if hdr.get("data_length"):
                data = json.loads(read_exact(f, hdr["data_length"]))
            elif "data" in hdr:
                data = hdr["data"] or {}
            payload = read_exact(f, hdr["payload_length"]) if hdr.get("payload_length") else b""
            t = hdr["type"]
            out["types"].append(t)
            if t == "transcript":
                out["transcript"] = data.get("text")
            elif t == "reply-token":
                out["reply"] += data.get("text", "")
            elif t == "audio-chunk":
                out["tts_bytes"] += len(payload)
            elif t not in ("audio-start", "audio-stop"):
                out["frames"].append({"type": t, "data": data})
            if t == "audio-stop" and out["tts_bytes"] > 0:
                break
    except (EOFError, OSError, ValueError) as e:
        out["reader_error"] = repr(e)
    finally:
        done.set()


def main():
    if len(sys.argv) < 4:
        print(__doc__)
        sys.exit(2)
    host, port, wav_path = sys.argv[1], int(sys.argv[2]), sys.argv[3]
    timeout = float(sys.argv[sys.argv.index("--timeout") + 1]) if "--timeout" in sys.argv else 90.0
    with wave.open(wav_path, "rb") as w:
        if (w.getframerate(), w.getsampwidth(), w.getnchannels()) != (RATE, WIDTH, CHANNELS):
            sys.exit("WAV must be 16 kHz mono s16le")
        pcm = w.readframes(w.getnframes())

    out = {"transcript": None, "reply": "", "types": [], "frames": [], "tts_bytes": 0}
    done = threading.Event()
    t0 = time.time()
    sock = socket.create_connection((host, port), timeout=timeout)
    sock.settimeout(timeout)
    threading.Thread(target=reader, args=(sock, out, done), daemon=True).start()

    fmt = {"rate": RATE, "width": WIDTH, "channels": CHANNELS}
    sock.sendall(frame("audio-start", {**fmt, "timestamp": 0}))
    step = CHUNK_SAMPLES * WIDTH
    chunk_secs = CHUNK_SAMPLES / RATE
    ts = 0
    # Speech, then up to 10 s of trailing silence (stops once the Core transcribes).
    stream = pcm + b"\x00" * (RATE * WIDTH * 10)
    for i in range(0, len(stream), step):
        if done.is_set() or out["transcript"] is not None:
            break
        try:
            sock.sendall(frame("audio-chunk", {**fmt, "timestamp": ts}, stream[i:i + step]))
        except OSError:
            break
        ts += int(chunk_secs * 1000)
        time.sleep(chunk_secs)
    done.wait(timeout)
    out["elapsed_s"] = round(time.time() - t0, 2)
    sock.close()
    counts = {}
    for t in out.pop("types"):
        counts[t] = counts.get(t, 0) + 1
    out["type_counts"] = counts
    ok = bool(out["transcript"]) and bool(out["reply"]) and out["tts_bytes"] > 0 and "reader_error" not in out
    out["ok"] = ok
    print(json.dumps(out, indent=1))
    sys.exit(0 if ok else 1)


if __name__ == "__main__":
    main()
