//! Shared secret-path policy for file tools and both OS shell sandboxes.
pub(crate) const SECRET_NAMES: &[&str] = &[
    ".ssh", ".aws", ".azure", ".gnupg", ".portal-client.json", "connection.json", "auth.json",
];

pub(crate) fn is_secret_name(name: &str) -> bool {
    let normalized = name.to_ascii_lowercase();
    let normalized = normalized.trim_end_matches(['.', ' ']);
    SECRET_NAMES.contains(&normalized) || normalized.starts_with(".env")
}
