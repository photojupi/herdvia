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
`HERDVIA_DIR`은 절대 경로여야 한다. 빈 값이나 상대 경로는 명령을 실행한 폴더에 따라 가리키는 곳이 달라지므로
`HERDVIA_DIR must be an absolute path, not "<값>"`으로 거부한다(255).
Unix에서는 폴더 권한 0700, 소켓 `agent.sock` 권한 0600이다.
Unix에서는 데이터 폴더까지 가는 경로도 다른 계정이 바꿀 수 없어야 한다. 데이터 폴더의 모든 상위 폴더는 이 사용자나 root 소유여야 하고,
group이나 other에 쓰기 권한이 있으면 sticky 비트가 없는 한 거부한다(자세한 규칙과 이유는 M3의 "파일"). Windows도 상위 폴더를 검사한다(M5a).
이 상위 경로 검사는 사용자가 `HERDVIA_DIR`(Linux는 `XDG_DATA_HOME` 포함)로 직접 고른 위치에만 건다. 내장 기본 데이터 폴더에는 걸지 않는다(이유는 M3의 "파일").

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

주소 조회에는 EndpointId와 홈 릴레이 URL만 게시한다. 다만 iroh는 핸드셰이크 직후 연결을 등록하면서 이 PC의 직접 연결용 주소 후보
(LAN 주소, 포트 매핑된 공인 주소)를 상대에게 보낼 수 있고, herdvia의 거부 훅은 그 뒤에 돈다. 그래서 EndpointId를 아는 미인증 접속자도
이 PC의 IP 주소를 알 수 있다(릴레이 없는 구성의 실험에서만 확인했다). 훅보다 먼저 EndpointId로 거를 수단이 없으므로,
README에서 EndpointId를 공개 게시하지 말라고 안내한다.

### 명령

| 명령 | 역할 |
|---|---|
| `herdvia id` | 내 EndpointId 전체와 확인용 지문을 표준 출력에 쓴다. 키가 없으면 만든다 |
| `herdvia allow <이름> <EndpointId> (--inbound \| --outbound \| --both)` | 피어를 등록하거나 권한을 바꾼다 |
| `herdvia revoke <이름>` | 피어를 지우고, 그 피어와의 연결을 즉시 끊는다 |
| `herdvia peers` | 저장된 피어의 이름, 권한, 지문을 쓴다. 에이전트가 없어도 된다 |
| `herdvia status` | 피어별 이름, 권한, 연결 여부, 직접/중계, 왕복 시간을 쓰고, 연결되지 않은 피어마다 이유를 한 줄씩 덧붙인다 |
| `herdvia agent reload` | 떠 있는 에이전트에 `peers`를 다시 읽게 하고, 재시도를 기다리는 연결을 바로 다시 걸게 한다 |

- 지문은 EndpointId 문자열의 앞 16자를 4자씩 `-`로 묶은 것이다. `herdvia id`의 첫 줄은 EndpointId, 둘째 줄은 지문이다.
- 이름은 `[A-Za-z0-9._-]{1,32}`다.
- inbound: 그 피어가 이 PC에서 명령을 실행해도 된다. outbound: 이 PC가 그 피어로 걸어도 된다.
- `allow`, `revoke`는 에이전트가 떠 있으면 즉시 반영되고, 떠 있지 않아도 파일에는 반영된다.
  소켓이 없으면 바로 에이전트가 없다고 본다. 소켓이 연결을 거부하면 Unix에서는 데이터 폴더의 잠금(에이전트가 사는 동안 쥐는 `flock`)을 비차단으로 잡아 본다(`accept`의 reload도 같다).
  잡히면 곧바로 풀고 에이전트가 없다고 보아 기다리지 않고 성공한다. 잡히지 않으면 에이전트가 살아 있으므로 1초 동안 다시 시도하고, 그래도 거부되면
  reload가 확인되지 않은 것으로 보고 실패한다(원인 문구 `the running agent refused the reload request for a second`. 아래 "확인되지 않은 reload").
  macOS는 살아 있는 에이전트의 listen 대기열이 가득 찬 순간에도 죽은 에이전트가 남긴 소켓과 같은 `ECONNREFUSED`로 거부하므로(실측: 대기열 128개), 거부만 보고 포기하면 reload가 조용히 빠져 revoke한 피어가 계속 허용될 수 있기 때문이다.
  잠금을 잠깐 잡는 그 순간에 시작하던 에이전트는 드물게 `agent is already running`으로 실패할 수 있다. Windows는 이 잠금을 보지 않고 예전처럼 1초 동안 거부가 이어지면 에이전트가 없다고 본다.
- 단발 CONTROL 요청(`reload`, `status`, `status-detail`, `pending`)은 요청 하나마다 연결, 요청 쓰기, 응답 끝까지를 묶어 5초 안에 끝나야 한다.
  넘으면 TimedOut `the agent did not answer the <요청> request within 5 seconds`다. 멈춘 에이전트 때문에 명령이 끝나지 않으면 안 되기 때문이다.
  이 제한은 위의 1초 재시도 안에서 요청마다 걸린다. doctor는 자기 3초 제한을 쓴다(아래 "점검: doctor"). `listen`, `connect`, `via`의 연결에는 제한이 없다.
- 확인되지 않은 reload: `allow`, `revoke`, `accept`가 파일을 바꾼 뒤 reload가 "에이전트 없음"이 아닌 다른 이유(5초 시간 초과, 1초 동안 이어진 거부,
  프로토콜 오류, `peers`를 읽지 못한 에이전트의 ERROR 등)로 실패하면 성공 줄을 쓰지 않고 한 줄을 stderr에 쓰고 255로 끝낸다.
  - `allow`, `accept`: `saved <이름> in the peers file, but the running agent did not confirm it (<원인>); run herdvia agent reload, and if that fails, restart the agent (herdvia agent install, or restart herdvia agent)`
  - `revoke`: `revoked <이름> in the peers file, but the running agent did not confirm it (<원인>), so it may still admit <이름> until it reloads; run herdvia agent reload, and if that fails, restart the agent (herdvia agent install, or restart herdvia agent)`

  원인은 에이전트가 보낸 ERROR 글, 또는 위의 시간 초과·거부 문구다. 파일은 이미 바뀌었고, 같은 `revoke`를 다시 실행하면 에이전트에 닿기 전에 `no peer named <이름>`으로 실패한다.
  그래서 파일을 그대로 다시 적용하는 `herdvia agent reload`를 안내한다. 예전에는 reload가 멈추면 명령도 멈췄고, 실패해도 에이전트가 revoke한 피어를 계속 허용하는지 알 수 없었다.
  저장된 파일을 설명하는 경고(`allow`의 키 변경 경고, `revoke`의 중복 키 경고)는 이 줄보다 먼저 그대로 쓰고, `revoke`의 정리 안내(아래)는 쓰지 않는다.
- `herdvia agent reload`는 CONTROL `reload`를 보낸다. 에이전트가 확인하면 표준 출력에 `reloaded: peers re-read; waiting connections retry now`를 쓰고 0으로 끝난다.
  reload는 `peers`를 다시 읽고, 재시도를 기다리던 거는 쪽 연결을 모두 지연 없이 다시 걸게 하므로 "지금 다시 시도"에도 쓴다. 요청은 예전부터 있던 것이라 이전 에이전트에도 통한다.
  에이전트가 없으면 `allow`와 달리 `agent is not running; start it with 'herdvia agent install' (or 'herdvia agent' in a terminal)`로 실패한다(255). 에이전트에 닿으라고 직접 요청한 명령이기 때문이다.
  1초 재시도와 5초 제한은 위와 같고, 실패하면 그 원인을 그대로 쓴다(255). 데이터 폴더와 키를 만들지 않는다.
- 성공하면 `allow`는 `<이름> <EndpointId> <in|out|both>`(accept와 같은 형식), `revoke`는 `revoked <이름> <EndpointId>` 한 줄을 표준 출력에 쓴다.
  에이전트 reload까지 성공한 뒤에 쓴다. 무엇이 바뀌었는지 눈으로 확인하게 하려는 것이다.
- `allow`가 이미 있는 이름의 EndpointId를 바꾸면 표준 출력 줄 뒤에 stderr로 `herdvia: warning: <이름> now points to a different key (fingerprint <옛 지문> -> <새 지문>)`를 쓴다.
  같은 키의 권한만 바꾸면 더 쓰지 않는다. 동작과 종료 코드는 그대로다. 잘못 붙여 넣은 ID가 기존 피어를 조용히 바꾸지 않게, 키가 바뀐 것을 보이게 하려는 것이다.
- `revoke`가 성공하면 표준 출력 줄과 아래의 중복 키 경고 다음에 stderr로 안내를 쓴다. outbound 권한(`out`, `both`)이 있던 피어면
  `herdvia: note: Herdr may still list <이름>; remove it with: hc machine rm <이름>`, 그리고 언제나 `herdvia: note: on that PC, also revoke this PC (herdvia revoke <the name it uses for this PC>)`다.
  revoke는 이 PC의 `peers`만 바꾸고 Herdr의 머신 등록과 상대 PC의 항목은 남기기 때문이다. `hc machine rm`을 대신 실행하지는 않는다.
  그래서 성공한 `revoke`도 stderr에 쓴다(종료 코드와 표준 출력은 그대로다).
- 없는 이름을 `revoke`하면 `no peer named <이름>`으로 실패하고 파일을 건드리지 않는다. 오타로 아무것도 지우지 않았는데 성공으로 보이면 안 되기 때문이다.
- EndpointId 하나에는 이름 하나만 둔다. `allow`와 `accept`는 다른 이름에 이미 있는 EndpointId를
  `EndpointId <id> is already registered as peer <이름>; revoke it first or use that name`으로 거부한다.
  권한은 EndpointId로 판정하므로, 같은 키가 두 이름으로 있으면 한 이름을 revoke해도 다른 이름의 권한으로 계속 연결된다.
  키가 새었을 때 revoke 한 번으로 끊기지 않는 것을 막으려고 별칭을 허용에서 거부로 바꿨다.
  이전 버전이 쓴 중복 파일도 그대로 읽는다. 그중 한 이름을 revoke하면 성공하되 표준 오류에
  `warning: <EndpointId> is still allowed as peer <남은 이름들>; revoke each of those names to remove it`을 쓴다.
  그런 파일에서 중복된 이름 하나의 권한만 바꾸는 `allow`·`accept`도 거부되므로, 먼저 revoke로 정리해야 한다.
- `peers`는 `peers` 파일만 읽는다. 머리 줄 `name permission fingerprint`, 피어마다 `<이름> <in|out|both> <지문>`(전체 EndpointId는 쓰지 않는다),
  마지막 줄 `in = may open sessions on this PC; out = this PC may connect to it`를 표준 출력에 쓴다. 피어가 없거나 파일이나 데이터 폴더가 없으면 `no peers registered`다.
  에이전트 없이 동작하고 실시간 연결은 말하지 않는다(그것은 `status`). 데이터 폴더와 키를 만들지 않고 권한도 고치지 않는다.
  파일은 에이전트와 같은 검사로 읽으므로, 에이전트가 거부하는 파일은 같은 문구로 거부한다(255).
- `status`는 에이전트가 떠 있어야 한다. 없으면 한 줄을 stderr에 쓰고 255로 끝낸다. 표 아래의 이유 줄은 아래 "끊긴 이유: status-detail"에 적는다.

### 파일 (데이터 폴더 안)

- `secret.key`: 비밀키. 권한 0600. 권한이 더 넓으면 에이전트와 `id`는 시작을 거부한다.
- `peers`: 피어 목록. 권한 0600. 한 줄에 `이름 EndpointId 권한`(권한은 `in`, `out`, `both`).
- Unix의 데이터 폴더 준비(`prepare_dir`): 없으면 0700으로 만든 뒤 `O_NOFOLLOW | O_DIRECTORY`로 한 번 열고, 그 디스크립터 하나로 소유자 검사(fstat), `chmod 700`(fchmod), macOS ACL 검사를 한다.
  경로로 따로 보고 바꾸면 그 사이에 폴더가 링크로 바뀔 수 있기 때문이다. 디렉터리를 가리키는 심볼릭 링크는 열 때 `data path must be a real directory`로 거부한다.
  이 디스크립터가 지키는 범위는 말단 데이터 폴더 하나의 검사와 chmod 사이다. 그 뒤 `secret.key`, `peers`, 임시 파일, `agent.sock`은 경로로 다시 열므로,
  그 위의 경로가 바뀌는 것은 아래의 상위 경로 검사가 막는다.
  그 경로에 일반 파일이나 디렉터리를 가리키지 않는 링크(깨진 링크 포함)가 이미 있으면 그 전에 폴더를 만드는 단계(`DirBuilder`)가 OS 오류(`File exists`)로 끝난다.
  이 사용자 소유인데 이 사용자가 읽을 수 없는 폴더(모드 0000 등, 열기가 EACCES)는 열 수 없으므로 예전처럼 경로로 확인하고 0700으로 고친 뒤 다시 연다.
  이 chmod는 링크를 따라가지 않는다(`fchmodat`에 `AT_SYMLINK_NOFOLLOW`). 확인 뒤 그 경로가 링크로 바뀌어도 링크 대상의 모드를 바꾸지 않기 위해서다.
  이 플래그를 지원하지 않는 Linux(glibc 2.32 미만, 또는 `/proc`이 없는 환경)에서 ENOTSUP나 EOPNOTSUPP가 나오면, 그 경로가 아직 실제 디렉터리인지 다시 확인한 뒤 경로로 chmod한다.
  링크이거나 디렉터리가 아니면 그 오류를 그대로 돌려준다. 다시 확인한 뒤 chmod하기까지의 짧은 틈은 이런 환경에서만 남는다.
  다시 열 때 링크나 디렉터리가 아닌 것으로 바뀌어 있어도 OS 오류 문구가 아니라 같은 `data path must be a real directory`로 거부한다.
- macOS에서는 데이터 폴더(chmod 700 뒤), `secret.key`, `peers`에 allow 항목이 있는 확장 ACL이 있으면
  `<경로> has an access control list (ACL) entry that allows access; remove the ACL with chmod -N '<경로>'`로 거부한다.
  ACL은 권한 비트와 따로 접근을 허용하고, 상속 ACL이 걸린 폴더에서는 새로 만든 `secret.key`도 그 ACL을 받기 때문이다.
  deny 항목만 있는 ACL은 받는다. `acl_get_fd_np`가 ENOENT, ENOTSUP, EOPNOTSUPP로 실패하면 ACL이 없는 것으로 본다.
  ACL을 지원하지 않는 파일 시스템에는 allow 항목도 있을 수 없으므로, 그런 볼륨을 오류(`Operation not supported`)로 막을 이유가 없다. 다른 errno는 오류다.
  Linux는 ACL을 검사하지 않는다.
