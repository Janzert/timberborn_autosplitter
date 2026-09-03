//! What state a snapshot has to be a snapshot *of*.
//!
//! A test needs a game in a particular condition, not a particular file. Naming
//! the file makes the suite unreproducible: the capture lives on one machine,
//! and anyone else -- or the same machine after a game update -- is left with a
//! path that does not exist and no idea what should have been at it.
//!
//! So a test asks for a [`Requirement`], a capture records which ones it
//! satisfies, and a missing one fails with the instructions for producing it.

/// A game state that captures can be taken of.
pub struct Requirement {
    /// Recorded in the manifest as `satisfies <id>`, and passed to `tb-dump`
    /// as `--state <id>`.
    pub id: &'static str,
    /// What the state is, in one line.
    pub summary: &'static str,
    /// How to get the game there. Written for someone who has never done it.
    pub reproduce: &'static [&'static str],
}

impl Requirement {
    /// The instructions, as a message for a test that could not find a capture.
    pub fn instructions(&self) -> String {
        let steps = self
            .reproduce
            .iter()
            .map(|step| format!("  - {step}"))
            .collect::<Vec<_>>()
            .join("\n");
        format!(
            "No snapshot satisfies {:?} ({}).\n\nTo make one:\n{}\n  - then, with the \
             game left in that state:\n      tb-dump --freeze --state {} --notes '<what you did>'\n\n\
             See snapshots/README.md.",
            self.id, self.summary, steps, self.id
        )
    }
}

/// Every state the tests know how to ask for.
///
/// Adding one means adding it here first: `tb-dump --state` refuses an id that
/// is not listed, so a typo cannot produce a capture no test will ever find.
pub const CATALOGUE: &[Requirement] = &[
    Requirement {
        id: "main-menu",
        summary: "the main menu, with no save loaded",
        reproduce: &["Start Timberborn and stop at the main menu; do not load or start a game."],
    },
    Requirement {
        id: "run-finished",
        summary: "a finished wonder run, Congratulations screen already shown",
        reproduce: &[
            "Start a new game as either faction.",
            "Build every split-triggering building: Forester, Gear Workshop, \
             Tapper's Shack, the faction's advanced science building (Observatory \
             for Folktails, Numbercruncher for Iron Teeth), Smelter and Wood Workshop.",
            "Unlock the wonder with science, then activate it.",
            "Wait out the countdown until the Congratulations screen appears.",
            "Developer mode is a legitimate way to get here quickly -- the splitter \
             reads the same state either way, and the day counter will simply be low.",
        ],
    },
    Requirement {
        id: "wonder-run",
        summary: "a whole wonder run recorded as it was played, split by split",
        reproduce: &[
            "Start the game and stop at the main menu -- do not load a save yet.",
            "Start `tb-record --state wonder-run` and leave it running.",
            "Start a new game and play it through: every split-triggering \
             building, then the wonder unlocked and activated, then the \
             Congratulations screen.",
            "The recorder captures whenever the splitter starts, splits or \
             resets, so nothing needs doing at each split. Stop it with Ctrl-C \
             once the run is over.",
            "Developer mode is a legitimate way to get through the run quickly.",
        ],
    },
];

/// Looks a requirement up by id.
pub fn get(id: &str) -> Option<&'static Requirement> {
    CATALOGUE.iter().find(|r| r.id == id)
}

/// The catalogue, for an error message listing what is valid.
pub fn listing() -> String {
    CATALOGUE
        .iter()
        .map(|r| format!("  {:<14} {}", r.id, r.summary))
        .collect::<Vec<_>>()
        .join("\n")
}
