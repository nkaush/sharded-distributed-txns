use std::{
    collections::{BTreeMap, BTreeSet}, 
    ops::Bound::{Excluded, Included},
    convert::Infallible
};
use super::{transaction_id::TransactionId, Checkable};
use tx_common::config::NodeId;
use log::{debug};

#[derive(Debug)]
struct TentativeWrite<T> where {
    value: T
}

impl<T> TentativeWrite<T>
where 
    T: Checkable 
{
    fn new(value: T) -> Self {
        Self { value }
    }

    fn update(&mut self, value: T) {
        self.value = value;
    }
}

pub struct TimestampedObject<T> {
    value: T,
    committed_timestamp: TransactionId,
    read_timestamps: BTreeSet<TransactionId>,
    tentative_writes: BTreeMap<TransactionId, TentativeWrite<T>>
}

#[derive(Debug, PartialEq, Eq)]
pub enum RWFailure {
    WaitFor(TransactionId),
    AbortedNotFound,
    Abort
}

#[derive(Debug, PartialEq, Eq)]
pub enum CommitFailure<E> {
    WaitFor(TransactionId),
    ConsistencyCheckFailed(E),
}

#[derive(Debug, PartialEq, Eq)]
pub enum CommitSuccess<T> {
    ValueChanged(T),
    NoChange(T)
}

#[derive(Debug, PartialEq, Eq)]
pub enum CheckCommitSuccess<T> {
    CommitValue(T),
    NothingToCommit
}

impl<T> TimestampedObject<T> 
where 
    T: Clone + Checkable
{
    pub fn default(owner_id: NodeId) -> Self where T: Default {
        Self {
            value: Default::default(),
            committed_timestamp: TransactionId::default(owner_id),
            read_timestamps: BTreeSet::new(),
            tentative_writes: BTreeMap::new()
        }
    }

    pub fn read(&mut self, id: &TransactionId) -> Result<T, RWFailure> {
        if id > &self.committed_timestamp {
            // Get a range of timestamps starting from the committed timestamp
            // to the timestamp of the read request transaction, inclusive
            let ts_range = (Excluded(self.committed_timestamp), Included(*id));

            // Get the final timestamp of the range such that we have the 
            // version of the object with the maximum write timestamp less than 
            // or equal to the requested read timestamp
            let mut tw_range = self.tentative_writes.range(ts_range);
            let ts_lte_id = tw_range.next_back();

            match ts_lte_id {
                None => { 
                    // There have been no commits, the requesting transaction 
                    // has not performed a tentative write, and there were no
                    // transactions older than this one that DID write that we 
                    // can wait on... so abort
                    if self.committed_timestamp.is_default() {
                        Err(RWFailure::AbortedNotFound)
                    } else {
                        // if the timestamp we found is the committed timestamp
                        // read Ds and add Tc to RTS list (if not already added)
                        self.read_timestamps.insert(*id);
                        Ok(self.value.clone())
                    }
                },
                Some((ts, tw)) => {
                    if ts == id { // if Ds was written by Tc, simply read Ds
                        self.read_timestamps.insert(*id);
                        Ok(tw.value.clone())
                    } else {
                        // Wait until the transaction that wrote Ds is committed 
                        // or aborted, and reapply the read rule. If the 
                        // transaction is committed, Tc will read its value 
                        // after the wait. If the transaction is aborted, Tc 
                        // will read the value from an older transaction.
                        Err(RWFailure::WaitFor(*ts))
                    }
                }
            }
        } else {
            // Too late! A transaction with a later timestamp has either already 
            // read or has already written to this object
            Err(RWFailure::Abort)
        }
    }

    pub fn write(&mut self, id: &TransactionId, value: T) -> Result<(), RWFailure> {
        debug!("{:?}", self.read_timestamps);
        let is_after_mrt = self.read_timestamps
            .iter()
            .next_back()
            .map_or_else(|| true, |mrt| id >= mrt);

        // If the requesting transaction is OR is after the max read timestamp 
        // on the object AND is after the write timestamp on the committed 
        // version of the object, then perform a tentative write on the object
        if is_after_mrt && id > &self.committed_timestamp {
            // Modify the entry for the tentative write if the requesting 
            // transaction has already performed a tentative write. Otherwise,
            // insert a tentative write for the object for the transaction.
            self.tentative_writes
                .entry(*id)
                .and_modify(|tw| tw.update(value.clone()))
                .or_insert(TentativeWrite::new(value));

            Ok(())
        } else {
            // Too late! A transaction with a later timestamp has either already 
            // read or has already written to this object
            Err(RWFailure::Abort)
        }
    }

    pub fn check_commit(&self, id: &TransactionId) -> Result<CheckCommitSuccess<()>, CommitFailure<T::ConsistencyCheckError>> {
        if !self.tentative_writes.contains_key(id) {
            return Ok(CheckCommitSuccess::NothingToCommit);
        }
        
        match self.tentative_writes.keys().next() {
            Some(first) => {
                if id == first {
                    // TODO: drain read timestamps that are less than committed timestamp???
                    let tw = self.tentative_writes                    
                        .get(id)
                        .unwrap();

                    tw.value
                        .check()
                        .map(|v| CheckCommitSuccess::CommitValue(v))
                        .map_err(|e| CommitFailure::ConsistencyCheckFailed(e))
                } else {
                    Err(CommitFailure::WaitFor(*first))
                }
            },
            None => unreachable!()
        }
    }

    pub fn commit(&mut self, id: &TransactionId) -> Result<CommitSuccess<T>, CommitFailure<T::ConsistencyCheckError>> {
        self.check_commit(id)
            .map(|success| {
                if let CheckCommitSuccess::CommitValue(_) = success {
                    let (ts, tw) = self.tentative_writes
                        .remove_entry(id)
                        .unwrap();
                    self.committed_timestamp = ts;
                    self.value = tw.value;

                    CommitSuccess::ValueChanged(self.value.clone())
                } else {
                    CommitSuccess::NoChange(self.value.clone())
                }
        })
    }

    pub fn can_reap(&self, aborting_id: &TransactionId) -> bool {
        let only_violation = self.tentative_writes.len() == 1 
            && self.tentative_writes.contains_key(aborting_id);
        
        self.committed_timestamp.is_default() 
            && (self.tentative_writes.is_empty() || only_violation)
    }

    pub fn abort(&mut self, id: &TransactionId) -> Result<(), Infallible> {
        self.tentative_writes.remove(id);
        self.read_timestamps.remove(id); // TODO confirm we need this

        Ok(())
    }
}