- Unix의 상위 경로 검사(`ipc::check_parents`): 데이터 폴더의 상위 폴더를 다른 계정이 바꿀 수 있으면, 위 검사가 끝난 뒤 경로를 링크나 다른 폴더로 바꿔
  `secret.key`와 `peers`의 읽기·쓰기, `agent.sock`의 bind·connect를 다른 곳으로 돌릴 수 있다. 기본 경로에서는 성립하지 않지만 `HERDVIA_DIR`은 어디든 가리킬 수 있다.
  그래서 이 검사는 `HERDVIA_DIR`(Linux는 `XDG_DATA_HOME` 포함)로 고른 위치에만 건다(아래의 "기본 데이터 폴더").
  파일 저장 코드(경로 기반 API, tempfile, 소켓 bind/connect)를 디렉터리 핸들 기준으로 모두 바꾸는 대신, 다른 계정이 바꿀 수 없는 경로만 받는다.
  소켓은 핸들 기준으로 bind·connect할 공통 수단이 없어 어차피 경로가 안전해야 하기 때문이다. 규칙은 다음과 같다.
  - 루트부터 요소 단위로 내려가며 확인한다. 데이터 폴더의 모든 상위 디렉터리는 이 사용자(euid)나 root 소유여야 한다(`is owned by another user (uid <번호>)`).
    다른 소유자는 지금 권한이 0755여도 언제든 chmod하고 항목을 바꿀 수 있다.
  - group이나 other에 쓰기 권한이 있으면 거부한다(`is writable by other users (mode <모드>, no sticky bit)`, `chmod go-w` 안내). 그룹 구성원이 이 사용자뿐이어도 보수적으로 거부한다.
  - sticky 비트가 있는 디렉터리(`/tmp`)는 받는다. 그 안의 항목은 항목 소유자, 디렉터리 소유자, root만 지우거나 이름을 바꿀 수 있기 때문이다.
    대신 그 안에서 지나가는 항목(다음 경로 요소, 데이터 폴더 자신일 수도 있다)도 이 사용자나 root 소유여야 한다. `/tmp` 아래 남의 소유 폴더를 지나가면 그 소유자가 아래를 바꿀 수 있다.
  - 중간 요소가 심볼릭 링크면, 링크가 놓인 디렉터리를 위 규칙으로 확인한 뒤 링크 대상 경로도 같은 규칙으로 따라가며 확인한다.
    macOS의 `/tmp` → `/private/tmp`, `/var` → `/private/var`처럼 정상 경로에도 링크가 있어 일괄 거부할 수 없다. `..`는 커널처럼 링크를 푼 실제 폴더에서 올라간다.
    순환을 막기 위해 링크는 32개까지 따라가고, 넘으면 오류다. 데이터 폴더 자신이 링크여도 대상을 같은 규칙으로 따라가며 확인한다.
    경로가 `/`나 `/.`으로 끝나면 커널이 그 링크를 따라가고, `private_file`과 `connect`가 여는 `폴더/peers`, `폴더/agent.sock`에서는 그 링크가 중간 요소이기 때문이다.
    끝 슬래시 없이 준 링크는 위의 `data path must be a real directory` 규칙대로 `prepare_dir`가 거부한다. 끝 슬래시가 있으면 `O_NOFOLLOW`로 열어도 커널이 링크를 따라가므로
    그 규칙이 막지 못하지만, 대상의 상위 경로는 이 검사가 확인했다.
  - 없는 요소를 만나면 그 아래는 아직 없으므로 존재하는 부분까지만 확인한다. 상대 경로는 현재 폴더를 앞에 붙여 확인한다.
    다만 없는 요소 뒤에 `..`가 남아 있으면(링크 대상에서 온 것 포함) 그 `..`가 확인하지 못한 기존 폴더로 돌아갈 수 있고 `prepare_dir`가 그 경로로 폴더를 만들게 되므로,
    InvalidInput(`the path to the data directory <데이터 폴더> has ".." after <없는 요소>, which does not exist, so the directories that ".." leads back to cannot be checked; write the path without ".."`)으로 거부하고 아무것도 만들지 않는다. 문자열 정규화로 `..`를 지우지는 않는다(링크 위의 `..` 의미가 바뀐다).
  - macOS는 상위 디렉터리의 확장 ACL에 경로를 바꿀 수 있게 하는 allow 항목(`add_file`, `add_subdirectory`, `delete`, `delete_child`, `writesecurity`, `chown`)이 있으면 거부한다
    (`has an access control list (ACL) entry that allows adding, deleting, or changing entries or permissions there`, `ls -led` 안내). ACL은 권한 비트와 따로 이런 변경을 허용하기 때문이다.
    읽기·탐색 allow와 deny 항목은 받는다. 홈 폴더와 `~/Library`의 기본 ACL(`group:everyone deny delete`)은 그대로 통과한다. allow 항목은 누구에게 준 것이든 거부한다. 대상 계정을 따지는 계산보다 단순하고 보수적이다.
    상위 폴더에 데이터 폴더와 같은 규칙(allow 항목이 하나라도 있으면 거부)을 쓰면 읽기 전용 ACL까지 거부하므로 규칙을 나눈다. 데이터 폴더, `secret.key`, `peers`의 규칙은 바꾸지 않는다.
    ACL은 `acl_get_link_np`로 경로로 읽는다. 그 위 폴더를 먼저 확인했으므로 다른 계정이 그 이름을 바꿀 수 없고, 탐색 권한만 있는 폴더(0711)에서도 읽힌다. ACL이 없다는 errno 처리는 위와 같다.
    Linux는 mode만 본다. POSIX ACL의 이름 붙은 사용자·그룹 항목은 mask를 넘지 못하고 mask는 group 비트로 보이므로, group 쓰기 거부가 그 쓰기도 막는다.
  - 거부는 PermissionDenied이고, 문구는 `<경로 요소> <이유>, who could change the path to the data directory <데이터 폴더>; <할 일>`이며
    할 일 끝에는 `use a data directory whose parent directories only this user or root can change`가 붙는다. 경로 요소는 링크를 푼 실제 경로로 쓴다(macOS의 `/var/…`는 `/private/var/…`로 나온다).
  - 이 검사는 아무것도 만들거나 고치지 않는다. 거는 곳은 `prepare_dir`(폴더를 만들기 전과 만든 뒤, chmod 전), `check_dir`(doctor), `private_file`(`secret.key`·`peers` 열기, 에이전트의 reload 포함),
    클라이언트 `connect`(아무것도 만들지 않는다), 에이전트 `listen`(`prepare_dir`를 거친다)이다. `prepare_dir`가 만든 뒤 다시 보는 것은, sticky 폴더에서 다른 계정이 없는 중간 폴더를 먼저 만들었을 수 있기 때문이다.
    결과를 캐시하지 않는다. 오래 도는 에이전트의 나중 읽기도 그때 상태로 확인한다.
  - 상위 경로 거부는 "에이전트 없음"이 아니다. 소켓이 없으면 NotFound, 죽은 소켓이면 ConnectionRefused인 기존 의미는 그대로이고(없는 폴더는 검사를 통과한 뒤 connect가 NotFound다),
    거부는 PermissionDenied라 이 둘을 에이전트 없음으로 삼키는 `reload_if_running`과 doctor가 숨기지 않는다.
  - 신뢰 주체(이 사용자와 root)가 경로를 바꾸는 것은 막지 않는다. 막는 것은 다른 일반 계정의 바꿔치기다. 검사 뒤 이 사용자나 root가 상위 폴더 권한을 넓히면 다음 진입점에서 거부된다.
    OS 임시 폴더(macOS `/var/folders/…/T`, `/tmp`)는 정상 권한에서 통과한다.
  - 기본 데이터 폴더: 내장 기본 폴더(macOS `$HOME/Library/Application Support/herdvia`, 그 밖의 Unix `$HOME/.local/share/herdvia`)에는 이 검사를 걸지 않는다.
    `check_parents`는 맨 앞에서, 넘어온 폴더가 `data_dir`이 `HERDVIA_DIR` 없이 계산하는 그 경로(`home_dir()` 기준)와 같으면 검사 없이 통과시킨다.
    `XDG_DATA_HOME`에서 온 경로는 기본으로 치지 않고 검사한다. 경로 요소 단위로 비교하므로 끝의 `/` 같은 차이는 같은 경로로 보지만,
    `HERDVIA_DIR`이 같은 폴더를 가리켜도 링크를 거치거나 `..`를 넣는 등 표기가 다르면 예외로 치지 않고 검사한다.
    `private_file`(파일의 부모 폴더를 넘긴다), `check_dir`, `connect`, `prepare_dir`가 모두 같은 판정을 탄다. `HOME`이 없어 기본 폴더를 구하지 못하면 예외 없이 검사한다.
    이유: 점검에서 나온 문제의 조건은 다른 계정이 바꿀 수 있는 폴더 아래의 `HERDVIA_DIR`이고 기본 경로에서는 성립하지 않는다.
    기본 경로의 상위 폴더(홈과 그 위)를 다른 계정이 바꿀 수 있는 PC라면 그 계정은 이미 로그인 항목이나 셸 설정을 바꿔 이 사용자로 실행할 수 있으므로, 기본 경로를 거부해서 얻는 보호가 없다.
    반면 상위 폴더가 그룹 쓰기 가능한 Linux(사용자 전용 그룹과 umask 002), 관리 도구가 프로필에 ACL을 건 Mac에서는 기본 경로가 거부될 수 있고,
    자동 시작은 기본 폴더만 쓰므로 우회할 방법이 없다. 업그레이드한 에이전트가 시작을 거부하면 그 PC로 가는 통로가 끊긴다.
    데이터 폴더, `secret.key`, `peers` 자체의 검사(소유자, 0700과 0600, macOS ACL)는 기본 폴더에서도 그대로다.
  - 이 절의 규칙은 Unix의 것이다. Windows의 상위 폴더 검사는 M5a 설계에 있다.

### 에이전트

- root로 실행하면 거부한다. Windows의 관리자 실행 거부는 M5a에 적는다.
- 시작 순서: root·관리자 거부, 데이터 폴더 준비와 키·`peers` 읽기, iroh 바인드, 로컬 통신 통로 열기, 그 직후 `peers` 다시 읽기.
  `allow`·`revoke`는 통로가 없으면 에이전트가 없다고 보고 파일만 바꾼다. 그래서 처음 읽은 뒤 통로가 열리기 전에 한 변경은
  다시 읽지 않으면 반영되지 않는다(revoke한 피어가 계속 허용된다). 통로가 열린 뒤의 변경은 reload 요청으로 온다.
  다시 읽기가 실패하면(`peers` 손상, 권한 문제) 시작이 실패한다.
- QUIC 전송 설정은 iroh 기본값에서 세 가지만 바꾼다. 상대가 여는 단방향 스트림 0개, 동시 양방향 스트림 16개(기본 100), 데이터그램 수신 끔.
  herdvia는 양방향 스트림만 쓰므로, 연결된 피어(outbound만 허용한 피어 포함)가 아무도 받지 않는 스트림이나 데이터그램에 데이터를 쌓아 두지 못하게 한다.
  한 피어에 via 세션 16개가 열려 있으면 17번째는 자리를 기다리다 거는 쪽의 5초 제한에 걸려 끝날 수 있다.
- **받는 쪽**: 연결의 상대 EndpointId가 inbound 피어가 아니면 연결을 끊는다. 처음 본 피어를 자동으로 믿지 않는다.
  끊은 시도는 `pending`을 위해 메모리에 기록만 한다(아래 "피어 등록: pending / accept").
  허용된 연결의 양방향 스트림 하나마다 M2의 실행 쪽 함수를 그대로 돌린다(네트워크 OPEN의 payload는 줄).
  스트림을 받을 때마다 권한을 다시 확인한다.
  스트림이 열리고 10초 안에 첫 OPEN이 오지 않으면 그 스트림을 끝낸다(`no OPEN within 10 seconds`). 아무것도 보내지 않는 스트림이 자리를 계속 차지하지 않게 하려는 것이다.
  정상 호출 쪽은 스트림을 열자마자 OPEN을 보내므로 영향이 없다.
- **거는 쪽**: via의 OPEN에 적힌 이름이 outbound 피어가 아니면 ERROR로 거부한다.
  outbound 피어와는 연결을 미리 맺어 두고, 실패하거나 끊기면 다시 건다. 간격은 1초에서 두 배씩 늘려 최대 30초이고,
  연결이 10초 이상 유지됐다가 끊긴 경우에만 1초로 되돌린다. 받는 쪽이 핸드셰이크 직후 403으로 거부해도 connect는 성공으로 돌아오므로,
  connect가 성공할 때마다 되돌리면 거부당하는 동안 약 1초 간격으로 끝없이 다시 걸게 된다.
  그렇게 곧바로 거부당하는 연결도 잠깐은 연결 목록에 들어가므로, 거는 쪽 `status`는 그동안 `connected`를 잠깐 보일 수 있다.
  연결 확인은 받는 쪽 `status`로 하는 편이 확실하다.
  via 요청 때 연결이 없으면 최대 5초 기다리고, 안 되면 ERROR를 돌려준다. 그 피어의 끊긴 이유 기록이 있으면 문구에 덧붙인다(아래 "끊긴 이유: status-detail").
  연결, open_bi, OPEN 쓰기, 5초 제한 중 하나로 실패해 보내는 ERROR에는 iroh 오류 문구(상대가 정한 종료 사유가 들어갈 수 있다)가 들어가므로,
  로그 줄과 같은 정화(제어 문자 이스케이프, 300자 제한)를 거친다. via는 이 문구를 표준 오류에 쓴다.
  연결되면 스트림을 열어 OPEN(줄)을 쓰고, 이후 프레임은 양방향으로 그대로 옮긴다.
- via가 끊기면 스트림을 리셋한다. 스트림이나 연결이 끊기면 실행 쪽은 자식을 정리하고, 거는 쪽 via는 255로 끝난다.
- 피어 목록이 바뀌면 지워졌거나 권한을 잃은 피어의 연결과 실행 중인 자식을 즉시 정리한다.
  정리하는 것은 통로와 통로가 띄운 자식(Unix는 프로세스 그룹, Windows는 직접 자식)뿐이다. 그 피어가 이미 연 원격 터미널(herdr-core 안에 남는다),
  그 안에서 떼어 낸 프로세스, 이미 바꾼 것은 되돌리지 않는다. README의 보안 모델에 그렇게 적고, 침해가 의심될 때 볼 것(Herdr 세션, 로그의 세션 기록)을 안내한다.
- iroh의 로그는 기본으로 끈다. 에이전트의 로그에는 접속·거부·종료 같은 사건만 남기고 스트림 내용은 남기지 않는다.
- 에이전트가 실행 중에 쓰는 로그 줄은 UTC 시각으로 시작한다(`2026-11-14T14:17:03Z herdvia: connected <id>`).
  시작하다 실패할 때의 마지막 오류 줄은 CLI와 같은 출력이라 시각이 없다.
- 실행 중의 로그 줄은 모두 한 함수(`log::line`)를 거친다. 형식은 그대로 `<UTC 시각> herdvia: <내용>`이고, 내용의 제어 문자(`char::is_control`),
  Unicode 줄·문단 구분자(U+2028, U+2029), 양방향 제어 문자 12자(Bidi_Control: U+061C, U+200E, U+200F, U+202A~U+202E, U+2066~U+2069)는
  Rust `escape_default` 표기(`\n`, `\r`, `\t`, `\u{1b}`, `\u{0}`, `\u{7f}`, `\u{85}`, `\u{2028}`, `\u{202e}`, `\u{61c}` 등)로 바꾼다.
  그래서 호출 한 번은 Unicode 줄 구분자로 줄을 나누는 리더에서도 반드시 한 줄이고, 로그 뷰어의 표시 순서를 바꾸지 못한다.
  이스케이프 뒤 300자(char)를 넘으면 300자에서 자르고 `... [truncated, <N> bytes]`(N은 정화 전 내용의 바이트 수)를 붙인다.
  QUIC 종료 사유나 거부한 OPEN처럼 원격이 정하는 글이 가짜 줄을 만들거나, 줄의 보이는 순서를 뒤집거나, 한 줄로 로그를 채우지 못하게 하려는 것이다.
