//! The game's own clock, read as a fractional day.
//!
//! `DayNightCycle` counts whole days in `DayNumber` and the ticks elapsed
//! within the current one in `_ticksPassedToday`, so the two together are the
//! game's position in time to a resolution of one tick:
//!
//! ```text
//! day = DayNumber + _ticksPassedToday / ticks_per_day
//! ```
//!
//! Nothing here reads the tick count *per day* directly -- the game keeps that
//! on `DayNightCycleSpec`, one pointer away, and the same number falls out of
//! three fields already on the clock:
//!
//! ```text
//! ticks_per_day = (DaytimeLengthInHours + NighttimeLengthInHours)
//!                 / FixedDeltaTimeInHours
//! ```
//!
//! which saves following a pointer and keeps every read on an address the
//! splitter already holds. Vanilla is 768 ticks to a day -- 24 in-game hours
//! at 0.03125 hours a tick, which is `TickTimeSpec.TickIntervalInSeconds` of
//! 0.6 seconds of real time at 1x speed. Read out of two builds' captures
//! rather than assumed: 0.03125 hours a tick and a day of 16 + 8 hours,
//! identical on 1.0.13.1 and 1.1.2.4, and so 768.0 both times.
//!
//! The three length constants read as zero until the game has finished
//! loading, so they are read until they are usable and then kept. The two
//! counters are read every tick.
//!
//! # Between ticks
//!
//! Those counters only move once a game tick, which is 0.6s of real time at
//! 1x, so a timer driven by them alone updates under twice a second and reads
//! as broken however small each step is. `TickProgressService.Progress` --
//! how far through the current tick the game is, 0 to 1 -- fills the gap, and
//! it is reached through the DI container like everything else rather than
//! costing a scan. Measured across all three recordings: it sweeps the unit
//! interval at live instants, and reads exactly zero only where the game is
//! not ticking, which is a scene still loading and the Congratulations
//! screen.

use asr::{game_engine::unity::mono::Module, time::Duration, timer, Address, Process};

use crate::{service::Locatable, status};

/// Seconds of LiveSplit game time per in-game day.
///
/// LiveSplit's game time is a `Duration`, so the game's days have to be
/// mapped onto one, and every sensible candidate is linear. 60 makes a day a
/// minute: the day count reads straight off the minutes column, sub-day
/// progress off the seconds, and a world-record wonder run -- about 64 in-game
/// days -- reads `1:04:00`, which is also a plausible-looking speedrun time.
/// One tick is then 78ms, small enough that the display does not visibly step
/// and `TickProgressService` interpolation is not needed.
///
/// **This is the one line to change** if the community settles on another
/// scale. An `.lss` stores the raw game time, so runs already recorded can be
/// rescaled arithmetically -- the choice is not a one-way door. Two others
/// were costed: 460.8 (an in-game day at 1x real time, which would read
/// `8:11:31` for that same run and would step by 0.6s a tick), and 3600 (an
/// in-game hour as a clock hour).
///
/// What it must not be is read from the game. `DayLengthInSeconds` is the
/// tempting version of the 460.8 above, and two installs whose day length
/// differed would then produce incomparable times with nothing on screen
/// saying so.
pub const SECONDS_PER_GAME_DAY: f64 = 60.0;

/// Ticks in a vanilla day, from `Configurations/DayNightCycle.blueprint.json`.
///
/// Only a cross-check: the real value is derived from the clock's own fields,
/// so a game that changes it keeps working. What it buys is a log line saying
/// the day is not the length this splitter's times were measured against,
/// which is otherwise invisible and makes two runs incomparable.
pub const VANILLA_TICKS_PER_DAY: f64 = 768.0;

/// Where the clock's fields sit, resolved once per session.
///
/// `DayNumber` is required -- without it the splitter cannot say what day it
/// is, which is the one thing it has always logged. The rest are optional so
/// that a build missing them loses game time alone rather than everything.
#[derive(Clone, Copy)]
pub struct ClockFields {
    day_number: u32,
    sub_day: Option<SubDay>,
}

/// The fields that turn a day counter into a position within the day.
#[derive(Clone, Copy)]
struct SubDay {
    ticks_today: u32,
    hours_per_tick: u32,
    daytime: u32,
    nighttime: u32,
}

