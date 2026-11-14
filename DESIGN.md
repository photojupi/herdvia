# herdvia 설계

Herdr 앱의 `hc machine add --via '<명령>' --name <이름>`에 꽂는 전송 통로다.
SSH 서버·VPN 없이, 서로 다른 공유기 뒤의 PC를 Herdr 사이드바에 모으는 것이 목적이다.

통로는 herdr 프로토콜을 해석하지 않는다. 바이트만 옮긴다.

## 실측으로 확정된 hc 동작 (hc 0.1.21, macOS)

- hc는 via 명령 뒤에 인자 **하나**를 붙여 실행한다. 표준 입력은 터미널이 아니다.
- 넘어오는 줄은 connect 줄 하나뿐이다. OS 판별 명령은 오지 않았다.
  - 기본: `sh -c 'p=$(command -v herdr-core) || p=$HOME/.local/bin/herdr-core; [ -x "$p" ] || { echo "herdr-core is not installed here" >&2; exit 127; }; exec "$p" connect'`
  - 세션 지정 시 끝부분이 `HC_SESSION=<이름> exec "$p" connect'`로 바뀐다.
- 머신당 오래 유지되는 via 프로세스 1개. exec·터미널·이벤트가 모두 그 위로 흐른다.
  머신 등록 시점에만 짧은 via가 한 번 더 뜬다. 동시 여러 개를 처리할 수 있어야 한다.
- via가 끝나면 hc는 약 0.05초 안에 다시 띄운다. 계속 실패하면 0.3초부터 두 배씩 늘린다.
- via 종료 코드 127은 "hc 미설치"로 표시된다. 그 외 0이 아닌 코드는 via가 stderr에 쓴 글과 함께 "연결 불가"로 표시된다.
- 통로가 끊겨도 원격 터미널은 살아 있다.
- `herdr-core connect`는 표준 입력이 닫히면 약 0.1초 안에 코드 0으로 끝난다.

## 구성

실행 파일 하나 `herdvia`, 단일 crate.

| 명령 | 역할 |
|---|---|
| `herdvia agent` | 상주 프로그램(포그라운드). 로컬 통신 통로를 열고 요청을 처리한다 |
| `herdvia via <피어> <줄>` | hc가 부르는 통로. 로컬 에이전트에 붙어 바이트를 중계한다 |

`id`, `allow`, `revoke`, `status`, `agent install`은 M3 이후에 넣는다.

데이터 폴더는 환경 변수 `HERDVIA_DIR`이 있으면 그 값, 없으면 OS 규약의 사용자별 로컬 데이터 폴더다.
Unix에서는 폴더 권한 0700, 소켓 `agent.sock` 권한 0600이다.

## 프레임 형식

로컬 통신과 (M3의) 네트워크 스트림에 같은 형식을 쓴다.

`tag(u8) + len(u32, big-endian) + payload(len바이트, 최대 65536)`

| tag | 이름 | payload | 방향 |
|---|---|---|---|
| 0 | OPEN | 로컬 통신: `피어이름\0줄`. 네트워크: `줄`. UTF-8, 최대 8192바이트. 반드시 첫 프레임 | 여는 쪽 → 실행 쪽 |
| 1 | STDIN | 입력 바이트 | 여는 쪽 → 실행 쪽 |
| 2 | STDIN_EOF | 없음. 받는 쪽은 자식의 표준 입력을 닫는다 | 여는 쪽 → 실행 쪽 |
| 3 | STDOUT | 출력 바이트 | 실행 쪽 → 여는 쪽 |
| 4 | STDERR | 오류 출력 바이트 | 실행 쪽 → 여는 쪽 |
| 5 | EXIT | i32 big-endian. 시그널 종료는 128+n | 실행 쪽 → 여는 쪽 |
| 6 | ERROR | UTF-8 사유 | 실행 쪽 → 여는 쪽 |

- EXIT는 STDOUT·STDERR를 모두 보낸 뒤 마지막에 보낸다.
- 규칙에 어긋난 프레임(알 수 없는 tag, 길이 초과, OPEN 누락·중복)은 프로토콜 오류로 보고 연결을 끊는다.

## 동작

### via

1. 에이전트의 로컬 통신 통로에 붙는다. 에이전트가 없으면 기다리지 않고 stderr에 한 줄을 쓰고 255로 끝낸다.
2. OPEN을 보낸다.
3. 표준 입력을 읽는 대로 STDIN 프레임으로 보낸다. 입력이 끝나면 STDIN_EOF를 보낸다.
4. STDOUT은 표준 출력에, STDERR는 표준 오류에 그대로 쓴다. EXIT를 받으면 그 코드로 끝낸다.
5. ERROR를 받거나 EXIT 없이 끊기면 stderr에 사유를 쓰고 255로 끝낸다.

via는 표준 출력에 원격 바이트 외에는 아무것도 쓰지 않는다. 로그는 전부 표준 오류로 간다. 아무것도 묻지 않는다.

### 실행 쪽

1. OPEN의 줄을 허용 형태와 **정확 일치**로 검사한다. 허용하는 것은 위의 connect 줄 두 형태뿐이고,
   세션 이름은 `[A-Za-z0-9._-]{1,64}`만 받는다. 어긋나면 ERROR를 보내고 끊는다. 거부한 줄은 로그에 남긴다.
2. `/bin/sh -c <줄>`을 사용자 홈에서 실행한다. 터미널(PTY) 없이 파이프로 잇는다.
3. 자식의 출력은 읽는 대로 프레임으로 보낸다. 자식이 끝나면 남은 출력을 모두 보낸 뒤 EXIT를 보낸다.
4. 상대가 끊기면 자식의 표준 입력을 닫고 프로세스 그룹을 정리한다.

