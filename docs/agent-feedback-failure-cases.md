# Agent feedback of November 2026 — failure cases

Written before the repairs, 26 November 2026, from what agents reported and
what ibara recorded on three computers between 14 and 24 November. Each
problem was reproduced first on a local page (`e2e/browser/feedback.html`).

## What was reported

- **Typing stops in rich web editors.** 14 stops in 7 tasks, and 4 written
  reports: "ibara typed the first 4 of the 195 characters, then stopped".
  Cause, measured: an input method (fcitx5) sends its keyboard's keymap again
  every time a field takes the keyboard focus, Hyprland announces a layout
  change, and Cua's plugin ends every input connection on that, as it does for
  a display change. An editor that takes the focus again as it redraws (on
  every key in the reproduction; on a suggestion popup or a new line in the
  report) does this over and over. Exactly the keys Cua counts arrived, and
  typing went on from there only 3 times in one text.
- **An option inside an open list is refused as covered.** 20 refusals in 7
  tasks, and 5 written reports. The option is inside the window but outside
  the part of its own scrolling list that shows, so a press at its place
  reaches whatever is there instead; the page reader scrolls an element into
  view only when it is outside the window.
- **A wheel scroll aimed at a list scrolls the page.** In the same 5 reports.
  The point chooses only the window: Cua is given no place, and scrolls at
  the window's centre. A page scroll with an element as its target scrolls
  the element into view and then also scrolls at the window's centre.
- **Refusals that leave out what the agent needs.** `exec arguments exceed
  limits` names no limit. The refusal of text beyond ASCII typed into a
  browser window sends the agent to a field's `type`, which replaces what the
  field holds, without saying so (an agent lost 364 typed characters), and
  there is no way to add such text where the caret is. `send` refuses a wrong
  `to.host` in 4 tasks, though a send can go to one computer only.

## How the repairs could fail

Typing through a field that keeps taking the focus

1. Typing still stops after 3 interruptions although every one of them let
   keys through, so a long text into such an editor never arrives.
2. Typing goes on forever when the desktop ends the connection before any key
   every time (a display that keeps changing): it must still stop, after 3
   in a row that let nothing through, and say exactly what arrived.
3. Going on types a character twice or skips one: every character must arrive
   once, in order.
4. Going on after a stop with a key in flight (`transport_or_protocol_error`),
   where one key may or may not have arrived: that still stops, with the one
   key unsure.
5. Going on types into a window that took the keyboard focus meanwhile.
6. Going on ignores a pause, a takeover or a cancel between pieces.
7. The stop's message still blames a display that never changed, or tells an
   agent that typed into a page field only to type the whole text again
   (which replaces the field) without saying how to add only the rest.
7a. Interruptions that let no key through are counted together although
    whole pieces arrived between them, so a long text stops at the fourth
    such interruption anywhere in it.
7b. Going on after an interruption before the text's first key skips the
    pause and the check that the window still has the keyboard.

Text beyond ASCII where the caret is

8. Text beyond ASCII typed into the browser without a target lands in the
   address bar, or wherever else the keyboard is, when no page field has it:
   it must be refused before anything is sent.
9. It replaces what the field holds instead of adding at the caret.
10. A piece is pasted before the one before it arrived, or the person's
    clipboard is put back before the page read the paste, so the wrong text
    is pasted. A paste that does not show within its time is `unknown`, and
    nothing more is sent.
11. The person's clipboard is not put back afterwards, or a copy they made
    meanwhile is pasted over.
12. The refusal for such text typed with `computer_act` still does not say
    that a field's `type` replaces its text, or that `browser_act` `type`
    without a target adds at the caret.
12a. The question of whether a page field has the keyboard names no tab, so
    the page reader refuses it and every such `type` is refused though a
    field has the keyboard. (Found on a real desktop on 26 November: the
    stand-in reader had answered without a tab. It now refuses as the page
    reader does.)
12b. A paste counts as arrived because the field holds a different amount
    for another reason (the keys typed before it landing late, a character
    the page adds, someone else typing), so the next piece, or the person's
    own clipboard put back, is pasted in its place. A paste has arrived when
    the text before the caret ends with it.
12c. A paste that replaces a selection of the same length never counts as
    arrived, so the rest is dropped though it arrived.
12d. The page reader cannot answer for a moment (a frame of the page is
    loading) and that is taken for the field losing the keyboard: the type
    is refused, or it stops and puts the clipboard back before the page read
    the paste. Only an answer that no field has the keyboard stops at once;
    no answer waits until the paste's time is up.

An option its own list hides

13. An option inside the window but outside the visible part of its scrolling
    list is still refused as covered, instead of being scrolled into the list
    and clicked.
14. The list is scrolled but the point returned is not on the option (it
    moved after the point was chosen), so the click reaches another option.
15. An element truly covered (a cookie banner, a dialog) is no longer refused,
    or the refusal no longer names what covers it.
16. An element the page holds the pointer locked away from is now located.
16a. Bringing an option into view scrolls a box a person cannot scroll back
    (`overflow: hidden`), scrolls sideways a list that scrolls only down, or
    scrolls a box the element does not move with (a fixed element inside it)
    and leaves it scrolled.
16b. An option in a web component whose list is across a shadow boundary (a
    slotted option, a list outside the shadow tree) is still refused as
    covered.

Scrolling a list

17. A wheel scroll at a point over a list still goes to the window's centre,
    so the page scrolls and the list does not.
18. A wheel scroll with no point no longer scrolls the focused window as
    before.
19. A page scroll with an element as its target still scrolls at the window's
    centre, so a list given as the target does not scroll.
20. A page scroll whose target cannot be pointed at (covered, or locked away)
    fails where it worked before: it must still scroll the window as before.
21. A scroll at a point on Cua 0.29 is refused for want of a picture, as a
    click at a point was, instead of taking one and trying once more.
21a. A page scroll with an element as its target sends the wheel to another
    window that lies over the browser at that place.

Refusals

22. `exec` with too many or too long arguments still does not say the limits
    (100 arguments after the program, 4,096 characters each) or which was
    exceeded.
23. `send` without `to.host` is still refused, or goes anywhere but the
    computer the agent works from; a `to.host` that names another computer is
    no longer refused.
