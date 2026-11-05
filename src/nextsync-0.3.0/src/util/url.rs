//! URL helpers.

/// Host part of a server URL (`https://cloud.example.com` ->
/// `cloud.example.com`); the raw URL when it does not parse as expected.
pub fn server_host(server_url: &str) -> &str {
    let trimmed = server_url.trim_end_matches('/');
    match trimmed.split_once("://") {
        Some((_scheme, host)) => host,
        None => trimmed,
    }
}
