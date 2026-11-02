//! A swappable [`Catalog`] holder (see [`CatalogCell`]).

use super::Catalog;

/// A [`Catalog`] that can be REPLACED through `&self`.
///
/// Readers deref to the current catalog for the length of a borrow;
/// [`Self::swap`] installs a new one and parks the old one in `retired`
/// instead of freeing it, so a reader holding the old `&Catalog` (another
/// thread mid-statement) stays valid. Retired catalogs are freed by
/// [`Self::reclaim`], which needs `&mut self` — proof that no borrow of
/// any catalog is outstanding. Mutation (`DerefMut`) is likewise
/// `&mut`-only.
pub struct CatalogCell {
    cur: std::sync::atomic::AtomicPtr<Catalog>,
    // Boxed on purpose: readers hold `&Catalog` into retired entries, so
    // they must not move when the Vec reallocates.
    #[allow(clippy::vec_box)]
    retired: std::sync::Mutex<Vec<Box<Catalog>>>,
}

impl CatalogCell {
    pub fn new(c: Catalog) -> Self {
        Self {
            cur: std::sync::atomic::AtomicPtr::new(Box::into_raw(Box::new(c))),
            retired: std::sync::Mutex::new(Vec::new()),
        }
    }

    /// Install `c` as the current catalog (callable through `&self`).
    pub fn swap(&self, c: Catalog) {
        let new = Box::into_raw(Box::new(c));
        let old = self.cur.swap(new, std::sync::atomic::Ordering::AcqRel);
        // SAFETY: `old` came from Box::into_raw and is no longer
        // reachable through `cur`; it is only parked, never freed, here.
        self.retired
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .push(unsafe { Box::from_raw(old) });
    }

    /// Free the catalogs replaced by [`Self::swap`].
    pub fn reclaim(&mut self) {
        self.retired
            .get_mut()
            .unwrap_or_else(|e| e.into_inner())
            .clear();
    }

    /// Replace the catalog under exclusive access.
    pub fn set(&mut self, c: Catalog) {
        **self = c;
    }
}

impl std::ops::Deref for CatalogCell {
    type Target = Catalog;
    fn deref(&self) -> &Catalog {
        // SAFETY: `cur` always points at a live Box; replaced boxes are
        // parked in `retired` until `reclaim` (&mut).
        unsafe { &*self.cur.load(std::sync::atomic::Ordering::Acquire) }
    }
}

impl std::ops::DerefMut for CatalogCell {
    fn deref_mut(&mut self) -> &mut Catalog {
        // SAFETY: `&mut self` is exclusive.
        unsafe { &mut *self.cur.load(std::sync::atomic::Ordering::Acquire) }
    }
}

impl Drop for CatalogCell {
    fn drop(&mut self) {
        // SAFETY: exclusive; `cur` owns its box.
        unsafe { drop(Box::from_raw(*self.cur.get_mut())) };
    }
}

// SAFETY: the raw pointer is an owned Box<Catalog>; Catalog is Send+Sync
// (it is shared by reference across reader threads today).
unsafe impl Send for CatalogCell {}
unsafe impl Sync for CatalogCell {}
