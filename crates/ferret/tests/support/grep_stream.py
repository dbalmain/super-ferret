"""D138: first output precedes a real pipe producer's bounded sleep."""
import os
import select
import subprocess
import sys
import threading
import time
import tty

for terminal in (False, True):
    for operand in ('-', '/dev/stdin'):
        reader, writer = os.pipe()
        if terminal:
            output, destination = os.openpty()
            tty.setraw(destination)
        else:
            output, destination = os.pipe()
        child = subprocess.Popen(
            [sys.argv[1], 'grep', '--color=never', '--no-heading', '-N', 'hit', operand],
            stdin=reader, stdout=destination, stderr=subprocess.PIPE,
        )
        os.close(reader)
        os.close(destination)
        awake = threading.Event()

        def produce():
            try:
                os.write(writer, b'hit early\n')
                time.sleep(2)
                awake.set()
                os.write(writer, b'hit later\n')
            finally:
                os.close(writer)

        producer = threading.Thread(target=produce)
        producer.start()
        try:
            ready = select.select([output], [], [], 2)[0]
            first = os.read(output, 4096) if ready else b''
            before_waking = not awake.is_set()
        finally:
            producer.join()
            _, errors = child.communicate(timeout=5)
            os.close(output)
        assert first == b'hit early\n', (terminal, operand, first, errors)
        assert before_waking, (terminal, operand, 'first output waited for producer')
        assert child.returncode == 0 and not errors, (child.returncode, errors)
