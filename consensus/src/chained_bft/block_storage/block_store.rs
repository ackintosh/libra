// Copyright (c) The Libra Core Contributors
// SPDX-License-Identifier: Apache-2.0

use crate::{
    chained_bft::{
        block_storage::{block_tree::BlockTree, BlockReader, VoteReceptionResult},
        persistent_storage::{PersistentStorage, RecoveryData},
    },
    state_replication::StateComputer,
};
use consensus_types::{
    block::{Block, ExecutedBlock},
    common::{Payload, Round},
    quorum_cert::QuorumCert,
    timeout_certificate::TimeoutCertificate,
    vote::Vote,
};
use executor::{ProcessedVMOutput, StateComputeResult};
use failure::ResultExt;
use libra_crypto::HashValue;
use libra_logger::prelude::*;

#[cfg(any(test, feature = "fuzzing"))]
use libra_types::validator_set::ValidatorSet;
use libra_types::{
    crypto_proxies::{ValidatorSigner, ValidatorVerifier},
    ledger_info::LedgerInfo,
};
use mirai_annotations::checked_precondition;
use std::{
    collections::{vec_deque::VecDeque, HashMap},
    sync::{Arc, RwLock},
};

#[cfg(test)]
#[path = "block_store_test.rs"]
mod block_store_test;

#[path = "sync_manager.rs"]
pub mod sync_manager;

/// Responsible for maintaining all the blocks of payload and the dependencies of those blocks
/// (parent and previous QC links).  It is expected to be accessed concurrently by multiple threads
/// and is thread-safe.
///
/// Example tree block structure based on parent links.
///                         ╭--> A3
/// Genesis--> B0--> B1--> B2--> B3
///             ╰--> C1--> C2
///                         ╰--> D3
///
/// Example corresponding tree block structure for the QC links (must follow QC constraints).
///                         ╭--> A3
/// Genesis--> B0--> B1--> B2--> B3
///             ├--> C1
///             ├--------> C2
///             ╰--------------> D3
pub struct BlockStore<T> {
    inner: Arc<RwLock<BlockTree<T>>>,
    validator_signer: ValidatorSigner,
    state_computer: Arc<dyn StateComputer<Payload = T>>,
    enforce_increasing_timestamps: bool,
    /// The persistent storage backing up the in-memory data structure, every write should go
    /// through this before in-memory tree.
    storage: Arc<dyn PersistentStorage<T>>,
}

impl<T: Payload> BlockStore<T> {
    pub async fn new(
        storage: Arc<dyn PersistentStorage<T>>,
        initial_data: RecoveryData<T>,
        validator_signer: ValidatorSigner,
        state_computer: Arc<dyn StateComputer<Payload = T>>,
        enforce_increasing_timestamps: bool,
        max_pruned_blocks_in_mem: usize,
    ) -> Self {
        let highest_tc = initial_data.highest_timeout_certificate();
        let (root, blocks, quorum_certs) = initial_data.take();
        let inner = Arc::new(RwLock::new(
            Self::build_block_tree(
                root,
                blocks,
                quorum_certs,
                highest_tc,
                Arc::clone(&state_computer),
                max_pruned_blocks_in_mem,
            )
            .await,
        ));
        BlockStore {
            inner,
            validator_signer,
            state_computer,
            enforce_increasing_timestamps,
            storage,
        }
    }

