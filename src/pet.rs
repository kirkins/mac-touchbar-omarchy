//! A small virtual pet that lives on its own pet layer, opened by a Pet
//! button on the home bar, in the spirit of the 2016 Touchbar Pet. The pet
//! is a behavior state machine driven by the same
//! background tick machinery as pixel shift and the battery widgets: every
//! daemon loop iteration calls [`PetManager::update`] with the current time,
//! which returns whether a redraw is needed and when the next tick is due so
//! the shared epoll wait wakes in time. Sprites are tiny per-state pixel
//! frames composed from constant grids and drawn through the existing cairo
//! surface - no image loading, no new rendering architecture. The pet
//! layer also gets a painted backdrop scene: blue sky, a green ground strip
//! with the sun and clouds above it. Taps inside
//! the pet layer are offered to the pet before the layer's hit testing, so a
//! consumed tap never activates the button underneath: a tap on the pet
//! itself is petting (the startle reaction, or a handout kibble when it is
//! hungry), and a tap on open ground - away from the pet and from the
//! layer's buttons - drops one kibble the pet walks over and munches, while
//! taps on buttons pass through untouched. The pet pauses while
//! any other layer is displayed and resumes with its state intact.
//!
//! The pet also lives in the time of day, mirroring the hour bands of the
//! time-of-day background tint: mornings bring zoomies (short idles, long
//! roams, few naps) over a pale sky with a low climbing sun, evenings wind
//! down (longer idles, likelier naps) under a warm dusk with the sun
//! sinking low, and at night a well-fed pet mostly sleeps beneath a moon
//! and stars where the sun stood by day. Hunger still overrides any
//! sleepiness, exactly as by day.

use cairo::Context;
use rand::Rng;
use std::time::{Duration, Instant};

/// Pixels per sprite pixel. 4 turns a 17x12 grid into a 68x48 sprite,
/// a chunky cat that still fits a 60 px tall bar.
const SCALE: f64 = 4.0;
/// Blank margins from the bar ends the pet treats as its territory.
const EDGE_MARGIN: f64 = 26.0;
/// Distance kept beyond a button edge when standing or landing beside it.
const CLEAR_MARGIN: f64 = 6.0;
/// Extra tap slop around the sprite in every direction.
const TAP_PAD: f64 = 9.0;
/// Satiety below which the pet is hungry and begs.
const HUNGRY: f64 = 0.35;
/// Napping requires at least this much satiety.
const NAP_MIN: f64 = 0.5;
/// Gap kept to the left of the clock widget when curling up for a nap.
const NAP_GAP: f64 = 8.0;
const STARTLE_MS: u64 = 900;
const STARTLE_LIFT: f64 = 10.0;
const MUNCH_MS: u64 = 1600;
const MUNCH_STEP_MS: u64 = 160;
const HEART_MS: u64 = 900;
const BEG_MS: u64 = 420;
const BLINK_MS: u64 = 1800;
/// Idle pause after a nap, a meal, or a startle.
const BRIEF_IDLE_MS: u64 = 700;
/// Flee distance after a startle.
const FLEE_PX: f64 = 140.0;

/// Bar geometry, visibility, and local hour for one tick. `active` gates
/// the pet to its own layer, the only place it is allowed to wander and
/// drawn; `hour` (local, 0-23) selects the part of the day the pet's mood
/// and scene follow.
pub struct Bar {
    pub width: f64,
    pub height: f64,
    pub active: bool,
    pub hour: u32,
}

/// An occupied horizontal span of the bar in layout coordinates mirroring
/// the drawn button geometry: the static pixel-shift inset is included, the
/// dynamic shift is applied later at draw and tap time. `clock` marks the
/// clock widget the pet prefers to nap next to.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Span {
    pub left: f64,
    pub right: f64,
    pub clock: bool,
}

pub use dfr_config::{PetEnergy, PetVariant};

/// The sprite colors for one pet variant: the pet renderer's own
/// palette, distinct from the accent palette the configuration names.
fn pet_palette(variant: PetVariant) -> Palette {
    match variant {
        PetVariant::Cat => Palette {
            body: (0.95, 0.55, 0.18),
            stripe: (0.72, 0.33, 0.10),
            dark: (0.25, 0.13, 0.08),
            light: (0.98, 0.95, 0.90),
            nose: (0.98, 0.55, 0.62),
            ear: (0.97, 0.62, 0.66),
            eye: (0.10, 0.55, 0.25),
        },
        PetVariant::Fox => Palette {
            body: (0.90, 0.42, 0.10),
            stripe: (0.72, 0.30, 0.06),
            dark: (0.20, 0.10, 0.06),
            light: (0.97, 0.93, 0.85),
            nose: (0.25, 0.15, 0.12),
            ear: (0.95, 0.60, 0.50),
            eye: (0.15, 0.12, 0.10),
        },
    }
}

/// The part of the day the pet is living in, following the same local-hour
/// boundaries as the shipped default time-of-day background bands (morning
/// from 5, day from 12, evening from 17, night from 21) so the pet's mood
/// and scene complement the strip's tint.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum DayPart {
    Morning,
    #[default]
    Day,
    Evening,
    Night,
}

impl DayPart {
    /// Map a local hour (0-23) onto the day part covering it, wrapping
    /// past midnight into night.
    pub fn from_hour(hour: u32) -> Self {
        match hour {
            5..=11 => Self::Morning,
            12..=16 => Self::Day,
            17..=20 => Self::Evening,
            _ => Self::Night,
        }
    }

    /// How this part bends the pet's rhythm on top of the energy dial:
    /// mornings are frisky, evenings wind down, nights are for sleeping.
    fn mood(self) -> Mood {
        match self {
            Self::Morning => Mood {
                idle: 0.6,
                nap: 0.7,
                nap_chance: 0.5,
                wander: 1.15,
            },
            Self::Day => Mood::EVEN,
            Self::Evening => Mood {
                idle: 1.5,
                nap: 1.4,
                nap_chance: 1.7,
                wander: 0.7,
            },
            Self::Night => Mood {
                idle: 2.5,
                nap: 3.0,
                nap_chance: 4.0,
                wander: 0.25,
            },
        }
    }
}

/// Per-part multipliers bending the cadence: how long idles and naps run,
/// how likely a nap is, and how readily the pet wanders.
struct Mood {
    idle: f64,
    nap: f64,
    nap_chance: f64,
    wander: f64,
}

impl Mood {
    /// The neutral midday tempo: the cadence exactly as configured.
    const EVEN: Mood = Mood {
        idle: 1.0,
        nap: 1.0,
        nap_chance: 1.0,
        wander: 1.0,
    };
}

/// Movement and metabolism parameters per energy dial position; the part
/// of the day scales the tempo fields on top of these.
#[derive(Clone, Copy)]
struct Cadence {
    step_ms: u64,
    px_per_step: f64,
    idle_ms: (u64, u64),
    nap_ms: (u64, u64),
    nap_chance: f64,
    /// Chance an idle pet sets out wandering rather than sitting.
    wander_chance: f64,
    /// Satiety lost per millisecond; rowdy pets burn through meals faster.
    decay_per_ms: f64,
}

/// Scale both ends of a duration range by `by`, capped at a quarter hour
/// so no mood can park the pet for good.
fn scale_range(range: (u64, u64), by: f64) -> (u64, u64) {
    let scale = |ms: u64| ((ms as f64 * by).round() as u64).min(15 * 60_000);
    (scale(range.0), scale(range.1))
}

const CALM: Cadence = Cadence {
    step_ms: 280,
    px_per_step: 5.0,
    idle_ms: (900, 3200),
    nap_ms: (9000, 20000),
    nap_chance: 0.25,
    wander_chance: 0.85,
    decay_per_ms: 1.0 / 3_600_000.0, // full to hungry in about an hour
};

const ROWDY: Cadence = Cadence {
    step_ms: 160,
    px_per_step: 9.0,
    idle_ms: (300, 1200),
    nap_ms: (6000, 12000),
    nap_chance: 0.12,
    wander_chance: 0.85,
    decay_per_ms: 1.0 / 2_400_000.0,
};

struct Palette {
    body: (f64, f64, f64),
    stripe: (f64, f64, f64),
    dark: (f64, f64, f64),
    light: (f64, f64, f64),
    nose: (f64, f64, f64),
    ear: (f64, f64, f64),
    eye: (f64, f64, f64),
}

impl Palette {
    fn get(&self, ch: char) -> Option<(f64, f64, f64)> {
        match ch {
            'B' => Some(self.body),
            'S' => Some(self.stripe),
            'D' => Some(self.dark),
            'W' => Some(self.light),
            'N' => Some(self.nose),
            'P' => Some(self.ear),
            'E' => Some(self.eye),
            _ => None,
        }
    }
}

// Per-state sprite frames, drawn facing right and mirrored for leftward
// travel. '.' is transparent; B body, S stripe, D dark outline, W white,
// N nose, P inner ear, E eye. The cat is a chibi orange tabby: big head,
// tabby stripes, white muzzle and chest, pink nose and inner ears.
const WALK_A: &[&str] = &[
    "...........D..D..",
    "..........DPDDPD.",
    "..S.......DBBBBD.",
    "..SS.....DBSBBED.",
    "...SS..SDBSBWWND.",
    "....SSDBBBBBWWWD.",
    "....DBBBBBBBWWWD.",
    "....DBSBSBBBDDDD.",
    "....DBBBBBBD.....",
    "....DB..DBBD.....",
    "....DB...DBD.....",
    "....DD...DWD.....",
];
const WALK_B: &[&str] = &[
    "...........D..D..",
    "..........DPDDPD.",
    "..S.......DBBBBD.",
    "..SS.....DBSBBED.",
    "...SS..SDBSBWWND.",
    "....SSDBBBBBWWWD.",
    "....DBBBBBBBWWWD.",
    "....DBSBSBBBDDDD.",
    "....DBBBBBBD.....",
    ".....DBD.DBD.....",
    ".....DBD..DB.....",
    ".....DWD..DW.....",
];
const SIT: &[&str] = &[
    "...........D..D..",
    "..........DPDDPD.",
    "..........DBBBBD.",
    "..........DBSBED.",
    "..........DBWWND.",
    "..........DBWWWD.",
    ".......DDDDBBBBD.",
    ".....SDBBBBBBBWD.",
    "....DSBSBBBBBBWD.",
    "....DBBBBBBBBBWD.",
    "....DB.BB..BB.DD.",
    "....DD.DWW.DWWD..",
];
const BEG_A: &[&str] = &[
    "...........D..D..",
    "..........DPDDPD.",
    "..........DBBBBD.",
    "..........DBSBED.",
    "..........DBWWND.",
    "......DDDDDBWWWD.",
    "....SDBBBBBBDDDD.",
    "...DSBSBBBBBD....",
    "...DBBBBBBBBD....",
    "...DBBBBBBBBD....",
    "...DBD...DBD.....",
    "...DWD...DWD.....",
];
const BEG_B: &[&str] = &[
    "..........D..D...",
    ".........DPDDPD..",
    ".........DBBBBD..",
    ".........DBSBED..",
    ".........DBWWND..",
    ".....DDDDDBWWWD..",
    "...SDBBBBBBDDDD..",
    "..DSBSBBBBBD.W...",
    "..DBBBBBBBBD.....",
    "..DBBBBBBBBD.....",
    "..DBD...DBD......",
    "..DWD...DWD......",
];
const SLEEP: &[&str] = &[
    "................",
    "................",
    "................",
    "................",
    ".......DDDD.....",
    ".....DDBSSBDD...",
    "...DDBBBBBBBBD..",
    "..DBBBBBBBBBSBD.",
    "..DBBBBBBBBBSBD.",
    "..DBNWWBBBBBSBD.",
    "..DWWWWDBBBBSBD.",
    "...DDDDDDDDDDDD.",
];
const STARTLE: &[&str] = &[
    "................",
    "..D..S........D.",
    "..DPDSS......DPD",
    "..DBBBSDDDD..DBD",
    "...DBBBBBBBDDBBD",
    "...DBBBBBBBBBD..",
    "..DBSBSBSBBBD...",
    "..DBBBBBBBBD....",
    "..DBD.DBD.DD....",
    "..DD...DD.......",
    "................",
    "................",
];
const MUNCH_A: &[&str] = &[
    "................",
    "................",
    "..........D..D..",
    ".........DPDDPD.",
    "....S.....DBBBBD",
    "...SS....DBSBED.",
    "..SS..SDBSBWWND.",
    "...SDBBBBBWWWWWD",
    "...DBSBSBBDDDDDD",
    "...DBBBBBBD.....",
    "...DB..DBBD.....",
    "...DD..DWWD.....",
];
const MUNCH_B: &[&str] = &[
    "................",
    "................",
    "..........D..D..",
    ".........DPDDPD.",
    "....S.....DBBBBD",
    "...SS....DBSBED.",
    "..SS..SDBSBWWND.",
    "...SDBBBBBWWWWWD",
    "...DBSBSBBD.DDD.",
    "...DBBBBBBD.....",
    "...DB..DBBD.....",
    "...DD..DWWD.....",
];
const BANG: &[&str] = &[".DD.", ".DD.", ".DD.", "....", ".DD."];
const ZZZ: &[&str] = &["DDDD", "..D.", ".D..", "DDDD"];
const HEART: &[&str] = &[".DD.DD.", "DDDDDDD", ".DDDDD.", "..DDD..", "...D..."];
const KIBBLE: &[&str] = &[".KK.", "KKKK", ".KK."];
const KIBBLE_RGB: (f64, f64, f64) = (0.62, 0.38, 0.15);
const OVERLAY_DARK: (f64, f64, f64) = (0.95, 0.95, 0.95);
const OVERLAY_HEART: (f64, f64, f64) = (0.98, 0.35, 0.5);

