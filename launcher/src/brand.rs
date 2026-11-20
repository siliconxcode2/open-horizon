//! Open Horizon's name on what players read. Upstream's text says "Horizon"; this fork shows
//! [`NAME`] instead, at display time, so translation keys and upstream's strings stay as they are
//! and merges from upstream keep applying cleanly.
//!
//! The game has the same rule in `horizon-client-core/src/brand.rs`; change both together.

use std::borrow::Cow;
use std::collections::HashMap;
use std::sync::{Mutex, OnceLock};

/// The product's name as players see it.
pub const NAME: &str = "Open Horizon";
/// The launcher's name as players see it.
pub const LAUNCHER: &str = "Open Horizon Launcher";

const OLD: &str = "Horizon";

/// `text` with every standalone "Horizon" read as [`NAME`]. A word that only starts with it is
/// left alone ("Horizontal", German "Horizont", Spanish "horizonte"), and so is one already
/// preceded by "Open ". Inflected forms of the name ("Horizon's", Czech "Horizonu") are renamed.
pub fn apply(text: &str) -> Cow<'_, str> {
    if !text.contains(OLD) {
        return Cow::Borrowed(text);
    }
    let mut out = String::with_capacity(text.len() + 16);
    let mut rest = text;
    let mut changed = false;
    while let Some(at) = rest.find(OLD) {
        let (before, from) = rest.split_at(at);
        out.push_str(before);
        let after = &from[OLD.len()..];
        let prev = out.chars().next_back();
        let next = after.chars().next();
        let starts_word = !prev.is_some_and(char::is_alphanumeric);
        let common_noun = matches!(next, Some('t' | 'T' | 'e'));
        let already = out.ends_with("Open ");
        if starts_word && !common_noun && !already {
            out.push_str(NAME);
            changed = true;
        } else {
            out.push_str(OLD);
        }
        rest = after;
    }
    if !changed {
        return Cow::Borrowed(text);
    }
    out.push_str(rest);
    Cow::Owned(out)
}

/// [`apply`] for text that must live as long as the program, such as the English fallback of
/// `tr`: each distinct renamed string is built once and kept.
pub fn apply_static(text: &str) -> &str {
    match apply(text) {
        Cow::Borrowed(_) => text,
        Cow::Owned(renamed) => {
            static SEEN: OnceLock<Mutex<HashMap<String, &'static str>>> = OnceLock::new();
            let seen = SEEN.get_or_init(Default::default);
            let mut seen = seen.lock().unwrap_or_else(|e| e.into_inner());
            seen.entry(text.to_owned())
                .or_insert_with(|| Box::leak(renamed.into_boxed_str()))
        }
    }
}

#[cfg(test)]
mod tests {
    use super::apply;

    #[test]
    fn renames_the_product_but_not_the_word() {
        assert_eq!(apply("Horizon Launcher is installed."), "Open Horizon Launcher is installed.");
        assert_eq!(apply("Horizon's settings"), "Open Horizon's settings");
        assert_eq!(apply("Spustit Horizonu"), "Spustit Open Horizonu");
        assert_eq!(apply("(Horizon)"), "(Open Horizon)");
        assert_eq!(apply("Open Horizon"), "Open Horizon");
        assert_eq!(apply("Horizontal"), "Horizontal");
        assert_eq!(apply("am Horizont"), "am Horizont");
        assert_eq!(apply("el Horizonte"), "el Horizonte");
        assert_eq!(apply("NoHorizon"), "NoHorizon");
        assert_eq!(apply("horizon-rust/horizon"), "horizon-rust/horizon");
    }
}