- 로그 줄은 앞의 요약 줄(아래)과 함께 한 번의 write로 표준 오류에 쓴다. 쓰기가 실패하면(디스크가 가득 참, 읽는 쪽이 닫힌 파이프) 그 줄은 버리고 계속한다.
  예전에는 `eprint!`가 panic해서 세션이 명령을 실행하기 전에 끊기거나, `session ended`를 쓰는 Drop 안에서 panic이 났다.
  로그를 쓰지 못한다고 세션과 에이전트를 멈출 이유는 없어서 버리는 쪽을 택했다. 버린 줄은 흔적 없이 사라진다(세지도, 나중에 알리지도 않는다).
  시작하다 실패할 때의 마지막 오류 줄도 쓰지 못하면 버린다(종료 코드 255는 그대로다).
- 속도 제한은 단조 시계 기준의 토큰 버킷 둘이다. 미인증 버킷(`rejected peer <id>`, `incoming connection failed: <오류>`)은 한꺼번에 30줄, 그 뒤 10초에 1줄,
  나머지 줄의 버킷은 한꺼번에 60줄, 그 뒤 1초에 1줄이다. 버킷이 차 있는 동안에는 토큰이 더 쌓이지 않는다.
  EndpointId만 알면 누구나 미인증 줄을 만들 수 있으므로, 버킷을 나눠 그쪽 홍수가 인가된 피어의 줄을 밀어내지 못하게 한다.
  버킷이 비면 그 줄을 버리고 종류별로 센다. 그 종류의 다음 줄이 허용될 때 같은 시각으로 바로 앞에
  `suppressed <N> log lines about unauthenticated connections` 또는 `suppressed <N> other log lines`(N이 1이면 `line`)를 한 번 쓰고 수를 0으로 돌린다.
  요약 줄은 토큰을 쓰지 않는다. 그 종류의 줄이 더 오지 않으면 요약도 나오지 않는다.
- 받는 쪽 훅이 거부한 연결은 `rejected peer <id>` 한 줄만 남긴다. 그 밖의 들어오는 핸드셰이크 실패는 `incoming connection failed: <오류>`다.
- 세션 기록: 인가된 피어의 스트림이 줄 검사를 통과해 명령을 실행하기 직전에 `session started: <이름> (fingerprint <지문>), herdr session <세션>`을 남긴다
  (세션 이름이 없으면 `, herdr session ...`이 빠진다. 이름은 같은 EndpointId를 가진 첫 inbound 항목의 이름이다).
  끝나면 `session ended: <같은 설명>: <사유>`를 남긴다. 사유는 정상 종료면 `exit code <n>`(Unix 시그널 종료는 128+n), 원격이 스트림을 멈추면
  `the peer stopped the stream`, 연결이 닫히면 그 연결 오류 문구(이 PC의 revoke·권한 변경이면 `closed`), 그 밖에는 실행 중 오류 문구다.
  끝 줄은 스트림 태스크가 가진 guard의 Drop에서 쓰므로, 연결이 닫혀 태스크가 중단돼도 남는다. 에이전트가 멈추는 중이면(TERM, Ctrl+C 등) `the agent stopped`,
  사유도 연결 종료 사유도 없으면 `connection closed`다. 종료할 때는 태스크를 중단한 뒤 끝 줄이 쓰이도록 최대 1초 기다리고 엔드포인트를 닫는다.
  침해가 의심될 때 누가 언제 세션을 열었는지 볼 수 있게 하려는 것이고, 스트림 내용은 남기지 않는다.
  명령 실행 전에 오류로 끝난 스트림은 예전처럼 `stream from <id> ended: <오류>`다. 실행 전에 연결이 닫히거나 스트림이 멈춘 경우와
  Windows에서 아무것도 실행하지 않는 응답(Unix 줄 탐색, herdr-core 미설치)은 줄을 남기지 않는다.
- 자동 시작의 로그 파일(M4, M5b)은 크기와 기간으로 관리한다. 시작 직후(로컬 통신 통로를 연 뒤)와 그 뒤 한 시간마다 크기를 본다.
  10 MiB(10*1024*1024 바이트)를 넘으면 끝의 5 MiB만 읽어 그 안의 온전한 줄만 남기고 같은 파일을 제자리에서 다시 쓴다(잘린 첫 줄은 버리고,
  그 안에 개행이 없으면 빈 파일이 된다). 기간만으로는 줄이 쏟아질 때 며칠 안에 디스크가 차는 것을 막지 못하기 때문이다.
  크기 처리 뒤, 마지막으로 성공한 기간 정리에서 24시간이 지났을 때만(시작 직후 한 번 포함) 180일이 지난 줄을 지운다.
  macOS의 타이머는 잠자기 동안 멈추므로 24시간을 기다리지 않고 한 시간마다 깨어나 벽시계로 24시간이 지났는지 본다.
  앞에서부터 읽어, 180일 안의 시각이 붙은 첫 줄보다 앞에 있는 마지막 만료 줄까지 지운다. 시각이 없는 줄(이전 버전이 쓴 줄, 시작 실패의 오류 줄 등)은 그다음 시각이 붙은 줄을 따른다.
  지울 것이 없으면 쓰지 않는다. 있으면 남길 내용을 파일 앞으로 옮겨 쓰고 길이를 줄인다. 파일을 새로 만들지 않으므로 이어 쓰는 핸들이 그대로 유효하다.
  고쳐 쓰는 동안 자기 표준 오류를 잠가 그사이의 로그 줄을 잃지 않는다. 실패하면 시각 붙은 한 줄(`cannot remove old lines from <경로>: <오류>`)을 남기고
  다음 시간별 점검(한 시간 뒤)에 다시 한다. 24시간 주기는 정리가 성공했을 때만 새로 시작한다. 실패를 실행한 것으로 치면 24시간 동안 다시 하지 않기 때문이다.
  정리는 에이전트 루프 안에서 끝까지 기다리므로, 정상 종료와 리스너 해제는 진행 중인 정리가 끝난 뒤에 일어난다.
  다만 강제 종료(Windows의 `agent install` 재실행·`uninstall`이 쓰는 `TerminateProcess`, SIGKILL)가 고쳐 쓰는 도중에 오면 파일 끝에 옛 내용 일부가 겹쳐 남을 수 있고, 같은 로그 파일에 다른 프로세스가 동시에 이어 쓰면 그 순간의 줄이 잘릴 수 있다(프로세스 사이에는 잠그지 않는다).
  macOS는 표준 오류가 `~/Library/Logs/herdvia.log`에 이어 쓰고 있을 때만(O_APPEND, 연 파일과 dev/ino 비교), Windows는 `agent --log`로 표준 오류를 돌린 그 파일일 때만 지운다.
  Windows에서는 로그 경로의 항목을 링크를 따라가지 않고(`FILE_FLAG_OPEN_REPARSE_POINT`) 열어, 그것이 재분석 지점이 아니고 표준 오류 핸들과 같은 파일(볼륨 일련번호와 파일 인덱스)일 때만 정리한다.
  이름이 바뀌었거나 교체된 파일, 그 자리의 링크에는 아무것도 하지 않는다(로그 줄도 남기지 않는다). 경로로 연 다른 파일을 고쳐 쓰지 않기 위해서이고, macOS의 dev/ino 비교와 같은 역할이다.
  터미널이나 다른 파일로 가는 수동 실행의 표준 오류와 Linux에서는 지우지 않는다. macOS에서는 수동 실행이라도 표준 오류가 그 로그 파일에 이어 쓰면 똑같이 지운다.

### 로컬 통신 추가

tag 7 CONTROL을 추가한다. 첫 프레임으로만 올 수 있고 payload는 `reload`, `status`, `status-detail`, `pending`, `doctor`, `listen`, `connect` 중 하나다(`status-detail`, `pending`, `doctor`, `listen`, `connect`는 나중에 추가).
프레임 계층은 CONTROL payload가 1~64바이트의 UTF-8인지만 보고, 어긋나면 프레임을 읽는 단계에서 프로토콜 오류로 보고 연결을 끊는다.
어떤 요청이 있는지는 에이전트가 판단한다. 모르는 요청은 끝까지 읽은 뒤 ERROR `unknown control request`로 답하고, 그 문구를 로그에도 한 줄 남긴다.
나중의 CLI가 새 요청을 보냈을 때 응답으로 "이 에이전트는 모른다"를 알 수 있게 하려는 것이다. 연결이 그냥 끊기면 고장과 구별하기 어렵다.
v0.1.4 이하의 에이전트는 프레임 계층에서 길이(6~7바이트)와 요청 목록을 검사했으므로, 모르는 요청에는 답 없이 연결을 끊는다.
클라이언트에는 `agent closed without EXIT`, 연결 재설정이나 중단, 끊긴 파이프, 예상하지 못한 EOF로 보인다. 새 요청을 쓰는 CLI는 이것도 "모르는 요청"으로 다룬다(`status`의 대체 동작은 아래).
단발 요청(`reload`, `status`, `status-detail`, `pending`, `doctor`)에 에이전트는 결과 글을 STDOUT 프레임으로, 끝을 EXIT 0으로 답한다. CLI는 요청마다 5초를 기다린다(위 "명령"). `listen`과 `connect`는 뒤이어 STDIN·STDOUT·ERROR·EXIT 프레임으로 대화한다(아래 "대화형 페어링: listen / connect"). 네트워크 스트림에서는 CONTROL을 받지 않는다.

### 끊긴 이유: status-detail

`status`의 `disconnected`만으로는 상대가 거부했는지, 닿지 않는지, 상대가 걸어 오기를 기다리는지 알 수 없다. 그래서 거는 쪽 에이전트가 이유를 기록하고, `status`, doctor, via 오류가 그것을 보여 준다.

- 기록은 메모리에만 둔다. 파일에 저장하지 않으므로 에이전트를 다시 시작하면 비어 있다. 피어(EndpointId)마다 마지막 실패와 다음 재시도 시각을 둔다.
- 이 PC가 건 방향만 기록한다. `endpoint.connect`가 실패했을 때와, 이 PC가 건(클라이언트 쪽) 연결이 끝났을 때 쓴다. 들어온 연결이 끝나는 것은 기록을 건드리지 않는다.
  그래야 `both` 피어의 거부 이유가 반대 방향 연결이 닫힌 것으로 덮이지 않는다. inbound만 허용한 피어에는 기록이 없다.
- 분류: 원격이 코드 403으로 닫은 애플리케이션 종료만 `rejected`이고, 이유 글은 `403 <원격의 사유>`(사유가 비면 `403`)다. connect의 오류로 드러나든 핸드셰이크 뒤의 종료 사유로 드러나든 같다.
  그런 종료가 없는 다이얼 실패는 `unreachable`, 이 PC가 건 연결이 다른 이유로 끝난 것은 `closed`이고, 이유 글은 iroh 오류 문구다(예: `timed out`, `closed by peer: session closed (code 0)`).
  원격 에이전트가 멈추거나 다시 시작할 때는 코드 0으로 닫으므로 `rejected`가 아니라 `closed`다. 403만 상대가 이 PC를 받지 않는다는 확실한 사실이고, 시간 초과를 "PC가 꺼졌다"로 단정하지 않는다.
- 이유 글에는 원격이 정한 사유가 들어가므로 저장하기 전에 로그 줄과 같은 정화(제어 문자 이스케이프, 300자 제한)를 거친다.
- 기록은 그 ID에 outbound 권한이 있는 동안에만 쓴다(기록 잠금 안에서 확인한다). 이 PC가 건 연결이 등록되면 지우고, reload 때 outbound 권한이 없어진 ID의 기록은 버린다.
  잠금은 `connections` 다음에 기록 순서로만 잡는다.
- 거는 쪽 워커는 잠들기 전에 다음 시도 시각을 기록하고 다시 걸 때 지운다. 남은 시간은 올림한 초로 쓴다. 재시도가 남았는데 0초로 보이지 않게 하려는 것이다.
- 한계: 거부당하는 연결은 connect가 성공으로 돌아온 뒤 원격이 닫으므로, 그 짧은 사이에는 등록 때 지운 이유가 아직 다시 쓰이지 않아 이유가 보이지 않는다.
  꺼진 PC로 거는 시도는 QUIC 유휴 제한(약 30초)이 지나야 실패하므로, 그때까지는 새 이유가 없고 이전 이유나 이유 없음이 보인다.
- CONTROL `status-detail`의 답은 첫 줄 `herdvia-status-detail 1`과 피어마다 한 줄이다.
  ```
  <이름> <in|out|both> <connected|disconnected|waiting> <경로> <rtt> <-|rejected|unreachable|closed> <-|재시도까지 남은 초> <이유 글>
  ```
  앞 일곱 칸에는 공백이 없고, 이유 글은 줄의 나머지 전부다. 이유 글이 비면 줄은 재시도 칸에서 끝난다. `waiting`은 연결되지 않은 inbound 전용 피어다(이 PC가 걸지 않으므로 실패도 없다).
  경로와 rtt는 `status`와 같다. 첫 줄이 다르거나 읽을 수 없는 줄이 있으면 CLI는 그 답을 쓰지 않는다. 모르는 이유 종류는 안내 없이 글만 보여 준다.
- `status`의 5칸 답은 그대로다(`waiting`은 `disconnected`로 쓴다). 이전 doctor는 5칸이 아닌 줄을 경고하므로, 칸을 늘리지 않고 요청을 따로 둔다.
- `herdvia status`는 먼저 `status-detail`을 요청해 그 답으로 `status`와 같은 표를 그린다. 연결되지 않은 피어가 있으면 빈 줄 하나와 그런 피어마다 한 줄을 표준 출력에 쓴다.
  - `<이름>: rejected by the peer (<이유 글>)`. 이유 글이 `403 peer not allowed`면 `; on <이름> run herdvia pending, then herdvia accept`를,
    `403 inbound permission required`면 `; on <이름> allow this PC with --inbound or --both (herdvia pending, then herdvia accept)`를,
    `403 peer permissions changed`면 `; <이름> changed this PC's permissions (it may have revoked this PC); check herdvia peers on <이름>`을 붙인다. 그 밖의 403에는 붙이지 않는다.
  - `<이름>: could not reach it (<이유 글>); its agent may be off or offline`
  - `<이름>: the connection closed (<이유 글>)`
  - `<이름>: waiting for <이름> to connect (this PC only accepts it)`
  - 기록이 없으면 `<이름>: not connected yet`, 모르는 종류면 `<이름>: not connected (<이유 글>)`. 보지 않은 원인을 짐작해 쓰지 않는다.

  줄마다 `.`으로 끝나고, 재시도를 기다리면 ` Retrying in <n>s.`를 붙인다. 이유 글이 없으면 괄호를 뺀다. 그런 줄 가운데 하나라도 재시도를 기다리면 마지막에 `Run herdvia agent reload to retry now.`를 쓴다.
  예: `win1: rejected by the peer (403 peer not allowed); on win1 run herdvia pending, then herdvia accept. Retrying in 12s.`
  연결된 피어에는 줄을 쓰지 않는다. `both` 피어가 들어온 연결로 연결돼 있으면 거는 쪽 기록이 남아 있어도 줄이 없다. macOS의 중계 안내(stderr)는 같은 표로 판단한다.
- 에이전트가 `status-detail`을 모르면(ERROR `unknown control request`, 또는 위 "로컬 통신 추가"의 답 없는 끊김. Windows는 파이프 오류 코드
  `ERROR_BROKEN_PIPE`, `ERROR_NO_DATA`, `ERROR_PIPE_NOT_CONNECTED`도 끊김으로 본다) `status`를 다시 요청해 표만 쓰고, stderr에
  `herdvia: note: restart the agent (herdvia agent install) to see why peers are disconnected`를 쓴다. 종료 코드는 0이다.
  이 배치의 에이전트가 답하는 도중 죽어도(연결 재설정, EOF) 같은 대체 동작과 안내가 나온다. 그때도 다시 시작하라는 안내가 맞다.
  첫 줄의 판이 다르면(더 새 에이전트) 다시 시작해도 소용없으므로 안내 없이 `status`의 표만 쓴다. 그 밖의 오류(시간 초과, 에이전트 없음, 다른 ERROR)는 지금처럼 명령의 오류다(255).
