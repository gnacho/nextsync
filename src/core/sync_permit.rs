//! One-at-a-time sync permit.
//!
//! Fase 2 (Task 2.2): a FIFO semaphore so only one sync runs at a time.
//! Mirrors `core/sync_permit.py`: the whole app runs on a single main loop,
//! so a plain counter plus a FIFO queue of release callbacks is enough —
//! schedulers that cannot acquire the permit register a callback and retry
//! later, queueing up instead of hammering the network in parallel.
//!
//! Issue #35 extends the gate with folder awareness: a holder can acquire
//! the permit *for a local root*, and the acquisition is refused while
//! another holder runs on an overlapping folder (the same root or a
//! parent/child of it, compared over canonical paths). This keeps the
//! global serialization intact and additionally guarantees that two
//! reconciliations never race over the same tree even if `max_concurrent`
//! is ever raised above one.

use std::cell::RefCell;
use std::path::{Path, PathBuf};
use std::rc::Rc;

/// Error returned when constructing a [`SyncPermit`] with `max_concurrent < 1`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SyncPermitError;

impl std::fmt::Display for SyncPermitError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "max_concurrent must be at least 1")
    }
}

impl std::error::Error for SyncPermitError {}

/// Canonical form of a local sync root: symlinks resolved when the path
/// exists, `~`-expanded and made absolute otherwise. Two roots only count
/// as distinct when their canonical forms do.
pub fn canonical_sync_root(path: &Path) -> PathBuf {
    if let Ok(resolved) = path.canonicalize() {
        return resolved;
    }
    let expanded = crate::storage::config::expanduser(&path.to_string_lossy());
    std::path::absolute(&expanded).unwrap_or(expanded)
}

/// Whether two paths cover a common tree: one is equal to, an ancestor of,
/// or a descendant of the other (component-wise comparison).
pub fn paths_overlap(a: &Path, b: &Path) -> bool {
    let (mut left, mut right) = (a.components(), b.components());
    loop {
        match (left.next(), right.next()) {
            (None, None) => return true,
            (None, Some(_)) | (Some(_), None) => return true,
            (Some(x), Some(y)) if x == y => continue,
            (Some(_), Some(_)) => return false,
        }
    }
}

/// Global gate limiting how many reconciliations run at once.
///
/// `release` wakes exactly one waiter in FIFO order (the oldest first). The
/// permit is shared between schedulers by cloning, like the Python global
/// `SyncPermit`.
#[derive(Clone)]
pub struct SyncPermit {
    inner: Rc<RefCell<SyncPermitInner>>,
}

struct SyncPermitInner {
    max_concurrent: usize,
    in_use: usize,
    waiters: Vec<Box<dyn FnOnce()>>,
    /// Canonical roots of the holders that acquired with
    /// [`SyncPermit::try_acquire_root`], oldest first. Plain `try_acquire`
    /// holders stay untracked (they still consume a slot).
    roots: Vec<PathBuf>,
}

impl SyncPermit {
    /// Create a permit allowing up to `max_concurrent` simultaneous holders.
    pub fn try_new(max_concurrent: usize) -> Result<Self, SyncPermitError> {
        if max_concurrent < 1 {
            return Err(SyncPermitError);
        }
        Ok(Self {
            inner: Rc::new(RefCell::new(SyncPermitInner {
                max_concurrent,
                in_use: 0,
                waiters: Vec::new(),
                roots: Vec::new(),
            })),
        })
    }

    /// Whether the permit can be acquired right now.
    pub fn available(&self) -> bool {
        let inner = self.inner.borrow();
        inner.in_use < inner.max_concurrent
    }

    /// Try to acquire the permit without waiting.
    pub fn try_acquire(&self) -> bool {
        let mut inner = self.inner.borrow_mut();
        if inner.in_use >= inner.max_concurrent {
            return false;
        }
        inner.in_use += 1;
        true
    }

