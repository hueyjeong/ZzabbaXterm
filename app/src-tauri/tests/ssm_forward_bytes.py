#!/usr/bin/env python3
r"""SSM 포트 전달 바이너리 보존 검증 도구.

로컬 수신 포트(127.0.0.1:<포트>)로 접속해 0x00–0xFF 전 범위, CR/LF, 0x1b 제어열과 의사 난수를 섞은
수 MB의 바이트를 보내고, 노드의 에코 서버가 돌려준 바이트의 SHA-256을 보낸 바이트와 비교한다.
표준 라이브러리만 사용한다. Python 3.9 이상.

대상 값은 아래 환경 변수로 지정한다(필수, 기본값 없음). 예시 명령은 POSIX 셸 기준이다.
  ZZABBA_SSM_FORWARD_PROFILE  AWS CLI SSO 프로필 이름
  ZZABBA_SSM_FORWARD_REGION   리전
  ZZABBA_SSM_FORWARD_NODE     관리 노드 ID(i-… 또는 mi-…)

사용 순서

1. SSO 로그인
     aws sso login --profile "$ZZABBA_SSM_FORWARD_PROFILE"

2. 노드에 에코 서버 띄우기(127.0.0.1:9009, 기본 15분 뒤 스스로 종료)
   a) Run Command로 띄우기: 아래 명령이 send-command 매개변수 파일을 만든다.
        python ssm_forward_bytes.py node-params --port 9009 --lifetime 900 > echo-params.json
        aws ssm send-command --profile "$ZZABBA_SSM_FORWARD_PROFILE" --region "$ZZABBA_SSM_FORWARD_REGION" \
            --instance-ids "$ZZABBA_SSM_FORWARD_NODE" --document-name AWS-RunShellScript \
            --parameters file://echo-params.json
        aws ssm list-command-invocations --profile "$ZZABBA_SSM_FORWARD_PROFILE" --region "$ZZABBA_SSM_FORWARD_REGION" \
            --command-id <위 CommandId> --details
      (출력의 마지막 줄에 `LISTEN ... 127.0.0.1:9009`가 보이면 준비된 것이다.)
   b) 앱의 SSM 셸 탭에서 직접 띄우기: 아래 명령의 출력(셸 스크립트)을 SSM 셸에 붙여 넣는다.
        python ssm_forward_bytes.py node-script --port 9009 --lifetime 900
   c) 한 줄 대안(python3만 있으면 됨, Ctrl+C로 종료):
        python3 -c "import socket,threading as t;s=socket.create_server(('127.0.0.1',9009));exec('def e(c):\n while 1:\n  d=c.recv(65536)\n  if not d:break\n  c.sendall(d)\n c.close()');[t.Thread(target=e,args=(s.accept()[0],),daemon=True).start() for _ in iter(int,1)]"
      socat이 있으면: socat TCP-LISTEN:9009,bind=127.0.0.1,reuseaddr,fork EXEC:cat

3. 기준선: 공식 CLI로 전달을 열고 검사한다(다른 터미널에서 실행 상태 유지).
        aws ssm start-session --profile "$ZZABBA_SSM_FORWARD_PROFILE" --region "$ZZABBA_SSM_FORWARD_REGION" \
            --target "$ZZABBA_SSM_FORWARD_NODE" --document-name AWS-StartPortForwardingSession \
            --parameters portNumber=9009,localPortNumber=50090
        python ssm_forward_bytes.py check --port 50090

4. 앱 경로: 왼쪽 트리의 SSM 대상에서 **터널 열기** → 수신 포트 50091, 원격 호스트 비움,
   목적지 포트 9009, 서비스 TCP로 시작 → 상태가 `수신 준비됨`이 된 뒤:
        python ssm_forward_bytes.py check --port 50091
   원격 호스트 문서(AWS-StartPortForwardingSessionToRemoteHost)는 원격 호스트에 노드에서 접속 가능한
   이름(예: 노드 자신의 사설 DNS 이름)과 에코 서버를 0.0.0.0 또는 그 주소에 띄운 뒤 같은 방법으로 검사한다.

5. 정리: 터널 탭을 닫은 뒤 포트 해제와 세션 종료를 확인한다.
        python ssm_forward_bytes.py check --port 50091      # 연결 거부여야 한다(종료 코드 2)
        aws ssm describe-sessions --profile "$ZZABBA_SSM_FORWARD_PROFILE" --region "$ZZABBA_SSM_FORWARD_REGION" \
            --state History --filters key=Target,value="$ZZABBA_SSM_FORWARD_NODE" --max-items 5

종료 코드: 0 일치, 1 불일치·짧은 응답, 2 연결 실패.
"""

from __future__ import annotations

import argparse
import hashlib
import json
import random
import socket
import sys
import threading
import time

ECHO_SERVER = r'''
import socket, sys, threading, time
port, lifetime = int(sys.argv[1]), int(sys.argv[2])
server = socket.socket(socket.AF_INET, socket.SOCK_STREAM)
server.setsockopt(socket.SOL_SOCKET, socket.SO_REUSEADDR, 1)
server.bind(("127.0.0.1", port))
server.listen(16)
def echo(conn):
    with conn:
        while True:
            data = conn.recv(65536)
            if not data:
                break
            conn.sendall(data)
def stop():
    time.sleep(lifetime)
    server.close()
threading.Thread(target=stop, daemon=True).start()
while True:
    try:
        conn, _ = server.accept()
    except OSError:
        break
    threading.Thread(target=echo, args=(conn,), daemon=True).start()
'''