- via 오류: 5초를 기다려도 연결이 없을 때 그 피어에 기록이 있으면 `peer did not connect within 5 seconds: <짧은 이유>`다.
  짧은 이유는 위 줄에서 이름, 안내, 재시도를 뺀 것이다(예: `rejected by the peer (403 peer not allowed)`, `could not reach it (timed out)`). 기록이 없으면 예전처럼 `peer did not connect within 5 seconds`다.
  Herdr의 머신 목록에서 원인이 보이게 하려는 것이다. 전체를 300자 안에 맞추므로(넘으면 297자에서 자르고 `...`를 붙인다) via ERROR의 정화를 거쳐도 이중으로 이스케이프되지 않는다.

### M3에 넣지 않는 것

초대 코드, 자체 중계 서버 설정, 자동 시작, Windows, 키 교체, `hc machine add` 자동 실행.
자동 시작은 M4, Windows는 M5에서 넣었다. 피어 등록 간소화는 먼저 `pending`/`accept`를 넣었고, 나중에 LAN 자동 발견·일회용 초대 문자열·`hc machine add` 자동 실행을 묶은 `listen`/`connect`를 넣었다(아래).
키 교체 명령도 없다. 손으로 하는 순서(모든 피어에서 이 PC의 옛 항목을 revoke, 에이전트 중지, `secret.key` 삭제, `herdvia id`,
`agent install`, 다시 페어링)를 README에 적는다. 키를 백업에서 복원했거나 다른 PC로 옮긴 경우도 교체 대상이다.

## M4: macOS 자동 시작

| 명령 | 역할 |
|---|---|
| `herdvia agent install` | 로그인 시 에이전트가 자동으로 뜨도록 등록하고 바로 시작한다 |
| `herdvia agent uninstall` | 에이전트를 멈추고 등록을 지운다 |

- 사용자별 LaunchAgent다. 파일은 `~/Library/LaunchAgents/io.github.photojupi.herdvia.agent.plist`, 라벨은 `io.github.photojupi.herdvia.agent`.
  시스템 전체 데몬으로는 등록하지 않는다(herdr-core가 사용자 로그인 세션에서 돌아야 한다).
- 실행 경로는 `install`을 실행한 그 실행 파일의 절대 경로다. 임시 폴더나 빌드 폴더(`target/`) 안에서 실행하면
  경고를 stderr에 쓰되 등록은 한다.
- `RunAtLoad`와 `KeepAlive`를 켠다. 로그는 `~/Library/Logs/herdvia.log`에 표준 출력·오류를 모은다. 에이전트가 M3의 규칙(10 MiB, 180일)으로 정리한다.
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
- 운영체제별 코드는 `ipc`, `exec`, `template`의 줄 판정, 데이터 폴더(권한 검사 포함), root·관리자 거부에만 둔다.
  프레임, 네트워크, 피어 목록, via·에이전트의 흐름은 공유한다.
- 데이터 폴더: `%APPDATA%\herdvia`. `HERDVIA_DIR`이 있으면 그것을 쓴다(Unix와 같다).
- 로컬 통신: 이름 있는 파이프 `\\.\pipe\herdvia-<사용자 SID>`. `HERDVIA_DIR`이 있으면 그 경로의 해시를 이름 끝에 붙여 구분한다.
  - 파이프의 보안 설정은 현재 사용자 SID에만 전체 권한을 준다. 원격 클라이언트는 거부한다.
  - 에이전트는 첫 인스턴스로만 파이프를 만든다. 다만 파이프 이름은 `HERDVIA_DIR`의 표기를 따르므로, 같은 폴더를 다른 표기
    (대소문자, 끝의 역슬래시, 기본 경로를 `HERDVIA_DIR`로 명시)로 가리키면 이름이 달라져 두 에이전트가 같은 `secret.key`로 뜰 수 있었다.
    그래서 단일 실행은 데이터 폴더의 `agent.lock`으로 보장한다. 에이전트는 이 파일을 만들고(지우지 않는다) 사는 동안 `LockFileEx` 배타 잠금을 쥔다
    (즉시 실패 방식. FILE_SHARE_DELETE 없이 열어 쥔 동안 이름을 지울 수 없다). 잠겨 있으면 "agent is already running"이다.
    파이프 이름은 바꾸지 않았으므로 via와 다른 버전의 에이전트도 같은 이름을 쓴다.
  - 첫 파이프 인스턴스를 만들지 못하면(ERROR_ACCESS_DENIED) 그 이름에 익명 클라이언트(SECURITY_ANONYMOUS)로 접속해 소유자 SID를 읽는다.
    익명으로 접속하는 것은 남의 서버가 이 사용자를 가장하지 못하게 하려는 것이다. 접속할 때는 `READ_CONTROL`만 요청한다. 읽기를 막은 파이프도 소유자는 알려 주게 하려는 것이다.
    모든 인스턴스가 사용 중이면(ERROR_PIPE_BUSY, 에이전트가 접속을 받고 다음 인스턴스를 만들기 전) 최대 1초 동안 다시 시도한다.
    자기 것이면 "agent is already running"이다(agent.lock을 이미 쥐었으므로 잠금을 모르는 이전 버전 에이전트다).
    다른 계정이면 `another account (<SID>) holds this agent's pipe name; the agent cannot start until that account's process closes it`,
    소유자를 읽을 수 없으면 `another process holds this agent's pipe name, and its owner cannot be read (<OS 오류>); the agent cannot start until that process closes it`이다.
    예전에는 읽을 수 없는 소유자도 "agent is already running"으로 보아, 에이전트가 없는데 떠 있다고 안내했다.
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
- Windows에는 root 거부 대신 관리자 실행 거부가 있다. `herdvia agent`(`--log` 포함)는 토큰의 `TokenElevationType`이 `TokenElevationTypeFull`
  (UAC로 분리된 토큰의 상승된 쪽, 즉 "관리자 권한으로 실행"한 터미널)이면
  `herdvia agent must not run as administrator; run it from a normal terminal without administrator privileges`로 거부한다(`--log`면 이 줄이 로그 파일에 남는다).
  상승된 에이전트가 띄운 herdr-core는 높은 무결성 수준으로 도는데, 같은 사용자의 보통 무결성 프로세스가 파이프로 그것을 부릴 수 있기 때문이다.
  `TokenElevationTypeDefault`(UAC 꺼짐, 기본 제공 Administrator, 일반 사용자)와 `Limited`는 허용한다. UAC가 꺼졌거나 기본 제공 Administrator면
  토큰이 늘 상승돼 있고 대신 쓸 보통 토큰이 없어서, 막으면 에이전트를 아예 띄울 수 없다. 다른 명령은 막지 않는다(`agent install`은 M5b).
- 데이터 폴더는 `chmod 700`에 해당하는 검사와 설정을 한다(`prepare_dir`, 키를 읽는 모든 명령). 상위 폴더 검사(아래)를 먼저 하고, 폴더를 만든 뒤 재분석 지점(정션, 심볼릭 링크 등)이면
  `data path must be a real directory, not a junction, symbolic link, or other reparse point`로, 소유자가 현재 사용자, Administrators(S-1-5-32-544),
  LocalSystem(S-1-5-18)이 아니면 `data directory must be owned by this user, Administrators, or SYSTEM, not <SID>; use a directory that this user created`로 거부한다.
  소유자는 언제든 DACL을 되돌릴 수 있으므로 소유자부터 본다. Administrators를 받는 것은 상승된 사용자가 만든 객체의 소유자가 Administrators이기 때문이다.
  - 안전 규칙(폴더와 파일 공통): DACL이 NULL이 아니고, 허용 항목이 상속용 항목까지 모두 이 사용자, SYSTEM(S-1-5-18), Administrators(S-1-5-32-544) 중 하나를 가리켜야 한다.
    deny 항목은 문제 삼지 않는다. 검사하지 않는 허용 형식(object, callback, compound allow)은 열린 것으로 본다.
  - 폴더가 이미 herdvia가 거는 상태(보호되어 있고, 안전하고, 세 계정 각각에게 하위 파일과 폴더로 상속되는 모든 권한 `OICI FA`를 주고, 상속되는 deny 항목이 없는 상태)이면 DACL을 다시 쓰지 않는다.
    그 밖에는 보호 DACL `D:P(A;OICI;FA;;;<사용자 SID>)(A;OICI;FA;;;SY)(A;OICI;FA;;;BA)`를 건다. 상위 폴더에서 상속받지 않고,
    안에 만드는 파일(임시 파일로 쓰는 `secret.key`, `peers` 포함)이 상속한다. 매번 다시 쓰지 않는 것은 쓰려면 WRITE_DAC가 필요해서,
    이미 안전해도 이 사용자가 DACL을 바꿀 수 없는 폴더(SYSTEM 소유 등)가 맨 `Access is denied`로 실패하기 때문이다.
    보호되어 있고 신뢰 계정만 있어도 상속 항목(OI)이 없는 DACL은 안의 파일이 이 사용자 권한을 물려받는다고 볼 수 없으므로 다시 쓴다.
    상속되는 deny 항목(OI나 CI가 붙은 deny)이 있는 폴더도 그 상태로 보지 않고, 바꿀 수 있으면 다시 써서 deny를 없앤다. 그 deny가 안에 새로 만드는 키와 `peers`로 넘어가
    이 사용자가 자기 키를 못 읽게 될 수 있기 때문이다. 상속되지 않는 deny는 폴더 자신에만 걸리므로 그 상태로 인정한다.
  - `secret.key`와 `peers` 중 DACL이 열린 파일은, 이 사용자가 그 파일의 DACL을 바꿀 수 있으면(WRITE_DAC로 열리면) 그 핸들로 종류와 소유자를 확인하고
    같은 핸들로 파일 자체에 보호된 명시 DACL `D:P(A;;FA;;;<사용자 SID>)(A;;FA;;;SY)(A;;FA;;;BA)`를 건다. 폴더가 herdvia의 상태가 아니어도(안전하지만 이 사용자가 바꿀 수 없는 폴더 등) 고친다.
    폴더에서 상속받게 하지 않는 것은, 검사와 수리 사이에 상위 폴더가 바뀌거나 폴더가 넘겨주는 deny 항목 때문에 소유자가 자기 파일을 못 읽게 되는 일을 막기 위해서다.
    WRITE_DAC로 열리지 않거나 검사에 실패하면 그대로 두고, 이어서 `private_file`이 아래 문구로 거부한다. herdvia가 새로 만드는 파일(새 키, 임시 파일과 rename으로 다시 쓰는 `peers`)은 폴더에서 상속한다.
  - 기존 폴더와 파일을 고치는 이유: v0.1.2는 Windows에서 데이터 폴더와 파일의 DACL을 전혀 건드리지 않고 프로필의 기본(상속) 권한을 그대로 썼다.
    실측: v0.1.2가 설치된 Windows 11 PC 두 대에서 `%APPDATA%\herdvia`, `secret.key`, `peers` 모두에 상위 폴더에서 상속된 다른 주체의 읽기 항목(로컬 그룹 CodexSandboxUsers)이 있었다.
  - 폴더가 열려 있는데 이 사용자가 DACL을 바꿀 수 없으면
    `data directory allows access to accounts other than this user, SYSTEM, and Administrators, and this user cannot change its permissions; in an administrator terminal run icacls "<폴더>" /grant "*<사용자 SID>:(OI)(CI)F" and run herdvia again, or use a directory that this user created`로 거부한다.
    인수 전체를 따옴표로 감싸 PowerShell과 cmd 어느 쪽에나 그대로 붙여 넣을 수 있게 한다(따옴표가 없으면 PowerShell이 `(OI)`를 명령으로 해석한다).
  - 폴더가 그 상태가 아니고 이 사용자가 바꿀 수도 없지만 안전하면(예: SYSTEM이 소유하고 이 사용자에게 Modify만 준 폴더) 그대로 쓴다.
    보호되지 않은 폴더라면 부모에서 상속이 계속 들어오고, 에이전트가 도는 동안 부모에 더해진 항목은 다음 시작 때에야 검사된다.
  - 폴더는 한 핸들로 검사하고, WRITE_DAC가 필요할 때만 경로로 다시 연다(ReOpenFile로는 WRITE_DAC를 받지 못한다). 다시 연 항목의 파일 ID(볼륨 일련번호와 파일 인덱스)가
    처음 연 것과 다르면 그 핸들은 쓰지 않아, 그 사이의 바꿔치기를 막는다. 폴더와 doctor의 쓰기 가능 판정은 이때 `<경로> was replaced while herdvia checked it`으로 거부한다.
    `secret.key`와 `peers`의 수리는 처음부터 WRITE_DAC로 열어 검사한 그 핸들로 걸므로 다시 열지 않는다.
  프로필의 기본 권한에 기대지 않는 것은 `HERDVIA_DIR`이 프로필 밖을 가리켜도(상위 폴더가 아래의 검사를 통과하는 한) 같은 보호를 받게 하고, 위 실측처럼 프로필의 기본 권한에도 다른 주체가 들어 있을 수 있기 때문이다.
- `secret.key`와 `peers`는 재분석 지점이나 일반 파일이 아닌 것을 `<경로> must be a regular file, not a directory, link, or other reparse point`로,
  소유자가 위 셋이 아니면 `<경로> must be owned by this user, Administrators, or SYSTEM, not <SID>`로 거부한다.
  위의 수리 뒤에도 DACL이 열려 있으면 `<경로> allows access to accounts other than this user, SYSTEM, and Administrators, and herdvia cannot restrict it; in an administrator terminal run icacls "<경로>" /grant "*<사용자 SID>:F" and run herdvia again`으로 거부한다.
  이 사용자에게 그 파일의 모든 권한을 주면 다시 실행할 때 herdvia가 파일 자체의 DACL로 제한한다. 이 명령도 PowerShell과 cmd에 그대로 붙여 넣을 수 있다.
  에이전트, CLI, 실행 중의 `peers` 다시 읽기, doctor가 같은 문구를 쓴다. 파일이 없을 때의 동작(키 생성, 빈 목록)은 그대로다.
