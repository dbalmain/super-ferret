"""Capture an actual terminal's bytes; never stand in for an isatty check."""
import errno
import struct
import os
import subprocess
import sys

master, slave = os.openpty()
assert os.isatty(slave)
child = subprocess.Popen(sys.argv[1:], stdin=subprocess.DEVNULL,
                         stdout=slave, stderr=subprocess.PIPE)
os.close(slave)
output = bytearray()
while True:
    try:
        chunk = os.read(master, 65536)
        if not chunk:
            break
        output.extend(chunk)
    except OSError as error:
        if error.errno != errno.EIO:
            raise
        break
os.close(master)
_, errors = child.communicate(timeout=10)
sys.stdout.buffer.write(struct.pack(">iQ", child.returncode, len(output)) + output + errors)