이 검사는 권한을 줄이는 장치가 아니다. `herdr-core connect`에 닿으면 무엇이든 실행할 수 있다.
오용 표면을 줄이고 이상을 탐지하는 장치다. 실제 보안 경계는 M3의 피어 키 인증이다.

### M2의 에이전트

M2에는 네트워크가 없었다. 에이전트는 OPEN의 피어 이름과 무관하게 자기 PC에서 실행했다(M3에서 없앤다).
실행 쪽 로직은 "프레임을 주고받는 양방향 스트림"만 받는 함수로 만들어, M3에서 같은 함수를 네트워크 스트림에 그대로 쓴다.

## 반응속도 규칙

- 읽는 즉시 쓰고 flush한다. 묶어 보내지 않는다.
- 입력 방향과 출력 방향을 서로 다른 태스크로 처리한다. 한쪽이 막혀도 다른 쪽이 멈추지 않아야 한다.
- 버퍼는 한 번 읽기(최대 65536바이트) 수준으로 얕게 둔다.

## 종료 코드

| 상황 | via 종료 코드 |
|---|---|
| 원격 자식이 끝남 | 자식의 코드 그대로 (미설치면 줄 자체가 127을 낸다) |
| 통로 오류, 에이전트 없음, 줄 거부, EXIT 없이 끊김 | 255 |

## M3: 네트워크 연결

전송은 iroh 1.4.x다. 암호화와 NAT 통과는 iroh가 제공하는 것만 쓴다.
기본 설정(`presets::N0`)으로 n0의 공개 중계 서버와 주소 조회를 쓴다. 사용자 설정 옵션은 두지 않는다.
ALPN은 `herdvia/1`이다.

### 명령

| 명령 | 역할 |
|---|---|
| `herdvia id` | 내 EndpointId 전체와 확인용 지문을 표준 출력에 쓴다. 키가 없으면 만든다 |
| `herdvia allow <이름> <EndpointId> (--inbound \| --outbound \| --both)` | 피어를 등록하거나 권한을 바꾼다 |
| `herdvia revoke <이름>` | 피어를 지우고, 그 피어와의 연결을 즉시 끊는다 |
| `herdvia status` | 피어별 이름, 권한, 연결 여부, 직접/중계, 왕복 시간을 쓴다 |

- 지문은 EndpointId 문자열의 앞 16자를 4자씩 `-`로 묶은 것이다. `herdvia id`의 첫 줄은 EndpointId, 둘째 줄은 지문이다.
- 이름은 `[A-Za-z0-9._-]{1,32}`다.
- inbound: 그 피어가 이 PC에서 명령을 실행해도 된다. outbound: 이 PC가 그 피어로 걸어도 된다.
- `allow`, `revoke`는 에이전트가 떠 있으면 즉시 반영되고, 떠 있지 않아도 파일에는 반영된다.
- `status`는 에이전트가 떠 있어야 한다. 없으면 한 줄을 stderr에 쓰고 255로 끝낸다.

### 파일 (데이터 폴더 안)

- `secret.key`: 비밀키. 권한 0600. 권한이 더 넓으면 에이전트와 `id`는 시작을 거부한다.
- `peers`: 피어 목록. 권한 0600. 한 줄에 `이름 EndpointId 권한`(권한은 `in`, `out`, `both`).

### 에이전트

- root로 실행하면 거부한다.
- **받는 쪽**: 연결의 상대 EndpointId가 inbound 피어가 아니면 연결을 끊는다. 처음 본 피어를 자동으로 믿지 않는다.
  끊은 시도는 `pending`을 위해 메모리에 기록만 한다(아래 "피어 등록: pending / accept").
  허용된 연결의 양방향 스트림 하나마다 M2의 실행 쪽 함수를 그대로 돌린다(네트워크 OPEN의 payload는 줄).
  스트림을 받을 때마다 권한을 다시 확인한다.
- **거는 쪽**: via의 OPEN에 적힌 이름이 outbound 피어가 아니면 ERROR로 거부한다.
  outbound 피어와는 연결을 미리 맺어 두고, 실패하거나 끊기면 다시 건다. 간격은 1초에서 두 배씩 늘려 최대 30초이고,
  연결이 10초 이상 유지됐다가 끊긴 경우에만 1초로 되돌린다. 받는 쪽이 핸드셰이크 직후 403으로 거부해도 connect는 성공으로 돌아오므로,
  connect가 성공할 때마다 되돌리면 거부당하는 동안 약 1초 간격으로 끝없이 다시 걸게 된다.
  그렇게 곧바로 거부당하는 연결도 잠깐은 연결 목록에 들어가므로, 거는 쪽 `status`는 그동안 `connected`를 잠깐 보일 수 있다.
  연결 확인은 받는 쪽 `status`로 하는 편이 확실하다.
  via 요청 때 연결이 없으면 최대 5초 기다리고, 안 되면 ERROR를 돌려준다.
  연결되면 스트림을 열어 OPEN(줄)을 쓰고, 이후 프레임은 양방향으로 그대로 옮긴다.
