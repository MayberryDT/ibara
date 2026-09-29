# Cua Hyprland plugin (vendored)

Upstream: [trycua/cua](https://github.com/trycua/cua) tag `cua-driver-rs-v0.28.2`
(`fc188250b4ca8549b8e61f937fdb1fb560770e86`), directory
`libs/cua-driver/hyprland-plugin`, MIT licence (`LICENSE.md`). Tests,
packaging and protocol notes are omitted; the build uses `-DBUILD_TESTING=OFF`.

ibara's changes are `ibara.patch`:

- **Keymaps.** The input v3 route accepts a keymap whose keys
  match plain US for everything the driver sends. That admits Omarchy's
  default `compose:caps,shift:both_capslock_cancel` and an input method's
  virtual keyboard, and it ignores a locked NumLock for keyboard actions.
  Variants, other layouts, extra groups and remapped keys still refuse, as do
  held modifiers and Caps Lock.
- **Clicks that point like a person (26 September 2026).** Upstream warps the
  real pointer onto the target and presses at once, so a page sees the pointer
  jump and click with no movement, and the named cursor was seen to lag behind
  it. A foreground click now starts where the pointer is (kept inside the
  target window), travels over 250 ms + 150 ms × log2(distance / 20 + 1)
  (350–1400 ms) along a minimum-jerk path, sending motion every frame and
  keeping pointer focus on the target root (with no button held, the
  compositor could otherwise re-pick it), rests 110 ms, and then presses and
  releases in one turn as upstream does. A press held across frames let the
  page react before the release (focus moving into a field, a list opening),
  which the focus guard then refused. The reply follows the release, within
  the driver's 3 s wait and the 5 s grant. ibara sets the named cursor's glide
  to the same time and hides Hyprland's own pointer while the agent points.
  Drags, scrolls and keys are unchanged.
- **What a click that has not pressed yet reports (27 September 2026).** The
  press point must hit the target window's own surface, checked before any
  effect as upstream does; otherwise the click is refused (`pointer_target`).
  The start point must hit it too: when the pointer rests over one of the
  window's own subsurfaces (a video, an overlay), the click starts at the press
  point instead of being refused. Until the press, the click has only focused
  the window and moved the pointer, so a failure or a revocation during travel
  or rest (the person moves the mouse, the grant expires, the window changes,
  the press point is covered on arrival) is a refusal with its cause, not
  `foreground_partial_unknown`, and a failure thrown from the frame timer keeps
  its cause instead of becoming `internal_error`. Once the press is sent, a
  failure is `foreground_partial_unknown` as before.
- **Only real pointer movement interrupts the agent (26 September 2026).**
  Upstream cancels foreground input on every Hyprland mouse-move event. Hyprland
  also sends that event when the pointer has not moved: when a popup appears
  (such as GTK's path completion list in a Save As dialog) and, with
  `follow_mouse = 1`, when a popup's grab ends. Typing into a Save As dialog
  then failed with `stale_target` part of the way through a path. A move event
  now interrupts only when it reports a position different from the last move
  event and from where the plugin itself last put the pointer (Hyprland sends no
  event for the plugin's own warps). Buttons, scrolling, keys, touch and tablet
  contact still interrupt at once.
- **A pointer on computers without a mouse (28 September 2026).** Hyprland
  offers apps a pointer, and gives pointer focus, only while some pointer
  device exists, and upstream refuses every foreground click without one
  (`foreground_physical_pointer`). The plugin now registers its own pointer
  device, `ibara-agent-pointer`, for its lifetime on every computer, mouse or
  not, and removes it before the module is unloaded. The device never sends
  an event, so it cannot move the pointer, press a button or interrupt the
  agent; a mouse, a touchpad or any other kernel pointer device (Take
  Control's passthrough pointer, `ydotool`) still interrupts it as before.
- **The window's own popup takes keys and points (29 September 2026).**
  Upstream refuses all foreground input while any seat grab exists
  (`foreground_grab`). A GTK popover or menu holds one: Files' rename box
  (F2) then refused every key, Escape included, and every click. A grab that
  accepts the target window's own surface is that window's popup (Hyprland's
  xdg-shell grab holds each grabbing popup and its parent), and is no longer
  in the way: keys go where Hyprland put the keyboard (the popup, or the
  window itself), a window focus that would take the keyboard from the popup
  is skipped, and a point on one of the window's popups under the grab goes
  to that popup in its own coordinates, as a person's would. A drag keeps the
  surface it pressed on. A grab by another client or another window, a
  window drag and a panel with exclusive input still refuse.

Build and load it with `deploy/cua-plugin.sh` on each computer. The module must
match the running Hyprland's ABI and GCC, so rebuild after every Hyprland
update; replacing a loaded module needs a new desktop session.
