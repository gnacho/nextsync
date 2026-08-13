//! Nautilus bookmark, desktop shortcut and app icon integration (Task 5.6).
//!
//! Mirrors `core/desktop_integration.py` (v0.4.0) for a single sync root.
//! Each integration is small and reversible:
//!
//! - **Nautilus bookmark**: a `file://... Name` line in
//!   `$XDG_CONFIG_HOME/gtk-3.0/bookmarks`.
//! - **Desktop shortcut**: a symlink on the XDG desktop named `<folder>` or
//!   `<folder> (NextSync)`, never overwriting a file.
//! - **Special icon**: `metadata::custom-icon-name` on the sync root (and on
//!   the managed shortcut) set to [`FOLDER_ICON_NAME`]; the pixmap is copied
//!   to `$XDG_DATA_HOME/icons/hicolor/scalable/places` when the system theme
//!   does not already provide it.
//!
//! The three features are deliberately independent; [`state`](Self::state)
//! reports each one. `initialize_defaults()` enables all three and
//! `cleanup()` removes exactly what this module created.
//!
//! # Deviations from `desktop_integration.py` (motivated)
//!
//! - **No `subscribe`/monitors**: the Python watches the bookmarks file and
//!   the desktop for external changes and notifies listeners. Nothing in the
//!   rewrite consumes those callbacks yet, so they are dropped; the module is
//!   a pure, pull-based API. (`plans/2026-08-13-rust-rewrite.md` Task 5.6.)
//! - **Metadata injectable via builder**: the required constructor is
//!   `new(local_root, config_home, data_home)`; tests inject fake metadata
//!   with [`with_metadata_getter`](Self::with_metadata_getter) /
//!   [`with_metadata_setter`](Self::with_metadata_setter) where the Python
//!   took the fakes as keyword arguments.
//! - **`initialize_defaults()` returns `[bool; 3]`** in the fixed order
//!   `[nautilus_bookmark, desktop_shortcut, special_icon]` (the Python
//!   returned a dict).
//! - **No `Path.as_uri` in std**: file URIs and `%XX` decoding are
//!   implemented inline; the encoder keeps RFC 3986 unreserved characters
//!   plus `/` literal and percent-encodes the rest, while the parser accepts
//!   both encoded and raw spaces (like the Python `urlsplit`+`unquote`).
//! - **The folder icon asset** (`data/icons/io.github.gnacho.nextsync-folder
//!   .svg`) was copied from the Python v0.4.0 repo so `set_special_icon` can
//!   work from a source build; if it is ever missing (e.g. a minimal
//!   install), `set_special_icon` returns `false`, exactly like the Python.

use std::env;
use std::fs;
use std::io;
use std::path::{Path, PathBuf};

use crate::util::paths;

/// The themed icon name applied to the sync root and the managed shortcut.
pub const FOLDER_ICON_NAME: &str = "io.github.gnacho.nextsync-folder";

/// The icon pixmap file name under `icons/hicolor/scalable/places`.
pub const FOLDER_ICON_FILENAME: &str = "io.github.gnacho.nextsync-folder.svg";

/// The GIO metadata key that stores the custom icon name.
pub const CUSTOM_ICON_ATTRIBUTE: &str = "metadata::custom-icon-name";

/// System icon theme location checked before copying to the user data dir.
const SYSTEM_PLACES_DIR: &str = "/usr/share/icons/hicolor/scalable/places";

/// Current on/off state of the three reversible integrations.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct IntegrationState {
    pub nautilus_bookmark: bool,
    pub desktop_shortcut: bool,
    pub special_icon: bool,
}

/// Metadata getter, equivalent to the Python `_get_custom_icon`.
type MetadataGetter = dyn Fn(&Path, bool) -> Option<String>;

/// Metadata setter, equivalent to the Python `_set_custom_icon`.
type MetadataSetter = dyn Fn(&Path, Option<&str>, bool) -> bool;

/// Owns the reversible GNOME integrations for one sync root.
pub struct DesktopIntegration {
    sync_root: PathBuf,
    bookmarks: PathBuf,
    desktop: PathBuf,
    data_home: PathBuf,
    icon_source: PathBuf,
    metadata_getter: Box<MetadataGetter>,
    metadata_setter: Box<MetadataSetter>,
}

