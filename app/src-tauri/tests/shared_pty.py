"""Run as root inside a disposable Linux container with the runner and hook installed."""

import base64
import json
import os
from pathlib import Path
import pty
import re
import secrets
import select
import signal
import socket
import time
from contextlib import ExitStack

UID = 21017
OTHER = 21018
RUNTIME = Path(f'/run/user/{UID}')
BIN = '/usr/local/libexec/zzabba-ai-control'
HOOK = '/usr/local/share/zzabba/shared.bash'
KEY_PAIR = b'\x18p'
KEY_CLAIM = b'\x18u'
COUNT = Path('/tmp/zzabba-shared-uptime-count')


class Terminal:
    def __init__(self, argv, uid=UID):
        self.pid, self.fd = pty.fork()
        if self.pid == 0:
            os.setgroups([])
            os.setgid(uid)
            os.setuid(uid)
            os.chdir('/tmp')
            os.execve(argv[0], argv, {'PATH': '/usr/bin:/bin', 'TERM': 'xterm', 'HOME': '/tmp', 'PS1': 'zzabba$ ', 'LANG': 'C.UTF-8'})
        self.buffer = b''
        self.transcript = b''

    def __enter__(self):
        return self

    def __exit__(self, *_):
        if self.fd is not None:
            os.close(self.fd)
        if self.pid is not None:
            try:
                os.kill(self.pid, signal.SIGKILL)
            except ProcessLookupError:
                pass
            self.wait()

    def wait(self):
        deadline = time.monotonic() + 4
        while time.monotonic() < deadline:
            pid, status = os.waitpid(self.pid, os.WNOHANG)
            if pid:
                self.pid = None
                return status
            time.sleep(0.025)
        raise AssertionError('process did not exit')

    def send(self, data):
        assert os.write(self.fd, data) == len(data)

    def read(self, pattern):
        deadline = time.monotonic() + 4
        while not (match := re.search(pattern, self.buffer)):
            assert select.select([self.fd], [], [], max(0, deadline - time.monotonic()))[0], (
                f'PTY timed out waiting for {pattern!r}; pending={re.sub(rb"[0-9a-f]{32,}", b"<redacted>", self.buffer)!r}'
            )
            chunk = os.read(self.fd, 4096)
            assert chunk
            self.buffer += chunk
            self.transcript += chunk
        self.buffer = self.buffer[match.end():]
        return match.group()

    def quiet(self):
        assert not self.buffer and not select.select([self.fd], [], [], 0.2)[0], 'unexpected control event'


class Control(Terminal):
    def __init__(self):
        super().__init__([BIN, '--shared-control'])
        self.channel = secrets.token_hex(16)
        ready = self.event('ready')
        self.frame('hello', version=1, nonce=ready['nonce'])
        self.event('hello_ack')
        self.seq = 0
        pairing = self.event('pairing', 1)
        self.code = pairing['code'].encode()
        assert re.fullmatch(rb'[a-f0-9]{64}', self.code)
        self.path = RUNTIME / f'zzabba-ai-{self.code[:32].decode()}.sock'
        assert self.path.stat().st_uid == UID and self.path.stat().st_mode & 0o777 == 0o600

    def event(self, kind, seq=None):
        msg = json.loads(self.read(rb'[^\n]+\n'))
        assert msg['type'] == kind
        if kind != 'ready':
            assert msg['channel'] == self.channel
        if seq is not None:
            assert msg['seq'] == seq
        return msg

    def frame(self, kind, **fields):
        self.send(json.dumps(dict(type=kind, channel=self.channel, **fields)).encode() + b'\n')

    def pair(self, tab):
        tab.send(KEY_PAIR)
        tab.read(rb'Zzabba pairing code: ')
        tab.send(self.code + b'\n')
        tab.read(rb'zzabba\$ ')

    def ticket(self, job):
        return dict(main_id=1, main_generation=2, document=3, control_id=4, control_generation=5,
                    shell_generation=self.generation, approval_id=job, input_generation=6, policy_revision=7)

    def approval(self, kind, ticket):
        self.seq += 1
        extra = {'operation': 'uptime'} if kind != 'deny' else {}
        self.frame(kind, seq=self.seq, ticket=ticket, **extra)


def bash(stack, uid=UID):
    term = stack.enter_context(Terminal(['/usr/bin/bash', '--noprofile', '--norc', '-i'], uid))
    term.read(rb'zzabba\$ ')
    term.send(f'source {HOOK}\n'.encode())
    term.read(rb'zzabba\$ ')
    return term


def absent(path):
    deadline = time.monotonic() + 4
    while path.exists() and time.monotonic() < deadline:
        time.sleep(0.025)
    assert not path.exists(), 'stale control socket'


def wrong_uid(path):
    child = os.fork()
    if child == 0:
        os.setgroups([])
        os.setgid(OTHER)
        os.setuid(OTHER)
        try:
            with socket.socket(socket.AF_UNIX) as sock:
                sock.connect(str(path))
        except PermissionError:
            os._exit(0)
        os._exit(1)
    assert os.waitpid(child, 0)[1] == 0