#[cfg(test)]
mod test {
    use crate::sharding::{transaction_id::*};
    use super::*;

    fn verify_check_commit_success(object: &TimestampedObject<i64>, id: &TransactionId) {
        assert!(object.check_commit(&id).is_ok());
    }

    fn verify_check_commit_failure(object: &TimestampedObject<i64>, id: &TransactionId, f: CommitFailure<()>) {
        let check = object.check_commit(&id);
        assert!(check.is_err());
        assert_eq!(check.unwrap_err(), f);
    }

    fn verify_commit_success(object: &mut TimestampedObject<i64>, id: &TransactionId, expected: i64) {
        let commit_res = object.commit(&id);
        assert!(commit_res.is_ok());
        assert_eq!(commit_res.unwrap(), CommitSuccess::ValueChanged(expected));
        assert_eq!(object.value, expected);
        assert_eq!(&object.committed_timestamp, id);
    }

    fn verify_commit_failure(object: &mut TimestampedObject<i64>, id: &TransactionId, f: CommitFailure<()>) {
        let original_value = object.value;
        let original_cts = object.committed_timestamp;

        let commit_res = object.commit(&id);
        assert!(commit_res.is_err());
        assert_eq!(commit_res.unwrap_err(), f);

        // Ensure that the object's committed value was not changed
        assert_eq!(object.value, original_value);
        assert_eq!(object.committed_timestamp, original_cts);
    }

