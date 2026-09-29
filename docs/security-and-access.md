# Security and access

This page explains who can reach a computer running ibara, what each of them may do, how approvals work and where the protection ends. The first half is for everyone. The reference at the end is for people changing ibara or recovering a computer by hand.

To report a vulnerability, see [SECURITY.md](../SECURITY.md).

## Who can reach a computer

Only computers on your Tailscale network, and of those only the ones you paired.

- ibara listens only on the computer's Tailscale addresses: the agent entry (SSH, port 2222), pairing (port 24247) and, while someone holds control, the stream. [Architecture](architecture.md#how-computers-reach-each-other) lists the ports.
- The agent entry accepts only the keys of paired computers. Each key can run only ibara's own entry, never a shell.
- Each paired computer gets its own restricted Unix account, `ibara-op-NAME`, which can reach only its own socket. Agents arrive as `ibara-agent`.
- ibara has no server of its own and no accounts to sign in to. Pictures, files and commands go directly between your computers.

## How a computer is added

Pairing is how one computer gets a place on another. The asking computer signs its request with its key, so it cannot pretend to hold a key it does not have. Tailscale tells the other computer which computer and which login is asking.

There are 3 ways a pairing is accepted:

| Who is asking | How it is accepted | What it gets |
|---|---|---|
| Your own computer (the same Tailscale login, neither computer tagged) | Your ibara on the asking computer confirms that its console made this request. No code needed. | Every permission, Administer included |
| Someone else's computer | A person at your computer accepts a 6-digit code that both screens show, within 5 minutes | Watch and Files |
| A friend with an invite code you made | The code, used once before it expires | Exactly the level you chose, until the invite ends |

Invite levels are Watch; Use with Approval (files, Take Control and agent tasks each ask first); and Take Control (agent tasks ask first). An invite never includes Administer. Codes are 8 characters, and ibara keeps only their SHA-256. 5 wrong codes within 10 minutes lock a computer out for up to 10 minutes, even if it then brings a good one.

A program running as your desktop user counts as you. Another account on the asking computer, such as the agent account, cannot vouch for itself: its request waits for a person like anyone else's.

## Permissions

Every paired computer and every agent has 5 permissions on each computer:

| Permission | Lets them |
|---|---|
| Watch | See the screen, the agent tasks and the history |
| Files | Send and collect files |
| Take Control | Use the keyboard and mouse through the viewer |
| Agent Tasks | Let their agents work on this computer |
| Administer | Change access, answer approvals, restart, shut down, sleep and update |

Each permission is Allowed, Ask First or Denied, and can end at a set time. Where several rules match, Denied wins over Ask First, which wins over Allowed. No matching rule means Denied. For an agent's send, spend and delete steps that no rule covers, see [Turning approvals off](#turning-approvals-off). An agent also inherits the permissions of the computer it came from, and never has more: its own rules count only while its computer has that permission, so they end when the computer's permission ends or is removed.

The computer's own owner always keeps Administer, so you cannot lock yourself out: ibara refuses any change that would take it away or make it expire.

Change permissions in the console's Access tab. The change applies at once, and already-open calls are checked again.

## Named agents

An agent is known by its own name and the computer it came from, for example `codex@laptop`. The name comes from the agent's MCP client, and the paired computer vouches for it. Its named cursor on the screen shows the same name.

This naming is a guardrail, not isolation. Two agents on the same paired computer could claim each other's names. So an agent's own Allowed never overrides Ask First set for its computer.

## Approvals

Every step an agent takes has an effect class. By default:

| Effect class | Examples | Default |
|---|---|---|
| Observe | Reading the screen, a file or a page | Allowed |
| Change | Typing, clicking, saving a file | Allowed |
| Send | Submitting a form, sending a message | Ask First |
| Spend | Buying something | Ask First |
| Destructive | Deleting or overwriting | Ask First |
| Access | Changing who can do what | Ask First |

A step that asks first stops and waits. The console shows who wants to do what, on which computer, in plain words, with Approve and Deny, and a desktop notification says so too: for example "codex@lumen wants to press Return in Chromium on localhost/signup, which sends something. For the task “Create an account”." The exact request (the step, its window, page and process) is under Details. The agent then repeats the exact same request, and only then does the step run, once.

An approval is bound to that exact step, its content, its target, the agent's current control and the current permissions. A changed step, a later step in the same sequence, or a step after control changed hands needs a new approval. Pausing, Take Control, a restart and any access change cancel approvals that were not used yet.

An agent can declare a step stricter than its class, for example marking a click that sends as a send. It can never make a denied change allowed.

