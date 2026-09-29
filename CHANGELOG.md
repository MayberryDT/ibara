# Changelog

Each release's notes are also in its signed manifest, and the console shows them once after an update, under What's New. Versions are `PKGVER-PKGREL`: all 3 packages (`ibara`, `ibara-stream` and `ibara-view`) always share one version.

## 0.1.0

The first public release.

### Your computers

- A console in Omarchy's bar shows every computer you added as a live picture, with who is using it and what it is doing, attention first.
- Live pictures are light on Wi-Fi and metered networks: each crosses the network as a JPEG, and a screen that has not changed sends no new picture. Watching a still desktop in the Screen tab sent 24.5 kbit/s; a full picture goes only when something on the screen changes.
- Add Computer lists the computers on your tailnet. Your own computers add each other in one step. Someone else's computer is added when a person there accepts a 6-digit code that both screens show.
- Share This Computer lets a friend use a computer for an hour, a day, a week or until you revoke it, with a single-use invite code: Watch, Use with Approval or Take Control.
- Each computer's page has Screen, Activity, Files, Access, System and Settings tabs.
- Take Control opens a viewer with the keyboard, mouse and a shared clipboard. Super+Alt+Escape moves the keyboard between the 2 computers, and files dropped on the viewer are sent. Hand Back never restarts an agent by itself.
- Send files by dropping them on a computer, and collect what agents made. A file up to the other computer's limit (250 MB unless changed, and never more than 500 MB through the console) is reported as sent once it arrives whole, however long that computer takes to check it. A bigger file is refused before anything is sent, with its size and the limit. Sizes read in MB and GB counted from the bytes (1 MB is 1,000,000 bytes), the same in the console and in its messages.
- Wake, restart, shut down, sleep and lock computers, and apply your Omarchy theme to every computer at once.
- While you were away shows one line of news per computer when you open the console.
- Starting without the disk password is available on encrypted Omarchy computers, off unless you turn it on.

### Agents

- Any agent that can use MCP servers connects itself from one prompt: Copy Prompt in Connect an Agent, or `ibara prompt`. The agent adds the `ibara` server, links the ibara skill (installed at `/usr/share/ibara/skills/ibara`, updated with ibara) and adds a short note to its instructions file, so each session knows when to use ibara and how to work well with it.
- 11 tools let an agent begin a task, observe the screen, act on it, use web pages, run commands, move files, wait and finish with an honest account.
- Every step is written down before it runs. A step ibara cannot prove is reported as unknown, never as done.
- By default, sending, spending, deleting and access changes ask a person first. An approval covers only the exact step it was given for.
- An approval reads as plain words: who asks, what it wants to do, in which app and on which page or window, why it asks and the task it is for. The exact request is under Details.
- Approvals can be turned off for your own computers' agents: Ask before agents send, spend or delete in Settings (for every computer you manage), on a computer's Settings tab (for that computer) and for one agent in Access. Agents from someone else's computer, such as a friend's you shared with, keep asking. Always Allow on an approval lets that agent do that kind of step on that computer without asking again, for as long as its computer may run agent tasks there; it never overrides Ask First set for the agent's computer. An agent told it need not ask asks ibara, which asks you once (Allow or Not Now); an agent can never change its own rules. Denied steps stay denied, Ask First set for one agent stays, and a step you refused still asks.
- A step waiting for approval cannot be sent again as a new request under a milder effect, however its folder is written or however many questions the agent asks meanwhile. Once a person has answered, the same step asks again each time it is sent, so a denied step never runs without a person and never locks up the window it was in. ibara itself counts a web form's submit button, and Return in a form field, as sending.
- A web page step held for approval still runs once approved, however long the person takes, as long as the page still shows the same element.
- An agent's question shows in the console, where a person answers it with one of the agent's choices, or dismisses it. A question or approval ends with its task's control, so one from an agent that went away no longer waits forever.
- A step waiting for approval tells the agent, in its reply, to wait for the person's answer and then send the same request again, and not to end its turn or ask its own user. Sent again once approved, the step runs once.
- An agent sends a file only to the computer it works from, named by its name or id; any other computer, a destination that is not a full path, or a missing file is refused at once, before anyone is asked to approve it, and the refusal names the computer a send can reach. The send's reply, the file's status and the task's status say that no bytes have moved yet and give the exact `ibara client … fetch` command that moves them; the delivery counts only once that runs or a person saves the file from the console.
- While an agent works you see only its cursor, with its name beside it. Your own pointer comes back the moment you move the mouse, and the agent's input stops.
- An agent's cursor goes to the text box it is about to type into when that box's place on the screen is known, and shows Cua's typing and key animations while it types or presses keys. Typing and keys never move your pointer, the keyboard focus or the order of windows.
- Agents can type accented letters and other alphabets into web forms. ibara pastes the text and puts your clipboard back, and never reads a password manager's clipboard.
- Agents can use app menus, and Escape closes an open menu.
- An agent's click on a point lands where the picture it read the point from shows it, however many steps came after. When the agent has no picture yet, or the window moved or changed size since, nothing is clicked and the agent is told to take a new picture.
- Typing goes on when a display is plugged in or unplugged part way through, and a step that stops says exactly how many characters arrived.
- Apps an agent opens keep running, with your unsaved work, when ibara restarts or updates. Finishing a task closes only the windows it opened that you did not use.
- After a network drop, an agent picks up its task as soon as it is back.
- For a few seconds after ibara starts or restarts, an agent is told ibara is starting and to try again, never that a person has the computer. That includes the moment before ibara answers at all: nothing was sent, so the agent may send the same call again. A step cut off by a restart says ibara restarted.
- An agent can check a web page's address even when the browser is not open yet at the start of a task. Status says "no browser open yet" when the browser is simply closed.
- Agents can click, point and type into web pages on a computer with no mouse plugged in. Your own mouse, and a person in Take Control, still take over the moment they move.
- When a way of acting is refused, the agent is told what it can use instead.
- Agents can open the file manager (Files) as well as the text editor, terminal and browser.
- Agents can name files, and the folder a command runs in, by their full path: in the task's folder, or anywhere in your home folder except ibara's own folders (its data, settings and keys, `~/.ssh`, and what its services and the page reader start with) and other people's home folders. A path outside those is refused with the folders an agent may use. File checks follow the same rule.
- An agent that also assesses a check ibara verifies itself still finishes in one call: ibara's own result stands, and the reply says the assessment was ignored.
- Every ibara command that names a computer takes the same names: the name you gave it, its host, or the id agents see (`cmp_…`) or the one `ibara-client --list-computers` shows (`computer_…`). An agent can collect a file with `ibara-client --computer Desk fetch …`. A name that fits no computer, or more than one, is refused with each computer's name and ids.

### Installing and updating

- One-line install from GitHub, checked against the release signature and each package's digest.
- `ibara update`, `ibara rollback` and `ibara uninstall`, with the journal backed up before each update.
- When setup, an update or a rollback restarts Omarchy's shell to load a changed bar icon, ibara makes sure the bar comes back: if the new shell closes right away because the old one was slow to exit, ibara starts it again, and it says so plainly if the bar still does not come back.
- No Node or Python at run time.