    fn verify_read(object: &mut TimestampedObject<i64>, id: &TransactionId, expected: i64) {
        let read_res = object.read(&id);
        assert!(read_res.is_ok());
        assert_eq!(read_res.unwrap(), expected);
    }

    #[test]
    fn test_basic_write() {
        let mut object = TimestampedObject::default('A');
        let mut id_gen = TransactionIdGenerator::new('B');
        let tx = id_gen.next();

        // Basic write should be able to write with no conflicting transactions
        assert!(object.write(&tx, 10).is_ok());

        verify_check_commit_success(&object, &tx);
        verify_commit_success(&mut object, &tx, 10);
    }

    #[test]
    fn test_basic_write_with_update() {
        let mut object = TimestampedObject::default('A');
        let mut id_gen = TransactionIdGenerator::new('B');
        let tx = id_gen.next();

        // Basic write should be able to write with no conflicting transactions
        assert!(object.write(&tx, 10).is_ok());

        // Basic write that takes balance negative should succeed 
        assert!(object.write(&tx, -10).is_ok());

        // Basic write should be able to write again with no conflicting transactions
        assert!(object.write(&tx, 30).is_ok());

        verify_check_commit_success(&object, &tx);
        verify_commit_success(&mut object, &tx, 30);
    }

    #[test]
    fn test_commit_stall() {
        let mut object = TimestampedObject::default('A');
        let mut id_gen = TransactionIdGenerator::new('B');
        let tx1 = id_gen.next();
        let tx2 = id_gen.next();

        // Older transaction writes first...
        assert!(object.write(&tx1, 10).is_ok());

        // Newer transaction writes next...
        assert!(object.write(&tx2, 30).is_ok());

        // Newer transaction must wait for older transaction to commit/abort
        verify_check_commit_failure(&object, &tx2, CommitFailure::WaitFor(tx1));
        verify_commit_failure(&mut object, &tx2, CommitFailure::WaitFor(tx1));

        // Older transaction should be able to commit without failure
        verify_check_commit_success(&object, &tx1);
        verify_commit_success(&mut object, &tx1, 10);

        // Newer transaction should be able to commit after older transaction
        verify_check_commit_success(&object, &tx2);
        verify_commit_success(&mut object, &tx2, 30);
    }

    #[test]
    fn test_write_after_newer_commit() {
        let mut object = TimestampedObject::default('A');
        let mut id_gen = TransactionIdGenerator::new('B');
        let tx1 = id_gen.next();
        let tx2 = id_gen.next();

        // Newer write should succeed without any other writes present
        assert!(object.write(&tx2, 20).is_ok());

        // Newer transaction should be able to commit since no older 
        // transactions have written to this object yet
        verify_check_commit_success(&object, &tx2);
        verify_commit_success(&mut object, &tx2, 20);

        // Older transaction should not be able to write since a newer 
        // transaction has written and committed a value
        let write_res = object.write(&tx1, 10);
        assert!(write_res.is_err());
        assert_eq!(write_res.unwrap_err(), RWFailure::Abort);
    }
    
    #[test]
    fn test_newer_transaction_writes_first() {
        let mut object = TimestampedObject::default('A');
        let mut id_gen = TransactionIdGenerator::new('B');
        let tx1 = id_gen.next();
        let tx2 = id_gen.next();

        // Newer write should succeed without any other writes present
        assert!(object.write(&tx2, 20).is_ok());

        // Older write should also succeed.
        assert!(object.write(&tx1, 30).is_ok());

        // Older transaction should be able to commit
        verify_check_commit_success(&object, &tx1);
        verify_commit_success(&mut object, &tx1, 30);

        // Newer transaction should also be able to commit
        verify_check_commit_success(&object, &tx2);
        verify_commit_success(&mut object, &tx2, 20);
    }

    #[test]
    fn test_basic_abort() {
        let mut object = TimestampedObject::<i64>::default('A');
        let mut id_gen = TransactionIdGenerator::new('B');
        let tx = id_gen.next();

        // Basic write should be able to write with no conflicting transactions
        assert!(object.write(&tx, 10).is_ok());

        // Basic write should be able to write again with no conflicting transactions
        assert!(object.write(&tx, 20).is_ok());

        // Abort the transaction
        assert!(object.abort(&tx).is_ok());

        // Ensure that no updates have been made to the object
        assert_eq!(object.value, 0);
        assert_eq!(object.committed_timestamp, TransactionId::default('A'));
    }

