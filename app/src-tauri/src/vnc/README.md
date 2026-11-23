# R03 Rust 엔진 계약

워크스페이스에서 **VNC** 탭을 직접 열면 TLS 호스트·포트와 서비스 인증을 선택한다. 준비된 SSH 터널에서 연 **VNC** 탭은 지정된 원격 목적지만 사용한다. 두 경로 모두 다시 연결할 때 네이티브 승인을 받는다. 이전 입력 큐와 워커는 새 접속 전에 폐기된다.

격리 검증은 저장소 루트에서 `cargo test --locked --manifest-path app/src-tauri/Cargo.toml --lib vnc`과 `npm --prefix app test -- src/features/vnc src/features/remoteDesktop`로 실행한다. 시험은 루프백 TLS, 격리 CA와 합성 비밀번호 공급원만 사용한다. Windows 비밀번호 대화 상자는 `User32`의 `DialogBoxIndirectParamW`와 기본 Edit·Static·Button 컨트롤을 사용한다. Windows MSVC 앱과 라이브러리 시험은 기존 `app/src-tauri/build.rs`의 Common Controls v6 매니페스트를 공유한다.

## 전송과 비밀

1. 웹뷰 입력은 접속 초안이다. Rust가 호스트와 포트를 검증하고 실제 `Target`을 만들며, 네이티브 승인 후에만 `Approved`를 발급한다. 앱의 직접 경로는 첫 바이트부터 TLS 1.2 이상이며 TCP 호스트와 인증서 검증 이름이 같다. 앱은 OS 신뢰 저장소를 사용하고 격리 시험만 전용 CA를 공급한다. 평문과 VeNCrypt 재시도 경로는 없다.
2. `VncPassword`는 RFB 보안 종류 2만 선택한다. Linux에서는 `service=dev.zzabbaxterm.desktop.vnc`, `target=default`, 경로별 `user` 범위가 일치하고 32자리 16진수 `vnc-item-id`를 가진 잠금 해제 항목 하나만 승인 후보가 된다. 직접 TLS의 `user`는 `[정규화된 호스트]:포트`다. Rust가 Secret Service의 D-Bus 고유 소유자, 객체 경로, 항목 ID, 생성·변경 시각을 승인 전에 고정한다. 챌린지 이후 콜백은 승인당 한 번 같은 대상의 같은 항목만 읽는다. D-Bus 호출은 세션 버스의 고유 소유자에게만 보내며, Secret Service의 `plain` 세션을 사용한다. 철회는 RFB 응답 쓰기와 직렬화된다.
3. Windows는 검증된 직접 TLS 또는 SSH 터널에서 전체 16바이트 챌린지를 받은 뒤 로컬 Win32 입력을 연다. 60초 안에 입력한 Latin-1 1~8바이트는 같은 연결의 일회성 응답에만 사용된다. `ExplicitNone`은 별도 승인한 보안 종류 1이다. 이외 플랫폼은 비밀번호 모드를 소켓 연결 전에 거부한다.
4. 터널 초안의 `source`는 `{tabHandle,tunnelId,targetId,destination:{host,port}}`이다. 문서 증표에 묶인 R01 세대로 네이티브 승인 당시의 저장 SSH 스냅샷과 정확한 원격 루프백 목적지를 확인하고, `Protocol::Vnc` 전용 `DesktopLease`에서 이미 연결된 소켓을 인계받는다. SSH 터널과 직접 TLS는 서로 대체 경로가 아니다.
5. Linux 터널 모드의 금고 `user` 범위는 `ssh://계정@[SSH 호스트]:SSH 포트#호스트 키 지문|vnc://[원격 호스트]:원격 포트`이다. 직접 TLS 범위나 로컬 리슨 주소를 사용하지 않는다. 임대 발급 후 이 범위의 항목 ID를 별도 승인한다. 임대가 끊기면 추가 조회와 대기 중인 결과의 사용을 중단하고, RFB 인증 응답을 쓰는 각 비차단 호출은 R01 철회와 직렬화한다. 비밀·금고 객체 경로·SSH 인증 경로는 VNC IPC나 새 명령행 인자로 전달되지 않는다.

## 탭과 출력