- via가 끊기면 스트림을 리셋한다. 스트림이나 연결이 끊기면 실행 쪽은 자식을 정리하고, 거는 쪽 via는 255로 끝난다.
- 피어 목록이 바뀌면 지워졌거나 권한을 잃은 피어의 연결과 실행 중인 자식을 즉시 정리한다.
- iroh의 로그는 기본으로 끈다. 에이전트의 로그에는 접속·거부·종료 같은 사건만 남기고 스트림 내용은 남기지 않는다.
- 에이전트가 실행 중에 쓰는 로그 줄은 UTC 시각으로 시작한다(`2026-11-14T14:17:03Z herdvia: connected <id>`).
  시작하다 실패할 때의 마지막 오류 줄은 CLI와 같은 출력이라 시각이 없다.
- 자동 시작의 로그 파일(M4, M5b)에서는 180일이 지난 줄을 지운다. 데이터 폴더 잠금을 얻은 직후 한 번, 그 뒤 24시간마다 한다.
  macOS의 타이머는 잠자기 동안 멈추므로 한 시간마다 깨어나 벽시계로 24시간이 지났는지 본다.
  앞에서부터 읽어, 180일 안의 시각이 붙은 첫 줄보다 앞에 있는 마지막 만료 줄까지 지운다. 시각이 없는 줄(이전 버전이 쓴 줄, 시작 실패의 오류 줄 등)은 그다음 시각이 붙은 줄을 따른다.
  지울 것이 없으면 쓰지 않는다. 있으면 남길 내용을 파일 앞으로 옮겨 쓰고 길이를 줄인다. 파일을 새로 만들지 않으므로 이어 쓰는 핸들이 그대로 유효하다.
  고쳐 쓰는 동안 자기 표준 오류를 잠가 그사이의 로그 줄을 잃지 않는다. 실패하면 시각 붙은 한 줄을 남기고 다음 차례에 다시 한다.
  정리는 에이전트 루프 안에서 끝까지 기다리므로, 정상 종료와 리스너 해제는 진행 중인 정리가 끝난 뒤에 일어난다.
  다만 강제 종료(Windows의 `agent install` 재실행·`uninstall`이 쓰는 `TerminateProcess`, SIGKILL)가 고쳐 쓰는 도중에 오면 파일 끝에 옛 내용 일부가 겹쳐 남을 수 있고, 같은 로그 파일에 다른 프로세스가 동시에 이어 쓰면 그 순간의 줄이 잘릴 수 있다(프로세스 사이에는 잠그지 않는다).
  macOS는 표준 오류가 `~/Library/Logs/herdvia.log`에 이어 쓰고 있을 때만(O_APPEND, 연 파일과 dev/ino 비교), Windows는 `agent --log`로 연 파일일 때만 지운다.
  터미널이나 다른 파일로 가는 수동 실행의 표준 오류와 Linux에서는 지우지 않는다. macOS에서는 수동 실행이라도 표준 오류가 그 로그 파일에 이어 쓰면 똑같이 지운다.

### 로컬 통신 추가

tag 7 CONTROL을 추가한다. 첫 프레임으로만 올 수 있고 payload는 `reload`, `status`, `pending`, `doctor` 중 하나다(`pending`과 `doctor`는 나중에 추가).
그 밖의 payload는 프레임을 읽는 단계에서 프로토콜 오류로 보고 연결을 끊는다.
에이전트는 결과 글을 STDOUT 프레임으로, 끝을 EXIT 0으로 답한다. 네트워크 스트림에서는 CONTROL을 받지 않는다.

### M3에 넣지 않는 것

초대 코드, 자체 중계 서버 설정, 자동 시작, Windows, 키 교체, `hc machine add` 자동 실행.
자동 시작은 M4, Windows는 M5에서 넣었다. 초대 코드는 지금도 없고, 대신 나중에 `pending`/`accept`를 넣었다(아래).

## M4: macOS 자동 시작

| 명령 | 역할 |
|---|---|
| `herdvia agent install` | 로그인 시 에이전트가 자동으로 뜨도록 등록하고 바로 시작한다 |
| `herdvia agent uninstall` | 에이전트를 멈추고 등록을 지운다 |

- 사용자별 LaunchAgent다. 파일은 `~/Library/LaunchAgents/io.github.photojupi.herdvia.agent.plist`, 라벨은 `io.github.photojupi.herdvia.agent`.
  시스템 전체 데몬으로는 등록하지 않는다(herdr-core가 사용자 로그인 세션에서 돌아야 한다).
- 실행 경로는 `install`을 실행한 그 실행 파일의 절대 경로다. 임시 폴더나 빌드 폴더(`target/`) 안에서 실행하면
  경고를 stderr에 쓰되 등록은 한다.
- `RunAtLoad`와 `KeepAlive`를 켠다. 로그는 `~/Library/Logs/herdvia.log`에 표준 출력·오류를 모은다. 180일이 지난 줄은 에이전트가 지운다(M3).
- plist의 `EnvironmentVariables`에 `PATH`를 넣는다. 값은 `$HOME/.local/bin`을 앞에 둔 기본 경로다
  (connect 줄은 `$HOME/.local/bin/herdr-core`로도 찾으므로 필수는 아니다).
- `HERDVIA_DIR`이 설정된 채 `install`하면 거부한다. 자동 시작은 기본 데이터 폴더만 쓴다.
- 등록·해제는 `launchctl bootstrap gui/<uid>` / `launchctl bootout gui/<uid>/io.github.photojupi.herdvia.agent`를 쓴다.
- `install`은 이미 등록돼 있으면 새 내용으로 바꾸고 다시 시작한다. `uninstall`은 등록이 없어도 성공한다.
- 에이전트가 이미 수동으로 떠 있으면 `install`은 실패한다고 알리고 아무것도 바꾸지 않는다.

## M5: Windows

### 실측 (hc 0.1.21, macOS에서 Windows인 척하는 가짜 via로 측정)