    #[test]
    fn test_aborted_transaction_with_future_commits() {
        let mut object = TimestampedObject::default('A');
        let mut id_gen = TransactionIdGenerator::new('B');
        let tx1 = id_gen.next();
        let tx2 = id_gen.next();

        // Older transaction writes first...
        assert!(object.write(&tx1, 10).is_ok());

        // Newer transaction writes next...
        assert!(object.write(&tx2, 30).is_ok());

        // Abort the older transaction
        assert!(object.abort(&tx1).is_ok());

        // Ensure that no updates have been made to the object
        assert_eq!(object.value, 0);
        assert_eq!(object.committed_timestamp, TransactionId::default('A'));

        // Newer transaction should be able to commit after older transaction
        // was aborted, and the older transaction should not be applied.
        verify_check_commit_success(&object, &tx2);
        verify_commit_success(&mut object, &tx2, 30);
    }

    #[test]
    fn test_basic_consistency_check_failure() {
        let mut object = TimestampedObject::default('A');
        let mut id_gen = TransactionIdGenerator::new('B');
        let tx = id_gen.next();

        // Basic write should be able to write with no conflicting transactions
        assert!(object.write(&tx, 1).is_ok());

        // Another write should be able to write with no conflicting transactions
        assert!(object.write(&tx, -10).is_ok());

        verify_check_commit_failure(&object, &tx, CommitFailure::ConsistencyCheckFailed(()));
        verify_commit_failure(&mut object, &tx, CommitFailure::ConsistencyCheckFailed(()));
    }

    #[test]
    fn test_consistency_check_failure_with_future_commit() {
        let mut object = TimestampedObject::default('A');
        let mut id_gen = TransactionIdGenerator::new('B');
        let tx1 = id_gen.next();
        let tx2 = id_gen.next();

        // Write the diff that will make the consistency check fail
        assert!(object.write(&tx1, -10).is_ok());

        // Different tx makes a write that passes the consistency check
        assert!(object.write(&tx2, 10).is_ok());

        // The consistency check on the bad transaction should fail
        verify_check_commit_failure(&object, &tx1, CommitFailure::ConsistencyCheckFailed(()));
        verify_commit_failure(&mut object, &tx1, CommitFailure::ConsistencyCheckFailed(()));

        assert!(object.abort(&tx1).is_ok());

        // The consistency check on the next transaction will succeed
        verify_check_commit_success(&object, &tx2);
        verify_commit_success(&mut object, &tx2, 10);
    }

    #[test]
    fn test_basic_read() {
        let mut object = TimestampedObject::default('A');
        let mut id_gen = TransactionIdGenerator::new('B');
        let tx1 = id_gen.next();
        let tx2 = id_gen.next();

        assert!(object.write(&tx1, 10).is_ok());
        verify_read(&mut object, &tx1, 10);

        verify_check_commit_success(&object, &tx1);
        verify_commit_success(&mut object, &tx1, 10);

        verify_read(&mut object, &tx2, 10);
    }

    #[test]
    fn test_read_before_non_committed_write() {
        let mut object = TimestampedObject::default('A');
        let mut id_gen = TransactionIdGenerator::new('B');
        let tx1 = id_gen.next();
        let tx2 = id_gen.next();
        let tx3 = id_gen.next();

        assert!(object.write(&tx1, 10).is_ok());
        verify_check_commit_success(&object, &tx1);
        verify_commit_success(&mut object, &tx1, 10);

        assert!(object.write(&tx3, 20).is_ok());
        verify_read(&mut object, &tx2, 10);
    }

