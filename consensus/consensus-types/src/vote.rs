// Copyright (c) The Libra Core Contributors
// SPDX-License-Identifier: Apache-2.0

use crate::{
    common::{self, Author},
    vote_data::VoteData,
};
use failure::{ensure, ResultExt};
use libra_crypto::hash::CryptoHash;
use libra_types::{
    crypto_proxies::{Signature, ValidatorSigner, ValidatorVerifier},
    ledger_info::LedgerInfo,
};
use serde::{Deserialize, Serialize};
use std::fmt::{Display, Formatter};

/// Vote is the struct that is ultimately sent by the voter in response for
/// receiving a proposal.
/// Vote carries the `LedgerInfo` of a block that is going to be committed in case this vote
/// is gathers QuorumCertificate (see the detailed explanation in the comments of `LedgerInfo`).
#[derive(Deserialize, Serialize, Clone, Debug, PartialEq, Eq)]
pub struct Vote {
    /// The data of the vote
    vote_data: VoteData,
    /// The identity of the voter.
    author: Author,
    /// LedgerInfo of a block that is going to be committed in case this vote gathers QC.
    ledger_info: LedgerInfo,
    /// Signature of the LedgerInfo
    signature: Signature,
    /// The round signatures can be aggregated into a timeout certificate if present.
    timeout_signature: Option<Signature>,
}

impl Display for Vote {
    fn fmt(&self, f: &mut Formatter) -> std::fmt::Result {
        write!(
            f,
            "Vote: [vote data: {}, author: {}, is_timeout: {}, {}]",
            self.vote_data,
            self.author.short_str(),
            self.is_timeout(),
            self.ledger_info
        )
    }
}

impl Vote {
    /// Generates a new Vote corresponding to the "fast-vote" path without the round signatures
    /// that can be aggregated into a timeout certificate
    pub fn new(
        vote_data: VoteData,
        author: Author,
        mut ledger_info_placeholder: LedgerInfo,
        validator_signer: &ValidatorSigner,
    ) -> Self {
        ledger_info_placeholder.set_consensus_data_hash(vote_data.hash());
        let li_sig = validator_signer
            .sign_message(ledger_info_placeholder.hash())
            .expect("Failed to sign LedgerInfo");
        Self {
            vote_data,
            author,
            ledger_info: ledger_info_placeholder,
            signature: li_sig.into(),
            timeout_signature: None,
        }
    }

    /// Generates a round signature, which can then be used for aggregating a timeout certificate.
    /// Typically called for generating vote messages that are sent upon timeouts.
    pub fn add_timeout_signature(&mut self, validator_signer: &ValidatorSigner) {
        if self.timeout_signature.is_some() {
            return; // round signature is already set
        }
        self.timeout_signature.replace(
            validator_signer
                .sign_message(common::timeout_hash(
                    self.vote_data().proposed().round(),
                    self.vote_data().proposed().epoch(),
                ))
                .expect("Failed to sign round")
                .into(),
        );
    }

    pub fn vote_data(&self) -> &VoteData {
        &self.vote_data
    }

    /// Return the author of the vote
    pub fn author(&self) -> Author {
        self.author
    }

    /// Return the LedgerInfo associated with this vote
    pub fn ledger_info(&self) -> &LedgerInfo {
        &self.ledger_info
    }

    /// Return the signature of the vote
    pub fn signature(&self) -> &Signature {
        &self.signature
    }

    /// Returns the signature for the vote_data().proposed().round() that can be aggregated for
    /// TimeoutCertificate.
    pub fn timeout_signature(&self) -> Option<&Signature> {
        self.timeout_signature.as_ref()
    }

    /// The vote message is considered a timeout vote message if it carries a signature on the
    /// round, which can then be used for aggregating it to the TimeoutCertificate.
    pub fn is_timeout(&self) -> bool {
        self.timeout_signature.is_some()
    }

    /// Verifies that the consensus data hash of LedgerInfo corresponds to the vote info,
    /// and then verifies the signature.
    pub fn verify(&self, validator: &ValidatorVerifier) -> failure::Result<()> {
        ensure!(
            self.ledger_info.consensus_data_hash() == self.vote_data.hash(),
            "Vote's hash mismatch with LedgerInfo"
        );
        self.signature()
            .verify(validator, self.author(), self.ledger_info.hash())
            .with_context(|e| format!("Fail to verify Vote: {:?}", e))?;
        if let Some(timeout_signature) = &self.timeout_signature {
            timeout_signature
                .verify(
                    validator,
                    self.author(),
                    common::timeout_hash(
                        self.vote_data().proposed().round(),
                        self.vote_data.proposed().epoch(),
                    ),
                )
                .with_context(|e| format!("Fail to verify Vote: {:?}", e))?;
        }
        Ok(())
    }
}