- hc는 먼저 Unix 줄(`sh -c '...'`)을 보낸다. 그것이 0도 127도 아닌 코드(측정은 1)로 끝나면 Windows 줄을 보낸다.
- Windows 줄(기본):
  `cmd /d /c "if exist .\AppData\Local\Programs\herdr-core\herdr-core.exe (.\AppData\Local\Programs\herdr-core\herdr-core.exe connect) else (echo herdr-core is not installed here 1>&2 & exit /b 127)"`
- Windows 줄(세션 지정): 위에서 `connect`가 `connect --session <이름>`으로 바뀐다.
- hc의 세션 이름 규칙: `a-z 0-9 - _ .` 1~32자, 글자나 숫자로 시작.
- 경로가 상대 경로이므로 실행 위치는 사용자 홈(`%USERPROFILE%`)이어야 한다.
- Windows의 hc는 via를 "프로그램 + 인자"로 직접 실행하고(셸 없음, 콘솔 창 없음) 원격 줄을 마지막 인자 하나로 붙인다(hc 도움말).
- herdr-core의 Windows 빌드는 x64만 있다.

### 범위

M5a: Windows에서 `agent`, `via`, `id`, `allow`, `revoke`, `status`가 동작한다(에이전트는 콘솔에서 수동 실행).
M5b: Windows 자동 시작(`agent install`/`uninstall`). M5a 실기 확인 뒤에 한다.

### M5a 설계

- 대상은 `x86_64-pc-windows-gnu`(맥에서 mingw-w64로 교차 빌드)다. 공개 배포용 빌드 방식은 M7에서 정한다.
- 운영체제별 코드는 `ipc`, `exec`, `template`의 줄 판정, 데이터 폴더, root 거부에만 둔다.
  프레임, 네트워크, 피어 목록, via·에이전트의 흐름은 공유한다.
- 데이터 폴더: `%APPDATA%\herdvia`. `HERDVIA_DIR`이 있으면 그것을 쓴다(Unix와 같다).
- 로컬 통신: 이름 있는 파이프 `\\.\pipe\herdvia-<사용자 SID>`. `HERDVIA_DIR`이 있으면 그 경로의 해시를 이름 끝에 붙여 구분한다.
  - 파이프의 보안 설정은 현재 사용자 SID에만 전체 권한을 준다. 원격 클라이언트는 거부한다.
  - 에이전트는 첫 인스턴스로만 파이프를 만든다(이미 있으면 "agent is already running"). 이것이 단일 실행 보장이다.
  - via·관리 명령은 접속한 파이프의 소유자 SID가 자신과 같은지 확인하고, 다르면 접속을 끊고 오류를 낸다.
  - 파이프가 없으면 Unix의 "에이전트 없음"과 같은 오류·종료 코드(255)를 낸다.
- 실행 쪽(Windows 에이전트가 OPEN을 받았을 때):
  - Windows 줄 두 형태와 정확히 일치하면 셸 없이 직접 실행한다. `%USERPROFILE%\AppData\Local\Programs\herdr-core\herdr-core.exe`가
    있으면 인자 `connect` 또는 `connect --session <이름>`으로, 작업 폴더 `%USERPROFILE%`에서, 콘솔 창 없이 띄운다.
    없으면 STDERR `herdr-core is not installed here`와 EXIT 127을 보낸다.
  - Unix 줄 두 형태와 정확히 일치하면 아무것도 실행하지 않고 STDERR
    `'sh' is not recognized as an internal or external command,`를 보낸 뒤 EXIT 1을 보낸다(hc가 Windows 줄로 다시 시도한다).
  - 그 밖의 줄은 Unix와 같이 ERROR로 거부한다.
  - 연결이 끊기면 직접 띄운 자식(`herdr-core.exe connect`)만 종료한다. 자손은 건드리지 않는다.
    `connect`가 코어 서버를 자손으로 띄울 수 있고, 그것까지 죽이면 그 PC의 터미널이 모두 사라지기 때문이다.
    Job Object는 쓰지 않는다. 에이전트가 죽으면 자식은 stdin이 닫혀 스스로 끝난다(M1 실측: stdin EOF 후 약 0.1초).
  - EXIT 코드는 자식의 종료 코드 그대로다.
- Unix 에이전트는 Windows 줄을 계속 거부한다.
- root 거부는 Unix에만 있다. Windows에서는 검사하지 않는다.
- `secret.key`와 `peers`는 사용자 프로필의 기본 권한을 그대로 쓴다(별도 ACL을 걸지 않는다).
- `agent install`/`uninstall`은 M5a의 Windows에서 "지원하지 않음" 오류를 낸다.
- 기존 통합 테스트(`tests/`)는 Unix 전용으로 둔다. Windows 쪽은 줄 판정 등 순수 로직만 단위 테스트한다.

### M5a 실기 결과 (Windows 11 10.0.26300, hc 0.1.21)

- 맥북→Windows: 직접 연결 4ms, 원격 명령 중앙값 89ms, 8MB 0.66초, 통로 재시작 후 5.1초에 복구, 원격 터미널 유지.
- Windows→맥북 28ms, Windows→맥미니 33ms(원격 명령 중앙값). Windows 로컬 `hc exec`가 75ms라서 89ms의 대부분은 Windows의 프로세스 생성 시간이다.
- `DeviceIoControl(FSCTL_PIPE_PEEK)`는 이 Windows에서 오류 1(ERROR_INVALID_FUNCTION)로 실패했다.
  끊김 감지는 `NtQueryInformationFile(FilePipeLocalInformation)`으로 바꿨고, 조회가 실패하면 감시를 포기하되 세션은 유지한다.