    /// Try to acquire the permit for a local sync root (issue #35).
    ///
    /// The acquisition is refused while another tracked holder runs on an
    /// overlapping folder (same root or parent/child), so two
    /// reconciliations never race over a shared tree. Untracked holders
    /// still exhaust the plain slot count.
    pub fn try_acquire_root(&self, root: &Path) -> bool {
        let canonical = canonical_sync_root(root);
        let mut inner = self.inner.borrow_mut();
        if inner.in_use >= inner.max_concurrent {
            return false;
        }
        if inner
            .roots
            .iter()
            .any(|active| paths_overlap(active, &canonical))
        {
            return false;
        }
        inner.in_use += 1;
        inner.roots.push(canonical);
        true
    }

    /// Whether a sync root would overlap a currently tracked holder.
    pub fn overlaps_active(&self, root: &Path) -> bool {
        let canonical = canonical_sync_root(root);
        let inner = self.inner.borrow();
        inner
            .roots
            .iter()
            .any(|active| paths_overlap(active, &canonical))
    }

    /// Release one slot and wake the oldest waiter, if any.
    ///
    /// The woken callback runs synchronously (Python semantics); it must not
    /// re-enter the permit. Exactly one waiter is woken per release. The
    /// oldest tracked root is dropped along with the slot.
    pub fn release(&self) {
        let waiter = {
            let mut inner = self.inner.borrow_mut();
            if inner.in_use > 0 {
                inner.in_use -= 1;
                if !inner.roots.is_empty() {
                    inner.roots.remove(0);
                }
            }
            if inner.waiters.is_empty() {
                None
            } else {
                Some(inner.waiters.remove(0))
            }
        };
        if let Some(waiter) = waiter {
            waiter();
        }
    }

