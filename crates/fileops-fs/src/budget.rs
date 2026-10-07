//! The output byte budget.
//!
//! Unbounded output is the failure mode this crate exists to prevent: a `cat` of a
//! generated file, or a `grep` that matches every line, costs more context than the whole
//! task was worth. So a budget is always in force — the request can raise or lower it, but
//! not remove it — and every renderer spends from the same one, in request order.

/// Default ceiling on a single response's rendered text, in bytes.
///
/// Roughly 10k tokens of code: large enough for a dozen file slices, small enough that an
/// accident cannot eat a context window.
pub const DEFAULT_MAX_BYTES: usize = 20_000;

/// Hard ceiling on what a request may ask for, however large its `max_bytes`.
pub const MAX_MAX_BYTES: usize = 400_000;

/// A shared spend counter. Cheap to pass around; deliberately not clonable, because two
/// copies of a budget are two budgets.
#[derive(Debug)]
pub struct Budget {
    limit: usize,
    spent: usize,
}

impl Budget {
    /// `None` means [`DEFAULT_MAX_BYTES`]; anything above [`MAX_MAX_BYTES`] is clamped.
    pub fn new(limit: Option<usize>) -> Self {
        Self {
            limit: limit.unwrap_or(DEFAULT_MAX_BYTES).min(MAX_MAX_BYTES),
            spent: 0,
        }
    }

    pub fn limit(&self) -> usize {
        self.limit
    }

    pub fn spent(&self) -> usize {
        self.spent
    }

    pub fn remaining(&self) -> usize {
        self.limit.saturating_sub(self.spent)
    }

    pub fn exhausted(&self) -> bool {
        self.remaining() == 0
    }

    /// Spend `n` bytes whether or not they fit — headers and footers are small and always
    /// written, because a budget that hides the reason output stopped is worse than one
    /// that overshoots by a line.
    pub fn spend(&mut self, n: usize) {
        self.spent = self.spent.saturating_add(n);
    }

    /// Spend `n` bytes only if they fit, reporting whether they did.
    pub fn try_spend(&mut self, n: usize) -> bool {
        if n > self.remaining() {
            return false;
        }
        self.spent += n;
        true
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_request_cannot_opt_out_of_the_budget() {
        assert_eq!(Budget::new(None).limit(), DEFAULT_MAX_BYTES);
        assert_eq!(Budget::new(Some(usize::MAX)).limit(), MAX_MAX_BYTES);
        // Zero is honoured as "nothing but the frame", not reinterpreted as unlimited.
        let mut zero = Budget::new(Some(0));
        assert!(zero.exhausted());
        assert!(!zero.try_spend(1));
    }

    #[test]
    fn try_spend_refuses_rather_than_overshooting() {
        let mut budget = Budget::new(Some(10));
        assert!(budget.try_spend(6));
        assert!(!budget.try_spend(6), "would exceed the limit");
        assert_eq!(budget.remaining(), 4);
        assert!(budget.try_spend(4));
        assert!(budget.exhausted());
    }
}