### M5b 설계: Windows 자동 시작

| 명령 | 역할 |
|---|---|
| `herdvia agent install` | 로그온 시 에이전트가 창 없이 뜨도록 등록하고 바로 시작한다 |
| `herdvia agent uninstall` | 에이전트를 멈추고 등록을 지운다 |

- 관리자 권한 없이 동작해야 한다. 등록은 현재 사용자 레지스트리의 Run 키다:
  `HKCU\Software\Microsoft\Windows\CurrentVersion\Run`, 값 이름 `herdvia`,
  값 `conhost.exe --headless "<실행 파일 절대 경로>" agent --log`.
  `conhost.exe --headless`는 콘솔 창을 만들지 않고 콘솔 프로그램을 띄운다. 서비스나 "로그온 여부와 무관" 작업으로는 등록하지 않는다
  (herdr-core가 사용자의 대화형 세션에서 돌아야 한다).
- `agent --log`는 Windows 전용이다. 표준 출력·오류를 `%LOCALAPPDATA%\herdvia\herdvia.log`에 이어 쓰도록 바꾼 뒤 평소의 `agent`로 동작한다. 이 파일에서 180일이 지난 줄을 지운다(M3). `--log` 없는 `agent`는 지우지 않는다.
- `install`은 등록한 뒤 같은 명령을 콘솔 창 없이, 자신과 분리된 프로세스로 바로 띄운다(재로그온을 기다리지 않는다).
  시작한 에이전트의 파이프가 5초 안에 생기지 않으면 오류를 낸다(등록은 남긴다. 오류 문구에 로그 경로를 적는다).
- 실행 경로는 `install`을 실행한 그 실행 파일이다. 임시 폴더나 `target` 안이면 경고하되 등록은 한다(macOS와 같다).
- `HERDVIA_DIR`이 설정된 채 `install`하면 거부한다(macOS와 같다).
- `install`은 변경 전에 `TokenElevation`을 검사해 상승된 관리자 토큰이면 거부하고, 관리자 권한이 아닌 일반 PowerShell에서 실행하도록 안내한다(`uninstall`에는 적용하지 않는다).
- `install`은 등록 전에 실행 파일 경로에 `%`가 있으면 거부한다(conhost의 환경 변수 확장 방지). Run 명령줄은 NUL을 제외하고 최대 259 UTF-16 코드 단위다.
- 에이전트가 이미 떠 있을 때의 `install`: Run 값이 이미 있으면(재설치) 떠 있는 에이전트를 끝내고 새로 등록·시작한다.
  Run 값이 없으면(수동 실행 중) 실패한다고 알리고 아무것도 바꾸지 않는다.
- `uninstall`은 Run 값을 지우고 떠 있는 에이전트를 끝낸다. 등록이 없거나 에이전트가 없어도 성공한다. 데이터 폴더와 로그는 남긴다.
- 떠 있는 에이전트를 끝내는 방법: 파이프에 접속해 소유자 SID를 확인한 뒤 `GetNamedPipeServerProcessId`로 얻은 프로세스를 종료하고,
  파이프가 사라질 때까지 최대 5초 기다린다.
- 넣지 않는 것: 에이전트가 죽었을 때의 자동 재시작(다음 로그온까지 꺼져 있다). M6 실사용 결과를 보고 정한다.
  로그 파일은 크기 대신 기간으로 관리한다. 180일이 지난 줄을 지운다(M3).

## 피어 등록: pending / accept

| 명령 | 역할 |
|---|---|
| `herdvia pending` | 받는 쪽 에이전트가 inbound 권한이 없어 거부한 접속 시도를 쓴다 |
| `herdvia accept <이름> <지문> (--inbound \| --both)` | 지문이 일치하는 시도 하나의 전체 EndpointId를 피어로 등록한다 |

기존 `herdvia allow <이름> <EndpointId> --inbound`는 그대로 둔다. accept는 받는 쪽에 64자 EndpointId를 옮겨 오지 않고 등록하는 또 하나의 길이다.

### 흐름

1. 거는 쪽 A: `herdvia allow B <B의 EndpointId> --outbound`. 붙여 넣은 B의 ID는 지금처럼 지문으로 대조한다.
2. A의 에이전트가 B로 건다. B는 A가 inbound 피어가 아니므로 핸드셰이크 직후 거부하고, A의 EndpointId를 대기 목록에 남긴다.
3. 받는 쪽 B: `herdvia pending`으로 시도를 보고, A 화면의 `herdvia id` 둘째 줄을 직접 보고 입력해 `herdvia accept A <지문> --inbound`.
4. B가 A를 inbound로 등록한다. A가 다음에 다시 걸 때 연결된다. 거는 쪽의 재시도 간격(최대 30초) 때문에 최대 약 30초가 걸린다.

### 왜 이 방식인가

- 짧은 초대 코드로 키를 주고받으려면 코드와 키를 이어 줄 서버가 필요하다. 이 방식은 거는 쪽이 실제로 걸어 온 연결을 쓰므로 서버를 더 두지 않는다.
- 처음 걸어 온 피어를 자동으로 믿는 방식은 "처음 본 피어를 자동으로 믿지 않는다"는 원칙(M3)에 어긋난다. 여기서는 받는 쪽 사람이 accept해야만 등록된다.
- 지문 대조가 선택이 아니라 입력이다. `allow`는 붙여 넣은 ID를 눈으로 대조하라고 안내할 뿐, 대조를 건너뛰어도 동작한다.
  accept는 상대 화면에서 읽은 지문을 입력해야 동작하고, 그 지문과 일치하는 시도가 대기 목록에 있어야 한다.
