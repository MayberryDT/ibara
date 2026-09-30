# Changelog

Each release's notes are also in its signed manifest, and the console shows them once after an update, under What's New. Versions are `PKGVER-PKGREL`: all 3 packages (`ibara`, `ibara-stream` and `ibara-view`) always share one version.

## 0.1.0

The first public release.

### Your computers

- A console in Omarchy's bar shows every computer you added as a live picture, with who is using it and what it is doing, attention first.
- The console moves where it matters: a card rises under the pointer and stays clear of the toolbar; a computer that joins your fleet arrives on it; a pairing code shows its match on both screens when someone accepts; Hand Back lets go of your ring around the picture.
- Live pictures are light on Wi-Fi and metered networks: each crosses the network as a JPEG, and a screen that has not changed sends no new picture. Watching a still desktop in the Screen tab sent 24.5 kbit/s; a full picture goes only when something on the screen changes.
- Add Computer lists the computers on your tailnet. Your own computers add each other in one step. Someone else's computer is added when a person there accepts a 6-digit code that both screens show.
- A computer reinstalled with `ibara uninstall --delete-data` can come back: Add Computer shows it with Add Again, which pairs it the same way as the first time (your own computer at once, someone else's with the 6-digit code) and keeps its card instead of adding a second one. Its new SSH host key is never trusted without that. Remove Computer, in a card's ⋯ menu and on the System tab, takes a computer off your fleet, with its pinned host key; Add Computer can add it again. It does not change access on that computer: remove the pairing on its Access tab for that. When the reinstalled computer adds your other computers again, it keeps its old name and account on each of them, instead of a second one such as `desk-2`, also where its first pairing is from before a computer's pairings recorded its Tailscale node.
- Share This Computer lets a friend use a computer for an hour, a day, a week or until you revoke it, with a single-use invite code: Watch, Use with Approval or Take Control.
- Each computer's page has Screen, Activity, Files, Access, System and Settings tabs.
- Take Control opens a viewer with the keyboard, mouse and a shared clipboard. Super+Alt+Escape moves the keyboard between the 2 computers, and files dropped on the viewer are sent. Hand Back lets agents work again unless a person paused them, and never restarts an agent's task by itself; the console says which.
- When ibara restarts on a computer whose Resume agents after a restart setting is off, the console says so and offers Resume, instead of saying the agents resume by themselves.
- Send files by dropping them on a computer, and collect what agents made. A file up to the other computer's limit (250 MB unless changed, and never more than 500 MB through the console) is reported as sent once it arrives whole, however long that computer takes to check it. A bigger file is refused before anything is sent, with its size and the limit. Sizes read in MB and GB counted from the bytes (1 MB is 1,000,000 bytes), the same in the console and in its messages.
- Wake, restart, shut down, sleep and lock computers, and apply your Omarchy theme to every computer at once.
- While you were away shows one line of news per computer when you open the console.
- Starting without the disk password is available on encrypted Omarchy computers, off unless you turn it on.
- A computer agents use never locks them out by itself: ibara turns on Omarchy's Stay Awake when it starts, after setup and after each update, so the screensaver and idle lock no longer start. It stays on after an agent finishes. If you turn it off in Omarchy, ibara turns it on again the next time it starts.
- A locked computer says Locked in the console instead of looking offline, and Take Control works on it: type your password in the viewer to unlock it. Agents still wait until it is unlocked.

### Agents

- Any agent that can use MCP servers connects itself from one prompt: Copy Prompt in Connect an Agent, or `ibara prompt`. The agent adds the `ibara` server, links the ibara skill (installed at `/usr/share/ibara/skills/ibara`, updated with ibara) and adds a short note to its instructions file, so each session knows when to use ibara and how to work well with it.
- 11 tools let an agent begin a task, observe the screen, act on it, use web pages, run commands, move files, wait and finish with an honest account.
- Every step is written down before it runs. A step ibara cannot prove is reported as unknown, never as done.
- An agent is known by a short name and the computer it came from: Codex is `codex@laptop` and Claude Code is `claude@laptop`, instead of their programs' long names. Permissions and tasks an agent had under the long name carry over when ibara updates.
- Resending a request with the same `request_id` and arguments returns the first result and never does it twice, even from a new session. In a new session, an agent can use an id it used before for a new request.
- By default, sending, spending, deleting and access changes ask a person first. An approval covers only the exact step it was given for.
- An approval reads as plain words: who asks, what it wants to do, in which app and on which page or window, why it asks and the task it is for. The exact request is under Details.
- Approvals can be turned off for your own computers' agents: Ask before agents send, spend or delete in Settings (for every computer you manage), on a computer's Settings tab (for that computer) and for one agent in Access. Agents from someone else's computer, such as a friend's you shared with, keep asking. Always Allow on an approval lets that agent do that kind of step on that computer without asking again, for as long as its computer may run agent tasks there; it never overrides Ask First set for the agent's computer. An agent told it need not ask asks ibara, which asks you once (Allow or Not Now); an agent can never change its own rules. Denied steps stay denied, Ask First set for one agent stays, and a step you refused still asks.
- A step waiting for approval cannot be sent again as a new request under a milder effect, however its folder is written or however many questions the agent asks meanwhile. Once a person has answered, the same step asks again each time it is sent, so a denied step never runs without a person and never locks up the window it was in. ibara itself counts a web form's submit button, and Return in a form field, as sending.
- A web page step held for approval still runs once approved, however long the person takes, as long as the page still shows the same element.
- An agent's question shows in the console, where a person answers it with one of the agent's choices, or dismisses it. A question or approval ends with its task's control, so one from an agent that went away no longer waits forever.
- A step waiting for approval tells the agent, in its reply, to wait for the person's answer and then send the same request again, and not to end its turn or ask its own user. Sent again once approved, the step runs once.
- An agent sends a file only to the computer it works from, named by its name or id; any other computer, a destination that is not a full path, or a missing file is refused at once, before anyone is asked to approve it, and the refusal names the computer a send can reach. The send's reply, the file's status and the task's status say that no bytes have moved yet and give the exact `ibara client … fetch` command that moves them; the delivery counts only once that runs or a person saves the file from the console.
- While an agent works you see only its own cursor, never yours beside it. Your own pointer comes back the moment you move the mouse, and the agent's input stops.
- An agent's cursor goes to the text box it is about to type into when that box's place on the screen is known, and shows Cua's typing and key animations while it types or presses keys. Typing and keys never move your pointer, the keyboard focus or the order of windows.
- Agents can type accented letters and other alphabets into web forms. ibara pastes the text and puts your clipboard back, and never reads a password manager's clipboard.
- Agents can use app menus, and Escape closes an open menu.
- Agents can use an app's own popups, such as the rename box Files opens on F2: typing, Return, Escape and clicks on the popup reach it, where before every input was refused until a person closed it. This takes effect at the computer's next sign-in after the update. While another app's popup, a launcher or a drag holds the keyboard and mouse, the agent is told to click items by their accessibility action instead, not to press Escape, which would be refused too.
- An agent's click on a point lands where the picture it read the point from shows it, however many steps came after. When the agent has no picture yet, or the window moved or changed size since, nothing is clicked and the agent is told to take a new picture.
- Agents can click and double-click items that apps offer no accessibility action for, such as a folder in Files or Mousepad's text: ibara clicks the middle of the item with the pointer, as a click on a point. Double and right clicks on an item always go that way. Double-clicking a folder in Files opens it, where before the outcome was unknown.
- Typing goes on when a display is plugged in or unplugged part way through, and a step that stops says exactly how many characters arrived.
- Apps an agent opens keep running, with your unsaved work, when ibara restarts or updates. Finishing a task closes only the windows it opened that you did not use.
- After a network drop, an agent picks up its task as soon as it is back.
- For a few seconds after ibara starts or restarts, an agent is told ibara is starting and to try again, never that a person has the computer. That includes the moment before ibara answers at all: nothing was sent, so the agent may send the same call again. A step cut off by a restart says ibara restarted.
- An agent can check a web page's address even when the browser is not open yet at the start of a task. Status says "no browser open yet" when the browser is simply closed.
- Agents can click, point and type into web pages on a computer with no mouse plugged in. Your own mouse, and a person in Take Control, still take over the moment they move.
- When a way of acting is refused, the agent is told what it can use instead. A web page element that doesn't take an action, such as a click on a text box, says which actions it takes, and for a text box how to type into it.
- Agents can open the file manager (Files) as well as the text editor, terminal and browser.
- Agents can name files, and the folder a command runs in, by their full path: in the task's folder, or anywhere in your home folder except ibara's own folders (its data, settings and keys, `~/.ssh`, and what its services and the page reader start with) and other people's home folders. A path outside those is refused with the folders an agent may use. File checks follow the same rule.
- An agent that also assesses a check ibara verifies itself still finishes in one call: ibara's own result stands, and the reply says the assessment was ignored. When ibara cannot read what the check names, such as text in a terminal, which has no accessibility tree, the agent's assessment decides it instead, and the reply says why, so a finished task can be complete.
- A check that text is on screen finds text anywhere in a web page, read through ibara's page reader, not only what the browser's accessibility tree shows; in other apps it searches every element, not the first 60. A web page address check whose page reader does not answer reconnects it and asks again before saying it could not tell.
- Agents can read the elements of long web pages, such as a Wikipedia article or a GitHub repository page. The page reader took 33 seconds on the Ulm Minster article on a small computer, well past its 6-second limit, and turned every request away until it finished, so the agent was told no tab was available. It now takes about 0.3 seconds there and gives the same elements. The browser installs the new page reader when it next starts.
- The editor agents open is Mousepad with your own settings, so what an agent reads with `gsettings` is what the editor uses. ibara turns off Mousepad's session restore, which also turns off its autosave, so a restore prompt never takes an agent's typing.
- Every ibara command that names a computer takes the same names: the name you gave it, its host, or the id agents see (`cmp_…`) or the one `ibara client --list-computers` shows (`computer_…`). An agent can collect a file with `ibara client --computer Desk fetch …`. A name that fits no computer, or more than one, is refused with each computer's name and ids.

### Installing and updating

- One-line install from GitHub, checked against the release signature and each package's digest.
- `ibara update`, `ibara rollback` and `ibara uninstall`, with the journal backed up before each update.
- When setup, an update or a rollback restarts Omarchy's shell to load a changed bar icon, ibara makes sure the bar comes back: if the new shell closes right away because the old one was slow to exit, ibara starts it again, and it says so plainly if the bar still does not come back.
- `ibara --help` and `ibara help` list the commands a person runs and exit 0, and `ibara --version` prints the installed version. Commands ibara starts itself stay out of the list; `ibara agent-entry --help` and `ibara chrome-host --help` say so, `ibara admin --help` says `ibara admin`, and `ibara control`, which this version does not use, says so.
- `ibara rollback` with no earlier release kept says so in one line, before asking for your password.
- `ibara uninstall` also removes setup's `ibara` and `ibarad` links in `~/.local/bin`, and with `--delete-data` the viewer's settings and cache; it names the 3 packages it removes and says `cua-driver-bin` stays.
- A first install shows only setup's own output: the package no longer says to run `ibara setup` while the installer is about to run it.
- No Node or Python at run time.
