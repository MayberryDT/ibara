# Troubleshooting

Start with the console. ibara repairs most problems by itself, and when it cannot, the computer's page says so in one sentence with Fix It. This page covers what to check when that is not enough.

## First checks

1. Run `ibara setup` again on the computer that misbehaves. It changes only what is missing or wrong, and names any step that fails.
2. Check that Tailscale is running and signed in: `tailscale status`.
3. Check ibara's services:

   ```bash
   systemctl --user status agent-computer.service ibara-operator.service
   systemctl status ibara-agent-sshd.service
   ```

4. Read the logs. A computer's System tab in the console shows them. On the computer itself:

   ```bash
   journalctl --user -u agent-computer.service -n 100    # this computer, for agents and other consoles
   journalctl --user -u ibara-operator.service -n 100    # this computer's console
   journalctl -u ibara-agent-sshd.service -n 50          # the agent entry
   ```

## The bar icon is missing or the console is empty

- Run `ibara setup` again. It links the plugin into Omarchy, enables it and restarts the shell once if needed.
- If the whole bar is gone after `ibara setup`, `ibara update` or `ibara rollback` (it says Omarchy's shell did not come back), run `omarchy restart shell`.
- If the plugin is enabled but not showing, run `omarchy-shell shell rescanPlugins`.
- If the console says ibara is not running, start the console service: `systemctl --user restart ibara-operator.service`.

## A computer does not appear in Add Computer

Add Computer lists the computers on your tailnet and asks each one whether ibara is ready there.

- Not in the list: the computer is not on your tailnet, or Tailscale is off there. Run `tailscale status` on both.
- "ibara isn't installed there": install ibara on that computer. Add Computer shows the install command.
- "Offline · Turn it on to add it" or "ibara didn't answer": the computer is asleep or off, or its service is not answering. Check `agent-computer.service` there.
- Someone else's computer waits for a person to accept the 6-digit code on their screen. If nobody accepts it within 5 minutes, ask again.

A computer with no screen, or one you reach only over SSH, can accept requests with `ibara join`.

## A computer shows as offline or not answering

- Check that it is on and awake. The console's Wake works if the computer had Wake from the network on before it slept, and this computer or another of your computers on its network can send the wake signal.
- Check Tailscale on both computers.
- If the computer was reinstalled with `--delete-data`, its identity changed, so this console no longer trusts it. Open Add Computer: it shows that computer with **Add Again**. Choosing it pairs the computer the same way as the first time, on the same card. You can also remove it from your fleet with **Remove Computer** (its card's ⋯ menu, or its System tab) and add it again as a new computer.

## An agent cannot see my computers

1. Ask the agent to call its `computer_status` tool. If the agent has no `ibara` tools, it did not add the server: paste the prompt from Connect an Agent again, or print it with `ibara prompt`.
2. Some agents load new MCP servers only after a restart. Restart the agent.
3. `computer_status` lists every computer you added in this computer's console. If a computer is missing, add it in the console first.
4. An agent works on a computer only with the Agent Tasks permission. Check the computer's Access tab.

## An agent's clicks or keys are refused

The agent is told why. The common reasons are:

- a person is using the mouse or keyboard: the agent's input waits until the mouse is still, then is refused if the person carries on
- the window lost the keyboard focus, or a different window came to the front
- a menu or dialog holds the keyboard: the agent should act on its items, or close the menu with Escape
- the computer is locked: a person has to unlock it, at the computer or through Take Control, which works on a locked screen. ibara turns on Omarchy's Stay Awake on every computer it runs on, so the screensaver and idle lock do not start by themselves; a computer locks only when someone locks it

A computer with no mouse plugged in is not a reason: agents click and type there too, in apps and in web pages.

If every input is refused after a Hyprland update, Cua's Hyprland plugin may not match the new Hyprland yet. The package rebuilds it after each Hyprland update, and Hyprland loads the new build at your next sign-in. Sign out and back in.

## Take Control does not open

- The other computer needs Tailscale running, because the stream listens only on its Tailscale address.
- The viewer opens about 8 to 11 seconds after you choose Take Control (we measured a median of 8.2 seconds). If it does not, check the computer's page for a Fix It about screen sharing.
- Closing the viewer does not hand back. Open Viewer starts it again, and Hand Back ends control.
- A computer with no screen connected gets a virtual screen. If ibara could not add one, the computer's page offers Fix It.

## A step says outcome unknown

ibara says unknown when it cannot prove that a step worked, for example when ibara restarted in the middle of the step or a person took over during it. The step may have happened. Look at the computer, or ask the agent to observe it, before doing the step again.

## Agents are held back after a problem

If earlier work did not finish stopping, the computer's page says "agents are held back" and offers Fix It, which restarts ibara on that computer. Apps agents opened keep running through the restart.

## Updates

- `ibara update --check` says whether there is a newer release.
- `ibara update` refuses a release whose signature or digests do not match. That is the protection working: do not work around it, and [report it](https://github.com/MayberryDT/ibara/issues/new/choose).
- `ibara rollback` goes back to the release before, and restores the journal from before the update when needed. With no earlier release kept in `/var/cache/ibara/packages`, it says so in one line and asks for nothing.
- `ibara --version` prints the installed version, such as `ibara 0.1.0-18`.

## Starting over

`ibara uninstall` removes the `ibara`, `ibara-stream` and `ibara-view` packages, and setup's `ibara` and `ibarad` links in `~/.local/bin`, and keeps this computer's identity, pairings and history for a later install. `ibara uninstall --delete-data` also removes them and the viewer's settings and cache (`~/.config/Ibara/ibara-view.conf`, `~/.cache/Ibara`), so other computers will need to add this one again. Files you received stay in `~/Downloads/Ibara`. `cua-driver-bin` stays installed, since other software may use it.

## Still stuck

[Open an issue](https://github.com/MayberryDT/ibara/issues/new/choose) with the ibara version (`pacman -Q ibara`), what you did, what you expected and what happened, and the relevant log lines. Remove anything private from the logs first, such as computer names, addresses and file names you would rather not share.
