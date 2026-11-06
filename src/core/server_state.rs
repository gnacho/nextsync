//! Server-side presence of the paths quoted by the deletion review
//! (issue #294).
//!
//! The review dialog can already say what each answer would do, but not
//! whether the files it lists still exist on the server, and that decides
//! which answer can recover them: a rescan only brings back what the server
//! still has, and the trash only what was deleted on the server side. The
//! check costs one shallow `PROPFIND` per path, so the number of probes is
//! capped ([`MAX_PRESENCE_CHECKS`]) and every summary reports how much was
//! actually checked instead of guessing.

use crate::util::i18n::{plural, t};

/// Cap of paths probed per review; bigger deletions report the checked
/// subset instead of turning the dialog into thousands of requests.
pub const MAX_PRESENCE_CHECKS: usize = 200;

/// Cap of paths probed per group, so one huge folder does not eat the whole
/// budget and leave the other groups unmarked.
pub const GROUP_PRESENCE_CAP: usize = 25;

/// What the server answered about one reviewed path.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PathPresence {
    /// The path still exists on the server.
    Present,
    /// The path is not on the server any more.
    Gone,
    /// The probe did not run, or the check stopped before reaching it.
    Unchecked,
}

/// Counts of the three [`PathPresence`] values over a set of paths.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct PresenceSummary {
    /// Paths the server still has.
    pub present: usize,
    /// Paths the server no longer has.
    pub gone: usize,
    /// Paths that were not probed.
    pub unchecked: usize,
}

impl PresenceSummary {
    /// Aggregate one presence per path.
    pub fn of(presences: impl Iterator<Item = PathPresence>) -> Self {
        let mut summary = Self::default();
        for presence in presences {
            match presence {
                PathPresence::Present => summary.present += 1,
                PathPresence::Gone => summary.gone += 1,
                PathPresence::Unchecked => summary.unchecked += 1,
            }
        }
        summary
    }

    /// Paths the server answered about.
    pub fn checked(&self) -> usize {
        self.present + self.gone
    }
}

/// Subtitle of one reviewed row.
pub fn row_subtitle(presence: PathPresence) -> &'static str {
    match presence {
        PathPresence::Present => t("still on the server"),
        PathPresence::Gone => t("already gone from the server"),
        PathPresence::Unchecked => t("server state not checked"),
    }
}

/// Note naming the local conflicted copies that could restore a path (issue
/// #302): the content is on disk under the engine's conflict name.
pub fn copies_note(count: usize) -> String {
    counted(
        count,
        t("1 local conflicted copy"),
        t("{count} local conflicted copies"),
    )
}

/// Subtitle of one reviewed row: the server answer plus any local copies.
///
/// A path the server already dropped still reads as recoverable when a copy
/// sits next to it, and a failed probe falls back to the copy note instead of
/// hiding it.
pub fn row_subtitle_with(presence: PathPresence, copies: usize) -> String {
    let server = match presence {
        PathPresence::Unchecked => None,
        checked => Some(row_subtitle(checked).to_string()),
    };
    let note = (copies > 0).then(|| copies_note(copies));
    match (server, note) {
        (Some(server), Some(note)) => format!("{server} · {note}"),
        (Some(server), None) => server,
        (None, Some(note)) => note,
        (None, None) => String::new(),
    }
}

/// Subtitle of a group row: its size plus what the probes found. A partial
/// check says so, so the numbers are never read as the whole group.
pub fn group_subtitle(total: usize, summary: &PresenceSummary) -> String {
    let mut text = plural(total as u64, t("{count} file"), t("{count} files"))
        .replace("{count}", &total.to_string());
    if summary.checked() == 0 {
        return text;
    }
    text.push_str(" · ");
    if summary.unchecked > 0 {
        text.push_str(
            &t("checked {checked} of {count}")
                .replacen("{checked}", &summary.checked().to_string(), 1)
                .replacen("{count}", &total.to_string(), 1),
        );
        text.push_str(" · ");
    }
    text.push_str(&outcome(summary));
    text
}

/// The "what the server still has" half of a group subtitle.
fn outcome(summary: &PresenceSummary) -> String {
    if summary.gone == 0 {
        return t("all still on the server").to_string();
    }
    if summary.present == 0 {
        return t("all already gone from the server").to_string();
    }
    format!(
        "{} · {}",
        counted(
            summary.gone,
            t("1 already gone from the server"),
            t("{count} already gone from the server")
        ),
        counted(
            summary.present,
            t("1 still there"),
            t("{count} still there")
        ),
    )
}

