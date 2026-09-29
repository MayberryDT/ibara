# Agent tools

ibara gives an agent 11 MCP tools for using a computer: begin a task, look at the screen, act on it, run commands, move files, wait, and finish with an honest account of what happened. This page is the reference for those tools, which together we call contract 4.

## Connecting

An agent connects itself. You copy the prompt from Connect an Agent in the console, or print it with `ibara prompt`, and paste it to the agent. The agent adds `ibara mcp` (stdio, no arguments) to its own MCP settings as a user-level server named `ibara`, links the ibara skill (`/usr/share/ibara/skills/ibara`) into its skills folder, adds a short marked block to its instructions file, and calls `computer_status`. Without `--computer`, `ibara mcp` reaches every computer added to this one, including computers added later. The skill (`skills/ibara/SKILL.md` in this repository) covers when to use the tools and how to work with them; this page is their reference.

`ibara mcp` answers `initialize`, `ping` and `tools/list` itself. It contacts a computer only when the agent calls a tool for it.

## How the tools are designed

- One task at a time per computer. `computer_begin` takes control of the computer for the agent, and `computer_finish` gives it back.
- Every response says where things stand. Each one starts with a situation line, so an agent that reads one response on its own is still oriented.
- The agent chooses from what is on screen. Frames list windows, elements and ranked choices, so the agent picks a reference instead of guessing coordinates.
- Every effect is recorded before it happens, and a request retried with the same `request_id` never repeats it.
- Outcomes are honest. When ibara cannot prove that a step worked, the outcome is `unknown`, never `done`.

The Rust types in `src/contract/` are the source of truth. The JSON Schemas, the tool list and `help:<tool>` are generated from them. This page describes them. There is no version negotiation: every computer and console in a fleet speaks contract 4.

## Shape of every response

Each tool returns one JSON object, the **envelope**:

```json
{
  "situation": "Tulip1 · you (codex@vesper) control · task \"Save note\" 0/1 checks · 0 unknown · 0 attention · r7",
  "status": "ok",
  "since": ["window \"Save As\" opened", "file dogfood.txt created"],
  "result": { },
  "error": null
}
```

**Fields:**

| Field | What it is |
|---|---|
| `situation` | One line of about 200 characters: the computer, who holds control, the task, checks met of total, unknown-outcome count, open attention items and the frame revision. A response read on its own still orients the agent. Through `ibara mcp` on a console, the computer is called by the name its person gave it in that console. |
| `status` | `ok`, `pending` (work continues; wait on the returned reference) or `error`. A reply held for a person's approval is `pending` with the `att_` reference and a `next` that says what to do (see [Held for approval](#held-for-approval)). |
| `since` | Meaningful events since this session's previous response: windows, dialogs, focus taken, control changes, notifications. It is live observation, not timeline history. |
| `result` | Tool-specific; see below. |
| `error` | `{code, message, retry_safe, requires_reconciliation, next, …}`, following the registry in `src/error.rs`. It names the true cause, such as the offending field, and gives next moves. |

**How the envelope reaches the harness:**

- MCP `content[0]` is a text rendering: the situation line, then a compact rendering of `result`.
- `structuredContent` carries the envelope unless the response also carries images. Codex drops images when `structuredContent` is present, so in that case the full envelope is sent as the text part.
- Images appear only when the agent asked for one (`view: "image"`). They are always a crop of the requested surface, WebP or JPEG, and never a full-screen PNG unless the agent asked for `view: "screen"`.

## References

| Prefix | Refers to |
|---|---|
| `cmp_` | A computer |
| `task_` | A task, which is always on one computer |
| `op_` | An operation or step |
| `att_` | An attention item |
| `art_` | A published file |
| `frame_` | An observation |
| `e12` | An element within a frame |
| `c3` | A choice within a frame |

Any reference resolves through `computer_status({ref})` to its current state, or to a plain `ended` or `expired` with next moves. Handles from before a restart or a handoff are `ended`, never revived.

## The eleven tools

