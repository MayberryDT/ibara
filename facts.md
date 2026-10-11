---
facts: 1
name: ibara
source:
  kind: http
  list: https://ibara-facts.mayberrydt.workers.dev/v1/archive?prefix=raw/v1/
  get: https://ibara-facts.mayberrydt.workers.dev/v1/archive/{key}
  token_file: ~/.config/ibara/facts/read.token
environment: private
admin_kinds: [admin]
kinds:
  call: A production agent or test agent tool call, its status and elapsed milliseconds; unknown tools are redacted.
  operator: A human operator call, its operation category, status and elapsed milliseconds.
  admin: A local administrator call; the associated session is excluded from ordinary analysis.
  transfer: An agent transfer request and whether it succeeded.
  transition: An internal task, step, ownership, cleanup or login state transition, without its text or data.
  recorder.loss: Count of facts dropped by bounded queue or unavailable spool; shutdown-only losses also appear in daemon stderr.
checks:
  unattributed_build:
    about: Recording builds must have deployment-stamped source identity.
    level: fail
    sql: SELECT build FROM facts_all WHERE build = 'unattributed-development' GROUP BY build
  recorder_loss:
    about: A recording loss prevents claims of complete coverage.
    level: fail
    sql: SELECT session, t, data FROM facts_all WHERE kind = 'recorder.loss'
  content_fields:
    about: Facts must never contain request or response bodies.
    level: fail
    sql: SELECT session, t FROM facts_all WHERE (data->>'$.text') IS NOT NULL OR (data->>'$.url') IS NOT NULL OR (data->>'$.path') IS NOT NULL OR (data->>'$.summary') IS NOT NULL
views:
  call_outcomes: SELECT build, who, data->>'$.fields.tool' AS tool, data->>'$.fields.status' AS status, count(*) AS calls FROM facts GROUP BY ALL
budget:
  archive_mb_per_day: 100
minimums:
  events: 20
  sessions: 5
  exposure_hours: 0.5
eras:
  - id: recording-v1
    from: '2026-10-11T00:00:00Z'
    environment: private
    change: First content-free recorder; no historical backfill.
---

# Ibara facts

Run `facts`, `facts sync`, `facts check`, `facts session SESSION`, or `facts query 'SELECT * FROM call_outcomes'` on Halla. Set `FACTS_CACHE=/home/halla/build/ibara-record-everything/cache` so the mirror stays on the build host. The private read token is provisioned on Halla; neither token belongs in source or evidence.

Recording captures operational categories and outcomes, not desktop contents. Screens, keystrokes, clipboard, file bytes, task goals, names, URLs, paths, credential material and raw arguments/results never enter the archive. Existing private journal/receipts still hold the detailed operational evidence under their own retention rules.

`bot` means a production MCP agent; `agent` means a test actor, enabled on the target with `IBARA_FACTS_TEST_AGENT=1`. Human operator activity is `human`; autonomous transitions and administrator operations are `system`. These are transport origins, not a claim that the caller is a verified human. Ibara has different human and MCP interfaces; compare outcomes within the interface rather than treating them as identical motor paths. Transitions have a system actor and do not identify their initiator.

Active task calls share a session while taskless calls use the transport connection. Successful finish retires its session; later history reads start a new episode. A 1,024-entry LRU preserves the active lease session; other old episodes may restart with a new UUID. A 256-actor cap similarly starts a new UUID before allocating another actor, so numbers are never reused inside a session. Human taskless operations share one operator session per recorder epoch; administrator taskless calls have a separate session. Opaque session UUIDs and actor numbers stay stable across reconnect within a recorder epoch; restarting starts new session UUIDs. Recovered pre-crash events retain their original UUIDs. Session spans include idle time: they are not exposure hours. No human-versus-agent rate comparison is valid until an actual activity exposure measure is added.

A bounded 1,024-item queue feeds a disk writer. Up to the queued facts can be lost on abrupt process death; pending complete JSONL lines already written survive process death. Power-loss durability is established at flush by fsync, not on every event. Flush on task finish, shutdown, 2 MiB or five minutes; no idle facts or periodic snapshots. A 64 MiB spool budget plus one in-flight chunk bounds disk use; full/unavailable spool drops facts and reports counts. Loss reporting is best effort if the disk remains unavailable. No completeness claim is valid for a crash or unavailable disk interval. Rejected requests before controller dispatch (authentication, JSON-RPC validation and unknown tools) are outside this recorder. `.pending` files recover on startup; the uploader sends only finished gzip objects.

Each immutable gzip object contains metadata `{schema, lines, from, to, sha256}` in R2. Upload retries compare the hash and never replace history. Failed uploads retain their local objects. The archive has no automatic deletion and no per-event database writes. Upload the spool with `python3 scripts/facts-upload.py --dir STATE/facts --url https://ibara-facts.mayberrydt.workers.dev --token-file ~/.config/ibara/facts/write.token`. Schedule it every five minutes on an opted-in target; read and write credentials are separate. `IBARA_FACTS_DISABLED=1` opts out; no upload happens merely by installing the recorder.

The first era is private acceptance only. Do not infer production adoption, reliability, human activity or actual provider cost from it. The budget is a local archive-volume check; provider billing/analytics is the cost authority. Public deployment and each new recording era require a stamped clean candidate plus its own receipt.
