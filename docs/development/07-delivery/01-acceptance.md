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
   **보조 실행 파일:** 배포 전용 `src-tauri/tauri.bundle.conf.json`의 `bundle.externalBin`에 `binaries/zxt`와 `binaries/zxt-edit`를 둔다. 두 CLI는 Cargo의 기본 `local-cli` 기능으로 빌드하고, Tauri 배포 빌드에서는 기본 기능을 꺼서 Cargo 타깃과 sidecar로 중복 포장하지 않는다. GUI는 `package.default-run`으로 유지하며 askpass는 제외한다.

   **Linux 노드 전용:** 운영자는 노드와 같은 아키텍처의 Linux 빌드 환경에서 `app/`을 열고 `cargo build --release --locked --manifest-path src-tauri/Cargo.toml --no-default-features --features ai-control --bin zxt-ai-control --target "$(rustc --print host-tuple)"`로 헬퍼를 빌드한다. 배포 위치는 [A02의 노드 준비 절차](../06-ai/02-dedicated.md#구현-순서)를 따른다. 이 타깃은 기본 Cargo·Tauri 빌드와 데스크톱 번들에서 제외한다.

3. **번들 생성:** 각 OS의 [빌드 사전 요구 사항](../00-start/01-bootstrap.md#준비물)을 갖추고 `app/`에서 `npm ci` 후 `npm run bundle`을 실행한다. 스크립트는 `cargo metadata --locked --no-deps`의 `target_directory`와 `rustc --print host-tuple`을 사용한다. 같은 네이티브 타깃에서 `--locked`로 두 CLI를 빌드해 `src-tauri/binaries/<이름>-<타깃 트리플>`(Windows는 `.exe` 포함)에 복사한 뒤 Windows NSIS 또는 Ubuntu deb를 생성한다. 기본 Tauri 설정은 sidecar 준비 없이 개발할 수 있다. `release/bundle/` 아래 설치 파일의 경로·버전·아키텍처와 SHA-256(Windows `Get-FileHash -Algorithm SHA256`, Ubuntu `sha256sum`)을 기록한다.

   설치 전에는 Windows `7z l <설치 파일.exe>` 또는 Bandizip `bz l -list:v <설치 파일.exe>`, Ubuntu `dpkg-deb -c <설치 파일.deb>`로 GUI·`zxt`·`zxt-edit`가 포함되고 제외한 헬퍼가 없는지 확인한다. 번들 생성·추출 실행과 실제 설치는 별도로 판정한다.
4. **실제 설치:** 승인된 시험 PC/계정에 같은 해시의 설치 파일을 전달한다. Windows는 생성된 설치 파일, Ubuntu는 생성된 `.deb`와 패키지 관리자를 사용한다. 서명이나 조직 정책이 설치를 막으면 배포 담당자에게 서명된 시험본을 요청하고 `미실행(설치 정책)`으로 기록한다. 설치 후 시작 메뉴·앱 목록에서 실행해 위 세 시나리오를 시연한다.

   개발 소스가 없는 시험 계정에서 GUI를 연 뒤 Windows PowerShell은 `& '<설치 폴더>\zxt.exe' list`, Ubuntu는 `/usr/bin/zxt list`를 실행한다. Windows의 `zxt.exe`는 설치한 GUI 실행 파일 옆에 있다. 파일 경로·SHA-256과 목록 응답(빈 목록 포함)을 기록한다. 전역 PATH 변경 없이 설치한 CLI가 설치본 GUI에 연결되는 것이 통과 기준이다.

   같은 설치본의 `zxt-edit --wait`에 시험 파일을 지정한다. 추가 확인 없이 가운데 탭이 열리고, 해당 편집 탭을 닫은 뒤에만 성공으로 반환하는지 기록한다.
5. **최종 판정:** 같은 빌드의 [제품 인수 기준](../../ops-shell-blueprint.md#검증)으로 두 OS에서 T1–T8을 종합한다. 해당 티켓에서 수집한 **동일 빌드·동일 환경**의 근거는 링크로 재사용한다. 빌드·실행 경로·정책이 달라진 항목은 설치본으로 재현한다. 상태는 `통과`, `실패(단계·실제값)`, `미실행(사유)`으로 분리한다. T2는 8칸, T1은 4칸, T5는 서버별, T6는 엔드포인트별 결과를 유지한다. T7은 [A03의 인수표](../06-ai/03-shared.md#검증과-완료-증거)에 따라 OS·정책별로 지정 세션 ID·세대, 승인 ID, 포커스 및 거부·중지 증거를 기록한다. 외부 도구 미설치는 미실행이지 기능 통과가 아니다.

**완료 증거:** 두 설치 파일의 식별자·해시, 설치/기동 기록, T1–T8 증거 링크, 재현에 필요한 도구 목록. 필수 조합의 실패·미실행이 남으면 만든 설치본과 사유를 인계하되 Windows·Linux 인수 완료 판정은 보류한다. Mac 장비가 준비되면 [M01](02-macos.md)로 간다.
