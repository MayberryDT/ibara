---
name: ibara
description: Use a real computer your person owns through ibara's MCP tools (server `ibara`) — its desktop apps, its signed-in browser, its screen and its files. Use when a task needs real input, a real browser session, a visual check or work on another computer; not for code, git, tests or pages you can fetch where you are.
---

# ibara

ibara lets you use computers your person owns: their desktop, apps and signed-in browser, with real mouse and keyboard input, while they watch from the ibara console. Every tool reply starts with a situation line and says what to do next. This skill covers what the replies don't: when to use ibara and how to work so the result can be trusted.

## When to use it

Use ibara when the task needs a real screen:
- trying a web page or desktop app the way a person would, and reporting what worked;
- visual checks, and work in a browser where your person is signed in;
- running something on another computer and seeing the result;
- getting a file from that computer, or putting one there.

Keep ordinary code, git, tests, APIs and public retrieval on the normal build/research host. When a task specifically needs another computer's environment—for example an Omarchy build plus GUI verification—use managed commands in that computer's Ibara task. Do not acquire a desktop and then run an untracked SSH build.

`computer_status` lists your computers. **For ordinary work, omit `computer` in `computer_begin`: Ibara selects any ready computer and tries another after a definite acquisition refusal.** Never default to one particular computer or ask which machine merely because several are available. Name a computer only when your person names it, a task requires state/apps on it, or a benchmark fixes that target. Verify the returned ID before effects; keep that task on its assigned machine. A named route stays pinned.

You may use **multiple computers for separate independent tasks** in the same agent session. Give each begin a different request_id, keep each task_ref with its computer, and finish each promptly. Do not move an uncertain operation, partial registration or started benchmark to another machine. If an older client requires a name, inspect fleet status and try any eligible ready target; a definite BUSY/HUMAN_CONTROL refusal means another free machine can do ordinary unstarted work. Use a fresh request_id when starting that separate attempt.

## One task, start to finish

A task holds the computer: your person sees it in their console, and no other agent can use that computer until you finish. A busy computer does not block work on other free computers. If all suitable machines are unavailable, continue independent work and report the actual states. Never steal a lease or override a person’s pause.

1. **Prepare before you begin.** Have the URLs, text and your definition of done ready before `computer_begin`.
2. **Begin with checks that prove the result.** Give `computer_begin` a `goal` and `checks`. Prefer typed checks (`file_exists`, `file_content`, `url`, `text_present`, `element`, `delivered`); ibara proves those itself. Checks without a type are yours to assess honestly at finish, and so is a typed check ibara can't read: text in a terminal, which has no accessibility tree, is `unknown` until your assessment decides it. The reply has your `task_ref` and the first frame.
   - Every call that acts takes a `request_id`. Use a new one for each new request in this session. To retry a request, send it again with the same `request_id` and the same arguments, even from a new session: ibara returns the first result and does not do it twice.
3. **Act from the frame.**
   - Pick element refs (`e12`) and choices (`c3`) from the latest frame instead of coordinates.
   - Open apps with a `launch` action (`editor`, `terminal`, `browser`, `files`) rather than hunting for icons.
   - Batch up to 8 steps with an `expect` on each, so ibara stops at the first surprise.
   - Need to see more? Ask for the cheapest view that answers the question: `computer_observe` `situation`, then `elements` with a `query`, then an `image` of one surface. Use `screen` last.
   - Click a point `{x, y}` only on a picture you just took with `computer_observe` (`view: image` or `screen`), in that picture's pixels.
   - In the signed-in browser, use `browser_act`: observe `surface: "tab"`, `view: "elements"` first for page ids like `b3`.