impl ClockFields {
    /// Resolves the offsets. `None` only when `DayNumber` is missing, which is
    /// a game version this splitter cannot read at all.
    pub fn resolve(process: &Process, module: &Module, clock: &Locatable) -> Option<Self> {
        let field = |name| clock.field(process, module, name);
        let sub_day = match (
            field("_ticksPassedToday"),
            field("FixedDeltaTimeInHours"),
            field("DaytimeLengthInHours"),
            field("NighttimeLengthInHours"),
        ) {
            (Some(ticks_today), Some(hours_per_tick), Some(daytime), Some(nighttime)) => {
                Some(SubDay {
                    ticks_today,
                    hours_per_tick,
                    daytime,
                    nighttime,
                })
            }
            _ => None,
        };
        Some(Self {
            day_number: field("DayNumber")?,
            sub_day,
        })
    }

    /// Whether the sub-day fields resolved, and so whether a fractional day is
    /// available at all.
    pub fn has_sub_day(&self) -> bool {
        self.sub_day.is_some()
    }
}

/// The in-game day a new game starts on: day 1 plus the blueprint's
/// `HoursPassedOnNewGame` of 4, over a 24-hour day.
///
/// Measured at day 1 tick 128 in every recording on both builds. Used only to
/// check the sampled baseline, never in place of it.
const NEW_GAME_DAY: f64 = 1.0 + 128.0 / VANILLA_TICKS_PER_DAY;

/// How far the sampled baseline may sit from [`NEW_GAME_DAY`] before it is
/// worth a line in the log, in ticks. The clock is already running at
/// `ShowUI` -- `UnpauseGame` is the step before it -- so a tick or two of
/// drift is expected and means nothing.
const BASELINE_SLACK_TICKS: f64 = 8.0;

/// `TickProgressService`, where the game says how far through the current
/// tick it is.
///
/// Optional throughout: without it game time still follows the clock, just in
/// whole-tick steps, so a build that renames this loses smoothness rather than
/// timing.
#[derive(Clone, Copy)]
pub struct TickProgress {
    instance: Address,
    offset: u32,
}

impl TickProgress {
    /// Looks the service up in the DI container. `None` if it is not there
    /// yet, or if the class has never been constructed.
    pub fn resolve(
        process: &Process,
        module: &Module,
        lookup: impl FnOnce(Address) -> Option<Address>,
    ) -> Option<Self> {
        let vtable = crate::service::class_vtable(process, module, IMAGE, CLASS)?;
        let instance = lookup(vtable)?;
        let offset = crate::service::field_offset(process, module, IMAGE, CLASS, "Progress")?;
        asr::print_message(&alloc::format!(
            "Found {CLASS} at {instance}. Game time will move between ticks."
        ));
        Some(Self { instance, offset })
    }

    /// How far through the current tick, clamped to the tick it belongs to.
    ///
    /// Clamped rather than trusted: this is read a beat after the tick counter
    /// and a value above one would put game time into a tick that has not
    /// happened.
    fn fraction(&self, process: &Process) -> f64 {
        let value = process
            .read::<f32>(self.instance.add(self.offset as u64))
            .map(f64::from)
            .unwrap_or(0.0);
        if value.is_nan() {
            return 0.0;
        }
        value.clamp(0.0, 1.0)
    }
}

const IMAGE: &str = "Timberborn.TimeSystem";
const CLASS: &str = "TickProgressService";

/// One tick's look at the clock.
pub struct Reading {
    /// The whole-day counter, which reads on every build.
    pub day_number: i32,
    /// The same moment as a fractional day, or `None` on a build without the
    /// sub-day fields, or before the game has loaded far enough for the
    /// lengths to be usable.
    pub day: Option<f64>,
}

/// A clock bound to one game's `DayNightCycle`.
///
/// Built per scene, because the instance is: the offsets outlive a game and
/// the object does not.
pub struct Clock {
    fields: ClockFields,
    instance: Address,
    /// Where the sub-tick fraction comes from, when the container had it.
    progress: Option<TickProgress>,
    /// Derived once the length constants read as something usable, and then
    /// kept: they do not change within a game, and re-deriving them would be
    /// three reads a tick for an answer already known.
    ticks_per_day: Option<f64>,
}

