//! Saying something to the runner.
//!
//! `asr::print_message` goes to the host's log, which in LiveSplit is `Trace`
//! and therefore nowhere at all unless a trace listener has been added to
//! `LiveSplit.exe.config` by hand. So every warning this splitter can produce
//! is invisible in normal use -- including "bound too late, not starting the
//! timer", which reaches the runner as a timer that silently did not start.
//! That is the same symptom as the bug this splitter exists to fix, which makes
//! it the worst possible thing to say only into a log nobody reads.
//!
//! `asr::timer::set_variable` reaches `RunMetadata.SetCustomVariable`, and
//! LiveSplit's **Text** component displays it: tick "Custom Variable" and put
//! [`NAME`] in the second box. Leaving the first box empty means the component
//! shows nothing at all until there is something to say. It still occupies its
//! row -- a component cannot give up its height -- but the row is blank.
//!
//! # This does not touch the runner's splits file
//!
//! Read out of LiveSplit's source rather than assumed, because writing status
//! strings into someone's `.lss` would be unforgivable:
//!
//! - `RunMetadata.SetCustomVariable` goes through `GetOrAddCustomVariable`,
//!   which constructs the variable with `IsPermanent = false`.
//! - `XMLRunSaver` writes a custom variable only `if (entry.Value.IsPermanent)`.
//! - `SetCustomVariable` sets `HasChanged` only for permanent variables, so
//!   this does not even make LiveSplit think the splits need saving.
//!
//! A variable is permanent only if the runner added it by hand in the Run
//! Editor. The one hazard left is colliding with such a variable, which is why
//! [`NAME`] is specific rather than something like "Status".

/// The custom variable's name, and what goes in the Text component's second
/// box.
pub const NAME: &str = "Timberborn Autosplitter";

/// Tells the runner something they need to know, and logs it.
///
/// Reserved for what actually affects the run: a start that did not fire, or a
/// game version this build cannot read. Everything routine stays in the log --
/// a status line that usually says something is a status line nobody reads,
/// which is how we got here.
pub fn warn(message: &str) {
    asr::print_message(message);
    asr::timer::set_variable(NAME, message);
}

/// Blanks the message.
///
/// Desktop LiveSplit renders an empty value as an empty string, so the
/// component goes blank -- `CustomVariableValue` returns `""`, which is not
/// null and so does not hit its `?? DASH` fallback. livesplit-core hosts
/// (LiveSplit One, asr-debugger) filter empty values and substitute a dash
/// instead, so there they show `—` rather than nothing. No value is blank in
/// both.
pub fn clear() {
    asr::timer::set_variable(NAME, "");
}
