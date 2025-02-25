// Copyright (c) The Libra Core Contributors
// SPDX-License-Identifier: Apache-2.0

//! Protocol to discover network addresses of other peers on the Libra network
//!
//! ## Implementation
//!
//! The discovery module is implemented as a stand-alone actor in the Network sub-system of the
//! Libra stack. The actor participates in discovery by periodically sending its observed state of
//! the network to a randomly chosen peer. Other peers are also expected to be running the same
//! protocol. Therefore, in expectation, every peer expects to hear from 1 other peer in each
//! round. On hearing from the remote peer, the local discovery module tries to reconcile its state
//! to reflect any changes. In addition to updating its state, it also passes on new information to
//! the [`ConnectivityManager`] module.
//!
//! For the initial bootstrap of a node, it sends the discovery message to a randomly chosen seed
//! peer in each round. The message only contains the identity of this peer unless it learns more
//! about the network membership from another peer.
//!
//! Currently we do not use this mechanism to detect peer failures - instead, we simply connect to
//! all the peers in the network, and hope to learn about their failure on connection errors.
//!
//! TODO: We need to handle to case of peers who may no longer be a part of the network.
//!
//! ## Future work
//!
//! - Currently, we do not try to detect/punish nodes which are just lurking (without contributing
//! to the protocol), or actively trying to spread misinformation in the network. In the future, we
//! plan to remedy this by introducing a module dedicated to detecting byzantine behavior, and by
//! making the discovery protocol itself tolerant to byzantine faults.
//! - As an optimization, instead of creating a new substream to the chosen peer in each round, we
//! could maintain a cache of open substreams which could be re-used across numerous rounds.
//!
//! [`ConnectivityManager`]: ../../connectivity_manager
use crate::{
    common::NegotiatedSubstream,
    connectivity_manager::ConnectivityRequest,
    error::{NetworkError, NetworkErrorKind},
    peer_manager::{PeerManagerNotification, PeerManagerRequestSender},
    proto::{DiscoveryMsg, FullNodePayload, Note, PeerInfo, SignedFullNodePayload, SignedPeerInfo},
    utils::{self, MessageExt},
    NetworkPublicKeys, ProtocolId,
};
use channel;
use failure::{format_err, Fail};
use futures::{
    future::{Future, FutureExt, TryFutureExt},
    io::{AsyncRead, AsyncWrite},
    sink::SinkExt,
    stream::{FusedStream, FuturesUnordered, Stream, StreamExt},
};
use libra_crypto::{
    ed25519::*,
    hash::{CryptoHasher, DiscoveryMsgHasher},
    HashValue,
};
use libra_logger::prelude::*;
use libra_types::{
    crypto_proxies::{ValidatorSigner as Signer, ValidatorVerifier as SignatureValidator},
    validator_verifier::ValidatorInfo as SignatureInfo,
    PeerId,
};
use netcore::compat::IoCompat;
use parity_multiaddr::Multiaddr;
use prost::Message;
use rand::{rngs::SmallRng, FromEntropy, Rng};
use std::{
    collections::HashMap,
    convert::TryFrom,
    fmt::Debug,
    pin::Pin,
    sync::{Arc, RwLock},
    time::{Duration, SystemTime},
};
use tokio::{
    codec::{Framed, LengthDelimitedCodec},
    future::FutureExt as _,
};

#[cfg(test)]
mod test;

pub const DISCOVERY_PROTOCOL_NAME: &[u8] = b"/libra/discovery/0.1.0";