    async fn build_block_tree(
        root: (Block<T>, QuorumCert, QuorumCert),
        blocks: Vec<Block<T>>,
        quorum_certs: Vec<QuorumCert>,
        highest_timeout_cert: Option<TimeoutCertificate>,
        state_computer: Arc<dyn StateComputer<Payload = T>>,
        max_pruned_blocks_in_mem: usize,
    ) -> BlockTree<T> {
        let (root_block, root_qc, root_li) = (root.0, root.1, root.2);
        // root_compute_res will not used anywhere so use default value to simplify code.
        let root_compute_res = StateComputeResult::default();
        // TODO: check root matches the committed_trees.
        let root_output = ProcessedVMOutput::new(vec![], state_computer.committed_trees());
        let executed_root_block = ExecutedBlock::new(root_block, root_output, root_compute_res);
        let mut tree = BlockTree::new(
            executed_root_block,
            root_qc,
            root_li,
            max_pruned_blocks_in_mem,
            highest_timeout_cert.map(Arc::new),
        );
        let quorum_certs = quorum_certs
            .into_iter()
            .map(|qc| (qc.certified_block().id(), qc))
            .collect::<HashMap<_, _>>();
        for block in blocks {
            assert!(!block.is_genesis_block());
            let parent_trees = tree
                .get_block(&block.parent_id())
                .expect("Parent block must exist")
                .executed_trees()
                .clone();
            let (output, state_compute_result) = state_computer
                .compute(&block, parent_trees)
                .await
                .expect("fail to rebuild scratchpad");
            // if this block is certified, ensure we agree with the certified state.
            if let Some(qc) = quorum_certs.get(&block.id()) {
                assert_eq!(
                    qc.certified_block().executed_state_id(),
                    state_compute_result.executed_state.state_id,
                    "We have inconsistent executed state with Quorum Cert for block {}",
                    block.id()
                );
            }
            tree.insert_block(ExecutedBlock::new(block, output, state_compute_result))
                .expect("Block insertion failed while build the tree");
        }
        quorum_certs.into_iter().for_each(|(_, qc)| {
            tree.insert_quorum_cert(qc)
                .expect("QuorumCert insertion failed while build the tree")
        });
        tree
    }

    pub async fn rebuild(
        &self,
        root: (Block<T>, QuorumCert, QuorumCert),
        blocks: Vec<Block<T>>,
        quorum_certs: Vec<QuorumCert>,
    ) {
        let max_pruned_blocks_in_mem = self.inner.read().unwrap().max_pruned_blocks_in_mem();
        // Rollover the previous highest TC from the old tree to the new one.
        let prev_htc = self.highest_timeout_cert().map(|tc| tc.as_ref().clone());
        let tree = Self::build_block_tree(
            root,
            blocks,
            quorum_certs,
            prev_htc,
            Arc::clone(&self.state_computer),
            max_pruned_blocks_in_mem,
        )
        .await;
        let to_remove = self.inner.read().unwrap().get_all_block_id();
        if let Err(e) = self.storage.prune_tree(to_remove) {
            // it's fine to fail here, the next restart will try to clean up dangling blocks again.
            error!("fail to delete block: {:?}", e);
        }
        *self.inner.write().unwrap() = tree;
    }

    pub fn signer(&self) -> &ValidatorSigner {
        &self.validator_signer
    }

    /// Execute and insert a block if it passes all validation tests.
    /// Returns the Arc to the block kept in the block store after persisting it to storage
    ///
    /// This function assumes that the ancestors are present (returns MissingParent otherwise).
    ///
    /// Duplicate inserts will return the previously inserted block (
    /// note that it is considered a valid non-error case, for example, it can happen if a validator
    /// receives a certificate for a block that is currently being added).
    pub async fn execute_and_insert_block(
        &self,
        block: Block<T>,
    ) -> failure::Result<Arc<ExecutedBlock<T>>> {
        if let Some(existing_block) = self.get_block(block.id()) {
            return Ok(existing_block);
        }
        let executed_block = self.execute_block(block).await?;
        self.storage
            .save_tree(vec![executed_block.block().clone()], vec![])
            .with_context(|e| format!("Insert block failed with {:?} when saving block", e))?;
        self.inner.write().unwrap().insert_block(executed_block)
    }

