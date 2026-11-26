//! Switches that may change while the title runs. A switch's value is the
//! one the control file gave it (`set NAME=VALUE`, `unset NAME`: diagnostics
//! builds, `crate::control`), else its environment variable's, as the
//! process started with it.
//!
//! A switch read where every draw passes keeps its parsed value with the
//! generation of the settings it was read under ([`Switch`]): such a read
//! costs two atomic loads, and the variable is looked up again only after
//! the control file changed a setting. Switches read when shaders are
//! translated or pipelines built stay as the run started: changed later,
//! what was made before would keep the old setting.

use std::sync::Mutex;
use std::sync::atomic::{AtomicU64, Ordering};

/// The switches read through [`Switch`] (or [`generation`]) wherever they
/// are used, which the control file can change mid-run.
#[cfg(any(test, feature = "diagnostics"))]
pub(crate) const SWITCHES: [&str; 6] = [
    "VICTORY_HEAP_RECHECK",
    "VICTORY_HEAP_SYNC",
    "VICTORY_STALE_TEXTURES",
    "VICTORY_TEXTURE_GUARD",
    "VICTORY_SCISSOR_TEST",
    "VICTORY_SKIP_PIPELINES",
];

/// Bumped by every change of a setting; starts at 1, so that a [`Switch`]
/// never read (0) is read.
static GENERATION: AtomicU64 = AtomicU64::new(1);

/// What the control file set, by name: a value, or `None` for unset.
static SET: Mutex<Vec<(String, Option<String>)>> = Mutex::new(Vec::new());

/// The settings' generation: a value read under one generation holds until
/// it changes.
pub(crate) fn generation() -> u64 {
    GENERATION.load(Ordering::Acquire)
}

/// The value of switch `name`: the control file's, if it set or unset it,
/// else the environment's.
pub(crate) fn get(name: &str) -> Option<String> {
    let set = SET.lock().unwrap_or_else(|e| e.into_inner());
    match set.iter().find(|(n, _)| n == name) {
        Some((_, value)) => value.clone(),
        None => std::env::var(name).ok(),
    }
}

/// Sets switch `name` to `value`, or with `None` makes it unset, whatever
/// the environment says.
#[cfg(any(test, feature = "diagnostics"))]
pub(crate) fn set(name: &str, value: Option<&str>) {
    {
        let mut set = SET.lock().unwrap_or_else(|e| e.into_inner());
        let value = value.map(str::to_owned);
        match set.iter_mut().find(|(n, _)| n == name) {
            Some(entry) => entry.1 = value,
            None => set.push((name.to_owned(), value)),
        }
    }
    // After the change: a reader that sees the new generation finds it.
    GENERATION.fetch_add(1, Ordering::Release);
}

/// The live switches that have a value, with it (for the overlay).
#[cfg(any(test, feature = "diagnostics"))]
pub(crate) fn active() -> Vec<(&'static str, String)> {
    SWITCHES
        .iter()
        .filter_map(|&name| Some((name, get(name)?)))
        .collect()
}

/// A switch read where it must cost next to nothing: its value parsed to a
/// number by `parse` (from the value, `None` when unset), kept with the
/// generation it was read under.
pub(crate) struct Switch {
    name: &'static str,
    parse: fn(Option<&str>) -> u32,
    /// The generation (high half) and the value (low half); 0: never read.
    cached: AtomicU64,
}

impl Switch {
    pub(crate) const fn new(name: &'static str, parse: fn(Option<&str>) -> u32) -> Switch {
        Switch {
            name,
            parse,
            cached: AtomicU64::new(0),
        }
    }

    pub(crate) fn value(&self) -> u32 {
        let generation = generation();
        let cached = self.cached.load(Ordering::Relaxed);
        if cached >> 32 == generation {
            return cached as u32;
        }
        let value = (self.parse)(get(self.name).as_deref());
        self.cached
            .store(generation << 32 | u64::from(value), Ordering::Relaxed);
        value
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A switch follows the control file's settings, the environment where
    /// the file never named it; `unset` hides the environment's value.
    #[test]
    fn a_switch_takes_the_control_files_value_over_the_environment() {
        const NAME: &str = "VICTORY_LIVE_TEST_SWITCH";
        static S: Switch = Switch::new(NAME, |v| u32::from(v.is_none_or(|v| v != "0")));
        // SAFETY: no other test reads or writes this variable.
        unsafe { std::env::set_var(NAME, "0") };
        assert_eq!(S.value(), 0, "the environment's value when nothing was set");
        set(NAME, Some("1"));
        assert_eq!(S.value(), 1, "the control file's value");
        assert_eq!(get(NAME).as_deref(), Some("1"));
        set(NAME, None);
        assert_eq!(
            S.value(),
            1,
            "unset: as if the variable were not there (on by default)"
        );
        assert_eq!(get(NAME), None);
        set(NAME, Some("0"));
        assert_eq!(S.value(), 0);
    }

    /// Only the switches read live are listed as active.
    #[test]
    fn active_lists_live_switches_with_a_value() {
        set("VICTORY_SCISSOR_TEST", Some("full"));
        set("VICTORY_NOT_LIVE_TEST", Some("1"));
        let listed = active();
        assert!(listed.contains(&("VICTORY_SCISSOR_TEST", "full".to_owned())));
        assert!(!listed.iter().any(|(n, _)| *n == "VICTORY_NOT_LIVE_TEST"));
        set("VICTORY_SCISSOR_TEST", None);
        assert!(!active().iter().any(|(n, _)| *n == "VICTORY_SCISSOR_TEST"));
    }
}