- 받는 쪽에서 손으로 옮기는 값이 64자가 아니라 16자다.

### 기록 규칙

- 받는 쪽(서버 쪽)이고 ALPN이 `herdvia/1`인 연결이 핸드셰이크를 통과했지만 inbound 권한이 없어 거부될 때만 기록한다.
  EndpointId는 iroh가 핸드셰이크에서 인증한 값이다. 거는 쪽(클라이언트 쪽) 연결이나 다른 ALPN은 기록하지 않는다.
- 항목은 EndpointId, 시도 횟수, 마지막으로 본 시각이다.
- 최대 16개다. 가득 찬 상태에서 새 ID가 오면 마지막으로 본 지 가장 오래된 항목을 버린다.
- 마지막으로 본 지 10분(600초)이 지난 항목은 버린다. 버린 ID가 다시 오면 횟수를 1부터 센다.
  EndpointId를 아는 누구든 시도를 만들 수 있으므로 목록이 끝없이 늘지 않게 상한과 만료를 둔다.
- 피어 목록을 다시 읽을 때(reload) inbound 권한이 생긴 ID는 버린다. 기록 직전에도 잠금 안에서 권한을 다시 확인해,
  수락 직후의 거부가 다시 기록되지 않게 한다.
- 메모리에만 둔다. 파일에 저장하지 않으므로 에이전트를 다시 시작하면 비어 있다. 그래서 `pending`과 `accept`는 에이전트가 떠 있어야 한다.
- 로컬 통신의 CONTROL `pending`으로 요청한다. 답은 status처럼 머리 줄 `fingerprint id attempts last-seen`과 항목당 한 줄
  `<지문> <EndpointId> <횟수> <마지막으로 본 뒤 지난 초>s`이고, 마지막으로 본 시각이 최근인 것부터 쓴다. 항목이 없으면 머리 줄만 쓴다.

### accept의 일치 규칙

- 지문은 hex 16자다. 4자마다 `-`가 있는 19자 형태(전부 있거나 전부 없거나)도 받고, 대소문자를 가리지 않는다. 다른 화면을 보고 손으로 치는 값이기 때문이다.
- `--outbound`는 받지 않는다. 대기 목록은 들어오는 시도이므로 inbound를 줄 때만 쓸모가 있다.
- 에이전트에서 `pending` 목록을 받아 EndpointId 앞 16자가 지문과 같은 항목을 찾는다.
  - 0개: 오류. 상대가 이 PC를 `--outbound`로 allow하고 에이전트를 띄웠는지 확인하라고 안내한다.
  - 2개 이상: 오류. 전체 EndpointId로 `herdvia allow`를 쓰라고 안내한다.
  - 1개: 그 전체 EndpointId를 `allow`와 같은 경로(`peers` 파일 갱신 후 reload)로 등록하고 `<이름> <EndpointId> <in|both>` 한 줄을 쓴다.
- 이름 충돌: 그 이름이 이미 다른 EndpointId로 등록돼 있으면 등록하지 않고 오류를 낸다(먼저 revoke하거나 다른 이름을 쓰라고 안내한다).
  `allow`는 같은 이름이면 항목을 바꾸지만, accept가 사람이 64자를 직접 보지 않은 ID로 기존 피어를 조용히 바꾸면 안 되기 때문이다.
  같은 EndpointId로 이미 있으면 `allow`처럼 그 항목을 바꾼다. 같은 EndpointId를 다른 이름으로 등록하는 것은 `allow`처럼 막지 않는다.
- 수락한 ID는 대기 목록에서 빠지므로, 이미 수락한 피어의 권한을 바꾸려면 `allow`를 쓴다.

### 64비트 접두사의 한계

- 입력받는 것은 EndpointId(공개키 32바이트)의 앞 16자, 즉 64비트뿐이다. 전체 키를 대조하는 것보다 약하다.
  실제 상대가 대기 목록에 없을 때 앞 16자가 같은 다른 키가 목록에 있으면 그 키가 등록된다. 이것을 막는 것은 특정 64비트 접두사를 가진 키를 만드는 비용뿐이다.
- 일치 항목이 둘 이상이면 거부한다. 사람이 입력한 지문으로는 어느 쪽이 상대 PC인지 구분할 수 없고, 하나를 고르면 공격자의 키를 등록할 수 있기 때문이다.
  이 경우는 전체 EndpointId로 `allow`한다.
- 목록에 보인다는 것은 신뢰의 근거가 아니다. 이 PC의 EndpointId를 아는 누구나 목록에 나타날 수 있다.
  그래서 지문은 대기 목록이 아니라 상대 PC 화면에서 읽는다. 목록의 지문을 옮겨 치면 대조가 되지 않는다.

## 설치 스크립트

- `scripts/install.sh`(macOS arm64, POSIX sh)와 `scripts/install.ps1`(Windows x64, PowerShell 5.1·7)을 릴리스에 함께 올리고 `SHA256SUMS`에도 넣는다.
  `curl -fsSL .../releases/latest/download/install.sh | sh`, `powershell -c "irm .../releases/latest/download/install.ps1 | iex"` 한 줄로 설치한다.
- 하는 일: 릴리스 zip과 `SHA256SUMS`를 받아 검증한다. 실행 파일을 고정 위치(macOS `~/.local/bin/herdvia`, Windows `%LOCALAPPDATA%\Programs\herdvia\herdvia.exe`)에 두고,
  그 폴더가 PATH에 없으면 추가한다(macOS는 로그인 셸이 zsh일 때만 `~/.zprofile`에 한 줄, Windows는 사용자 PATH). 그 뒤 `herdvia id`, `herdvia agent install`을 실행하고 다음 단계를 안내한다.