    async fn execute_block(&self, block: Block<T>) -> failure::Result<ExecutedBlock<T>> {
        let parent_block = match self.verify_and_get_parent(&block) {
            Ok(t) => t,
            Err(e) => {
                security_log(SecurityEvent::InvalidBlock)
                    .error(&e)
                    .data(&block)
                    .log();
                return Err(e);
            }
        };

        // Reconfiguration rule - if a block is a child of reconfiguration, it needs to be empty
        // So we roll over the executed state until it's committed and we start new epoch.
        let parent_state = parent_block.compute_result();
        let (output, state_compute_result) = if parent_state.has_reconfiguration() {
            ensure!(
                block.payload().filter(|p| **p != T::default()).is_none(),
                "Reconfiguration suffix should not carry payload"
            );
            (
                parent_block.output().as_ref().clone(),
                StateComputeResult {
                    executed_state: parent_state.executed_state.clone(),
                    compute_status: vec![],
                },
            )
        } else {
            let parent_trees = parent_block.executed_trees().clone();
            // Although NIL blocks don't have payload, we still send a T::default() to compute
            // because we may inject a block prologue transaction.
            self.state_computer
                .compute(&block, parent_trees)
                .await
                .with_context(|e| format!("Execution failure for block {}: {:?}", block, e))?
        };
        Ok(ExecutedBlock::new(block, output, state_compute_result))
    }

    /// Validates quorum certificates and inserts it into block tree assuming dependencies exist.
    pub fn insert_single_quorum_cert(&self, qc: QuorumCert) -> failure::Result<()> {
        // If the parent block is not the root block (i.e not None), ensure the executed state
        // of a block is consistent with its QuorumCert, otherwise persist the QuorumCert's
        // state and on restart, a new execution will agree with it.  A new execution will match
        // the QuorumCert's state on the next restart will work if there is a memory
        // corruption, for example.
        match self.get_block(qc.certified_block().id()) {
            Some(executed_block) => {
                // TODO: Once we fill in the root compute result, we should remove this
                if self.root() != executed_block {
                    assert_eq!(
                        executed_block.compute_result().executed_state.state_id,
                        qc.certified_block().executed_state_id(),
                        "QC for block {} has a different execution result than the local computation. Restart and try again.",
                        qc.certified_block().id(),
                    );
                }
            }
            None => bail!("Insert {} without having the block in store first", qc),
        }

        self.storage
            .save_tree(vec![], vec![qc.clone()])
            .with_context(|e| format!("Insert block failed with {:?} when saving quorum", e))?;
        self.inner.write().unwrap().insert_quorum_cert(qc)
    }

    /// Replace the highest timeout certificate in case the given one has a higher round.
    /// In case a timeout certificate is updated, persist it to storage.
    pub fn insert_timeout_certificate(&self, tc: Arc<TimeoutCertificate>) -> failure::Result<()> {
        let cur_tc_round = self.highest_timeout_cert().map_or(0, |tc| tc.round());
        if tc.round() <= cur_tc_round {
            return Ok(());
        }
        self.storage
            .save_highest_timeout_cert(tc.as_ref().clone())
            .with_context(|e| {
                format!(
                    "Timeout certificate insert failed with {:?} when persisting to DB",
                    e
                )
            })?;
        self.inner.write().unwrap().replace_timeout_cert(tc);
        Ok(())
    }

    /// Adds a vote for the block.
    /// The returned value either contains the vote result (with new / old QC etc.) or a
    /// verification error.
    /// A block store does not verify that the block, which is voted for, is present locally.
    /// It returns QC, if it is formed, but does not insert it into block store, because it might
    /// not have required dependencies yet
    /// Different execution ids are treated as different blocks (e.g., if some proposal is
    /// executed in a non-deterministic fashion due to a bug, then the votes for execution result
    /// A and the votes for execution result B are aggregated separately).
    pub fn insert_vote(
        &self,
        vote: &Vote,
        validator_verifier: &ValidatorVerifier,
    ) -> VoteReceptionResult {
        self.inner
            .write()
            .unwrap()
            .insert_vote(vote, validator_verifier)
    }