impl DesktopIntegration {
    /// Create the integrations for `local_root`.
    ///
    /// `config_home` and `data_home` default to the real XDG directories
    /// (`~/.config` and `~/.local/share`) and can be injected for tests.
    /// `local_root` is expanded (`~`) and made absolute, like the Python
    /// `sync_root.expanduser().absolute()`.
    pub fn new(
        local_root: PathBuf,
        config_home: Option<PathBuf>,
        data_home: Option<PathBuf>,
    ) -> Self {
        let config_home = config_home.unwrap_or_else(xdg_config_home);
        let data_home = data_home.unwrap_or_else(paths::user_data_dir);
        let sync_root = normalize(&local_root);
        let icon_source = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
            .join("data")
            .join("icons")
            .join(FOLDER_ICON_FILENAME);
        Self {
            sync_root,
            bookmarks: config_home.join("gtk-3.0").join("bookmarks"),
            desktop: paths::desktop_dir_from(&config_home),
            data_home,
            icon_source,
            metadata_getter: Box::new(get_custom_icon),
            metadata_setter: Box::new(set_custom_icon),
        }
    }

    /// Override the bookmarks file (used by tests).
    pub fn with_bookmarks(mut self, path: PathBuf) -> Self {
        self.bookmarks = path;
        self
    }

    /// Override the desktop directory (used by tests).
    pub fn with_desktop(mut self, path: PathBuf) -> Self {
        self.desktop = path;
        self
    }

    /// Override the folder icon source file (used by tests).
    pub fn with_icon_source(mut self, path: PathBuf) -> Self {
        self.icon_source = path;
        self
    }

    /// Inject a metadata getter (used by tests).
    pub fn with_metadata_getter(
        mut self,
        getter: impl Fn(&Path, bool) -> Option<String> + 'static,
    ) -> Self {
        self.metadata_getter = Box::new(getter);
        self
    }

    /// Inject a metadata setter (used by tests).
    pub fn with_metadata_setter(
        mut self,
        setter: impl Fn(&Path, Option<&str>, bool) -> bool + 'static,
    ) -> Self {
        self.metadata_setter = Box::new(setter);
        self
    }

    /// Display names for the desktop shortcut, as the Python
    /// `desktop_names`: the folder name and `<name> (NextSync)`.
    pub fn desktop_names(&self) -> (String, String) {
        let name = self
            .sync_root
            .file_name()
            .map(|name| name.to_string_lossy().into_owned())
            .filter(|name| !name.is_empty())
            .unwrap_or_else(|| "NextCloud".to_string());
        (name.clone(), format!("{name} (NextSync)"))
    }

    /// The two desktop shortcut candidates, primary and fallback.
    pub fn desktop_candidates(&self) -> [PathBuf; 2] {
        let (primary, fallback) = self.desktop_names();
        [self.desktop.join(primary), self.desktop.join(fallback)]
    }

    /// Current state of the three integrations.
    pub fn state(&self) -> IntegrationState {
        IntegrationState {
            nautilus_bookmark: self.has_nautilus_bookmark(),
            desktop_shortcut: self.has_desktop_shortcut(),
            special_icon: self.has_special_icon(),
        }
    }

    /// Enable all three integrations, in the fixed order
    /// `[nautilus_bookmark, desktop_shortcut, special_icon]`.
    pub fn initialize_defaults(&self) -> [bool; 3] {
        [
            self.set_nautilus_bookmark(true),
            self.set_desktop_shortcut(true),
            self.set_special_icon(true),
        ]
    }

    /// Remove exactly the integrations this module created, keeping local
    /// files untouched (same order as the Python `cleanup`).
    pub fn cleanup(&self) {
        let _ = self.set_nautilus_bookmark(false);
        let _ = self.set_special_icon(false);
        let _ = self.set_desktop_shortcut(false);
    }

    /// Whether the sync root has a Nautilus bookmark line.
    pub fn has_nautilus_bookmark(&self) -> bool {
        self.bookmark_lines()
            .iter()
            .filter_map(|line| bookmark_path(line))
            .any(|path| same_path(&path, &self.sync_root))
    }