- 교체: macOS는 같은 폴더의 임시 파일로 복사해 `--version`을 실행해 본 뒤 `mv`로 바꾼다(실행 중인 파일 위에 `cp`하면 다음 시작이 `Killed: 9`로 실패한다).
  Windows는 실행 중인 exe를 덮어쓸 수 없으므로 새 파일을 같은 폴더의 `herdvia.new.exe`로 복사해 `--version`을 실행해 본 뒤, 기존 파일을 `herdvia.exe.old`로, 새 파일을 `herdvia.exe`로 이름을 바꾼다.
  `.old`는 새 `herdvia.exe`가 제자리에서 실행된 뒤에만, 쓰이지 않게 되면 그 실행이나 다음 실행에서 지운다. 중간에 실패한 실행은 `.old`를 남긴다.
- 다시 실행하면 업그레이드다. `agent install`을 다시 실행해 새 실행 파일로 에이전트를 다시 시작한다. 데이터 폴더는 건드리지 않는다.
- 환경 변수: `HERDVIA_VERSION`(특정 릴리스), `HERDVIA_NO_AGENT=1`, `HERDVIA_NO_MODIFY_PATH=1`, `HERDVIA_RELEASE_BASE`(다른 곳에서 받기, 시험용).
  스크립트에는 버전이 박혀 있지 않아서 버전 경로에서 받은 스크립트도 `HERDVIA_VERSION`이 없으면 최신 릴리스를 받는다.
- 받은 실행 파일을 확인하려고 `herdvia --version`을 둔다. 표준 출력에 `herdvia <Cargo 패키지 버전>` 한 줄을 쓰고 0으로 끝난다. 데이터 폴더나 키를 만들지 않는다.
- herdr-core는 설치하지 않는다. herdr-core는 Herdr의 것이고 Herdr의 설치 스크립트가 있다. 스크립트는 herdr-core가 없거나
  에이전트가 찾지 못하는 위치에 있으면 Herdr의 설치 명령(macOS에서 다른 곳에 있으면 `~/.local/bin`에 링크하는 명령)을 안내만 하고, 설치는 성공으로 끝낸다.
- 피어 등록과 `hc machine add`도 하지 않는다.

## 점검: doctor

| 명령 | 역할 |
|---|---|
| `herdvia doctor` | 설치와 연결이 왜 안 되는지를 한 번에 보여 준다 |

- 읽기 전용이다. 데이터 폴더, `secret.key`, `peers`, 자동 시작 등록, 에이전트 상태를 만들거나 바꾸지 않는다.
  키가 없어도 만들지 않는다(키를 만드는 `peers::load_key` 대신 `ipc::private_file`로 열어 읽기만 한다).
  실행하는 외부 프로그램은 `herdr-core --version`(2초 제한) 하나다. 출력은 앞 4096바이트만 읽고, 해석하지 못한 출력은 첫 줄 80자까지만 쓴다.
  Unix에서는 새 프로세스 그룹으로 띄워 시간이 넘으면 그룹 전체를 죽인다(Windows는 직접 자식만 죽는다). root 실행은 다른 명령처럼 거부한다.
- 출력: 표준 출력에 점검 결과 한 줄씩. 줄 머리는 `ok`, `warn`, `fail`(4칸에 맞추고 공백 하나)이고, warn과 fail 줄에는 할 일을 같은 줄에 쓴다.
  fail이 하나라도 있으면 종료 코드 1, 없으면 0이다(warn만 있으면 0). 인자 오류 같은 기존 오류는 `herdvia: …` 한 줄과 255 그대로다.