    /// Prune the tree up to next_root_id (keep next_root_id's block).  Any branches not part of
    /// the next_root_id's tree should be removed as well.
    ///
    /// For example, root = B0
    /// B0--> B1--> B2
    ///        ╰--> B3--> B4
    ///
    /// prune_tree(B3) should be left with
    /// B3--> B4, root = B3
    ///
    /// Returns the block ids of the blocks removed.
    pub fn prune_tree(&self, next_root_id: HashValue) -> VecDeque<HashValue> {
        let id_to_remove = self
            .inner
            .read()
            .unwrap()
            .find_blocks_to_prune(next_root_id);
        if let Err(e) = self
            .storage
            .prune_tree(id_to_remove.clone().into_iter().collect())
        {
            // it's fine to fail here, as long as the commit succeeds, the next restart will clean
            // up dangling blocks, and we need to prune the tree to keep the root consistent with
            // executor.
            error!("fail to delete block: {:?}", e);
        }
        self.inner
            .write()
            .unwrap()
            .process_pruned_blocks(next_root_id, id_to_remove.clone());
        id_to_remove
    }

    /// If block id information is found, returns the ledger info placeholder, otherwise, return
    /// a placeholder with info of the genesis block.
    pub fn ledger_info_placeholder(
        &self,
        potential_commit_id: Option<HashValue>,
        epoch_num: u64,
    ) -> LedgerInfo {
        let block_id = match potential_commit_id {
            Some(id) => id,
            None => return Self::zero_ledger_info_placeholder(epoch_num),
        };
        let block_to_commit = match self.get_block(block_id) {
            Some(b) => b,
            None => {
                return Self::zero_ledger_info_placeholder(epoch_num);
            }
        };
        let compute_result = block_to_commit.compute_result();
        // The block we vote for should be in the same epoch as the block we commit.
        assert_eq!(epoch_num, block_to_commit.epoch());
        LedgerInfo::new(
            compute_result.executed_state.version,
            compute_result.executed_state.state_id,
            HashValue::zero(),
            block_id,
            block_to_commit.epoch(),
            block_to_commit.timestamp_usecs(),
            compute_result.executed_state.validators.clone(),
        )
    }

    /// Used in case we're using a ledger info just as a placeholder for signing the votes / QCs
    /// and there is no real block committed.
    /// It's all pretty much zeroes.
    fn zero_ledger_info_placeholder(epoch_num: u64) -> LedgerInfo {
        LedgerInfo::new(
            0,
            HashValue::zero(),
            HashValue::zero(),
            HashValue::zero(),
            epoch_num,
            0,
            None,
        )
    }

    fn verify_and_get_parent(&self, block: &Block<T>) -> failure::Result<Arc<ExecutedBlock<T>>> {
        ensure!(
            self.inner.read().unwrap().root().round() < block.round(),
            "Block with old round"
        );

        let parent = self
            .get_block(block.parent_id())
            .ok_or_else(|| format_err!("Block with missing parent {}", block.parent_id()))?;
        ensure!(parent.round() < block.round(), "Block with invalid round");
        ensure!(
            !self.enforce_increasing_timestamps
                || block.timestamp_usecs() > parent.timestamp_usecs(),
            "Block with non-increasing timestamp"
        );

        Ok(parent)
    }
}

impl<T: Payload> BlockReader for BlockStore<T> {
    type Payload = T;

    fn block_exists(&self, block_id: HashValue) -> bool {
        self.inner.read().unwrap().block_exists(&block_id)
    }

    fn get_block(&self, block_id: HashValue) -> Option<Arc<ExecutedBlock<T>>> {
        self.inner.read().unwrap().get_block(&block_id)
    }

    fn root(&self) -> Arc<ExecutedBlock<T>> {
        self.inner.read().unwrap().root()
    }