Rust는 로드가 완료된 로컬 기본 문서에 128비트 증표를 발급한다. `vnc_*` IPC는 `x-zzabba-vnc-document`와 기존 탭·세대 권한을 함께 검사한다. Tauri 채널에는 128비트 일회용 출력 티켓만 실린다. `vnc-packet` 프로토콜은 현재 문서 증표, 원점, 티켓을 대조한 후 바이트를 한 번만 전달한다. 화면과 제어 레인은 각각 미확인 패킷 하나를 보관한다. Raw 페이로드는 256 KiB + 헤더, JSON은 2 MiB + 4 KiB까지 허용한다. 패킷은 회수 후 ACK하며, 마지막 이미지 조각은 Canvas에 실제 적용된 뒤 ACK한다. 다음 프레임의 ACK가 지연돼도 현재 화면 세대의 입력과 제어는 진행된다. 탐색·탭 닫기·재연결은 철회된 연결의 패킷을 폐기한다.

- `open`이 기존 워커를 닫고 인증·ServerInit을 새로 완료한 뒤 고유 ID를 발급한다. 연결 준비는 최초 전체 갱신 요청과 픽셀 범위 수신 후 `Change::Ready`로 판단한다. `Change::Size`가 먼저 오면 canvas를 새로 초기화하고 `Ready`까지 입력 UI를 비활성화한다.
- 탭 조작은 현재 세션 ID·연결 세대·화면 세대의 `Input`(X11 키 심볼, 포인터, 명시적 클립보드 또는 크기 요청)만 사용한다. 초기 또는 크기가 바뀐 화면은 마지막 조각의 ACK까지 입력을 보류한다. 다른 화면 세대와 폐기한 연결의 입력은 버린다. `vnc_input` 성공은 큐 수락이며 원격 적용과는 별개다.
- `Event::Frame`의 변경은 순서가 있다. `Pixels`는 RGBA8이며, `Copy`는 갱신 중인 canvas에서 크기가 같은 원본 영역을 대상으로 복사한다. 중첩 영역에도 원본 스냅샷과 같은 결과를 적용한다. 이벤트는 픽셀 길이와 경계를 모두 검증한 배치다. `CutText`는 양방향 Latin-1이고 클립보드에 쓸 때도 앱의 사용자 동작 정책을 적용한다.
- `SetDesktopSize`는 서버가 단일 전체 화면 레이아웃을 알려 준 뒤에만 보낸다. 화면 ID와 플래그는 보존한다. `Applied`는 요청과 같은 레이아웃을 담은 서버 성공 응답에 한정한다. 실패·전달 응답은 크기를 바꾸지 않으며, 후속 서버 알림은 별도 변경이다. 응답 시간 초과 시 세션을 닫는다.
- `ExtendedDesktopSize`가 든 갱신은 서버 알림과 요청 응답 모두 화면 픽셀 갱신과 분리돼야 한다. `Raw`·`CopyRect` 또는 기존 `DesktopSize`와 혼합한 배치는 전체를 거부한다.

## 엔진 선택 근거

[`vnc-rs` 0.6.1 공개 소스](https://docs.rs/crate/vnc-rs/0.6.1/source/src/client/)에서 세 부분을 대조했다. `connector.rs`는 제안된 `None`을 `VncAuth`보다 먼저 선택하며 보안 종류를 고정하는 API가 없다. `messages.rs`는 양방향 CutText를 구현하지만 UTF-8 바이트와 손실 복원을 사용한다. `resize.rs`와 `connection.rs`에는 `SetDesktopSize` 및 서버 결과 처리, CopyRect 경계 검사가 있다. 커넥터의 협상 바이트를 중간에서 바꾸거나 이 포크를 통째로 유지하는 대신 [RFB 공개 명세](https://github.com/rfbproto/rfbproto/blob/master/rfbproto.rst)에 맞춘 좁은 엔진을 선택했다. DES는 표준 VNC 챌린지 응답에만 사용한다. 전송 보안은 R01 또는 이름이 검증된 TLS가 담당한다.

이 엔진이 협상하는 범위는 RFB 3.3/3.7/3.8, Raw, CopyRect, DesktopSize, ExtendedDesktopSize와 기본 CutText다. 압축 인코딩, 다중 모니터 크기 요청, 확장 클립보드, 추가 인증 방식은 클라이언트 범위 밖이며 실제 서버의 기능 여부와 구분한다. Raw의 네트워크 비용은 압축이 필요한 실제 경로를 검증할 때 재평가한다.

`fixture-*.pem`은 이 테스트만을 위해 생성한 로컬 CA, `localhost` 인증서와 공개 시험용 개인 키다. 테스트는 프로세스 내부에서만 CA를 사용하며 운영 신뢰 저장소에 설치할 필요가 없다.
