# Full session runtime directory — failure cases

Written before the repair, 17 November 2026. Incident: an agent computer's `/run/user/1000`
(a 376 MB tmpfs, 10% of RAM) filled with shell logs. `systemd-run --user` then
failed with "Unit … not found", so `omarchy launch browser` exited 1, so the
disposable reset could not reopen the browser to retire its saved session.
ibara stayed `control_unsettled` and relaunched every ~3 s for 4 hours (5,032
resets) without ever naming the cause.

## How the repair could fail

Retry fence

1. The first reset pass fails (launch refused, nothing started); the follow-up
   pass runs with the already-cancelled token and leaves the receipt at
   `resetting`, so reconcile does not see a blocker and relaunches every tick.
2. Fixing (1) by recording `blocked` makes the unchanged-inventory fence refuse
   every later attempt, so the computer never recovers on its own once space
   returns and needs a person, although nothing ever started.
3. Retrying a refused launch is allowed to retry uncertain input or a launch
   whose outcome is unknown (a launch that may have opened a window).
4. A retry-safe blocker is retried by reconcile on every tick instead of on the
   watchdog's backoff (10 s doubling to 5 min). (An agent's begin or finish on
   that computer may also retry it once; each is one quick refusal.)
5. The second pass that retires restore state after an *unreadable* browser was
   closed stops running, breaking the existing recovery for that case.
6. A person's pause or control is overridden by the automatic retry.

Cause and reporting

7. The launch error still says only "service manager did not start … (exit
   status: 1)" and nobody can tell that the runtime directory is full.
8. Status shows no capability for the runtime directory, so a nearly full one
   is invisible until launches fail; or a degraded runtime directory still
   lets the computer be chosen as a warning-only fallback.
9. The check reports degraded on a normal directory (false alarm) or reads the
   wrong directory (the daemon's environment rather than the session's).

Reclaim

10. A live shell's log directory is removed (its lock is held), or one owned by
    another user, or something outside `quickshell/by-id`.
11. A symlink inside an instance directory is followed and its target removed.
12. A dead instance's `by-pid` link is left dangling, or a link to a live
    instance is removed.
13. Reclaim runs while space is fine and deletes logs a person might read with
    `qs log`.
14. Reclaim cannot free enough (live logs, other apps) and the computer still
    reports healthy.
15. Reclaim silently deletes: no timeline/repair event says what was removed.

Found by review

16. A stale `failed`/`retry_safe` from an earlier receipt skips a second pass
    that would have finished, or lets windows that outlived termination be
    signalled again on every backoff (independent review, 17 November).
