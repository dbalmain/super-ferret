#!/usr/bin/env python3
"""S2 M5: warm resident socket/batch and native CLI latency on a scratch tree.

Run after building release ferret binaries, inside the isolated measurement
shell. Every subprocess has a private HOME, XDG directories and FERRET_INDEX.
The process and tree are removed on exit; no existing daemon is contacted.
"""
import json
import os
from pathlib import Path
import socket
import statistics
import subprocess
import tempfile
import time

ROOT = Path(__file__).resolve().parents[3]
BIN = ROOT / "target/release"
QUERIES = {
    "rare": ["text:uniqueneedle"],
    "mid": ["text:middle"],
    "common": ["text:common"],
    "phrase": ["text:request handler"],
    "bare_not": ["NOT", "text:absentneedle"],
}


def distribution(values):
    return {"median_ms": round(statistics.median(values), 3),
            "p95_ms": round(sorted(values)[int(len(values) * .95) - 1], 3)}


def query(writer, reader, args):
    writer.write((json.dumps({"id": "q", "op": "search", "args": args}) + "\n").encode())
    writer.flush()
    rows = []
    while True:
        event = json.loads(reader.readline())
        if event["event"] == "row":
            rows.append(event["path"])
        if event["event"] == "end":
            assert event["exit"] in (0, 1), event
            assert "error" not in event, event
            return rows, event["elapsed_us"] / 1000


def main():
    with tempfile.TemporaryDirectory(prefix="ferret-m5-latency-") as scratch:
        base = Path(scratch)
        env = os.environ.copy()
        for key, path in {
            "HOME": "home", "XDG_CONFIG_HOME": "config", "XDG_DATA_HOME": "data",
            "XDG_STATE_HOME": "state", "XDG_CACHE_HOME": "cache",
            "XDG_RUNTIME_DIR": "runtime", "FERRET_INDEX": "index",
        }.items():
            env[key] = str(base / path)
            (base / path).mkdir()
        (base / "runtime").chmod(0o700)
        env.update(FERRET_NO_DAEMON="1", FERRET_DAEMON_IDLE_MS="0",
                   GIT_CONFIG_GLOBAL="/dev/null", GIT_CONFIG_SYSTEM="/dev/null")
        tree = base / "tree"
        tree.mkdir()
        for i in range(2000):
            head = f"identityDoc{i} common "
            head += "middle " if i < 100 else ""
            head += "uniqueneedle " if i == 0 else ""
            head += "requestHandler " if i % 2 == 0 else "request other handler "
            text = (head + "otherAlpha otherBeta otherGamma " * 600)[:8192]
            (tree / f"file-{i:04}.txt").write_text(text)
        subprocess.run([BIN / "ferret", "index", tree], env=env, check=True,
                       stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL, timeout=30)
        env.pop("FERRET_NO_DAEMON")
        daemon = subprocess.Popen([BIN / "ferretd"], env=env,
                                  stdout=subprocess.DEVNULL, stderr=subprocess.PIPE)
        batch = None
        connection = None
        try:
            deadline = time.monotonic() + 15
            while True:
                found = list((base / "runtime").glob("ferret/*.sock"))
                if found:
                    try:
                        connection = socket.socket(socket.AF_UNIX)
                        connection.settimeout(15)
                        connection.connect(str(found[0]))
                        break
                    except (FileNotFoundError, ConnectionRefusedError):
                        connection.close()
                assert time.monotonic() < deadline, "daemon startup timed out"
                time.sleep(.01)
            wire = connection.makefile("rwb", buffering=65536)
            while json.loads(wire.readline())["state"] != "ready":
                pass
            while True:
                wire.write(b'{"id":"s","op":"status"}\n')
                wire.flush()
                status = json.loads(wire.readline())
                if (status["last_complete_backstop"] is not None
                        and not status["writer_busy"] and status["index"]["uncovered"] == 0):
                    break
                assert time.monotonic() < deadline, status
                time.sleep(.01)
            local_env = dict(env, FERRET_NO_DAEMON="1")
            batch = subprocess.Popen([BIN / "ferret", "batch"], env=local_env,
                                     stdin=subprocess.PIPE, stdout=subprocess.PIPE,
                                     stderr=subprocess.PIPE)
            result = {"files": 2000, "content_bytes": 2000 * 8192,
                      "cache": "warm OS cache", "repetitions": 50,
                      "index": status["index"],
                      "index_resident_bytes": status["index_resident_bytes"],
                      "daemon_rss_kib": status["current_rss_kb"], "classes": {}}
            for name, args in QUERIES.items():
                expected, _ = query(batch.stdin, batch.stdout, args)
                actual, _ = query(wire, wire, args)
                assert actual == expected, (name, len(actual), len(expected))
                resident = {"daemon": [], "in_process_batch": []}
                native = {"daemon": [], "in_process_cli": []}
                for _ in range(50):
                    _, elapsed = query(wire, wire, args)
                    resident["daemon"].append(elapsed)
                    _, elapsed = query(batch.stdin, batch.stdout, args)
                    resident["in_process_batch"].append(elapsed)
                    for host, child_env in (("daemon", env), ("in_process_cli", local_env)):
                        started = time.perf_counter()
                        output = subprocess.run([BIN / "ferret", "search", *args], env=child_env,
                                                capture_output=True, timeout=15)
                        native[host].append((time.perf_counter() - started) * 1000)
                        assert output.returncode in (0, 1) and output.stderr == b"", output.stderr
                result["classes"][name] = {"rows": len(expected),
                    "resident": {k: distribution(v) for k, v in resident.items()},
                    "native_cli": {k: distribution(v) for k, v in native.items()}}
            print(json.dumps(result, indent=2))
        finally:
            if batch is not None:
                batch.stdin.close()
                batch.wait(timeout=10)
            if connection is not None:
                connection.sendall(b'{"op":"drain"}\n')
                connection.close()
            try:
                daemon.wait(timeout=10)
            except subprocess.TimeoutExpired:
                daemon.kill()
                daemon.wait()
            assert daemon.returncode == 0, daemon.stderr.read().decode()


if __name__ == "__main__":
    main()
