# AI Lab paired research runtime

This fork turns the Codex TUI into the runtime shell for the AI Lab. The change is runtime-only; it does not alter the EGRU model or its experiments.

## MAIN and SIDE

Every new or resumed primary conversation is paired with a durable SIDE conversation. The panes are displayed next to each other when the terminal is at least 40 columns wide and 8 rows tall.

- MAIN and SIDE have different UUIDv7 thread IDs, rollout histories, runtime state, drafts, and transcript view state.
- The SIDE records the MAIN UUID only as pairing metadata. Closing Codex or returning focus to MAIN does not delete SIDE.
- New SIDE threads carry a persisted `ai_lab_side:<MAIN_UUID>` thread-source marker. Older SIDE threads can still be recovered from their generated `AI Lab SIDE · parent <UUID>` title.
- Renaming a newly created SIDE does not break pairing because restoration uses its source marker and fork-parent UUID.

SIDE is a fork so it initially receives MAIN as reference context, but everything after the side boundary is independently persisted in SIDE's own rollout.

## Interaction

- `Ctrl+h` or `Ctrl+k`: focus MAIN.
- `Ctrl+l` or `Ctrl+j`: focus SIDE.
- `/side`: focus the existing SIDE, or create it if the automatic setup was deferred until MAIN had its first persisted message.
- Keyboard scrolling and composer input apply only to the focused pane. Each pane restores its own transcript position when focus changes.
- `Ctrl+C` from an idle SIDE returns focus to MAIN without deleting or unsubscribing SIDE.

The runtime injects the newest completed MAIN user or assistant message into SIDE as hidden developer context. It also refreshes that context when a persisted SIDE is restored.

SIDE may advise or instruct MAIN through `send_message_to_thread` using the paired MAIN UUID. Delivery is explicit and does not merge the two histories.

## Research provenance

Results that use this behavior depend on a **runtime change**. They should not be attributed to an EGRU model or architecture change unless the experiment also modifies files under the model program.
