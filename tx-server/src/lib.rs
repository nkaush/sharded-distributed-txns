pub mod sharding;
pub mod server;
pub mod pool;

use crate::sharding::object::{Diffable, Updateable};

#[derive(Debug, Clone)]
pub struct BalanceDiff(tx_common::Balance);

#[derive(Debug, Clone)]
pub struct NegativeBalance(tx_common::Balance);

impl Updateable for BalanceDiff {
    fn update(&mut self, other: &Self) {
        let BalanceDiff(inner) = self;
        let BalanceDiff(other) = other;

        *inner += other;
    }
}

impl Diffable<BalanceDiff> for tx_common::Balance {
    #[cfg(test)]
    type ConsistencyCheckError = ();

    #[cfg(not(test))]
    type ConsistencyCheckError = NegativeBalance;

    fn diff(&self, diff: &BalanceDiff) -> Self { 
        let BalanceDiff(change) = diff;
        self + change
    }

    #[cfg(test)]
    fn check(self) -> Result<Self, Self::ConsistencyCheckError> {
        if self >= 0 {
            Ok(self)
        } else {
            Err(())
        }
    }

    #[cfg(not(test))]
    fn check(self) -> Result<Self, Self::ConsistencyCheckError> {
        if self >= 0 {
            Ok(self)
        } else {
            Err(NegativeBalance(self))
        }
    }
}