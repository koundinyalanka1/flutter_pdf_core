//! Local PDF Helper hardening: cumulative allocation budget for decoded
//! bitmaps and other large buffers. Kept per thread and per decode call.

use crate::error::{OverflowError, Result};

const MAX_BYTES: usize = 128 * 1024 * 1024;

#[cfg(feature = "std")]
std::thread_local! {
    static REMAINING: std::cell::Cell<Option<usize>> = const { std::cell::Cell::new(None) };
}

pub(crate) struct Budget {
    #[cfg(feature = "std")]
    previous: Option<usize>,
}

impl Budget {
    pub(crate) fn enter() -> Self {
        Self {
            #[cfg(feature = "std")]
            previous: REMAINING.with(|budget| budget.replace(Some(MAX_BYTES))),
        }
    }
}

impl Drop for Budget {
    fn drop(&mut self) {
        #[cfg(feature = "std")]
        REMAINING.with(|budget| budget.set(self.previous));
    }
}

pub(crate) fn charge(bytes: usize) -> Result<()> {
    if bytes > MAX_BYTES {
        return Err(OverflowError::BitmapDimension.into());
    }
    #[cfg(feature = "std")]
    REMAINING.with(|budget| {
        if let Some(remaining) = budget.get() {
            let remaining = remaining
                .checked_sub(bytes)
                .ok_or(OverflowError::BitmapDimension)?;
            budget.set(Some(remaining));
        }
        Ok::<(), crate::DecodeError>(())
    })?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn cumulative_budget_rejects_without_allocating_and_resets_between_images() {
        {
            let _budget = Budget::enter();
            charge(MAX_BYTES - 1).unwrap();
            assert!(charge(2).is_err());
        }
        let _budget = Budget::enter();
        charge(MAX_BYTES).unwrap();
        assert!(charge(1).is_err());
    }
}
