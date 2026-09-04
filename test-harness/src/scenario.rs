//! Replaying a recorded sequence, so the splitter sees the world change.
//!
//! A single capture can only say what was true at one instant. A split is a
//! *change*, so testing that one fires needs memory that differs between one
//! tick and the next. That is all this is: the steps of a recording, served in
//! order, advancing when told.
//!
//! The substitution is closer to honest than it looks. Every step is a capture
//! of the same process, and Unity ships Mono with the Boehm collector, which
//! does not move objects -- so an object has the same address in every step it
//! is alive for. Swapping step *n* for step *n+1* between ticks shows the
//! splitter the same addresses holding their new values, which is what actually
//! happened, minus the time in between.

use std::{
    cell::Cell,
    path::{Path, PathBuf},
};

use crate::{
    memory::{FakeProcess, Memory, MemoryRange},
    snapshot::Snapshot,
};

/// A recorded run, held open and served one step at a time.
pub struct Scenario {
    steps: Vec<Snapshot>,
    /// Which step reads currently come from. A `Cell` because `Memory::read`
    /// takes `&self` -- the splitter holds the process, and the test needs to
    /// move it on from outside.
    at: Cell<usize>,
}

impl Scenario {
    /// Opens every step of the scenario satisfying `requirement_id`.
    pub fn find(requirement_id: &str) -> Result<Self, String> {
        let dirs = crate::snapshot::find_scenario(requirement_id)?;
        Self::open(&dirs)
    }

    /// Every recorded scenario satisfying `requirement_id`, by name.
    ///
    /// Opened one at a time by the caller rather than all at once: each holds a
    /// file handle per step, and there is no reason for two runs to be in
    /// memory together.
    pub fn all(requirement_id: &str) -> Result<Vec<(String, Vec<PathBuf>)>, String> {
        crate::snapshot::find_scenarios(requirement_id)
    }

    pub fn open(dirs: &[PathBuf]) -> Result<Self, String> {
        let steps = dirs
            .iter()
            .map(|dir| Snapshot::open(dir).map_err(|e| format!("opening {}: {e}", dir.display())))
            .collect::<Result<Vec<_>, _>>()?;
        if steps.is_empty() {
            return Err("a scenario with no steps".into());
        }
        Ok(Self {
            steps,
            at: Cell::new(0),
        })
    }

    pub fn len(&self) -> usize {
        self.steps.len()
    }

    pub fn is_empty(&self) -> bool {
        self.steps.is_empty()
    }

    /// What the splitter did to make each step a moment worth keeping, in
    /// order: `begin`, `start`, `split`, `split`, ...
    pub fn events(&self) -> Vec<String> {
        self.steps
            .iter()
            .map(|s| {
                s.metadata
                    .step
                    .as_ref()
                    .map(|step| step.event.clone())
                    .unwrap_or_default()
            })
            .collect()
    }

    pub fn game_version(&self) -> &str {
        &self.steps[0].metadata.game_version
    }
}

/// A shared handle on where a scenario has got to.
///
/// The process handed to the splitter owns the scenario, so advancing it has to
/// go through something both sides hold. One `Rc`, no cloning of gigabytes.
pub type Shared = std::rc::Rc<Scenario>;

impl Scenario {
    /// The process to hand to a [`World`](crate::World), and the handle that
    /// moves it forward.
    pub fn into_process(self) -> (FakeProcess, Shared) {
        let shared: Shared = std::rc::Rc::new(self);
        let first = &shared.steps[0];
        let mut process = FakeProcess::new(
            first.metadata.pid as u64,
            first.metadata.process_name.clone(),
        );
        process.memory = Box::new(Playhead {
            scenario: shared.clone(),
        });
        // The tables move with the playhead, not just the bytes. A run's
        // mappings grow as it goes -- the recorded scenario runs from 1443
        // ranges at the main menu to over 2000 by the end -- and a table fixed
        // at step 0 leaves the splitter sweeping a fraction of the heap.
        let tables = shared.clone();
        let process = process.with_tables(move || tables.tables());
        (process, shared)
    }

    /// Moves to the next step. Returns false at the end of the recording.
    pub fn advance(&self) -> bool {
        let next = self.at.get() + 1;
        if next >= self.steps.len() {
            return false;
        }
        self.at.set(next);
        true
    }

    pub fn position(&self) -> usize {
        self.at.get()
    }

    /// What the splitter did at the step now being served.
    pub fn event(&self) -> String {
        self.steps[self.at.get()]
            .metadata
            .step
            .as_ref()
            .map(|s| s.event.clone())
            .unwrap_or_default()
    }

