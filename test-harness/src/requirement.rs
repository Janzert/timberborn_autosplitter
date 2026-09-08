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
    /// The splitter's own words for *this* scenario's run being over, which is
    /// how the recorder knows which step to tag with [`ends_at`](Self::ends_at).
    ///
    /// Per scenario rather than one constant, because the two categories end on
    /// different things and a recorder that only knew the wonder's phrase would
    /// silently leave a Timberbot recording untagged.
    ///
    /// Matching a phrase is safe here in a way it would not be for choosing
    /// what to capture: a reworded message loses the tag, and a test wanting
    /// the end state then fails with the instructions for producing one --
    /// loud, and about the right thing. `tests/synthetic_scenario.rs` checks
    /// these against what the splitter actually says, so the drift is caught at
    /// commit time rather than after a run has been played.
    pub ends_when: Option<&'static str>,
    /// Splitter settings the recorder must force before this scenario is
    /// played, as `(key, value)`.
    ///
    /// The recorder drives the real splitter, so it splits where a runner's
    /// splitter would -- including *not* splitting on a trigger that ships off.
    /// A Timberbot recording made on the stock defaults would capture the run
    /// start and the Gear Workshop and then go quiet, because captures are
    /// taken on splits and the rest of that route's triggers are off. That is a
    /// recording which misses exactly what it was made for, after an hour of
    /// playing, and nothing about it looks wrong.
    ///
    /// Naming them here rather than in the reproduce steps means
    /// `tb-record --state <id>` is enough and a person cannot forget.
    /// `tests/attach.rs` checks every key against what the splitter registers.
    pub settings: &'static [(&'static str, bool)],
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
        ends_when: None,
        settings: &[],
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
        ends_when: None,
        settings: &[],
    },
    Requirement {
        id: "first-bot",
        summary: "a settlement that has produced its first Timberbot",
        reproduce: &[
            "Start a new game as either faction.",
            "Build a Bot Part Factory and a Bot Assembler, and assemble one bot.",
            "The developer console is a legitimate way to get there: it can grant \
             the science, the buildings and the parts, and the splitter reads the \
             same state either way.",
            "Stop once the population overlay shows a bot count beside the adults \
             and children -- that is the flag this state is of.",
        ],
        begins_at: None,
        ends_at: None,
        ends_when: None,
        settings: &[],
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
        ends_when: Some("Run end: Congratulations screen."),
        settings: &[],
    },
    Requirement {
        id: "timberbot-run",
        summary: "a whole Timberbot run recorded as it was played, split by split",
        reproduce: &[
            "Start the game and stop at the main menu -- do not load a save yet.",
            "Start `tb-record --state timberbot-run` and leave it running. It \
             turns the Timberbot triggers on for you; they ship off, and a \
             recording made without them would capture the start and the Gear \
             Workshop and nothing else.",
            "Start a new game and play it through: **build and finish** a Gear \
             Workshop, a Smelter and a Bot Part Factory, then a Bot Assembler, \
             and assemble one bot.",
            "Developer mode is a legitimate way to get through it quickly -- \
             grant the science and the resources freely. Do **not** shortcut the \
             three buildings by consoling their goods in: they are the splits, \
             and matching their template names against real memory is half of \
             what this recording is for. The `first-bot` capture was taken that \
             way and walks 5252 entities for none of them.",
            "Stop the recorder with Ctrl-C once the bot is out.",
        ],
        begins_at: Some("main-menu"),
        ends_at: Some("first-bot"),
        ends_when: Some("Run end: the first Timberbot was created."),
        // The three triggers this route splits on, which ship off so that a
        // wonder run on the stock defaults does not split twice on the Smelter.
        settings: &[
            ("smelter", true),
            ("bot_part_factory", true),
            ("first_bot", true),
        ],
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
        ends_when: None,
        settings: &[],
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