/// The actor running the discovery protocol.
pub struct Discovery<TTicker, TSubstream> {
    /// Note for self.
    self_note: Note,
    /// Validator for verifying signatures on messages.
    trusted_peers: Arc<RwLock<HashMap<PeerId, NetworkPublicKeys>>>,
    /// Current state, maintaining the most recent Note for each peer, alongside parsed PeerInfo.
    known_peers: HashMap<PeerId, (PeerInfo, Note)>,
    /// Info for seed peers.
    seed_peers: HashMap<PeerId, PeerInfo>,
    /// Currently connected peers.
    connected_peers: HashMap<PeerId, Multiaddr>,
    /// Ticker to trigger state send to a random peer. In production, the ticker is likely to be
    /// fixed duration interval timer.
    ticker: TTicker,
    /// Channel to send requests to PeerManager.
    peer_mgr_reqs_tx: PeerManagerRequestSender<TSubstream>,
    /// Channel to receive notifications from PeerManager.
    peer_mgr_notifs_rx: channel::Receiver<PeerManagerNotification<TSubstream>>,
    /// Channel to send requests to ConnectivityManager.
    conn_mgr_reqs_tx: channel::Sender<ConnectivityRequest>,
    /// Message timeout duration.
    msg_timeout: Duration,
    /// Random-number generator.
    rng: SmallRng,
}

