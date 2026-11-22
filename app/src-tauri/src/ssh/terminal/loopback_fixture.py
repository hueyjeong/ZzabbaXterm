"""Test-only SSH server. Uses only fixture keys and 127.0.0.1."""
import re
import socket
import sys
import threading
from pathlib import Path

from paramiko import AUTH_FAILED, AUTH_SUCCESSFUL, OPEN_SUCCEEDED, RSAKey, ServerInterface, Transport

account = "fixture-user"
password = "fixture-password"
key_path = Path(sys.argv[1])
user_key = RSAKey.generate(2048)
user_key.write_private_key_file(str(key_path), password=password)
key_path.chmod(0o600)
host_key = RSAKey.generate(2048)


class Fixture(ServerInterface):
    def __init__(self):
        self.ready = threading.Event()
        self.result = None
        self.shell = False

    def get_allowed_auths(self, username):
        return "password,publickey"

    def check_auth_password(self, username, secret):
        return AUTH_SUCCESSFUL if (username, secret) == (account, password) else AUTH_FAILED

    def check_auth_publickey(self, username, key):
        return AUTH_SUCCESSFUL if username == account and key == user_key else AUTH_FAILED

    def check_channel_request(self, kind, channel_id):
        return OPEN_SUCCEEDED if kind == "session" else 1

    def check_channel_pty_request(self, *args):
        return True

    def check_channel_shell_request(self, channel):
        self.shell = True
        self.result = b"FIXTURE-REOPENED\n"
        self.ready.set()
        return True

    def check_channel_exec_request(self, channel, command):
        match = re.fullmatch(rb"printf 'ZZABBA-F01:%s%s\\n' '([0-9a-f]{16})' '([0-9a-f]{16})'", command)
        if not match:
            return False
        self.result = b"ZZABBA-F01:" + b"".join(match.groups()) + b"\n"
        self.ready.set()
        return True


def serve(client):
    with Transport(client) as transport:
        transport.add_server_key(host_key)
        server = Fixture()
        try:
            transport.start_server(server=server)
            channel = transport.accept(20)
            if channel and server.ready.wait(10):
                channel.sendall(server.result)
                if server.shell:
                    channel.settimeout(20)
                    while channel.recv(1024):
                        pass
                    return
                channel.send_exit_status(0)
                channel.close()
        except (OSError, EOFError):
            # A cancelled trial disconnects before opening a channel.
            pass


with socket.socket() as listener:
    listener.bind(("127.0.0.1", 0))
    listener.listen()
    print(f"{listener.getsockname()[1]} {host_key.get_name()} {host_key.get_base64()}", flush=True)
    while True:
        client, _ = listener.accept()
        threading.Thread(target=serve, args=(client,), daemon=True).start()
