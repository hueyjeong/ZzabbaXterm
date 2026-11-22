# D01 · Windows와 Ubuntu에서 설치본으로 인수하기

선행: L03, C02, R02, R03, S02, A03

**사용자 결과:** 개발 서버가 없는 시험 PC에서도 설치본을 열어 로컬 셸을 쓰고, 자신에게 승인된 자격 증명과 도구를 갖춘 경우 회사 연결·파일·원격 화면·AI 보조를 이용한다. 여기서 말하는 완료는 Windows·Linux **내부 인수 가능 설치본**이다. 외부 공개, 조직 서명 키 사용, 운영 계정 배포는 별도 배포 결정이다.

### 인수 예

| Given | When | Then |
|---|---|---|
| Windows 11 및 Ubuntu 24.04 GNOME Wayland, 개발 서버가 꺼진 상태 | 각 OS용 설치본을 설치하고 가운데 메인 셸에서 `echo TERMINAL_OK`를 입력한다 | 설치본의 로컬 자산으로 창이 열리고, 해당 셸에서 출력 한 줄을 읽고 계속 입력할 수 있다. |
| AWS 도구나 AI 모델 설정이 없는 시험 계정 | 앱을 다시 시작해 로컬 셸을 연 뒤 해당 외부 기능을 선택한다 | 로컬 셸은 계속 사용할 수 있고 해당 기능에서만 필요한 준비·오류 상태가 보인다. |
| 기존 연결 설정과 두 개의 앱 소유 세션 | 창을 닫았다가 설치본을 다시 연다 | 이전 앱 소유 자식·터널이 남지 않고 설정한 대상만 복원된다. 연결 암호와 AI 승인 상태가 저장된 설정에 섞이지 않는다. |

### 실행 순서

1. **Red:** 개발 명령을 실행할 수 없는 새 시험 계정에서 무엇이 없는지 적는다. 빌드 식별자를 결정하고 [작업판의 T1–T8](../README.md#통과-지점)을 열어 필수 증거 중 없는 칸을 찾는다. 콘솔이 아닌 GUI 런처에서 실행할 것이므로 PATH가 다를 수 있다.
2. **Green:** 각 네이티브 OS에서 같은 소스와 잠금 파일을 사용한다. `app/`에서 `npm ci`, `npm test`, `cargo test --locked --manifest-path src-tauri/Cargo.toml`을 실행한다. `src-tauri/tauri.conf.json`의 앱 식별자·버전과 `frontendDist`를 확인하고, 원격 페이지 권한이나 개발 서버 URL을 출시 자산에 추가하지 않는다. 제품이 소유한 로컬 CLI 및 helper는 각 티켓에서 정한 실행 파일 이름·경로로 패키징한다. 사용자가 선택해 설치하는 원격 헬퍼, AWS CLI/플러그인, SSH Agent, AI CLI는 구별한다. 실험용 프로필·토큰·사설 키는 번들에 포함하지 않는다.
   **보조 실행 파일을 놓는 방법:** 먼저 각 CLI/헬퍼를 `cargo build --release --locked --manifest-path src-tauri/Cargo.toml --bin <이름>`으로 빌드한다. `<이름>`에는 실제 Cargo 바이너리 이름(예: `zzabba`, `zzabba-edit`)을 쓴다. 파일을 `src-tauri/binaries/<이름>-<타깃 트리플>`로 복사한다(Windows는 `.exe` 유지, 네이티브 트리플은 `rustc --print host-tuple`). 배포 전용 `src-tauri/tauri.bundle.conf.json`의 `bundle.externalBin`에는 접미사 없이 `binaries/<이름>`을 열거한다. 이렇게 해야 빈 개발 폴더에서 누락된 sidecar를 찾느라 최초 빌드가 막히지 않는다. Rust 주 패키지에 바이너리가 여러 개면 `Cargo.toml`의 `package.default-run`을 GUI 바이너리로 지정한다. [Tauri 보조 바이너리 규약](https://v2.tauri.app/develop/sidecar/)을 따른다. CLI는 설치 경로에서 직접 호출할 수 있어야 하며 시험 PC의 전역 PATH 변경은 필수가 아니다.
3. **번들 생성:** `app/`에서 Windows는 `npm run tauri build -- --config src-tauri/tauri.bundle.conf.json --bundles nsis`, Ubuntu는 같은 명령의 `nsis`를 `deb`로 바꾼다. 성공 시 CLI가 표시한 `target/release/bundle/` 아래의 설치 파일 경로·버전·대상 아키텍처·SHA-256을 기록한다. 빌드가 성공해도 설치본 실행 전에는 미검증이다. [Tauri 배포 안내](https://v2.tauri.app/distribute/)와 해당 OS 요구 사항을 따른다.
4. **실제 설치:** 승인된 시험 PC/계정에 같은 해시의 설치 파일을 전달한다. Windows는 생성된 설치 파일, Ubuntu는 생성된 `.deb`와 패키지 관리자를 사용한다. 서명이나 조직 정책이 설치를 막으면 배포 담당자에게 서명된 시험본을 요청하고 `미실행(설치 정책)`으로 기록한다. 설치 후 시작 메뉴·앱 목록에서 실행해 위 세 시나리오를 시연한다. 앱과 함께 제공한 CLI/헬퍼도 개발 소스 경로가 없는 상태에서 찾아 실행되는지 확인한다.
5. **최종 판정:** 같은 빌드의 [제품 인수 기준](../../ops-shell-blueprint.md#검증)으로 두 OS에서 T1–T8을 종합한다. 해당 티켓에서 수집한 **동일 빌드·동일 환경**의 근거는 링크로 재사용한다. 빌드·실행 경로·정책이 달라진 항목은 설치본으로 재현한다. 상태는 `통과`, `실패(단계·실제값)`, `미실행(사유)`으로 분리한다. T2는 8칸, T1은 4칸, T5는 서버별, T6는 엔드포인트별, T7은 정책·셸 모드별 결과를 유지한다. 외부 도구 미설치는 미실행이지 기능 통과가 아니다.

**완료 증거:** 두 설치 파일의 식별자·해시, 설치/기동 기록, T1–T8 증거 링크, 재현에 필요한 도구 목록. 필수 조합의 실패·미실행이 남으면 만든 설치본과 사유를 인계하되 Windows·Linux 인수 완료 판정은 보류한다. Mac 장비가 준비되면 [M01](02-macos.md)로 간다.