def main():
    assert os.getuid() == 0 and Path('/.dockerenv').exists()
    RUNTIME.mkdir(mode=0o700, parents=True)
    os.chown(RUNTIME, UID, UID)
    COUNT.touch(mode=0o600)
    os.chown(COUNT, UID, UID)
    uptime_bin = Path('/usr/bin/uptime')
    uptime_bin.rename('/usr/bin/uptime.real')
    uptime_bin.write_text(f'#!/bin/sh\n[ "$#" = 0 ] && [ "$PWD" = /tmp ] || exit 2\nprintf x >>{COUNT}\nexec /usr/bin/uptime.real\n')
    uptime_bin.chmod(0o755)
    with Terminal([BIN]) as unconfigured:
        assert unconfigured.wait() == 1 << 8

    with Control() as control:
        control.frame('start', seq=1, job_id=1, operation='uptime')
        assert control.wait() == 1 << 8
        absent(control.path)
        assert COUNT.read_bytes() == b''

    with ExitStack() as stack:
        tab = bash(stack)
        other = bash(stack)
        unauthorized = bash(stack, OTHER)
        control = stack.enter_context(Control())
        wrong_uid(control.path)
        control.pair(unauthorized)
        control.quiet()
        control.pair(tab)
        paired = control.event('paired', 2)
        assert paired['uid'] == UID
        control.generation = paired['shell_generation']
        control.pair(other)
        control.quiet()
        ticket = control.ticket(1)
        control.approval('reserve', ticket)
        other.send(f'__zzabba_shared_locator={control.code[:32].decode()}\n'.encode())
        other.read(rb'zzabba\$ ')
        other.send(KEY_CLAIM)
        control.quiet()
        other.send(b'echo other-unchanged\n')
        other.read(rb'\rother-unchanged\r\n')
        tab.send(b'printf "<%s>\\n" "\xed\x95\x9c' + KEY_CLAIM + b'\xea\xb8\x80"\n')
        tab.read(rb'\r<\xed\x95\x9c\xea\xb8\x80>\r\n')
        control.quiet()
        tab.send(KEY_CLAIM)
        assert control.event('claim', 3)['ticket'] == ticket
        control.approval('deny', ticket)
        control.quiet()
        assert b'\r\n/usr/bin/uptime\r\n' not in tab.transcript
        assert COUNT.read_bytes() == b''
        ticket = control.ticket(2)
        control.approval('reserve', ticket)
        control.quiet()
        tab.send(KEY_CLAIM)
        assert control.event('claim', 4)['ticket'] == ticket
        control.approval('grant', ticket)
        assert control.event('started', 5)['job_id'] == 2
        output = control.event('output', 6)
        assert output['stream'] == 'stdout' and output['job_id'] == 2
        uptime = base64.b64decode(output['data'], validate=True).rstrip(b'\n')
        assert b'load average' in uptime
        assert control.event('exited', 7)['code'] == 0
        tab.read(re.escape(uptime) + rb'\r\n')
        tab.send(KEY_CLAIM)
        control.quiet()
        tab.send(b'echo main-unchanged\n')
        tab.read(rb'\rmain-unchanged\r\n')
        control.quiet()
        assert tab.transcript.count(b'\r\n/usr/bin/uptime\r\n') == 1
        assert b'/usr/bin/uptime' not in other.transcript + unauthorized.transcript
        assert all(control.code not in t.transcript for t in (tab, other, unauthorized))
        control.approval('reserve', control.ticket(3))
        tab.send(b'exit\n')
        tab.wait()
        absent(control.path)
        control.wait()

    with ExitStack() as stack:
        tab = bash(stack)
        control = stack.enter_context(Control())
        control.pair(tab)
        control.generation = control.event('paired', 2)['shell_generation']
        control.approval('reserve', control.ticket(1))
        os.kill(control.pid, signal.SIGKILL)
        control.wait()
        absent(control.path)
        tab.send(KEY_CLAIM)
        tab.read(rb'zzabba\$ ')
        tab.send(b'echo after-disconnect\n')
        tab.read(rb'\rafter-disconnect\r\n')
        assert b'/usr/bin/uptime' not in tab.transcript

    with ExitStack() as stack:
        tab = bash(stack)
        control = stack.enter_context(Control())
        control.pair(tab)
        control.generation = control.event('paired', 2)['shell_generation']
        ticket = control.ticket(1)
        control.approval('reserve', ticket)
        tab.send(KEY_CLAIM)
        assert control.event('claim', 3)['ticket'] == ticket
        control.approval('grant', {**ticket, 'main_generation': 99})
        control.wait()
        absent(control.path)
        tab.send(b'echo after-bad-generation\n')
        tab.read(rb'\rafter-bad-generation\r\n')
        assert b'/usr/bin/uptime' not in tab.transcript

    with ExitStack() as stack:
        tab = bash(stack)
        control = stack.enter_context(Control())
        control.pair(tab)
        control.generation = control.event('paired', 2)['shell_generation']
        control.frame('reserve', seq=1, ticket=control.ticket(1), operation='uptime; id')
        assert control.wait() == 1 << 8
        absent(control.path)
        tab.send(KEY_CLAIM)
        tab.read(rb'zzabba\$ ')
        assert b'/usr/bin/uptime' not in tab.transcript

    assert COUNT.read_bytes() == b'x'
    print('PASS: one approved uptime; no default/direct start, invalid operation, denial, wrong UID/tab/generation, editing, replay or disconnect; input preserved')


if __name__ == '__main__':
    main()