impl<TTicker, TSubstream> Discovery<TTicker, TSubstream>
where
    TTicker: Stream + FusedStream + Unpin,
    TSubstream: AsyncRead + AsyncWrite + Send + Unpin + Debug + 'static,
{
    pub fn new(
        self_peer_id: PeerId,
        self_addrs: Vec<Multiaddr>,
        signer: Signer,
        seed_peers: HashMap<PeerId, PeerInfo>,
        trusted_peers: Arc<RwLock<HashMap<PeerId, NetworkPublicKeys>>>,
        ticker: TTicker,
        peer_mgr_reqs_tx: PeerManagerRequestSender<TSubstream>,
        peer_mgr_notifs_rx: channel::Receiver<PeerManagerNotification<TSubstream>>,
        conn_mgr_reqs_tx: channel::Sender<ConnectivityRequest>,
        msg_timeout: Duration,
    ) -> Self {
        // TODO(philiphayes): wire through config
        let dns_seed_addr = b"example.com";

        let self_peer_info = create_peer_info(self_addrs);
        let self_full_node_payload = create_full_node_payload(dns_seed_addr);
        let self_note = create_note(
            &signer,
            self_peer_id,
            self_peer_info.clone(),
            self_full_node_payload.clone(),
        );

        let known_peers = vec![(self_peer_id, (self_peer_info, self_note.clone()))]
            .into_iter()
            .collect();
        Self {
            self_note,
            seed_peers,
            trusted_peers,
            known_peers,
            connected_peers: HashMap::new(),
            ticker,
            peer_mgr_reqs_tx,
            peer_mgr_notifs_rx,
            conn_mgr_reqs_tx,
            msg_timeout,
            rng: SmallRng::from_entropy(),
        }
    }

    // Connect with all the seed peers. If current node is also a seed peer, remove it from the
    // list.
    async fn connect_to_seed_peers(&mut self) {
        debug!("Connecting to seed peers");
        let self_peer_id =
            PeerId::try_from(self.self_note.peer_id.clone()).expect("PeerId parsing failed");
        for (peer_id, peer_info) in self
            .seed_peers
            .iter()
            .filter(|(peer_id, _)| **peer_id != self_peer_id)
        {
            self.conn_mgr_reqs_tx
                .send(ConnectivityRequest::UpdateAddresses(
                    *peer_id,
                    peer_info
                        .addrs
                        .iter()
                        .cloned()
                        .map(|addr| Multiaddr::try_from(addr).expect("Multiaddr parsing failed"))
                        .collect(),
                ))
                .await
                .expect("ConnectivityRequest::UpdateAddresses send");
        }
    }

    // Starts the main event loop for the discovery actor. We bootstrap by first dialing all the
    // seed peers, and then entering the event handling loop. Messages are received from:
    // - a ticker to trigger discovery message send to a random connected peer
    // - an incoming substream from a peer wishing to send its state
    // - an internal task once it has processed incoming messages from a peer, and wishes for
    // discovery actor to update its state.
    pub async fn start(mut self) {
        // Bootstrap by connecting to seed peers.
        self.connect_to_seed_peers().await;
        let mut unprocessed_inbound = FuturesUnordered::new();
        let mut unprocessed_outbound = FuturesUnordered::new();
        loop {
            futures::select! {
                _ = self.ticker.select_next_some() => {
                    self.handle_tick(&mut unprocessed_outbound);
                }
                notif = self.peer_mgr_notifs_rx.select_next_some() => {
                    self.handle_peer_mgr_notification(notif, &mut unprocessed_inbound);
                },
                (peer_id, stream_result) = unprocessed_inbound.select_next_some() => {
                    match stream_result {
                        Ok(remote_notes) => {
                            self.reconcile(peer_id, remote_notes).await;
                        }
                        Err(e) => {
                            warn!("Failure in processing stream from peer: {}. Error: {:?}",
                                  peer_id.short_str(), e);
                        }
                    }
                },
                _ = unprocessed_outbound.select_next_some() => {}
                complete => {
                    crit!("Discovery actor terminated");
                    break;
                }
            }
        }
    }

    // Handles a clock "tick" by:
    // 1. Selecting a random peer to send state to.
    // 2. Compose the msg to send.
    // 3. Spawn off a new task to push the msg to the peer.
    fn handle_tick<'a>(
        &'a mut self,
        unprocessed_outbound: &'a mut FuturesUnordered<Pin<Box<dyn Future<Output = ()> + Send>>>,
    ) {
        // On each tick, we choose a random neighbor and push our state to it.
        if let Some(peer) = self.choose_random_neighbor() {
            // We clone `peer_mgr_reqs_tx` member of Self, since using `self` inside fut below
            // triggers some lifetime errors.
            let sender = self.peer_mgr_reqs_tx.clone();
            // Compose discovery msg to send.
            let msg = self.compose_discovery_msg();
            let timeout = self.msg_timeout;
            let fut = async move {
                if let Err(err) = push_state_to_peer(sender, peer, msg).timeout(timeout).await {
                    warn!(
                        "Failed to send discovery msg to {}; error: {:?}",
                        peer.short_str(),
                        err
                    );
                }
            };
            unprocessed_outbound.push(fut.boxed());
        }
    }

    fn handle_peer_mgr_notification<'a>(
        &'a mut self,
        notif: PeerManagerNotification<TSubstream>,
        unprocessed_inbound: &'a mut FuturesUnordered<
            Pin<Box<dyn Future<Output = (PeerId, Result<Vec<Note>, NetworkError>)> + Send>>,
        >,
    ) {
        trace!("PeerManagerNotification::{:?}", notif);
        match notif {
            PeerManagerNotification::NewPeer(peer_id, addr) => {
                // Add peer to connected peer list.
                self.connected_peers.insert(peer_id, addr);
            }
            PeerManagerNotification::LostPeer(peer_id, addr) => {
                match self.connected_peers.get(&peer_id) {
                    Some(curr_addr) if *curr_addr == addr => {
                        // Remove node from connected peers list.
                        self.connected_peers.remove(&peer_id);
                    }
                    _ => {
                        debug!(
                            "Received redundant lost peer notification for {}",
                            peer_id.short_str()
                        );
                    }
                }
            }
            PeerManagerNotification::NewInboundSubstream(peer_id, substream) => {
                // We should not receive substreams from peer manager for any other protocol.
                assert_eq!(substream.protocol, DISCOVERY_PROTOCOL_NAME);
                // Add future to handle new inbound substream.
                unprocessed_inbound.push(
                    handle_inbound_substream(
                        self.trusted_peers.clone(),
                        peer_id,
                        substream,
                        self.msg_timeout,
                    )
                    .boxed(),
                );
            }
        }
    }

    // Chooses a random connected neighbour.
    fn choose_random_neighbor(&mut self) -> Option<PeerId> {
        if !self.connected_peers.is_empty() {
            let peers: Vec<_> = self.connected_peers.keys().collect();
            let idx = self.rng.gen_range(0, peers.len());
            Some(*peers[idx])
        } else {
            None
        }
    }

    // Creates DiscoveryMsg to be sent to some remote peer.
    fn compose_discovery_msg(&self) -> DiscoveryMsg {
        let mut msg = DiscoveryMsg::default();
        for (_, note) in self.known_peers.values() {
            msg.notes.push(note.clone());
        }
        msg
    }

    // Updates local state by reconciling with notes received from some remote peer.
    // Assumption: `remote_notes` have already been verified for signature validity and content.
    async fn reconcile(&mut self, remote_peer: PeerId, remote_notes: Vec<Note>) {
        // If a peer is previously unknown, or has a newer epoch number, we update its
        // corresponding entry in the map.
        let self_peer_id =
            PeerId::try_from(self.self_note.peer_id.clone()).expect("PeerId parsing fails");
        for note in remote_notes {
            let peer_id = PeerId::try_from(note.peer_id.clone()).expect("PeerId parsing fails");
            let peer_info_bytes = &note.signed_peer_info.as_ref().unwrap().peer_info;
            let peer_info = PeerInfo::decode(peer_info_bytes).expect("PeerInfo parsing fails");

            match self.known_peers.get_mut(&peer_id) {
                // If we know about this peer, and receive the same or an older epoch, we do
                // nothing.
                Some((ref curr_peer_info, _)) if peer_info.epoch <= curr_peer_info.epoch => {
                    if peer_info.epoch < curr_peer_info.epoch {
                        debug!(
                            "Received stale note for peer: {} from peer: {}",
                            peer_id.short_str(),
                            remote_peer
                        );
                    }
                    continue;
                }
                _ => {
                    info!(
                        "Received updated note for peer: {} from peer: {}",
                        peer_id.short_str(),
                        remote_peer.short_str()
                    );
                    // We can never receive a note with a higher epoch number on us than what we
                    // ourselves have broadcasted.
                    assert_ne!(peer_id, self_peer_id);
                    // Update internal state of the peer with new Note.
                    self.known_peers.insert(peer_id, (peer_info.clone(), note));

                    // The multiaddrs in the peer's discovery Note.
                    let mut peer_addrs: Vec<Multiaddr> = peer_info
                        .addrs
                        .iter()
                        .cloned()
                        .map(|addr| Multiaddr::try_from(addr).expect("Multiaddr parsing fails"))
                        .collect();

                    // Append the addrs in the seed PeerInfo if this peer is
                    // configured as one of our seed peers.
                    if let Some(seed_info) = self.seed_peers.get(&peer_id) {
                        let seed_addrs_iter = seed_info.addrs.iter().cloned().map(|addr| {
                            Multiaddr::try_from(addr).expect("Multiaddr parsing fails")
                        });
                        peer_addrs.extend(seed_addrs_iter);
                    }

                    self.conn_mgr_reqs_tx
                        .send(ConnectivityRequest::UpdateAddresses(peer_id, peer_addrs))
                        .await
                        .expect("ConnectivityRequest::UpdateAddresses send");
                }
            }
        }
    }
}