- 상위 폴더 검사: 위 검사는 데이터 폴더와 두 파일만 본다. 상위 폴더를 다른 계정이 바꿀 수 있으면 검사가 끝난 뒤 데이터 폴더 경로를 정션이나 다른 폴더로 바꿔
  `secret.key`, `peers`, `agent.lock`을 읽고 쓰는 위치를 돌릴 수 있었다. 그래서 `HERDVIA_DIR`로 고른 위치에서는 신뢰할 수 있는 상위 경로만 받는다.
  - 기본 데이터 폴더(`%APPDATA%\herdvia`)에는 이 검사를 걸지 않는다. `check_ancestors`는 맨 앞에서, 넘어온 폴더가 `data_dir`이 `HERDVIA_DIR` 없이 계산하는 그 경로
    (`APPDATA` 기준)와 같으면 검사 없이 통과시킨다. 경로 요소 단위로 비교하므로 끝의 역슬래시 같은 차이는 같은 경로로 보지만,
    `HERDVIA_DIR`이 같은 폴더를 가리켜도 대소문자, `\\?\` 접두사, 정션을 거친 경로 등 표기가 다르면 예외로 치지 않고 검사한다.
    `open_file`(파일의 부모 폴더를 넘긴다), `prepare_dir`, `inspect_dir`, `agent.lock` 잠금이 모두 같은 판정을 탄다. `APPDATA`가 없어 기본 폴더를 구하지 못하면 예외 없이 검사한다.
    이유는 Unix(M3의 "파일")와 같다. 점검에서 나온 문제의 조건은 다른 계정이 바꿀 수 있는 폴더 아래의 `HERDVIA_DIR`이고 기본 경로에서는 성립하지 않는다.
    프로필의 상위 폴더를 다른 계정이 바꿀 수 있는 PC라면 그 계정은 이미 로그인 항목이나 시작 프로그램을 바꿔 이 사용자로 실행할 수 있으므로, 기본 경로를 거부해서 얻는 보호가 없다.
    반면 프로필이나 `AppData`를 정션으로 다른 드라이브에 옮긴 PC에서는 기본 경로가 거부될 수 있고, 자동 시작은 기본 폴더만 쓰므로 우회할 방법이 없다.
    업그레이드한 에이전트가 시작을 거부하면 그 PC로 가는 통로가 끊긴다. 데이터 폴더, `secret.key`, `peers` 자체의 검사(소유자, DACL, 레이블)는 기본 폴더에서도 그대로다.
  드라이브 루트부터 데이터 폴더의 부모까지 각 폴더를 한 핸들로 열어 읽기만 하고, 아무것도 만들거나 고치지 않는다.
  파일 저장 방식(경로와 임시 파일)과 파이프 이름은 바꾸지 않는다. 검사한 폴더를 다른 일반 계정이 더는 바꿀 수 없다는 것이 그 뒤 경로를 그대로 쓰는 근거다.
  - 재분석 지점이면 `<폴더> is a junction, symbolic link, or other reparse point above the data directory; use a data directory whose parent folders are all real directories`로 거부한다.
  - 소유자는 이 사용자, SYSTEM, Administrators, TrustedInstaller(S-1-5-80-956008885-3418522649-1831038044-1853292631-2271478464)여야 한다.
    `C:\`와 Windows 폴더는 TrustedInstaller가 소유할 수 있어서 넣었다. 남이 먼저 만들어 둔 상위 폴더는 여기서 걸린다.
  - DACL이 NULL이면 거부한다. 그 폴더 자신에 적용되는 허용 항목(INHERIT_ONLY 항목은 뺀다) 가운데 위 넷이 아닌 계정에게 경로를 바꿀 수 있는 권한,
    즉 DELETE(그 폴더의 이름 바꾸기), FILE_DELETE_CHILD(안의 항목 이름 바꾸기), FILE_WRITE_DATA(폴더에서는 FILE_ADD_FILE)와 FILE_WRITE_ATTRIBUTES(재분석 지점 설정),
    WRITE_DAC, WRITE_OWNER(이 권한들을 스스로 얻기), 이들을 포함하는 GENERIC_ALL과 GENERIC_WRITE를 주는 것이 있으면 거부한다.
    FILE_WRITE_DATA나 FILE_WRITE_ATTRIBUTES가 있는 핸들로는 빈 폴더에 재분석 지점(정션)을 설정할 수 있다(FSCTL_SET_REPARSE_POINT).
    검사가 그 폴더를 실제 폴더로 확인한 직후 다른 계정이 정션으로 바꾸면 그 아래 경로가 다른 곳을 가리키게 되므로, 파일을 추가하는 권한도 경로를 바꾸는 권한으로 본다.
    읽기·탐색 권한과 하위 폴더를 추가하기만 하는 권한(FILE_ADD_SUBDIRECTORY, 곧 FILE_APPEND_DATA)은 받는다.
    `C:\`의 Authenticated Users 하위 폴더 추가 전용 항목과 상속 전용 Modify, CREATOR OWNER의 상속 전용 항목, 프로필 위의 Users·Everyone 읽기 항목 같은 기본 구성을 거부하지 않기 위해서다.
    FILE_ADD_SUBDIRECTORY까지 거부하면 `C:\` 때문에 드라이브 아래의 거의 모든 경로가 거부된다.
    deny 항목은 문제 삼지 않고, 검사하지 않는 허용 형식(object, callback, compound)은 열린 것으로 본다.
  - 거부 문구는 `<폴더> lets <SID> write to it (enough to turn it into a junction), or delete, rename, or change the permissions of it or of what is inside it, so that account could swap the data directory inside it for another; remove that access, or use a data directory whose parent folders only this user, SYSTEM, Administrators, and TrustedInstaller can change`이다.
    소유자와 NULL DACL, 검사하지 않는 허용 형식도 앞부분만 다르고 같은 형식이다. 오류 종류는 PermissionDenied다.
  - 이 판정은 데이터 폴더와 두 파일의 안전 규칙(다른 계정의 허용 항목을 하나도 받지 않는다)과 따로 둔다. 그 규칙을 상위 폴더에 쓰면 프로필 위의 읽기 항목 때문에 프로필 아래의 경로부터 거부되고,
    반대로 그 규칙을 느슨하게 하면 키와 `peers`의 보호가 약해지기 때문이다. 판정은 Win32 호출이 없는 함수(`src/ipc/dir_security.rs`)로 두어 모든 플랫폼에서 단위 테스트한다.
  - 검사하는 곳: `prepare_dir`(폴더를 만들거나 DACL을 고치기 전. 없는 상위 폴더는 있는 데까지만 보고, `create_dir_all`이 만든 뒤 다시 본다), `inspect_dir`과 `ipc::check_parents`(doctor),
    `private_file`과 `inspect_file`(실행 중의 `peers` 다시 읽기 포함), 에이전트의 `agent.lock` 잠금. 에이전트가 오래 돌므로 진입점마다 다시 본다.
    파이프 접속(`connect`)에는 넣지 않는다. 파이프는 파일 시스템 경로가 아니고, 접속한 파이프의 소유자 SID를 이미 확인한다.
  - 경로는 Win32가 해석하는 대로(`std::path::absolute`) 보고, 드라이브 문자로 시작하는 경로(`C:\`, `\\?\C:\`)만 검사한다.
- 무결성 레이블: DACL은 이 사용자를 허용하므로, 같은 사용자의 Low 무결성 프로세스도 그 허용 항목으로 `secret.key`를 읽을 수 있었다.
  레이블이 없는 객체는 Medium + NO_WRITE_UP으로 취급되는데, 이것은 쓰기만 막고 읽기는 막지 않는다.
  그래서 `prepare_dir`가 데이터 폴더에 `S:(ML;OICI;NWNR;;;ME)`(Medium, NO_WRITE_UP과 NO_READ_UP, 하위 파일과 폴더로 상속)를, `secret.key`와 `peers`에 `S:(ML;;NWNR;;;ME)`를 건다.
  NO_WRITE_UP은 레이블이 없을 때의 보호이므로 빼지 않는다.
  - 범위: Low 무결성 프로세스가 보호된 파일을 새로 열어 읽는 것을 막는 최선 노력 강화다. 엄격한 불변 조건이 아니다.
    레이블을 걸 수 없는 폴더와 파일(WRITE_OWNER가 없는 경우. 예: SYSTEM이 소유하고 이 사용자에게 Modify만 준 폴더)은 레이블 없이 지금처럼 쓰고,
    레이블을 걸지 못했다고 명령이나 에이전트 시작을 실패시키지 않는다. 지금 지원하는 구성을 깨지 않는 것이 우선이기 때문이다. 그런 폴더에는 이 보호가 적용되지 않는다.
    레이블을 건 뒤에도 이미 열려 있던 핸들이나 그 전에 읽힌 키(과거의 노출)는 되돌리지 못한다. 파이프와 프로세스의 경계는 이 레이블과 별개다.
  - 레이블 상태는 DACL 상태와 따로 읽고 판단한다. DACL이 이미 herdvia의 상태인 기존 설치에서도 레이블이 없으면 건다.
    객체 자신에 적용되는 첫 레이블 항목이 이미 Medium이고 NO_WRITE_UP과 NO_READ_UP이 있으면(폴더는 하위 파일과 폴더로 상속도 되면) 쓰지 않는다.
    Medium보다 높은 레이블은 Medium으로 낮추지 않고 그대로 두며, Low 이하의 레이블은 Medium으로 올린다.
  - 조회는 `READ_CONTROL`과 `LABEL_SECURITY_INFORMATION`으로, 설정은 `WRITE_OWNER`와 `LABEL_SECURITY_INFORMATION`으로 한다.
    SACL 전체(`SACL_SECURITY_INFORMATION`)는 다루지 않는다. 특권이 필요하고 감사 항목을 덮어쓰기 때문이다.
    설정이 필요할 때만 `WRITE_OWNER`로 다시 열고, DACL 수리처럼 파일 ID가 같은지 확인한 뒤 건다. DACL을 고칠 때와 따로 연다. 둘을 함께 요구하면 DACL만 고칠 수 있는 폴더에서 DACL 수리까지 못 하게 되기 때문이다.
  - 이미 있는 `secret.key`와 `peers`는 파일마다 레이블을 읽어, 부족하면 파일 자체에 레이블을 건다. 폴더의 상속 레이블이 이미 있던 파일에 늘 전파된다고 볼 수 없기 때문이다.
    폴더에서 상속된 레이블이 그 파일에 있으면 충분한 것으로 본다. 파일 DACL 수리(DACL이 열렸을 때만 일한다)와 상관없이 매번 확인한다.
  - herdvia가 새로 만드는 파일(새 키, 임시 파일로 다시 쓰는 `peers`)은 만들어질 때부터 폴더의 상속 레이블을 받는다.
    폴더에 레이블을 걸 수 없으면 새 파일은 다음 실행의 `prepare_dir`가 레이블을 걸 때까지 레이블이 없다.
  - doctor의 판정과 출력에는 레이블을 넣지 않는다.
- 남은 한계: 파일이나 폴더에 이 사용자를 겨냥한 명시적 WRITE_DAC deny가 있으면 `/grant` 안내만으로는 풀리지 않는다.
  이 사용자가 바꿀 수 없는 폴더가 deny 항목을 넘겨주면 herdvia가 새로 만드는 키와 `peers`는 그 deny를 받는다.
  - UNC 경로(`\\server\share\...`, `\\?\UNC\...`)와 그 밖의 장치 경로는 상위 폴더 검사를 하지 않고 예전처럼 동작한다. 이런 `HERDVIA_DIR`에서는 상위 폴더를 통한 바꿔치기를 막지 못한다.
    `subst`로 만든 드라이브는 그 드라이브의 루트부터만 보므로, 실제 폴더의 위쪽은 검사하지 않는다.
  - 상위 폴더 검사는 이 사용자, SYSTEM, Administrators, TrustedInstaller가 검사 뒤에 경로나 권한을 바꾸는 것까지 막지 않는다.
  - 상위 폴더가 재분석 지점이면 소유자나 권한과 상관없이 거부하므로, `HERDVIA_DIR`로 지정한 경로의 상위에 정션이나 볼륨 마운트 지점이 있으면 거부된다
    (`C:\Users`나 프로필, `AppData`를 정션으로 다른 드라이브에 옮긴 PC에서 그 아래를 지정한 경우 등). 이전 버전에서 그 `HERDVIA_DIR`을 쓰던 경우 업그레이드 뒤 에이전트와 CLI가 시작하지 못한다.
    이때는 `HERDVIA_DIR`을 정션을 거치지 않는 실제 경로로 지정한다. 그 실제 경로의 상위 폴더도 같은 검사를 통과해야 하며, 통과하지 못하면 doctor가 걸린 폴더와 계정을 알려 준다.
    기본 경로(`%APPDATA%\herdvia`)는 이 검사를 하지 않으므로 그런 PC에서도 그대로 쓴다. 정션의 소유자와 대상 경로의 상위까지 확인해 받는 방식은 정하지 않았다.
- `agent install`/`uninstall`은 M5a의 Windows에서 "지원하지 않음" 오류를 낸다.
- 기존 통합 테스트(`tests/`)는 Unix 전용으로 둔다. Windows 쪽 단위 테스트는 줄 판정 같은 순수 로직과, 실제 NTFS와 파이프 위에서 확인하는 테스트로 이루어진다.
  후자는 DACL 검사와 수리(`icacls`로 상태를 만든다), 정션과 링크 거부, 상위 폴더 검사(다른 계정의 Modify와 쓰기 권한 등, 읽기·하위 폴더 추가 전용 항목, 상위 정션, 기본 폴더 예외),
  무결성 레이블(새 폴더와 새 파일, DACL만 맞는 기존 설치, 레이블을 걸 수 없는 폴더, Low로 띄운 프로세스의 읽기 거부), `agent.lock`, `READ_CONTROL`만으로 파이프 소유자 읽기와 사용 중인 파이프 기다리기를 본다.
  CI의 windows 잡이 windows-latest에서 MSVC 툴체인으로 단위 테스트(`cargo test --locked --lib --bins`)를 돌린다.
  데이터 폴더를 쓰는 테스트는 실행 환경의 상위 폴더 권한에 따라 결과가 달라진다. 그래서 `src/ipc/windows.rs`의 테스트는 `%TEMP%`가 아니라 `%LOCALAPPDATA%\herdvia-tests` 아래에 폴더를 만들고,
  그 상위가 검사를 통과하지 않으면 그 이유를 알리며 실패한다. `%LOCALAPPDATA%`는 기본 데이터 폴더(`%APPDATA%`)와 상위가 같아, 프로필을 옮기지 않은 기기에서는 통과한다.
  `%TEMP%`는 다른 계정(샌드박스 계정 등)에 Modify를 주는 기기가 있어서 쓰지 않는다. 다른 모듈의 테스트(`peers`, `doctor`, `pair` 등)는 여전히 `%TEMP%` 아래에 데이터 폴더를 만들므로,
  그런 기기에서는 `TMP`와 `TEMP`를 검사를 통과하는 폴더로 지정하고 돌린다. CI의 windows 잡은 러너의 `%TEMP%` 권한을 미리 알 수 없으므로
  단위 테스트 전에 `%LOCALAPPDATA%\herdvia-tests`를 만들어 `TMP`와 `TEMP`로 지정한다.
  릴리스는 여전히 Linux에서 `x86_64-pc-windows-gnu`로 교차 빌드한다(gnu 디버그 빌드는 링크가 실패한다). 그래서 링크·ABI 차이는 이 잡으로 잡지 못한다.

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
- `agent --log`는 Windows 전용이다. 표준 출력·오류를 `%LOCALAPPDATA%\herdvia\herdvia.log`에 이어 쓰도록 바꾼 뒤 평소의 `agent`로 동작한다. 이 파일을 M3의 규칙(10 MiB, 180일)으로 정리한다. `--log` 없는 `agent`는 정리하지 않는다.
- `install`은 등록한 뒤 같은 명령을 콘솔 창 없이, 자신과 분리된 프로세스로 바로 띄운다(재로그온을 기다리지 않는다).
  시작한 에이전트의 파이프가 5초 안에 생기지 않으면 오류를 낸다(등록은 남긴다. 오류 문구에 로그 경로를 적는다).
- 실행 경로는 `install`을 실행한 그 실행 파일이다. 임시 폴더나 `target` 안이면 경고하되 등록은 한다(macOS와 같다).
- `HERDVIA_DIR`이 설정된 채 `install`하면 거부한다(macOS와 같다).
- `install`은 변경 전에 `TokenElevation`을 검사해 상승된 관리자 토큰이면 거부하고, 관리자 권한이 아닌 일반 PowerShell에서 실행하도록 안내한다(`uninstall`에는 적용하지 않는다).
  에이전트(M5a)와 달리 UAC가 꺼진 상승 토큰도 거부한다. doctor의 관리자 경고도 이 `install` 기준이다.
- `install`은 등록 전에 실행 파일 경로에 `%`가 있으면 거부한다(conhost의 환경 변수 확장 방지). Run 명령줄은 NUL을 제외하고 최대 259 UTF-16 코드 단위다.
- 에이전트가 이미 떠 있을 때의 `install`: Run 값이 이미 있으면(재설치) 떠 있는 에이전트를 끝내고 새로 등록·시작한다.
  Run 값이 없으면(수동 실행 중) 실패한다고 알리고 아무것도 바꾸지 않는다.
- `uninstall`은 Run 값을 지우고 떠 있는 에이전트를 끝낸다. 등록이 없거나 에이전트가 없어도 성공한다. 데이터 폴더와 로그는 남긴다.
- 떠 있는 에이전트를 끝내는 방법: 파이프에 접속해 소유자 SID를 확인한 뒤 `GetNamedPipeServerProcessId`로 얻은 프로세스를 종료하고,
  파이프가 사라질 때까지 최대 5초 기다린다.
- 넣지 않는 것: 에이전트가 죽었을 때의 자동 재시작(다음 로그온까지 꺼져 있다). M6 실사용 결과를 보고 정한다.
  로그 파일은 크기와 기간으로 관리한다(10 MiB, 180일. M3).

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
  - 목록이 상한(16개)에 이르렀으면 일치 여부와 관계없이 거부한다. 지문 대조보다 먼저 본다.
    `the list of connection attempts is full (someone may be flooding this PC with connection attempts); use herdvia allow <name> <EndpointId> --inbound with the full EndpointId`.
    가득 찬 목록에서는 가장 오래된 항목이 밀려나므로, 누구나 시도를 쏟아부어 진짜 상대를 밀어낼 수 있기 때문이다.
  - 0개: 오류. 상대가 이 PC를 `--outbound`로 allow하고 에이전트를 띄웠는지 확인하라고, 또는 전체 EndpointId로 `allow`하라고 안내한다.
  - 2개 이상: 오류. 전체 EndpointId로 `herdvia allow`를 쓰라고 안내한다.
  - 1개: 그 전체 EndpointId를 `allow`와 같은 경로(`peers` 파일 갱신 후 reload)로 등록하고 `<이름> <EndpointId> <in|both>` 한 줄을 쓴다.
    reload가 확인되지 않으면 이 줄 대신 `allow`와 같은 `saved <이름> in the peers file, but the running agent did not confirm it (...)` 실패다(M3의 "확인되지 않은 reload").
- 이름 충돌: 그 이름이 이미 다른 EndpointId로 등록돼 있으면 등록하지 않고 오류를 낸다(먼저 revoke하거나 다른 이름을 쓰라고 안내한다).
  `allow`는 같은 이름이면 항목을 바꾸지만, accept가 사람이 64자를 직접 보지 않은 ID로 기존 피어를 조용히 바꾸면 안 되기 때문이다.
  같은 이름에 같은 EndpointId면 `allow`처럼 그 항목(권한)을 바꾼다. 같은 EndpointId가 다른 이름으로 있으면 `allow`처럼 거부한다(M3의 명령).
  이름 충돌 검사를 먼저 한다.
- 수락한 ID는 대기 목록에서 빠지므로, 이미 수락한 피어의 권한을 바꾸려면 `allow`를 쓴다.

### 64비트 접두사의 한계

- 입력받는 것은 EndpointId(공개키 32바이트)의 앞 16자, 즉 64비트뿐이다. 전체 키를 대조하는 것보다 약하다.
  실제 상대가 대기 목록에 없을 때 앞 16자가 같은 다른 키가 목록에 있으면 그 키가 등록된다. 이것을 막는 것은 특정 64비트 접두사를 가진 키를 만드는 비용뿐이다.
- 일치 항목이 둘 이상이면 거부한다. 사람이 입력한 지문으로는 어느 쪽이 상대 PC인지 구분할 수 없고, 하나를 고르면 공격자의 키를 등록할 수 있기 때문이다.
  이 경우는 전체 EndpointId로 `allow`한다.
- 목록에 보인다는 것은 신뢰의 근거가 아니다. 이 PC의 EndpointId를 아는 누구나 목록에 나타날 수 있다.
  그래서 지문은 대기 목록이 아니라 상대 PC 화면에서 읽는다. 목록의 지문을 옮겨 치면 대조가 되지 않는다.

## 대화형 페어링: listen / connect

| 명령 | 역할 |
|---|---|
| `herdvia listen [<이름>]` | 연결될 대상 PC(받는 쪽)에서 5분 동안 페어링 요청을 기다리고, 6자리 확인 번호를 대조해 `--inbound` 승인 여부를 묻는다 |
| `herdvia connect <이름> [<초대>]` | Herdr를 실행하는 PC(거는 쪽)에서 LAN 자동 발견 또는 일회용 초대 문자열로 대상 PC와 페어링하고, 6자리 확인 번호가 같은지 이쪽에서도 확인받은 뒤 `--outbound` 등록과 `hc machine add`까지 수행한다 |
| `herdvia register <이름>` | 이미 페어링한 outbound 피어의 Herdr 등록(`hc machine add`)만 다시 한다 |

기존 `id`, `allow`, `pending`, `accept`는 비대화형·수동 등록용으로 그대로 둔다. `listen`과 `connect`는 처음 설치한 사용자가 긴 EndpointId 복사, 방향 플래그(`--inbound`/`--outbound`) 판단, 최대 30초 재시도 대기, `hc machine add --via ...` 따옴표 처리를 직접 하지 않게 하려는 것이다.

### 흐름과 종료 코드

1. 받는 쪽 B에서 `herdvia listen [<이름>]`을 실행하면 시작할 때 `herdr-core` 설치 여부를 `doctor` 5번과 같은 기준으로 확인해, 없거나 경로·버전이 어긋나면 표준 오류에 `warning: ...`을 먼저 쓴다(실행을 막지는 않는다).
2. `herdvia listen [<이름>]`은 에이전트가 떠 있어야 하며, 이미 다른 `listen`이 돌고 있으면 거부한다(255).
   에이전트는 5분짜리 일회용 비밀 두 개(`invite_secret` 16바이트, `listen_id` 16바이트)를 메모리에만 만들고, CLI는 원격 연결용 초대 문자열 `hvi1-<base32>`와 이 PC의 지문(`This PC's fingerprint: ...`, `herdvia id` 둘째 줄과 같은 값)을 화면에 출력하는 동시에 포그라운드 UDP 소켓으로 같은 LAN에 신호를 보낸다.
   지문은 거는 쪽이 LAN 후보 가운데 이 PC를 고를 때 대조할 기준이다.
