# Console reference

This page describes everything the ibara console does, for people changing the plugin or checking a detail. The [README](../README.md) is the short tour, and [design](design.md) holds the visual and interaction rules.

The bar shows only the ibara mark and one state dot; its tooltip and accessible name carry the fleet summary. Click it for the quick panel (or, while it shows a count or a red dot, for what that is about in the console), or right-click it to open the console.

Kinds: `service`, `bar-widget`, `panel`. The quick panel is bar-anchored. The
console is an independently summoned Omarchy-hosted floating panel. Both use
the same service, which talks to this computer's ibara service, `ibarad`, over
one Unix socket.

## Interface

Every shape is square: no rounded corners, pills or badges. Colors, font, scale and borders come from Omarchy. The mark uses its master and compact geometry exactly; its `i` is transparent.

### Bar and quick panel

The bar shows a count when approvals, login requests, agents' questions or computers need you; clicking it opens that one computer (with the keyboard on the one approval's Approve), or the fleet when there are several. Otherwise it shows a dot for the fleet's worst state: red when a computer has been offline or needed attention for a minute or more (a restart or an update never counts; clicking opens it), purple when you have control of one, blue when agents are working, and no dot otherwise. Whatever makes the bar red has a toast in the console on every page that says why, and a toast put away there takes its red with it. With nothing open, a computer the bar counts as offline or needing attention is asked again on every refresh, so a problem that has passed clears. The tooltip reads `ibara · <summary>`. Approvals and computers that need you are read every 10 s (`fleet-attention`) even with nothing open, and each new approval raises one desktop notification with Approve and Deny on it (`notify-send --action`; the Omarchy shell's notification server shows the actions), unless Approval notifications is off in Settings. A login request raises one with Share and Don't Share (for every site it names) on the computer your logins come from, one saying to open the console while no computer shares, and none where another computer shares. An approval answered elsewhere closes its notification.

The quick panel is small. What waits for your answer comes first, under its title: a request from another computer to use this one, with its code, Accept and Decline; then the first two approvals, login requests and agents' questions, oldest first, each approval with Approve, Deny and Details, and Always Allow on a send, spend or delete (A, D, Shift+A and I answer the first approval), and each login request as in the console; then "+N more waiting for your answer" with Show All, which opens the console. Then the number of computers, a count for each state, up to 5 computers, the ones that need attention or are in use first, and "+N more". Open Console is its main action. Enter on a computer opens the console at that computer. It shows no previews.

### Console

The console window is about 1700 by 920 and opens on the fleet wall. It has 5 routes:

- fleet: every computer as a live card, attention first
- computer: the fleet list on the left and one computer on the right, with its own tabs
- add: the computers on your tailnet, each added with one choice
- share: Share This Computer
- settings: this console's own settings

With no computers yet, the wall shows one line and one button in the middle, **Add Your First Computer**; a console before its first computer is normal, never an error. A request from another computer to use this one is a toast, "Bench (alex@example.com) wants to use this computer" with its code ("Accept only if Bench shows 482 913"), Accept and Decline, and a notification says so once. **Connect an Agent** in the fleet's toolbar opens a card attached to the button with the one prompt that connects any agent program and **Copy Prompt**: paste it to Codex, Claude Code or any other agent that can use MCP servers, and the agent adds ibara to its own settings and says which computers it can use. ibara has no setup of its own for any agent program. Until an agent has begun a task through this computer, one note on the fleet points to it once, with its own Connect an Agent button. Add Computer always shows the same prompt at the end of its page, so another agent can be connected any time; on a computer without the console, `ibara prompt` prints the prompt.

**Add Computer** starts with Tailscale. Not installed, off or signed out, it says so in one sentence with one button: Install Tailscale shows Omarchy's one install command, and Sign In to Tailscale (or Turn On Tailscale) opens Tailscale's sign-in page, running `tailscale up` first when Tailscale has none waiting, as Omarchy's own Tailscale menu does. Otherwise it lists the computers on your tailnet, this computer first: each with its owner ("Yours" or theirs) and whether ibara is ready there. Your own computers add themselves with Add, or all at once with Add Your 2 Computers, which leaves out the computer you're on (add it by itself if you want it on your fleet too); another person's computer shows the six-digit code both screens show ("Check that Bench shows 482 913, then accept there.") until someone there accepts, declines or it expires, with Cancel. A computer without ibara shows the command that installs it, with Copy Command; one that is off is dimmed. A computer already on your fleet that now answers with a new identity, as after it was reinstalled with its data deleted, reads "It answers as a new computer, as after a reinstall" with **Add Again**: it is added the same way as the first time (your own computer at once, someone else's with the code) and keeps its card. When nothing else is being added, the console goes back to the fleet, where the new card is, and offers Add Another. Up and Down move between the computers' buttons.