// The pet layer's backdrop: a blue sky above a green ground strip, with a
// sun and clouds, painted only across the layer's open ground so the Home
// button keeps its normal deck look. The scene follows the part of the
// day: a pale morning sky with the sun climbing low on the left, the
// plain day look, a warm dusk with the sun sinking low on the right, and
// a dark night sky with a moon and stars instead of the sun.
const CLOUD: &[&str] = &["..CCCC..", ".CCCCCC.", "CCCCCCCC"];
const SUN: &[&str] = &["..YY..", ".YYYY.", "YYYYYY", "YYYYYY", ".YYYY.", "..YY.."];
const MOON: &[&str] = &[".MMMM.", "MM....", "M.....", "M.....", "MM....", ".MMMM."];
const STAR: &[&str] = &[".S.", "SSS", ".S."];
const CLOUD_RGB: (f64, f64, f64) = (0.97, 0.99, 1.0);
const SUN_RGB: (f64, f64, f64) = (1.0, 0.85, 0.25);
const CLOUD_DUSK_RGB: (f64, f64, f64) = (0.98, 0.80, 0.68);
const SUNSET_RGB: (f64, f64, f64) = (1.0, 0.58, 0.18);
const MOON_RGB: (f64, f64, f64) = (0.90, 0.93, 0.99);
const STAR_RGB: (f64, f64, f64) = (0.95, 0.96, 1.0);
/// Fraction of the bar height where the ground strip begins; the pet's
/// feet rest inside it.
const GROUND_TOP: f64 = 0.80;
const SKY_RGB: (f64, f64, f64) = (0.47, 0.72, 0.96);
const GROUND_RGB: (f64, f64, f64) = (0.35, 0.68, 0.32);
const GRASS_RGB: (f64, f64, f64) = (0.22, 0.50, 0.22);
const MORNING_SKY_RGB: (f64, f64, f64) = (0.66, 0.80, 0.94);
const EVENING_SKY_RGB: (f64, f64, f64) = (0.84, 0.60, 0.42);
const EVENING_GROUND_RGB: (f64, f64, f64) = (0.30, 0.52, 0.27);
const EVENING_GRASS_RGB: (f64, f64, f64) = (0.20, 0.38, 0.19);
const NIGHT_SKY_RGB: (f64, f64, f64) = (0.07, 0.10, 0.21);
const NIGHT_GROUND_RGB: (f64, f64, f64) = (0.12, 0.20, 0.14);
const NIGHT_GRASS_RGB: (f64, f64, f64) = (0.08, 0.13, 0.09);

#[derive(Clone, Copy, Debug, PartialEq)]
enum PetState {
    Idle {
        until: Instant,
    },
    Walk {
        target: f64,
        after: After,
    },
    Nap {
        started: Instant,
        until: Instant,
    },
    Beg {
        until: Instant,
    },
    Startled {
        started: Instant,
        until: Instant,
        away: f64,
    },
    Munch {
        started: Instant,
        until: Instant,
    },
}

#[derive(Clone, Copy, Debug, PartialEq)]
enum After {
    Pause,
    Nap,
    Munch,
}

#[derive(Clone, Copy, Debug)]
struct Food {
    x: f64,
}

pub struct PetManager {
    enabled: bool,
    variant: PetVariant,
    energy: PetEnergy,
    /// The part of the day the pet is living in, taken from the bar's
    /// local hour each tick; bends the cadence and picks the scene's look.
    day_part: DayPart,
    visible: bool,
    placed: bool,
    state: PetState,
    food: Option<Food>,
    /// Horizontal center in layout coordinates (before pixel-shift offset).
    pos: f64,
    facing: f64,
    /// Current upward displacement from the ground, in pixels.
    lift: f64,
    phase: u32,
    satiety: f64,
    happy_until: Option<Instant>,
    /// Whether the nap Z overlay is currently shown, for blink redraws.
    blink_on: bool,
    next_step: Option<Instant>,
    last_tick: Instant,
    // Bar geometry and widget spans cached by update() for draw() and tap().
    width: f64,
    height: f64,
    spans: Vec<Span>,
}

impl PetManager {
    pub fn new(enabled: bool, variant: PetVariant, energy: PetEnergy) -> Self {
        let now = Instant::now();
        Self {
            enabled,
            variant,
            energy,
            day_part: DayPart::default(),
            visible: false,
            placed: false,
            state: PetState::Idle {
                until: now + Duration::from_millis(800),
            },
            food: None,
            pos: 0.0,
            facing: 1.0,
            lift: 0.0,
            phase: 0,
            // Start content but not stuffed, so the first begging arrives
            // within a lazy afternoon rather than immediately.
            satiety: 0.8,
            happy_until: None,
            blink_on: false,
            next_step: None,
            last_tick: now,
            width: 0.0,
            height: 0.0,
            spans: Vec::new(),
        }
    }

    /// Apply new configuration on reload without waking the pet: position,
    /// state, and satiety survive so a knob tweak does not respawn it.
    pub fn apply_config(&mut self, enabled: bool, variant: PetVariant, energy: PetEnergy) {
        self.enabled = enabled;
        self.variant = variant;
        self.energy = energy;
    }

    pub fn is_visible(&self) -> bool {
        self.visible
    }

    /// Horizontal center in layout coordinates, for tests asserting that
    /// layer switches never teleport the pet.
    #[cfg(test)]
    pub(crate) fn position(&self) -> f64 {
        self.pos
    }

    fn cadence(&self) -> Cadence {
        let base = match self.energy {
            PetEnergy::Calm => CALM,
            PetEnergy::Rowdy => ROWDY,
        };
        // The part of the day bends the tempo of life on top of the energy
        // dial; the stride and the metabolism stay the pet's own.
        let mood = self.day_part.mood();
        Cadence {
            step_ms: base.step_ms,
            px_per_step: base.px_per_step,
            idle_ms: scale_range(base.idle_ms, mood.idle),
            nap_ms: scale_range(base.nap_ms, mood.nap),
            nap_chance: (base.nap_chance * mood.nap_chance).clamp(0.0, 1.0),
            wander_chance: (base.wander_chance * mood.wander).clamp(0.0, 1.0),
            decay_per_ms: base.decay_per_ms,
        }
    }

    fn min_x(&self) -> f64 {
        EDGE_MARGIN + self.half_w()
    }

    fn max_x(&self) -> f64 {
        (self.width - EDGE_MARGIN - self.half_w()).max(self.min_x())
    }

    fn half_w(&self) -> f64 {
        grid_w(WALK_A) * SCALE / 2.0
    }

    /// Feet position; sprites are drawn up from this baseline.
    fn feet_y(&self) -> f64 {
        self.height * 0.88
    }

    fn top_y(&self) -> f64 {
        self.feet_y() - self.sprite_h()
    }

    fn sprite_h(&self) -> f64 {
        grid_h(WALK_A) * SCALE
    }

    /// Advance the state machine to `now`. Returns whether the bar needs a
    /// redraw and how long until the next interesting moment, in milliseconds.
    pub fn update(&mut self, now: Instant, bar: &Bar, spans: &[Span]) -> (bool, i32) {
        let mut redraw = false;
        if self.visible != (self.enabled && bar.active) {
            self.visible = self.enabled && bar.active;
            // Repaint either clears a hidden pet or restores a shown one.
            redraw = true;
        }
        self.width = bar.width;
        self.height = bar.height;
        self.spans = spans.to_vec();

        // The mood and scene track the wall clock alongside the background
        // tint; a new part of the day repaints the scene once.
        let day_part = DayPart::from_hour(bar.hour);
        if self.visible && day_part != self.day_part {
            redraw = true;
        }
        self.day_part = day_part;

        let dt_ms = now.saturating_duration_since(self.last_tick).as_secs_f64() * 1000.0;
        self.last_tick = now;
        self.satiety = (self.satiety - self.cadence().decay_per_ms * dt_ms).max(0.0);

        if !self.visible {
            // Paused while another layer is displayed: the state machine and
            // position freeze, only the metabolism keeps running.
            return (redraw, i32::MAX);
        }

        // An expired heart overlay must come off the bar even without a
        // state transition to redraw it.
        if self.happy_until.is_some_and(|until| until <= now) {
            self.happy_until = None;
            redraw = true;
        }

        if !self.placed {
            self.placed = true;
            self.pos = clamp_outside(
                (bar.width * 0.3).clamp(self.min_x(), self.max_x()),
                spans,
                self.min_x(),
                self.max_x(),
            );
            redraw = true;
        }
        self.pos = self.pos.clamp(self.min_x(), self.max_x());
        // A grounded pet never stands inside a button; this also relocates
        // it politely when a reload changes the layout underneath.
        self.pos = clamp_outside(self.pos, spans, self.min_x(), self.max_x());
        // The same reload can drop widgets onto a chosen target or waiting
        // food; keep both standable against the layout now in force.
        let (min_x, max_x) = (self.min_x(), self.max_x());
        if let PetState::Walk { target, .. } = &mut self.state {
            if span_at(spans, *target).is_some() {
                *target = clamp_outside(*target, spans, min_x, max_x);
            }
        }
        if let Some(food) = &mut self.food {
            if span_at(spans, food.x).is_some() {
                let old_x = food.x;
                food.x = clamp_outside(food.x, spans, min_x, max_x);
                redraw |= food.x != old_x;
            }
        }

        match self.state {
            PetState::Idle { until } => {
                if now >= until {
                    self.decide(now);
                    redraw = true;
                }
            }
            PetState::Walk { .. } => {
                while matches!(self.state, PetState::Walk { .. })
                    && self.next_step.is_some_and(|t| now >= t)
                {
                    self.walk_step(now);
                    redraw = true;
                }
            }
            PetState::Nap { started, until } => {
                let blink = zzz_visible(started, now);
                if blink != self.blink_on {
                    self.blink_on = blink;
                    redraw = true;
                }
                if now >= until {
                    self.state = PetState::Idle {
                        until: now + Duration::from_millis(BRIEF_IDLE_MS),
                    };
                    self.blink_on = false;
                    redraw = true;
                }
            }
            PetState::Beg { until } => {
                while self.next_step.is_some_and(|t| now >= t) {
                    self.phase += 1;
                    self.next_step = self.next_step.map(|t| t + Duration::from_millis(BEG_MS));
                    redraw = true;
                }
                if now >= until {
                    self.decide(now);
                    redraw = true;
                }
            }
            PetState::Startled {
                started,
                until,
                away,
            } => {
                if now >= until {
                    self.lift = 0.0;
                    let target = clamp_outside(
                        (self.pos + away * FLEE_PX).clamp(self.min_x(), self.max_x()),
                        &self.spans,
                        self.min_x(),
                        self.max_x(),
                    );
                    self.state = PetState::Walk {
                        target,
                        after: After::Pause,
                    };
                    self.next_step = Some(now);
                    redraw = true;
                } else {
                    let total = until.saturating_duration_since(started).as_secs_f64();
                    let t = now.saturating_duration_since(started).as_secs_f64() / total.max(1e-9);
                    self.lift = STARTLE_LIFT * (std::f64::consts::PI * t).sin();
                    redraw = true;
                }
            }
            PetState::Munch { until, .. } => {
                while self.next_step.is_some_and(|t| now >= t) {
                    self.phase += 1;
                    self.next_step = self
                        .next_step
                        .map(|t| t + Duration::from_millis(MUNCH_STEP_MS));
                    redraw = true;
                }
                if now >= until {
                    self.food = None;
                    self.satiety = 1.0;
                    self.happy_until = Some(now + Duration::from_millis(HEART_MS));
                    self.state = PetState::Idle {
                        until: now + Duration::from_millis(BRIEF_IDLE_MS),
                    };
                    redraw = true;
                }
            }
        }

        (redraw, self.next_timeout(now))
    }