### `computer_status({ref?})`
- **With no `ref`:** returns the fleet, meaning every computer this harness may use, each with its name, id, state, current user, holder and a one-line capability summary. With one computer configured, it returns that computer. The summary says "no browser open yet" when the browser is simply closed, which is not a fault, and "browser page reader (not installed)" only when browser checks and `browser_act` cannot work on that computer.
- **With a `ref`:** returns that thing's state, its parent and children, and its valid next moves.
- **`ref: "help:<tool>"`:** returns the long description and examples for that tool, keeping the tool list small.
- It never captures the screen.

### `computer_begin({computer?, goal, checks?, deliver?, request_id})`
**Inputs:**
- `computer` is a name, a host or a `cmp_` id. It is resolved once and echoed back as an id. It may be left out when only one computer is reachable. Every ibara command that names a computer takes the same names, so the collector takes them too: `ibara client --computer COMPUTER fetch ARTIFACT_REF NEW_LOCAL_PATH`, with COMPUTER as `computer_status` lists it (its name or `cmp_` id).
- `checks` is a list of `{id, description, check?}`. `check` is a typed check (see below), such as `{kind: "file_exists", path: "note.txt"}`.
- `deliver` is an optional `{host, path}` obligation. `host` and `path` are checked as a send's `to` is (see `computer_files`), so a delivery no send could make is refused here, and the delivery is the one a later send to the same computer and path makes, under whichever of the computer's names.
  It is verified when the sent file is collected to exactly that path (after any change a person makes to it): by the agent's collector, or by a person saving it in the console. A copy saved anywhere else does not count.

**Result:**

| Field | Contents |
|---|---|
| `task_ref`, `computer` | The task, and the computer's `{id, name}` |
| `you` | `{agent, principal}`, e.g. `codex@vesper` and `vesper`. The agent label is written beside the agent's cursor on the screen. Cua picks the cursor's color anew each time it starts, so ibara names none |
| `workspace` | The absolute path of the task's workspace. Relative paths in `computer_files`, the `computer_exec` `cwd`, checks and `file` expectations resolve here; type it into an app's Save As dialog to save there |
| `checks` | For each check: `{id, basis: "automatic" \| "your_assessment", state}`. A typed check that can never pass is refused here, not discovered at `finish` |
| `frame` | The first frame (see `observe`) |
| `notes` | Any app notes that match what is on screen |

It is idempotent by `request_id`. Replaying a `request_id` with the same arguments returns the original result, or the original error, in any session. A `request_id` names one request within an MCP session: different arguments under a `request_id` already used in this session return `REQUEST_CONFLICT`. In a later session, different arguments are a new request, once the earlier one under that `request_id` has finished; while it is still running, of unknown outcome or waiting for a person's approval, they return `REQUEST_CONFLICT`. This holds for every tool that takes a `request_id`.

