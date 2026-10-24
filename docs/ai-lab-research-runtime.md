# AI Lab research runtime

This fork turns the Codex TUI into the runtime shell for the AI Lab. Its pane,
thread, and adapter orchestration are runtime changes; model integrations remain
separate and do not imply an architecture or checkpoint change.

## MAIN and SIDE

Every new or resumed primary conversation is paired with a durable SIDE conversation. The panes are displayed next to each other when the terminal is at least 48 columns wide and 10 rows tall.

The resume picker lists only primary MAIN conversations. Persistent SIDE threads are internal workspace state: after a MAIN conversation finishes resuming, its matching SIDE UUID and complete SIDE transcript are restored automatically.

Pair identity is durable in both directions. SIDE records its MAIN UUID in app-server thread metadata, while `$CODEX_HOME/ai_lab_side_pairs.json` records the inverse MAIN-to-SIDE UUID lookup. Older reverse-only pairs migrate automatically; when duplicate empty SIDE threads exist, migration selects the oldest canonical pair so prior conversation history wins. A resumable SIDE is never silently replaced merely because a resume attempt fails.

Both panes are painted in the same terminal frame. The unfocused pane remains visible and receives a live read-only projection of its persisted thread events, including streamed assistant text, reasoning summaries, plans, and tool lifecycle items. A task can therefore keep running visibly in MAIN while keyboard input is focused in SIDE, and vice versa.

The paired workspace uses inset rounded borders, an outer margin, and a center gutter so MAIN and SIDE read as independent terminal windows. On terminals smaller than 48 columns by 10 rows, the focused conversation falls back to the normal single-pane layout.

- MAIN and SIDE have different UUIDv7 thread IDs, rollout histories, runtime state, drafts, and transcript view state.
- New SIDE conversations use independent rollouts. MAIN history never becomes SIDE history; only the explicit latest-MAIN context item crosses the UUID boundary.
- The SIDE records the MAIN UUID only as pairing metadata. Closing Codex or returning focus to MAIN does not delete SIDE.
- New SIDE threads carry a persisted `ai_lab_side:<MAIN_UUID>` thread-source marker. Older SIDE threads can still be recovered from their generated `AI Lab SIDE · parent <UUID>` title.
- Renaming a newly created SIDE does not break pairing because restoration uses its source marker and the MAIN-to-SIDE registry.

SIDE starts as an independent thread. It receives only an explicit snapshot of MAIN's latest message as reference context, while every SIDE turn is independently persisted in SIDE's own rollout.

## Interaction

- `Ctrl+h` or `Ctrl+k`: focus MAIN.
- `Ctrl+l` or `Ctrl+j`: focus SIDE when INFER is closed.
- `/side`: focus the existing SIDE, or create it if the automatic setup was deferred until MAIN had its first persisted message.
- Keyboard scrolling and composer input apply only to the focused pane. Each pane restores its own transcript position when focus changes.
- `Ctrl+C` from an idle SIDE returns focus to MAIN without deleting or unsubscribing SIDE.

The runtime injects the newest completed MAIN user or assistant message into SIDE as hidden developer context. It also refreshes that context when a persisted SIDE is restored.

SIDE may advise or instruct MAIN through `send_message_to_thread` using the paired MAIN UUID. Delivery is explicit and does not merge the two histories.

## Ephemeral INFER pane

`/infer <model>` starts a registered local-model adapter and opens a rounded,
full-width chat pane below MAIN and SIDE. `/infer` alone uses the registry's
`default_model`. While INFER is open, pane focus is:

- `Ctrl+h` or `Ctrl+k`: MAIN;
- `Ctrl+l`: SIDE;
- `Ctrl+j`: INFER.

Only the focused pane accepts keyboard input and scrolling. MAIN and SIDE keep
rendering and processing work while INFER owns input.

INFER is deliberately session-local. `/close` works from the Codex composer or
inside INFER, terminates the model subprocess, and drops the entire inference
transcript and model state. Nothing from INFER is written to Codex thread
history or restored on resume.

The pane is architecture-neutral. It resolves `runtime/inference.toml` from the
lab root and communicates with the selected adapter over the documented JSONL
protocol. EGRU is the first adapter, not a runtime dependency; future models can
be added without changing TUI code. See `../INFERENCE_RUNTIME.md` in the lab
repository for the registry and protocol contract.

## Research provenance

Results that use MAIN/SIDE behavior depend on a **runtime change**. INFER itself
is also a runtime change. The Stage-20B JSONL wrapper is a **model integration
adapter change**, not an EGRU architecture or parameter change. Experimental
results must record which adapter/model/checkpoint was selected.
