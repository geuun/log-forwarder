# log-forwarder

Loki에서 `forward_logs="true"` 스트림(컨테이너 라벨 `monitoring.forward-logs: "true"`)의 새 로그 줄을 주기적으로 가져와 Discord 웹훅 카드(embed)로 보냅니다. 홈서버 모니터링 스택(`~/dev/infra/monitoring`)의 일부로, 그 compose가 이 디렉터리를 빌드합니다.

- logfmt 줄은 `msg`를 제목, 나머지 키를 필드로 만들고, 아니면 코드 블록으로 보냅니다. 색은 `level` 라벨을 따릅니다.
- 채널: 스트림 라벨 `channel`(컨테이너 라벨 `monitoring.channel`)이 있으면 `DISCORD_WEBHOOK_URL_<CHANNEL>`, 없으면 `DISCORD_WEBHOOK_URL`.
- 시작 시점 이후의 로그만 보냅니다. 전송에 실패한 줄은 다시 보내지 않습니다(429는 최대 3번 재시도).

| 환경변수 | 기본값 | 설명 |
|---|---|---|
| `LOKI_URL` | `http://loki:3100` | Loki 주소 |
| `POLL_INTERVAL` | `15s` | 조회 간격 |
| `DISCORD_WEBHOOK_URL` / `DISCORD_WEBHOOK_URL_<NAME>` | | 웹훅 |

## 개발

```sh
cargo test
cd ~/dev/infra/monitoring && docker compose up -d --build log-forwarder
```