ibara also counts some steps as Send itself, whatever the agent declares. In Chromium or Google Chrome with the ibara extension, these are a click on the submit button of a form that sends data, Return in a field of such a form, and Return or space on its submit button. A search box that only opens a results page does not count. On the desktop, a click on a button named Send or Submit counts too.

A step that is waiting for approval stays stopped. If the agent sends the same step again as a new request while it waits, ibara refuses it, even when the agent now declares it as something milder. Once someone has answered, whether they approved or denied it, or the approval was canceled, the same step counts as at least what it was held as each time the agent sends it as a new request, and asks again while that kind asks first. If the last answer was no, it asks again whatever the rules say: ibara never runs a refused step without asking, and never blocks it for the rest of the task.

The same step means the same action on the same target, whatever the request says around it. A command is the same in the same folder, however the folder is written (no folder, `.`, `./` or the workspace's full path). A key press is the same key in the same window, whatever was typed or pressed before it. In Chromium or Google Chrome with the ibara extension, it must also be on the same page (its address without the part after `?`). When ibara cannot tell which page the window shows, it counts as the same on any page of that window. So after someone denies Return in a terminal, every later Return in that terminal asks first, and Return in another window or on another page runs as usual. A click is the same on the same element, or within a few pixels of the same point, whether it is a single, double or right click. However many other questions an agent asks, an answered step keeps asking for the rest of the task.

Watch set to Ask First asks once per sitting, not for every picture: the answer holds for that computer until it has not watched for 5 minutes, access changes or ibara restarts.

### Turning approvals off

You can let agents send, spend and delete without asking you:

- **Settings, for all your computers:** turn off **Ask before agents send, spend or delete**. Every computer you manage from that console takes it, including one that is off now or that you add later, the next time the console hears from it. A computer shared with you by someone else does not.
- **A computer's Settings tab, for that computer:** the same choice, as **Same as in Settings**, **On** or **Off**. On or Off there wins over Settings.
- **Access, for one agent:** each agent's row has **Ask before it sends, spends or deletes**, and a rule for each kind of step. An agent's own Ask First wins over its computer's Allowed and over Settings. Its own Allowed wins over Settings, but not over Ask First set for its computer, because any agent on that computer can use its name.
- **Always Allow**, on an approval of an agent's send, spend or delete, approves that step and lets that agent do that kind of step on that computer without asking from then on. It lasts only while that agent's computer may run agent tasks there: when a friend's invite or a time limit ends, Always Allow ends with it. Where the agent's computer is set to Ask First for that kind, ibara refuses Always Allow and the approval stays open for Approve or Deny. The message says what changed; undo it in that computer's Access tab.
- **When you tell your agent not to ask**, the agent asks ibara, and ibara asks you once: "codex@laptop asks to stop asking you before it sends, spends or deletes on Tulip1", with **Allow** and **Not Now**. Allow sets the same rules as Always Allow, for the kinds it names; it leaves out a kind the agent's computer is set to ask for. An agent cannot change its own rules: its tools can only ask.

Off applies to agents from your own computers: the ones that may administer this computer. Agents from any other computer, such as a friend's computer you shared this one with, still ask before they send, spend or delete, whatever the switch says, unless you choose Always Allow for one of them.

Off means those steps run without a person. Nothing else changes: a kind of step that is Denied stays denied, changes of access still ask first, and Administer, Take Control and pausing agents work as before. A step a person refused asks again even when its kind no longer asks.

Where rules meet, for a send, spend or delete step: Denied anywhere wins; then Ask First set for the agent or its computer; then Allowed set for either; then, for an agent from your own computer, this computer's Ask before agents send, spend or delete (its own choice, else the one from Settings). An agent from any other computer asks. A computer's access rule that lists every kind of step, as pairing writes it, counts only where it differs from the defaults. An agent's own rules always count, including ones an older console wrote for every kind.

## Where the protection ends

Approvals cover the effects ibara knows about or an agent declares. ibara cannot always tell what a shell command or a click will do. An agent with permission to run commands or use the desktop is not in a sandbox, and it could send, spend or delete in a way ibara does not recognize.

ibara cannot tell that a step sends, and relies on the agent declaring it, when:

- the form is inside a frame on the page, or the page sends with its own script rather than a form (many chat apps and web apps do)
- the browser extension is not connected
- the click is aimed at screen coordinates, not at a named element
- the app is not a browser, for example Return in a chat or email app

It also knows a step as "the same" only by its kind, its target and its window or page. The same click sent another way, for example at coordinates instead of on the named element, is a new step. So is a command written another way, or a line ended by typing a line break instead of pressing Return.

So give Agent Tasks only to agents you would trust at that computer's keyboard, and use Ask First or Denied where that trust is not there.

Other limits we want you to know:

- Tailscale names a computer, not the account on it. Own-computer pairing relies on the asking computer's ibara holding port 24247. On a computer where ibara's service is not running, another local account could take that port and vouch for itself.
- Starting without the disk password (`ibara unattended-boot`, off unless you turn it on) seals the disk key to the TPM and Secure Boot state. Anyone who has the whole computer can then get at your files, even with Secure Boot on. Only a disk taken out of the computer stays unreadable. When Omarchy signs you in automatically, ibara lets you turn this on only while the screen locks at sign-in.
- Files other computers send you land in `~/Downloads/Ibara`. Nothing there is overwritten, but ibara does not scan what arrives.
- An agent's ibara tools can only ask for fewer approvals. An agent that also runs programs as you on a computer with the ibara console, outside ibara, can use that console as you can, answering approvals included, just as it could edit its own agent's settings.

## Taking back control

You can always take a computer back from an agent:

- moving the mouse stops the agent's input at once, and its next input waits until the mouse is still
- Take Control pauses every agent until you hand back. Hand Back lets agents work again unless a person paused them, and never restarts an agent's task by itself
- Pause Agents stops agents without taking control, until you choose Resume
- removing a permission or a pairing takes effect on the next call, and ends any viewer that permission allowed

## Signed releases

The installer and `ibara update` check the release manifest's SSH signature against the public key built into ibara, then each package's SHA-256 and size, before anything is installed. The key's public half is in [packaging/release.env](../packaging/release.env). [Release verification](release-verification.md) shows how to check a release by hand.

## Reference

### The access record

The access model in `journal.sqlite` (`meta.access_model`) is the only source of truth: identities, verified pairings and grants. The Access tab reads the same table enforcement uses. An agent's `computer_status` returns its own row.

Read it on the computer with `sudo computerctl access`. Changes take one JSON argument and the `expected_revision` from the read:

```sh
sudo computerctl access_set '{"subject":"vesper","capability":"files","rule":"ask","expected_revision":42}'
sudo computerctl access_remove '{"grant_id":"vesper:files","expected_revision":43}'
sudo computerctl access_unpair '{"subject":"vesper","expected_revision":44}'
```

- `access_set` without a `grant_id` replaces `subject:capability`. An explicit ID can add an overlapping rule.
- `access_remove` with a `subject` removes all its grants and those of its agents.
- `access_unpair` removes the pairing and disables its key. A lost or changed key is paired again from the other computer. Restoring a grant cannot bring back an unpaired identity.
- `expires_at` takes UTC `YYYY-MM-DDTHH:MM:SS[.sss]Z`. Expired grants stop matching.
- Only Administer can change grants or answer approvals.
- The name `owner` is kept for the local owner. A paired computer whose Tailscale name is `owner` is paired as `owner-2`.

### Accounts and keys

Root owns the keys and accounts. `ibara setup` runs `ibara access-system init USER` once, which seals the reviewed key bindings in `/etc/agent-computer/access-transport.json`. After that, `ibarad` sends only decisions to the root helper behind `ibara-access.socket`: enable or disable a binding, and for a newly paired computer its own Ed25519 public key.

The helper never accepts commands, paths, account names or user IDs. From the pairing's name it derives the `ibara-op-NAME` account, a free system user ID below 1000, the key file `/etc/ibara-operator/authorized_keys/ibara-op-NAME` and the agent entry's key line. It refuses the whole request for a key already bound to another name, a malformed key or name, or more than 64 paired computers. The socket is mode 0600 and checks the caller's user ID, so only the desktop user can reach it.

A denial is saved and enforced before the accounts are cleaned up. If the cleanup fails, the error carries `access_saved: true`: the permission changed, but the transport needs repair. Check `journalctl -u 'ibara-access@*'`, then run `sudo computerctl access_sync`. Do not edit the generated files to grant access.

### Journal versions and going back

Access-aware journals are at base schema 3 and core schema 4 or later. Core schema 5 records who paused the computer, and 6 is the current release's. Builds from before access control refuse these journals, because they could not enforce newer revocations.

`ibara rollback` handles going back between releases. Restoring an old journal or running an old binary by hand can bring back access you have since removed, so it is not a supported way back. If you must restore a journal from before a change, first pause the computer, keep the current journals and settings, and redo every later grant and revocation.
