// Copyright (c) The Libra Core Contributors
// SPDX-License-Identifier: Apache-2.0

use crate::{
    block::{Block, BlockType},
    block_info::BlockInfo,
    common::Round,
    quorum_cert::QuorumCert,
    vote_data::VoteData,
};
use libra_crypto::hash::{CryptoHash, HashValue};
use libra_types::{
    crypto_proxies::{SecretKey, ValidatorSigner},
    ledger_info::{LedgerInfo, LedgerInfoWithSignatures},
    validator_signer::proptests,
};
use proptest::prelude::*;
use std::collections::BTreeMap;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

type LinearizedBlockForest<T> = Vec<Block<T>>;

prop_compose! {
    /// This strategy is a swiss-army tool to produce a low-level block
    /// dependent on signer, round, parent and ancestor_id.
    /// Note that the quorum certificate carried by this block is still placeholder: one will have
    /// to generate it later on when adding to the tree.
    pub fn make_block(
        _ancestor_id: HashValue,
        round_strategy: impl Strategy<Value = Round>,
        signer_strategy: impl Strategy<Value = ValidatorSigner>,
        parent_qc: QuorumCert,
    )(
        round in round_strategy,
        payload in 0usize..10usize,
        signer in signer_strategy,
        parent_qc in Just(parent_qc)
    ) -> Block<Vec<usize>> {
        Block::new_internal(
            vec![payload],
            0,
            round,
            get_current_timestamp().as_micros() as u64,
            parent_qc,
            &signer,
        )
    }
}

/// This produces the genesis block
pub fn genesis_strategy() -> impl Strategy<Value = Block<Vec<usize>>> {
    Just(Block::make_genesis_block())
}

prop_compose! {
    /// This produces an unmoored block, with arbitrary parent & QC ancestor
    pub fn unmoored_block(ancestor_id_strategy: impl Strategy<Value = HashValue>)(
        ancestor_id in ancestor_id_strategy,
    )(
        block in make_block(
            ancestor_id,
            Round::arbitrary(),
            proptests::arb_signer(),
            QuorumCert::certificate_for_genesis(),
        )
    ) -> Block<Vec<usize>> {
        block
    }
}

/// Offers the genesis block.
pub fn leaf_strategy() -> impl Strategy<Value = Block<Vec<usize>>> {
    genesis_strategy().boxed()
}

prop_compose! {
    /// This produces a block with an invalid id (and therefore signature)
    /// given a valid block
    pub fn fake_id(block_strategy: impl Strategy<Value = Block<Vec<usize>>>)
        (fake_id in HashValue::arbitrary(),
         block in block_strategy) -> Block<Vec<usize>> {
            Block {
                timestamp_usecs: get_current_timestamp().as_micros() as u64,
                id: fake_id,
                epoch: block.epoch(),
                round: block.round(),
                quorum_cert: block.quorum_cert().clone(),
                block_type: BlockType::Proposal {
                    payload: block.payload().unwrap().clone(),
                    author: block.author().unwrap(),
                    signature: block.signature().unwrap().clone(),
                },
            }
        }
}

prop_compose! {
    fn bigger_round(initial_round: Round)(
        increment in 2..8,
        initial_round in Just(initial_round),
    ) -> Round {
        initial_round + increment as u64
    }
}

/// This produces a round that is often higher than the parent, but not
/// too high
pub fn some_round(initial_round: Round) -> impl Strategy<Value = Round> {
    prop_oneof![
        9 => Just(1 + initial_round),
        1 => bigger_round(initial_round),
    ]
}