    /// The mapping tables of the step now being served.
    pub fn tables(&self) -> (Vec<crate::memory::ModuleInfo>, Vec<MemoryRange>) {
        let step = &self.steps[self.at.get()];
        (step.modules.clone(), step.ranges.clone())
    }

    pub fn directory(&self) -> &Path {
        Path::new(&self.steps[self.at.get()].directory_name)
    }
}

/// Reads from whichever step the scenario is currently at.
struct Playhead {
    scenario: Shared,
}

impl Memory for Playhead {
    fn read(&self, address: u64, buf: &mut [u8]) -> bool {
        self.scenario.steps[self.scenario.at.get()].read(address, buf)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{
        memory::flags,
        snapshot::{Metadata, Step, Writer, CHUNK},
    };

    const AT: u64 = 0x20_0000;

    /// Writes a three-step scenario where one byte changes at each step, each a
    /// delta against the one before.
    fn record(parent: &Path) -> Vec<PathBuf> {
        let mut dirs = Vec::new();
        let mut previous: Option<PathBuf> = None;
        for (index, (event, value)) in [("begin", 0u8), ("start", 1), ("split", 2)]
            .into_iter()
            .enumerate()
        {
            let dir = parent.join(format!("step{index:02}"));
            let mut metadata = Metadata {
                game_version: "1.1.2.4-52e959e-sw".into(),
                label: format!("step{index:02}"),
                process_name: "Unity Main Thre".into(),
                pid: 7,
                frozen: true,
                satisfies: vec!["wonder-run".into()],
                scenario: Some("wonder-run".into()),
                ..Default::default()
            };
            metadata.step = Some(Step {
                index: index as u32,
                event: event.into(),
            });

            let mut writer = Writer::create(dir.clone(), metadata).unwrap();
            if let Some(base) = &previous {
                writer = writer.with_base(Snapshot::open(base).unwrap());
            }
            let mut bytes = vec![0xEE; (CHUNK * 2) as usize];
            bytes[0] = value;
            writer
                .add_range(
                    MemoryRange {
                        address: AT,
                        size: bytes.len() as u64,
                        flags: flags::HEAP,
                    },
                    Some(&bytes),
                )
                .unwrap();
            writer.finish().unwrap();
            previous = Some(dir.clone());
            dirs.push(dir);
        }
        dirs
    }

    #[test]
    fn serves_each_step_in_turn() {
        let parent = std::env::temp_dir().join(format!("tb-scenario-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&parent);
        std::fs::create_dir_all(&parent).unwrap();
        let dirs = record(&parent);

        let scenario = Scenario::open(&dirs).unwrap();
        assert_eq!(scenario.len(), 3);
        assert_eq!(scenario.events(), ["begin", "start", "split"]);

        let (process, playhead) = scenario.into_process();
        let mut byte = [0u8; 1];

        // The world does not move on its own: a tick that does not advance the
        // scenario must see exactly what the last one saw, or the splitter
        // would be reacting to changes nothing made.
        assert!(process.memory.read(AT, &mut byte));
        assert_eq!(byte[0], 0);
        assert!(process.memory.read(AT, &mut byte));
        assert_eq!(byte[0], 0);

        assert!(playhead.advance());
        assert_eq!(playhead.event(), "start");
        assert!(process.memory.read(AT, &mut byte));
        assert_eq!(byte[0], 1);

        assert!(playhead.advance());
        assert!(process.memory.read(AT, &mut byte));
        assert_eq!(byte[0], 2);

        assert!(!playhead.advance(), "the recording has ended");
        assert!(process.memory.read(AT, &mut byte));
        assert_eq!(
            byte[0], 2,
            "past the end it holds the last step, not nothing"
        );

        std::fs::remove_dir_all(&parent).unwrap();
    }

    /// Unchanged memory has to survive being inherited through the whole chain,
    /// or a long recording would decay into holes.
    #[test]
    fn inherited_bytes_survive_every_step() {
        let parent =
            std::env::temp_dir().join(format!("tb-scenario-inherit-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&parent);
        std::fs::create_dir_all(&parent).unwrap();
        let dirs = record(&parent);

        let scenario = Scenario::open(&dirs).unwrap();
        let (process, playhead) = scenario.into_process();
        playhead.advance();
        playhead.advance();

        // The second chunk was never touched after step 0.
        let mut buf = [0u8; 32];
        assert!(process.memory.read(AT + CHUNK, &mut buf));
        assert_eq!(buf, [0xEE; 32]);

        std::fs::remove_dir_all(&parent).unwrap();
    }
}
