use crate::metrics;

use beacon_chain::blob_verification::BlockWrapper;
use beacon_chain::validator_monitor::{get_block_delay_ms, timestamp_now};
use beacon_chain::{
    AvailabilityProcessingStatus, BeaconChain, BeaconChainError, BeaconChainTypes, BlockError,IntoGossipVerifiedBlock,
    GossipVerifiedBlock, NotifyExecutionLayer,
};
use eth2::types::BroadcastValidation;
use eth2::types::SignedBlockContents;
use execution_layer::ProvenancedPayload;
use lighthouse_network::PubsubMessage;
use network::NetworkMessage;
use slog::{debug, error, info, warn, Logger};
use slot_clock::SlotClock;
use std::marker::PhantomData;
use std::sync::Arc;
use std::time::Duration;
use tokio::sync::mpsc::UnboundedSender;
use tree_hash::TreeHash;
use types::{
    AbstractExecPayload, BeaconBlockRef, BlindedPayload, EthSpec, ExecPayload, ExecutionBlockHash,
    FullPayload, Hash256, SignedBeaconBlock, SignedBlobSidecarList, VariableList,
};
use warp::Rejection;

pub enum ProvenancedBlock<T: BeaconChainTypes, B: IntoGossipVerifiedBlock<T>> {
    /// The payload was built using a local EE.
    Local(B, PhantomData<T>),
    /// The payload was build using a remote builder (e.g., via a mev-boost
    /// compatible relay).
    Builder(B, PhantomData<T>),
}

impl<T: BeaconChainTypes, B: IntoGossipVerifiedBlock<T>> ProvenancedBlock<T, B> {
    pub fn local(block: B) -> Self {
        Self::Local(block, PhantomData)
    }

    pub fn builder(block: B) -> Self {
        Self::Builder(block, PhantomData)
    }
}

