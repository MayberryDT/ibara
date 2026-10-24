# 0.3.2 maintenance release

Quiet login declarations, automatic journal recovery and clearer recording diagnostics. Requires Omarchy on x86_64; no Ubuntu, Windows, macOS or experimental input-driver support is added.

Use the existing browser session and verify its account before requesting assistance. Assistance is not authentication. Recording is local and content-free; uploads remain explicitly configured and opt-in. A crash, unavailable disk or pre-dispatch protocol refusal can leave recording gaps. No new full benchmark or reliability-rate claim is made.

All three packages must be version0.3.2-1, with exact sibling dependency pins. Preserve the white-console fix from0.3.1-3. Update through the signed installer or `ibara update`; verify the effective executable, because a private service drop-in may still select an older controller. Review and archive the specific development override before activating the package; do not remove unrelated user configuration.

Preserve browser profiles, identities, grants, journals and pending login intents during update or rollback. Existing explicit metadata caps stay hard unless a growth ceiling is configured. Fresh defaults are1GiB with bounded growth to4GiB while keeping disk reserve. Do not restore an old journal snapshot to roll software back. The browser extension updates at its next check/restart, and older reader evidence can refuse assistance safely until the new reader is active.

Release readiness evidence and exact package hashes accompany the prepared candidate. Public publishing is a separate final action.
