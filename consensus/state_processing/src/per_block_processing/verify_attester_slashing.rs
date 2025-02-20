use super::errors::{AttesterSlashingInvalid as Invalid, BlockOperationError};
use super::is_valid_indexed_attestation::is_valid_indexed_attestation;
use crate::per_block_processing::VerifySignatures;
use std::collections::BTreeSet;
use types::*;

type Result<T> = std::result::Result<T, BlockOperationError<Invalid>>;

fn error(reason: Invalid) -> BlockOperationError<Invalid> {
    BlockOperationError::invalid(reason)
}

/// Indicates if an `AttesterSlashing` is valid to be included in a block in the current epoch of
/// the given state.
///
/// Returns `Ok(indices)` with `indices` being a non-empty vec of validator indices in ascending
/// order if the `AttesterSlashing` is valid. Otherwise returns `Err(e)` with the reason for
/// invalidity.
pub fn verify_attester_slashing<E: EthSpec>(
    state: &BeaconState<E>,
    attester_slashing: AttesterSlashingRef<'_, E>,
    verify_signatures: VerifySignatures,
    spec: &ChainSpec,
) -> Result<Vec<u64>> {
    let attestation_1 = attester_slashing.attestation_1();
    let attestation_2 = attester_slashing.attestation_2();

    // Spec: is_slashable_attestation_data
    verify!(
        attestation_1.is_double_vote(attestation_2)
            || attestation_1.is_surround_vote(attestation_2),
        Invalid::NotSlashable
    );

    is_valid_indexed_attestation(state, attestation_1, verify_signatures, spec)
        .map_err(|e| error(Invalid::IndexedAttestation1Invalid(e)))?;
    is_valid_indexed_attestation(state, attestation_2, verify_signatures, spec)
        .map_err(|e| error(Invalid::IndexedAttestation2Invalid(e)))?;

    get_slashable_indices(state, attester_slashing)
}

/// For a given attester slashing, return the indices able to be slashed in ascending order.
///
/// Returns Ok(indices) if `indices.len() > 0`
pub fn get_slashable_indices<E: EthSpec>(
    state: &BeaconState<E>,
    attester_slashing: AttesterSlashingRef<'_, E>,
) -> Result<Vec<u64>> {
    get_slashable_indices_modular(state, attester_slashing, |_, validator| {
        validator.is_slashable_at(state.current_epoch())
    })
}

/// Same as `gather_attester_slashing_indices` but allows the caller to specify the criteria
/// for determining whether a given validator should be considered slashable.
pub fn get_slashable_indices_modular<F, E: EthSpec>(
    state: &BeaconState<E>,
    attester_slashing: AttesterSlashingRef<'_, E>,
    is_slashable: F,
) -> Result<Vec<u64>>
where
    F: Fn(u64, &Validator) -> bool,
{
    let attestation_1 = attester_slashing.attestation_1();
    let attestation_2 = attester_slashing.attestation_2();

    let attesting_indices_1 = attestation_1
        .attesting_indices_iter()
        .cloned()
        .collect::<BTreeSet<_>>();

    let attesting_indices_2 = attestation_2
        .attesting_indices_iter()
        .cloned()
        .collect::<BTreeSet<_>>();

    let mut slashable_indices = vec![];

    for index in &attesting_indices_1 & &attesting_indices_2 {
        let validator = state
            .validators()
            .get(index as usize)
            .ok_or_else(|| error(Invalid::UnknownValidator(index)))?;

        if is_slashable(index, validator) {
            slashable_indices.push(index);
        }
    }

    verify!(!slashable_indices.is_empty(), Invalid::NoSlashableIndices);

    Ok(slashable_indices)
}
