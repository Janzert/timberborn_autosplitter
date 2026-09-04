//! Reading facts off a game install, with no game running.
//!
//! Which install a capture or a fixture was made against is the thing that
//! makes it meaningful later, and it cannot be recovered afterwards -- so it
//! is read here, once, and written down.

use std::{fs, path::Path};

/// The game's own version, e.g. `1.1.2.4-52e959e-sw`, from PlayerSettings'
/// bundleVersion in `globalgamemanagers`. The game's own name for the build,
/// so a snapshot and a saved-off install can be paired by it.
///
/// This is what identifies a capture. Steam's app manifest cannot: it reports
/// whichever build is *installed*, and an old version run out of the version
/// store is not that one -- so a capture of 1.0.13.1 would have been filed
/// under the installed 25096761 and been quietly wrong about what it held.
pub fn version(dir: &Path) -> Option<String> {
    let blob = fs::read(dir.join("Timberborn_Data").join("globalgamemanagers")).ok()?;
    let head = &blob[..blob.len().min(200_000)];

    let strings: Vec<&[u8]> = head
        .split(|b| !(0x20..0x7f).contains(b))
        .filter(|s| s.len() >= 4)
        .collect();
    let name = strings.iter().position(|s| *s == b"Timberborn")?;
    strings.iter().skip(name + 1).take(4).find_map(|s| {
        let text = str::from_utf8(s).ok()?;
        let versionish = text.split('-').next()?;
        let parts: Vec<&str> = versionish.split('.').collect();
        (parts.len() >= 3
            && parts
                .iter()
                .all(|p| !p.is_empty() && p.bytes().all(|b| b.is_ascii_digit())))
        .then(|| text.to_owned())
    })
}