    /// Register a callback to run the next time the permit is released.
    ///
    /// Callers should retry `try_acquire` from the callback.
    pub fn wait_for_release(&self, callback: impl FnOnce() + 'static) {
        self.inner.borrow_mut().waiters.push(Box::new(callback));
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn single_permit_is_acquired_and_released() {
        let permit = SyncPermit::try_new(1).unwrap();
        assert!(permit.available());
        assert!(permit.try_acquire());
        assert!(!permit.available());
        assert!(!permit.try_acquire());
        permit.release();
        assert!(permit.available());
        assert!(permit.try_acquire());
    }

    #[test]
    fn release_wakes_one_waiter_in_fifo_order() {
        let permit = SyncPermit::try_new(1).unwrap();
        let fired = Rc::new(RefCell::new(Vec::new()));
        permit.try_acquire();
        permit.wait_for_release({
            let fired = Rc::clone(&fired);
            move || fired.borrow_mut().push(1)
        });
        permit.wait_for_release({
            let fired = Rc::clone(&fired);
            move || fired.borrow_mut().push(2)
        });
        permit.release();
        assert_eq!(*fired.borrow(), vec![1]);
        // The woken waiter retries; it acquires and later releases again.
        assert!(permit.try_acquire());
        permit.release();
        assert_eq!(*fired.borrow(), vec![1, 2]);
    }

    #[test]
    fn two_concurrent_waiters_only_one_enters() {
        let permit = SyncPermit::try_new(1).unwrap();
        assert!(permit.try_acquire());
        let entered = Rc::new(RefCell::new(0));
        let max_entered = Rc::new(RefCell::new(0));
        // Two waiters compete; each retries on wake and only one can hold it.
        permit.wait_for_release({
            let permit = permit.clone();
            let entered = Rc::clone(&entered);
            let max_entered = Rc::clone(&max_entered);
            move || {
                if permit.try_acquire() {
                    *entered.borrow_mut() += 1;
                    let current = *max_entered.borrow();
                    let now = *entered.borrow();
                    *max_entered.borrow_mut() = current.max(now);
                    *entered.borrow_mut() -= 1;
                    permit.release();
                }
            }
        });
        permit.wait_for_release({
            let permit = permit.clone();
            let entered = Rc::clone(&entered);
            let max_entered = Rc::clone(&max_entered);
            move || {
                if permit.try_acquire() {
                    *entered.borrow_mut() += 1;
                    let current = *max_entered.borrow();
                    let now = *entered.borrow();
                    *max_entered.borrow_mut() = current.max(now);
                    *entered.borrow_mut() -= 1;
                    permit.release();
                }
            }
        });
        permit.release();
        // After both waiters ran their turn, only one held the permit at a time.
        assert_eq!(*max_entered.borrow(), 1);
        assert!(permit.available());
    }

    #[test]
    fn max_concurrent_limits_parallel_runs() {
        let permit = SyncPermit::try_new(2).unwrap();
        assert!(permit.try_acquire());
        assert!(permit.try_acquire());
        assert!(!permit.try_acquire());
        permit.release();
        assert!(permit.try_acquire());
    }

    #[test]
    fn rejects_zero_max_concurrent() {
        assert!(SyncPermit::try_new(0).is_err());
    }

    #[test]
    fn release_without_holder_is_harmless() {
        let permit = SyncPermit::try_new(1).unwrap();
        permit.release();
        assert!(permit.try_acquire());
        assert!(!permit.try_acquire());
    }

    #[test]
    fn shared_permit_blocks_across_clones() {
        let shared = SyncPermit::try_new(1).unwrap();
        let first = shared.clone();
        let second = shared.clone();
        assert!(first.try_acquire());
        assert!(!second.try_acquire());
        first.release();
        assert!(second.try_acquire());
    }

    // ---- issue #35: overlap-aware acquisition --------------------------------

    #[test]
    fn path_overlap_covers_same_ancestor_and_descendant() {
        let root = Path::new("/data/nc");
        assert!(paths_overlap(root, root));
        assert!(paths_overlap(root, Path::new("/data/nc/Documents")));
        assert!(paths_overlap(Path::new("/data/nc/Documents"), root));
        assert!(!paths_overlap(root, Path::new("/data/other")));
        // A prefix that stops mid-component is NOT an ancestor.
        assert!(!paths_overlap(
            Path::new("/data/nc"),
            Path::new("/data/nc-docs")
        ));
    }

    #[test]
    fn overlapping_roots_are_refused() {
        let permit = SyncPermit::try_new(2).unwrap();
        assert!(permit.try_acquire_root(Path::new("/data/nc")));
        // Same folder, child and parent all refuse while the holder runs.
        assert!(!permit.try_acquire_root(Path::new("/data/nc")));
        assert!(!permit.try_acquire_root(Path::new("/data/nc/sub")));
        assert!(!permit.try_acquire_root(Path::new("/data")));
        // A sibling folder is fine (the permit has a second slot).
        assert!(permit.try_acquire_root(Path::new("/data/other")));
        assert!(permit.overlaps_active(Path::new("/data/nc/sub/deep.txt")));
        assert!(!permit.overlaps_active(Path::new("/media/usb")));
    }

    #[test]
    fn release_frees_the_oldest_tracked_root() {
        let permit = SyncPermit::try_new(2).unwrap();
        assert!(permit.try_acquire_root(Path::new("/data/nc")));
        assert!(permit.try_acquire_root(Path::new("/data/other")));
        assert!(permit.overlaps_active(Path::new("/data/nc")));
        permit.release();
        assert!(!permit.overlaps_active(Path::new("/data/nc")));
        assert!(permit.overlaps_active(Path::new("/data/other")));
        permit.release();
        assert!(!permit.overlaps_active(Path::new("/data/other")));
        assert!(permit.available());
    }

    #[test]
    fn untracked_acquisition_still_consumes_a_slot() {
        let permit = SyncPermit::try_new(1).unwrap();
        assert!(permit.try_acquire());
        assert!(!permit.try_acquire_root(Path::new("/data/nc")));
        permit.release();
        assert!(permit.try_acquire_root(Path::new("/data/nc")));
    }

    #[test]
    fn missing_root_falls_back_to_the_expanded_form() {
        let permit = SyncPermit::try_new(2).unwrap();
        let missing = Path::new("/nonexistent-sync-root-xyz/nc");
        assert!(permit.try_acquire_root(missing));
        assert!(permit.overlaps_active(Path::new("/nonexistent-sync-root-xyz/nc/sub")));
    }
}
