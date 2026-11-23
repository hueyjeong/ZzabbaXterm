import socket
import sys
import threading
from pathlib import Path

import paramiko


class Server(paramiko.ServerInterface):
    def __init__(self):
        self.ready = threading.Event()

    def get_allowed_auths(self, username):
        return "password"

    def check_auth_password(self, username, password):
        valid = username == "fixture-user" and password == "only-a-fixture-password"
        return paramiko.AUTH_SUCCESSFUL if valid else paramiko.AUTH_FAILED

    def check_channel_request(self, kind, chanid):
        return paramiko.OPEN_SUCCEEDED if kind == "session" else paramiko.OPEN_FAILED_ADMINISTRATIVELY_PROHIBITED

    def check_channel_pty_request(self, channel, term, width, height, width_px, height_px, modes):
        return True

    def check_channel_shell_request(self, channel):
        self.ready.set()
        return True


# 랜덤 루프백 포트와 일회용 호스트 키만 사용한다.
with socket.socket() as listener:
    listener.bind(("127.0.0.1", 0))
    listener.listen(1)
    listener.settimeout(20)
    key = paramiko.RSAKey.generate(2048)
    print(f"{listener.getsockname()[1]} {key.get_name()} {key.get_base64()}", flush=True)
    while not Path(sys.argv[1]).exists():
        threading.Event().wait(0.02)
    connection, _ = listener.accept()
    with paramiko.Transport(connection) as transport:
        server = Server()
        transport.add_server_key(key)
        try:
            transport.start_server(server=server)
        except EOFError:
            # 호스트 핀이 등록되지 않은 경우 인증 전에 연결이 닫힌다.
            sys.exit(0)
        channel = transport.accept(12)
        if channel is not None and server.ready.wait(3):
            channel.sendall(b"FIXTURE-AUTH-OK\n")
            channel.send_exit_status(0)
            channel.shutdown_write()
            # 클라이언트가 출력과 종료 코드를 읽었다는 신호다.
            channel.settimeout(5)
            try:
                channel.recv(1)
            except (TimeoutError, EOFError):
                pass
