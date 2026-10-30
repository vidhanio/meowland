# Handoff

Status of the TODO backlog work as of main `2d5d748` plus the docs commit that adds this
file. Read `TODO.md` for the backlog and `README.md` for the architecture diagrams.

## State of main

- 16 crates, about 20.5k lines of Rust. Formatting, workspace Clippy (pedantic and
  nursery, `-D warnings`), all tests, docs, and `benchmarks/smoke.py` pass.
- Every item in the original backlog is merged except the open items in `TODO.md`:
  - **Reusable shared-frame slots:** unmerged branch, needs a decision; see below.
  - **Damage-limited DMA-BUF readback:** blocked on Smithay's mapping API.
  - **Tests:** an end-to-end incremental render fixture, fuzz targets, and a
    process-level CLI error test.
  - **Repo hygiene:** owner only.
- Notable behavior changes since the original inventory:
  - Keys resolve through the configured XKB layout, so non-US layouts work.
  - Shortcuts are configurable (`[shortcuts]`); the Home Manager example lists them.
  - Updates are sent as rectangles instead of full-width row bands; sparse-1080p
    p50 dropped from ~0.70 ms to ~0.2 ms, with about 9× fewer PTY bytes.
  - Oversized escape sequences fail closed instead of leaking as typed text.
  - Clipboard export follows a MIME allowlist.
  - Library crates have their own error types; the binary uses `anyhow`.

## Open decision: shared-frame slots (`todo/terminal-5`)

The branch is complete and reviewed four times, but it is not mergeable as is. The
safety invariant is that a slot file is never rewritten while the terminal may still
read it, and that requires reply identities that are never reused. Choose one:

1. **Drop it.** Keep the one-shot `t=s` objects. The measured win is ~0.45 ms of a
   ~4.6 ms full-1080p frame, about 10%.
2. **Shared allocator.** A locked, persistent, monotonic image-id allocator in
   `$XDG_RUNTIME_DIR` used by all pane processes. Scope it to one host and user, fail
   closed to objects or inline on any coordination failure, and never reset the
   transfer counter on `Resync`.
3. **Accept probabilistic safety.** Document the collision probability and threat
   model rather than claiming "never".

The fourth review found that Kitty `a=q` does not look up stored images and `a=d`
cannot remove replies already queued in the PTY, so neither can serve as a barrier.
It reproduced the cross-process collision by forcing two pane processes to equal PIDs.

## Workflow

Work was split across three roles, each with a fixed model:

| Role | Model | Responsibility |
|---|---|---|
| Coordinator | Claude Opus 5.5 | Splits the backlog into briefs, chooses designs, routes review findings, merges |
| Implementer | DeepSeek V4.1 Flash, maximum reasoning | One task per branch: implements the brief and commits |
| Reviewer | GPT-6.1 Terra, extra-high reasoning | Reviews each branch against `main`, reproduces claims, writes a verdict |

- Each task gets a fresh implementer in its own git worktree and `todo/*` branch,
  based on `main`. Small fixes after a review go back to the same implementer; larger
  rounds start a fresh one that reads the brief, the earlier reviews, and the branch
  history.
- Each review, including re-reviews, uses a fresh reviewer in a detached worktree. It
  checks commits individually, runs fmt, Clippy, tests, and the smoke test, executes
  edge cases where feasible, and checks `git merge-tree` against current `main`.
  Verdicts are approve, approve with fixes, or request changes.
- A branch merges with `--no-ff` only after approval, followed by fmt, Clippy, tests,
  and the smoke test on `main`. Non-blocking findings become small follow-up branches.
- Every agent follows `AGENTS.md` (purposeful tests only; no comments except docs and
  `// SAFETY:`; no README edits), the `rust-best-practices` guidance, and fmt- and
  Clippy-clean commits.

## Leftovers at handoff

- Worktrees `~/Projects/meowland-terminal-5` (branch `todo/terminal-5`) and
  `~/Projects/meowland-rv-terminal-5-r4` (its reviewer's checkout). Remove them once
  the slot decision is made: `git worktree remove <path>`.
- Merged `todo/*` branches remain locally and can be deleted:
  `git branch --merged main --list 'todo/*' | xargs git branch -d`.
