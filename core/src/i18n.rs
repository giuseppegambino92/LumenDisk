//! Language of user-visible core messages (the app sets it via `ld_set_language`).
use std::sync::atomic::{AtomicBool, Ordering};

static ENGLISH: AtomicBool = AtomicBool::new(false);

pub fn set_language(code: &str) {
    ENGLISH.store(code.to_ascii_lowercase().starts_with("en"), Ordering::SeqCst);
}

pub fn is_english() -> bool {
    ENGLISH.load(Ordering::SeqCst)
}

/// Pick the Italian or English text.
pub fn t(it: &str, en: &str) -> String {
    if is_english() { en.to_string() } else { it.to_string() }
}