- 점검 항목(이 순서):
  1. 이 실행 파일의 버전과 경로. 임시 폴더나 `target` 아래면 warn(`agent install`이 이 경로를 기록한다. 판정은 `agent install`의 경고와 같다).
  2. 데이터 폴더. `HERDVIA_DIR`에서 왔으면 그렇다고 쓴다(자동 시작은 기본 폴더만 쓴다). 없으면 warn(`herdvia id`가 만든다).
     디렉터리가 아니거나(Unix는 심볼릭 링크도) 다른 사용자 소유이거나 읽을 수 없으면 fail. 이 사용자 소유여도 목록을 읽거나 안을 탐색할 수 없으면 fail이다(`read_dir`와 `폴더/.` 조회로 판정하고, Unix는 `chmod 700`을 안내한다). Unix는 `agent.sock` 경로가 소켓 주소 한도를 넘으면 fail.
     폴더 자체가 fail이면 3, 4를 건너뛰고, 이 줄이 fail이면(소켓 경로 포함) 7~9도 건너뛴다. 원인은 이 줄에 있으므로 뒤 줄에서 틀린 안내를 하지 않는다.
  3. `secret.key`. 없으면 warn. Unix에서 에이전트와 같은 권한 검사(이 사용자 소유, 0600 이하)에 어긋나거나 32바이트가 아니면 fail. 있으면 지문만 쓴다.
  4. `peers`. 에이전트와 같은 로더로 읽는다. 형식·권한 오류는 로더의 문구를 넣어 fail(그 줄을 고치라는 안내는 형식 오류일 때만). 방향별 개수를 쓰고, 파일이 없으면 0개다.
     권한 안내(소유자와 `chmod 600`)는 Unix에서만 쓰고, Windows에서는 이 사용자가 읽을 수 있는 일반 파일인지 보라고 쓴다. 3도 같다.
  5. herdr-core. 에이전트가 실제로 찾는 위치를 본다. Windows는 `%USERPROFILE%\AppData\Local\Programs\herdr-core\herdr-core.exe`,
     macOS는 LaunchAgent가 주는 PATH(`~/.local/bin:/usr/bin:/bin:/usr/sbin:/sbin`)의 실행 가능한 파일, 그 밖의 Unix는 현재 PATH와 `~/.local/bin`이다.
     macOS에서 현재 PATH에만 있으면 warn하고 `~/.local/bin/herdr-core`로 링크하는 `mkdir -p … && ln -s …` 명령을 쓴다. PATH의 상대 경로 항목은 현재 폴더 기준 절대 경로로 바꿔 찾으므로, 표시하는 경로와 링크 대상은 절대 경로다. 어디에도 없으면 fail이고 Herdr 설치 스크립트를 안내한다(설치는 하지 않는다).
     찾으면 `--version` 첫 줄의 마지막 단어를 버전으로 읽는다. 검증 버전(상수 `VERIFIED_CORE_VERSION`, `0.1.21`)과 다르면 warn(connect 줄이 바뀌었으면 원격이 거부한다. 로그의 `rejected command`를 보라). 실행이 실패해도 warn.
  6. 자동 시작. macOS는 plist의 `ProgramArguments` 첫 값, Windows는 Run 값을 읽기만 한다(`launchctl`은 부르지 않는다).
     등록이 없거나 다른 실행 파일을 가리키면 warn(`agent install`을 다시 실행). Windows는 Run 값을 `install`이 쓸 명령과 ASCII 대소문자를 무시하고 비교한다. Linux는 지원하지 않는다는 warn. Windows에서 관리자 권한으로 돌고 있으면 `agent install`이 거부된다는 warn을 한 줄 더 쓴다.
  7. 에이전트. 제어 요청 `doctor`를 보낸다. 응답이 없으면 fail. 버전이나 실행 파일 경로가 이 실행 파일과 다르면 warn(업그레이드 뒤 다시 시작하지 않은 경우).
     doctor가 보내는 제어 요청(`doctor`, 구버전 확인용 `status`, 8의 `status`, 9의 `pending`)은 요청마다 연결부터 응답 끝까지 3초(상수 `AGENT_TIMEOUT`)만 기다린다.
     `doctor`나 구버전 확인용 `status`가 시간을 넘기면 "실행 중일 수 있으나 3초 안에 답하지 않았다"는 fail이고 뒤 요청은 보내지 않는다. 8의 `status`가 넘기면 warn이고 9는 묻지 않는다. 9가 넘기면 warn이다.
     그래서 멈춘 에이전트를 기다리는 시간은 한 번의 제한 시간 정도다. 연결이 끊기는 것(구버전)과 시간 초과는 구별한다.
     홈 릴레이에 연결되어 있지 않으면 relay 줄이 warn이다(외부 HTTPS 연결과 프록시 환경을 확인하라. herdvia는 프록시 환경 변수를 쓰지 않는다).
  8. 피어별 상태. 기존 `status` 응답에서 피어마다 한 줄. connected는 ok이고, macOS에서 relay면 로컬 네트워크 허용 안내를 덧붙인다.
     disconnected는 warn이고, out/both면 상대 에이전트가 꺼졌거나 상대가 이 PC를 허용하지 않았다고, in이면 상대가 걸어 와야 연결된다고 안내한다. 피어가 0개면 warn 한 줄.
  9. 대기 중인 접속 시도. `pending` 응답의 항목 수를 ok 한 줄로 쓰고, 1개 이상이면 `pending`으로 확인하고 `accept`로 수락하라고 안내한다.

  relay 줄과 8, 9는 에이전트가 응답할 때만 쓴다(relay 줄은 `doctor`에 답하는 에이전트일 때만).
- 제어 요청 `doctor`의 응답은 STDOUT의 `키 값` 줄들과 EXIT 0이다.
  ```
  version <에이전트의 Cargo 패키지 버전>
  exe <에이전트 실행 파일의 경로>
  relay <연결된 홈 릴레이 URL 또는 none>
  ```
  - `exe`는 에이전트가 자기 경로를 얻지 못하면 빠진다.
  - relay는 iroh `Endpoint::home_relay_status()` 중 연결된 항목의 URL이다. 연결된 것이 없으면 `none`이다.
  - 받는 쪽은 첫 공백 뒤 나머지 전체를 값으로 읽고(경로에 공백이 있을 수 있다), 모르는 키는 무시한다. 키를 늘려도 이전 doctor가 깨지지 않는다.
- 구버전 에이전트(0.1.1 이하)는 `doctor`를 모르므로 프로토콜 오류로 연결을 끊고 로그에 `unknown CONTROL request` 한 줄을 남긴다.
  doctor는 그 경우 `status`를 다시 요청해, 답이 오면 "에이전트가 이 실행 파일보다 오래되었다. 다시 시작하라"는 warn을 쓰고 8, 9를 이어 한다. `status`도 실패하면 fail이다.
- 판정과 줄 생성은 파일 시스템·네트워크 없이 단위 테스트할 수 있는 순수 함수로 두고, 사실을 모으는 부분과 나눈다.

## 단계

- M1 (완료): 가짜 via로 hc 동작 실측.
- M2 (완료): 네트워크 없는 루프백 통로. macOS.
- M3: iroh 연결, 피어 키 허용 목록, 방향별 권한.
- M4: macOS 자동 시작. M5: Windows. M6: 4대 실사용.