    /// Add (`enabled`) or remove the Nautilus bookmark for the sync root.
    /// Returns whether the desired state is now true.
    pub fn set_nautilus_bookmark(&self, enabled: bool) -> bool {
        let mut lines = self.bookmark_lines();
        let matching: Vec<bool> = lines
            .iter()
            .map(|line| bookmark_path(line).is_some_and(|path| same_path(&path, &self.sync_root)))
            .collect();
        if enabled {
            if matching.iter().any(|&is_match| is_match) {
                return true;
            }
            let name = self
                .sync_root
                .file_name()
                .map(|name| name.to_string_lossy().into_owned())
                .filter(|name| !name.is_empty())
                .unwrap_or_else(|| "NextCloud".to_string());
            lines.push(format!("{} {name}", file_uri(&self.sync_root)));
        } else {
            if !matching.iter().any(|&is_match| is_match) {
                return true;
            }
            lines = lines
                .into_iter()
                .zip(matching)
                .filter(|(_, is_match)| !is_match)
                .map(|(line, _)| line)
                .collect();
        }
        if self.write_bookmarks(&lines).is_err() {
            return false;
        }
        self.has_nautilus_bookmark() == enabled
    }

    /// Read the bookmark lines, treating a missing or unreadable file as an
    /// empty list (like the Python `_bookmark_lines`).
    fn bookmark_lines(&self) -> Vec<String> {
        match fs::read_to_string(&self.bookmarks) {
            Ok(content) => content.lines().map(str::to_string).collect(),
            Err(_) => Vec::new(),
        }
    }

    /// Write the bookmarks atomically (0600 tmp + fsync + rename), replicating
    /// the Python `_write_bookmarks`.
    fn write_bookmarks(&self, lines: &[String]) -> io::Result<()> {
        if let Some(parent) = self.bookmarks.parent() {
            fs::create_dir_all(parent)?;
        }
        let file_name = self
            .bookmarks
            .file_name()
            .map(|name| name.to_string_lossy().into_owned())
            .unwrap_or_default();
        let temporary = self
            .bookmarks
            .with_file_name(format!(".{file_name}.nextsync.tmp"));
        let mut payload = lines.join("\n");
        if !payload.is_empty() {
            payload.push('\n');
        }
        {
            use std::io::Write;
            use std::os::unix::fs::OpenOptionsExt;
            let mut file = fs::OpenOptions::new()
                .write(true)
                .create(true)
                .truncate(true)
                .mode(0o600)
                .open(&temporary)?;
            file.write_all(payload.as_bytes())?;
            file.sync_all()?;
        }
        fs::rename(&temporary, &self.bookmarks)
    }

    /// The managed shortcuts on the desktop pointing at the sync root.
    fn matching_desktop_shortcuts(&self) -> Vec<PathBuf> {
        let mut matches = Vec::new();
        for candidate in self.desktop_candidates() {
            if !is_symlink(&candidate) {
                continue;
            }
            let target = match fs::read_link(&candidate) {
                Ok(target) => target,
                Err(_) => continue,
            };
            let target = if target.is_absolute() {
                target
            } else {
                candidate
                    .parent()
                    .map(|parent| parent.join(&target))
                    .unwrap_or(target)
            };
            if same_path(&target, &self.sync_root) {
                matches.push(candidate);
            }
        }
        matches
    }

    /// Whether a managed shortcut exists on the desktop.
    pub fn has_desktop_shortcut(&self) -> bool {
        !self.matching_desktop_shortcuts().is_empty()
    }

    /// Create or remove the desktop shortcut, never replacing an existing
    /// file (it falls back to `<name> (NextSync)` and refuses if both are
    /// taken). Returns whether the desired state is now true.
    pub fn set_desktop_shortcut(&self, enabled: bool) -> bool {
        let matches = self.matching_desktop_shortcuts();
        if enabled {
            if !matches.is_empty() {
                return true;
            }
            if fs::create_dir_all(&self.desktop).is_err() {
                return false;
            }
            let target = self
                .desktop_candidates()
                .into_iter()
                .find(|candidate| !lexists(candidate));
            let target = match target {
                Some(target) => target,
                None => return false,
            };
            if create_dir_symlink(&self.sync_root, &target).is_err() {
                return false;
            }
            if self.has_special_icon() {
                (self.metadata_setter)(&target, Some(FOLDER_ICON_NAME), true);
            }
        } else {
            for path in matches {
                (self.metadata_setter)(&path, None, true);
                if fs::remove_file(&path).is_err() {
                    return false;
                }
            }
        }
        self.has_desktop_shortcut() == enabled
    }

    /// Whether the sync root carries the custom folder icon name.
    pub fn has_special_icon(&self) -> bool {
        (self.metadata_getter)(&self.sync_root, false).as_deref() == Some(FOLDER_ICON_NAME)
    }

