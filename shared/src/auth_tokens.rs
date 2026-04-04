//! Optional bearer tokens for Portal Cowork (`PORTAL_TOKEN` / `LOOM_TOKEN`) and Core (`LOOM_TOKEN`).

use subtle::ConstantTimeEq;

/// `PORTAL_TOKEN` if set, else `LOOM_TOKEN` if set (Portal Cowork HTTP/WS).
pub fn optional_portal_cowork_token() -> Option<String> {
    let t = std::env::var("PORTAL_TOKEN").unwrap_or_default();
    if !t.is_empty() {
        return Some(t);
    }
    let t = std::env::var("LOOM_TOKEN").unwrap_or_default();
    if !t.is_empty() {
        return Some(t);
    }
    None
}

/// `LOOM_TOKEN` if set (Core HTTP API middleware).
pub fn optional_loom_token() -> Option<String> {
    let t = std::env::var("LOOM_TOKEN").unwrap_or_default();
    if !t.is_empty() {
        Some(t)
    } else {
        None
    }
}

/// Constant-time equality for bearer secrets (returns false if lengths differ).
pub fn ct_eq_str(a: &str, b: &str) -> bool {
    let ab = a.as_bytes();
    let bb = b.as_bytes();
    if ab.len() != bb.len() {
        return false;
    }
    ab.ct_eq(bb).into()
}