    /// Offer a touchbar tap to the pet before layer hit testing. Returns true
    /// when the pet consumed it and the widget underneath must not fire.
    /// Precedence: the pet itself first (petting, always consumed), then
    /// open ground (a kibble drop, consumed), while taps landing on a
    /// widget span always pass through to that widget.
    pub fn tap(&mut self, x: f64, y: f64, now: Instant, shift_x: f64) -> bool {
        if !self.visible {
            return false;
        }
        let screen_x = self.pos + shift_x;
        let direct = (x - screen_x).abs() <= self.half_w() + TAP_PAD
            && y >= self.top_y() - TAP_PAD
            && y <= self.feet_y() + TAP_PAD;
        let receptive = matches!(
            self.state,
            PetState::Idle { .. } | PetState::Walk { .. } | PetState::Beg { .. }
        );
        if direct {
            if receptive && self.satiety < HUNGRY {
                // Tapping a hungry pet drops a kibble right beside it.
                let drop_x = self.pos + self.facing * (self.half_w() + 14.0);
                self.spawn_food(drop_x, now);
            } else if !matches!(self.state, PetState::Munch { .. }) {
                let away = if x >= screen_x { -1.0 } else { 1.0 };
                self.lift = 0.0;
                self.state = PetState::Startled {
                    started: now,
                    until: now + Duration::from_millis(STARTLE_MS),
                    away,
                };
                self.next_step = None;
            }
            return true;
        }
        // A tap on open ground - not on the pet, not on a button - drops a
        // kibble there. It never steals a tap aimed at a widget, never
        // interrupts a meal in progress, and never wakes a napping or
        // startled pet: those two find the kibble when their act ends.
        let ground_x = x - shift_x;
        if matches!(self.state, PetState::Munch { .. }) || span_at(&self.spans, ground_x).is_some()
        {
            return false;
        }
        if receptive {
            self.spawn_food(ground_x, now);
        } else {
            self.place_food(ground_x);
        }
        true
    }

    /// Clamp a kibble drop onto open, standable ground and set it as the
    /// one waiting kibble: the pet holds at most one at a time, so a new
    /// drop replaces a waiting one (a meal in progress is never replaced -
    /// [`PetManager::tap`] guards it). Returns where the kibble landed.
    fn place_food(&mut self, x: f64) -> f64 {
        let x = clamp_outside(
            x.clamp(self.min_x(), self.max_x()),
            &self.spans,
            self.min_x(),
            self.max_x(),
        );
        self.food = Some(Food { x });
        x
    }

    /// Drop a kibble the receptive pet immediately goes for.
    fn spawn_food(&mut self, x: f64, now: Instant) {
        let x = self.place_food(x);
        self.lift = 0.0;
        if (x - self.pos).abs() < self.cadence().px_per_step {
            self.state = PetState::Munch {
                started: now,
                until: now + Duration::from_millis(MUNCH_MS),
            };
            self.next_step = Some(now);
        } else {
            self.state = PetState::Walk {
                target: x,
                after: After::Munch,
            };
            self.next_step = Some(now);
        }
    }

    /// Draw the pet onto its layer. `c` must carry the same
    /// translate/rotate transform the layer draw sets up, `shift` the total
    /// pixel-shift offset applied to widgets.
    pub fn draw(&self, c: &Context, height: i32, shift: (f64, f64), now: Instant) {
        if !self.visible {
            return;
        }
        self.draw_scene(c, height as f64, shift);
        let feet = height as f64 * 0.88 + shift.1 - self.lift;
        if let Some(food) = &self.food {
            draw_grid(
                c,
                KIBBLE,
                food.x + shift.0,
                height as f64 * 0.88 + shift.1,
                |ch| (ch == 'K').then_some(KIBBLE_RGB),
                false,
            );
        }
        let (grid, mirror) = self.current_grid();
        let palette = pet_palette(self.variant);
        draw_grid(
            c,
            grid,
            self.pos + shift.0,
            feet,
            |ch| palette.get(ch),
            mirror,
        );
        let top = feet - self.sprite_h();
        match self.state {
            PetState::Nap { started, .. } => {
                if zzz_visible(started, now) {
                    draw_overlay(c, ZZZ, OVERLAY_DARK, self.pos + shift.0, top);
                }
            }
            PetState::Startled { .. } => {
                draw_overlay(c, BANG, OVERLAY_DARK, self.pos + shift.0, top);
            }
            _ => {
                if self.happy_until.is_some_and(|until| until > now) {
                    draw_overlay(c, HEART, OVERLAY_HEART, self.pos + shift.0, top);
                }
            }
        }
    }

    /// The pet layer's backdrop: a sky above a ground strip in every open
    /// stretch of the layer, dressed for the part of the day - the sun
    /// high by day and hanging low at dawn and dusk, the moon and stars at
    /// night. Button columns are left untouched so widgets keep their
    /// deck look.
    fn draw_scene(&self, c: &Context, height: f64, shift: (f64, f64)) {
        let (sky, ground, grass) = scene_palette(self.day_part);
        let ground_top = height * GROUND_TOP;
        let overdraw = 16.0;
        for (a, b, reaches_right_edge) in free_intervals(&self.spans, self.width) {
            let (a, mut b) = (a + shift.0, b + shift.0);
            if reaches_right_edge {
                b += overdraw;
            }
            c.set_source_rgb(sky.0, sky.1, sky.2);
            c.rectangle(
                a,
                shift.1 - overdraw,
                b - a,
                ground_top - shift.1 + overdraw,
            );
            c.fill().unwrap();
            c.set_source_rgb(ground.0, ground.1, ground.2);
            c.rectangle(
                a,
                ground_top + shift.1,
                b - a,
                height - ground_top + overdraw,
            );
            c.fill().unwrap();
            c.set_source_rgb(grass.0, grass.1, grass.2);
            c.rectangle(a, ground_top + shift.1, b - a, 2.5);
            c.fill().unwrap();
        }
        // The celestial bodies float over the widest stretch of open sky.
        let Some((a, b, _)) = free_intervals(&self.spans, self.width)
            .into_iter()
            .max_by(|x, y| (x.1 - x.0).total_cmp(&(y.1 - y.0)))
            .filter(|&(a, b, _)| b - a > 240.0)
        else {
            return;
        };
        let span = b - a;
        // Where the sun rides, and what hangs there instead at night: low
        // and climbing on the left in the morning, high on the right by
        // day, low and sinking on the right in the evening.
        let (body, ink, rgb, x, y) = match self.day_part {
            DayPart::Morning => (SUN, 'Y', SUN_RGB, a + span * 0.16, 42.0),
            DayPart::Day => (SUN, 'Y', SUN_RGB, a + span * 0.86, 30.0),
            DayPart::Evening => (SUN, 'Y', SUNSET_RGB, a + span * 0.84, 42.0),
            DayPart::Night => (MOON, 'M', MOON_RGB, a + span * 0.85, 26.0),
        };
        draw_grid(
            c,
            body,
            x + shift.0,
            y + shift.1,
            move |ch| (ch == ink).then_some(rgb),
            false,
        );
        if self.day_part == DayPart::Night {
            for (frac, y) in [(0.18, 12.0), (0.55, 20.0)] {
                draw_grid(
                    c,
                    STAR,
                    a + span * frac + shift.0,
                    y + shift.1,
                    |ch| (ch == 'S').then_some(STAR_RGB),
                    false,
                );
            }
            return;
        }
        let cloud_rgb = match self.day_part {
            DayPart::Evening => CLOUD_DUSK_RGB,
            _ => CLOUD_RGB,
        };
        let clouds: [(f64, f64); 2] = match self.day_part {
            DayPart::Morning => [(0.50, 16.0), (0.78, 24.0)],
            _ => [(0.22, 20.0), (0.52, 26.0)],
        };
        for (frac, y) in clouds {
            draw_grid(
                c,
                CLOUD,
                a + span * frac + shift.0,
                y + shift.1,
                |ch| (ch == 'C').then_some(cloud_rgb),
                false,
            );
        }
    }

