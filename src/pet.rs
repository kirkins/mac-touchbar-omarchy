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
//! consumed tap never activates the button underneath; the pet pauses while
//! any other layer is displayed and resumes with its state intact.

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
/// Taps this far to either side of the pet drop food while it is hungry.
const FOOD_ZONE: f64 = 150.0;
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

/// Bar geometry and visibility for one tick. `active` gates the pet to its
/// own layer, the only place it is allowed to wander and drawn.
pub struct Bar {
    pub width: f64,
    pub height: f64,
    pub active: bool,
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

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum PetVariant {
    #[default]
    Cat,
    Fox,
}

impl PetVariant {
    pub fn parse(value: &str) -> Result<Self, String> {
        match value {
            "cat" => Ok(Self::Cat),
            "fox" => Ok(Self::Fox),
            _ => Err(format!(
                "PetVariant must be one of: cat, fox (got {value:?})"
            )),
        }
    }

    fn palette(self) -> Palette {
        match self {
            Self::Cat => Palette {
                body: (0.95, 0.55, 0.18),
                stripe: (0.72, 0.33, 0.10),
                dark: (0.25, 0.13, 0.08),
                light: (0.98, 0.95, 0.90),
                nose: (0.98, 0.55, 0.62),
                ear: (0.97, 0.62, 0.66),
                eye: (0.10, 0.55, 0.25),
            },
            Self::Fox => Palette {
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
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum PetEnergy {
    #[default]
    Calm,
    Rowdy,
}

impl PetEnergy {
    pub fn parse(value: &str) -> Result<Self, String> {
        match value {
            "calm" => Ok(Self::Calm),
            "rowdy" => Ok(Self::Rowdy),
            _ => Err(format!(
                "PetEnergy must be one of: calm, rowdy (got {value:?})"
            )),
        }
    }
}

/// Movement and metabolism parameters per energy dial position.
struct Cadence {
    step_ms: u64,
    px_per_step: f64,
    idle_ms: (u64, u64),
    nap_ms: (u64, u64),
    nap_chance: f64,
    /// Satiety lost per millisecond; rowdy pets burn through meals faster.
    decay_per_ms: f64,
}

const CALM: Cadence = Cadence {
    step_ms: 280,
    px_per_step: 5.0,
    idle_ms: (900, 3200),
    nap_ms: (9000, 20000),
    nap_chance: 0.25,
    decay_per_ms: 1.0 / 3_600_000.0, // full to hungry in about an hour
};

const ROWDY: Cadence = Cadence {
    step_ms: 160,
    px_per_step: 9.0,
    idle_ms: (300, 1200),
    nap_ms: (6000, 12000),
    nap_chance: 0.12,
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
// button keeps its normal deck look.
const CLOUD: &[&str] = &["..CCCC..", ".CCCCCC.", "CCCCCCCC"];
const SUN: &[&str] = &["..YY..", ".YYYY.", "YYYYYY", "YYYYYY", ".YYYY.", "..YY.."];
const CLOUD_RGB: (f64, f64, f64) = (0.97, 0.99, 1.0);
const SUN_RGB: (f64, f64, f64) = (1.0, 0.85, 0.25);
/// Fraction of the bar height where the ground strip begins; the pet's
/// feet rest inside it.
const GROUND_TOP: f64 = 0.80;
const SKY_RGB: (f64, f64, f64) = (0.47, 0.72, 0.96);
const GROUND_RGB: (f64, f64, f64) = (0.35, 0.68, 0.32);
const GRASS_RGB: (f64, f64, f64) = (0.22, 0.50, 0.22);

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

    fn cadence(&self) -> &'static Cadence {
        match self.energy {
            PetEnergy::Calm => &CALM,
            PetEnergy::Rowdy => &ROWDY,
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
        if receptive && self.satiety < HUNGRY && (x - screen_x).abs() <= FOOD_ZONE {
            // A hungry pet takes nearby taps as food drops.
            self.spawn_food(x - shift_x, now);
            return true;
        }
        false
    }

    fn spawn_food(&mut self, x: f64, now: Instant) {
        let x = clamp_outside(
            x.clamp(self.min_x(), self.max_x()),
            &self.spans,
            self.min_x(),
            self.max_x(),
        );
        self.food = Some(Food { x });
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
        let palette = self.variant.palette();
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

    /// The pet layer's backdrop: blue sky above a green ground strip in
    /// every open stretch of the layer, plus a sun and clouds in the sky.
    /// Button columns are left untouched so widgets keep their deck look.
    fn draw_scene(&self, c: &Context, height: f64, shift: (f64, f64)) {
        let ground_top = height * GROUND_TOP;
        let overdraw = 16.0;
        for (a, b, reaches_right_edge) in free_intervals(&self.spans, self.width) {
            let (a, mut b) = (a + shift.0, b + shift.0);
            if reaches_right_edge {
                b += overdraw;
            }
            c.set_source_rgb(SKY_RGB.0, SKY_RGB.1, SKY_RGB.2);
            c.rectangle(
                a,
                shift.1 - overdraw,
                b - a,
                ground_top - shift.1 + overdraw,
            );
            c.fill().unwrap();
            c.set_source_rgb(GROUND_RGB.0, GROUND_RGB.1, GROUND_RGB.2);
            c.rectangle(
                a,
                ground_top + shift.1,
                b - a,
                height - ground_top + overdraw,
            );
            c.fill().unwrap();
            c.set_source_rgb(GRASS_RGB.0, GRASS_RGB.1, GRASS_RGB.2);
            c.rectangle(a, ground_top + shift.1, b - a, 2.5);
            c.fill().unwrap();
        }
        // The sun and clouds float over the widest stretch of open sky.
        let Some((a, b, _)) = free_intervals(&self.spans, self.width)
            .into_iter()
            .max_by(|x, y| (x.1 - x.0).total_cmp(&(y.1 - y.0)))
            .filter(|&(a, b, _)| b - a > 240.0)
        else {
            return;
        };
        let span = b - a;
        draw_grid(
            c,
            SUN,
            a + span * 0.86 + shift.0,
            30.0 + shift.1,
            |ch| (ch == 'Y').then_some(SUN_RGB),
            false,
        );
        draw_grid(
            c,
            CLOUD,
            a + span * 0.22 + shift.0,
            20.0 + shift.1,
            |ch| (ch == 'C').then_some(CLOUD_RGB),
            false,
        );
        draw_grid(
            c,
            CLOUD,
            a + span * 0.52 + shift.0,
            26.0 + shift.1,
            |ch| (ch == 'C').then_some(CLOUD_RGB),
            false,
        );
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
        } else if rng.gen_bool(0.85) {
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
        let layer = FunctionLayer::with_config(keys);
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
        ]);
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
    fn food_dropped_on_a_widget_lands_beside_it_and_is_still_eaten() {
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
        // Tap aimed inside the widget while the hungry pet is nearby.
        assert!(p.tap(1550.0, HEIGHT / 2.0, now, 0.0));
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
        assert!(
            !p.tap(p.pos + 400.0, HEIGHT / 2.0, now, 0.0),
            "far tap passes through to the widget underneath"
        );
        // Satiated pets ignore food drops: the widget underneath keeps working.
        assert!(!p.tap(p.pos + 100.0, HEIGHT / 2.0, now, 0.0));
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
        ]);
        let spans = layer.layout_spans(WIDTH as i32, 0);
        assert_eq!(spans.len(), 2, "spacers are free space");
        assert!(spans[0].left < spans[0].right);
        assert!(spans[1].clock, "the time widget is marked as the nap spot");
        assert!(spans[0].left < spans[1].left);
        let no_clock = FunctionLayer::with_config(vec![ButtonConfig {
            text: Some("A".into()),
            ..ButtonConfig::default()
        }]);
        assert!(!no_clock.layout_spans(WIDTH as i32, 0)[0].clock);
        // Pixel-shifted spans follow the drawn geometry: buttons compress
        // into width minus the shift and center by half of it.
        let single = FunctionLayer::with_config(vec![ButtonConfig {
            time: Some("24hr".into()),
            ..ButtonConfig::default()
        }]);
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
        for (grid, ink) in [(CLOUD, 'C'), (SUN, 'Y'), (KIBBLE, 'K')] {
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
}