// Creates a PeerInfo combining the given addresses with the current unix timestamp as epoch.
fn create_peer_info(addrs: Vec<Multiaddr>) -> PeerInfo {
    let mut peer_info = PeerInfo::default();
    // TODO: Currently, SystemTime::now() in Rust is not guaranteed to use a monotonic clock.
    // At the moment, it's unclear how to do this in a platform-agnostic way. For Linux, we
    // could use something like the [timerfd trait](https://docs.rs/crate/timerfd/1.0.0).
    let time_since_epoch = SystemTime::now()
        .duration_since(SystemTime::UNIX_EPOCH)
        .expect("System clock reset to before unix epoch")
        .as_millis() as u64;
    peer_info.epoch = time_since_epoch;
    peer_info.addrs = addrs.into_iter().map(|addr| addr.as_ref().into()).collect();
    peer_info
}

fn create_full_node_payload(dns_seed_addr: &[u8]) -> FullNodePayload {
    let mut full_node_payload = FullNodePayload::default();
    // TODO: Currently, SystemTime::now() in Rust is not guaranteed to use a monotonic clock.
    // At the moment, it's unclear how to do this in a platform-agnostic way. For Linux, we
    // could use something like the [timerfd trait](https://docs.rs/crate/timerfd/1.0.0).
    let time_since_epoch = SystemTime::now()
        .duration_since(SystemTime::UNIX_EPOCH)
        .expect("System clock reset to before unix epoch")
        .as_millis() as u64;
    full_node_payload.epoch = time_since_epoch;
    full_node_payload.dns_seed_addr = dns_seed_addr.into();
    full_node_payload
}