/// Handles a request from the HTTP API for full blocks.
pub async fn publish_block<T: BeaconChainTypes, B: IntoGossipVerifiedBlock<T>>(
    block_root: Option<Hash256>,
    provenanced_block: ProvenancedBlock<T, B>,
    chain: Arc<BeaconChain<T>>,
    network_tx: &UnboundedSender<NetworkMessage<T::EthSpec>>,
    log: Logger,
    validation_level: BroadcastValidation,
) -> Result<(), Rejection> {
    let seen_timestamp = timestamp_now();

    let (block, blobs_opt, is_locally_built_block) = match provenanced_block {
        ProvenancedBlock::Local(block_contents, _) => {
            let (block, maybe_blobs, ) = block_contents.deconstruct();
            (Arc::new(block), maybe_blobs, true)
        }
        ProvenancedBlock::Builder(block_contents, _) => {
            let (block, maybe_blobs) = block_contents.deconstruct();
            (Arc::new(block), maybe_blobs, false)
        }
    };
    let delay = get_block_delay_ms(seen_timestamp, block.message(), &chain.slot_clock);
    debug!(log, "Signed block received in HTTP API"; "slot" => block.slot());

    /* actually publish a block */
    let publish_block = move |block: Arc<SignedBeaconBlock<T::EthSpec>>,
                              blobs_opt: Option<SignedBlobSidecarList<T::EthSpec>>,
                              sender,
                              log,
                              seen_timestamp| {
        let publish_timestamp = timestamp_now();
        let publish_delay = publish_timestamp
            .checked_sub(seen_timestamp)
            .unwrap_or_else(|| Duration::from_secs(0));

        info!(log, "Signed block published to network via HTTP API"; "slot" => block.slot(), "publish_delay" => ?publish_delay);
        // Send the block, regardless of whether or not it is valid. The API
        // specification is very clear that this is the desired behaviour.
        match block.as_ref() {
            SignedBeaconBlock::Base(_)
            | SignedBeaconBlock::Altair(_)
            | SignedBeaconBlock::Merge(_)
            | SignedBeaconBlock::Capella(_) => {
                crate::publish_pubsub_message(&sender, PubsubMessage::BeaconBlock(block.clone()))
                    .map_err(|_| BlockError::BeaconChainError(BeaconChainError::UnableToPublish))?;
            }
            SignedBeaconBlock::Deneb(_) => {
                crate::publish_pubsub_message(&sender, PubsubMessage::BeaconBlock(block.clone()))
                    .map_err(|_| BlockError::BeaconChainError(BeaconChainError::UnableToPublish))?;
                if let Some(signed_blobs) = blobs_opt {
                    for (blob_index, blob) in signed_blobs.into_iter().enumerate() {
                        crate::publish_pubsub_message(
                            &sender,
                            PubsubMessage::BlobSidecar(Box::new((blob_index as u64, blob))),
                        )
                        .map_err(|_| {
                            BlockError::BeaconChainError(BeaconChainError::UnableToPublish)
                        })?;
                    }
                }
            }
        };
        Ok(())
    };

    let mapped_blobs = blobs_opt.clone().map(|blobs| {
        VariableList::from(
            blobs
                .into_iter()
                .map(|blob| blob.message)
                .collect::<Vec<_>>(),
        )
    });

    /* only publish if gossip- and consensus-valid and equivocation-free */
    let chain_clone = chain.clone();
    let slot = block.message().slot();
    let block_clone = block.clone();
    let proposer_index = block.message().proposer_index();
    let sender_clone = network_tx.clone();
    let log_clone = log.clone();

    /* if we can form a `GossipVerifiedBlock`, we've passed our basic gossip checks */
    let gossip_verified_block =
        BlockWrapper::new(block.clone(), mapped_blobs).into_gossip_verified_block(
        &chain,
    )
    .map_err(|e| {
        warn!(log, "Not publishing block, not gossip verified"; "slot" => slot, "error" => ?e);
        warp_utils::reject::custom_bad_request(e.to_string())
    })?;

    let block_root = block_root.unwrap_or(gossip_verified_block.block_root);

    if let BroadcastValidation::Gossip = validation_level {
        publish_block(
            block.clone(),
            blobs_opt.clone(),
            sender_clone.clone(),
            log.clone(),
            seen_timestamp,
        )
        .map_err(|_| warp_utils::reject::custom_server_error("unable to publish".into()))?;
    }

    let publish_fn = move || match validation_level {
        BroadcastValidation::Gossip => Ok(()),
        BroadcastValidation::Consensus => publish_block(
            block_clone,
            blobs_opt,
            sender_clone,
            log_clone,
            seen_timestamp,
        ),
        BroadcastValidation::ConsensusAndEquivocation => {
            if chain_clone
                .observed_block_producers
                .read()
                .proposer_has_been_observed(block_clone.message(), block_root)
                .map_err(|e| BlockError::BeaconChainError(e.into()))?
                .is_slashable()
            {
                warn!(
                    log_clone,
                    "Not publishing equivocating block";
                    "slot" => block_clone.slot()
                );
                Err(BlockError::Slashable)
            } else {
                publish_block(
                    block_clone,
                    blobs_opt,
                    sender_clone,
                    log_clone,
                    seen_timestamp,
                )
            }
        }
    };

    match chain
        .process_block(
            block_root,
            gossip_verified_block,
            NotifyExecutionLayer::Yes,
            publish_fn,
        )
        .await
    {
        Ok(AvailabilityProcessingStatus::Imported(root)) => {
            info!(
                log,
                "Valid block from HTTP API";
                "block_delay" => ?delay,
                "root" => format!("{}", root),
                "proposer_index" => proposer_index,
                "slot" =>slot,
            );

            // Notify the validator monitor.
            chain.validator_monitor.read().register_api_block(
                seen_timestamp,
                block.message(),
                root,
                &chain.slot_clock,
            );

            // Update the head since it's likely this block will become the new
            // head.
            chain.recompute_head_at_current_slot().await;

            // Only perform late-block logging here if the block is local. For
            // blocks built with builders we consider the broadcast time to be
            // when the blinded block is published to the builder.
            if is_locally_built_block {
                late_block_logging(&chain, seen_timestamp, block.message(), root, "local", &log)
            }

            Ok(())
        }
        Ok(AvailabilityProcessingStatus::MissingComponents(_, block_root)) => {
            let msg = format!("Missing parts of block with root {:?}", block_root);
            error!(
                log,
                "Invalid block provided to HTTP API";
                "reason" => &msg
            );
            Err(warp_utils::reject::broadcast_without_import(msg))
        }
        Err(BlockError::BeaconChainError(BeaconChainError::UnableToPublish)) => {
            Err(warp_utils::reject::custom_server_error(
                "unable to publish to network channel".to_string(),
            ))
        }
        Err(BlockError::Slashable) => Err(warp_utils::reject::custom_bad_request(
            "proposal for this slot and proposer has already been seen".to_string(),
        )),
        Err(BlockError::BlockIsAlreadyKnown) => {
            info!(log, "Block from HTTP API already known"; "block" => ?block_root);
            Ok(())
        }
        Err(e) => {
            if let BroadcastValidation::Gossip = validation_level {
                Err(warp_utils::reject::broadcast_without_import(format!("{e}")))
            } else {
                let msg = format!("{:?}", e);
                error!(
                    log,
                    "Invalid block provided to HTTP API";
                    "reason" => &msg
                );
                Err(warp_utils::reject::custom_bad_request(format!(
                    "Invalid block: {e}"
                )))
            }
        }
    }
}

