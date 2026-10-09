# Runtime notes

Operational detail for `gray-ntfy`: how a message becomes a turn, how a
reply lands, and what is deliberately absent.

## Transport

- Outbound: `POST {server}/` with a JSON body
  `{"topic", "message", "title"?, "priority"?, "tags", "markdown"?}` —
  the JSON publish endpoint, so Unicode titles are safe. Every publish
  carries the `gray-agent` tag so inbound echo is identifiable.
- Inbound: `GET {server}/{topic}/json?poll=1` — a long-lived NDJSON
  stream. On reconnect the daemon resumes from the stored `since` cursor
  (5s overlap; message ids dedupe) so messages published while it was
  down still arrive, capped by the server's cache window (~12h).
- Reconnects back off 1s → 60s. HTTP 401/404 are fatal — the daemon logs
  and stops instead of retrying bad auth forever. 90s without any stream
  bytes (keepalives land ~55s) forces a reconnect.

## Inbound policy

- A message turns when its title or one of its tags equals an `allow`
  token (case-insensitive; default `gray`). `allow_all: true` admits
  everything — dev only.
- Events tagged `gray-agent` (our own publishes) are always dropped.
- There is no pairing flow: ntfy messages carry no authenticated sender,
  so there is nothing to approve a code against. Non-admitted messages
  are silently ignored.

## Queue

- Admitted messages are written to `ntfy.sqlite` beside the config
  (`queued` → `running` → `done`/`failed`) before any turn starts, so a
  daemon restart never loses input; `recover` re-queues interrupted
  items. Capacity defaults to 1000 (`queue_capacity`).
- `concurrency` workers (default 2) claim items oldest-first; one
  conversation runs one turn at a time via a `run.lock` flock — a second
  item waits (`Busy` → requeue) rather than racing the session pointer.

## Turns and delivery

- Each turn spawns `gray -p <text> --json --max-requests N [--session S]`
  with `GRAY_HOME` pointed at a per-conversation home under the config
  dir; the session id returned is pinned in `session.json` so follow-ups
  continue the same session. The child's plugin profile is
  `tools-minimal` + this sidecar only — nothing that could start another
  gateway.
- `session_reset` (default `{"mode":"idle","idle_minutes":1440}`)
  forgets a conversation after a day idle; `"never"` disables. `/new` or
  `/reset` as the whole body resets on demand.
- Replies publish to `publish_topic` titled `gray`. A publish failure is
  retried five times, then the item is `failed` — the generated answer
  is never regenerated (at-least-once, no duplicate turns).

## Limits

- `ntfy_send` / replies: 4096-char body cap (ntfy's own limit), priority
  1-5, free-form tags. Plain text when `markdown: false`.
- `timeout_seconds` (default 600) hard-kills a turn, process group
  included. `max_requests` (default 200) caps one turn's model calls.

## Files

- `~/.config/gray-ntfy/config.json` — config, mode 0600 (holds the token)
- `~/.config/gray-ntfy/ntfy.sqlite` — durable inbox + stream cursor
- `~/.config/gray-ntfy/conversations/<sha>/` — per-conversation homes
- `~/.config/gray-ntfy/daemon.json` — live pid stamp; `gateway.lock` —
  single-daemon flock; `gray-ntfy.pid`/`.log` under gray supervision