4. **Read text from elements, not pictures.** Use `computer_observe` with `view: "elements"` and a `query`. An agent that read a code off a picture typed M for N.
5. **Verify every step.** `done` means ibara sent the input, not that it had the intended effect. Confirm with `expect` or a fresh observation. When ibara can't prove an outcome it says `unknown`; treat that as unproven.
6. **Observe again after anything changes the screen,** such as navigation, a dialog or a new window. Refs from an old frame go stale (`STALE_TARGET`).
7. **Finish at the first safe point.** Call `computer_finish` with `complete` only when the checks are met. Otherwise use `partial` or `blocked`, with a summary that says what is and isn't done. Tell your person what the checks proved, not only that you finished. Finish when work on that computer is settled; finish cancels remaining managed jobs. Keep its task through dependent build and desktop phases. Do independent planning/research elsewhere; do not reserve an idle computer for later work.
   - Save required files first and read `cleanup`, `complete` and `notes`. A computer labeled **disposable desktop** must reach zero application windows between tasks, regardless of who opened them; finish and crash recovery enforce this. The mode preserves saved files and profile storage, not windows. Close current windows with `{kind:"close",surface:"wN"}`; observations show tile dimensions. On cleanup-capable targets, known task-owned Mousepad save prompts are resolved automatically and unsaved task edits discarded; saved files stay unchanged. Save required work before finish. Known task-owned native file choosers are cancelled before browser cleanup; this does not select or save a file. Human-touched or unidentified windows and unfamiliar dialogs remain protected; a refused reset never permits force-killing the app. Personal computers retain their conservative ownership rules.
   - Background connection pings do not count as work on disposable desktops. Use `computer_wait` for a bounded operation or approval wait; finish instead of keeping the machine while planning.

## Commands, builds and reconnection

On work-capable targets, one `task_ref` owns the workspace, supervised commands and desktop. The initial policy is one exclusive task per computer; separate tasks can use different computers. Different directories alone do not isolate builds.

Use `computer_exec` with the task reference for remote builds/tests, then the same task for native verification. Keep command output/result files in its workspace. `computer_status({ref:op_ref})` reads job state/output even after the task ends. `done` input and exit0 are not independent GUI or saved-output proof.

The equivalent CLI is `ibara work [--computer NAME] [--agent NAME] run --goal TEXT --request-id ID -- PROGRAM ARG...`, then `exec --task TASK --request-id ID -- PROGRAM ARG...`, `status REF`, or `cancel TASK`. Use the same agent name as the owning MCP client when mixing routes. Preserve the printed request, task and receipt IDs; a lost reply is reconciled with those IDs, never a replacement launch. Same-ID `run` inspects the existing work. Explicit `finish`/`cancel` cancels remaining jobs and cleans windows.

A disconnected agent is not a cancelled task. Managed jobs keep ownership; the same authenticated agent can reconnect without replaying effects. A live second session cannot steal it. Once jobs settle and the disconnected task's grace expires, cleanup releases the computer and retains readable results. A killed relay is detected after three missed30s heartbeats; this is liveness detection, not a job runtime limit. Human takeover fences input and cancels managed jobs; old approvals do not resume. A daemon restart may stop work and leaves uncertain effects for inspection.

Administrative SSH and detached external services are outside this supervision. Do not describe them as reserved/isolated work. Old targets without structured work details or the CLI subcommand are unsupported for this workflow; retain the normal build host instead of silently falling back to an untracked SSH job.

## Approvals and people

- **A `pending` reply with an `att_` ref means a person must approve** before the step runs.
  - Call `computer_wait({task_ref, for: {attention: "att_…"}, deadline_ms})` until it is answered.
  - Then send the same request again with the same `request_id`; it runs once if approved.
  - A new `request_id` is a new request and asks again. Agents that resent under a new id, or finished while waiting, failed their tasks.
  - The person answers in ibara, so don't end your turn or ask in chat.
- **Declare `effect`** (`send`, `spend`, `destructive`) on steps that send, pay or delete. ibara also recognizes form submits.
- **Your person may Take Control at any time.** Your input is then refused and the reply says so. Wait for them to hand back, or finish `partial`. Never work around them.
- **Decisions you can't make:** use `computer_checkpoint` with `ask`. It holds a question for your person in ibara.
- **Continuation notes:** use `computer_checkpoint` with `note` before a long pause, so you or another session can continue.
- **`stop_asking: true`** only when your person told you that you don't need to ask before you send, spend or delete. ibara asks them once.

## Signing in to websites

Each computer keeps its own browser session. Use an existing approved session and verify the expected account. Google/Gmail cookie copying is disabled. Eligible other sites may offer Share sign-in; once is the default, and remembered sharing is an explicit choice. Never expose credentials in tool text or receipts.

