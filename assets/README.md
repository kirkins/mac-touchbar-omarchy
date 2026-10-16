# README images

These PNGs are real, headless renders of the shipped consumer deck
(`share/tiny-dfr/multibar.toml`), captured from the same pinned frames the
`dfr-render` test suite verifies. They are not screenshots, mock-ups or
retouched images.

| File | Frame |
| --- | --- |
| `deck-home.png` | The Home bar: volume, brightness, play/pause, F1-F12, Media, More, battery and Settings. |
| `deck-media.png` | The Media bar: Home, previous, play/pause, next, now-playing, volume and Viz. |
| `deck-function.png` | The Function bar: Home and F1-F12. |

Regenerate them with the pinned-frame test, writing outside the repository:

```sh
DFR_DECK_REVIEW_OUTPUT=/tmp/dfr-deck-review \
    cargo test -p dfr-render --locked \
    shipped_multibar_frames_match_the_pinned_pixel_digests -- --test-threads=1
```

The test writes `golden-home.png`, `golden-media.png` and `golden-function.png`
at 2008x60 from the exact pinned frames; copy the useful ones here with clear
names. It renders with the configured fonts and icons. There is no Touch Bar,
display or session input, so a few live values are illustrative: the battery
reads `N/A` because no battery device is present, and the now-playing row shows
its dim `Nothing playing` state. The pinned digests in
`crates/dfr-render/src/config.rs` are the check that these files stay the real
output of the current draw path.

An optional desktop capture (the Touch Bar in situ, possibly with the Settings
window open) can be added as `desktop.png`; it is not committed here because it
must come from a real device session rather than this headless renderer.
