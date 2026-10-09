# Terminal input

Meowland requires [Kitty keyboard reporting][kitty], as well as Kitty graphics
and SGR pixel mouse reporting. There is no press-only keyboard fallback:
a terminal that cannot report releases cannot represent held Wayland keys.

## Negotiation and key lifetimes

On entering the alternate screen, the terminal guard pushes keyboard flags
`11` (`CSI > 11 u`):

- `1`: disambiguate escape codes, including Escape and modified keys;
- `2`: report press, repeat, and release event types;
- `8`: report **all** keys as escape codes, including standalone modifiers,
  Enter, Tab, Backspace, and text-producing keys.

The capability handshake queries `CSI ? u` before primary device attributes
and checks that all requested bits were accepted. An answer of `CSI ? 0 u`
means the protocol exists but the requested enhancements were not enabled;
a missing answer means unsupported. Both are rejected. The guard pops the
keyboard mode before leaving the alternate screen, including on error or
handled termination signals.

`src/terminal/keyboard.rs` translates Crossterm key identities into evdev
codes. Each physical press and release becomes one `Input::Key`. Host repeat
events are discarded: Wayland clients repeat held keys using the compositor's
`wl_keyboard.repeat_info` (25 Hz, 600 ms delay). Mixing host repeats with client
repeat would produce duplicate input.

The compositor tracks actual keys through a separate Smithay keyboard source
for each pane. Modifier snapshots, including Caps Lock and Num Lock, are applied
before forwarding a key, without inventing modifier key presses. Actual
left/right modifier transitions determine their own modifier's state: the
pinned Crossterm revision incorrectly adds that modifier bit even on release.
This also keeps one Control key down when the other Control key is released.

Focus reporting (`CSI ? 1004 h`) triggers `Input::ResetKeyboard` on focus loss.
Switching input panes/windows, changing the shown window, and disconnecting
also release held keys and clear snapshot-only modifiers. Late releases cannot
reclaim focus or affect another pane's keyboard state. Clicking focuses the
pane's window as well.

Shortcut releases are consumed only if their presses were intercepted, even
if the modifiers changed between press and release. Clipboard paste and native
clipboard fallback use explicit `Input::KeyTap` commands, not physical key
transitions; they restore the previous modifier state afterward. Bracketed
paste still transfers Unicode through the clipboard rather than fake typing.

## Removed workaround

`../meowlandold` commit `673a114` (`keys: treat every press as a whole keystroke`)
worked around Herdr losing printable-key releases. Branch
`test/no-hold-workaround` points to the preceding commit, `8683e8c`.
The rewritten compositor inherited that behavior: it discarded releases,
synthesized a press/release for every press or repeat, and wrapped each stroke
in synthetic modifier presses/releases. None of that is used for physical input
now, including the server's old Alt+Q/Alt+W release filter.

[Herdr #4184][herdr] is closed as fixed on master. Its October 7, 2026 comment
says the fix was queued for the next release, not yet published. Use a Herdr
build containing that fix. Older broken builds may advertise these flags while
still dropping printable releases; the capability query cannot detect that.
There is deliberately no Herdr-specific timeout or immediate-release workaround.

## Remaining limits and possible next steps

Kitty makes **event handling** much cleaner, but it is not an evdev protocol.
It sends logical, unshifted Unicode key identities, not physical scan codes.
The compositor advertises a plain US XKB keymap, so a key-identity-to-evdev
translation is still necessary. Named keys, left/right Shift/Control/Alt/Super,
locks, F1–F24, and keypad identities are mapped. Non-US layouts and characters
outside that keymap are not fully represented; Hyper/Meta/ISO-level keys do not
have dedicated modifier keys in the advertised keymap. They are not silently
remapped to Alt or Super. Unicode paste remains supported.

Kitty's `4` (alternate keys) could improve non-US shortcut matching, and `16`
(associated text) could support a separate text/IME path. Neither is requested
with the current parser: Crossterm replaces the primary key with its shifted
alternate, clears Shift, discards the base-layout alternate, and does not expose
associated text. Those fields need a richer parser/event model, followed by an
explicit choice of keymap/physical-key semantics and a Wayland text-input or
input-method path. Merely enabling every Kitty flag would lose useful
information rather than fix that translation.

Rebuild and restart the meowland server when updating: `ResetKeyboard` and
`KeyTap` add pane protocol variants that an older server cannot decode.

[kitty]: https://sw.kovidgoyal.net/kitty/keyboard-protocol/
[herdr]: https://github.com/herdrdev/herdr/issues/4184
