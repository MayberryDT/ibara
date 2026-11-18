# Disk retention — failure cases

Written before the change, 17 November 2026. Incident: one computer's 57 GB
disk fell below the 15 GiB warning and stayed there. Of what ibara itself kept,
13 GB was 2,349 task workspaces, the oldest from September, which nothing ever
removed, and 0.8 GB was 21 journal copies made before updates, of which
`ibara rollback` can use at most three.

## Task workspaces

1. Old workspaces are never removed, or are removed only when ibarad starts,
   so a daemon that runs for weeks keeps growing them.
2. A workspace of a task that is still open (active, blocked or any state that
   is not completed, cancelled or partial) is removed.
3. A workspace of a task that ended recently is removed while a person or agent
   may still want to look at what it left.
4. A workspace is removed while a job of its task is running, is in an unknown
   state, or has no confirmed termination.
5. A workspace is removed while its task has an unsettled operation.
6. Without the journal, on a journal error, or for a folder the journal has no
   task for, a workspace is removed.
7. A symlink in the workspaces folder, or inside a workspace, is followed and
   its target removed; or something that is not a task folder is removed.
8. A large backlog stalls status or admission: one pass removes thousands of
   folders, or every status read looks up every kept folder's task again.
9. Below the disk warning, the computer stays degraded for a week while ended
   workspaces wait out the normal window.
10. Published artifacts lose their bytes when their task's workspace goes.

## Journal copies made before updates

11. Every update adds a copy and none is ever removed.
12. Pruning removes the copy `ibara rollback` restores for a release that is
    still kept for going back.
13. Pruning removes the copy the same update just made.
14. Pruning follows a symlink, or removes something that is not a `before-`
    copy.
15. A pruning failure fails the update.

## Found by review

16. One workspace with many files (a `node_modules`, a build folder) is deleted
    on ibarad's only thread, so status and admission wait seconds for it; or a
    read-only folder inside it (a Go module cache) makes removal fail and the
    workspace is retried on every pass.
17. The kept releases cannot be read (an unreadable package cache, a failed
    signature check), so every journal copy but the newest is removed.
18. On a real journal a long task's receipts take about 10 ms to read, so a
    100 ms pass looks at about ten folders. Every pass starting again from the
    first folder spends its time on the same kept ones and never reaches the
    rest; and with passes only on status reads and admissions, an idle
    computer clears a backlog of thousands over days (found on the first
    computer to run 0.3.9).