    #[test]
    fn test_read_after_non_committed_write() {
        let mut object = TimestampedObject::default('A');
        let mut id_gen = TransactionIdGenerator::new('B');
        let tx1 = id_gen.next();
        let tx2 = id_gen.next();
        let tx3 = id_gen.next();

        assert!(object.write(&tx1, 10).is_ok());
        verify_check_commit_success(&object, &tx1);
        verify_commit_success(&mut object, &tx1, 10);

        assert!(object.write(&tx2, 20).is_ok());

        let read_res = object.read(&tx3);
        assert!(read_res.is_err());
        assert_eq!(read_res.unwrap_err(), RWFailure::WaitFor(tx2));

        verify_check_commit_success(&object, &tx2);
        verify_commit_success(&mut object, &tx2, 20);

        verify_read(&mut object, &tx3, 20);
    }

    #[test]
    fn test_read_before_committed_write() {
        let mut object = TimestampedObject::default('A');
        let mut id_gen = TransactionIdGenerator::new('B');
        let tx1 = id_gen.next();
        let tx2 = id_gen.next();

        assert!(object.write(&tx2, 10).is_ok());
        verify_check_commit_success(&object, &tx2);
        verify_commit_success(&mut object, &tx2, 10);

        let read_res = object.read(&tx1);
        assert!(read_res.is_err());
        assert_eq!(read_res.unwrap_err(), RWFailure::Abort);
    }

    #[test]
    fn test_read_after_write_on_same_tx() {
        let mut object = TimestampedObject::default('A');
        let mut id_gen = TransactionIdGenerator::new('B');
        let tx = id_gen.next();

        assert!(object.write(&tx, 10).is_ok());
        verify_read(&mut object, &tx, 10);
        assert!(object.write(&tx, 50).is_ok());
        verify_read(&mut object, &tx, 50);

        verify_check_commit_success(&object, &tx);
        verify_commit_success(&mut object, &tx, 50);
    }

    #[test]
    fn test_read_after_write_on_same_tx_multiple_tx() {
        let mut object = TimestampedObject::default('A');
        let mut id_gen = TransactionIdGenerator::new('B');
        let tx1 = id_gen.next();
        let tx2 = id_gen.next();

        assert!(object.write(&tx2, 20).is_ok());
        assert!(object.write(&tx1, 10).is_ok());

        verify_read(&mut object, &tx1, 10);
        verify_read(&mut object, &tx2, 20);

        verify_check_commit_success(&object, &tx1);
        verify_commit_success(&mut object, &tx1, 10);

        verify_check_commit_success(&object, &tx2);
        verify_commit_success(&mut object, &tx2, 20);
    }

    #[test]
    fn test_read_after_commit_on_different_tx() {
        let mut object = TimestampedObject::default('A');
        let mut id_gen = TransactionIdGenerator::new('B');
        let tx1 = id_gen.next();
        let tx2 = id_gen.next();

        assert!(object.write(&tx2, 20).is_ok());
        assert!(object.write(&tx1, 10).is_ok());

        verify_read(&mut object, &tx1, 10);
        verify_check_commit_success(&object, &tx1);
        verify_commit_success(&mut object, &tx1, 10);

        verify_read(&mut object, &tx2, 20);
        verify_check_commit_success(&object, &tx2);
        verify_commit_success(&mut object, &tx2, 20);
    }

    #[test]
    fn test_read_created_object() {
        let mut object = TimestampedObject::<i64>::default('A');
        let mut id_gen = TransactionIdGenerator::new('B');
        let tx = id_gen.next();

        let read_res = object.read(&tx);
        assert!(read_res.is_err());
        assert_eq!(read_res.unwrap_err(), RWFailure::AbortedNotFound);
    }

    #[test]
    fn test_read_on_unwritten_object() {
        let mut object = TimestampedObject::<i64>::default('A');
        let mut id_gen = TransactionIdGenerator::new('B');
        let tx1 = id_gen.next();
        let tx2 = id_gen.next();

        assert!(object.write(&tx2, 20).is_ok());

        let read_res = object.read(&tx1);
        assert!(read_res.is_err());
        assert_eq!(read_res.unwrap_err(), RWFailure::AbortedNotFound);
    }
}