3. 거는 쪽 A에서 `herdvia connect <이름> [<초대>]`를 실행한다. `<이름>`이 이미 outbound로 등록돼 있거나 다른 키가 쓰고 있으면 다이얼 전에 거부한다(255).
   이미 outbound로 등록된 이름의 거부 문구는 `peer <이름> is already registered; revoke it first or choose another name; to re-run only the Herdr registration, run herdvia register <이름>`이다. 페어링은 됐는데 Herdr 등록만 실패한 경우 좋은 페어링을 버리지 않고 되돌아갈 길을 알려 주려는 것이다(아래 "Herdr 등록만 다시: register"). 단, 명시적 `<초대>`를 넘겼고 초대의 `EndpointId`가 이미 같은 `<이름>`에 등록된 키와 같으면 재페어링·복구를 허용한다.
   - `<초대>`를 주지 않으면 포그라운드 UDP 소켓으로 2초 동안 같은 LAN의 `listen` 중인 PC를 찾는다. 1대 이상이면 대수와 관계없이 호스트명과 지문을 번호 메뉴로 보여 주고, 대상 PC의 `listen` 화면에 나온 지문과 같은 항목을 고르라고 안내한다(`0`은 초대 붙여 넣기). 비콘은 인증되지 않아 같은 LAN의 누구나 자기 키로 후보를 만들 수 있으므로, 1대여도 자동으로 고르지 않는다.
     0대면 화면의 `hvi1-...` 초대를 붙여 넣으라고 묻는다(메뉴나 초대 입력이 빈 줄이나 EOF로 끝나면 취소로 보아 1로 끝낸다).
   - `<초대>`를 주면 LAN 탐색 없이 초대에 들어 있는 `EndpointId`로 바로 건다(외부망이면 공개 릴레이·주소 조회 경유).
4. 두 에이전트는 전용 ALPN `herdvia/pair/1` 위에서 TLS 1.3 exporter에 묶인 commit-reveal로 6자리 확인 번호(`NNN-NNN`, `000-000`~`999-999`)를 계산해 양쪽 화면에 동시에 보여 준다.
5. 거는 쪽 A의 CLI는 확인 번호 아래에 `Same code on both PCs? [y/N]: `를 묻는다. 번호가 다르면 다른 PC에 연결된 것일 수 있으니 `n`을 답하라고 함께 안내한다.
   - `y`(`yes`)면 에이전트에 로컬 통신의 STDIN 프레임 `approve`를 보내고 `Waiting for approval on the other PC...`를 보여 준다. 그 밖(`n`, 빈 줄, EOF)이면 `reject`를 보내고 1로 끝낸다. 프레임 형식은 `listen`의 승인 입력과 같다.
   - A의 에이전트는 이 확인과 B의 승인 응답을 둘 다 받았을 때만 저장한다. 도착 순서는 상관없다. B의 승인이 먼저 와도 확인이 올 때까지 아무것도 저장하지 않는다.
   - 확인이 확인 번호를 보낸 뒤 60초 안에 오지 않거나, `reject`가 오거나, 로컬 통신이 끊기면(`Ctrl-C`) 페어링 연결을 닫고 아무것도 저장하지 않는다. 확인 프레임을 보내지 않는 구버전 CLI가 새 에이전트에 붙어도 이 시간 초과로 저장 없이 끝난다.
   - 프롬프트를 기다리는 동안에도 에이전트가 먼저 보내는 프레임(B의 거부, 오류)을 처리한다.
   - 새 CLI가 구버전 에이전트(업그레이드 뒤 다시 시작하지 않은 에이전트)에 붙으면 확인을 보내기 전에 `APPROVED`가 온다. 구버전 에이전트는 이때 이미 B를 저장했으므로, CLI는 `hc machine add`를 실행하지 않고 255로 끝내며 `herdvia revoke <이름>` 뒤 에이전트를 다시 시작하고(`herdvia agent install`) 다시 페어링하라고 안내한다.
     확인을 먼저 보내면 구버전 에이전트는 그 프레임을 예상하지 못해 저장 없이 오류로 끝나고, CLI는 에이전트를 다시 시작하라고 안내한다.
6. 받는 쪽 B의 사용자가 두 화면의 6자리 번호가 같음을 확인하고 `Allow? [y/N]: `에 `y`(`yes`)를 입력하면(프롬프트 표시 후 60초 제한, 전체 `listen` 5분 만료 및 동시 실패 중단도 함께 감시):
   - B는 A를 `--inbound`로 `peers`에 저장하고 즉시 reload한다(기존 outbound 권한이 이미 있던 피어면 `both`로 승격한다). `<이름>`을 생략했으면 A가 보낸 정화된 호스트명을 기본 이름으로 쓰되, 다른 키가 이미 쓰는 이름이면 A의 `EndpointId` 앞부분 16진수 접두사(`-<4~24자>`)를 붙인다. 이미 같은 키가 등록돼 있으면 기존 이름을 유지한다.
     호스트명이 `--help`나 `-h`면 쓰는 키가 없어도 같은 접미사를 붙인다. 그 이름으로 저장하면 `herdvia register --help`와 `herdvia revoke --help`가 도움말이 되어 등록할 수 없고 `revoke -- <이름>`으로만 지울 수 있기 때문이다(아래 "도움말: --help").
     B의 승인과 A의 확인은 서로를 기다리지 않는다. 그래서 B가 승인하고 A가 거절하면(순서와 관계없이. A가 먼저 거절해 연결을 닫은 뒤에 B가 `y`를 눌러도 B는 저장한다) B에는 A가 inbound로 남지만, A가 B를 outbound로 저장하지 않았으므로 연결은 생기지 않는다. 필요하면 B에서 `herdvia revoke`로 지운다.
   - B가 승인 응답을 보내고 A의 사용자도 확인했으면 A는 B를 `<이름>`으로 `--outbound` 저장하고 즉시 reload한다(재접속 대기 중이던 기존 outbound 워커도 즉시 깨워 재시도 지연을 초기화하며, LAN 발견으로 연결했으면 발견된 소켓 주소를 에이전트 `addr_hints`에 넣어 릴레이 없이 바로 직접 연결한다).
   - A의 CLI는 최대 5초 동안 `status`가 연결됨을 보일 때까지 짧게 기다린 뒤, 현재 실행 파일 경로를 OS 규칙(Unix는 `sh` 단일 인용, Windows는 `CommandLineToArgvW` 인용)에 맞게 감싼 `--via` 값으로 `hc machine add --via <...> --name <이름>`을 직접 실행한다(셸을 거치지 않는다).
7. 종료 코드:
   - `0`: 양쪽 피어 등록(거는 쪽은 `hc machine add`까지) 성공.
   - `1`: 받는 쪽 거부(`n`/빈 줄/EOF), 거는 쪽 확인 거부(`n`/빈 줄/EOF), 프롬프트 입력 취소(빈 줄/EOF), 대기 시간 초과(연결 대기 5분 또는 받는 쪽 승인 입력 60초), 또는 피어 등록은 됐으나 마지막 `hc machine add`가 없거나 실패함(이 경우 복사해 실행할 `hc machine add ...` 줄을 표준 오류에 안내한다).
   - `255`: 인자 오류, 에이전트 미실행, 이름·키 충돌, 잘못된 메뉴 선택이나 초대 문자열, 프로토콜·인증 실패(잘못된 증명 또는 Challenge 이후 중단 5회 누적으로 `listen`이 중단된 경우 포함),
     거는 쪽 확인 입력 60초 초과(에이전트가 오류로 끝내므로 확인 프롬프트가 없는 구버전 CLI도 이유를 보여 준다), 확인 없이 승인한 구버전 에이전트.

### Herdr 등록만 다시: register

`connect`가 페어링은 했는데 `hc machine add`가 없거나 실패하면, `connect`를 다시 실행해도 이미 등록된 이름이라 거부된다. `herdvia register <이름>`은 키, 권한, 에이전트를 건드리지 않고 Herdr 등록만 다시 한다.
에이전트가 떠 있지 않아도 되고, `connect`와 달리 `status`로 연결을 기다리지 않는다.

1. `peers`를 읽는다(키를 만들지 않는다). 그 이름의 피어가 없거나 outbound 권한이 없으면 `no outbound peer named <이름>; pair it with herdvia connect first`로 실패한다(255).
2. `hc`를 `connect`와 같은 방법으로 찾고, 있으면 `hc machine ls --json`(15초 제한)을 실행해 그 이름의 머신을 본다. 의존성을 늘리지 않으려고 JSON은 작은 전용 읽기 함수로 `machines` 배열 항목의 `name`, `target`, `via`만 읽는다.
   - `via`가 true이고 `target`이 이 실행 파일로 `connect`가 만들 `--via` 값(현재 실행 파일 경로와 ` via <이름>`, OS 규칙대로 인용)과 정확히 같으면 표준 출력에 `<이름> is already registered in Herdr`를 쓰고 0으로 끝난다. `hc machine add`는 실행하지 않는다.
   - `via`가 true이고 `target`이 ` via <이름>`으로 끝나며 그 앞 프로그램의 파일 이름이 `herdvia`나 `herdvia.exe`이지만 경로가 다르면(옮긴 설치, 빌드 폴더의 실행 파일)
     `Herdr's <이름> uses another herdvia executable (<hc가 보여 준 프로그램>); remove it with hc machine rm <이름>, then run herdvia register <이름>`으로 1로 끝난다.
     옛 경로를 건강한 등록으로 보이면 안 되고, `hc machine rm`을 대신 실행하지는 않는다.
   - 그 밖(`via`가 false이거나 다른 피어나 다른 프로그램을 거치는 머신)이면
     `Herdr already has a machine named <이름> that does not go through herdvia to peer <이름>; rename or remove it (hc machine rename / hc machine rm), then run herdvia register <이름>`으로 1로 끝난다.
   - `hc machine ls`가 실패하거나, 15초를 넘기거나, 출력을 읽을 수 없거나, 그 이름이 없거나, `via`나 `target`이 없으면 이 확인을 건너뛰고 다음으로 간다. 읽지 못한 것을 "이미 등록됨"으로 보지 않는다.
     JSON 문법에 맞지 않는 출력(숫자 문법 포함)과, 읽는 키(맨 위 `machines`, 항목의 `name`, `target`, `via`)가 한 객체에 두 번 나오는 출력도 읽을 수 없는 것으로 본다. 서로 다른 두 값 중 하나를 골라 판정하지 않으려는 것이다.
