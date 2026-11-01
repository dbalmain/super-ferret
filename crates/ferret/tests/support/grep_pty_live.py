"""Feed stdin past the stream threshold, then one marker line, and report
whether the marker reached a real terminal while stdin was still open."""
import os
import select
import subprocess
import sys
import threading
import time

master, slave = os.openpty()
assert os.isatty(slave)
child = subprocess.Popen(sys.argv[1:], stdin=subprocess.PIPE,
                         stdout=slave, stderr=subprocess.DEVNULL)
os.close(slave)
fed = threading.Event()


def feed():
    line = b"hit " + b"x" * 59 + b"\n"
    child.stdin.write(line * ((5 << 20) // len(line)) + b"hit MARKER\n")
    child.stdin.flush()
    fed.set()


threading.Thread(target=feed, daemon=True).start()
tail = b""
seen = False
deadline = time.monotonic() + 10
# A line the program withholds would block a plain read until stdin closed,
# and stdin closes only after this loop: poll instead.
while time.monotonic() < deadline:
    if not select.select([master], [], [], 0.1)[0]:
        continue
    try:
        chunk = os.read(master, 65536)
    except OSError:
        break
    if not chunk:
        break
    tail = (tail + chunk)[-64:]
    if b"MARKER" in tail:
        seen = fed.is_set()
        break
child.stdin.close()
while True:
    try:
        if not os.read(master, 65536):
            break
    except OSError:
        break
child.wait(timeout=10)
print("seen" if seen else "withheld")