impl Clock {
    pub fn new(fields: ClockFields, instance: Address, progress: Option<TickProgress>) -> Self {
        Self {
            fields,
            instance,
            progress,
            ticks_per_day: None,
        }
    }

    /// Where the game is in time, in one pass.
    ///
    /// One call a tick rather than one per counter: the day number is wanted
    /// for the log and the fraction for game time, and reading them apart
    /// would be three reads a tick for two numbers -- and, worse, two numbers
    /// that could come from either side of a rollover.
    pub fn read(&mut self, process: &Process) -> Option<Reading> {
        let day_number = process
            .read::<i32>(self.instance.add(self.fields.day_number as u64))
            .ok()?;
        Some(Reading {
            day_number,
            day: self.fraction(process, day_number),
        })
    }

    /// The fractional day, given the whole-day counter already read.
    ///
    /// **The tick counter is read before the progress within it, and the order
    /// is load-bearing.** A tick can turn between the two reads, and this way
    /// round the pair is at worst a whole tick stale -- the count from the old
    /// tick, the progress from the new one near zero -- which only ever
    /// understates. The other order pairs a progress near one with the count
    /// after the turn, overshooting by a tick and then falling back: a timer
    /// running backwards, which is the one thing game time must never do.
    fn fraction(&mut self, process: &Process, day_number: i32) -> Option<f64> {
        let ticks_per_day = self.ticks_per_day(process)?;
        let sub_day = self.fields.sub_day?;
        let ticks = process
            .read::<i32>(self.instance.add(sub_day.ticks_today as u64))
            .ok()?;
        let within = match &self.progress {
            Some(progress) => progress.fraction(process),
            None => 0.0,
        };
        Some(f64::from(day_number) + (f64::from(ticks) + within) / ticks_per_day)
    }

    /// Ticks in a day, derived from the clock's own length constants.
    ///
    /// `None` until the game has loaded far enough for them to be non-zero,
    /// which is the same condition `report_countdown_length` retries against.
    pub fn ticks_per_day(&mut self, process: &Process) -> Option<f64> {
        if let Some(ticks) = self.ticks_per_day {
            return Some(ticks);
        }
        let sub_day = self.fields.sub_day?;
        let read = |offset: u32| -> Option<f64> {
            process
                .read::<f32>(self.instance.add(offset as u64))
                .ok()
                .map(f64::from)
        };
        let hours_per_tick = read(sub_day.hours_per_tick)?;
        let daytime = read(sub_day.daytime)?;
        let nighttime = read(sub_day.nighttime)?;
        if hours_per_tick <= 0.0 || daytime + nighttime <= 0.0 {
            return None;
        }
        let ticks = (daytime + nighttime) / hours_per_tick;
        self.ticks_per_day = Some(ticks);
        if (ticks - VANILLA_TICKS_PER_DAY).abs() < 0.5 {
            asr::print_message(&alloc::format!(
                "Clock: {ticks} ticks a day ({daytime}+{nighttime} in-game hours \
                 at {hours_per_tick} an hour a tick)."
            ));
        } else {
            status::warn(&alloc::format!(
                "This game's day is {ticks} ticks, not {VANILLA_TICKS_PER_DAY}. \
                 Game time will not be comparable with other runs."
            ));
        }
        Some(ticks)
    }
}

/// The game-time side of a run: the baseline, and pushing the clock at the
/// host.
///
/// Held above [`Clock`] rather than beside it, because the host's pause state
/// is per-attach and survives both a scene change and a reset, while a `Clock`
/// belongs to one game.
#[derive(Default)]
pub struct GameTime {
    /// Whether the host's automatic flow of game time has been stopped. Sticky
    /// host state, so this happens once, and not until there is a run being
    /// timed and a clock to time it by.
    paused: bool,
    /// The fractional day the run started on. `None` means no run start was
    /// seen, and so nothing may be pushed: a run whose zero is unknown would
    /// get a plausible-looking wrong time.
    baseline: Option<f64>,
    /// So the two explanations below are each said once rather than every tick.
    warned_no_baseline: bool,
    warned_lost: bool,
}

