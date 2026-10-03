# Failure Cases Written Before Implementation

Wire: truncated prefixes/payloads; oversized length; malformed JSON; wrong first message; wrong video magic/version/reserved bits; empty/oversized access unit; sequence overflow; duplicate/out-of-order video; gap followed by a dependent frame; excessive events; partial stream reset.

Admission: fenced startup; zero/stale/repeated generation; stale generation ticket; malformed or expired ticket; client mismatch; leaked matching ticket must be invalidated; unknown ticket must not burn valid tickets; revoke must fence before closing connections and releasing keys; same identity/ticket may reattach until expiry; four-viewer limit; queued input must not survive revoke/open.

Turns: view-only input; motion must not request a turn; first meaningful event held; newest motion delivered before held event; second identity must not steal pending/current turn; turn false/settle/revoke clears held and pending events; delivery failures still consume sequences; duplicate batches do not repeat; gaps do not deliver; disconnect mid-key releases all keys; failed release does not report settled.

Identity: partial key/cert pair; corrupt files; unsafe permissions; concurrent creation; wrong server pin; handshake without client cert; certificate possession must be verified by TLS signatures.

## Software Fallback E2E Cases

A missing VA display/profile must select OpenH264, not crash. Linear RGB dmabuf
allocation may be rejected; report that as a startup failure for core fallback.
CPU mapping must honor pitch/offset and DMA synchronization, including cleanup
on error. Scaling must stay within 1280×720, preserve aspect ratio, use even
sizes, and run no faster than 15 fps. Input coordinates must still span the
captured output. Forced IDR must resend SPS/PPS after attach and loss. Validate
by forcing this path on an Intel sender and decoding its actual streamed frames.

Follow-up input ownership: a view-only follower disconnect must not release the
active person's keys; release_all before a delayed turn grant must discard
queued clicks/keys, so keyboard ungrab cannot inject them later. Exercise these
through real multi-peer control runs, rather than assertions mirroring code.

Geometry: a larger output must remain bounded to 1080p without cropping its
edges; RGB imports use the source dimensions and VPP output uses the encoded
dimensions. Odd, zero, or unreasonably large output sizes must fail safely.

Capture orientation: wlr YInvert must be corrected before encode, in GPU VPP
or software row sampling. Unsupported compositor transforms must fail explicitly
rather than showing an inverted or rotated screen with incorrect input mapping.