def payload(size: int, seed: int) -> bytes:
    """모든 바이트 값, 줄바꿈·이스케이프 조합, 의사 난수를 섞은 결정적 데이터."""
    head = bytes(range(256)) + b"\r\n\n\r\x1b[2J\x1b]0;t\x07\x1b\x1b\x00\x00\xff\xfe\r\r\n" * 64
    rng = random.Random(seed)
    body = bytearray(head)
    while len(body) < size:
        block = bytearray(rng.getrandbits(8) for _ in range(4096))
        # 4 KiB마다 CR/LF와 ESC를 경계 근처에 넣어 줄 단위 변환과 제어열 해석을 드러낸다.
        block[0:4] = b"\r\n\x1b\x00"
        block[-3:] = b"\x1b\r\n"
        body += block
    return bytes(body[:size])


def check(port: int, size: int, seed: int, repeat: int, timeout: float) -> int:
    data = payload(size, seed)
    expected = hashlib.sha256(data).hexdigest()
    print(f"보낼 바이트 {len(data):,} · SHA-256 {expected}")
    result = 0
    for attempt in range(1, repeat + 1):
        try:
            conn = socket.create_connection(("127.0.0.1", port), timeout=timeout)
        except OSError as error:
            print(f"[{attempt}] 연결 실패: {error}")
            return 2
        conn.settimeout(timeout)
        received = bytearray()
        failure: list[BaseException] = []

        def send() -> None:
            try:
                view = memoryview(data)
                for offset in range(0, len(view), 64 * 1024):
                    conn.sendall(view[offset:offset + 64 * 1024])
            except BaseException as error:  # 수신 쪽에서 함께 보고한다.
                failure.append(error)

        started = time.monotonic()
        writer = threading.Thread(target=send, daemon=True)
        writer.start()
        try:
            while len(received) < len(data):
                chunk = conn.recv(256 * 1024)
                if not chunk:
                    break
                received += chunk
        except OSError as error:
            failure.append(error)
        writer.join(timeout)
        conn.close()
        elapsed = time.monotonic() - started
        actual = hashlib.sha256(received).hexdigest()
        rate = len(received) / elapsed / 1024 / 1024 if elapsed > 0 else 0.0
        if failure:
            print(f"[{attempt}] 입출력 오류: {failure[0]}")
        if bytes(received) == data:
            print(f"[{attempt}] 일치 · {len(received):,} 바이트 · {elapsed:.2f}s · {rate:.2f} MiB/s")
            continue
        result = 1
        mismatch = next((i for i, (a, b) in enumerate(zip(data, received)) if a != b), min(len(data), len(received)))
        print(f"[{attempt}] 불일치 · 받은 바이트 {len(received):,} · SHA-256 {actual}")
        print(f"    첫 차이 위치 {mismatch} · 보낸 {data[mismatch:mismatch + 16].hex()} · 받은 {bytes(received[mismatch:mismatch + 16]).hex()}")
    return result


def node_script(port: int, lifetime: int) -> str:
    return (
        "cat > /tmp/zzabba_echo.py <<'ZZABBA_ECHO'\n" + ECHO_SERVER.strip() + "\nZZABBA_ECHO\n"
        f"setsid nohup python3 /tmp/zzabba_echo.py {port} {lifetime} >/tmp/zzabba_echo.log 2>&1 < /dev/null &\n"
        f"sleep 1; ss -ltn 'sport = :{port}'\n"
    )


def main() -> int:
    parser = argparse.ArgumentParser(description="SSM 포트 전달 바이너리 보존 검증")
    commands = parser.add_subparsers(dest="command", required=True)
    run = commands.add_parser("check", help="로컬 수신 포트로 에코 왕복을 검사한다")
    run.add_argument("--port", type=int, required=True, help="앱 또는 CLI가 연 127.0.0.1 수신 포트")
    run.add_argument("--size", type=int, default=8 * 1024 * 1024, help="연결당 보낼 바이트 수(기본 8 MiB)")
    run.add_argument("--seed", type=int, default=20261124)
    run.add_argument("--repeat", type=int, default=3, help="같은 전달로 순서대로 여는 연결 수")
    run.add_argument("--timeout", type=float, default=60.0)
    for name in ("node-script", "node-params"):
        node = commands.add_parser(name, help="노드 에코 서버 실행 스크립트 또는 send-command 매개변수를 출력한다")
        node.add_argument("--port", type=int, default=9009)
        node.add_argument("--lifetime", type=int, default=900, help="에코 서버 자동 종료까지의 초")
    args = parser.parse_args()
    if args.command == "check":
        return check(args.port, args.size, args.seed, args.repeat, args.timeout)
    script = node_script(args.port, args.lifetime)
    if args.command == "node-script":
        sys.stdout.write(script)
    else:
        json.dump({"commands": [script]}, sys.stdout, ensure_ascii=False, indent=2)
        sys.stdout.write("\n")
    return 0


if __name__ == "__main__":
    sys.exit(main())
