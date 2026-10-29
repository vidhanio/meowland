# AGENTS.md

Meowland is a Rust Wayland compositor that displays graphical apps in terminal panes using the Kitty graphics protocol.

Tests are allowed, but never tautological. Every test must have a purpose: it guards a real invariant, edge case, regression, or contract that could plausibly break. Do not write tests that restate constants, mirror the implementation, or only exercise trivial accessors. Before adding a test, be able to say in one sentence what bug it would catch.
Do not add comments unless explicitly asked. Documentation comments are allowed and encouraged.
Do not update README.md unless explicitly asked.

As a general guide, consider splitting a crate into smaller, focused crates when it grows to around 1,000 lines. This is not a hard limit; prioritize cohesive responsibilities and clean interfaces over line counts.