    /// Apply (`enabled`) or remove the special folder icon on the sync root
    /// and on any managed shortcut. Returns whether the desired state is now
    /// true; enabling without an installable icon asset returns `false`.
    pub fn set_special_icon(&self, enabled: bool) -> bool {
        if enabled && !self.ensure_icon_available() {
            return false;
        }
        let value = if enabled {
            Some(FOLDER_ICON_NAME)
        } else {
            None
        };
        if !(self.metadata_setter)(&self.sync_root, value, false) {
            return false;
        }
        for shortcut in self.matching_desktop_shortcuts() {
            (self.metadata_setter)(&shortcut, value, true);
        }
        self.has_special_icon() == enabled
    }

    /// Make the folder icon reachable by the theme: reuse the system copy
    /// when present, otherwise install the repo asset into
    /// `$XDG_DATA_HOME/icons/hicolor/scalable/places` (atomic copy, 0644).
    /// Mirrors the Python `_ensure_icon_available`.
    fn ensure_icon_available(&self) -> bool {
        let system_icon = Path::new(SYSTEM_PLACES_DIR).join(FOLDER_ICON_FILENAME);
        if system_icon.is_file() {
            return true;
        }
        if !self.icon_source.is_file() {
            return false;
        }
        let destination = self
            .data_home
            .join("icons")
            .join("hicolor")
            .join("scalable")
            .join("places")
            .join(FOLDER_ICON_FILENAME);
        if let Some(parent) = destination.parent() {
            if fs::create_dir_all(parent).is_err() {
                return false;
            }
        }
        let needs_copy = match fs::read(&destination) {
            Ok(existing) => fs::read(&self.icon_source)
                .map(|source| existing != source)
                .unwrap_or(true),
            Err(_) => true,
        };
        if needs_copy {
            let temporary = destination.with_extension("tmp");
            if fs::copy(&self.icon_source, &temporary).is_err() {
                return false;
            }
            if let Err(error) = fs::set_permissions(&temporary, io_permissions()) {
                let _ = error;
            }
            if fs::rename(&temporary, &destination).is_err() {
                return false;
            }
        }
        true
    }
}

/// Permissions mode 0644 for the installed icon copy.
#[cfg(unix)]
fn io_permissions() -> fs::Permissions {
    use std::os::unix::fs::PermissionsExt;
    fs::Permissions::from_mode(0o644)
}

#[cfg(not(unix))]
fn io_permissions() -> fs::Permissions {
    fs::Permissions::default()
}

/// Default metadata getter over GIO, equivalent to the Python
/// `_get_custom_icon`: reads `metadata::custom-icon-name` via `query_info`.
fn get_custom_icon(path: &Path, nofollow: bool) -> Option<String> {
    use gio::prelude::*;
    let flags = query_flags(nofollow);
    gio::File::for_path(path)
        .query_info(CUSTOM_ICON_ATTRIBUTE, flags, None::<&gio::Cancellable>)
        .ok()
        .and_then(|info| info.attribute_string(CUSTOM_ICON_ATTRIBUTE))
        .map(|value| value.to_string())
}

/// Default metadata setter over GIO, equivalent to the Python
/// `_set_custom_icon`. Clearing (`value = None`) removes the attribute with
/// `G_FILE_ATTRIBUTE_TYPE_INVALID` (GLib documents that as "unset").
fn set_custom_icon(path: &Path, value: Option<&str>, nofollow: bool) -> bool {
    use gio::prelude::*;
    let flags = query_flags(nofollow);
    let file = gio::File::for_path(path);
    match value {
        Some(value) => file
            .set_attribute_string(
                CUSTOM_ICON_ATTRIBUTE,
                value,
                flags,
                None::<&gio::Cancellable>,
            )
            .is_ok(),
        None => clear_custom_icon(&file, flags),
    }
}

fn query_flags(nofollow: bool) -> gio::FileQueryInfoFlags {
    if nofollow {
        gio::FileQueryInfoFlags::NOFOLLOW_SYMLINKS
    } else {
        gio::FileQueryInfoFlags::NONE
    }
}