    fn current_grid(&self) -> (&'static [&'static str], bool) {
        let mirror = self.facing < 0.0;
        match self.state {
            PetState::Walk { .. } => (frame_pair(WALK_A, WALK_B, self.phase), mirror),
            PetState::Idle { .. } => (SIT, false),
            PetState::Nap { .. } => (SLEEP, false),
            PetState::Beg { .. } => (frame_pair(BEG_A, BEG_B, self.phase), false),
            PetState::Startled { .. } => (STARTLE, mirror),
            PetState::Munch { .. } => (frame_pair(MUNCH_A, MUNCH_B, self.phase), mirror),
        }
    }

    fn decide(&mut self, now: Instant) {
        // An offered meal always wins.
        if let Some(food) = self.food {
            self.state = PetState::Walk {
                target: food.x,
                after: After::Munch,
            };
            self.next_step = Some(now);
            return;
        }
        let cadence = self.cadence();
        let mut rng = rand::thread_rng();
        if self.satiety < HUNGRY {
            if rng.gen_bool(0.8) {
                self.state = PetState::Beg {
                    until: now + Duration::from_millis(rng.gen_range(2500..=4200)),
                };
                self.next_step = Some(now);
            } else {
                // Pacing while waiting for dinner.
                let away = if rng.gen_bool(0.5) { -1.0 } else { 1.0 };
                let target = clamp_outside(
                    (self.pos + away * 60.0).clamp(self.min_x(), self.max_x()),
                    &self.spans,
                    self.min_x(),
                    self.max_x(),
                );
                self.state = PetState::Walk {
                    target,
                    after: After::Pause,
                };
                self.next_step = Some(now);
            }
        } else if self.satiety >= NAP_MIN && rng.gen_bool(cadence.nap_chance) {
            let target = nap_spot(&self.spans, self.pos, self.min_x(), self.max_x());
            if (target - self.pos).abs() < cadence.px_per_step {
                self.state = PetState::Nap {
                    started: now,
                    until: now
                        + Duration::from_millis(rng.gen_range(cadence.nap_ms.0..=cadence.nap_ms.1)),
                };
                self.blink_on = true;
                self.next_step = None;
            } else {
                self.state = PetState::Walk {
                    target,
                    after: After::Nap,
                };
                self.next_step = Some(now);
            }
        } else if rng.gen_bool(cadence.wander_chance) {
            let away = if rng.gen_bool(0.5) { -1.0 } else { 1.0 };
            let target = clamp_outside(
                (self.pos + away * rng.gen_range(120.0..=420.0)).clamp(self.min_x(), self.max_x()),
                &self.spans,
                self.min_x(),
                self.max_x(),
            );
            self.state = PetState::Walk {
                target,
                after: After::Pause,
            };
            self.next_step = Some(now);
        } else {
            self.state = PetState::Idle {
                until: now
                    + Duration::from_millis(rng.gen_range(cadence.idle_ms.0..=cadence.idle_ms.1)),
            };
            self.next_step = None;
        }
    }

    fn walk_step(&mut self, now: Instant) {
        let PetState::Walk { target, after } = self.state else {
            return;
        };
        let cadence = self.cadence();
        let diff = target - self.pos;
        if diff.abs() <= cadence.px_per_step {
            // Arrival.
            self.pos = target;
            self.phase += 1;
            match after {
                After::Pause => {
                    self.state = PetState::Idle {
                        until: now
                            + Duration::from_millis(
                                rand::thread_rng().gen_range(cadence.idle_ms.0..=cadence.idle_ms.1),
                            ),
                    };
                    self.next_step = None;
                }
                After::Nap => {
                    self.state = PetState::Nap {
                        started: now,
                        until: now
                            + Duration::from_millis(
                                rand::thread_rng().gen_range(cadence.nap_ms.0..=cadence.nap_ms.1),
                            ),
                    };
                    self.blink_on = true;
                    self.next_step = None;
                }
                After::Munch => {
                    self.state = PetState::Munch {
                        started: now,
                        until: now + Duration::from_millis(MUNCH_MS),
                    };
                    self.next_step = Some(now);
                }
            }
            return;
        }
        let dir = diff.signum();
        self.facing = dir;
        let next = (self.pos + dir * cadence.px_per_step).clamp(self.min_x(), self.max_x());
        if next == self.pos || sprite_blocked(&self.spans, next) {
            // Pinned at a bound, or facing a button with no room to squeeze
            // past: give up and idle.
            self.state = PetState::Idle {
                until: now + Duration::from_millis(BRIEF_IDLE_MS),
            };
            self.next_step = None;
            return;
        }
        self.pos = next;
        self.phase += 1;
        self.next_step = Some(now + Duration::from_millis(cadence.step_ms));
    }

    /// Milliseconds until the next moment worth waking for; the caller caps
    /// it like the other background tick sources.
    fn next_timeout(&self, now: Instant) -> i32 {
        let mut next: Option<Instant> = None;
        let mut consider = |t: Instant| {
            next = Some(match next {
                Some(old) if old < t => old,
                _ => t,
            });
        };
        match self.state {
            PetState::Idle { until } | PetState::Nap { until, .. } => consider(until),
            PetState::Beg { until } => {
                consider(until);
                if let Some(t) = self.next_step {
                    consider(t);
                }
            }
            PetState::Walk { .. } => {
                if let Some(t) = self.next_step {
                    consider(t);
                }
            }
            PetState::Munch { until, .. } => {
                consider(until);
                if let Some(t) = self.next_step {
                    consider(t);
                }
            }
            PetState::Startled { until, .. } => {
                consider(until);
                // Keep the startle arc smooth with short wakes while airborne.
                consider(now + Duration::from_millis(120));
            }
        }
        if let PetState::Nap { started, .. } = self.state {
            let elapsed = now.saturating_duration_since(started).as_millis() as u64;
            consider(started + Duration::from_millis((elapsed / BLINK_MS + 1) * BLINK_MS));
        }
        if let Some(until) = self.happy_until.filter(|t| *t > now) {
            consider(until);
        }
        match next {
            Some(t) => (t.saturating_duration_since(now).as_millis() as i32).clamp(1, i32::MAX - 1),
            None => i32::MAX,
        }
    }
}

fn frame_pair(
    a: &'static [&'static str],
    b: &'static [&'static str],
    phase: u32,
) -> &'static [&'static str] {
    if phase.is_multiple_of(2) {
        a
    } else {
        b
    }
}

/// One cairo source color in unit RGB.
type Rgb = (f64, f64, f64);

/// The scene's sky, ground, and grass colors for one part of the day.
fn scene_palette(part: DayPart) -> (Rgb, Rgb, Rgb) {
    match part {
        DayPart::Morning => (MORNING_SKY_RGB, GROUND_RGB, GRASS_RGB),
        DayPart::Day => (SKY_RGB, GROUND_RGB, GRASS_RGB),
        DayPart::Evening => (EVENING_SKY_RGB, EVENING_GROUND_RGB, EVENING_GRASS_RGB),
        DayPart::Night => (NIGHT_SKY_RGB, NIGHT_GROUND_RGB, NIGHT_GRASS_RGB),
    }
}

fn zzz_visible(started: Instant, now: Instant) -> bool {
    (now.saturating_duration_since(started).as_millis() / BLINK_MS as u128).is_multiple_of(2)
}

fn span_at(spans: &[Span], x: f64) -> Option<&Span> {
    spans.iter().find(|span| x >= span.left && x <= span.right)
}

/// Whether the whole sprite would overlap any button at center `x`.
fn sprite_blocked(spans: &[Span], x: f64) -> bool {
    let half = half_sprite_w();
    spans
        .iter()
        .any(|span| x + half > span.left && x - half < span.right)
}

/// Half sprite width used for span clearance; a free function so it stays
/// testable without an instance.
fn half_sprite_w() -> f64 {
    grid_w(WALK_A) * SCALE / 2.0
}

/// The maximal run of touching or overlapping spans intersecting
/// `[from, to]`, which the pet treats as a single obstacle.
fn run_overlapping(spans: &[Span], from: f64, to: f64) -> Option<(f64, f64)> {
    let (mut left, mut right) = spans
        .iter()
        .find(|span| span.left <= to && span.right >= from)
        .map(|span| (span.left, span.right))?;
    loop {
        let mut grew = false;
        for span in spans {
            if span.left <= right && span.right >= left {
                if span.left < left {
                    left = span.left;
                    grew = true;
                }
                if span.right > right {
                    right = span.right;
                    grew = true;
                }
            }
        }
        if !grew {
            return Some((left, right));
        }
    }
}

/// The run of touching spans around `x`.
fn run_at(spans: &[Span], x: f64) -> Option<(f64, f64)> {
    run_overlapping(spans, x, x)
}

/// Nearest resting position left of `edge`: one with the sprite fully clear
/// of widgets plus `margin` when the neighboring gap allows it, otherwise
/// that gap's center, so packed bars still leave somewhere to stand. `None`
/// when no free position exists on that side.
fn spot_left(edge: f64, spans: &[Span], min_x: f64, margin: f64) -> Option<f64> {
    if edge <= min_x {
        return None;
    }
    let polite = (edge - half_sprite_w() - margin).max(min_x);
    if !sprite_blocked(spans, polite) {
        return Some(polite);
    }
    let neighbor = spans
        .iter()
        .filter(|span| span.right < edge)
        .map(|span| span.right)
        .reduce(f64::max)?;
    Some(((neighbor + edge) / 2.0).max(min_x))
}

/// Nearest resting position right of `edge`, mirroring [`spot_left`].
fn spot_right(edge: f64, spans: &[Span], max_x: f64, margin: f64) -> Option<f64> {
    if edge >= max_x {
        return None;
    }
    let polite = (edge + half_sprite_w() + margin).min(max_x);
    if !sprite_blocked(spans, polite) {
        return Some(polite);
    }
    let neighbor = spans
        .iter()
        .filter(|span| span.left > edge)
        .map(|span| span.left)
        .reduce(f64::min)?;
    Some(((neighbor + edge) / 2.0).min(max_x))
}

/// Rest `x` at a standable position: unchanged when the whole sprite is
/// already clear of every button, otherwise at the nearer free spot beside
/// the run of touching spans it overlaps.
fn clamp_outside(x: f64, spans: &[Span], min_x: f64, max_x: f64) -> f64 {
    let x = x.clamp(min_x, max_x);
    let Some((left, right)) = run_overlapping(spans, x - half_sprite_w(), x + half_sprite_w())
    else {
        return x;
    };
    let near_left = x - left < right - x;
    match (
        spot_left(left, spans, min_x, CLEAR_MARGIN),
        spot_right(right, spans, max_x, CLEAR_MARGIN),
    ) {
        (Some(l), Some(r)) => {
            if near_left {
                l
            } else {
                r
            }
        }
        (Some(l), None) => l,
        (None, Some(r)) => r,
        (None, None) => x,
    }
}

/// Where to curl up: beside the clock widget when the pet layer has one,
/// otherwise wherever the pet already is.
fn nap_spot(spans: &[Span], pos: f64, min_x: f64, max_x: f64) -> f64 {
    if let Some(clock) = spans.iter().find(|span| span.clock) {
        if let Some((left, right)) = run_at(spans, clock.left) {
            return spot_left(left, spans, min_x, NAP_GAP)
                .or_else(|| spot_right(right, spans, max_x, NAP_GAP))
                .unwrap_or(pos);
        }
    }
    clamp_outside(pos, spans, min_x, max_x)
}

/// Free horizontal stretches of a layer - the gaps between and beyond the
/// button spans - as `(left, right, reaches_right_edge)` in layout
/// coordinates.
fn free_intervals(spans: &[Span], width: f64) -> Vec<(f64, f64, bool)> {
    let mut spans: Vec<&Span> = spans.iter().collect();
    spans.sort_by(|a, b| a.left.total_cmp(&b.left));
    let mut out = Vec::new();
    let mut cursor = 0.0;
    for span in spans {
        if span.left > cursor {
            out.push((cursor, span.left.min(width), false));
        }
        cursor = cursor.max(span.right);
    }
    if cursor < width {
        out.push((cursor, width, true));
    }
    out
}

fn grid_w(grid: &[&str]) -> f64 {
    grid.iter().map(|row| row.len()).max().unwrap_or(0) as f64
}

fn grid_h(grid: &[&str]) -> f64 {
    grid.len() as f64
}

fn draw_grid(
    c: &Context,
    grid: &[&str],
    center_x: f64,
    feet_y: f64,
    color: impl Fn(char) -> Option<(f64, f64, f64)>,
    mirror: bool,
) {
    let left = center_x - grid_w(grid) * SCALE / 2.0;
    let top = feet_y - grid_h(grid) * SCALE;
    for (row_i, row) in grid.iter().enumerate() {
        for (col_i, ch) in row.chars().enumerate() {
            let Some(rgb) = color(ch) else {
                continue;
            };
            let col = if mirror { row.len() - 1 - col_i } else { col_i };
            let x = left + col as f64 * SCALE;
            let y = top + row_i as f64 * SCALE;
            c.set_source_rgb(rgb.0, rgb.1, rgb.2);
            c.rectangle(x, y, SCALE, SCALE);
            c.fill().unwrap();
        }
    }
}

