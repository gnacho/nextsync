//! Pure activity-line parsing for the Recent view (Task 5.4).
//!
//! Mirrors `ui/activity.py` (v0.4.0): one application log line becomes a
//! compact [`ActivityEntry`] carrying the level, the message (without the
//! timestamp/level prefix) and an icon name. Everything here is pure — no GTK —
//! so it is unit-testable; the GTK Recent list built from these entries lives
//! in [`crate::ui::conflict_resolver`].
//!
//! # Deviations from `activity.py` (motivated)
//!
//! - **Manual parser instead of `re`**: the `LOG_LINE` pattern is matched with
//!   a byte scanner (the repo avoids the `regex` crate).
//! - **`LEVEL_ICONS` exposed as a const table + lookup helper** instead of a
//!   Python dict.

use std::fmt;

/// Icon used when the level is `INFO` and the message announces success.
const OK_ICON: &str = "nextsync-activity-ok";
/// Fallback icon when a level is unknown.
const FALLBACK_ICON: &str = "nextsync-activity-info";

/// Level → icon mapping, mirroring the Python `LEVEL_ICONS` dict.
pub const LEVEL_ICONS: [(&str, &str); 5] = [
    ("DEBUG", "nextsync-activity-debug"),
    ("INFO", "nextsync-activity-info"),
    ("WARNING", "nextsync-activity-warning"),
    ("ERROR", "nextsync-activity-error"),
    ("CRITICAL", "nextsync-activity-error"),
];

/// The icon name for a log level (fallback `nextsync-activity-info`).
pub fn level_icon(level: &str) -> &'static str {
    LEVEL_ICONS
        .iter()
        .find(|(candidate, _)| *candidate == level)
        .map(|(_, icon)| *icon)
        .unwrap_or(FALLBACK_ICON)
}

/// One parsed application log line.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ActivityEntry {
    /// `DEBUG` / `INFO` / `WARNING` / `ERROR` / `CRITICAL`, or `INFO` when the
    /// line did not carry a level prefix.
    pub level: String,
    /// The message without the timestamp/level prefix (stripped).
    pub message: String,
    /// The icon name to render next to the message.
    pub icon_name: String,
}

impl ActivityEntry {
    /// Build the entry icon name: successful `INFO` lines get the OK emblem.
    fn with_icon(level: String, message: String) -> Self {
        let icon_name = if level == "INFO" && is_success_line(&message) {
            OK_ICON.to_string()
        } else {
            level_icon(&level).to_string()
        };
        Self {
            level,
            message,
            icon_name,
        }
    }
}

/// Whether a log line reports a finished run. `outcome_log_line`
/// (account_runtime.rs) emits the fixed msgids "Synchronization completed"
/// / "Synchronization completed with conflicts"; match them and their Spanish
/// catalog forms (issue #125). The old English-only substring check never
/// matched the localized lines actually written to the daily log.
fn is_success_line(message: &str) -> bool {
    let lowered = message.to_lowercase();
    lowered.contains("synchronization completed") || lowered.contains("sincronización completada")
}

impl fmt::Display for ActivityEntry {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(formatter, "[{}] {}", self.level, self.message)
    }
}

/// Parse `^\d{4}-\d{2}-\d{2}\s+\d{2}:\d{2}:\d{2}\s+(DEBUG|INFO|WARNING|ERROR|CRITICAL)\s+.*$`,
/// returning the level and the message.
fn parse_log_line(line: &str) -> Option<(&'static str, &str)> {
    let bytes = line.as_bytes();
    if bytes.len() < 27 {
        return None;
    }
    let all_digits = |slice: &[u8]| slice.iter().all(u8::is_ascii_digit);
    // `YYYY-MM-DD ` (10 chars + whitespace after).
    if !all_digits(&bytes[..4]) || bytes[4] != b'-' {
        return None;
    }
    if !all_digits(&bytes[5..7]) || bytes[7] != b'-' {
        return None;
    }
    if !all_digits(&bytes[8..10]) {
        return None;
    }
    let mut position = skip_whitespace(bytes, 10);
    // `HH:MM:SS `.
    if position + 8 > bytes.len() {
        return None;
    }
    if !all_digits(&bytes[position..position + 2]) || bytes[position + 2] != b':' {
        return None;
    }
    if !all_digits(&bytes[position + 3..position + 5]) || bytes[position + 5] != b':' {
        return None;
    }
    if !all_digits(&bytes[position + 6..position + 8]) {
        return None;
    }
    position = skip_whitespace(bytes, position + 8);
    let rest = &line[position..];
    for level in ["DEBUG", "INFO", "WARNING", "ERROR", "CRITICAL"] {
        let Some(after) = rest.strip_prefix(level) else {
            continue;
        };
        // The level must be followed by whitespace (the regex `\s+`); a longer
        // token like `INFOSOMETHING` must not match `INFO`.
        let message = skip_whitespace(after.as_bytes(), 0);
        if message == 0 {
            continue;
        }
        return Some((level, after[message..].trim()));
    }
    None
}