/// Handles a request from the HTTP API for blinded blocks. This converts blinded blocks into full
/// blocks before publishing.
pub async fn publish_blinded_block<T: BeaconChainTypes>(
    block: SignedBlockContents<T::EthSpec, BlindedPayload<T::EthSpec>>,
    chain: Arc<BeaconChain<T>>,
    network_tx: &UnboundedSender<NetworkMessage<T::EthSpec>>,
    log: Logger,
    validation_level: BroadcastValidation,
) -> Result<(), Rejection> {
    let block_root = block.signed_block().canonical_root();
    let full_block: ProvenancedBlock<T, SignedBlockContents<T::EthSpec>> = reconstruct_block(chain.clone(), block_root, block, log.clone()).await?;
    publish_block::<T, _>(
        Some(block_root),
        full_block,
        chain,
        network_tx,
        log,
        validation_level,
    )
    .await
}

/// Deconstruct the given blinded block, and construct a full block. This attempts to use the
/// execution layer's payload cache, and if that misses, attempts a blind block proposal to retrieve
/// the full payload.
pub async fn reconstruct_block<T: BeaconChainTypes>(
    chain: Arc<BeaconChain<T>>,
    block_root: Hash256,
    block: SignedBlockContents<T::EthSpec, BlindedPayload<T::EthSpec>>,
    log: Logger,
) -> Result<ProvenancedBlock<T::EthSpec, SignedBlockContents<T::EthSpec>>, Rejection> {
    let full_payload_opt = if let Ok(payload_header) =
        block.signed_block().message().body().execution_payload()
    {
        let el = chain.execution_layer.as_ref().ok_or_else(|| {
            warp_utils::reject::custom_server_error("Missing execution layer".to_string())
        })?;

        // If the execution block hash is zero, use an empty payload.
        let full_payload = if payload_header.block_hash() == ExecutionBlockHash::zero() {
            let payload = FullPayload::default_at_fork(
                chain.spec.fork_name_at_epoch(
                    block
                        .signed_block()
                        .slot()
                        .epoch(T::EthSpec::slots_per_epoch()),
                ),
            )
            .map_err(|e| {
                warp_utils::reject::custom_server_error(format!(
                    "Default payload construction error: {e:?}"
                ))
            })?
            .into();
            ProvenancedPayload::Local(payload)
        // If we already have an execution payload with this transactions root cached, use it.
        } else if let Some(cached_payload) =
            el.get_payload_by_root(&payload_header.tree_hash_root())
        {
            info!(log, "Reconstructing a full block using a local payload"; "block_hash" => ?cached_payload.block_hash());
            ProvenancedPayload::Local(cached_payload)
        // Otherwise, this means we are attempting a blind block proposal.
        } else {
            // Perform the logging for late blocks when we publish to the
            // builder, rather than when we publish to the network. This helps
            // prevent false positive logs when the builder publishes to the P2P
            // network significantly earlier than when they return the block to
            // us.
            late_block_logging(
                &chain,
                timestamp_now(),
                block.signed_block().message(),
                block_root,
                "builder",
                &log,
            );

            let full_payload = el
                .propose_blinded_beacon_block(block_root, &block)
                .await
                .map_err(|e| {
                    warp_utils::reject::custom_server_error(format!(
                        "Blind block proposal failed: {:?}",
                        e
                    ))
                })?;
            info!(log, "Successfully published a block to the builder network"; "block_hash" => ?full_payload.block_hash());
            ProvenancedPayload::Builder(full_payload)
        };

        Some(full_payload)
    } else {
        None
    };

    match full_payload_opt {
        // A block without a payload is pre-merge and we consider it locally
        // built.
        None => block
            .deconstruct()
            .0
            .try_into_full_block(None)
            .map(SignedBlockContents::Block)
            .map(ProvenancedBlock::local),
        Some(ProvenancedPayload::Local(full_payload)) => block
            .deconstruct()
            .0
            .try_into_full_block(Some(full_payload))
            .map(SignedBlockContents::Block)
            .map(ProvenancedBlock::local),
        Some(ProvenancedPayload::Builder(full_payload)) => block
            .deconstruct()
            .0
            .try_into_full_block(Some(full_payload))
            .map(SignedBlockContents::Block)
            .map(ProvenancedBlock::builder),
    }
    .ok_or_else(|| {
        warp_utils::reject::custom_server_error("Unable to add payload to block".to_string())
    })
}