    fn get_quorum_cert_for_block(&self, block_id: HashValue) -> Option<Arc<QuorumCert>> {
        self.inner
            .read()
            .unwrap()
            .get_quorum_cert_for_block(&block_id)
    }

    fn path_from_root(&self, block_id: HashValue) -> Option<Vec<Arc<ExecutedBlock<T>>>> {
        self.inner.read().unwrap().path_from_root(block_id)
    }

    fn create_block(
        &self,
        parent: &Block<Self::Payload>,
        payload: Self::Payload,
        round: Round,
        timestamp_usecs: u64,
    ) -> Block<Self::Payload> {
        if self.enforce_increasing_timestamps {
            checked_precondition!(parent.timestamp_usecs() < timestamp_usecs);
        }
        let quorum_cert = self
            .get_quorum_cert_for_block(parent.id())
            .expect("Parent for the newly created block is not certified!")
            .as_ref()
            .clone();
        Block::make_block(
            parent,
            payload,
            round,
            timestamp_usecs,
            quorum_cert,
            &self.validator_signer,
        )
    }

    fn highest_certified_block(&self) -> Arc<ExecutedBlock<Self::Payload>> {
        self.inner.read().unwrap().highest_certified_block()
    }

    fn highest_quorum_cert(&self) -> Arc<QuorumCert> {
        self.inner.read().unwrap().highest_quorum_cert()
    }

    fn highest_ledger_info(&self) -> Arc<QuorumCert> {
        self.inner.read().unwrap().highest_ledger_info()
    }

    fn highest_timeout_cert(&self) -> Option<Arc<TimeoutCertificate>> {
        self.inner.read().unwrap().highest_timeout_cert()
    }
}

#[cfg(any(test, feature = "fuzzing"))]
impl<T: Payload> BlockStore<T> {
    /// Returns the number of blocks in the tree
    pub(crate) fn len(&self) -> usize {
        self.inner.read().unwrap().len()
    }

    /// Returns the number of child links in the tree
    pub(crate) fn child_links(&self) -> usize {
        self.inner.read().unwrap().child_links()
    }

    /// The number of pruned blocks that are still available in memory
    pub(super) fn pruned_blocks_in_mem(&self) -> usize {
        self.inner.read().unwrap().pruned_blocks_in_mem()
    }

    /// Helper to insert vote and qc
    /// Can't be used in production, because production insertion potentially requires state sync
    pub fn insert_vote_and_qc(
        &self,
        vote: &Vote,
        validator_verifier: &ValidatorVerifier,
    ) -> VoteReceptionResult {
        let r = self.insert_vote(vote, validator_verifier);
        if let VoteReceptionResult::NewQuorumCertificate(ref qc) = r {
            self.insert_single_quorum_cert(qc.as_ref().clone()).unwrap();
        }
        r
    }

    /// Helper function to insert the block with the qc together
    pub async fn insert_block_with_qc(
        &self,
        block: Block<T>,
    ) -> failure::Result<Arc<ExecutedBlock<T>>> {
        self.insert_single_quorum_cert(block.quorum_cert().clone())?;
        Ok(self.execute_and_insert_block(block).await?)
    }

    /// Helper function to insert a reconfiguration block
    pub async fn insert_reconfiguration_block(
        &self,
        block: Block<T>,
    ) -> failure::Result<Arc<ExecutedBlock<T>>> {
        self.insert_single_quorum_cert(block.quorum_cert().clone())?;
        let executed_block = self.execute_block(block).await?;
        let output = executed_block.output().as_ref().clone();
        let mut compute_result = executed_block.compute_result().as_ref().clone();
        compute_result.executed_state.validators = Some(ValidatorSet::new(vec![]));
        Ok(self
            .inner
            .write()
            .unwrap()
            .insert_block(ExecutedBlock::new(
                executed_block.block().clone(),
                output,
                compute_result,
            ))?)
    }
}
