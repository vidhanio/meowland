# Handoff

Status of the TODO backlog work as of main after the testing-backlog merges and the
treefmt commit. Read `TODO.md` for the backlog and `README.md` for the architecture
diagrams.

## State of main

- 16 crates, about 21.4k lines of Rust, plus the `fuzz/` crate outside the workspace.
  fmt, workspace Clippy (pedantic and nursery, `-D warnings`), all tests, docs,
  `benchmarks/smoke.py`, and `nix flake check` pass.
- Open items in `TODO.md`:
  - **Damage-limited DMA-BUF readback:** blocked on Smithay's mapping API.
  - **Repo hygiene:** owner only.
- Decided since the previous handoff:
  - **Shared-frame slots dropped.** The `todo/terminal-5` branch and its worktrees
    are deleted. Panes keep one-shot `t=s` objects. Reviving the idea needs reply
    identities that are never reused across pane processes; Kitty `a=q`/`a=d` cannot
    act as barriers.
- Added since the previous handoff:
  - `crates/meowland-render/tests/incremental.rs`: damage-limited recomposition
    against full recomposition with real `wl_surface` commits.
  - `crates/meowland/tests/server_errors.rs`: a process-level test that the Wayland
    bind failure cause prints once.
  - `fuzz/`: cargo-fuzz targets (`input_parser`, `input_csi`, `input_mouse`,
    `probe_parser`, `clipboard_packet`) through a `fuzzing` feature in
    `meowland-input` and `meowland-terminal`. Each ran 60 s without crashes. Not in
    CI; run with `nix shell nixpkgs#cargo-fuzz -c cargo fuzz run <target>`.

## Workflow

Coordinator splits work into briefs. Each implementer works in its own git worktree
on a `todo/*` branch based on `main`. Branches merge with `--no-ff`, followed by fmt,
Clippy, tests, docs, the smoke test, and `nix flake check` (treefmt included) on
`main`. Every agent follows `AGENTS.md` (purposeful tests only; no comments except
docs and `// SAFETY:`; no README edits) and the `rust-best-practices` guidance.

## Leftovers

- Detached benchmark worktrees under `/tmp/meowland-calloop-benchmarks-*` predate
  this work; remove them with `git worktree remove <path>` if no longer needed.