**Share This Computer** (this computer's card menu on the fleet, or its System tab) lets a friend use this computer from theirs. Choose what they can do (Watch; Use with Approval, where files, Take Control and agent tasks each ask first; or Take Control, where agent tasks ask first; never administer) and for how long (1 Hour, 1 Day, 1 Week or Until Revoked), then Create Invite Code: a single-use code such as `4H7K-92QX`, shown once. The page says in one sentence that the friend's computer must be able to reach this one, with Share in Tailscale, which opens this computer's page in the Tailscale admin console. Every invite is listed with Revoke; revoking a used one asks first and ends that friend's access at once. On the friend's console, another person's computer in Add Computer has I Have an Invite Code beside Add: the code adds it at once with exactly that level; a wrong, used or expired code gets one plain refusal.

The fleet wall shows its counts in one form, label first, in Title Case like the filters: Needs Attention, In Use by You, In Use by Others (only while someone on another computer holds one), In Use by Agents, Agents Paused and Ready for Work. The quick panel (which leaves out counts of 0) and the bar tooltip use the same words. A ready computer reads Ready both on its tag and on the line under its name. The wall also has filters (All, Needs Attention, In Use, Ready, Favorites), search and Sort, a menu of Attention First and Name. The filters and Sort share the title's line when the window is wide enough for everything, and otherwise take one line under it; nothing else pads the header. Cards show a 16:9 preview, the state, who is using the computer and one line of activity. Clicking a card opens the computer. Take Control is the card's main action and, with its More menu (⋯), appears only on the focused or hovered card. While you hold control, the card always shows **Open Viewer** (**Close Viewer** while the viewer is open) and then **Hand Back** in Take Control's place. A favorite shows a star; Favorite in the computer's header sets it. A card that needs attention has a red border.

Inside a computer, the header shows its state, who is using it and what it is doing. Its actions run Resume (paused by a person) or Pause Agents, Wake (off or asleep, when it can be woken), Send File and, last, the one primary action: Take Control, or, while you hold control, Open Viewer (Close Viewer while the viewer is open) and then Hand Back in its place. Close (✕) sits apart at the far right. The tabs start right under the header, and the Screen tab shows the computer's live screen whenever it is open, so there is no separate Watch. A repair ibara couldn't make by itself is a toast, on every page, "<computer> needs you" with one sentence and **Fix It**; files on their way to the computer are a toast with their progress. Files dropped anywhere on the computer's side are sent to it. **Rename** beside the name turns it into a field: Enter saves, Escape cancels. The name is kept only in this computer's private directory (1–128 characters on one line); the renamed computer is not changed, and pairing it again keeps your name. The tabs are:

- Screen: the live screen at selected quality while this tab is showing, as large as the tab allows, with a rail of one-line sections beside it (under it on a window narrower than 1400 px): Now (the task, its agent and how long it has run; opened, its latest steps), Who can use it (how many people and agents; opened, one line each, you first, with a short mark for each permission: Watch, Files, Take Control, Agent Tasks and Administer), Recent files (how many and the newest; opened, this session's files) and, with two or more displays, Display. One section opens at a time; an open one shows at most eight lines, and Show All opens the Activity, Access or Files tab
- Windows: the computer's Hyprland workspaces, each with its windows (app and title), the one on screen marked On Screen and an empty one on screen reading "No windows". A window an agent is using reads In Use by an Agent, and its Close and Move To… wait until its task stops; one an agent left reads Left by an Agent, with the task's goal. **Close** asks the app to close, as clicking its close button does, so an app with unsaved work asks there first; a window running in a terminal (the computer tells from its process, whatever its app id) asks first here, "Anything running in it stops." A window that closed goes, with "Closed <title>."; one still open says "Still open. It may be asking to save; look at the Screen tab." **Move To…** puts the window on another workspace (1 to 10) without changing what that computer shows. Where the computer asks first, a message and the row say "Waiting for approval. After approval, choose Close again." (or Move To…). If an agent took the window meanwhile, the row says so with **Stop Task**, and the window is free as soon as the task stops. The list is read when the tab opens, every 5 seconds while it shows and after each action, but not while a confirmation or Move To… list of this tab is open; a computer on an older ibara says "Update ibara on this computer to manage its windows."
- Logins: one row per site with a rule for this computer or All Computers (the Never Share sites are listed under the rows): its rule in the Access tab's menu (Allowed, Ask First, Denied; a choice sets this computer's rule at once, with Undo, and a site Denied for All Computers can't be changed here), the task that first asked for it, when it was last shared and how that went, with its date (Worked, or the site asked to sign in again), **Share With…** (tick other computers, or All Computers, then Share: each gets the login fresh from your browser, now if it is on, else the first time an agent needs it) and **Remove**, which asks first, signs that computer's browser out of the site and removes its rule there, so the next request asks again. It shows only on the computer your logins come from, and a computer that takes its logins from another sharing computer says which one to use there, and a computer on an older ibara says to update it
- Activity: tasks with their checks and evidence, procedures awaiting review, task results and Follow Current Task. Collect File saves a task result into the download folder and never overwrites a file there
- Files: send a file, follow this session's transfers, and get files from the computer's approved folder
- Access: a compact table of who can use the computer and what each may do. Choose a cell to change it: a list of Allowed, Ask First and Denied opens in the cell, the choice applies at once, and a message offers Undo. Taking away your own Administer permission asks first in the same card attached to the cell, as do Remove Access and Remove Pairing in a row's detail. A row's detail also has **Ask before it sends, spends or deletes** (for a computer, "before its agents send, spend or delete"): a switch that sets that row's own rules for those kinds of step, and **Use This Computer's Setting**, which removes them. Each kind also has its own Allowed, Ask First or Denied; a change writes only that kind. Denied always wins, then Ask First set for the agent or its computer (any agent on a computer can use another's name, so an agent's Allowed never outranks its computer's Ask First), then Allowed. A kind neither sets follows the computer's Settings tab for agents from your own computers, and asks for agents from someone else's computer
- System: health, the ibara and screen sharing logs, Open Terminal, Lock Screen, **Update ibara** and **Update Omarchy** side by side, with Restart, Shut Down, Sleep and Wake kept apart under Power (Restart warns when the disk asks for its password), and **Remove Computer** under Remove from your fleet. Both run with nobody at that computer and no password, and each asks first in one line: Update ibara "Updates ibara to the latest signed release. Nobody needs to be there."; Update Omarchy "Runs Omarchy's system update (packages, kernel, firmware) with no questions. Nobody needs to be there; a restart may be needed after." Update ibara's toast says it started ("NAME: ibara is updating to the latest release. It may restart its bar when it finishes.") or that it is already up to date; Update Omarchy's says "NAME: Omarchy is updating. The computer stays usable; it says when a restart is needed." Neither starts while Omarchy updates there (the buttons say why), and a computer on an older ibara that doesn't know Update Omarchy says "NAME runs an older ibara. Choose Update ibara first." Under Health and upkeep, the last Omarchy update shows: "Updating Omarchy…", "Omarchy updated · TIME" (with "· restart to finish" when it needs one), or "Omarchy's last update failed · TIME." with why. Remove Computer (also in a card's ⋯ menu) asks "Remove <name> from your fleet? You can add it again from Add Computer." beside the button, then takes the computer off this console's fleet with its pinned host key, and a message says so. Access on that computer stays until its pairing is removed there
- Settings: the computer's own settings (`operator-settings`), kept on that computer. Under Agents, **Ask before agents send, spend or delete** is Same as in Settings (it says which, on or off), On or Off; On or Off there wins over Settings. Either way it covers agents from your own computers; agents from someone else's computer still ask

Everything computer-specific stays inside that computer and goes over its pairing route (`--computer ID --epoch E`). Each tab shows that computer's last data at once when you come back to it, with "Updated … ago · refreshing…", while ibara reads it again in the background. The kept data lives only in memory, holds only while the computer keeps the identity it was read under, and goes when the computer refuses access, leaves the fleet or access is denied.

On the fleet, a card whose repair needs a person shows the sentence over its picture and **Fix It**; one paused by a person shows **Resume**; one whose Omarchy update needs a restart to finish shows **Restart** (the same confirmation as Restart…) and "Updated · restart to finish"; one that is off or asleep and can be woken shows **Wake**. While Omarchy updates there, its card reads "Updating Omarchy…", and if that update fails a toast says "Omarchy's update on NAME failed. See its System tab." A computer whose screen is locked reads **Locked** and "Screen locked · Take Control to unlock" on its card and in its header, and counts under Needs Attention; after a minute it has a toast like one that isn't answering. Take Control is never blocked because the screen is locked: typing the password in the Viewer unlocks it. Each card's More menu (⋯) has Pause Agents or Resume, Wake, Restart…, Shut Down…, Sleep…, Share This Computer (on this computer's card) and Settings. An approval or a question waiting on a computer never hides its Pause Agents or Resume, on the card or on its page. Files dropped on a card are sent to that computer's shared folder, with their progress along the bottom of its picture. Fleet Actions has Apply Theme to Fleet, which puts this computer's Omarchy theme on every computer that is on; a toast says how many have it, and when some didn't take it, Details lists what happened on each and Apply Again tries again. It also has **Update ibara on All…** and **Update Omarchy on All…**, which run that update on every computer whose own is allowed now, this computer last, once every other one has answered; nobody needs to be at them. Its confirmation names how many and which, and which can't and why ("1 can't: Onyx isn't answering."); it is blocked, with the reason, when none can. Each computer's answer is its own toast: "NAME: ibara is updating to the latest release. It may restart its bar when it finishes.", "NAME: ibara is already up to date (VERSION).", "NAME: Omarchy is updating. …", or an error that stays, "NAME didn't update. …". A computer whose ibara is too old to know Update ibara says "NAME runs an older ibara. Update it once at that computer (ibara update), then Update ibara works."; one too old to know Update Omarchy says "NAME runs an older ibara. Choose Update ibara first." When the console opens after something happened, **While you were away** is a toast that says how much happened on how many computers; Details lists one line of events per computer, and Mark All Seen hides them until something new happens. After ibara updated itself, a toast says so once, and What's New opens the release's notes inside it. **Settings** (Ctrl+,) in the fleet header holds this console's own settings. What can be undone happens at once and a message offers Undo (Pause, Resume, access changes, every setting); only what is hard to take back asks first: Restart, Shut Down, Sleep, Lock Screen, Update ibara, Update Omarchy, ending a task, removing access and revoking a used invite.

Nothing ever sits between a page's header and its content. Every message is a toast in the bottom-right corner, over the page, so nothing moves when one appears; the newest is at the bottom. Approvals (with Approve, Deny and Details, which opens labeled lines inside the toast: who asks, what, where, the page, the window's title and the task, with **Copy Request** to copy the request exactly as the computer sent it; an agent's send, spend or delete also has **Always Allow**, which approves it and lets that agent do that kind of step on that computer without asking (ibara refuses it, and the approval stays open, where that agent's computer is set to Ask First for that kind), then says so in a message with **Open Access** to undo it; an agent's request to stop asking reads "<computer> needs your answer" with **Allow** and **Not Now**), agents' questions ("An agent on <computer> asks you", with one button per answer the agent offered, or a field and Send when it offered none, and **Dismiss**, which tells the agent nobody will answer) and requests to use this computer show on every page and stay until they are answered, here or elsewhere. A question ends by itself when its task's control ends. On an ibara too old to take answers to questions, Dismiss puts the question away on this computer only. A computer offline or needing attention for a minute or more is a toast that says what is wrong and what to do, with Wake when it can be woken and Open; put away, it shows again once its words change. Errors stay until you dismiss them (✕ or Escape) or the problem clears; other notes disappear after 5 seconds (10 with Undo), never while the pointer or the keyboard is on them. When more arrive than fit, the newest few show under "+N more", which says how many wait for an answer; Show All opens the whole list. The same message is never shown twice. Every message names a real place or action in the console, or says that ibara is handling it. When a computer does not answer, the message names it, for example "Tulip0 isn't answering. ibara will keep trying." Core texts that only say to "inspect the target" are rewritten to name the computer and what to do, or to say that ibara is checking. Action messages clear when you change route. An action that is hard to take back, such as Remove, Restart or ending a task, asks you to confirm in a small card attached to the button you chose: under it, or above it near the bottom of the window, with a notch pointing at it. The card names the computer and fades in over about 120 ms with focus on Cancel; Escape, Cancel or a click anywhere else cancels, and choosing the same button again closes it. The confirmation is dropped if you switch computer or tab, and nothing runs if the computer changed while you were confirming.

Previews keep the last decoded frame on screen until the next one is ready. A refresh does not rebuild the cards or blank a preview. An offline computer shows its last frame grayed. Revoked or unauthorized computers clear it. At most 20 computers preview at once; the others say why.

### Files

Choose File… opens the desktop file chooser outside the shell. The file keeps its name unless you change it under Save as. Send to <computer> puts it in the computer's approved folder that is shown on the right; nothing there is overwritten. Transfers show progress, then "checked" only after the receipt is verified. A failed send retries by resuming its retained job, never by sending again.

Get saves a file to the download folder: the Download folder setting, or `~/Downloads` when that is empty. Choose Folder picks another place for this session, and that folder opens when the file is checked.

### Logins

Login sharing lets an agent use a site you're signed in to in your own browser. It is off until you turn it on on the computer you use, which then becomes the sharing computer for every computer it administers; a computer that also runs agents can't be it yet. Three places turn it on, all the same setting: the card "Let your agents use your logins?" (a standing toast once the console has a computer, until answered, with **Turn On**, a choice of browser and profile when there are several, and **Not Now**), **Let agents use your logins** under Logins in Settings, and **Turn On Login Sharing** on a request while no computer shares. The card and the setting say that ibara copies a site's login only when you allow it and that the browser shows "Managed by your organization". If another computer shares, Turn On asks "Logins for your computers come from Laptop. Share from this computer instead?". Turning it off asks first ("ibara stops sharing and refreshing logins, and removes its extension from Brave when it can") and says that logins already on your computers stay until they run out or are removed. Its message then says whether the extension came out: where ibara couldn't take it out, "Its extension stays in Brave until you remove it there or turn sharing on and off from a computer ibara can set up.", or the reason it failed. Under the switch are the browser and profile logins come from (chosen while it is off), then the All Computers rules, each in the Access tab's menu, and a field with **Never Share** that denies a site on every computer before any agent asks.

An agent's request for logins (attention kind `login`) is a toast that waits for an answer, like an approval: "<computer> needs your approval", then "codex@relay, working on “File Q3 estimated tax” on Birch, wants your logins for 3 sites", with a tick for each site when there are several (only ticked sites are shared), **Share** (Allowed for that computer), **Share With All Computers**, **Don't Share** (refused this time, no rule) and **Details**: who asks (an agent from someone else's computer is asked every time), the computer, the task, the sites, the page and, for a two-site sign-in, "irs.gov signs in through id.me", with **Never Share** (Denied for All Computers). It is answered only on the sharing computer it names; any other console, even one that shares logins with other computers, reads "Answer this on Laptop." While no computer shares it offers Turn On Login Sharing (which goes to Settings when there is more than one browser or profile to choose) and Don't Share.

What the person sees when something stands in the way:

- Not signed in to the site in your browser: the request stays with "You're not signed in to sos.ok.gov in Brave. Sign in there, then choose Retry." and **Retry** and **Don't Share**.
- Your browser isn't running: "Open Brave to share your login for sos.ok.gov. It goes to Birch once Brave runs.", until dismissed.
- A write ibara couldn't confirm: "Couldn't confirm the login for sos.ok.gov was written. Try Share again or use Take Control."
- The site rejected the shared login: "id.me didn't accept your login on Birch", with **Take Control**, once per rejection.
- A computer on an older ibara: Share With… lists it blocked, and Share With… and Sync Logins say "Nimbus runs an older ibara. Update ibara there to share logins with it."
- A Remove on a computer that is off: "sos.ok.gov is off the list now. It will be removed from Onyx's browser when it next opens." One that removed part of it now: "Removed part of the login for sos.ok.gov from Birch. The rest goes when its browser next opens, …"
- Sync Logins where a computer's write couldn't be confirmed: "Couldn't confirm the logins were written on Garnet. Try Sync Logins again or use Take Control."

**Sync Logins…** in Fleet Actions makes every site Allowed on any computer Allowed for All Computers. Its confirmation says how many sites and computers, that each gets a fresh login from your browser, and which of those sites you're signed out of there.

## Shared state

A replacement bar (a bar plugin used in place of Omarchy's own) gives its widgets a service-less facade,
so this plugin publishes its hosted service through `ServiceBridge.js`. The
service owns the only poller, status cache, read queue and mutation queue. The bar widget
and independently summonable panel both resolve that singleton.

## Commands and IPC

Keyboard in the console:

- `/` focuses search, and Enter in search moves to the results
- arrow keys move across the wall grid or the computer list, and Left and Right switch tabs
- on the Screen tab's rail, Up and Down move between sections, and Enter or Space opens or closes one
- Enter opens the focused computer
- T takes control, V opens or closes the viewer and H hands back, on the open computer or the fleet card with the keyboard; those buttons show their key after their name there ("Hand Back (H)"). Not while typing in a field or confirming
- F6 moves to the toasts and back; Tab moves through their buttons; in a toast, Escape closes its details, else dismisses it (never an approval, a question or a request, which wait for an answer), else returns to the page
- Escape cancels a confirmation or closes Connect an Agent, then dismisses the newest message that can be dismissed, then goes back from a computer or Add computer to the fleet. Only on the fleet does it close the console; inside a computer or Add computer it never does, even from a text field, a button or the rename field (where it first cancels the rename)
- Tab moves in reading order: in a computer, from the list to the tabs to the tab content, then the header actions
- on an Access cell, Enter, Space or Down opens its list, Up and Down move and Enter chooses; Escape closes only the list or the confirmation attached to a cell
- F5 refreshes

Focus always shows as an accent border. The quick panel uses Up, Down or Tab to move, Enter to open, F5 to refresh and Escape to close. The summon payload `{"route":"computer","computerId":"ID"}` opens a computer; `{"route":"add"}` opens Add computer; `{"route":"share"}` opens Share This Computer; `{"route":"settings"}` opens Settings; anything else opens the wall. Closing either surface never returns control to agents.

Execution, checks, current verified completion, original required delivery and
cleanup are shown separately. Historical completion is explicitly unverified
when current proof is absent. Denied access clears selected records and caches,
including replies that arrive after denial. Evidence inspection never repeats
an effect.

Every command goes to `ibarad` over `$XDG_RUNTIME_DIR/ibara/ibarad.sock`
(directory 0700, socket 0600): one connection, newline-delimited JSON, the
request `{id, command, args}` and the reply `{id, envelope}` with the versioned
JSON envelope; previews come back as `{event: "preview", computer_id, data}`.
The service reconnects with backoff; while the socket is absent the wall is
greyed with every computer at its last known state, and one toast says ibara
isn't running here, with Start ibara. `ibarad` never interpolates
task titles or filenames into a shell. A computer's route starts with
`operator-session --computer ID`, which returns its epoch. Every other per-computer command takes
`--computer ID --epoch E` and answers `{computer_id, …, result}`: `operator-status`,
`operator-task-status`, `operator-observe` (a picture, answered by a preview event),
`operator-tasks`, `operator-task`, `operator-artifacts`, `operator-procedures`,
`operator-procedure`, `operator-task-extend`, `operator-task-revoke`,
`operator-procedure-review`, `operator-artifact-save`, `operator-access`(`-set`,
`-remove`, `-unpair`), `operator-health`, `operator-logs`, `operator-power`,
`operator-repair FIX`, `operator-control --op pause|resume|take_control|handback`,
`operator-files`, `operator-file-send`, `operator-file-receive`, `operator-file-resume`,
`open-viewer`, `operator-windows` (`{workspaces, agent}`), `operator-window-close --address A --pid P`
(`{closed}`), `operator-window-move --address A --pid P --workspace N` (`{moved, workspace}`),
`operator-answer-attention REF approve|deny|always` (an approval), `REF --answer TEXT` or `REF --dismiss` (an agent's question) and `operator-settings get|set|reset`.
This console's own: `directory`, `fleet-attention`, `settings get|set|reset`,
`theme-fleet`, `wake ID`, `away`, `away-seen`, `whats-new`, `whats-new-seen`,
`open-terminal --computer ID`, `pick-file`, `pick-folder`, `preview-release`,
`unattended-boot`, `unattended-boot-lock on|off`, and for Live Video
`video open ID --width W --height H` (`{path, state}` or `{unsupported}`) and `video close ID`.
Login sharing uses this console's own commands: `login-settings` (whether this computer shares,
its browsers and profiles, the All Computers rules, each computer's standing, whether this
computer also runs agents, and `rejected`, the sites that rejected a shared login lately),
`login-on --browser B --profile P [--replace]` (refused with `ANOTHER_SOURCE` and the sharing
computer's `label` while another computer shares), `login-not-now`, `login-off`,
`login-rule --computer ID|all --site S --rule allow|ask|deny`, `login-rows --computer ID`,
`login-answer --computer ID --att REF --decisions JSON [--retry]` (each site `share`,
`share_all`, `decline` or `never`; each site's outcome is `shared`, `declined`, `denied`,
`signed_out_there`, `waiting_for_browser`, `sharing_off` or `unknown`),
`login-share-with --site S --to ID[,ID…]|all` (`{delivered, deferred, older, unknown}`, lists of computer IDs),
`login-remove --computer ID --site S` (`{removed: N}` when done; `{removed: null, deferred: true}` when queued; both `removed: N` and `deferred: true` when part is queued)
and `login-sync [--dry-run]` (`{sites, computers, signed_out, delivered, deferred, older, unknown}`). `login-off` answers `{enabled, extension_removed, extension_error?}`; `login-rows` rows carry `last_result` (`worked`, `site_rejected` or null) and `last_result_at_ms`; `login-settings.computers[ID].source_elsewhere` names the sharing computer a computer takes logins from when it isn't this one. A login request is an
item of kind `login` in `fleet-attention`; generic answers refuse it.
`rename-computer --computer ID --label TEXT` trims the name, refuses empty,
over-128-character or multi-line names, renames the computer in the private
directory and returns `data.computer`, the renamed directory row.
`remove-computer --computer ID` takes the computer out of the private directory
with its pinned host key and returns `data.removed` (`{computer_id, label}`).
Adding computers and connecting agents use `tailnet` (each computer's `changed` is true for an added computer whose host key is no longer the pinned one), `pair-start NODE [INVITE_CODE]`,
`pair-status ID`, `pair-cancel ID`, `pair-requests`, `pair-answer ID accept|decline`,
`invites`, `invite-create LEVEL LASTS`, `invite-revoke ID`,
`connect-prompt` (the prompt that connects an agent). These and the
everyday commands (approvals, Fix It, power, settings, theme) run beside other
actions, so none waits for Take Control or a file transfer. Shapes are in
`tests/dev-fixture-daemon.mjs`.

IPC target `io.zet.ibara`: `refresh`, `status`, `previewStats`. Shell
summon/hide/toggle uses the same plugin id; the payload `{"route":"settings"}`
opens Settings.

Default polling: 15s hidden, 5s while the panel is open, immediate on open and
after mutations, backoff to 60s when offline; approvals every 10 s. While the
panel is open, each computer in view where an agent works, a person has control,
agents are paused or something needs you re-reads its status about every second
(at most eight a second, and for 10 s after it settles, so Done shows at once),
and so does the open computer while it moves; the rest re-read every 20 s (at most
four a second), the open computer and ones you control every 5 s. While a
computer in view has an agent working or something waiting for you (and for 10 s
after), approvals and questions are read every 1.5 s, as `fleet-attention --fresh ID …`
naming those computers so ibarad reads them now rather than answer from its last read.

## Settings

This console's settings live in ibara (`settings`, stored in `~/.config/ibara/settings.toml`
`[console]`) and show on the Settings page: Ask before agents send, spend or delete,
notifications, approval notifications, the download folder and the fleet's picture
interval. **Ask before agents send, spend or delete** (on by default) is for every
computer this console may manage: off, agents send, spend money and delete without
asking you, except on a computer whose own Settings tab says On and for an agent set to
ask under Access. The change reaches each computer with the next approvals read, which
the console makes at once, and a computer that is off or added later gets it when it
answers. Denied permissions, Administer and Take Control stay as they are. Its message
says what off means. Under This Computer, **Start
without the disk password** (off by default) shows whether this computer's disk
unlocks by itself at start (`unattended-boot`); switching it runs
`ibara unattended-boot enable` or `disable` in a floating terminal, because it
needs your password (sudo) and the disk password, and the switch follows once the
change is made. It shows only on a computer whose disk is encrypted. Its help says
the trade-off: the TPM chip unlocks the disk, so anyone who has the whole computer
can get at your files, even with Secure Boot on. When Omarchy signs you in
automatically, **Lock the screen at sign-in** shows below it (`unattended-boot-lock`,
a hook in `~/.config/omarchy/hooks/post-boot.d/`), and starting without the disk
password can be turned on only while that lock is on. Each
computer's own settings are on its Settings tab. The bar widget's inline `shell.json` values are only the
refresh intervals (`openRefreshSec`, `closedRefreshSec`). Credentials never belong here.

Under Logins, **Let agents use your logins** makes this the computer your logins come from,
with the browser and profile they come from and the All Computers rules under it (see Logins
above). Its settings live in ibara (`~/.local/state/ibara/login-sharing.json`) and never hold
a login itself; the console only reads and changes them through the `login-` commands.

### Live Video (Preview)

**Live Video (Preview)** (off by default, under Fleet) is new and not yet stable, and uses
more memory and bandwidth. When it is on, a fleet card in view and the open Screen tab play
their computer's screen as video instead of pictures, on computers whose status says
`video.capable`; under the switch, each computer that can't stream says why. The picture
stays underneath as the poster, the video shows only once its frames move, and the agent's
ripples, Done and the offline fade stay drawn over it. While video plays, that computer's
pictures come every 30 s.

Each card showing video sends `video open` at once and every 5 s (ibarad closes a stream
nobody renews within 10 s) and `video close` when the last card for that computer hides or
the switch goes off. ibarad writes the computer's H.264 stream (MPEG-TS) into the pipe it
names, inside its private previews folder, and QtMultimedia plays it (`LiveVideoPlayer.qml`,
loaded by `LiveVideo.qml`). When the stream ends the player opens the pipe again. Video never
shows an error: a player error, a refusal or a minute without frames puts the pictures back and
tries again a minute later, and frames that stop for 6 s hide the video until they move again.

Live Video needs `qt6-multimedia` and `qt6-multimedia-ffmpeg` on this computer, and
`wf-recorder` with hardware H.264 encoding on each computer it plays. Without QtMultimedia the
plugin still loads; the cards keep their pictures and Settings says what to install.

## Installed from the marketplace

Installed from Omarchy's plugin marketplace instead, the plugin comes first.
Its console then shows one button in the middle of the page: Install ibara
runs the installer above in a terminal (Finish Setting Up runs `ibara setup`
when only that is left). Setup replaces the marketplace copy with the one in
the package (the copy is kept as `.io.zet.ibara.marketplace-*` beside it), so
the plugin always matches the installed ibara. The install command comes from
`ibaraBaseUrl` in `Service.qml`: the downloads of the newest GitHub release of
MayberryDT/ibara, the same address the core's `packaging/release.env` names.

## A centered console window

Hyprland otherwise tiles Quickshell's ordinary `FloatingWindow` into a
full-screen-looking slab. For a centered, resizable console, add this narrowly
matched rule to this computer's `~/.config/hypr/hyprland.lua` after Omarchy's
defaults, then run `hyprctl reload` and check `hyprctl configerrors`:

```lua
o.window({ class = "^org.quickshell$", title = "^ibara · console$" }, { float = true, center = true })
```

The rule affects only the ibara console; it does not float the bar or other
Quickshell windows. Without it the console still works, but Hyprland tiles it.

## Human control

Take Control starts at once. It asks first only when another person holds the
computer; if ownership changes during confirmation, nothing runs. Its button
reads **Connecting…** until the viewer starts or a plain failure reason arrives.
Question and login cards share the same Take Control action. While you hold
control, a question offers **Done & Hand Back**: a single affirmative option is
sent as written, and a question with no options gets “Done”. Several options
require a choice first. An answer must succeed before Hand Back runs. The computer then pauses
its agents, ends any earlier viewer and waits until every key that viewer held
is released, starts its screen sharing and gives this console a one-time
ticket. The console opens the Viewer (`ibara-view`) with that ticket; the
Viewer checks the computer's certificate, and the computer admits only this
console's Viewer, once. There is nothing to set up first: the first Take
Control makes this console's viewer identity and tells the computer about it
over the pairing route, with no PIN and no restart.

In the Viewer, keys go to the window you are using. **Super+Alt+Escape**
switches the keyboard between the two computers; the title and a tag say where
keys go ("Keys → Tulip1" or "Keys → This Computer"). Files dropped on the
Viewer are sent to the computer's shared folder. While you have control, text
and pictures you copy on either computer can be pasted on the other; the
computer's Shared clipboard setting (its Settings tab, Take Control) turns
that off. Nothing copied is recorded.

Closing the Viewer does not hand back: its screen sharing stops, the computer
stays paused and yours, and **Open Viewer** starts it again with a new ticket.
While the viewer this console opened runs, the button reads **Close Viewer**
instead, which closes only the viewer; when you close the viewer window
yourself, the button turns back to Open Viewer within a couple of seconds.
A toast says “<Name>: you still have control”, with **Hand Back** and
**Keep Control**. Closing alone never hands back. Opening a computer, hovering
or focusing Take Control, and showing a question or login card warm its stream
at most once every 30 seconds; older cores may ignore warm without a toast.
Hand Back ends the Viewer and the sharing, waits until your keys are released,
and lets agents work again unless a person paused them (then Resume does), or
until ibara has settled the computer. It never restarts an agent's task by
itself. Only the console the computer reports as holding control sees Hand Back.

Resume clears a pause; the panel never takes an agent lease.

Restart, Shut Down, Sleep, Lock Screen, Update ibara (`operator-power --action update_ibara`) and
Update Omarchy (`--action update_omarchy`) need the Administer permission on that computer; the
computer refuses them otherwise.
Wake sends the wake signal from this computer, or asks another computer on the
same network to.

## Recovery

Collecting a task result (`operator-artifact-save`) goes over the computer's
pairing route into the download folder and never overwrites an existing file.

If the widget is empty, confirm the plugin is enabled and in the bar, then
`omarchy-shell shell rescanPlugins`. If a computer shows stale, press F5 in the
console. A locked computer reads Locked; Take Control opens the Viewer, where you unlock it.