impl GameTime {
    /// Takes the baseline for a run that started before there was a clock to
    /// read.
    ///
    /// The run start can fire during the scan that locates the container the
    /// clock lives in, and at that moment there is nothing to sample. The
    /// constant is not a guess: the splitter only ever starts a run on a *new*
    /// game, and a new game's clock is at [`NEW_GAME_DAY`] -- day 1, tick 128
    /// -- in every recording of both builds. Being a tick out here is worth
    /// far less than the alternative, which is a run with no game time at all.
    pub fn begin_at_new_game(&mut self) {
        self.warned_no_baseline = false;
        self.warned_lost = false;
        self.baseline = Some(NEW_GAME_DAY);
        asr::print_message(&alloc::format!(
            "Run started before the clock was located. Game time starts from \
             day {NEW_GAME_DAY:.4}, where a new game begins."
        ));
    }

    /// Takes the baseline, on the tick the run starts.
    ///
    /// `None` is a clock that could not be read at that moment, which leaves
    /// the run untimed rather than timed from a guess.
    pub fn begin(&mut self, day: Option<f64>) {
        self.warned_no_baseline = false;
        self.warned_lost = false;
        let Some(day) = day else {
            // Deliberately not falling back to `NEW_GAME_DAY` here. This is a
            // clock that was located and then would not read, which is a
            // broken world rather than a missing one, and a number invented
            // for it would be believed.
            self.baseline = None;
            status::warn("Run started, but the clock could not be read. No game time.");
            return;
        };
        self.baseline = Some(day);
        let drift = (day - NEW_GAME_DAY).abs() * VANILLA_TICKS_PER_DAY;
        if drift > BASELINE_SLACK_TICKS {
            // Not an error: a modded day length or a start bound late both
            // land here, and both still produce a usable run. It is worth a
            // line because it is the one number the whole run is measured from.
            asr::print_message(&alloc::format!(
                "Game time starts from day {day:.4}, which is {drift:.0} ticks from \
                 where a new game begins. Expected {NEW_GAME_DAY:.4}."
            ));
        } else {
            asr::print_message(&alloc::format!("Game time starts from day {day:.4}."));
        }
    }

    /// Pushes the run's game time. Called every tick, before anything splits,
    /// so the final split records this tick rather than the last one.
    ///
    /// `day` is `None` when the clock cannot be read. That freezes game time
    /// where it is, deliberately: the counterpart -- handing the host
    /// `resume_game_time` -- would have it interpolate real time into a
    /// game-timed run, which looks right and is not.
    pub fn update(&mut self, day: Option<f64>) {
        if timer::state() != timer::TimerState::Running {
            return;
        }
        let Some(baseline) = self.baseline else {
            if !self.warned_no_baseline {
                self.warned_no_baseline = true;
                asr::print_message(
                    "The timer is running but no run start was seen, so there is no \
                     baseline and game time is not being set.",
                );
            }
            return;
        };
        let Some(day) = day else {
            if !self.warned_lost {
                self.warned_lost = true;
                status::warn("The clock was lost mid-run. Game time is frozen.");
            }
            return;
        };
        self.warned_lost = false;
        // Here rather than at attach, and this is the whole of what it means:
        // the splitter touches the timer only when it has a run to time. A
        // splitter that paused on attach would reach into the timer of a
        // runner it is not timing for -- a game already finished, a category
        // that is not this one -- and `refuses_to_start_a_timer_for_a_run_already_over`
        // in the snapshot suite is what says so.
        //
        // Sticky host state, so once. It stops the host flowing game time of
        // its own, which is what makes the value below the whole of it rather
        // than a correction to real time.
        if !self.paused {
            timer::pause_game_time();
            self.paused = true;
        }
        // Clamped: the host rejects a negative game time, and a clock that
        // reads behind its own baseline is a bug elsewhere, not a run that
        // happened before it started.
        let elapsed = (day - baseline).max(0.0) * SECONDS_PER_GAME_DAY;
        timer::set_game_time(Duration::seconds_f64(elapsed));
    }
}