3. `connect`와 같은 `hc machine add --via <...> --name <이름>`을 15초 제한으로 실행한다.
   - 성공: 표준 출력에 `Registered machine '<이름>' in Herdr.`, 0.
   - `hc`가 없음: `'hc' was not found, so '<이름>' was not added to Herdr. After installing Herdr, run:`과 들여 쓴 수동 `hc machine add ...` 줄(`connect`가 쓰는 줄과 같다), 1.
   - 실패: `failed to register '<이름>' in Herdr (<hc가 쓴 첫 줄>). To register manually, run:`과 들여 쓴 수동 줄, 1.
   - 시간 초과: `hc machine add did not finish within 15 seconds, so whether Herdr registered '<이름>' is unknown; check hc machine ls, and if '<이름>' is missing, run:`과 들여 쓴 수동 줄, 1.
     죽이기 전에 등록이 끝났을 수 있으므로 "등록되지 않았다"고 하지 않는다.

이 절의 오류 줄은 모두 `herdvia: `로 시작하고 stderr에 쓴다. 종료 코드 1은 `connect`처럼 Herdr 등록의 결과이고, 255는 인자 오류와 피어 조회 실패다.
이미 등록됐는지는 문자열이 정확히 같은지로만 본다. 같은 실행 파일을 심볼릭 링크나 다른 경로 표기로 실행하면 다른 실행 파일로 보고 `hc machine rm` 안내를 낸다(아무것도 바꾸지 않고 1로 끝난다).

### 프로토콜과 보안 경계

- **ALPN 격리 (`herdvia/pair/1`)**: 원격 명령을 실행하는 `herdvia/1`과 완전히 분리한다. 에이전트의 `Policy::after_handshake`는 받는 쪽에서는 현재 활성 `listen` 세션이 열려 있을 때(동시 진행 연결 최대 4개), 거는 쪽에서는 활성 `connect`가 지정한 대상 `EndpointId`일 때만 `herdvia/pair/1` 핸드셰이크를 통과시키고, 평소에는 즉시 거부하며 `pending` 목록에도 남기지 않는다. 연결당 양방향 스트림 1개만 처리하고 단계별 시간 제한(단계당 15초, 받는 쪽 승인 입력과 거는 쪽 확인 입력 각 60초, 전체 `listen` 5분)을 둔다. 로컬 IPC가 끊기면(`Ctrl-C` 등) 활성 `listen`·`connect` 상태와 열린 페어링 연결을 즉시 정리한다.
- **포그라운드 전용 LAN 발견**: 상주 `herdvia agent`는 LAN 발견용 UDP 포트를 열지 않는다. `herdvia listen`과 `herdvia connect` CLI 프로세스가 실행되는 짧은 동안에만 UDP 포트 `48721`(IPv4 멀티캐스트 `239.255.77.86`, 브로드캐스트 `255.255.255.255`, 루프백 `127.0.0.1`)로 `HDVD1`(탐색 질의)과 `HDVB1`(비콘: 버전, 에이전트 QUIC 포트, `EndpointId`, `listen_id`, 정화된 호스트명)을 주고받는다. `listen`은 `HDVD1` 질의에 비콘을 유니캐스트로 답하는데, 질의의 출처가 사설(RFC 1918), 링크 로컬(`169.254.0.0/16`), 루프백 주소일 때만 답하고 그 밖의 출처는 조용히 무시한다. 소켓이 `0.0.0.0`에 바인드되므로 이 포트에 닿는 원격 주소에 LAN 경로의 인증 토큰인 `listen_id`를 내주지 않기 위해서다(주기적인 멀티캐스트·브로드캐스트 비콘은 그대로 보낸다). 게스트 Wi-Fi 클라이언트 격리나 방화벽으로 UDP가 막혀 있으면 `hvi1-...` 초대 문자열로 똑같이 연결된다.
- **인증되지 않는 비콘과 거는 쪽 확인**: 비콘에는 서명이 없어, 같은 LAN의 누구나 자기 키와 아무 `listen_id`로 비콘을 보낼 수 있다. 그 키로 걸면 `auth_proof`는 상대가 고른 `listen_id`에 대해 맞고, 상대는 Challenge 뒤 곧바로 `Approved`를 보낼 수 있다. 상대가 보낸 `Approved`만으로 저장하면 사람의 확인 없이 그 키가 `--outbound`로 저장되고 Herdr에 머신으로 등록된다.
  그래서 거는 쪽은 두 가지로 막는다. 후보를 자동으로 고르지 않고 `listen` 화면의 지문과 대조해 고르게 하며, 거는 쪽 사용자가 두 화면의 확인 번호가 같다고 답해야만 저장한다. 공격자의 키로 걸었다면 대상 PC의 `listen` 화면에는 확인 번호가 아예 나오지 않거나, 공격자가 비콘에서 얻은 `listen_id`로 대상 PC에 따로 걸어 프롬프트를 띄우더라도 commit-reveal 때문에 그 번호가 거는 쪽 화면의 번호와 다르다(같을 확률은 시도당 100만분의 1). 그래서 사람이 두 화면을 대조하면 알아챌 수 있다.
  네트워크 프로토콜(`herdvia/pair/1`의 메시지)은 바꾸지 않았으므로 구버전과 그대로 통신한다. 바뀐 것은 거는 쪽의 로컬 동작뿐이다.
- **일회용 초대 문자열 (`hvi1-...`)**: `target_id(32바이트) || invite_secret(16바이트)` 48바이트를 소문자 무패딩 RFC 4648 Base32(77자)로 인코딩해 `hvi1-` 뒤에 붙인 82자 토큰이다(앞뒤 공백과 대소문자 무시). 별도 랑데부 서버를 두지 않고 공개키와 128비트 일회용 비밀을 함께 옮긴다.
- **TLS 채널 바인딩과 commit-reveal SAS**:
  - 양쪽 에이전트는 QUIC 연결의 TLS 1.3 exporter(`export_keying_material`, 라벨 `"herdvia-pair-v1"`, 빈 컨텍스트, 32바이트)를 뽑는다. 중간자(MITM)가 양쪽과 각각 다른 TLS 세션을 맺으면 두 exporter 값이 달라진다.
  - A는 먼저 `auth_proof = BLAKE3_keyed(derive_key("herdvia-pair-invite-v1" | "herdvia-pair-lan-v1", token), exporter)`와 `commit_a = BLAKE3("herdvia-pair-commit-a-v1" || exporter || id_a || id_b || nonce_a)`를 보낸다. B는 `auth_proof`가 현재 활성 `listen`의 `listen_id` 또는 `invite_secret`과 일치하는지 상수 시간에 검증하고, 틀리거나 Challenge 발급 뒤 Reveal 누락·커밋 불일치가 발생하면 실패 횟수를 올려 5회에 이르면 그 `listen` 세션 전체를(승인 입력 대기 중이더라도) 즉시 폐기한다.
  - `auth_proof`가 맞으면 B는 프롬프트 잠금(`prompt_lock`)을 잡아 한 번에 한 요청만 진행시키고(그사이 들어오는 다른 요청은 `429 busy`로 거절), `nonce_b`를 보낸다. A가 `nonce_a`를 공개해 `commit_a`와 일치하면 양쪽 모두 `BLAKE3("herdvia-pair-sas-v1" || exporter || id_a || id_b || nonce_a || nonce_b)` XOF 출력에서 `4_294_000_000` 미만인 리틀엔디언 `u32`를 기각 샘플링(`% 1_000_000`)해 편향 없는 6자리 번호를 얻는다.
  - commit-reveal로 어느 한쪽도 상대 난수를 보고 자기 난수를 고르지 못하며, 사용자가 승인하거나 거절하는 즉시(원자적으로 만료·중단 여부를 재확인한 뒤) 해당 `listen` 세션이 종료(`completed = true`)되어 재사용할 수 없다.
- **최소 권한 유지**: `listen`은 `--inbound`만, `connect`는 `--outbound`만 부여한다(기존 반대 방향 권한이 이미 있을 때만 `both`가 된다). 자동으로 `--both`를 주지 않는다.

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
- 내려받기는 HTTPS로만 한다. `SHA256SUMS`도 zip과 같은 곳에서 받으므로, 평문으로 받으면 중간에서 둘을 함께 바꿀 수 있어 체크섬 검사가 막지 못한다.
  두 스크립트 모두 curl에 `--proto =https --proto-redir =https --tlsv1.2`를 주어 리다이렉트로 http로 내려가는 것도 막는다(Windows는 `System32\curl.exe`).
  `HERDVIA_RELEASE_BASE`는 `https://`로 시작해야 한다. 예외는 CI가 로컬 서버로 시험하는 루프백 호스트(`127.0.0.1`, `localhost`, `[::1]`, 숫자 포트만)의 `http://`이고, 이때만 첫 요청에 http를 허용한다(리다이렉트는 여전히 https만).
  호스트 앞에 사용자 부분이 붙은 주소(`http://localhost:1@example.com`)는 루프백이 아니므로 거부한다. 거부는 아무것도 받기 전에 하고 아무것도 설치하지 않는다.
- 받은 실행 파일을 확인하려고 `herdvia --version`을 둔다. 표준 출력에 `herdvia <Cargo 패키지 버전>` 한 줄을 쓰고 0으로 끝난다. 데이터 폴더나 키를 만들지 않는다.
- herdr-core는 설치하지 않는다. herdr-core는 Herdr의 것이고 Herdr의 설치 스크립트가 있다. 스크립트는 herdr-core가 없거나
  에이전트가 찾지 못하는 위치에 있으면 Herdr의 설치 명령(macOS에서 다른 곳에 있으면 `~/.local/bin`에 링크하는 명령)을 안내만 하고, 설치는 성공으로 끝낸다.
- 피어 등록과 `hc machine add`도 하지 않는다.
- 릴리스 워크플로는 다섯 자산(두 zip, `install.sh`, `install.ps1`, `SHA256SUMS`)에 GitHub 빌드 출처 증명(`actions/attest-build-provenance`, Sigstore 서명)을 붙인다.
  사용자는 `gh attestation verify <파일> --repo photojupi/herdvia --signer-workflow photojupi/herdvia/.github/workflows/release.yml`로 확인한다.
  이 명령은 파일 내용과 일치하는 서명된 출처 증명이 있고, 그 증명을 이 저장소의 `.github/workflows/release.yml`이 만들었는지 본다.
  `--repo`만 주면 이 저장소의 다른 워크플로가 만든 증명도 통과하므로 서명한 워크플로를 `--signer-workflow`로 고정한다. 증명은 이 저장소의 release.yml이 만든 파일임을 보일 뿐이라,
  계정이 탈취돼 새 태그로 빌드된 릴리스도 유효한 증명을 받는다. `SHA256SUMS`는 같은 릴리스에서 받으므로 전송 손상만 잡는다. v0.1.2까지의 릴리스에는 증명이 없다.
- 서명 권한(`id-token: write`, `attestations: write`)은 zip을 묶고 증명을 붙이는 package 잡에만 준다. `THIRD-PARTY-LICENSES.html`은 `contents: read`만 가진 licenses 잡이
  cargo-about을 `cargo install`로 빌드해 만들고 artifact로 넘긴다. cargo-about과 그 의존성 트리를 컴파일하고 실행하는 잡에 서명 권한이 있으면, 그 가운데 악성 크레이트 하나가
  임의의 파일에 이 워크플로 이름의 증명을 받을 수 있기 때문이다. package는 licenses가 성공해야 돌고 release는 package가 성공해야 돌므로 라이선스 파일 없이 릴리스가 나가지 않는다.
  잡을 나누는 것만으로는 부족하다. 같은 실행의 잡은 모두 그 실행의 artifact를 지우고 같은 이름으로 다시 올릴 수 있으므로, licenses 잡의 악성 코드가 빌드 잡이 올린 실행 파일을 바꿔치기하면
  package가 그것을 묶어 증명을 붙인다. 그래서 빌드 잡은 실행 파일의 sha256을 잡 output으로 내보내고, package는 내려받은 실행 파일을 그 값과 대조한 뒤에야 묶는다.
  잡 output은 다른 잡이 바꿀 수 없다. package와 release 사이도 같다. package가 `SHA256SUMS`의 sha256을 output으로 내보내고, release는 내려받은 `SHA256SUMS`를 그 값과,
  올릴 파일을 `SHA256SUMS`와 대조한다. 대조에는 `sha256sum --strict -c`를 쓴다. `--strict`가 없으면 output이 비어 형식이 틀린 줄은 경고만 내고 통과한다.
- 워크플로의 액션은 모두 커밋 SHA로 고정하고 버전은 주석으로 단다. 태그가 옮겨져도 다른 코드가 빌드에 들어오지 않게 하려는 것이다.
  Dependabot은 github-actions만 매주 확인해 고정 SHA를 올리는 PR을 연다(cargo 의존성 PR은 열지 않는다).
- 의존성 권고는 audit.yml이 매주, 수동 실행, main에서 `Cargo.lock`이 바뀔 때 `cargo audit --deny warnings`로 확인한다(unmaintained·yanked도 실패).
  Linux 전용 빌드 도구인 paste(RUSTSEC-2024-0436)만 무시한다. 새 권고가 무관한 PR을 깨지 않도록 pull_request에는 걸지 않는다.
- CI의 macOS 잡은 단위 테스트와, 공개 n0 릴레이·주소 조회 없이 도는 통합 테스트(루프백 LAN 페어링 포함, 이름으로 나열한다)를 PR마다 돌린다.
  나머지 통합 테스트는 공개 n0 릴레이와 주소 조회가 필요하고 연결을 30~45초까지 기다리므로, integration.yml이 매주와 수동 실행 때 전체를 돌린다(PR 필수 검사가 아니다).
  Windows 단위 테스트는 M5a의 windows 잡이다.

## 점검: doctor

| 명령 | 역할 |
|---|---|
| `herdvia doctor` | 설치와 연결이 왜 안 되는지를 한 번에 보여 준다 |

- 읽기 전용이다. 데이터 폴더, `secret.key`, `peers`, 자동 시작 등록, 에이전트 상태를 만들거나 바꾸지 않는다.
  키가 없어도 만들지 않는다(키를 만드는 `peers::load_key` 대신 `ipc::private_file`, Windows는 `ipc::inspect_file`로 열어 읽기만 한다).
  실행하는 외부 프로그램은 `herdr-core --version`(2초 제한) 하나다. 출력은 앞 4096바이트만 읽고, 해석하지 못한 출력은 첫 줄 80자까지만 쓴다.
  Unix에서는 새 프로세스 그룹으로 띄워 시간이 넘으면 그룹 전체를 죽인다(Windows는 직접 자식만 죽는다). root 실행은 다른 명령처럼 거부한다.