fn draw_overlay(c: &Context, grid: &[&str], rgb: (f64, f64, f64), center_x: f64, top_y: f64) {
    draw_grid(
        c,
        grid,
        center_x + 14.0,
        top_y - 2.0,
        |ch| (ch == 'D').then_some(rgb),
        false,
    );
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::ButtonConfig;
    use crate::FunctionLayer;
    use cairo::{Context, Format, ImageSurface};

    const WIDTH: f64 = 2008.0;
    const HEIGHT: f64 = 60.0;

    fn bar(active: bool) -> Bar {
        Bar {
            width: WIDTH,
            height: HEIGHT,
            active,
            // Midday: the neutral tempo and the plain day scene, so the
            // pre-existing assertions keep their meaning.
            hour: 14,
        }
    }

    fn pet() -> PetManager {
        let mut pet = PetManager::new(true, PetVariant::Cat, PetEnergy::Calm);
        let mut now = Instant::now();
        pet.update(now, &bar(true), &[]);
        // Park the fresh pet in a stable, receptive idle that any test's
        // simulated clock has already expired.
        now += Duration::from_millis(100);
        pet.state = PetState::Idle {
            until: Instant::now(),
        };
        pet
    }

    fn tick(pet: &mut PetManager, now: &mut Instant, spans: &[Span], dt_ms: u64) -> (bool, i32) {
        *now += Duration::from_millis(dt_ms);
        pet.update(*now, &bar(true), spans)
    }

    #[test]
    fn placement_and_wander_stay_inside_bounds_and_outside_buttons() {
        let spans = vec![
            Span {
                left: 500.0,
                right: 700.0,
                clock: false,
            },
            Span {
                left: 1600.0,
                right: 1900.0,
                clock: false,
            },
        ];
        let mut p = pet();
        let mut now = Instant::now();
        for i in 0..4000 {
            tick(&mut p, &mut now, &spans, 60);
            assert!(
                p.pos >= p.min_x() && p.pos <= p.max_x(),
                "pet left the bar at iteration {i}: {}",
                p.pos
            );
            assert!(
                !sprite_blocked(&spans, p.pos),
                "pet overlapped a button at iteration {i}: {}",
                p.pos
            );
        }
    }

    #[test]
    fn targets_beyond_the_bar_clamp_and_turn_around() {
        let mut p = pet();
        let mut now = Instant::now();
        p.state = PetState::Walk {
            target: 90000.0,
            after: After::Pause,
        };
        p.next_step = Some(now);
        let mut left_the_right_bound = false;
        for _ in 0..3000 {
            tick(&mut p, &mut now, &[], 60);
            assert!(p.pos <= p.max_x() + f64::EPSILON);
            left_the_right_bound |= p.pos < p.max_x() - 1.0;
        }
        assert!(
            left_the_right_bound,
            "pet must not pin at the right bound forever"
        );
        p.state = PetState::Walk {
            target: -90000.0,
            after: After::Pause,
        };
        p.next_step = Some(now);
        let mut left_the_left_bound = false;
        for _ in 0..3000 {
            tick(&mut p, &mut now, &[], 60);
            assert!(p.pos >= p.min_x() - f64::EPSILON);
            left_the_left_bound |= p.pos > p.min_x() + 1.0;
        }
        assert!(left_the_left_bound, "pet must not pin at the left bound");
    }

    #[test]
    fn walking_into_a_button_stops_clear_of_it_and_recovers() {
        // Buttons on the pet layer are walls, not hurdles: the pet never
        // crosses or overlaps one, gives up on unreachable targets, and
        // keeps living on its own side of the wall.
        let spans = vec![Span {
            left: 500.0,
            right: 700.0,
            clock: false,
        }];
        let mut p = pet();
        let mut now = Instant::now();
        p.pos = 400.0;
        p.state = PetState::Walk {
            target: 900.0,
            after: After::Pause,
        };
        p.next_step = Some(now);
        let mut gave_up = false;
        for i in 0..400 {
            tick(&mut p, &mut now, &spans, 40);
            assert!(
                !sprite_blocked(&spans, p.pos),
                "pet overlapped the button at iteration {i}: {}",
                p.pos
            );
            assert!(p.pos < 500.0, "pet must never cross the button");
            gave_up |= matches!(p.state, PetState::Idle { .. });
        }
        assert!(gave_up, "the pet must give up on the unreachable target");
        assert!(
            p.pos < 500.0,
            "pet stays alive on its side of the button, got {}",
            p.pos
        );
    }

    #[test]
    fn a_packed_layer_parks_the_pet_in_a_gap() {
        // A packed user-authored pet layer leaves no fully clear spot, so
        // the pet parks at a gap center and never overlaps a button.
        let mut keys: Vec<ButtonConfig> = (0..11)
            .map(|i| ButtonConfig {
                text: Some(format!("F{i}")),
                ..ButtonConfig::default()
            })
            .collect();
        keys.push(ButtonConfig {
            time: Some("24hr".into()),
            ..ButtonConfig::default()
        });
        let layer = FunctionLayer::with_config(keys).unwrap();
        let spans = layer.layout_spans(WIDTH as i32, 0);
        assert_eq!(spans.len(), 12);
        for pair in spans.windows(2) {
            assert!(
                pair[1].left - pair[0].right < half_sprite_w() * 2.0,
                "test layout must be packed"
            );
        }
        let mut p = pet();
        let mut now = Instant::now();
        tick(&mut p, &mut now, &spans, 60);
        let parked = p.pos;
        for i in 0..4000 {
            tick(&mut p, &mut now, &spans, 60);
            assert!(
                span_at(&spans, p.pos).is_none(),
                "pet center left its gap at iteration {i}: {}",
                p.pos
            );
        }
        assert_eq!(p.pos, parked, "a packed layer leaves nowhere to walk");
    }

    #[test]
    fn the_open_pet_layer_gives_the_pet_free_ground() {
        // The synthesized pet layer is a Home button plus open ground: the
        // pet never overlaps the button and roams the rest of the layer.
        let layer = FunctionLayer::with_config(vec![
            ButtonConfig {
                text: Some("Home".into()),
                ..ButtonConfig::default()
            },
            ButtonConfig {
                stretch: Some(7),
                ..ButtonConfig::default()
            },
        ])
        .unwrap();
        let spans = layer.layout_spans(WIDTH as i32, 0);
        assert_eq!(spans.len(), 1, "spacers are free ground");
        let mut p = pet();
        let mut now = Instant::now();
        let mut seen = std::collections::HashSet::new();
        for i in 0..4000 {
            tick(&mut p, &mut now, &spans, 60);
            assert!(
                !sprite_blocked(&spans, p.pos),
                "pet overlapped the Home button at iteration {i}: {}",
                p.pos
            );
            seen.insert((p.pos * 4.0).round() as i64);
        }
        assert!(
            seen.len() > 3,
            "pet must roam its layer, saw {} positions",
            seen.len()
        );
    }

    #[test]
    fn begging_schedules_frame_flips() {
        let mut p = pet();
        let mut now = Instant::now();
        p.state = PetState::Beg {
            until: now + Duration::from_secs(3),
        };
        p.next_step = Some(now + Duration::from_millis(BEG_MS));
        let (_, timeout) = tick(&mut p, &mut now, &[], 60);
        assert!(
            timeout <= BEG_MS as i32,
            "beg frame flips must be scheduled, got {timeout}ms"
        );
    }

    #[test]
    fn reload_covering_the_target_and_food_re_clamps_them() {
        let mut p = pet();
        let mut now = Instant::now();
        // Mid-walk toward kibble that sits on the target spot.
        p.pos = 596.0;
        p.state = PetState::Walk {
            target: 640.0,
            after: After::Munch,
        };
        p.next_step = Some(now);
        p.food = Some(Food { x: 640.0 });
        // The reloaded pet layer places a button over both.
        let spans = vec![Span {
            left: 600.0,
            right: 690.0,
            clock: false,
        }];
        let mut reached_food = false;
        for i in 0..200 {
            tick(&mut p, &mut now, &spans, 60);
            if let Some(food) = &p.food {
                assert!(
                    span_at(&spans, food.x).is_none(),
                    "kibble must never sit on a button (iteration {i}: {})",
                    food.x
                );
            }
            assert!(
                span_at(&spans, p.pos).is_none(),
                "pet stood inside a button at iteration {i}: {}",
                p.pos
            );
            reached_food |= matches!(p.state, PetState::Munch { .. });
            if reached_food {
                break;
            }
        }
        assert!(reached_food, "the pet must still reach and eat the food");
        for _ in 0..100 {
            tick(&mut p, &mut now, &spans, 60);
        }
        assert!(p.satiety > 0.99, "meal eaten, got {}", p.satiety);
        assert!(p.food.is_none());
    }

    #[test]
    fn kibble_reclamps_redraw_only_when_moved() {
        let mut p = pet();
        let mut now = Instant::now();
        p.state = PetState::Idle {
            until: now + Duration::from_secs(60),
        };
        p.food = Some(Food { x: 640.0 });

        // A widget dropped over the kibble: the re-clamp moves it and must
        // repaint the moved sprite.
        let reload = vec![Span {
            left: 600.0,
            right: 690.0,
            clock: false,
        }];
        let (redraw, _) = tick(&mut p, &mut now, &reload, 60);
        let moved = p.food.unwrap().x;
        assert!((moved - 640.0).abs() > 0.01 && span_at(&reload, moved).is_none());
        assert!(redraw, "a moved kibble must repaint");

        // A single full-width widget leaves the kibble no standable spot:
        // it stays put and must not force a repaint on every quiet wake.
        let full = vec![Span {
            left: 0.0,
            right: WIDTH,
            clock: false,
        }];
        for _ in 0..10 {
            let (redraw, _) = tick(&mut p, &mut now, &full, 60);
            assert!(!redraw, "an unmoved kibble must not repaint");
        }
        assert!(span_at(&full, p.food.unwrap().x).is_some());
    }

    #[test]
    fn hungry_pets_beg_and_never_nap() {
        let mut p = pet();
        p.satiety = 0.1;
        let now = Instant::now();
        let mut seen_beg = false;
        for _ in 0..200 {
            p.decide(now);
            assert!(
                !matches!(p.state, PetState::Nap { .. })
                    && !matches!(
                        p.state,
                        PetState::Walk {
                            after: After::Nap,
                            ..
                        }
                    ),
                "hungry pet must not nap"
            );
            seen_beg |= matches!(p.state, PetState::Beg { .. });
        }
        assert!(seen_beg, "hungry pet must beg");

        // Pets below the nap threshold head for neither naps nor nap walks.
        p.satiety = 0.45;
        for _ in 0..200 {
            p.decide(now);
            assert!(
                !matches!(
                    p.state,
                    PetState::Walk {
                        after: After::Nap,
                        ..
                    }
                ),
                "pet below the nap threshold must not head for a nap"
            );
        }
    }

    #[test]
    fn well_fed_pets_do_nap_eventually() {
        let mut p = pet();
        p.satiety = 0.9;
        let mut now = Instant::now();
        let mut napped = false;
        for _ in 0..1000 {
            tick(&mut p, &mut now, &[], 500);
            napped |= matches!(p.state, PetState::Nap { .. });
        }
        assert!(napped, "a well-fed calm pet must nap sometimes");
    }

    #[test]
    fn naps_prefer_the_clock_widget() {
        let spans = vec![
            Span {
                left: 200.0,
                right: 400.0,
                clock: false,
            },
            Span {
                left: 1700.0,
                right: 2000.0,
                clock: true,
            },
        ];
        let half = half_sprite_w();
        let spot = nap_spot(&spans, 100.0, 30.0, 1978.0);
        assert!((spot - (1700.0 - half - NAP_GAP)).abs() < 0.01);
        // Without a clock, the pet curls up where it is.
        let no_clock = vec![Span {
            left: 200.0,
            right: 400.0,
            clock: false,
        }];
        assert!((nap_spot(&no_clock, 100.0, 30.0, 1978.0) - 100.0).abs() < 0.01);
    }

    #[test]
    fn satiety_decays_over_time_and_feeding_restores_it() {
        let mut p = pet();
        let mut now = Instant::now();
        p.satiety = 1.0;
        // Two calm hours pass.
        let mut begged_or_paced = false;
        for _ in 0..240 {
            tick(&mut p, &mut now, &[], 30_000);
            if p.satiety < HUNGRY {
                begged_or_paced |= matches!(p.state, PetState::Beg { .. } | PetState::Walk { .. });
            }
        }
        assert!(p.satiety < HUNGRY, "satiety must decay, got {}", p.satiety);
        assert!(
            begged_or_paced,
            "a hungry pet must beg or pace instead of sitting still"
        );

        // Feed through an adjacent tap.
        let food_x = p.pos + 100.0;
        assert!(
            p.tap(food_x, HEIGHT / 2.0, now, 0.0),
            "hungry pet must consume a food-drop tap"
        );
        assert_eq!(
            p.food.map(|f| f.x),
            Some(food_x.clamp(p.min_x(), p.max_x()))
        );
        assert!(matches!(
            p.state,
            PetState::Walk {
                after: After::Munch,
                ..
            }
        ));
        for _ in 0..2000 {
            tick(&mut p, &mut now, &[], 60);
            if matches!(p.state, PetState::Munch { .. }) {
                break;
            }
        }
        assert!(
            matches!(p.state, PetState::Munch { .. }),
            "pet must reach the food"
        );
        let mut heart_shown = false;
        for _ in 0..100 {
            tick(&mut p, &mut now, &[], 60);
            heart_shown |= p.happy_until.is_some();
        }
        assert!(
            p.satiety > 0.99,
            "a meal must restore satiety, got {}",
            p.satiety
        );
        assert!(p.food.is_none());
        assert!(heart_shown, "a content pet shows its happiness");
        assert!(p.happy_until.is_none(), "the heart must expire");
    }

    #[test]
    fn food_dropped_beside_a_widget_lands_clear_and_is_still_eaten() {
        let spans = vec![Span {
            left: 1500.0,
            right: 1800.0,
            clock: false,
        }];
        let mut p = pet();
        let mut now = Instant::now();
        p.satiety = 0.1;
        p.pos = 1430.0;
        p.state = PetState::Beg {
            until: now + Duration::from_secs(10),
        };
        p.spans = spans.clone();
        // A tap aimed inside the widget goes to the widget, never the pet.
        assert!(!p.tap(1550.0, HEIGHT / 2.0, now, 0.0));
        assert!(p.food.is_none());
        // The same tap just clear of its edge drops the kibble, clamped to
        // standable ground beside the widget.
        assert!(p.tap(1488.0, HEIGHT / 2.0, now, 0.0));
        let dropped = p.food.unwrap().x;
        assert_eq!(dropped, 1500.0 - half_sprite_w() - CLEAR_MARGIN);
        for _ in 0..3000 {
            tick(&mut p, &mut now, &spans, 60);
            if matches!(p.state, PetState::Munch { .. }) {
                break;
            }
        }
        assert!(matches!(p.state, PetState::Munch { .. }));
        for _ in 0..100 {
            tick(&mut p, &mut now, &spans, 60);
        }
        assert!(p.satiety > 0.99, "meal eaten, got {}", p.satiety);
    }

    #[test]
    fn open_ground_taps_drop_one_kibble_at_the_tap() {
        // The core feeding gesture: a tap away from the pet and any button
        // drops exactly one kibble right there, whatever the pet's mood,
        // and the pet sets out for it.
        let mut p = pet();
        let now = Instant::now();
        p.satiety = 0.9;
        let drop_x = 1500.0;
        assert!(p.tap(drop_x, HEIGHT / 2.0, now, 0.0));
        assert_eq!(p.food.map(|f| f.x), Some(drop_x));
        assert!(matches!(
            p.state,
            PetState::Walk {
                after: After::Munch,
                target: 1500.0
            }
        ));
        // The pet holds one kibble at a time: a second drop replaces the
        // waiting one and retargets the walk.
        assert!(p.tap(800.0, HEIGHT / 2.0, now, 0.0));
        assert_eq!(p.food.map(|f| f.x), Some(800.0));
        assert!(matches!(
            p.state,
            PetState::Walk {
                after: After::Munch,
                target: 800.0
            }
        ));
    }

    #[test]
    fn a_kibble_dropped_beyond_the_bar_lands_reachable_and_gets_eaten() {
        // Bounds: a drop aimed past the bar's edge clamps inside it, the
        // pet walks there and the meal restores satiety and clears the
        // kibble.
        let mut p = pet();
        let mut now = Instant::now();
        p.pos = 300.0;
        p.satiety = 0.5;
        assert!(p.tap(90000.0, HEIGHT / 2.0, now, 0.0));
        let dropped = p.food.unwrap().x;
        assert_eq!(dropped, p.max_x(), "the drop must clamp into the bar");
        let mut munched = false;
        for _ in 0..3000 {
            tick(&mut p, &mut now, &[], 60);
            if matches!(p.state, PetState::Munch { .. }) {
                munched = true;
                break;
            }
        }
        assert!(munched, "the pet must reach the clamped kibble");
        for _ in 0..100 {
            tick(&mut p, &mut now, &[], 60);
        }
        assert!(p.satiety > 0.99, "meal eaten, got {}", p.satiety);
        assert!(p.food.is_none(), "the eaten kibble must clear");
    }

    #[test]
    fn a_kibble_dropped_during_a_nap_waits_for_the_pet_to_wake() {
        // Nap gating survives the new gesture: the drop does not wake the
        // pet, and the meal is eaten only after the nap ends on its own.
        let mut p = pet();
        let mut now = Instant::now();
        p.satiety = 0.9;
        p.pos = 400.0;
        p.state = PetState::Nap {
            started: now,
            until: now + Duration::from_secs(2),
        };
        p.blink_on = true;
        p.next_step = None;
        assert!(p.tap(1500.0, HEIGHT / 2.0, now, 0.0));
        assert!(matches!(p.state, PetState::Nap { .. }), "the nap holds");
        assert_eq!(p.food.map(|f| f.x), Some(1500.0));
        // The nap runs its course untouched.
        for _ in 0..8 {
            tick(&mut p, &mut now, &[], 200);
            assert!(matches!(p.state, PetState::Nap { .. }));
        }
        // Then the pet wakes, heads for the kibble, and eats it.
        let mut munched = false;
        for _ in 0..3000 {
            tick(&mut p, &mut now, &[], 60);
            if matches!(p.state, PetState::Munch { .. }) {
                munched = true;
                break;
            }
        }
        assert!(munched, "the woken pet must still eat the waiting kibble");
        for _ in 0..100 {
            tick(&mut p, &mut now, &[], 60);
        }
        assert!(p.satiety > 0.99);
        assert!(p.food.is_none());
    }

    #[test]
    fn a_kibble_dropped_during_a_startle_is_eaten_after_the_flee() {
        // Startle survives the new gesture: the drop neither cancels the
        // startle nor its flee, and the meal follows once the pet lands.
        let mut p = pet();
        let mut now = Instant::now();
        p.pos = 1000.0;
        p.state = PetState::Startled {
            started: now,
            until: now + Duration::from_millis(STARTLE_MS),
            away: 1.0,
        };
        assert!(p.tap(400.0, HEIGHT / 2.0, now, 0.0));
        assert!(
            matches!(p.state, PetState::Startled { .. }),
            "the startle holds"
        );
        assert_eq!(p.food.map(|f| f.x), Some(400.0));
        let mut munched = false;
        for _ in 0..3000 {
            tick(&mut p, &mut now, &[], 60);
            if matches!(p.state, PetState::Munch { .. }) {
                munched = true;
                break;
            }
        }
        assert!(
            munched,
            "the startled pet must still eat the waiting kibble"
        );
        for _ in 0..100 {
            tick(&mut p, &mut now, &[], 60);
        }
        assert!(p.satiety > 0.99);
        assert!(p.food.is_none());
    }

    #[test]
    fn a_waiting_kibble_survives_a_layer_switch_pause() {
        // Pause/resume across layer switches survives the new gesture: the
        // kibble waits through the pause and the pet finishes the errand
        // after the layer reopens.
        let mut p = pet();
        let mut now = Instant::now();
        p.pos = 400.0;
        p.satiety = 0.5;
        assert!(p.tap(1500.0, HEIGHT / 2.0, now, 0.0));
        assert!(matches!(p.state, PetState::Walk { .. }));
        tick(&mut p, &mut now, &[], 60);
        // Another layer opens: the walk and the kibble freeze in place.
        now += Duration::from_millis(600_000);
        let (_, timeout) = p.update(now, &bar(false), &[]);
        assert_eq!(timeout, i32::MAX);
        assert_eq!(p.food.map(|f| f.x), Some(1500.0));
        assert!(
            !p.tap(1600.0, HEIGHT / 2.0, now, 0.0),
            "hidden pets drop nothing"
        );
        // Reopening resumes the walk to the waiting kibble.
        now += Duration::from_millis(60);
        p.update(now, &bar(true), &[]);
        let mut munched = false;
        for _ in 0..3000 {
            tick(&mut p, &mut now, &[], 60);
            if matches!(p.state, PetState::Munch { .. }) {
                munched = true;
                break;
            }
        }
        assert!(munched, "the resumed walk must still reach the kibble");
    }

    #[test]
    fn feeding_via_direct_tap_drops_food_beside_the_pet() {
        let mut p = pet();
        let now = Instant::now();
        p.satiety = 0.1;
        p.state = PetState::Beg {
            until: now + Duration::from_secs(10),
        };
        assert!(p.tap(p.pos, HEIGHT / 2.0, now, 0.0));
        let expected = (p.pos + p.facing * (p.half_w() + 14.0)).clamp(p.min_x(), p.max_x());
        assert_eq!(p.food.map(|f| f.x), Some(expected));
        assert!(matches!(
            p.state,
            PetState::Walk {
                after: After::Munch,
                ..
            }
        ));
    }

    #[test]
    fn direct_tap_when_fed_startles_and_is_consumed() {
        let mut p = pet();
        let mut now = Instant::now();
        p.satiety = 0.9;
        assert!(
            p.tap(p.pos, HEIGHT / 2.0, now, 0.0),
            "direct tap is always consumed"
        );
        assert!(matches!(p.state, PetState::Startled { .. }));
        let start = p.pos;
        for _ in 0..100 {
            tick(&mut p, &mut now, &[], 60);
        }
        assert!(
            !matches!(p.state, PetState::Startled { .. }),
            "startle must end"
        );
        assert!((p.pos - start).abs() > 10.0, "pet must flee some distance");
    }

    #[test]
    fn taps_only_count_near_an_active_pet() {
        let mut p = pet();
        let now = Instant::now();
        p.satiety = 0.9;
        // On a bare bar every stretch is open ground, so any tap away from
        // the pet drops food - hunger no longer gates the handout.
        assert!(
            p.tap(p.pos + 400.0, HEIGHT / 2.0, now, 0.0),
            "open-ground taps drop food even for a satiated pet"
        );
        assert_eq!(p.food.map(|f| f.x), Some(p.pos + 400.0));
        // Hidden (other layer) pets never consume.
        let mut q = pet();
        let mut t = Instant::now();
        q.update(t, &bar(false), &[]);
        t += Duration::from_millis(100);
        q.update(t, &bar(false), &[]);
        assert!(!q.is_visible());
        assert!(!q.tap(q.pos, HEIGHT / 2.0, t, 0.0));
        // Pixel-shifted pets hit-test in screen space.
        let shift = 11.0;
        assert!(p.tap(p.pos + shift, HEIGHT / 2.0, now, shift));
        assert!(matches!(p.state, PetState::Startled { .. }));
    }

    #[test]
    fn food_drops_never_steal_taps_aimed_at_widgets() {
        // Precedence: the pet itself first, open ground second, and widget
        // spans never - a tap inside a button passes through whether the
        // pet is hungry or not.
        let spans = vec![Span {
            left: 500.0,
            right: 700.0,
            clock: false,
        }];
        let mut p = pet();
        let now = Instant::now();
        p.pos = 900.0;
        p.spans = spans.clone();
        for satiety in [0.9, 0.1] {
            p.satiety = satiety;
            assert!(
                !p.tap(600.0, HEIGHT / 2.0, now, 0.0),
                "a widget tap must pass through (satiety {satiety})"
            );
            assert!(p.food.is_none(), "no kibble may appear on a widget");
        }
        // Open ground beside the button is consumed as a drop, in layout
        // coordinates even under pixel shift.
        assert!(p.tap(850.0, HEIGHT / 2.0, now, 11.0));
        assert_eq!(p.food.map(|f| f.x), Some(850.0 - 11.0));
        // A tap on the pet still wins over everything nearby.
        p.satiety = 0.9;
        assert!(p.tap(p.pos, HEIGHT / 2.0, now, 0.0));
        assert!(matches!(p.state, PetState::Startled { .. }));
        // A munching pet passes open-ground taps through: its meal is not
        // replaced and the kibble in progress stays put.
        p.satiety = 0.5;
        p.state = PetState::Munch {
            started: now,
            until: now + Duration::from_millis(MUNCH_MS),
        };
        p.food = Some(Food { x: p.pos });
        assert!(!p.tap(400.0, HEIGHT / 2.0, now, 0.0));
        assert_eq!(p.food.map(|f| f.x), Some(p.pos));
        assert!(matches!(p.state, PetState::Munch { .. }));
    }

    #[test]
    fn reload_transitions_preserve_the_pet_and_apply_new_knobs() {
        let mut p = pet();
        let start_pos = p.pos;
        let nap_until = Instant::now() + Duration::from_secs(30);
        p.state = PetState::Nap {
            started: Instant::now(),
            until: nap_until,
        };
        // Disabling hides the pet without forgetting it.
        p.apply_config(false, PetVariant::Cat, PetEnergy::Calm);
        let mut now = Instant::now();
        let (redraw, _) = tick(&mut p, &mut now, &[], 100);
        assert!(redraw, "hiding must trigger a clearing redraw");
        assert!(!p.is_visible());
        assert!(!p.tap(p.pos, HEIGHT / 2.0, now, 0.0));
        // Re-enabling restores it where it was.
        p.apply_config(true, PetVariant::Cat, PetEnergy::Calm);
        let (redraw, _) = tick(&mut p, &mut now, &[], 100);
        assert!(redraw);
        assert!(p.is_visible());
        assert!(
            (p.pos - start_pos).abs() < 0.01,
            "pet position must survive a reload"
        );
        assert!(
            matches!(p.state, PetState::Nap { .. }),
            "nap must survive a reload"
        );
        // Energy changes the cadence.
        p.apply_config(true, PetVariant::Cat, PetEnergy::Rowdy);
        assert_eq!(p.cadence().step_ms, ROWDY.step_ms);
    }

    #[test]
    fn the_pet_pauses_off_its_layer_and_resumes_with_state_intact() {
        let mut p = pet();
        let mut now = Instant::now();
        p.satiety = 0.9;
        // Mid-walk when its layer closes.
        p.pos = 400.0;
        p.state = PetState::Walk {
            target: 1200.0,
            after: After::Pause,
        };
        p.next_step = Some(now);
        tick(&mut p, &mut now, &[], 60);
        assert!(p.is_visible());

        // Closing the layer repaints once, needs no wakeups, and passes
        // taps through while the pet waits unseen.
        now += Duration::from_millis(60);
        let (redraw, timeout) = p.update(now, &bar(false), &[]);
        assert!(redraw, "closing the layer must repaint");
        assert_eq!(timeout, i32::MAX);
        assert!(!p.is_visible());
        assert!(!p.tap(p.pos, HEIGHT / 2.0, now, 0.0));

        // Ten minutes on another layer: the walk freezes in place, while
        // the metabolism keeps running unseen.
        now += Duration::from_millis(600_000);
        let (_, timeout) = p.update(now, &bar(false), &[]);
        assert_eq!(timeout, i32::MAX);
        let (frozen_pos, frozen_state) = (p.pos, p.state);
        now += Duration::from_millis(60);
        p.update(now, &bar(false), &[]);
        assert_eq!(p.pos, frozen_pos, "the walk must freeze while paused");
        assert_eq!(p.state, frozen_state);
        assert!(
            p.satiety < 0.85,
            "metabolism keeps running, got {}",
            p.satiety
        );

        // Reopening resumes the walk from the same spot - one step, not a
        // teleport to the target - and the walk then completes.
        now += Duration::from_millis(60);
        let (redraw, _) = p.update(now, &bar(true), &[]);
        assert!(redraw, "reopening must repaint");
        assert!(p.is_visible());
        assert!(
            matches!(p.state, PetState::Walk { .. }),
            "the walk resumes, got {:?}",
            p.state
        );
        assert!(
            (p.pos - frozen_pos).abs() <= CALM.px_per_step,
            "resume takes one step, not a teleport: {} -> {}",
            frozen_pos,
            p.pos
        );
        let mut reached = false;
        for _ in 0..3000 {
            tick(&mut p, &mut now, &[], 60);
            reached |= (p.pos - 1200.0).abs() <= CALM.px_per_step;
        }
        assert!(
            reached,
            "the resumed walk reaches its target, got {}",
            p.pos
        );
    }

    #[test]
    fn sprites_render_distinctly_and_stay_inside_the_bar() {
        fn render(p: &PetManager) -> Vec<u8> {
            let mut surface =
                ImageSurface::create(Format::ARgb32, HEIGHT as i32, WIDTH as i32).unwrap();
            {
                let c = Context::new(&surface).unwrap();
                c.translate(HEIGHT, 0.0);
                c.rotate((90.0f64).to_radians());
                p.draw(&c, HEIGHT as i32, (0.0, 0.0), Instant::now());
            }
            let pixels = surface.data().unwrap().to_vec();
            pixels
        }
        let mut walking = pet();
        let mut now = Instant::now();
        walking.state = PetState::Walk {
            target: walking.pos + 300.0,
            after: After::Pause,
        };
        walking.next_step = Some(now);
        tick(&mut walking, &mut now, &[], 60);
        let walk_image = render(&walking);

        let mut napping = pet();
        napping.state = PetState::Nap {
            started: Instant::now(),
            until: Instant::now() + Duration::from_secs(30),
        };
        let nap_image = render(&napping);
        assert_ne!(walk_image, nap_image);

        // A fox is not a cat.
        let mut fox = pet();
        fox.apply_config(true, PetVariant::Fox, PetEnergy::Calm);
        fox.state = PetState::Nap {
            started: Instant::now(),
            until: Instant::now() + Duration::from_secs(30),
        };
        assert_ne!(render(&fox), nap_image);

        // Edge positions keep the sprite on the surface.
        let mut edge = pet();
        edge.pos = edge.max_x();
        edge.state = PetState::Nap {
            started: Instant::now(),
            until: Instant::now() + Duration::from_secs(30),
        };
        assert!(
            edge.pos + edge.half_w() <= WIDTH,
            "sprite must not clip off the right edge"
        );
        render(&edge);
        edge.pos = edge.min_x();
        assert!(edge.pos - edge.half_w() >= 0.0);
        render(&edge);
    }

    #[test]
    fn layer_spans_report_buttons_and_the_clock() {
        let layer = FunctionLayer::with_config(vec![
            ButtonConfig {
                text: Some("A".into()),
                ..ButtonConfig::default()
            },
            ButtonConfig::default(), // spacer
            ButtonConfig {
                time: Some("24hr".into()),
                ..ButtonConfig::default()
            },
        ])
        .unwrap();
        let spans = layer.layout_spans(WIDTH as i32, 0);
        assert_eq!(spans.len(), 2, "spacers are free space");
        assert!(spans[0].left < spans[0].right);
        assert!(spans[1].clock, "the time widget is marked as the nap spot");
        assert!(spans[0].left < spans[1].left);
        let no_clock = FunctionLayer::with_config(vec![ButtonConfig {
            text: Some("A".into()),
            ..ButtonConfig::default()
        }])
        .unwrap();
        assert!(!no_clock.layout_spans(WIDTH as i32, 0)[0].clock);
        // Pixel-shifted spans follow the drawn geometry: buttons compress
        // into width minus the shift and center by half of it.
        let single = FunctionLayer::with_config(vec![ButtonConfig {
            time: Some("24hr".into()),
            ..ButtonConfig::default()
        }])
        .unwrap();
        assert_eq!(
            single.layout_spans(WIDTH as i32, 0),
            vec![Span {
                left: 0.0,
                right: WIDTH,
                clock: true
            }]
        );
        assert_eq!(
            single.layout_spans(WIDTH as i32, 22),
            vec![Span {
                left: 11.0,
                right: WIDTH - 11.0,
                clock: true
            }]
        );
    }

    #[test]
    fn grid_frames_are_well_formed() {
        for grid in [
            WALK_A, WALK_B, SIT, BEG_A, BEG_B, SLEEP, STARTLE, MUNCH_A, MUNCH_B,
        ] {
            assert!(!grid.is_empty());
            assert!(grid.iter().all(|row| !row.is_empty()));
            assert!(grid_w(grid) > 0.0);
            // Every frame uses only known palette characters.
            assert!(grid
                .iter()
                .flat_map(|row| row.chars())
                .all(|ch| matches!(ch, '.' | 'B' | 'S' | 'D' | 'W' | 'N' | 'P' | 'E')));
        }
        for (grid, ink) in [
            (CLOUD, 'C'),
            (SUN, 'Y'),
            (MOON, 'M'),
            (STAR, 'S'),
            (KIBBLE, 'K'),
        ] {
            assert!(grid
                .iter()
                .flat_map(|row| row.chars())
                .all(|ch| ch == '.' || ch == ink));
        }
    }

    #[test]
    fn the_pet_layer_paints_a_scene_of_sky_ground_sun_and_clouds() {
        // The pet layer's backdrop: blue sky above a green ground strip in
        // the open stretch beside the Home button, with a sun and clouds;
        // button columns stay untouched.
        let mut p = pet();
        let spans = vec![Span {
            left: 11.0,
            right: 248.0,
            clock: false,
        }];
        p.update(Instant::now(), &bar(true), &spans);
        // Park the cat away from the scanned decoration bands.
        p.pos = 1500.0;
        let mut surface =
            ImageSurface::create(Format::ARgb32, HEIGHT as i32, WIDTH as i32).unwrap();
        {
            let c = Context::new(&surface).unwrap();
            c.translate(HEIGHT, 0.0);
            c.rotate((90.0f64).to_radians());
            p.draw(&c, HEIGHT as i32, (0.0, 0.0), Instant::now());
        }
        let stride = surface.stride() as usize;
        let data = surface.data().unwrap();
        // Layout (x, y) lands at device (HEIGHT - y, x) under the transform.
        let pixel = |x: i32, y: i32| -> (u8, u8, u8) {
            let i = (x as usize) * stride + (HEIGHT as usize - y as usize) * 4;
            (data[i + 2], data[i + 1], data[i])
        };
        let sky = pixel(1200, 10);
        assert!(
            sky.2 > 200 && sky.1 > 150 && sky.0 < 160,
            "sky must be blue, got {sky:?}"
        );
        let ground = pixel(1200, 56);
        assert!(
            ground.1 > 140 && ground.0 < ground.1 && ground.2 < ground.1,
            "ground must be green, got {ground:?}"
        );
        let (r, g, b, a) = {
            let i = (100usize) * stride + (HEIGHT as usize - 10) * 4;
            (data[i + 2], data[i + 1], data[i], data[i + 3])
        };
        assert_eq!((r, g, b, a), (0, 0, 0, 0), "button columns stay untouched");
        let any = |x0: i32, x1: i32, y0: i32, y1: i32, pred: &dyn Fn((u8, u8, u8)) -> bool| {
            (x0..x1)
                .step_by(4)
                .any(|x| (y0..y1).step_by(2).any(|y| pred(pixel(x, y))))
        };
        assert!(
            any(400, 1300, 6, 26, &|p| p.0 > 230 && p.1 > 230 && p.2 > 230),
            "a cloud must float in the sky"
        );
        assert!(
            any(1600, 2008, 4, 30, &|p| p.0 > 220 && p.1 > 170 && p.2 < 120),
            "the sun must shine in the sky"
        );
    }

    #[test]
    fn day_parts_follow_the_default_band_hours() {
        // The pet's day mirrors the background tint's default bands:
        // morning from 5, day from 12, evening from 17, night from 21
        // wrapping past midnight.
        for (hour, part) in [
            (0, DayPart::Night),
            (4, DayPart::Night),
            (5, DayPart::Morning),
            (11, DayPart::Morning),
            (12, DayPart::Day),
            (16, DayPart::Day),
            (17, DayPart::Evening),
            (20, DayPart::Evening),
            (21, DayPart::Night),
            (23, DayPart::Night),
        ] {
            assert_eq!(DayPart::from_hour(hour), part, "hour {hour}");
        }
    }

    #[test]
    fn the_mood_bends_with_the_part_of_day() {
        let mut p = pet();
        let cadence = |p: &mut PetManager, part: DayPart| {
            p.day_part = part;
            p.cadence()
        };
        let morning = cadence(&mut p, DayPart::Morning);
        let day = cadence(&mut p, DayPart::Day);
        let evening = cadence(&mut p, DayPart::Evening);
        let night = cadence(&mut p, DayPart::Night);
        // Idles and naps stretch from morning to night, naps grow likelier
        // (capping at certainty), and the urge to wander fades.
        assert!(morning.idle_ms.1 < day.idle_ms.1, "morning idles shrink");
        assert!(day.idle_ms.1 < evening.idle_ms.1, "evening idles grow");
        assert!(evening.idle_ms.1 < night.idle_ms.1, "night idles linger");
        assert!(morning.nap_ms.1 < day.nap_ms.1 && day.nap_ms.1 < night.nap_ms.1);
        assert!(morning.nap_chance < day.nap_chance);
        assert!(day.nap_chance < evening.nap_chance);
        assert_eq!(night.nap_chance, 1.0, "night naps cap at certainty");
        assert!(morning.wander_chance > day.wander_chance);
        assert!(day.wander_chance > evening.wander_chance);
        assert!(evening.wander_chance > night.wander_chance);
        // The stride and the metabolism are the pet's own, not the clock's.
        assert_eq!(morning.step_ms, day.step_ms);
        assert_eq!(night.step_ms, day.step_ms);
        assert_eq!(night.px_per_step, day.px_per_step);
        assert_eq!(night.decay_per_ms, day.decay_per_ms);
    }

    #[test]
    fn morning_pets_roam_more_and_evenings_wind_down() {
        // Naps out of the picture (satiety under the nap threshold, well
        // above hungry), so the only difference left is the wander urge.
        let mut p = pet();
        p.satiety = 0.45;
        let now = Instant::now();
        let walks = |p: &mut PetManager, part: DayPart| {
            p.day_part = part;
            let mut walks = 0;
            for _ in 0..1000 {
                p.decide(now);
                walks += matches!(p.state, PetState::Walk { .. }) as u32;
            }
            walks
        };
        let morning = walks(&mut p, DayPart::Morning);
        let day = walks(&mut p, DayPart::Day);
        let evening = walks(&mut p, DayPart::Evening);
        assert!(
            morning > day + 20,
            "mornings bring zoomies, got {morning} vs {day}"
        );
        assert!(
            day > evening + 60,
            "evenings wind down, got {day} vs {evening}"
        );
    }

    #[test]
    fn night_pets_sleep_it_off_until_hunger_wins() {
        let mut p = pet();
        p.satiety = 0.9;
        p.day_part = DayPart::Night;
        let now = Instant::now();
        for _ in 0..100 {
            p.decide(now);
            assert!(
                matches!(p.state, PetState::Nap { .. })
                    || matches!(
                        p.state,
                        PetState::Walk {
                            after: After::Nap,
                            ..
                        }
                    ),
                "a well-fed night pet heads for bed, got {:?}",
                p.state
            );
        }
        // The night's naps also run longer than the day's.
        p.day_part = DayPart::Day;
        let day = p.cadence();
        p.day_part = DayPart::Night;
        let night = p.cadence();
        assert!(night.nap_ms.0 > day.nap_ms.1);
        // Hunger overrides sleepiness, exactly as by day.
        p.satiety = 0.1;
        for _ in 0..50 {
            p.decide(now);
            assert!(!matches!(p.state, PetState::Nap { .. }));
        }
    }

    #[test]
    fn the_scene_follows_the_time_of_day() {
        // The backdrop complements the background tint: a pale morning sky
        // with a low sun climbing on the left, the day look, a warm dusk
        // with the sun sinking low on the right, and a dark night with a
        // moon and stars where the sun stood. Under DFR_DECK_REVIEW_OUTPUT
        // the four scenes land as horizontal PNGs for offline visual
        // review, alongside the feeding frames.
        let spans = vec![Span {
            left: 11.0,
            right: 248.0,
            clock: false,
        }];
        let render = |hour: u32, name: &str| -> Vec<u8> {
            let mut p = PetManager::new(true, PetVariant::Cat, PetEnergy::Calm);
            p.update(
                Instant::now(),
                &Bar {
                    width: WIDTH,
                    height: HEIGHT,
                    active: true,
                    hour,
                },
                &spans,
            );
            // Park the cat, clear of every band sampled below.
            p.pos = 1500.0;
            p.state = PetState::Idle {
                until: Instant::now() + Duration::from_secs(60),
            };
            let mut portrait =
                ImageSurface::create(Format::ARgb32, HEIGHT as i32, WIDTH as i32).unwrap();
            {
                let c = Context::new(&portrait).unwrap();
                c.translate(HEIGHT, 0.0);
                c.rotate((90.0f64).to_radians());
                p.draw(&c, HEIGHT as i32, (0.0, 0.0), Instant::now());
            }
            if let Some(dir) = std::env::var_os("DFR_DECK_REVIEW_OUTPUT") {
                let horizontal =
                    ImageSurface::create(Format::ARgb32, WIDTH as i32, HEIGHT as i32).unwrap();
                let c = Context::new(&horizontal).unwrap();
                c.translate(0.0, HEIGHT);
                c.rotate((-90.0_f64).to_radians());
                c.set_source_surface(&portrait, 0.0, 0.0).unwrap();
                c.paint().unwrap();
                std::fs::create_dir_all(&dir).unwrap();
                horizontal
                    .write_to_png(
                        &mut std::fs::File::create(std::path::Path::new(&dir).join(name)).unwrap(),
                    )
                    .unwrap();
            }
            let pixels = portrait.data().unwrap().to_vec();
            pixels
        };
        let stride = {
            let probe = ImageSurface::create(Format::ARgb32, HEIGHT as i32, WIDTH as i32).unwrap();
            probe.stride() as usize
        };
        // Layout (x, y) lands at device (HEIGHT - y, x) under the transform.
        let pixel = |data: &[u8], x: i32, y: i32| -> (u8, u8, u8) {
            let i = x as usize * stride + (HEIGHT as usize - y as usize) * 4;
            (data[i + 2], data[i + 1], data[i])
        };
        let any = |data: &[u8],
                   x0: i32,
                   x1: i32,
                   y0: i32,
                   y1: i32,
                   pred: &dyn Fn((u8, u8, u8)) -> bool| {
            (x0..x1)
                .step_by(4)
                .any(|x| (y0..y1).step_by(2).any(|y| pred(pixel(data, x, y))))
        };

        let morning = render(6, "pet-scene-morning.png");
        let morning_sky = pixel(&morning, 1200, 10);
        assert!(
            morning_sky.2 > 200 && morning_sky.2 > morning_sky.0,
            "morning sky must be pale blue, got {morning_sky:?}"
        );
        assert!(
            any(&morning, 380, 1000, 14, 44, &|p| p.0 > 230
                && p.1 > 170
                && p.2 < 120),
            "the morning sun must climb low on the left"
        );

        let day = render(14, "pet-scene-day.png");
        let day_sky = pixel(&day, 1200, 10);
        assert!(
            day_sky.2 > 200 && day_sky.2 > day_sky.0 && day_sky.0 < 160,
            "the day sky stays blue, got {day_sky:?}"
        );

        let evening = render(18, "pet-scene-evening.png");
        let evening_sky = pixel(&evening, 1200, 10);
        assert!(
            evening_sky.0 > 180 && evening_sky.0 > evening_sky.2,
            "the evening sky must turn warm, got {evening_sky:?}"
        );
        assert!(
            any(&evening, 1650, 1990, 14, 46, &|p| p.0 > 220
                && (120..190).contains(&p.1)
                && p.2 < 120),
            "the sunset sun must sink low on the right"
        );

        let night = render(22, "pet-scene-night.png");
        let night_sky = pixel(&night, 1200, 10);
        assert!(
            night_sky.2 > night_sky.0 && night_sky.0 < 80,
            "the night sky must be dark and cool, got {night_sky:?}"
        );
        assert!(
            any(&night, 1650, 1990, 0, 30, &|p| p.0 > 200
                && p.1 > 200
                && p.2 > 200),
            "the moon must hang where the sun stood"
        );
        assert!(
            any(&night, 500, 1250, 4, 30, &|p| p.0 > 200
                && p.1 > 200
                && p.2 > 200),
            "stars must twinkle in the night sky"
        );
        assert!(
            !any(&night, 250, 2008, 0, 46, &|p| p.0 > 220
                && p.1 > 170
                && p.2 < 120),
            "no sun may shine at night"
        );

        assert_ne!(morning, day);
        assert_ne!(day, evening);
        assert_ne!(evening, night);
        assert_ne!(morning, night);
    }

    #[test]
    fn food_drop_walk_and_munch_frames_render_distinctly() {
        // The feeding gesture in three frames: the dropped kibble, the walk
        // toward it, and the munch. Under DFR_DECK_REVIEW_OUTPUT the frames
        // land in that directory as horizontal PNGs for offline visual
        // review, the same export path as the multibar review frames.
        let mut p = pet();
        let mut now = Instant::now();
        p.satiety = 0.6;
        p.pos = 300.0;
        let frame = |p: &PetManager, name: &str| -> Vec<u8> {
            let mut portrait =
                ImageSurface::create(Format::ARgb32, HEIGHT as i32, WIDTH as i32).unwrap();
            {
                let c = Context::new(&portrait).unwrap();
                c.translate(HEIGHT, 0.0);
                c.rotate((90.0f64).to_radians());
                p.draw(&c, HEIGHT as i32, (0.0, 0.0), Instant::now());
            }
            let pixels = portrait.data().unwrap().to_vec();
            if let Some(dir) = std::env::var_os("DFR_DECK_REVIEW_OUTPUT") {
                let horizontal =
                    ImageSurface::create(Format::ARgb32, WIDTH as i32, HEIGHT as i32).unwrap();
                let c = Context::new(&horizontal).unwrap();
                c.translate(0.0, HEIGHT);
                c.rotate((-90.0_f64).to_radians());
                c.set_source_surface(&portrait, 0.0, 0.0).unwrap();
                c.paint().unwrap();
                std::fs::create_dir_all(&dir).unwrap();
                horizontal
                    .write_to_png(
                        &mut std::fs::File::create(std::path::Path::new(&dir).join(name)).unwrap(),
                    )
                    .unwrap();
            }
            pixels
        };

        // The drop: a kibble lands on open ground, the pet turns for it.
        assert!(p.tap(1400.0, HEIGHT / 2.0, now, 0.0));
        let drop_frame = frame(&p, "pet-food-drop.png");
        // The walk: caught mid-stride, kibble still waiting ahead.
        for _ in 0..60 {
            tick(&mut p, &mut now, &[], CALM.step_ms);
        }
        assert!(matches!(p.state, PetState::Walk { .. }));
        assert!((p.pos - 1400.0).abs() > 50.0, "still mid-walk at {}", p.pos);
        let walk_frame = frame(&p, "pet-food-walk.png");
        // The munch: arrived and eating, kibble gone afterwards.
        for _ in 0..3000 {
            tick(&mut p, &mut now, &[], CALM.step_ms);
            if matches!(p.state, PetState::Munch { .. }) {
                break;
            }
        }
        assert!(matches!(p.state, PetState::Munch { .. }));
        let munch_frame = frame(&p, "pet-food-munch.png");
        assert_ne!(drop_frame, walk_frame);
        assert_ne!(walk_frame, munch_frame);
        assert_ne!(drop_frame, munch_frame);
    }
}
