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
    /// For a recorded scenario: the single-instant state its **first** step is
    /// also a capture of, because the recording's own contract says where it
    /// has to be started from. A `wonder-run` recording begun anywhere but the
    /// main menu is not a `wonder-run` recording -- the splitter has to be
    /// watched from before the game exists -- so its step 0 is a main menu.
    pub begins_at: Option<&'static str>,
    /// For a recorded scenario: the single-instant state reached at its **last**
    /// step, tagged by the recorder when it sees the run end.
    ///
    /// This is what lets the store keep recordings and nothing else. A whole
    /// run captured split by split already contains the instants a single
    /// capture would hold, so keeping both costs gigabytes for a duplicate.
    pub ends_at: Option<&'static str>,
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
        // A recorded scenario is made by `tb-record` while the game is played,
        // and its own steps say so, so it gets no trailing capture line.
        // Telling someone to `tb-dump` one would have them produce a single
        // instant that no scenario test can use -- and the message said exactly
        // that until a second recording state made it worth noticing.
        let capture = match self.begins_at {
            Some(_) => String::new(),
            None => format!(
                "\n  - then, with the game left in that state:\n      \
                 tb-dump --freeze --state {} --notes '<what you did>'",
                self.id
            ),
        };
        format!(
            "No snapshot satisfies {:?} ({}).\n\nTo make one:\n{steps}{capture}\n\n\
             See snapshots/README.md.",
            self.id, self.summary
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
        reproduce: &[
            "Start Timberborn and stop at the main menu; do not load or start a game.",
            "Recording a `wonder-run` also produces one: its first step is this state.",
        ],
        begins_at: None,
        ends_at: None,
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
            "Recording a `wonder-run` also produces one: its last step is this state.",
        ],
        begins_at: None,
        ends_at: None,
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
        // A recording is started at the menu and stopped after the run ends, so
        // it holds both of the single instants the other two states name. That
        // is why the store keeps recordings and not separate captures of them.
        begins_at: Some("main-menu"),
        ends_at: Some("run-finished"),
    },
    Requirement {
        id: "two-games",
        summary: "two games started in one process, so the second scene load is on record",
        reproduce: &[
            "Start the game and stop at the main menu -- do not load a save yet.",
            "Start `tb-record --state two-games` and leave it running.",
            "Start a new game and wait for the overlay to come up. Nothing else \
             needs building.",
            "Quit to the main menu, then start a second new game and again wait \
             for the overlay.",
            "Stop the recorder with Ctrl-C once the second game is up.",
            "Developer mode is fine; nothing here depends on what is built.",
        ],
        // No run is played, so nothing tags an end state.
        begins_at: Some("main-menu"),
        ends_at: None,
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
