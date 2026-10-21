//! Work a reader derives from a document, kept with the document.
use std::any::{Any, TypeId};
use std::collections::HashMap;
use std::sync::{Arc, Mutex, PoisonError};

/// What readers derive from a document and may keep for as long as it lives
/// — decoded images, parsed content, loaded fonts — so that rendering the
/// same page again, as a viewer does at every zoom step, reuses it.
///
/// Off unless built with [`DerivedCache::enabled`], and only for a document
/// that will not change any more: nothing tracks edits, so a document still
/// being edited must not keep what it derived from an earlier state. A clone
/// starts with the cache off.
#[derive(Default)]
pub struct DerivedCache(Option<Mutex<HashMap<TypeId, Arc<dyn Any + Send + Sync>>>>);

impl DerivedCache {
    pub fn enabled() -> Self {
        Self(Some(Mutex::default()))
    }

    pub fn is_enabled(&self) -> bool {
        self.0.is_some()
    }

    /// The kept value of type `T`, made by `make` the first time it is asked
    /// for; `None` when the cache is off.
    pub fn get_or_init<T: Any + Send + Sync>(&self, make: impl FnOnce() -> T) -> Option<Arc<T>> {
        let slots = self.0.as_ref()?;
        let mut slots = slots.lock().unwrap_or_else(PoisonError::into_inner);
        let value = slots
            .entry(TypeId::of::<T>())
            .or_insert_with(|| Arc::new(make()));
        Arc::clone(value).downcast::<T>().ok()
    }
}

impl Clone for DerivedCache {
    fn clone(&self) -> Self {
        Self::default()
    }
}

impl std::fmt::Debug for DerivedCache {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(if self.is_enabled() {
            "DerivedCache(enabled)"
        } else {
            "DerivedCache(off)"
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn values_are_made_once_and_only_when_enabled() {
        let off = DerivedCache::default();
        assert!(off.get_or_init(|| 1u32).is_none());

        let cache = DerivedCache::enabled();
        let mut made = 0;
        for _ in 0..3 {
            let value = cache.get_or_init(|| {
                made += 1;
                7u32
            });
            assert_eq!(value.as_deref(), Some(&7));
        }
        assert_eq!(made, 1);
        assert_eq!(cache.get_or_init(|| "other").as_deref(), Some(&"other"));
        assert!(!cache.clone().is_enabled());
    }
}
