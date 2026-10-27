"""Terminal integration checks. Run after cargo build:

    python3 tests/interactive_shell.py

Requires Bash and Zsh. Startup files are skipped for repeatable checks.
"""
import os
import pathlib
import pty
import select
import shutil
import signal
import tempfile
import time
root = str(pathlib.Path(__file__).resolve().parents[1])
binary = str(pathlib.Path(root) / 'target/debug/lynn')
with tempfile.TemporaryDirectory(prefix='lynn-shell-test-') as tmp:
    for name, flags in [('bash', '--noprofile --norc'), ('zsh', '-f')]:
        real = shutil.which(name)
        assert real, f'{name} is required for these checks'
        wrapper = pathlib.Path(tmp) / name
        wrapper.write_text(f'#!/bin/sh\nexec {real} {flags} "$@"\n')
        wrapper.chmod(0o755)
    pid, fd = pty.fork()
    if pid == 0:
        os.chdir(root)
        os.environ['PATH'] = tmp + ':' + os.environ['PATH']
        os.environ['NO_COLOR'] = '1'
        os.execv(binary, [binary])
    buf = b''
    def wait_for(needle, timeout=8):
        global buf
        end = time.monotonic() + timeout
        while needle not in buf:
            remaining = end - time.monotonic()
            if remaining <= 0:
                raise AssertionError(f'timed out waiting for {needle!r}: {buf[-2500:]!r}')
            if select.select([fd], [], [], remaining)[0]:
                buf += os.read(fd, 65536)
        buf = buf[buf.index(needle) + len(needle):]
    def send(text):
        os.write(fd, text.encode())
    try:
        wait_for(b'> ')
        for shell in ['bash', 'zsh']:
            send(shell + '!\n')
            wait_for(('[entering ' + shell).encode())
            send("PS1='INNER> '; printf '\\nSTART:%s\\n' \"${LYNN_SHELL_TEST-unset}\"\n")
            wait_for(b'\r\nSTART:unset\r\n')
            send(f'cd {tmp}\nexport LYNN_SHELL_TEST=inner\n')
            send("for n in one two; do\nprintf 'LOOP:%s\\n' \"$n\"\ndone\n")
            wait_for(b'LOOP:two\r\n')
            send("read -r answer; printf 'INPUT:%s\\n' \"$answer\"\n")
            time.sleep(.15)
            send('keyboard-input\n')
            wait_for(b'\r\nINPUT:keyboard-input\r\n')
            send("printf '\\nSLEEP-START\\n'; sleep 20\n")
            wait_for(b'\r\nSLEEP-START\r\n')
            time.sleep(.2)
            send('\x03')
            wait_for(b'INNER> ')
            send("printf '\\nSTATE:%s:%s\\n' \"$LYNN_SHELL_TEST\" \"$PWD\"\n")
            wait_for(f'\r\nSTATE:inner:{tmp}\r\n'.encode())
            send('exit 7\n')
            wait_for(f'[returned to Lynn from {shell}]'.encode())
            wait_for(b'> ')
            send('echo EXIT-$?\n')
            wait_for(b'EXIT-7\r\n')
            wait_for(b'> ')
            send('pwd\n')
            wait_for((root + '\r\n').encode())
            wait_for(b'> ')
            send('echo OUTER-$LYNN_SHELL_TEST\n')
            wait_for(b'OUTER-\r\n')
            wait_for(b'> ')
            print(shell + ': multiline commands, state, keyboard input, Ctrl-C, exit status, and Lynn isolation passed')
        send('exit\n')
        os.waitpid(pid, 0)
        pid = None
    finally:
        os.close(fd)
        try:
            if pid is not None:
                os.kill(pid, signal.SIGKILL)
                os.waitpid(pid, 0)
        except ProcessLookupError:
            pass