/// Remove `metadata::custom-icon-name` by calling `g_file_set_attribute`
/// directly with `G_FILE_ATTRIBUTE_TYPE_INVALID` and a null value.
fn clear_custom_icon(file: &gio::File, flags: gio::FileQueryInfoFlags) -> bool {
    use glib::translate::{FromGlibPtrFull, IntoGlib, ToGlibPtr};
    let attribute = std::ffi::CString::new(CUSTOM_ICON_ATTRIBUTE).expect("no NUL in constant");
    let mut error: *mut glib::ffi::GError = std::ptr::null_mut();
    // SAFETY: `file` and `attribute` outlive the call; the value pointer and
    // cancellable are null, which is accepted. A non-null returned error is
    // fully consumed via `from_glib_full`.
    let is_ok = unsafe {
        let result = gio::ffi::g_file_set_attribute(
            file.to_glib_none().0,
            attribute.as_ptr(),
            gio::FileAttributeType::Invalid.into_glib(),
            std::ptr::null_mut(),
            flags.into_glib(),
            std::ptr::null_mut(),
            &mut error,
        );
        if result != glib::ffi::GFALSE && !error.is_null() {
            let _ = glib::Error::from_glib_full(error);
        }
        result != glib::ffi::GFALSE
    };
    is_ok
}

/// Paths compare equal after expansion, absolutization and `.`/`..` folding,
/// standing in for the Python `_same_path` (`expanduser().resolve()`).
fn same_path(left: &Path, right: &Path) -> bool {
    normalize(left) == normalize(right)
}

/// Expand `~`, make absolute against the current directory and fold `.`/`..`.
fn normalize(path: &Path) -> PathBuf {
    use std::path::Component;
    let expanded = crate::storage::config::expanduser(&path.to_string_lossy());
    let absolute = if expanded.is_absolute() {
        expanded
    } else {
        env::current_dir()
            .unwrap_or_else(|_| PathBuf::from("/"))
            .join(expanded)
    };
    let mut out = PathBuf::new();
    for component in absolute.components() {
        match component {
            Component::CurDir => {}
            Component::ParentDir => {
                // A `..` that would climb above the root is dropped.
                let _ = out.pop();
            }
            other => out.push(other.as_os_str()),
        }
    }
    out
}

/// Parse the `file://...` path out of a bookmark line, replicating the
/// Python `_bookmark_path` (`urlsplit` + `unquote`). Lines whose first token
/// is not a `file://` URI on an empty/`localhost` authority return `None`.
fn bookmark_path(line: &str) -> Option<PathBuf> {
    let uri = line.trim().split(' ').next()?;
    let colon = uri.find(':')?;
    if &uri[..colon] != "file" {
        return None;
    }
    let after_scheme = &uri[colon + 1..];
    let (authority, path_part) = if let Some(rest) = after_scheme.strip_prefix("//") {
        match rest.find('/') {
            Some(index) => (&rest[..index], &rest[index..]),
            None => (rest, ""),
        }
    } else {
        ("", after_scheme)
    };
    if !authority.is_empty() && authority != "localhost" {
        return None;
    }
    let decoded = percent_decode(path_part);
    if decoded.is_empty() {
        return None;
    }
    Some(PathBuf::from(decoded))
}

/// The `file://` URI of a path, with `/` and RFC 3986 unreserved characters
/// literal and everything else percent-encoded (the Python `as_uri` keeps a
/// wider set, but the encoding is canonical and the parser accepts both).
fn file_uri(path: &Path) -> String {
    format!("file://{}", percent_encode(&path.to_string_lossy()))
}

fn percent_encode(input: &str) -> String {
    const HEX: &[u8; 16] = b"0123456789ABCDEF";
    let mut out = String::with_capacity(input.len());
    for byte in input.as_bytes() {
        match byte {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' | b'/' => {
                out.push(*byte as char);
            }
            _ => {
                out.push('%');
                out.push(HEX[(byte >> 4) as usize] as char);
                out.push(HEX[(byte & 0x0f) as usize] as char);
            }
        }
    }
    out
}

fn percent_decode(input: &str) -> String {
    let bytes = input.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut index = 0;
    while index < bytes.len() {
        if bytes[index] == b'%' && index + 2 < bytes.len() {
            if let (Some(high), Some(low)) =
                (hex_value(bytes[index + 1]), hex_value(bytes[index + 2]))
            {
                out.push(high * 16 + low);
                index += 3;
                continue;
            }
        }
        out.push(bytes[index]);
        index += 1;
    }
    String::from_utf8_lossy(&out).into_owned()
}