// Creates a note by signing the given peer info, and combining the signature, peer_info and
// peer_id into a note.
fn create_note(
    signer: &Signer,
    peer_id: PeerId,
    peer_info: PeerInfo,
    full_node_payload: FullNodePayload,
) -> Note {
    let peer_info_bytes = peer_info.to_bytes().expect("Protobuf serialization fails");
    let peer_info_signature = sign(&signer, &peer_info_bytes);

    let mut signed_peer_info = SignedPeerInfo::default();
    signed_peer_info.peer_info = peer_info_bytes.to_vec();
    signed_peer_info.signature = peer_info_signature;

    let payload_bytes = full_node_payload
        .to_bytes()
        .expect("Protobuf serialization fails");
    let payload_signature = sign(&signer, &payload_bytes);

    let mut signed_full_node_payload = SignedFullNodePayload::default();
    signed_full_node_payload.payload = payload_bytes.to_vec();
    signed_full_node_payload.signature = payload_signature;

    let mut note = Note::default();
    note.peer_id = peer_id.into();
    note.signed_peer_info = Some(signed_peer_info);
    note.signed_full_node_payload = Some(signed_full_node_payload);
    note
}

// Handles an inbound substream from a remote peer as follows:
// 1. Reads the DiscoveryMsg sent by the remote.
// 2. Verifies signatures on all notes contained in the message.
async fn handle_inbound_substream<TSubstream>(
    trusted_peers: Arc<RwLock<HashMap<PeerId, NetworkPublicKeys>>>,
    peer_id: PeerId,
    substream: NegotiatedSubstream<TSubstream>,
    timeout: Duration,
) -> (PeerId, Result<Vec<Note>, NetworkError>)
where
    TSubstream: AsyncRead + AsyncWrite + Send + Unpin + 'static,
{
    // Read the `DiscoveryMsg` from the remote
    let res_msg = recv_msg(substream.substream)
        .timeout(timeout)
        .map_err(Into::<NetworkError>::into)
        .map(|r| r.and_then(|x| x))
        .await;

    // Check that all received `Note`s are valid -- reject the whole message
    // if any `Note` is invalid.
    let res_notes = res_msg.and_then(|msg| {
        msg.notes.iter().try_for_each(|note| {
            is_valid(&note, &trusted_peers).map_err(|err| {
                security_log(SecurityEvent::InvalidDiscoveryMsg)
                    .error(&err)
                    .data(&peer_id)
                    .data(&note)
                    .data(&trusted_peers)
                    .log();
                err
            })
        })?;
        Ok(msg.notes)
    });

    (peer_id, res_notes)
}