While ibara itself holds the computer back, `computer_begin` returns `BUSY` with `retry_safe: true` and says why: for a few seconds after ibara starts (after an update, a restart or a crash) it is starting, and when earlier work did not finish stopping it is settling that work first. Begin again after the wait it names, with a new `request_id`. When ibara cannot resume by itself (a repair needs a person, or this computer's settings leave resuming to a person), the refusal says so and is not retry-safe. `HUMAN_CONTROL` is only ever a person: they paused the computer or hold it through Take Control. A step cut off by a restart says ibara restarted: the task's control has ended, so check what the step did and begin again.

### `computer_observe({task_ref, view?, query?, surface?, limit?, cursor?})`
**`view`:**
- `situation` (default): windows, focus, the dialog and up to 20 ranked choices, as compact lines like `e12 button "Save" enabled · dialog "Save As"`.
- `elements`: a query, subtree or region, with real paging via `cursor`.
- `image`: a crop of the named or focused surface.
- `screen`: the full screen, only when asked for.

**Result:** `{frame: {frame_ref, revision, captured_at, covered, cost_bytes, lines[], choices[], next_richer}}`. Each `lines[]` entry is about 60 bytes. `choices[]` is at most 20: `{choice_id, label, action, param?}`, where `param` names a parameter the agent must supply (for example `text`). `next_richer` names the next, richer view available, and `next_cursor` continues a paged element list.

### `computer_act({task_ref, request_id, choice?, text?, action?, expect?, effect?, steps?})`
**One step:** a `choice` from the latest frame (with `text` when the choice takes text), or an explicit `action`, plus an optional `expect`. A typing or key choice sends only to the window it was offered for; if another window has the keyboard focus by then, the step is refused (`STALE_TARGET`, `execution_not_started: true`) and nothing is sent. An explicit `type` or `key` action goes to the window that has the focus when it runs, and typing stops between pieces if the focus moves.

**Several steps:** `steps` holds up to 8 `{choice | action, text?, expect?, effect?}`. They are checked one at a time and stop at the first unmet expectation. The grant is checked for each step on its own.

**Actions** are objects with a `kind` and its parameters, such as `{kind: "launch", app: "editor"}` or `{kind: "click", target: "e12"}`. Expectations and typed checks take the same form.

| Action | Parameters |
|---|---|
| `launch` | `app`: `editor`, `terminal`, `browser` or `files` (the file manager, Nautilus). `text editor`, `mousepad`, `shell`, `chrome`, `file manager` and `nautilus` work too. The editor is Mousepad with the person's own settings, so `gsettings` and `dconf` show what it uses. ibara keeps one of them off: session restore (`org.xfce.mousepad.preferences.file session-restore` is set to `never` when the editor opens, which also turns off Mousepad's autosave), so the editor never offers to restore a session |
| `focus` | `surface` |
| `click` | `target`: an element, or a point `{x, y, frame?}` in a picture's pixels (see below) |
| `double_click`, `right_click` | `target` |
| `type` | `text`; sent in short pieces that are never cancelled |
| `key` | `keys`, e.g. `"ctrl+s"` |
| `scroll` | `target`, `dx`, `dy` |
| `close` | `surface`, only surfaces this task opened |

**Points:** `x` and `y` are pixels of a picture from `computer_observe` with `view: "image"` or `"screen"`, not screen coordinates. `frame` names the frame that returned the picture; without it, the task's latest picture is used, however many frames came after it (an elements observe, the frame every `computer_act` returns). ibara keeps the latest picture of each window, maps the point to the screen through it, and the step's `effect` names the screen point it clicked and the picture it came from. The step is refused with `STALE_TARGET` and `execution_not_started: true`, and nothing is sent, when the task has not taken a picture yet, when `frame` names a picture ibara no longer keeps, or when the pictured window has moved, changed size or closed since. Then observe with `view: "image"` again and read the point from the new picture. A point outside the picture is refused as `INVALID_ARGUMENT`.

**Expectations**, each with an optional `within_ms` (default per kind; the agent may override):

| Expectation | Parameters |
|---|---|
| `window` | `{app?, title?, gone?}` |
| `dialog` | `{title?, gone?}` |
| `focus` | `{app?, title?}` |
| `text` | `{text, surface?}` |
| `element` | `{query, state?, value?}` |
| `url` | `{contains}` |
| `file` | `{path, exists?, contains?}` |
| `settled` | `{quiet_ms}` |

An expectation after a step looks for a change the step made. `window` is met by a window that appeared, or came to match, after the step: an editor that was already open does not meet it. `dialog` with a `title` is met the same way, so a matching dialog left open from before the step does not meet it. `dialog` with `gone` is met when the dialog that was in front before the step has closed; with a `title`, the matching dialog in front before the step. `computer_wait` looks for a state instead, so a window that is already open meets it.

`app` in `window` and `focus` is any name `launch` takes for an approved app, or part of the window class: `browser`, `chromium`, `chrome` and `google chrome` all match both Chromium and Google Chrome.

**Result:** `{steps: [{index, outcome: "done" | "unmet" | "unknown" | "not_run", effect, op_ref}], frame, attention?}`.

`effect` (on the act or on a step) declares a stricter effect class, `send`, `spend` or `destructive`, so the step is held for approval when that class asks first. It can never loosen a class.

ibara also classes some steps as `send` on its own, whatever `effect` says: a click on a page element the page reader marks `submits` (the submit button of a form that posts), Return typed or pressed where the page says it submits such a form (a field marked `enter_submits`, or the focused field or submit button), space on a focused submit button, and a click on a desktop button named Send or Submit. A form that only opens a page, such as a site search, does not count. ibara cannot see a send for a form inside a frame, a page that sends from its own script without a form, a coordinate click, or an app other than the browser; there only `effect` holds the step.

