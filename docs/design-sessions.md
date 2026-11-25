# Live design sessions

A live design session lets you point at your running app and say, in plain
words, what to change, then watch the agent change it. You need no design
tool and no UI/UX experience: you click on the thing, type "make this green
and a bit bigger", and the page updates a few seconds later.

How it works in one paragraph: the agent starts the app's dev server in its
worktree. Archon (archond, on the host) runs a headless Chromium that opens
that app, and streams pictures of it to the web app. Your clicks go back to
the browser as input. When you leave a note, Archon sends the agent your words
and a picture of what you pointed at; the agent edits the code and makes one
commit per note. Nothing from the app itself, no script, style or cookie,
ever reaches your browser: only pictures and JSON.

## Contents

- [Configuration](#configuration)
- [Starting a session](#starting-a-session)
- [Using the design screen](#using-the-design-screen)
- [Notes and their statuses](#notes-and-their-statuses)
- [Compare, Undo and Revise](#compare-undo-and-revise)
- [Follow-ups, references and the tree](#follow-ups-references-and-the-tree)
- [Finishing and approving](#finishing-and-approving)
- [Keyboard shortcuts](#keyboard-shortcuts)
- [Using it from a phone](#using-it-from-a-phone)
- [When something goes wrong](#when-something-goes-wrong)
- [Limits](#limits)
- [Security](#security)
- [Troubleshooting](#troubleshooting)

## Configuration

Design sessions are on by default and start only for items with `proposal = live`.
To turn them off, set `enabled = false` under `[design]` in `config.toml`.
The defaults and other options are:

```toml
[design]
enabled = true               # false: `proposal = live` is refused with a clear message
browser = ""                 # path to Chrome or Chromium; empty finds one
browser_sandbox = "required" # "required" or "off"
max_sessions = 2             # live headless browsers on this host
```

The [configuration page](configuration.md) lists each key under `[design]`.

**A browser is required.** Archon does not bundle one. It uses `[design]
browser` (an empty value means auto-detect, the same as leaving it out), else the first of `google-chrome`, `chromium` or `chromium-browser`
on `PATH`, else the newest Playwright `chrome-headless-shell` it finds.
`archon doctor` has one line for it:

```text
design preview browser: /usr/bin/google-chrome
```

When no browser is found the line instead reads "design preview browser: not
found" followed by the install hint for your system.

**The sandbox.** Chromium's own sandbox is required. If it cannot start (for
example the host restricts user namespaces), the session says so and points to
the `archon doctor` fix. Only `browser_sandbox = "off"` in the host config
allows `--no-sandbox`; the design screen then shows an **Unsandboxed preview**
badge for as long as it runs that way. The residual risk is the same one you
already accept: the app's dev server runs unsandboxed as your user.

**Preview origin.** Live preview requires a Linux daemon host with access to
the dev server’s process and socket records in `/proc`. Serve the page and
its assets through the item's own HTTP
dev server on loopback. Start it in the run worktree and record its PID in
`.archon/services/<name>.pid`. The former repository setting
`[design] allow_hosts` is unsupported. Serve fonts, styles and other external
assets locally or through the dev server.

## Starting a session

Any of these starts one; all of them set the item's `proposal` knob to `live`:

- `archon queue add --proposal live …` ([queue scripting](queue-scripting.md)).
- The **Design it live** switch in the web app's New item sheet.
- Chat: "let's design the settings page live". The orchestrator enqueues the
  item with `proposal=live`.

`live` replaces Lavish's `propose` stage with a first stage, `design`. It is no
more autonomous than `lavish`, so no autonomy ceiling that allows `lavish`
refuses it. Choose it when you create the item: changing `proposal` to or from
`live` on an existing item is refused ("choose live when creating the item").

The item card shows a **Design** chip. When the preview is ready, an inbox
entry "Design session ready" appears with an **Open** button that opens the
design screen. (Archon sends no push notifications yet; the inbox entry is the
signal.) Until then the design screen shows
"Starting your app…" and the agent's latest progress line.

## Using the design screen

Open `#/design/<item>` in the web app. The app is shown on a canvas; you can
switch the viewport between **Desktop** (1440×900), **Tablet** (834×1112) and
**Phone** (390×844, touch).

Four modes:

| Mode | What it does |
| --- | --- |
| **Interact** | The frame behaves like the app: click, scroll and type. Clicking the frame captures the keyboard ("Typing into app · Esc to stop"). |
| **Comment** | Click or tap an element to select it. **Wider** and **Narrower** select its parent or child. |
| **Region** | Drag a box instead of selecting an element. |
| **Sketch** | Draw marks on a frozen frame (pen, rectangle, arrow, text) to show what you mean. |

After you select something, the composer opens under **What the agent will
receive**. It shows exactly what will be sent and nothing else from the page:

- your words;
- the cropped picture, with a **Send picture** switch;
- an editable details block (page path, element, selector, component name, a
  few styles), with a **Send details** switch. Delete anything you do not want
  sent, for example text that shows a secret.

With both switches off the note is text only. Send with Enter, or Ctrl+Enter
for a multi-line note.

## Notes and their statuses

Each note gets an id, `D1`, `D2`, … that is never reused in the item, even
across sessions. A pin with its number stays on the frame where the element is.

| Status | Meaning |
| --- | --- |
| **Queued** | Waiting. You can edit or cancel it until it is sent. |
| **Working** | The agent has it. The agent's reply appears in the note's thread. |
| **Needs reply** | The agent asked a question. Later notes wait ("Waiting for your answer on D3"). Answer in the thread, or press **Drop note**: the agent discards its unfinished edit and the queue moves on. |
| **Done** | The agent made one commit for it, and Archon checked it. "Done, nothing to change" means no edit was needed. |
| **Failed** | See [When something goes wrong](#when-something-goes-wrong). |
| **Cancelled** | Dropped or cancelled. |
| **Undone** | Archon reversed the commit ([Undo](#compare-undo-and-revise)). |

Only one note is with the agent at a time, in order. A note still working
after 15 minutes offers **Drop note** too. **Tell agent again** resends a note
that was not delivered.

Archon accepts `done` only when HEAD is the commit the agent reported, that
commit has exactly one parent, and the parent is the commit the note started
from. The commit message carries a trailer, `Archon-Note: D3`, so
`git log --grep "Archon-Note"` maps code to notes.

## Compare, Undo and Revise

**Compare.** Open a done note for a before/after slider (side by side on wide
screens). A note drawn on a sketch (New screen) shows your sketch and the
result next to each other instead. Each picture is captioned like "captured from the preview at commit
3f9c2a1, 14:32". A preview can show uncommitted dev-server edits. Only the operator receives the live
frames. A reference can send the after picture to the design agent,
and an included final or after picture can enter the approved intent.

**Undo.** **Undo** on a done note asks "Undo D3? Archon will reverse its
change." Archon itself reverts the commit (the agent never does). It waits
behind any note the agent is working on and takes about a second. The card
shows **Undone**, the preview updates, and the note leaves what you approve.
The undo itself is listed in the history only. If later changes touch the same
code, nothing changes, the note stays Done, and the card says:

> Can't undo automatically, later changes touch the same code; add a note
> describing what to change back.

Undo is refused while finishing, on a dirty worktree, and on a note that is
already undone or in an approved intent.

**Revise.** If you ask for changes at approval, the same agent resumes in a new
session that starts live. What you asked for becomes the new session's first
note, a note on the whole page sent as soon as the app is up. The agent makes
it like any other note, so it shows in the ledger and the approval. Notes from
earlier sessions are read-only: you cannot
edit, cancel, reply to or drop them. Their changes stay in the code, and you can
undo any of them from the new session. Ids continue where they stopped. A
design screen left open follows you into the new session and shows its preview
as soon as the app is up. At **Finish** only the new session's notes are
pictured and confirmed again; earlier notes keep the picture you confirmed for
them before and still count toward what you approve, unless a later note
supersedes them or you undo them. A "superseded by" you chose at an earlier
review still holds, and its card shows it; clear it there to bring the note
back. A replacement lapses when its replacing note is undone, so the
replaced note counts again.

## Follow-ups, references and the tree

**Follow-up.** Any note, done or not, has **Follow up**. A follow-up is a new
note (its own `D<n>`, its own commit and pictures) that continues the note it
follows; nothing earlier is amended. The agent gets your words with the earlier
versions of the same chain, oldest first, and builds on the code as it is now.
A chain is rooted at its first note; its versions count 1, 2, 3 in the order
they were made. At Finish a chain counts once, with its newest version that is
still in the code.

**References.** A note or follow-up can reference up to 5 earlier notes, picked
from the list or typed as `@D3`. The agent's message gets a "Referenced
changes" block with each one's words, commit and reply, marked as data, not
instructions, and up to 3 of their after pictures. Each card and tree node
shows its references as links, including ones picked without typing the id.

**Tree.** The **Tree** tab beside **Notes** shows every note as a node with its
picture, status and short commit, grouped by chain. A follow-up of an older
version branches. Versions set aside are dimmed. Select a node to see that
version's before and after pictures under "Viewing D5 · version 2 of 3", with
**Back to current** and **Compare** with another version of the chain. This
shows pictures only: the live preview stays at the current code.

**Continue from this version.** On an older version, **Continue from this
version** undoes the chain's newer versions, newest first, with new revert
commits through Undo (never a reset), marks them **Set aside**, and queues your
follow-up on the chosen version. A newer version that is still queued or with
the agent must be dropped or finished first. If a revert conflicts, or fails
across a restart, the rest is cancelled and the card shows Undo's message; a
follow-up that waited for that revert is cancelled too, never sent onto the
code it meant to leave. A follow-up sent while the reverts still wait in line
is accepted and runs after them. A follow-up on an older version whose newer
versions are in the code with no undo waiting is refused with a pointer to
Continue, as is one on a version an undo is about to set aside.

**For the agent.** The read-only MCP tool `design_history` lists the chains of
the session (or one chain, with `id`): each version's words, commit, reply,
whether it is set aside, and the notes it references. `design_note` is unchanged.

Protocol 26 adds these behind the `welcome` feature `design_iterations`. A
protocol 25 app sees follow-ups as plain notes. Protocol 23 and 24 clients
can use the ordinary item views. Design sessions require protocol 25 or newer.

## Pictures of a change

When the agent reports a note done, archond waits for the preview to settle
before it takes the after picture: it reloads the page, waits for the load
event and for the network and the pixels to stop moving, then crops the same
element at its new box.

A change that shows only in a state of the page (a dialog a button opens, a
hovered menu) is reached with `steps` on `design_note`: `open` a path, `click`
or `hover` a CSS selector, or `wait`. With `status: "working"`, sent before the
agent edits, the steps take the note's before picture in that state on the old
code; with `done` they are replayed from a fresh load for the after picture
(the whole view), and Finish replays them for the final picture. A second
`done` for the same commit, with other `steps`, retakes the after picture of a
done note; a revert note, an undone or superseded note, a note reported with no
change, a different commit, or a session that is finishing is refused with the
reason.

Archon compares the after picture with the before picture (decoded and scaled
to one size, so a different scale or encoder is not a change). When they match,
no comparison is offered: the agent is asked to retake the picture in the right
state, and if it still matches the note shows "No visible change was captured"
with the agent's `reply` as the explanation, in the design screen and on the
gate page.

## Finishing and approving

1. **Finish** is available when no note is in progress or waiting for an
   answer, the worktree is clean, and there is something to approve: a done
   note, or a design approved in an earlier session. Otherwise it
   says why ("Finish after D3 is done"). With notes still queued it asks
   "Cancel 2 queued notes and finish?".
2. Finishing freezes the session: no new notes, edits, replies or undos until
   **Back to editing**. Interact still works. Archon takes a final picture of
   each note's element at the final commit, on a fresh load. A note on the
   whole page (a New screen sketch, say) is pictured on the page the agent
   named when it reported the note done (`path`), and otherwise on the page
   the preview shows. Where the element
   only appears after some clicks, the card says "Not found on a fresh load:
   open it in the preview and press Capture".
3. Review the cards. Each shows your words and the final picture with an
   **Include picture** switch. Notes on the same target are grouped, and the
   earlier ones are marked "Superseded by D5", which you can change. A note
   whose change is no longer in the code is listed as not included.
4. **Confirm** tells the agent to wrap up: it stops the server and completes.
   If the page, the head commit or a note changed since the capture, Confirm
   answers `stale` and the screen captures again.
5. The ledger appears with **Approve**, **Ask for changes** and **Reject**, on
   the design screen and in the inbox; `/approve` in chat and `archon answer`
   work too. They use the same proposal gate as any other proposal.

The ledger is built by Archon from its own records, never from what the agent
reports. It has one requirement `D<n>` per active note, bound to one picture:
the final picture, or the after picture captured when the note was done if
there is no final one. A picture you switched off in the review makes a text
requirement marked "(visual; picture withheld by the operator)"; only a switch
you moved counts, never a card that merely had no picture yet. Before and
in-between pictures are never part of it and never sent to a later model.
Review and acceptance then check the shipped UI against these pictures and
your words.

Each picture in the ledger records how the preview browser took it: the
viewport the page was laid out at, the page's device scale factor and mobile
emulation, which also turns touch input on (1 and off for the desktop
preview; a replay must set both, or `pointer: coarse` pages lay out
otherwise), the area shown (the whole
viewport or a region), the screenshot's own clip scale (2 for the final
pictures, less if the size budget lowered it) and its size in pixels.
Pictures never include the design overlay. The clip scale is not the page's
device scale factor: a page laid out at a factor of 2 can match CSS
resolution queries the preview did not, and lay out differently in a picture
of the same size.

The build is told to take its own picture the same way and to copy the
reference's capture into its result. Acceptance refuses a result that
declares another capture, or a built picture of another pixel size, before
any judge compares the two. Archon cannot see how a built picture was taken,
so a judge who sees a pair taken differently anyway (another crop, zoom,
viewport or overlay) fails it as a capture mismatch rather than as a gap in
the build.

The page shown at the gate is a decision page. At the top are a summary (how
many notes changed the app, the checks the agent reported, what approving
does) and the answers. Each note then has a card: your words, what you
pointed at, the before and after pictures side by side, the commit's message
and changed files, and its status (done, no change needed, failed, cancelled)
with the agent's last reply. The full brief is folded at the end. The page is
one self-contained HTML file with the pictures inline, readable on a phone.
It says so when the intent nears its 128 image cap, or when pictures were left
out of the page to keep it under 24 MiB (they stay in the intent).

If every note ended as "nothing to change", or nothing is left in force after
your "superseded by" choices, the review asks "Approve the design as it is?"
and its button reads **Approve as it is** (`confirm_no_change`). A session
reopened after its design was approved (with notes or as it is) asks the same
when no note of its own is in force, even before you add one: the earlier
approval stands.
Archon then opens the proposal gate with an empty ledger delta, and you approve
it as usual. With no active note and nothing confirmed there is nothing to
approve: add a note or cancel.

## Keyboard shortcuts

Active when focus is not inside the frame.

| Keys | Action |
| --- | --- |
| I / C / R | Interact / Comment / Region |
| Esc | Cancel selection or composer; release keyboard capture |
| [ / ] | Wider / narrower selection |
| Enter / Ctrl+Enter | Send note (single-line / multi-line) |
| J / K | Next / previous note |
| 1 / 2 / 3 | Desktop / tablet / phone preset |
| 0 | Fit ↔ actual size |
| Shift+R | Reload preview |
| ? | Shortcut sheet |

Esc is never forwarded to the app; **Send Esc to app** sends one.

## Using it from a phone

Use the web app through the same pairing as for the rest of Archon
([remote-setup.md](remote-setup.md)). Below 640 px the frame fills the width
and notes live in a bottom sheet with three heights (peek, half, full). The bar
at the bottom has Use, Comment, Region and Notes, each a 44 px tap target.
There is no hover, so tap an element and use **Wider** and **Narrower**. Pinch
zooms the picture locally and is not sent to the app; a two-finger drag scrolls
the app. The composer opens above the keyboard. Tablets (640 to 1023 px) get
the same layout with notes in a side drawer.

Notes you type while disconnected are kept on the device and sent on reconnect,
once only.

## When something goes wrong

| What you see | Meaning and what to do |
| --- | --- |
| "Archon needs Chrome or Chromium on this machine to show the preview" | No browser found. Install one as the `archon doctor` hint says, or set `[design] browser`. **Continue without preview** still sends notes (pictures come from uploads). |
| "Chromium's sandbox is unavailable on this host" | Fix as `archon doctor` says, or set `browser_sandbox = "off"` knowingly. |
| "Preview down" / "The app stopped responding. The agent has been told and is restarting it." | The dev server died or hangs: it did not answer an HTTP request within about two seconds (checked every few seconds). Wait, or use **Reload** and **Tell agent again**. The label clears when the app answers again. |
| "Reconnecting…" | The connection dropped; notes are kept and sent on reconnect. |
| "Queued for a preview slot" | `max_sessions` browsers are running. Your notes still reach the agent. |
| "The code history changed in a way Archon can't track; its edits may still show; add a note if something should change" (note **Failed: history changed**) | The agent made two commits, a merge, a reset below the base, or reported the wrong commit. Archon leaves the worktree as it is and moves on. |
| "Can't undo automatically, later changes touch the same code; …" | See Undo. |
| "Undo of D3 was interrupted; check the preview" | archond restarted during an undo and could not confirm it. |
| "D3's change is no longer in the code history" | The commit left the branch; nothing to undo. |
| "the worktree has uncommitted changes" | Undo or Finish needs a clean tree. |
| "Finish after D3 is done" / "D4 needs your answer" | Finish waits for these. |
| "Finishing: press Back to editing to change anything" | The session is frozen. |
| "Waiting for your answer on D3" | A question holds the queue. |
| "Design approved" / "Changes requested" / "Design rejected" / "Session cancelled" | The session is over and says how it ended, also when you reopen it later. Every request for it is refused as `gone`. After "Changes requested" the screen switches to the next session by itself when it opens. |
| "The agent is wrapping up. Approve, Ask for changes and Reject appear here in a moment." | You confirmed; the approval question opens once the agent has stopped the app. |
| "Reviewed in an earlier session; kept as it was" | A note from before an "Ask for changes". Its picture was confirmed then and goes into the approval as it is; it is not captured or confirmed again. |
| "A design session is running. Only the operator's devices see the live preview." | You are on an observer device. |
| "File pickers aren't available in the preview yet" | The app opened a file picker. Downloads are denied. Opening a new window ends the preview. |
| "worktree moved after Finish: expected 3f9c2a1; restore it and complete again" | A commit was made after you confirmed. The agent must reset to the finish commit and complete again. |
| "choose live when creating the item" | `proposal` cannot change to or from `live` later. |
| "design sessions turned off" | `[design] enabled` is false; the stage fails. |

`alert`, `confirm` and `prompt` dialogs show their type and action buttons in Archon. Their page-provided
text and suggested answers stay inside Chromium.

## Limits

- At most `max_sessions` live browsers (default 2).
- The live preview captures up to 4 verified frames per second. Each request
  carries up to 64 input events. The daemon accepts 60 input requests and 10
  resolves a second.
- Notes up to 4,000 characters, replies up to 2,000; 200 notes per session, including follow-ups. Each
  item can keep up to 200 MiB of pictures and sketches. Equal encoded
  images count once. New sketches count separately for each capture.
- 20 versions per chain; 5 references per note; 3 referenced pictures per
  message to the agent.
- Every frame and picture is at most 768 KiB as sent; pictures are re-encoded
  until they fit.
- The browser is capped at 1 GiB of JavaScript heap, restarts if its memory
  stays above 2 GiB for 30 seconds, and restarts on a sample of 3 GiB or more.
  After five memory restarts another excess ends the design session. For
  ordinary crashes, five automatic restarts in ten minutes exhaust the retry
  budget. Select **Reload** to retry a crashed preview.
- The stage waits up to 240 minutes in total.
- With 128 reference images allowed per item, the screen warns 8 before the
  cap on active visual notes.
- At most 4 subscriptions per connection, shared with feeds.

## Security

Live design is opt-in. The preview runs the item’s own dev server in a fresh
temporary Chromium profile with no saved credentials or extensions. The
daemon clears the child environment, uses a private debugging pipe, denies
downloads and sensitive permissions, and deletes the profile on close.
Chromium’s platform sandbox is on by default. The daemon samples browser
process-group resident memory about every three seconds. Usage can rise
above either watchdog threshold between samples.

A session proxy checks the destination and the process serving each connection
before forwarding traffic. Serve fonts, styles and other assets through the
registered dev-server origin. The daemon refuses an off-origin redirect.
The proxy gives Chromium 10 seconds to send request headers and caps an HTTP
request and response at 90 seconds total, including active transfers. Long-lived
HTTP streams such as server-sent events must reconnect after that cutoff.
Upgraded WebSocket traffic runs outside the HTTP deadline.

A page can briefly commit an off-origin document inside Chromium before
detection. The driver checks main and child documents before releasing pixels
or DOM data. It accepts child `about:blank` and `about:srcdoc` documents when
Chromium verifies their origin matches the preview. If Chromium omits the
origin from the frame report, the driver checks the frame's storage key.
The driver discards the result if navigation intervened and ends the session
on an unexpected document or popup. The off-origin content is never shown or
forwarded. The session records why the preview stopped.

Only paired operators can see the preview and act on it. Observers receive a
status summary. Live frames go to the operator. Notes can send selected
pictures and reviewed details to the stage agent. Archon labels page-derived
details and pictures as untrusted data. Git verification and undo use the
daemon’s sealed Git environment.

## Troubleshooting

- **No frames.** Run `archon doctor`; check the browser line. Check the item's
  agent started a server (`archon log`), and that `design_preview` was called.
- **The page is blank or unstyled.** The app may load assets from other hosts;
  serve those assets through the same dev server as the page.
- **Edits do not show.** After a note is done Archon reloads once if nothing
  changed in 8 seconds. Use **Reload** (Shift+R).
- **The queue is stuck.** A note in Needs reply holds it: answer or **Drop
  note**.
- **Orphan browsers.** archond reaps recorded browsers and deletes stale
  profiles on start.
