# gray-ntfy

A standalone ntfy channel adapter for [gray](https://github.com/vstaln/gray)
— HTTP subscribe stream in, JSON publish out, with its own setup wizard,
title/tag allowlist, durable SQLite queue, and a background service. Same
shape as `gray-discord-plugin`/`gray-irc`, ntfy transport underneath
(behavior ported from hermes `plugins/platforms/ntfy`).

**Status: v1.** Admitted inbound messages run one isolated `gray` turn in
the topic's single conversation; outbound replies publish to the publish
topic. See [runtime policy and limits](docs/RUNTIME.md).

## Requirements

- `gray` on PATH, provider/model configured in `~/.gray/config.json`
- An ntfy server: `https://ntfy.sh` or self-hosted
- A topic. Anyone who knows its name can publish — use a long random one.

## Install

```sh
cargo install --path .          # puts gray-ntfy on PATH
gray plugin install ntfy        # or let gray find the binary
```

## Setup

```sh
gray ntfy setup                 # or: gray gateway setup ntfy
```

The wizard asks for:

- server URL (default `https://ntfy.sh`)
- subscribe topic and publish topic for replies (default: same)
- optional auth token — an ntfy access token, or `user:pass` for Basic
  auth — entered hidden
- markdown bodies (default yes)
- inbound allow tokens (default `gray`)

It saves the config privately (`~/.config/gray-ntfy/config.json`, mode
0600), registers the `ntfy_send` tool with gray, probes the server's
`/v1/health` with your auth (doctor), and offers to install the
background service.

## Running

```sh
gray ntfy status      # service health
gray ntfy stop        # graceful stop
gray ntfy restart
gray ntfy uninstall   # service only; config and sessions stay
gray ntfy run         # foreground daemon (what the service execs)
gray ntfy doctor      # verify server reachability + local gray config
gray ntfy queue list / cancel <id>
gray ntfy allowlist add|remove|list <token>
gray ntfy limits --timeout-seconds 600 --concurrency 2 --max-requests 200
```

The service probes what this box actually supervises with (runit,
`systemd --user`, or a detached pidfile daemon when no init is available).

## Talking to it

- Publish to the subscribe topic with the title `gray` (or a `gray` tag,
  or any token on the allowlist). The body is the prompt.
- Replies land on the publish topic titled `gray`, tagged `gray-agent`.
- `/new` or `/reset` as the whole body starts a fresh session.
- Everything else on the topic is ignored — there is no per-user
  identity to pair. `allow_all: true` in config admits every message
  (dev only).

## Tools (registered with gray)

- `ntfy_send {text, title?, priority?, tags?, topic?}` — markdown body
  capped at 4096 chars; priority `min|low|default|high|urgent` or 1-5;
  tags as array or comma list; `topic` overrides the publish topic.

## Limits (v1)

- Text only: no attachments, actions, or click buttons.
- One conversation per config — a topic is one trusted channel (ntfy has
  no authenticated user identity; titles are publisher-controlled and are
  never trusted for auth). The topic name itself is the secret.
- Delivery is at-least-once: the answer is generated once and retried on
  publish failure rather than regenerated.
- One daemon per config (`gateway.lock`); two daemons would both post.