A held step stays held while its approval is open: the same step under a new `request_id` is refused with `PERMISSION_DENIED`, whatever `effect` it declares. The error names the `att_` reference and says what to do next: wait for it with `computer_wait({for: {attention}})`, then repeat the held request with its own `request_id`. Once a person answered it (approved or denied) or the approval was canceled, the same step under a new `request_id` counts as at least the class it was held as, whatever `effect` it declares, and asks again when that class asks first for this agent; it is never refused for that. When the person's last answer to it was no, it asks again whatever the class's rule, so it never runs without a person. The same step is the same kind on the same element, window or page, never the observation's token; a command in the same folder however `cwd` is written (none, `.`, `./` or the workspace's absolute path); a key in the same window whatever came before it and, in a browser, on the same page (without the query), or on any page of the window when ibara cannot tell which page the window shows; a click on the same element, or on the screen within 16 pixels, whether single, double or right.

- The frame is captured after the last expectation resolved, never a fixed number of milliseconds after dispatch.
- After a `launch`, ibara waits up to 10 seconds for the app's first window, also without an `expect`. That window is the task's, so finishing closes it. For an app that keeps running as the program ibara started (the editor, the terminal), only that program's windows count, so a window of the same app that a person opens meanwhile is not the task's.
- "Unmet" means the expectation was not seen within the deadline. It does not mean the step failed.
- Nothing is replayed after an unknown outcome.
- A step that needs approval returns `status: "pending"` with an `att_` reference and a `next`, and does not run. See [Held for approval](#held-for-approval).

### `browser_act({task_ref, request_id, action, expect?, effect?})`
A semantic action in the signed-in Chrome, through the extension. `action` is one of `navigate` (`url`), `click` (`target`), `type` (`target?`, `text`), `select` (`target`, `value`), `scroll` (`target?`, `dx?`, `dy?`), `key` (`keys`) or `wait_for` (`target?`, `text?`, `within_ms?`), with `kind` naming it. `target` is a page element id such as `b3`, from observing `surface: "tab"` with `view: "elements"`. `expect` works as in `computer_act`.

### `computer_exec({task_ref, request_id, command[], cwd?, timeout_ms?, background?, effect?})`
Runs a command, bounded, in the task's workspace or in `cwd`. `cwd` takes a path as `computer_files` does. Its effect class is `change` unless declared otherwise with `effect: "send" | "spend" | "destructive"`. A command held for approval replies as in [Held for approval](#held-for-approval) and stays held: the same command and `cwd` under a new `request_id` is refused while its approval is open, and asks again once it was answered, as for `computer_act`.

### `computer_wait({task_ref, for, deadline_ms})`
**`for`:** `{op}`, `{attention}` or `{expect}` (any expectation above). `deadline_ms` is at most 600000 (10 minutes).

Returns when the thing it waits for is met, or at the deadline with `status: "pending"`. It never repeats an effect.

### `computer_files({task_ref, request_id, op, …})`
**`op`:**

| Op | Parameters or behaviour |
|---|---|
| `list` | `dir?` |
| `read` | `path`, `max_bytes?` |
| `write` | `path`, `text` or `base64` |
| `publish` | `path` |
| `send` | `path`, `to: {host, path}` |
| `status` | — |

**Paths:** a relative path is in the task's workspace. An absolute path may be inside the workspace, or anywhere in the home folder (`~/` works too) except ibara's own folders and other accounts' homes. ibara's own folders are everything it keeps or trusts in the home folder: its data (other tasks' workspaces), its state (the journal and the journal's backups), its settings, `~/.ssh` (its key, host-key pins and the SSH configuration it reads), its update scratch space, older copies of its programs in `~/.local/bin`, `~/.config/systemd` and `~/.config/environment.d` (how its services start), the console plugin, the browsers' native messaging hosts and flags files, and the lock at sign-in. These are matched by name and with symbolic links resolved. `..` is refused. A path outside these is refused with `INVALID_ARGUMENT`, and the message names the workspace and the home folder. A symbolic link along the path is never followed. File checks and file expectations follow the same rule, including through a link in the workspace.

- **Publish:** needs only the path. ibara links the step that wrote the file when it recorded that step; otherwise no author is claimed.
- **Send:** `to.host` is the computer the agent works from, where its collector runs: its name or host name in any case (the first part of a host name is enough), its `cmp_` or `computer_` id, its endpoint id, or the collector identity registered for it. Any other computer, a `to.path` that is not an absolute path in its plainest form (no `.` or `..` parts, no doubled slashes), or a `path` that is not a file is refused at once with `INVALID_ARGUMENT` and `execution_not_started: true`, before anyone is asked to approve it; a refused `to.host` names the computer a send can reach, with its ids. The reply's `to.host` is the name ibara records deliveries to that computer under.
- **A send moves no bytes.** It publishes the file and records where it must go; the delivery stays `pending`. The bytes move when the agent's collector, on its own computer, runs the command the send's `next` gives, `ibara client --computer <cmp_ id of the computer the file is on> fetch <art_ ref> <path>`, or when a person saves the file there from the ibara console. Only then is the delivery verified. `computer_status` on the `art_` ref and on the task repeats that command for each delivery not yet verified, and a `delivered` check that is not met says it too.
- **Held for approval:** `send`, and any `write` that overwrites, reply as in [Held for approval](#held-for-approval) when they ask first. A send is checked before it is held.

### Held for approval
A step, command or file operation that needs a person's approval does not run. Its reply has `status: "pending"`, the `att_` reference in `attention`, and a `next` in both the JSON and the text form:

> A person must approve this step (att_…) before it runs. Call computer_wait({task_ref: "task_…", for: {attention: "att_…"}, deadline_ms: 50000}) to wait for their answer, and again while it is still open. Then send this same request again (same request_id): it runs once if approved. Don't end your turn or ask your user; the person answers in ibara.

A held step in `computer_act` or `browser_act` also says so in its own line: `held for a person's approval (att_…); not run yet: wait for the answer, then send this same request again`.

Sent again with the same `request_id`:
- while the approval is open, the same pending reply comes back and nothing runs;
- once approved, the request runs, once; sending it again after that returns that result. A step whose target changed since it was approved is held again, with a new `att_`;
- once denied (or no longer valid, for example because control changed hands), it never runs: a step replies `not_run` with the reason, and a command or file operation is refused with `PERMISSION_DENIED` and the reason.

A person can also answer **Always Allow**: the step is approved, and from then on your steps of that kind (send, spend or delete) run on that computer without asking, for as long as your computer may run agent tasks there. Where your computer is set to ask first for that kind, they can only approve or deny.

A computer whose access rules make an agent ask before it begins, observes or reads files replies the same way. A `computer_begin` has no task to wait on yet, so its `next` says to send the same `computer_begin` request again after a few seconds until it is no longer pending.

### `computer_checkpoint({task_ref, note?, ask?, stop_asking?})`
- **`note`:** stores the agent's continuation note and returns its `note_ref`. The task's `computer_status` lists the last note among its children and repeats its text in `next`, so an agent that lost its context can continue; `computer_status({ref: note_ref})` returns the text.
- **`ask`:** `{question, options?}` raises an attention item. The answer appears in the next `situation` and `since`, and `computer_wait({for: {attention}})` waits for it. A person answers with one of the options (any short answer when there are none) or dismisses the question; a dismissed question, and every open question of a task whose control ended, is `expired` with no answer.
- **`stop_asking: true`:** if your person tells you that you don't need their approval before you send, spend or delete, call this. You can't change your own access, so ibara asks them once, on this computer: "codex@laptop asks to stop asking you before it sends, spends or deletes on Tulip1", with **Allow** and **Not Now**. Nothing changes until they choose Allow; then the kinds it names run on this computer without asking. It leaves out kinds you are denied and kinds your computer is set to ask first for, and changes of access still ask. The reply's `stop_asking` is `{attention: "att_…", state: "waiting_for_person"}`; asking again while that request waits returns the same `att_`. When nothing is left to ask for, it is `{state: "nothing_to_ask"}`. Carry on with the task meanwhile, and ask on each computer your person meant.

### `computer_finish({task_ref, request_id, outcome, summary, assessments?})`
**Inputs:**
- `outcome` is `complete`, `partial`, `cancelled` or `blocked`.
- `assessments` is `[{check, met, reason}]`, for checks whose basis is `your_assessment`. An assessment of an `automatic` check counts only when ibara cannot read what the check names (see [Typed checks](#typed-checks)); otherwise it is ignored and ibara's own result stands. `notes` says which. An assessment of a check the task does not have is refused, naming the checks to assess.

**What it does:**
- Evaluates every automatic check against current state.
- Closes windows and tabs the task opened and still owns. It leaves anything a person touched (focused or retitled while no agent step ran), and anything with unsaved changes. A window counts as closed only once it has gone; one that ignores the request is listed in `left`.
- Releases control.
- **After control ended** (a person took the computer, control expired, or ibara restarted), the task can still be finished. The outcome, summary and assessments are recorded, and control is not taken back. Checks that read the screen keep their last state. Windows are closed only when nobody has the computer; while a person or another task has it, every window is listed in `left` with the reason.
- Apps a task launched run as the desktop's own apps, not inside ibara, so they and their unsaved work outlive a restart of ibara. Their windows stay the task's across the restart, so finishing closes them, or leaves them, by the same rules.

**Result:** `{checks, delivery, cleanup: {closed[], left[]}, complete: bool, notes?}`. `complete` is true only with no unknown outcomes, every required check met, and every delivery verified.

### `computer_procedures({op: "search" | "read", query?, ref?})`
Reads approved procedures, which are deferred to later work. Until one exists, it returns the app notes that match.

## Typed checks

| Check | Parameters |
|---|---|
| `file_exists` | `path` |
| `file_content` | `path`, `equals` or `contains` |
| `artifact` | `sha256` |
| `url` | `contains` |
| `element` | `query`, `value?` or `state?` |
| `text_present` | `text` |
| `delivered` | `host`, `path` |

The basis is `automatic` when the check has a type, and `your_assessment` otherwise. Assessments are attributed to the session identity automatically.

A `url` check reads the browser's tabs through ibara's page reader, which is installed for Chromium and Google Chrome and connects when one of them opens. The browser does not need to be open at `computer_begin`: the check is read at finish. If no browser with the page reader is open then, the check is unmet and says so. If the page reader refuses or does not answer, ibara reconnects it and asks once more before the check is `unknown`. `computer_begin` refuses a `url` check only when the page reader is not installed for either browser on that computer.

A `text_present` check looks in the focused window. When that is a browser window, the page reader reads the focused tab's page, up to its first 200,000 characters, which the browser's accessibility tree mostly leaves out; the window's own tree (the address bar, the tab strip) is searched as well. A longer page without the text in that part is `unknown`, not `unmet`. In any other window, every element of its accessibility tree is searched (a long value only as far as its first 400 characters). The text must match exactly, apart from runs of spaces and line breaks on a web page.

A `url`, `element` or `text_present` check is `unknown` when ibara cannot read what it names: the window has no accessibility tree (a terminal such as foot; ibara does no OCR), or the page reader does not answer. Assess such a check at `computer_finish` like a `your_assessment` one: your assessment then decides it, its basis becomes `your_assessment` and its `detail` says why ibara could not read it. When ibara can read it, its `met` or `unmet` stands whatever you assess.

## What is left out on purpose

- Contract negotiation.
- Contract 2.0 and 3.0 shapes.
- Multi-computer tasks.
- OCR.
- Learned deadlines.
- A keypress classifier.
- More than eleven tools.

## Access-aware status

Status includes `access`: the named caller, effective capabilities, effect rules (`effects`, what its steps follow; `own_effects`, the ones its own grant sets) and pairing state. Deny wins over ask over allow across agent/computer grants, and an agent's own grants count only while its computer's do. Send, spend and delete steps no rule covers follow this computer's Ask before agents send, spend or delete for agents from the owner's own computers, and ask for any other computer's agents (see [security and access](security-and-access.md#turning-approvals-off)). Ask returns a pending attention reference; a person with administer answers it, then the caller retries the exact request. Approvals expire across authority changes and never authorize a later step. See [security and access](security-and-access.md).