- **Name the sites you know you'll need when you begin:** `computer_begin({…, logins: ["irs.gov", "id.me"]})`. Your person answers once for all of them, and you can start working meanwhile. The reply gives each site's `state`. For `rejected_before`, plan for your person to sign in with Take Control.
- **At a sign-in page, use `browser_act` with `{kind: "sign_in"}`** before anything else. It covers the site the tab shows and the site it came from. The reply's `page` is `left_sign_in` when the page is past its sign-in form. `unknown` is not success, so check the page.
- **Every other outcome has a reason code with a `next`.** Follow it:
  - `waiting_for_person`, `waiting_for_browser` or `waiting_for_sharing_computer`: wait as `next` says, then resend the same request.
  - `declined` or `denied`: don't ask again for that site. Take another route, or ask with `computer_checkpoint`.
  - `site_rejected`: the site didn't accept the shared login. Ask your person to sign in with Take Control.
- **On assistance-capable computers, read the actual human answer.** `deferred` means do independent work, checkpoint and finish partial when blocked; do not repeatedly ask or leave windows open. `no_account` means check a legitimate guest/public route or prepare a concrete signup proposal; it authorizes no creation. `without_account` means use the guest route. `person_sign_in` means respect human control and verify the expected account after handback. `cancelled` stops this sign-in.
- **Continue after cleanup:** keep the `att_` reference. `computer_status({ref: "att_…"})` exposes the durable answer. Acquire the same computer with a fresh task and call `computer_checkpoint({task_ref, login:{attention:"att_…",action:"continue"}})`. This restores intent, never old input approval or a cancelled task. A late answer does not start a disconnected agent.
- **When a new signup decision is needed:** `computer_checkpoint({task_ref,login:{attention,action:"propose_signup",proposal:{site,identity,credential_store,cost:"free",summary}}})`. Use exact approved owner/email and storage labels, registration data and verification requirements in the summary; no secrets. The same card displays the proposal. After approval, `claim_signup` with `site` records one attempt before submission. If exact setup is already authorized in the task, do not ask for redundant approval; use that existing authority and keep equivalent non-secret evidence through normal checkpoints.
- **Never retry uncertain registration.** `record_created` marks a claimed setup for verification, not completion. `resolve` with `site` and `evidence_ref` records verified task access, account identity and recoverable storage, or a verified guest route. After cancellation/process loss, use `recover_signup` with the `claim_ref` from `prior_setups` on the same computer. `record_not_created` requires evidence that submission had no effect; absence of confirmation is insufficient. Unknown stays blocked. This ledger is per computer: keep a setup attempt pinned there until reconciled; do not re-register on another computer.
- **Provider not configured:** have the person join for secure setup. Do not invent a vault integration or create an account with credentials only the agent can recover. Keep account/site substitutions, paid plans and added identity scopes within explicit authority.
- **Each site gets one try per task.**
- **Never ask your person for a password or a code in chat.** A code the site texts or emails goes through `computer_checkpoint` with `ask`.
- `computer_status` shows each computer's logins ("logins from Laptop · 23 sites allowed"). Prefer a computer that already has the sites you need.

## When something goes wrong

- **Every error names the cause and gives `next`.** Do what `next` says.
  - `computer_status({ref: "help:<tool>"})` gives a tool's full shape and an example.
  - `computer_status({ref})` gives the state of any `task_`, `op_` or `att_` ref.
- **Resending with the same `request_id` never repeats an effect,** so it is safe after a timeout.
- **A refusal is information, not an obstacle.** Don't drive the computer another way: no `ydotool`, `wtype` or `xdotool`, and no SSH to click around. Don't search your person's files for ibara's internals.
- **Stuck in a particular app?** `computer_procedures` with `op: "search"` returns notes about that app, such as how its Save As dialog behaves.

## Files

- **On that computer:** `computer_files` reads, writes and lists files in the task's workspace and the home folder. `computer_exec` runs managed command argv in the task workspace, with no implicit shell. On work-capable targets, `background:true` returns an operation reference immediately; jobs keep the computer occupied through client loss. `timeout_ms` is an explicit runtime deadline, not the response wait. An explicit host maximum is enforced and reported; no default action/image quota or build deadline is implied.
- **Bringing a file to your own computer takes two steps.**
  - `send` records the delivery but moves no bytes.
  - Then run the fetch command its `next` gives (`ibara client … fetch …`) on your computer.
  - Agents that stopped after `send` never delivered the file.

## What leaves the computer

Screens and text you read go to your model provider. Read what the task needs: an element query or a cropped image, not the whole screen.

On a designated disposable desktop, finish/reset may visibly create a blank browser tab and close transient tabs using native input before closing windows. A failed retirement leaves the computer unavailable with recovery guidance; do not bypass it or force-kill the browser. Save/deliver needed files before finish.