/// Fill a counted message, picking the singular msgid when there is one.
fn counted(count: usize, one: &'static str, many: &'static str) -> String {
    plural(count as u64, one, many).replacen("{count}", &count.to_string(), 1)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::util::i18n::{set_locale, Locale};

    /// The developer's own locale may select the Spanish catalog, so every
    /// assertion about English text pins the locale first (the override is
    /// thread-local, so parallel tests do not interfere).
    fn english() {
        set_locale(Locale::English);
    }

    #[test]
    fn summary_counts_each_presence() {
        let summary = PresenceSummary::of(
            [
                PathPresence::Gone,
                PathPresence::Present,
                PathPresence::Gone,
                PathPresence::Unchecked,
            ]
            .into_iter(),
        );
        assert_eq!(
            summary,
            PresenceSummary {
                present: 1,
                gone: 2,
                unchecked: 1
            }
        );
        assert_eq!(summary.checked(), 3);
    }

    #[test]
    fn empty_summary_counts_as_unchecked() {
        english();
        let summary = PresenceSummary::of(std::iter::empty());
        assert_eq!(summary.checked(), 0);
        assert_eq!(group_subtitle(133, &summary), "133 files");
    }

    #[test]
    fn group_subtitle_reports_a_full_check() {
        english();
        let all_gone = PresenceSummary {
            present: 0,
            gone: 133,
            unchecked: 0,
        };
        assert_eq!(
            group_subtitle(133, &all_gone),
            "133 files · all already gone from the server"
        );
        let all_present = PresenceSummary {
            present: 133,
            gone: 0,
            unchecked: 0,
        };
        assert_eq!(
            group_subtitle(133, &all_present),
            "133 files · all still on the server"
        );
    }

    #[test]
    fn group_subtitle_splits_a_mixed_result() {
        english();
        let mixed = PresenceSummary {
            present: 13,
            gone: 120,
            unchecked: 0,
        };
        assert_eq!(
            group_subtitle(133, &mixed),
            "133 files · 120 already gone from the server · 13 still there"
        );
    }

    #[test]
    fn group_subtitle_keeps_single_paths_singular() {
        english();
        let mixed = PresenceSummary {
            present: 1,
            gone: 1,
            unchecked: 0,
        };
        assert_eq!(
            group_subtitle(2, &mixed),
            "2 files · 1 already gone from the server · 1 still there"
        );
    }

    #[test]
    fn group_subtitle_admits_a_partial_check() {
        english();
        let partial = PresenceSummary {
            present: 0,
            gone: 25,
            unchecked: 108,
        };
        assert_eq!(
            group_subtitle(133, &partial),
            "133 files · checked 25 of 133 · all already gone from the server"
        );
    }

    #[test]
    fn single_file_group_uses_the_singular() {
        english();
        let gone = PresenceSummary {
            present: 0,
            gone: 1,
            unchecked: 0,
        };
        assert_eq!(
            group_subtitle(1, &gone),
            "1 file · all already gone from the server"
        );
    }

    #[test]
    fn row_subtitles_are_translated() {
        set_locale(Locale::Spanish);
        assert_eq!(
            row_subtitle(PathPresence::Gone),
            "ya no está en el servidor"
        );
        assert_eq!(row_subtitle(PathPresence::Present), "sigue en el servidor");
        english();
        assert_eq!(
            row_subtitle(PathPresence::Gone),
            "already gone from the server"
        );
    }

    #[test]
    fn copy_notes_count_the_copies() {
        english();
        assert_eq!(copies_note(1), "1 local conflicted copy");
        assert_eq!(copies_note(3), "3 local conflicted copies");
    }

    #[test]
    fn row_subtitle_with_joins_server_state_and_copies() {
        english();
        assert_eq!(
            row_subtitle_with(PathPresence::Gone, 2),
            "already gone from the server · 2 local conflicted copies"
        );
        assert_eq!(
            row_subtitle_with(PathPresence::Present, 0),
            "still on the server"
        );
    }

    #[test]
    fn unchecked_rows_keep_the_copy_note() {
        english();
        assert_eq!(
            row_subtitle_with(PathPresence::Unchecked, 1),
            "1 local conflicted copy"
        );
        assert_eq!(row_subtitle_with(PathPresence::Unchecked, 0), "");
    }
}