/// If the `seen_timestamp` is some time after the start of the slot for
/// `block`, create some logs to indicate that the block was published late.
fn late_block_logging<T: BeaconChainTypes, P: AbstractExecPayload<T::EthSpec>>(
    chain: &BeaconChain<T>,
    seen_timestamp: Duration,
    block: BeaconBlockRef<T::EthSpec, P>,
    root: Hash256,
    provenance: &str,
    log: &Logger,
) {
    let delay = get_block_delay_ms(seen_timestamp, block, &chain.slot_clock);

    metrics::observe_timer_vec(
        &metrics::HTTP_API_BLOCK_BROADCAST_DELAY_TIMES,
        &[provenance],
        delay,
    );

    // Perform some logging to inform users if their blocks are being produced
    // late.
    //
    // Check to see the thresholds are non-zero to avoid logging errors with small
    // slot times (e.g., during testing)
    let too_late_threshold = chain.slot_clock.unagg_attestation_production_delay();
    let delayed_threshold = too_late_threshold / 2;
    if delay >= too_late_threshold {
        error!(
            log,
            "Block was broadcast too late";
            "msg" => "system may be overloaded, block likely to be orphaned",
            "provenance" => provenance,
            "delay_ms" => delay.as_millis(),
            "slot" => block.slot(),
            "root" => ?root,
        )
    } else if delay >= delayed_threshold {
        error!(
            log,
            "Block broadcast was delayed";
            "msg" => "system may be overloaded, block may be orphaned",
            "provenance" => provenance,
            "delay_ms" => delay.as_millis(),
            "slot" => block.slot(),
            "root" => ?root,
        )
    }
}
