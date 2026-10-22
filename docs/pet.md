# Touch Bar pet

A small virtual pet that lives on its own layer of the Touch Bar. Back to the
[README](../README.md).

The pet is a homage to the 2016 Touchbar Pet, written from scratch: no original
assets or code.

## Finding it

With `PetEnabled = true` (the default) the More bar gets a **Pet** button:
Home → More → Pet. A deck without a More layer gets the button on Home instead,
unless a visible button already opens the pet.

The pet layer is a bar with a Home button and open ground. The pet exists and
animates only there; it never appears on or interferes with other layers. The
layer follows the same rules as any named bar, including Fn behavior, safe
handling of invalid configurations, and input release on layer switches.
Leaving the layer pauses the pet in place, and its state survives layer switches
and config reloads.

## What it does

The layer is a little scene: a blue sky with sun and clouds over a strip of
green ground. The default cat is a chunky orange tabby - big head, stripes,
white muzzle and chest, pink nose and inner ears, green eyes - about four fifths
of the bar's height. It is drawn as pixel frames straight onto the Cairo
surface, so no images or font glyphs are needed. `PetVariant = "fox"` swaps in a
red fox.

- It wanders left and right with short pauses and curls up for naps.
- **Tap the pet** and it startles: it jumps and scurries away.
- **Hunger.** Satiety decays over about an hour. A hungry pet sits and begs and
  never naps; a well-fed pet naps and wanders.
- **Feeding.** Tapping a hungry pet drops a kibble right beside it. Tapping open
  ground anywhere else drops a kibble there, and the pet walks over and eats it,
  hungry or not. Only one kibble waits at a time; a new drop replaces it. A
  napping or startled pet finishes first, and a pet that is eating ignores new
  drops until it is done.
- Taps on the pet are always consumed and never press the button underneath.
  Food only lands on open ground, never on the layer's buttons.

## Time of day

The pet follows the same local-hour bands as the
[background tint](themes.md#time-of-day-background):

- **Morning:** zoomies - short pauses, long roams, few naps - under a pale sky
  with the sun low on the left.
- **Midday:** the normal tempo.
- **Evening:** longer pauses and longer, likelier naps under a warm dusk with the
  sun low on the right.
- **Night:** a well-fed pet mostly sleeps under a moon and stars, waking for the
  odd stroll or a midnight snack. Hunger still wins over sleep.

Its stride and metabolism never change with the clock, only the tempo. The
scene repaints on the daemon's existing wake cadence.

## Configuration

```toml
PetEnabled = true   # false removes the Pet button and the pet layer
PetVariant = "cat"  # or "fox"
PetEnergy = "calm"  # or "rowdy": dashes more and gets hungry sooner
```

Invalid values are rejected like any other knob; files without pet keys use the
defaults above. A `[[Layers]]` entry named `pet` replaces the synthesized
playground, and an existing visible button targeting `pet` replaces the
synthesized Pet button. With the pet disabled the `pet` layer no longer exists,
so buttons that still target it are rejected like any unknown layer.

The pet has no scripting, no network access and no state that outlives the
running daemon.