prop_compose! {
    /// This creates a child with a parent on its left, and a QC on the left
    /// of the parent. This, depending on branching, does not require the
    /// QC to always be an ancestor or the parent to always be the highest QC
    fn child(
        signer_strategy: impl Strategy<Value = ValidatorSigner>,
        block_forest_strategy: impl Strategy<Value = LinearizedBlockForest<Vec<usize>>>,
    )(
        signer in signer_strategy,
        (forest_vec, parent_idx, qc_idx) in block_forest_strategy
            .prop_flat_map(|forest_vec| {
                let len = forest_vec.len();
                (Just(forest_vec), 0..len)
            })
            .prop_flat_map(|(forest_vec, parent_idx)| {
                (Just(forest_vec), Just(parent_idx), 0..=parent_idx)
            }),
    )( block in make_block(
        // ancestor_id
        forest_vec[qc_idx].id(),
        // round
        some_round(forest_vec[parent_idx].round()),
        // signer
        Just(signer),
        // parent_qc
        forest_vec[qc_idx].quorum_cert().clone(),
    ), mut forest in Just(forest_vec),
    ) -> LinearizedBlockForest<Vec<usize>> {
        forest.push(block);
        forest
    }
}

/// This creates a block forest with keys extracted from a specific
/// vector
fn block_forest_from_keys(
    depth: u32,
    keypairs: Vec<SecretKey>,
) -> impl Strategy<Value = LinearizedBlockForest<Vec<usize>>> {
    let leaf = leaf_strategy().prop_map(|block| vec![block]);
    // Note that having `expected_branch_size` of 1 seems to generate significantly larger trees
    // than desired (this is my understanding after reading the documentation:
    // https://docs.rs/proptest/0.3.0/proptest/strategy/trait.Strategy.html#method.prop_recursive)
    leaf.prop_recursive(depth, depth, 2, move |inner| {
        child(proptests::mostly_in_keypair_pool(keypairs.clone()), inner)
    })
}

/// This returns keys and a block forest created from them
pub fn block_forest_and_its_keys(
    quorum_size: usize,
    depth: u32,
) -> impl Strategy<Value = (Vec<SecretKey>, LinearizedBlockForest<Vec<usize>>)> {
    proptest::collection::vec(proptests::arb_signing_key(), quorum_size).prop_flat_map(
        move |private_key| {
            (
                Just(private_key.clone()),
                block_forest_from_keys(depth, private_key),
            )
        },
    )
}

// Using current_timestamp in this test
// because it's a bit hard to generate incremental timestamps in proptests
pub fn get_current_timestamp() -> Duration {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .expect("Timestamp generated is before the UNIX_EPOCH!")
}

pub fn placeholder_ledger_info() -> LedgerInfo {
    LedgerInfo::new(
        0,
        HashValue::zero(),
        HashValue::zero(),
        HashValue::zero(),
        0,
        0,
        None,
    )
}

pub fn placeholder_certificate_for_block(
    signers: Vec<&ValidatorSigner>,
    certified_block_id: HashValue,
    certified_block_round: u64,
    certified_parent_block_id: HashValue,
    certified_parent_block_round: u64,
    consensus_block_id: Option<HashValue>,
) -> QuorumCert {
    // Assuming executed state to be Genesis state.
    let genesis_ledger_info = LedgerInfo::genesis();
    let vote_data = VoteData::new(
        BlockInfo::new(
            genesis_ledger_info.epoch(),
            certified_block_round,
            certified_block_id,
            genesis_ledger_info.transaction_accumulator_hash(),
            genesis_ledger_info.version(),
            genesis_ledger_info.timestamp_usecs(),
        ),
        BlockInfo::new(
            genesis_ledger_info.epoch(),
            certified_parent_block_round,
            certified_parent_block_id,
            genesis_ledger_info.transaction_accumulator_hash(),
            genesis_ledger_info.version(),
            genesis_ledger_info.timestamp_usecs(),
        ),
    );

    // This ledger info doesn't carry any meaningful information: it is all zeros except for
    // the consensus data hash that carries the actual vote.
    let mut ledger_info_placeholder = placeholder_ledger_info();
    ledger_info_placeholder.set_consensus_data_hash(vote_data.hash());

    if let Some(bid) = consensus_block_id {
        ledger_info_placeholder.set_consensus_block_id(bid)
    }

    let mut signatures = BTreeMap::new();
    for signer in signers {
        let li_sig = signer
            .sign_message(ledger_info_placeholder.hash())
            .expect("Failed to sign LedgerInfo");
        signatures.insert(signer.author(), li_sig);
    }

    QuorumCert::new(
        vote_data,
        LedgerInfoWithSignatures::new(ledger_info_placeholder, signatures),
    )
}