- 출력: 표준 출력에 점검 결과 한 줄씩. 줄 머리는 `ok`, `warn`, `fail`(4칸에 맞추고 공백 하나)이고, warn과 fail 줄에는 할 일을 같은 줄에 쓴다.
  fail이 하나라도 있으면 종료 코드 1, 없으면 0이다(warn만 있으면 0). 인자 오류 같은 기존 오류는 `herdvia: …` 한 줄과 255 그대로다.
- 점검 항목(이 순서):
  1. 이 실행 파일의 버전과 경로. 임시 폴더나 `target` 아래면 warn(`agent install`이 이 경로를 기록한다. 판정은 `agent install`의 경고와 같다).
  2. 데이터 폴더. `HERDVIA_DIR`에서 왔으면 그렇다고 쓴다(자동 시작은 기본 폴더만 쓴다). 없으면 warn(`herdvia id`가 만든다).
     그 전에 상위 경로를 에이전트와 같은 검사(`ipc::check_parents`. Windows는 `check_ancestors`를 부른다)로 읽기만 하며 확인한다. 거부되면 폴더가 아직 없어도 warn이 아니라 그 문구로 fail이다.
     에이전트처럼 `HERDVIA_DIR`로 고른 위치만 검사하고, 기본 데이터 폴더는 검사 없이 통과한다.
     그 자리에서는 `herdvia id`도 거부되므로 만들라고 안내하면 틀린 안내가 된다.
     `HERDVIA_DIR`이 절대 경로가 아니면 `fail data directory: HERDVIA_DIR must be an absolute path, not "<값>"; set it to a full path, or unset it`이다.
     디렉터리가 아니거나(폴더를 `symlink_metadata`로 보므로 심볼릭 링크도, Windows는 정션도) 다른 사용자 소유이거나 읽을 수 없으면 fail. 이 사용자 소유여도 목록을 읽거나 안을 탐색할 수 없으면 fail이다(`read_dir`와 `폴더/.` 조회로 판정하고, Unix는 `chmod 700`을 안내한다). Unix는 `agent.sock` 경로가 소켓 주소 한도를 넘으면 fail.
     에이전트가 거부하는 폴더도 fail이고 그 오류 문구를 그대로 쓴다(Unix는 `ipc::check_dir`, Windows는 `ipc::inspect_dir`). macOS는 allow 항목이 있는 ACL(`chmod -N` 안내),
     Windows는 다른 소유자, 그 밖의 재분석 지점, 이 사용자가 바꿀 수 없는 열린 DACL(`icacls … /grant` 안내)이다.
     Windows에서 안전한 폴더는 보호 여부와 관계없이 ok다. 다른 계정에 열려 있지만 이 사용자가 DACL을 바꿀 수 있으면(다음 실행에서 herdvia가 제한한다) warn이고,
     줄은 `warn data directory: <경로>; it allows access to accounts other than this user, SYSTEM, and Administrators; herdvia restricts it the next time it runs (herdvia id, or the agent when it starts)`이다.
     이 warn에서도 3, 4는 이어서 한다.
     폴더 자체가 fail이면 3, 4를 건너뛰고, 이 줄이 fail이면(소켓 경로 포함) 7~9도 건너뛴다. 원인은 이 줄에 있으므로 뒤 줄에서 틀린 안내를 하지 않는다.
  3. `secret.key`. 없으면 warn. 에이전트와 같은 권한 검사(Unix는 이 사용자 소유, 0600 이하, macOS는 allow ACL 없음. Windows는 링크가 아닌 일반 파일, 위 세 소유자, 안전한 DACL)에 어긋나거나 32바이트가 아니면 fail. 있으면 지문만 쓴다.
     Windows에서 DACL이 열려 있어도 이 사용자가 그 파일의 DACL을 바꿀 수 있으면(다음 실행에서 herdvia가 고친다. 폴더 상태는 보지 않는다)
     `warn secret.key: present, fingerprint <지문>; <2와 같은 안내>`이고, 바꿀 수 없으면 에이전트와 같은 `icacls … /grant` 문구로 fail이다.
  4. `peers`. 에이전트와 같은 로더로 읽는다. 형식·권한 오류는 로더의 문구를 넣어 fail(그 줄을 고치라는 안내는 형식 오류일 때만). 방향별 개수를 쓰고, 파일이 없으면 0개다.
     Windows에서 3과 같은 조건의 열린 DACL은 `warn peers: <2와 같은 안내>`이고 개수는 쓰지 않는다. 이때도 내용을 검사해, 형식 오류면 평소의 fail 줄을 낸다.
     다음 실행의 제한은 내용을 고치지 않으므로, warn만 보고 넘어가면 그 실행이 형식 오류로 실패하기 때문이다.
     2~4의 이 warn은 데이터 폴더를 준비하는 명령(`prepare_dir`를 거치는 `herdvia id`, `allow`, `accept`, `revoke`, 에이전트 시작)이 제한할 때까지 남는다.
     doctor와 `status`, `pending`, `via`, `peers`, `register`, `agent reload`는 아무것도 바꾸지 않는다. v0.1.2는 Windows에서 DACL을 건드리지 않았으므로, v0.1.2에서 올린 PC는 새 버전으로 그런 명령을 처음 실행하기 전까지
     2~4 모두 warn일 수 있다(실측: v0.1.2가 설치된 Windows 11 PC 두 대에서 data directory, secret.key, peers 세 줄 모두 warn).
     그 명령 뒤에는 ok다. 폴더에는 보호 DACL이 걸려 세 신뢰 계정의 항목만 남고, 두 파일도 세 계정의 항목만 남는다(임시 폴더로 모사해 확인). 설치 스크립트(`install.ps1`)는 `herdvia id`를 실행한다.
     3, 4의 거부에 붙이는 일반 안내는 거부 문구에 해법이 없을 때만 붙인다. 해법이 이미 있는 문구에 다른 해법을 덧붙이면 무엇을 해야 할지 흐려지기 때문이다.
     이 규칙은 권한 거부(PermissionDenied)로 끝난 경우에만 쓴다. 그 밖의 오류는 `secret.key`면 `fix its permissions or ownership if that is the cause; otherwise remove it, run herdvia id to create a new identity, and pair again with every peer (restoring a copied or backed-up key is not recommended)`
     (README의 보안 모델과 같이, 백업에서 되살리지 말고 새 신원을 만들라는 안내다. 32바이트가 아닌 키도 같다), `peers`면 형식 오류는 그 줄을 고치라는 안내, 나머지는 `it must be a regular file that this user can read`다.
     Unix는 `make this user its owner and run chmod 600 on it`이고, 소유자·0600 거부와 권한 거부 OS 오류(EACCES)에는 붙이고 macOS ACL 거부(`chmod -N`이 든 문구)에는 붙이지 않는다.
     Windows는 `check that it is a regular file, not a link, that this user can read`이고, 권한 거부 OS 오류(예: Access is denied)에만 붙이며 herdvia 자신의 종류·소유자·DACL 거부에는 붙이지 않는다.
  5. herdr-core. 에이전트가 실제로 찾는 위치를 본다. Windows는 `%USERPROFILE%\AppData\Local\Programs\herdr-core\herdr-core.exe`,
     macOS는 LaunchAgent가 주는 PATH(`~/.local/bin:/usr/bin:/bin:/usr/sbin:/sbin`)의 실행 가능한 파일, 그 밖의 Unix는 현재 PATH와 `~/.local/bin`이다.
     macOS에서 현재 PATH에만 있으면 warn하고 `~/.local/bin/herdr-core`로 링크하는 `mkdir -p … && ln -s …` 명령을 쓴다. 현재 PATH의 빈 항목과 상대 경로 항목은 건너뛰므로, 표시하는 경로와 링크 대상은 절대 경로다. 그런 항목은 현재 폴더를 가리키는데, 에이전트는 그 폴더에서 실행되지 않고 doctor는 찾은 파일을 실행하므로, 남이 파일을 둘 수 있는 폴더에서 doctor를 실행하면 그 폴더의 `herdr-core`가 실행되기 때문이다. `listen`이 시작할 때 하는 같은 점검도 이 탐색을 쓰고, `connect`가 `hc machine add`를 위해 `hc`를 찾을 때도 같은 이유로 그런 항목을 건너뛴다. 어디에도 없으면 fail이고 Herdr 설치 스크립트를 안내한다(설치는 하지 않는다).
     찾으면 `--version` 첫 줄의 마지막 단어를 버전으로 읽는다. 검증 버전(상수 `VERIFIED_CORE_VERSION`, `0.1.21`)과 다르면 warn(connect 줄이 바뀌었으면 원격이 거부한다. 로그의 `rejected command`를 보라). 실행이 실패해도 warn.
  6. 자동 시작. macOS는 plist의 `ProgramArguments` 첫 값, Windows는 Run 값을 읽기만 한다(`launchctl`은 부르지 않는다).
     등록이 없거나 다른 실행 파일을 가리키면 warn(`agent install`을 다시 실행). Windows는 Run 값을 `install`이 쓸 명령과 ASCII 대소문자를 무시하고 비교한다. Linux는 지원하지 않는다는 warn. Windows에서 관리자 권한으로 돌고 있으면 `agent install`이 거부된다는 warn을 한 줄 더 쓴다.
  7. 에이전트. 제어 요청 `doctor`를 보낸다. 응답이 없으면 fail. 버전이나 실행 파일 경로가 이 실행 파일과 다르면 warn(업그레이드 뒤 다시 시작하지 않은 경우).
     doctor가 보내는 제어 요청(`doctor`, 구버전 확인용 `status`, 8의 `status`와 `status-detail`, 9의 `pending`)은 요청마다 연결부터 응답 끝까지 3초(상수 `AGENT_TIMEOUT`)만 기다린다.
     `doctor`나 구버전 확인용 `status`가 시간을 넘기면 "실행 중일 수 있으나 3초 안에 답하지 않았다"는 fail이고 뒤 요청은 보내지 않는다. 8의 `status`가 넘기면 warn이고 9는 묻지 않는다.
     8의 `status`에는 답하고 `status-detail`이 넘기면 피어 줄은 예전 문구로 쓰고 `warn peer details: the agent did not answer within 3 seconds; restart it: <시작 명령>`을 더하며 9는 묻지 않는다. 9가 넘기면 warn이다.
     그래서 멈춘 에이전트를 기다리는 시간은 한 번의 제한 시간 정도다. 연결이 끊기는 것(구버전)과 시간 초과는 구별한다.
     홈 릴레이에 연결되어 있지 않으면 relay 줄이 warn이다(외부 HTTPS 연결과 프록시 환경을 확인하라. herdvia는 프록시 환경 변수를 쓰지 않는다).
  8. 피어별 상태. 기존 `status` 응답에서 피어마다 한 줄. connected는 ok이고, macOS에서 relay면 로컬 네트워크 허용 안내를 덧붙인다.
     disconnected는 warn이고, out/both면 상대 에이전트가 꺼졌거나 상대가 이 PC를 허용하지 않았다고, in이면 상대가 걸어 와야 연결된다고 안내한다. 피어가 0개면 warn 한 줄.
     표는 여전히 `status` 답으로 판단하고, `status-detail`을 따로 요청한다. 답을 해석할 수 있으면 연결되지 않은 피어의 warn 줄은 `herdvia status`의 이유 줄에서 앞의 이름을 뺀 문구를 쓴다
     (`warn peer <이름> (<in|out|both>): <이유 줄>`, 예: `warn peer win1 (out): rejected by the peer (403 peer not allowed); on win1 run herdvia pending, then herdvia accept. Retrying in 12s.`).
     `status-detail`을 모르는 에이전트이거나 두 요청 사이에 목록이 바뀌어 그 피어가 답에 없으면 위의 예전 문구다.
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

## 도움말: --help

- `herdvia --help`, `herdvia -h`, `herdvia help`는 표준 출력에 짧은 설명 문단, `usage: herdvia <command> [<arguments>]`, `commands:` 아래 명령마다 한 줄,
  `Run herdvia <command> --help for a command's usage and an example.`을 쓰고 0으로 끝난다.
- `herdvia <명령> --help`(또는 `-h`)는 공개 명령(`agent`, `listen`, `connect`, `register`, `peers`, `id`, `allow`, `revoke`, `status`, `pending`, `accept`, `doctor`)마다
  `usage: herdvia ...` 줄, 짧은 설명, `example: herdvia ...` 줄을 표준 출력에 쓰고 0으로 끝난다. usage 줄은 인자 오류 때 쓰는 usage 줄의 해당 부분과 같다.
  `agent`의 usage는 macOS·Linux에서 `agent [install|uninstall|reload]`, Windows에서 `agent [install|uninstall|reload|--log]`이고, Windows 도움말만 `--log`를 설명한다.
  `via --help`는 `Internal: hc runs this for machines added with --via. It is not meant to be run by hand.`만 쓴다.
- 고정된 글만 쓰므로 데이터 폴더, `secret.key`, 에이전트 어느 것도 건드리지 않고, `HERDVIA_DIR`이 없는 폴더를 가리켜도 아무것도 만들지 않는다. root 거부는 도움말보다 먼저다.
- 도움말 형태는 인자 해석에서 가장 먼저 본다. `--help`와 `-h`는 이름 규칙(`[A-Za-z0-9._-]{1,32}`)에 맞는 피어 이름이기도 해서, 예전에는 `listen --help`가 `--help`라는 이름으로 5분 동안 기다렸다.
  도움말로 보는 것은 인자가 하나뿐일 때(`--help`, `-h`, `help`)와, 알려진 명령 바로 뒤에 `--help`나 `-h` 하나만 올 때다.
  그 밖의 자리는 도움말이 아니다. `connect <이름> --help`는 초대 문자열이 `--help`인 `connect`이고, `revoke home --help`, `agent install --help`, `help listen`은 인자 오류다.
- 그래서 `revoke --help`, `register --help`는 도움말이 되어, `--help`나 `-h`라는 이름의 피어는 그렇게 지우거나 등록할 수 없다. 피어를 저장하는 명령은 이 둘을 이름으로 받지 않는다.
  0.1.4 이하는 `allow --help <EndpointId> --inbound`처럼 이 이름으로 저장했고 `peers`는 지금도 그 항목을 읽으므로, `revoke`만 `revoke -- <이름>`을 받는다.
  `--`는 뒤가 이름이라는 표시일 뿐이라 올바른 이름이면 무엇이든 그대로 지운다(`herdvia revoke -- --help`). 다른 명령은 `--`를 받지 않는다. `revoke` 도움말이 이를 한 문장으로 안내한다.
  `connect --help <초대>`, `allow --help <EndpointId> ...`, `accept --help <지문> ...`(`-h`도 같다)은 `--help and -h cannot be peer names`로 거부한다(255).
  `listen`이 상대 호스트명을 기본 이름으로 쓸 때는 접미사를 붙인다(위 "흐름과 종료 코드" 6).
- 인자 오류는 예전처럼 usage 줄을 stderr에 쓰고 255로 끝난다. 그 줄은 새 명령을 넣어
  `usage: herdvia agent [install|uninstall|reload] | listen [<name>] | connect <name> [<invite>] | register <name> | via <peer> <line> | id | allow <name> <EndpointId> (--inbound|--outbound|--both) | revoke <name> | peers | status | pending | accept <name> <fingerprint> (--inbound|--both) | doctor | --version | --help`이다
  (Windows는 `agent [install|uninstall|reload|--log]`).

## 단계

- M1 (완료): 가짜 via로 hc 동작 실측.
- M2 (완료): 네트워크 없는 루프백 통로. macOS.
- M3: iroh 연결, 피어 키 허용 목록, 방향별 권한.
- M4: macOS 자동 시작. M5: Windows. M6: 4대 실사용.
