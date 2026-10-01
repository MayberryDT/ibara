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

Keep code edits, git, tests, APIs and public pages you can fetch where you are. Don't use ibara to read your own files.

`computer_status` lists the computers you may use, with their state. Use the one your person named. If they named none and more than one is ready, ask which.

## One task, start to finish

A task holds the computer: your person sees it in their console, and no other agent can use that computer until you finish. If `computer_begin` says the computer is in use by a person or another task, tell your person instead of waiting in a loop.

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
7. **Finish at the first safe point.** Call `computer_finish` with `complete` only when the checks are met. Otherwise use `partial` or `blocked`, with a summary that says what is and isn't done. Tell your person what the checks proved, not only that you finished. Don't keep the computer while you write code, think at length, or wait for your person. Begin again when you next need the screen.

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

Your person can share their logins with you, one site at a time. You never see a password or a login; ibara copies it from their browser into the computer's browser.

- **Name the sites you know you'll need when you begin:** `computer_begin({…, logins: ["irs.gov", "id.me"]})`. Your person answers once for all of them, and you can start working meanwhile. The reply gives each site's `state`. For `rejected_before`, plan for your person to sign in with Take Control.
- **At a sign-in page, use `browser_act` with `{kind: "sign_in"}`** before anything else. It covers the site the tab shows and the site it came from. The reply's `page` is `left_sign_in` when the page is past its sign-in form. `unknown` is not success, so check the page.
- **Every other outcome has a reason code with a `next`.** Follow it:
  - `waiting_for_person`, `waiting_for_browser` or `waiting_for_sharing_computer`: wait as `next` says, then resend the same request.
  - `declined` or `denied`: don't ask again for that site. Take another route, or ask with `computer_checkpoint`.
  - `site_rejected`: the site didn't accept the shared login. Ask your person to sign in with Take Control.
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

- **On that computer:** `computer_files` reads, writes and lists files in the task's workspace and the home folder. `computer_exec` runs one bounded command; `command` is the program and its arguments, with no shell.
- **Bringing a file to your own computer takes two steps.**
  - `send` records the delivery but moves no bytes.
  - Then run the fetch command its `next` gives (`ibara client … fetch …`) on your computer.
  - Agents that stopped after `send` never delivered the file.

## What leaves the computer

Screens and text you read go to your model provider. Read what the task needs: an element query or a cropped image, not the whole screen.