/// Advance over one or more whitespace bytes starting at `position`.
fn skip_whitespace(bytes: &[u8], position: usize) -> usize {
    let mut position = position;
    while position < bytes.len() && bytes[position].is_ascii_whitespace() {
        position += 1;
    }
    position
}

/// Turn one formatted application log line into a compact UI entry.
pub fn parse_activity_line(line: &str) -> ActivityEntry {
    let stripped = line.trim();
    match parse_log_line(stripped) {
        Some((level, message)) => ActivityEntry::with_icon(level.to_string(), message.to_string()),
        None => ActivityEntry::with_icon("INFO".to_string(), stripped.to_string()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_level_without_exposing_prefix_in_message() {
        let entry = parse_activity_line(
            "2026-08-07 14:12:41 INFO    Synchronization completed successfully.",
        );
        assert_eq!(entry.level, "INFO");
        assert_eq!(entry.message, "Synchronization completed successfully.");
        assert_eq!(entry.icon_name, "nextsync-activity-ok");
    }

    #[test]
    fn preserves_literal_ampersand_from_nextcloudcmd() {
        let entry =
            parse_activity_line("2026-08-07 14:12:41 INFO    CMD lambda(const QJsonDocument&)");
        assert!(entry.message.contains("QJsonDocument&"));
    }

    #[test]
    fn warning_has_warning_icon() {
        let entry = parse_activity_line("2026-08-07 14:12:41 WARNING Push unavailable");
        assert_eq!(entry.level, "WARNING");
        assert_eq!(entry.icon_name, "nextsync-activity-warning");
    }

    #[test]
    fn error_and_critical_have_their_icons() {
        assert_eq!(
            parse_activity_line("2026-08-07 14:12:41 ERROR Sync failed").icon_name,
            "nextsync-activity-error"
        );
        assert_eq!(
            parse_activity_line("2026-08-07 14:12:41 CRITICAL Keyring locked").icon_name,
            "nextsync-activity-error"
        );
    }

    #[test]
    fn success_icon_is_case_insensitive_in_the_message() {
        let entry = parse_activity_line(
            "2026-08-07 14:12:41 INFO    SYNCHRONIZATION COMPLETED SUCCESSFULLY",
        );
        assert_eq!(entry.icon_name, "nextsync-activity-ok");
    }

    #[test]
    fn spanish_outcome_line_gets_the_ok_icon() {
        let entry = parse_activity_line(
            "2026-08-07 14:12:41 INFO    nacho@host · /home/user/Files: Sincronización completada",
        );
        assert_eq!(entry.icon_name, "nextsync-activity-ok");
    }

    #[test]
    fn conflict_outcome_line_gets_the_ok_icon() {
        let entry = parse_activity_line(
            "2026-08-07 14:12:41 INFO    alice@host · /home/alice/Nextcloud: Synchronization completed with conflicts",
        );
        assert_eq!(entry.icon_name, "nextsync-activity-ok");
    }

    #[test]
    fn plain_line_falls_back_to_info() {
        let entry = parse_activity_line("nextcloudcmd started");
        assert_eq!(entry.level, "INFO");
        assert_eq!(entry.message, "nextcloudcmd started");
        assert_eq!(entry.icon_name, "nextsync-activity-info");
    }

    #[test]
    fn level_prefix_without_whitespace_does_not_match() {
        let entry = parse_activity_line("2026-08-07 14:12:41 INFOSOMETHING");
        assert_eq!(entry.level, "INFO");
        assert_eq!(entry.message, "2026-08-07 14:12:41 INFOSOMETHING");
    }

    #[test]
    fn level_icon_has_the_python_defaults() {
        assert_eq!(level_icon("DEBUG"), "nextsync-activity-debug");
        assert_eq!(level_icon("INFO"), "nextsync-activity-info");
        assert_eq!(level_icon("WARNING"), "nextsync-activity-warning");
        assert_eq!(level_icon("ERROR"), "nextsync-activity-error");
        assert_eq!(level_icon("CRITICAL"), "nextsync-activity-error");
        assert_eq!(level_icon("nope"), "nextsync-activity-info");
    }
}
