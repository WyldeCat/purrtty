# Telegram 메시지 누락 디버깅

## 현상
- 텔레그램에서 1~37까지 순차 전송
- Claude 세션에서 수신된 것: **15, 16, 17, 18, 19, 35, 36, 37** (8/37, 78% 드랍)
- Claude가 tool call 처리 중일 때 특히 누락 심함

## 플러그인 구조
- 경로: `~/.claude/plugins/cache/claude-plugins-official/telegram/0.0.4/server.ts`
- grammy 봇 → `handleInbound()` → `mcp.notification()` → Claude 세션

## 분석
1. **텔레그램 → grammy**: getUpdates 기반, 텔레그램 서버가 보장. 누락 가능성 낮음.
2. **grammy → mcp.notification()**: 원래 `void` (fire-and-forget, await 안 함). 빠른 메시지가 동시에 발사되면 Claude Code 쪽에서 드랍 가능.
3. **mcp.notification() → Claude 세션**: MCP notification은 unix socket으로 전달. Claude Code 런타임이 바쁠 때(tool call 처리 중) 수신 못 할 가능성.

## 수정 내용 (server.ts line 925 부근)
- `void mcp.notification(...)` → `await mcp.notification(...)`
- 성공/실패 로깅 추가: `[SEND]`, `[OK]`, `[FAIL]`
- 순차 전달로 변경해서 동시 발사 드랍 방지

## stderr 로그
- MCP 서버 stderr은 unix socket으로 Claude Code 프로세스에 연결
- 외부에서 직접 확인 불가
- Claude Code 내부에서 처리됨

## 재테스트 결과 (2026-04-10)
- `void` → `await` 변경 후 1~37 재전송
- 수신: **15, 16, 17** (이전과 동일 패턴, 1~14 드랍)
- `[SEND]`/`[OK]`/`[FAIL]` 로그는 MCP stderr이 Claude Code 내부 unix socket으로 연결되어 외부 확인 불가
- **결론**: grammy → `mcp.notification()` 구간 문제 아님. Claude Code 런타임이 바쁠 때 MCP notification을 드랍하는 것으로 판단.

## 파일 로깅 추가 (2026-04-10)
- MCP stderr은 Claude Code 내부 unix socket이라 외부 확인 불가 → 파일 로깅으로 전환
- 로그 경로: `~/.claude/channels/telegram/telegram-debug.log`
- `appendFileSync`로 직접 파일에 기록

### 로그 포인트
| 태그 | 위치 | 확인하는 것 |
|------|------|------------|
| `[BOT.START]` | bot.start onStart | 봇 초기화 시점 |
| `[BOT.START] 409` | 409 retry | 다른 인스턴스 충돌 |
| `[BOT.ON:text]` | bot.on('message:text') | grammy가 핸들러 호출하는 순간 |
| `[HANDLE_INBOUND]` | handleInbound 진입 | 함수 진입 + gate 결과 |
| `[GATE]` | gate() 내부 | 접근 제어 판단 과정 |
| `[SEND]` / `[OK]` / `[FAIL]` | mcp.notification | MCP 전달 |
| `[BOT.CATCH]` | bot.catch | 핸들러 에러 |

### 1차 파일 로그 결과
- 1~10 전송, 로그에 5~10만 기록 (msg_id=485~490)
- **1~4는 `[BOT.ON:text]` 로그조차 없음** → grammy 핸들러 자체가 호출 안 됨
- grammy → handleInbound 구간이 아니라, 텔레그램 서버 → grammy(getUpdates) 구간에서 누락 가능성

## 409 Conflict 무한 루프 문제 (2026-04-10)

### 현상
- 세션 재시작 후 봇이 `[BOT.START] polling as @wyldecatbot` → `409 Conflict` 무한 반복
- attempt 20+ 까지 진행, 수 분간 지속
- 이전 세션 프로세스는 확인되지 않음 (ps aux로 확인)
- 현재 세션의 bun MCP 서버 프로세스 1개만 존재

### 시도한 것
| 시도 | 결과 |
|------|------|
| 이전 세션 종료 | 409 지속 |
| `deleteWebhook` API 호출 | "Webhook is already deleted" — 효과 없음 |
| `getUpdates?timeout=0&limit=1` 호출 | 성공 반환, 409 지속 |
| bun 프로세스 kill (PID 83636) | 재시작 후에도 409 지속 |
| `close` API 호출 | 성공, MCP 서버 disconnect됨 |

### 분석
- 이전 세션의 long-polling 연결이 텔레그램 서버 쪽에서 타임아웃 안 됨
- 또는 grammy의 `bot.start()` retry 루프 자체가 자기 충돌 발생 가능성
- `close` API 호출 후 MCP 서버가 죽었지만 자동 재시작 안 됨
- **세션 자체를 재시작해야 해결될 것으로 판단**

### 수정 내용 (server.ts line 975 부근)
- `bot.start()` 전에 cleanup 단계 추가:
  - `deleteWebhook({ drop_pending_updates: false })` 호출
  - `getUpdates({ offset: -1, limit: 1, timeout: 0 })` 호출로 이전 long-poll 슬롯 해제
- retry delay: `Math.min(1000 * attempt, 15000)` → **고정 35초** (텔레그램 long-poll 타임아웃 ~30초보다 길게)
- 무한 루프 → **최대 5회**로 제한, 초과 시 에러 메시지 출력 후 종료
- `[BOT.PRE]` 로그 태그 추가 (cleanup 단계 추적용)

### 다음 단계
- [ ] 세션 재시작 후 409 없이 봇 polling 성공하는지 확인
- [ ] 성공 시 1~50 숫자 테스트 진행
- [ ] `[BOT.ON:text]` 로그 없는 메시지 → getUpdates 누락인지 확인
- [ ] Claude Code 런타임 쪽 드랍 우회 방안 탐색 (큐잉, 재전송 등)

## TODO (이전)
- [x] 세션 재시작 후 1~37 재테스트
- [x] await 변경으로 드랍률 개선되는지 확인 → 개선 안 됨
- [x] 파일 로깅 추가 (stderr → appendFileSync)
