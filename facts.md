---
facts: 1
name: ibara
source:
  kind: dir
  path: ~/.local/state/agent-computer/facts
environment: private
fresh_hours: 24
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
  poll_totals: SELECT build, data->>'$.fields.op' AS op, sum(coalesce((data->>'$.fields.samples')::BIGINT,1)) AS reads FROM facts WHERE kind='operator' AND (data->>'$.fields.op') IN ('login_pending','attention') GROUP BY ALL
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

Recording captures operational categories and outcomes, not desktop contents. Screens, keystrokes, clipboard, file bytes, task goals, names, URLs, paths, credentials and raw arguments/results never enter the facts archive. The separate private operational journal keeps detailed receipts under its own retention rules.

This contract reads the local spool. Copy it to your analysis/build host and set `source.path` to an exported spool, or configure your own authenticated HTTP archive. Run `facts sync`, `facts check`, `facts session SESSION` and `facts query 'SELECT * FROM call_outcomes'`. Keep a large mirror on a build host with disk space. The reference archive service is in `facts-service/`; use separate read/write credentials. No upload is enabled by installation, and no credential is bundled.

`bot` means an MCP agent; `agent` means a test actor enabled with `IBARA_FACTS_TEST_AGENT=1`. Operator calls are `human` and internal transitions/admin calls are `system`. These are transport origins, not verified human identities. Active-task calls share a session until finish; taskless calls use their connection. Session/actor bookkeeping is bounded, can restart old episode UUIDs, and resets on recorder restart. Session spans include idle time and are not activity exposure. Do not infer production adoption or human-versus-agent rates from test records or session duration. Declare appropriately scoped eras before comparing builds.

A bounded1024item queue feeds the disk writer. Complete pending JSONL lines recover on startup; queued events can be lost on abrupt death. Flush/fsync happens on task finish, shutdown,2MiB or5minutes. A64MiB spool plus one in-flight chunk bounds disk use. Full/unavailable spool drops facts and reports loss best effort. Authentication/protocol refusals before dispatch are outside recording. No completeness claim is valid for crash or unavailable-disk intervals.

Successful background `login_pending` and `attention` reads become counted minute summaries with `fields.samples` and `duration_ms_total`; failures remain individual. Use `poll_totals`, not raw summary event counts or per-read latency assumptions. Normal shutdown flushes the final partial summary; abrupt death can also lose that in-memory partial minute. No idle snapshots are emitted.

Agent/operator/admin status exposes `recording`: enabled/test mode, cumulative and unreported losses, spool use, last append, upload-helper attempt age/state and pending poll samples. A zero-object helper run establishes a successful check, not new archived activity. `fresh_hours:24` independently warns when the newest ingested fact is old.

Upload explicitly with `python3 scripts/facts-upload.py --dir STATE/facts --url YOUR_ARCHIVE --token-file PRIVATE_WRITE_TOKEN`. Provision a mode600write token, configure the upload service URL and directory, then opt into its five-minute user timer. Failed objects remain; successful objects are removed only after a matching hash acknowledgment. Upload receipts report bounded HTTP/receipt/IO failures. Immutable archive keys are never overwritten, and no automatic archive deletion is configured. Preserve historical test labels and loss records.