// Verifies validity of notes. Following conditions should be met for validity:
// 1. We should be able to correctly parse the peer id in each note.
// 2. The signature of the serialized peer info should be valid for the given peer_id.
// 3. The address(es) in the PeerInfo should be correctly parsable as Multiaddrs.
// 4. The signature of the serialized full node payload should be valid for the given peer_id.
fn is_valid(
    note: &Note,
    trusted_peers: &RwLock<HashMap<PeerId, NetworkPublicKeys>>,
) -> Result<(), NetworkError> {
    // validate PeerId

    let peer_id = PeerId::try_from(note.peer_id.clone())
        .map_err(|err| err.context(NetworkErrorKind::ParsingError))?;

    // validate PeerInfo

    let signed_peer_info = note.signed_peer_info.as_ref().ok_or_else(|| {
        format_err!("Discovery Note missing signed_peer_info field")
            .context(NetworkErrorKind::ParsingError)
    })?;

    let peer_info_bytes = &signed_peer_info.peer_info;
    let peer_info_signature = &signed_peer_info.signature;
    verify_signature(
        trusted_peers,
        peer_id,
        &peer_info_signature,
        &peer_info_bytes,
    )?;

    let peer_info = PeerInfo::decode(peer_info_bytes)?;
    for addr in peer_info.addrs {
        let _: Multiaddr = Multiaddr::try_from(addr.clone())?;
    }

    // validate FullNodePayload (optional)
    // TODO(philiphayes): actually use the FullNodePayload

    if let Some(signed_full_node_payload) = &note.signed_full_node_payload {
        verify_signature(
            trusted_peers,
            peer_id,
            &signed_full_node_payload.signature,
            &signed_full_node_payload.payload,
        )?;

        let _ = FullNodePayload::decode(&signed_full_node_payload.payload)?;

        // TODO(philiphayes): validate internal fields
    }

    Ok(())
}

fn get_hash(msg: &[u8]) -> HashValue {
    let mut hasher = DiscoveryMsgHasher::default();
    hasher.write(msg);
    hasher.finish()
}

fn verify_signature(
    trusted_peers: &RwLock<HashMap<PeerId, NetworkPublicKeys>>,
    signer: PeerId,
    signature: &[u8],
    msg: &[u8],
) -> Result<(), NetworkError> {
    let verifier = SignatureValidator::new_with_quorum_voting_power(
        trusted_peers
            .read()
            .unwrap()
            .iter()
            .map(|(peer_id, network_public_keys)| {
                (
                    *peer_id,
                    SignatureInfo::new(network_public_keys.signing_public_key.clone(), 1),
                )
            })
            .collect(),
        1, /* quorum size */
    )
    .expect("Quorum size should be valid.");
    let signature = Ed25519Signature::try_from(signature)
        .map_err(|err| err.context(NetworkErrorKind::SignatureError))?;
    verifier.verify_signature(signer, get_hash(msg), &signature)?;
    Ok(())
}

fn sign(signer: &Signer, msg: &[u8]) -> Vec<u8> {
    let signature: Ed25519Signature = signer
        .sign_message(get_hash(msg))
        .expect("Message signing fails");
    signature.to_bytes().to_vec()
}

async fn push_state_to_peer<TSubstream>(
    mut sender: PeerManagerRequestSender<TSubstream>,
    peer_id: PeerId,
    msg: DiscoveryMsg,
) -> Result<(), NetworkError>
where
    TSubstream: AsyncRead + AsyncWrite + Send + Unpin + 'static,
{
    trace!(
        "Push discovery message to peer {} msg: {:?}",
        peer_id.short_str(),
        msg
    );
    // Request a new substream to peer.
    let substream = sender
        .open_substream(peer_id, ProtocolId::from_static(DISCOVERY_PROTOCOL_NAME))
        .await?;
    // Messages are length-prefixed. Wrap in a framed stream.
    let mut substream = Framed::new(IoCompat::new(substream), LengthDelimitedCodec::new());
    // Send serialized message to peer.
    let bytes = msg
        .to_bytes()
        .expect("writing protobuf failed; should never happen");
    substream.send(bytes).await?;
    Ok(())
}

async fn recv_msg<TSubstream>(substream: TSubstream) -> Result<DiscoveryMsg, NetworkError>
where
    TSubstream: AsyncRead + AsyncWrite + Unpin,
{
    // Messages are length-prefixed. Wrap in a framed stream.
    let mut substream = Framed::new(IoCompat::new(substream), LengthDelimitedCodec::new());
    // Read the message.
    utils::read_proto(&mut substream).await
}