fn hex_value(byte: u8) -> Option<u8> {
    match byte {
        b'0'..=b'9' => Some(byte - b'0'),
        b'a'..=b'f' => Some(byte - b'a' + 10),
        b'A'..=b'F' => Some(byte - b'A' + 10),
        _ => None,
    }
}

fn is_symlink(path: &Path) -> bool {
    path.symlink_metadata()
        .map(|metadata| metadata.file_type().is_symlink())
        .unwrap_or(false)
}

/// `os.path.lexists`: true for an existing file and for a dangling symlink.
fn lexists(path: &Path) -> bool {
    path.symlink_metadata().is_ok()
}

/// Create a symlink (the app is Linux/GNOME-only, so the Unix API is used).
#[cfg(unix)]
fn create_dir_symlink(target: &Path, link: &Path) -> io::Result<()> {
    std::os::unix::fs::symlink(target, link)
}

#[cfg(not(unix))]
fn create_dir_symlink(_target: &Path, _link: &Path) -> io::Result<()> {
    Err(io::Error::new(
        io::ErrorKind::Unsupported,
        "desktop shortcuts require symlink support",
    ))
}

/// `$XDG_CONFIG_HOME` (default `~/.config`), for the default bookmarks path.
fn xdg_config_home() -> PathBuf {
    env::var_os("XDG_CONFIG_HOME")
        .filter(|value| !value.is_empty())
        .map(PathBuf::from)
        .unwrap_or_else(|| {
            env::var_os("HOME")
                .map(PathBuf::from)
                .unwrap_or_else(|| PathBuf::from("/"))
                .join(".config")
        })
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::cell::RefCell;
    use std::collections::HashMap;
    use std::rc::Rc;
    use tempfile::tempdir;

    /// In-memory metadata store replicating the Python `FakeMetadata`.
    struct FakeMetadata {
        values: Rc<RefCell<HashMap<(PathBuf, bool), String>>>,
    }

    impl FakeMetadata {
        fn new() -> Self {
            Self {
                values: Rc::new(RefCell::new(HashMap::new())),
            }
        }

        fn get(&self) -> impl Fn(&Path, bool) -> Option<String> + 'static {
            let values = self.values.clone();
            move |path, nofollow| {
                values
                    .borrow()
                    .get(&(path.to_path_buf(), nofollow))
                    .cloned()
            }
        }

        fn set(&self) -> impl Fn(&Path, Option<&str>, bool) -> bool + 'static {
            let values = self.values.clone();
            move |path, value, nofollow| {
                let mut values = values.borrow_mut();
                let key = (path.to_path_buf(), nofollow);
                match value {
                    Some(value) => {
                        values.insert(key, value.to_string());
                    }
                    None => {
                        values.remove(&key);
                    }
                }
                true
            }
        }

        fn value(&self, path: &Path, nofollow: bool) -> Option<String> {
            self.values
                .borrow()
                .get(&(path.to_path_buf(), nofollow))
                .cloned()
        }
    }

    /// Build an integration under `base`, like the Python `_integration`:
    /// sync root `base/Next Cloud`, desktop `base/Desktop`, bookmarks under
    /// `base/config/gtk-3.0`, icon `base/folder.svg`, data home `base/data`.
    fn integration(base: &Path) -> (DesktopIntegration, PathBuf, PathBuf, PathBuf, FakeMetadata) {
        let root = base.join("Next Cloud");
        fs::create_dir_all(&root).unwrap();
        let desktop = base.join("Desktop");
        let bookmarks = base.join("config").join("gtk-3.0").join("bookmarks");
        let icon = base.join("folder.svg");
        fs::write(&icon, "<svg/>").unwrap();
        let metadata = FakeMetadata::new();
        let manager = DesktopIntegration::new(
            root.clone(),
            Some(base.join("config")),
            Some(base.join("data")),
        )
        .with_desktop(desktop.clone())
        .with_icon_source(icon)
        .with_metadata_getter(metadata.get())
        .with_metadata_setter(metadata.set());
        (manager, root, desktop, bookmarks, metadata)
    }

    #[test]
    fn bookmark_preserves_other_entries_and_tracks_manual_removal() {
        let dir = tempdir().unwrap();
        let (integration, root, _desktop, bookmarks, _metadata) = integration(dir.path());
        fs::create_dir_all(bookmarks.parent().unwrap()).unwrap();
        fs::write(
            &bookmarks,
            "file:///tmp/Documents Work files\nsmb://server/share Shared\n",
        )
        .unwrap();

        assert!(integration.set_nautilus_bookmark(true));
        let content = fs::read_to_string(&bookmarks).unwrap();
        assert!(content.contains("file:///tmp/Documents Work files"));
        assert!(content.contains(&file_uri(&root)));
        assert!(integration.state().nautilus_bookmark);

        fs::write(
            &bookmarks,
            "file:///tmp/Documents Work files\nsmb://server/share Shared\n",
        )
        .unwrap();
        assert!(!integration.state().nautilus_bookmark);
    }

    #[test]
    fn desktop_shortcut_uses_safe_fallback_and_never_replaces_a_file() {
        let dir = tempdir().unwrap();
        let (integration, root, desktop, _bookmarks, _metadata) = integration(dir.path());
        fs::create_dir_all(&desktop).unwrap();
        let name = root.file_name().unwrap().to_string_lossy().into_owned();
        let collision = desktop.join(&name);
        fs::write(&collision, "keep").unwrap();

        assert!(integration.set_desktop_shortcut(true));
        let shortcut = desktop.join(format!("{name} (NextSync)"));
        assert!(is_symlink(&shortcut));
        assert_eq!(
            fs::canonicalize(&shortcut).unwrap(),
            fs::canonicalize(&root).unwrap()
        );
        assert_eq!(fs::read_to_string(&collision).unwrap(), "keep");

        assert!(integration.set_desktop_shortcut(false));
        assert!(!lexists(&shortcut));
        assert!(collision.is_file());
    }

    #[test]
    fn special_icon_applies_to_folder_and_managed_shortcut() {
        let dir = tempdir().unwrap();
        let (integration, root, desktop, _bookmarks, metadata) = integration(dir.path());
        assert!(integration.set_desktop_shortcut(true));
        assert!(integration.set_special_icon(true));
        let shortcut = desktop.join(root.file_name().unwrap());
        assert_eq!(
            metadata.value(&root, false).as_deref(),
            Some(FOLDER_ICON_NAME)
        );
        assert_eq!(
            metadata.value(&shortcut, true).as_deref(),
            Some(FOLDER_ICON_NAME)
        );
        assert!(integration.state().special_icon);

        assert!(integration.set_special_icon(false));
        assert!(metadata.value(&root, false).is_none());
        assert!(metadata.value(&shortcut, true).is_none());
        assert!(!integration.state().special_icon);
    }

    #[test]
    fn cleanup_removes_only_integrations_and_keeps_local_files() {
        let dir = tempdir().unwrap();
        let (integration, root, _desktop, _bookmarks, _metadata) = integration(dir.path());
        let local_file = root.join("keep.txt");
        fs::write(&local_file, "data").unwrap();

        assert_eq!(integration.initialize_defaults(), [true, true, true]);
        integration.cleanup();

        assert!(local_file.is_file());
        assert!(!integration.state().nautilus_bookmark);
        assert!(!integration.state().desktop_shortcut);
        assert!(!integration.state().special_icon);
    }

    #[test]
    fn set_nautilus_bookmark_is_idempotent() {
        let dir = tempdir().unwrap();
        let (integration, _root, _desktop, bookmarks, _metadata) = integration(dir.path());
        assert!(integration.set_nautilus_bookmark(true));
        assert!(integration.set_nautilus_bookmark(true));
        let content = fs::read_to_string(&bookmarks).unwrap();
        assert_eq!(content.lines().count(), 1);
        assert!(integration.set_nautilus_bookmark(false));
        assert!(!bookmarks.exists() || fs::read_to_string(&bookmarks).unwrap().is_empty());
    }

    #[test]
    fn set_desktop_shortcut_is_idempotent() {
        let dir = tempdir().unwrap();
        let (integration, root, desktop, _bookmarks, _metadata) = integration(dir.path());
        assert!(integration.set_desktop_shortcut(true));
        assert!(integration.set_desktop_shortcut(true));
        let name = root.file_name().unwrap().to_string_lossy().into_owned();
        assert!(is_symlink(&desktop.join(&name)));
        assert!(!desktop.join(format!("{name} (NextSync)")).exists());
    }

    #[test]
    fn desktop_shortcut_refuses_when_both_names_taken() {
        let dir = tempdir().unwrap();
        let (integration, root, desktop, _bookmarks, _metadata) = integration(dir.path());
        fs::create_dir_all(&desktop).unwrap();
        let name = root.file_name().unwrap().to_string_lossy().into_owned();
        fs::write(desktop.join(&name), "occupied").unwrap();
        fs::write(desktop.join(format!("{name} (NextSync)")), "occupied").unwrap();
        assert!(!integration.set_desktop_shortcut(true));
        assert_eq!(fs::read_to_string(desktop.join(&name)).unwrap(), "occupied");
    }

    #[test]
    fn special_icon_installs_pixmap_into_data_home() {
        let dir = tempdir().unwrap();
        let (integration, _root, _desktop, _bookmarks, _metadata) = integration(dir.path());
        assert!(integration.set_special_icon(true));
        let installed = dir
            .path()
            .join("data")
            .join("icons")
            .join("hicolor")
            .join("scalable")
            .join("places")
            .join(FOLDER_ICON_FILENAME);
        assert_eq!(fs::read_to_string(&installed).unwrap(), "<svg/>");
    }

    #[test]
    fn set_special_icon_returns_false_when_icon_asset_missing() {
        let dir = tempdir().unwrap();
        let (integration, _root, _desktop, _bookmarks, _metadata) = integration(dir.path());
        let missing = dir.path().join("missing.svg");
        let integration = integration.with_icon_source(missing);
        assert!(!integration.set_special_icon(true));
        assert!(!integration.has_special_icon());
    }

    #[test]
    fn default_gio_getter_reports_no_icon_on_fresh_root() {
        let dir = tempdir().unwrap();
        let root = dir.path().join("root");
        fs::create_dir_all(&root).unwrap();
        let integration = DesktopIntegration::new(
            root,
            Some(dir.path().join("config")),
            Some(dir.path().join("data")),
        );
        assert!(!integration.has_special_icon());
    }

    #[test]
    fn bookmark_path_parses_file_uris() {
        assert_eq!(
            bookmark_path("file:///tmp/Next%20Cloud Name").unwrap(),
            PathBuf::from("/tmp/Next Cloud")
        );
        assert_eq!(
            bookmark_path("file:///tmp/Next Cloud Name").unwrap(),
            PathBuf::from("/tmp/Next")
        );
        assert_eq!(
            bookmark_path("file://localhost/tmp/x").unwrap(),
            PathBuf::from("/tmp/x")
        );
        assert_eq!(
            bookmark_path("file:/tmp/no-double-slash").unwrap(),
            PathBuf::from("/tmp/no-double-slash")
        );
        assert!(bookmark_path("smb://server/share Shared").is_none());
        assert!(bookmark_path("https://example.test/").is_none());
        assert!(bookmark_path("").is_none());
    }

    #[test]
    fn file_uri_round_trips_percent_encoding() {
        let path = PathBuf::from("/tmp/Next Cloud/ñño");
        let uri = file_uri(&path);
        assert_eq!(uri, "file:///tmp/Next%20Cloud/%C3%B1%C3%B1o");
        assert_eq!(bookmark_path(&uri).unwrap(), path);
    }

    #[test]
    fn same_path_normalizes_tilde_and_dot_segments() {
        let base = tempdir().unwrap();
        let target = base.path().join("a").join("b");
        fs::create_dir_all(&target).unwrap();
        let left = base.path().join("a").join(".").join("b");
        let right = target.clone();
        assert!(same_path(&left, &right));
        assert!(same_path(
            &base.path().join("a").join("..").join("a").join("b"),
            &right
        ));
        assert!(!same_path(&base.path().join("a"), &right));
    }

    #[test]
    fn desktop_names_use_folder_name_and_fallback() {
        let dir = tempdir().unwrap();
        let (integration, root, _desktop, _bookmarks, _metadata) = integration(dir.path());
        assert_eq!(
            integration.desktop_names(),
            (
                "Next Cloud".to_string(),
                "Next Cloud (NextSync)".to_string()
            )
        );
        let integration = DesktopIntegration::new(
            PathBuf::from("/"),
            Some(dir.path().join("config")),
            Some(dir.path().join("data")),
        );
        assert_eq!(
            integration.desktop_names(),
            ("NextCloud".to_string(), "NextCloud (NextSync)".to_string())
        );
        assert_eq!(
            root.file_name().map(|n| n.to_string_lossy().into_owned()),
            Some("Next Cloud".to_string())
        );
    }
}
