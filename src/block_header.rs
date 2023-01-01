use super::{
    util::{bytes_be_to_u128, encode_h256_to_field, EthConfigParams as EthBlockHeaderConfigParams},
    Field, Network,
};
#[cfg(feature = "display")]
use ark_std::{end_timer, start_timer};
use core::{
    iter::{self, once},
    marker::PhantomData,
};
use ethers_core::types::H256;
#[cfg(feature = "providers")]
use ethers_providers::{Http, Provider};
use halo2_base::{
    gates::{flex_gate::FlexGateConfig, range::RangeConfig, GateInstructions, RangeInstructions},
    halo2_proofs::{
        circuit::{Layouter, SimpleFloorPlanner, Value},
        plonk::{Circuit, Column, ConstraintSystem, Error, Instance},
    },
    utils::{bit_length, PrimeField, ScalarField},
    AssignedValue, Context, ContextParams,
    QuantumCell::{Constant, Existing},
    SKIP_FIRST_PASS,
};
use halo2_mpt::{
    keccak::{zkevm::keccak_packed_multi::KeccakPackedConfig, KeccakChip},
    rlp::{
        rlc::{RlcFixedTrace, RlcTrace, RLC_PHASE},
        RlpArrayTraceWitness, RlpChip, RlpConfig, RlpFieldTrace,
    },
};
use itertools::Itertools;
use serde::{Deserialize, Serialize};
use std::env::set_var;

#[cfg(feature = "aggregation")]
pub mod aggregation;
#[cfg(all(feature = "aggregation", feature = "providers"))]
pub mod helpers;
#[cfg(test)]
mod tests;

const MAINNET_EXTRA_DATA_MAX_BYTES: usize = 32;
const MAINNET_EXTRA_DATA_RLP_MAX_BYTES: usize = MAINNET_EXTRA_DATA_MAX_BYTES + 1;
pub const MAINNET_BLOCK_HEADER_RLP_MAX_BYTES: usize =
    1 + 2 + 520 + MAINNET_EXTRA_DATA_RLP_MAX_BYTES;
const GOERLI_EXTRA_DATA_MAX_BYTES: usize = 97;
const GOERLI_EXTRA_DATA_RLP_MAX_BYTES: usize = GOERLI_EXTRA_DATA_MAX_BYTES + 1;
pub const GOERLI_BLOCK_HEADER_RLP_MAX_BYTES: usize = 1 + 2 + 520 + GOERLI_EXTRA_DATA_RLP_MAX_BYTES;
const BLOCK_HEADER_RLP_MIN_BYTES: usize = 479;

const NUM_BLOCK_HEADER_FIELDS: usize = 16;
const BLOCK_NUMBER_MAX_BYTES: usize = 4;
const MAINNET_HEADER_FIELDS_MAX_BYTES: [usize; NUM_BLOCK_HEADER_FIELDS] =
    [32, 32, 20, 32, 32, 32, 256, 8, 4, 5, 5, 5, MAINNET_EXTRA_DATA_MAX_BYTES, 32, 8, 6];
const GOERLI_HEADER_FIELDS_MAX_BYTES: [usize; NUM_BLOCK_HEADER_FIELDS] =
    [32, 32, 20, 32, 32, 32, 256, 8, 4, 5, 5, 5, GOERLI_EXTRA_DATA_MAX_BYTES, 32, 8, 6];

// Field        Type        Size (bytes) RLP size (bytes) RLP size (bits)
// parentHash	256 bits	32	33	264
// ommersHash	256 bits	32	33	264
// beneficiary	160 bits	20	21	168
// stateRoot	256 bits	32	33	264
// transactionsRoot	256 bits	32	33	264
// receiptsRoot	256 bits	32	33	264
// logsBloom	256 bytes	256	259	2072
// difficulty	big int scalar	variable	8	64
// number	big int scalar	variable	<= 4	<= 32
// gasLimit	big int scalar	variable	5	40
// gasUsed	big int scalar	variable	<= 5	<= 40
// timestamp	big int scalar	variable	5	40
// extraData	up to 256 bits	variable, <= 32	<= 33	<= 264
// mixHash	256 bits	32	33	264
// nonce	64 bits	8	9	72
// basefee (post-1559)	big int scalar	variable	<= 6	<= 48
#[allow(dead_code)]
#[derive(Clone, Debug)]
pub struct EthBlockHeaderTrace<'v, F: Field> {
    // pub rlp_trace: RlcTrace<F>,
    pub parent_hash: RlpFieldTrace<'v, F>,
    pub ommers_hash: RlpFieldTrace<'v, F>,
    pub beneficiary: RlpFieldTrace<'v, F>,
    pub state_root: RlpFieldTrace<'v, F>,
    pub transactions_root: RlpFieldTrace<'v, F>,
    pub receipts_root: RlpFieldTrace<'v, F>,

    pub logs_bloom: RlpFieldTrace<'v, F>,
    pub difficulty: RlpFieldTrace<'v, F>,
    pub number: RlpFieldTrace<'v, F>,
    pub gas_limit: RlpFieldTrace<'v, F>,
    pub gas_used: RlpFieldTrace<'v, F>,
    pub timestamp: RlpFieldTrace<'v, F>,
    pub extra_data: RlpFieldTrace<'v, F>,
    pub mix_hash: RlpFieldTrace<'v, F>,
    pub nonce: RlpFieldTrace<'v, F>,
    pub basefee: RlpFieldTrace<'v, F>,

    pub block_hash: RlcFixedTrace<'v, F>,

    // pub prefix: AssignedValue<'v, F>,
    pub len_trace: RlcTrace<'v, F>,
    pub field_prefix: Vec<AssignedValue<'v, F>>,
}
#[derive(Clone, Debug)]
pub struct EthBlockHeaderTraceWitness<'v, F: Field> {
    pub rlp_witness: RlpArrayTraceWitness<'v, F>,
    pub block_hash_bytes: Vec<AssignedValue<'v, F>>,
}

#[derive(Clone, Debug)]
pub struct EthBlockHeaderConfig<F: Field> {
    pub rlp: RlpConfig<F>,
    pub keccak: KeccakPackedConfig<F>,
    // TODO: describe instances
    pub instance: Column<Instance>,
}

impl<F: Field> EthBlockHeaderConfig<F> {
    pub fn configure(
        meta: &mut ConstraintSystem<F>,
        params: EthBlockHeaderConfigParams,
        context_id: usize,
    ) -> Self {
        let mut rlp = RlpConfig::configure(
            meta,
            params.num_rlc_columns,
            &params.num_range_advice,
            &[params.num_lookup_advice],
            params.num_fixed,
            8, // set this to 8 for now since it coincides with bytes
            context_id,
            params.degree as usize,
        );
        set_var("KECCAK_DEGREE", params.degree.to_string());
        set_var("KECCAK_ROWS", params.keccak_rows_per_round.to_string());
        set_var("UNUSABLE_ROWS", params.unusable_rows.to_string());
        let keccak = KeccakPackedConfig::configure(meta);
        #[cfg(feature = "display")]
        println!("Unusable rows: {}", meta.minimum_rows());

        let instance = meta.instance_column();
        meta.enable_equality(instance);
        rlp.range.gate.max_rows = (1 << params.degree) - meta.minimum_rows();
        Self { rlp, keccak, instance }
    }
}

#[derive(Clone, Debug)]
pub struct EthBlockHeaderChip<'g, F: Field> {
    pub rlp: RlpChip<'g, F>,
    pub keccak: KeccakChip<F>,
    pub instance: Column<Instance>,
}

impl<'g, F: Field> EthBlockHeaderChip<'g, F> {
    pub fn new(config: EthBlockHeaderConfig<F>, gamma: Value<F>) -> Self {
        let rlp = RlpChip::new(config.rlp, gamma);
        let keccak = KeccakChip::new(config.keccak);
        Self { rlp, keccak, instance: config.instance }
    }

    pub fn range(&self) -> &RangeConfig<F> {
        &self.rlp.range
    }

    pub fn gate(&self) -> &FlexGateConfig<F> {
        self.rlp.range.gate()
    }

    pub fn get_challenge(&mut self, ctx: &mut Context<'_, F>) {
        self.rlp.get_challenge(ctx);
    }
    /// Takes the variable length RLP encoded block header, padded with 0s to the maximum possible block header RLP length, and outputs the decomposition into block header fields.
    ///
    /// In addition, the keccak block hash of the block is calculated.
    ///
    /// Assumes `block_header` and `block_header_assigned` have the same values as bytes. The former is only used for faster witness generation.
    ///
    /// This is the preparation step that computes the witnesses. This MUST be done in `FirstPhase`.
    /// The accompanying `decompose_block_header_finalize` must be called in `SecondPhase` to constrain the RLCs associated to the RLP decoding.
    pub fn decompose_block_header_prepare<'v>(
        &mut self,
        ctx: &mut Context<'v, F>,
        block_header: &[u8],
        network: Network,
    ) -> EthBlockHeaderTraceWitness<'v, F> {
        debug_assert_eq!(ctx.current_phase(), 0);
        let (max_len, max_field_lens) = match network {
            Network::Mainnet => {
                (MAINNET_BLOCK_HEADER_RLP_MAX_BYTES, &MAINNET_HEADER_FIELDS_MAX_BYTES)
            }
            Network::Goerli => (GOERLI_BLOCK_HEADER_RLP_MAX_BYTES, &GOERLI_HEADER_FIELDS_MAX_BYTES),
        };
        assert_eq!(block_header.len(), max_len);
        let block_header_assigned = self.gate().assign_witnesses(
            ctx,
            block_header.iter().map(|byte| Value::known(F::from(*byte as u64))),
        );
        let rlp_witness =
            self.rlp.decompose_rlp_array_prepare(ctx, block_header_assigned, max_field_lens, false);

        let block_hash_bytes = self.keccak.keccak_var_len(
            ctx,
            &self.rlp.range,
            block_header.to_vec(),
            &rlp_witness.rlp_array, // this is `block_header_assigned`
            &rlp_witness.rlp_len,
            BLOCK_HEADER_RLP_MIN_BYTES,
        );
        EthBlockHeaderTraceWitness { rlp_witness, block_hash_bytes }
    }

    /// Takes the variable length RLP encoded block header, padded with 0s to the maximum possible block header RLP length, and outputs the decomposition into block header fields.
    ///
    /// In addition, the keccak block hash of the block is calculated.
    ///
    /// Assumes `block_header` and `block_header_assigned` have the same values as bytes. The former is only used for faster witness generation.
    ///
    /// This is the finalization step that constrains RLC concatenations.
    /// This should be called after `decompose_block_header_prepare`.
    /// This MUST be done in `SecondPhase`.
    pub fn decompose_block_header_finalize<'v>(
        &mut self,
        ctx: &mut Context<'v, F>,
        witness: EthBlockHeaderTraceWitness<'v, F>,
    ) -> EthBlockHeaderTrace<'v, F> {
        debug_assert_eq!(ctx.current_phase(), RLC_PHASE);
        let trace = self.rlp.decompose_rlp_array_finalize(ctx, witness.rlp_witness, false);
        let block_hash =
            self.rlp.rlc.compute_rlc_fixed_len(ctx, self.rlp.gate(), witness.block_hash_bytes);

        let [parent_hash, ommers_hash, beneficiary, state_root, transactions_root, receipts_root, logs_bloom, difficulty, number, gas_limit, gas_used, timestamp, extra_data, mix_hash, nonce, basefee]: [RlpFieldTrace<F>; 16] =
            trace.field_trace.try_into().unwrap();

        EthBlockHeaderTrace {
            parent_hash,
            ommers_hash,
            beneficiary,
            state_root,
            transactions_root,
            receipts_root,
            logs_bloom,
            difficulty,
            number,
            gas_limit,
            gas_used,
            timestamp,
            extra_data,
            mix_hash,
            nonce,
            basefee,
            block_hash,
            len_trace: trace.len_trace,
            field_prefix: trace.field_prefix,
        }
    }

    /// Takes a list of (purported) RLP encoded block headers and
    /// decomposes each header into it's fields.
    /// `headers[0]` is the earliest block.
    ///
    /// - If `num_blocks` is not None, then the circuit checks that the first `num_blocks` block headers form a chain: meaning that the parent hash of block i + 1 equals the hash of block i.
    /// - Otherwise if `num_blocks` is None, the circuit checks that all `headers` form a hash chain.
    ///
    /// This is the preparation step that computes the witnesses. This MUST be done in `FirstPhase`.
    /// The accompanying `decompose_block_header_chain_finalize` must be called in `SecondPhase` to constrain the RLCs associated to the RLP decoding.
    pub fn decompose_block_header_chain_prepare<'v>(
        &mut self,
        ctx: &mut Context<'v, F>,
        headers: &[Vec<u8>],
        network: Network,
    ) -> Vec<EthBlockHeaderTraceWitness<'v, F>> {
        debug_assert_eq!(ctx.current_phase(), 0);
        headers
            .iter()
            .map(|header| self.decompose_block_header_prepare(ctx, header, network))
            .collect()
    }

    /// Takes a list of `2^max_depth` (purported) RLP encoded block headers.
    /// Decomposes each header into it's fields.
    /// `headers[0]` is the earliest block
    ///
    /// If `num_blocks_minus_one` is not None, then the circuit checks that the first `num_blocks := num_blocks_minus_one + 1` block headers form a chain: meaning that the parent hash of block i + 1 equals the hash of block i.
    ///
    /// Otherwise if `num_blocks` is None, the circuit checks that all `headers` form a hash chain.
    ///
    /// Assumes that `0 <= num_blocks_minus_one < 2^max_depth`.
    ///
    /// This is the finalization step that constrains RLC concatenations. In this step the hash chain is actually constrained.
    /// This should be called after `decompose_block_header_chain_prepare`.
    /// This MUST be done in `SecondPhase`.
    pub fn decompose_block_header_chain_finalize<'v>(
        &mut self,
        ctx: &mut Context<'v, F>,
        witnesses: Vec<EthBlockHeaderTraceWitness<'v, F>>,
        num_blocks_minus_one: Option<&AssignedValue<'v, F>>,
    ) -> Vec<EthBlockHeaderTrace<'v, F>> {
        debug_assert_eq!(ctx.current_phase(), RLC_PHASE);
        let traces = witnesses
            .into_iter()
            .map(|witness| self.decompose_block_header_finalize(ctx, witness))
            .collect_vec();

        // record for each idx whether hash of headers[idx] is in headers[idx + 1]
        if let Some(num_blocks_minus_one) = num_blocks_minus_one {
            let mut hash_checks = Vec::with_capacity(traces.len() - 1);
            for idx in 0..traces.len() - 1 {
                let hash_check = self.gate().is_equal(
                    ctx,
                    Existing(&traces[idx].block_hash.rlc_val),
                    Existing(&traces[idx + 1].parent_hash.field_trace.rlc_val),
                );
                hash_checks.push(hash_check);
                /*
                // RLC equality with block_hash already ensures this:
                self.gate().assert_is_const(
                    ctx,
                    &traces[idx + 1].parent_hash.field_trace.len,
                    self.f_32,
                );
                */
            }
            let hash_check_sums =
                self.gate().sum_with_assignments(ctx, hash_checks.iter().map(Existing));
            let hash_check_sum = self.gate().select_from_idx(
                ctx,
                once(Constant(F::zero())).chain(hash_check_sums.iter().step_by(3).map(Existing)),
                Existing(num_blocks_minus_one),
            );
            ctx.region.constrain_equal(hash_check_sum.cell(), num_blocks_minus_one.cell());
        } else {
            for idx in 0..traces.len() - 1 {
                ctx.region.constrain_equal(
                    traces[idx].block_hash.rlc_val.cell(),
                    traces[idx + 1].parent_hash.field_trace.rlc_val.cell(),
                );
            }
        }

        traces
    }
}

/// Given a block header chain `chain` of fixed length `2^max_depth`, returns
/// ```
/// (prev_block_hash, end_block_hash, start_block_number || end_block_number)
/// ```
/// where
/// ```
/// prev_block_hash = chain[0].parent_hash,
/// end_block_hash = chain[num_blocks_minus_one].block_hash,
/// start_block_number = chain[0].number,
/// end_block_number = chain[num_blocks_minus_one].number
/// ```
/// The hashes are H256 that are represented as two u128
/// (we need them in 128 bits to fit in Bn254 scalar field).
///
/// The numbers are left padded by zeros to be exactly 4 bytes (u32); the two padded numbers are concatenated together to a u64.  
pub fn get_boundary_block_data<'v, F: Field + PrimeField>(
    ctx: &mut Context<'_, F>,
    gate: &impl GateInstructions<F>,
    chain: &[EthBlockHeaderTrace<'v, F>],
    num_blocks_minus_one: &AssignedValue<'v, F>,
) -> ([AssignedValue<'v, F>; 2], [AssignedValue<'v, F>; 2], AssignedValue<'v, F>) {
    let prev_block_hash: [_; 2] =
        bytes_be_to_u128(ctx, gate, &chain[0].parent_hash.field_trace.values).try_into().unwrap();
    let end_block_hash: [_; 2] = {
        let end_block_hash_bytes = (0..32)
            .map(|idx| {
                gate.select_from_idx(
                    ctx,
                    chain.iter().map(|header| Existing(&header.block_hash.values[idx])),
                    Existing(num_blocks_minus_one),
                )
            })
            .collect_vec();
        bytes_be_to_u128(ctx, gate, &end_block_hash_bytes).try_into().unwrap()
    };

    // start_block_number || end_block_number
    let block_numbers = {
        debug_assert_eq!(chain[0].number.field_trace.max_len, BLOCK_NUMBER_MAX_BYTES);
        let start_block_number_bytes = bytes_be_var_to_fixed(
            ctx,
            gate,
            &chain[0].number.field_trace.values,
            &chain[0].number.field_trace.len,
            BLOCK_NUMBER_MAX_BYTES,
        );
        // TODO: is there a way to do this without so many selects
        let end_block_number_bytes: [_; BLOCK_NUMBER_MAX_BYTES] =
            core::array::from_fn(|i| i).map(|idx| {
                gate.select_from_idx(
                    ctx,
                    chain.iter().map(|header| Existing(&header.number.field_trace.values[idx])),
                    Existing(num_blocks_minus_one),
                )
            });
        let end_block_number_len = gate.select_from_idx(
            ctx,
            chain.iter().map(|header| Existing(&header.number.field_trace.len)),
            Existing(num_blocks_minus_one),
        );
        let mut end_block_number_bytes = bytes_be_var_to_fixed(
            ctx,
            gate,
            &end_block_number_bytes,
            &end_block_number_len,
            BLOCK_NUMBER_MAX_BYTES,
        );
        let mut block_numbers_bytes = start_block_number_bytes;
        block_numbers_bytes.append(&mut end_block_number_bytes);
        let [block_numbers]: [_; 1] =
            bytes_be_to_u128(ctx, gate, &block_numbers_bytes).try_into().unwrap();
        block_numbers
    };

    (prev_block_hash, end_block_hash, block_numbers)
}

/// Takes a fixed length array `bytes` and returns a length `out_len` array equal to
/// `[[0; out_len - len], bytes[..len]].concat()`, i.e., we take `bytes[..len]` and
/// zero pad it on the left.
///
/// Assumes `0 < len <= max_len <= out_len`.
pub fn bytes_be_var_to_fixed<'v, F: ScalarField>(
    ctx: &mut Context<'_, F>,
    gate: &impl GateInstructions<F>,
    bytes: &[AssignedValue<'v, F>],
    len: &AssignedValue<'v, F>,
    out_len: usize,
) -> Vec<AssignedValue<'v, F>> {
    debug_assert!(bytes.len() <= out_len);
    debug_assert!(bit_length(out_len as u64) < F::CAPACITY as usize);

    // If `bytes` is an RLP field, then `len <= bytes.len()` was already checked during `decompose_rlp_array_prepare` so we don't need to do it again:
    // range.range_check(ctx, len, bit_length(bytes.len() as u64));

    // out[idx] = 1{ len >= out_len - idx } * bytes[idx + len - out_len]
    (0..out_len)
        .map(|idx| {
            let byte_idx = gate.sub(ctx, Existing(len), Constant(F::from((out_len - idx) as u64)));
            // If `len - (out_len - idx) < 0` then the `F` value will be >= `bytes.len()` provided that `out_len` is not too big -- namely `bit_length(out_len) <= F::CAPACITY - 1`
            // Thus select_from_idx at idx < 0 will return 0
            gate.select_from_idx(ctx, bytes.iter().map(|x| Existing(x)), Existing(&byte_idx))
        })
        .collect()
}

#[derive(Serialize, Deserialize, Debug, Clone)]
pub struct EthBlockHeaderChainInstance {
    prev_hash: H256,
    end_hash: H256,
    start_block_number: u32,
    end_block_number: u32,
    merkle_mountain_range: Vec<H256>,
}

impl EthBlockHeaderChainInstance {
    pub fn new(
        prev_hash: H256,
        end_hash: H256,
        start_block_number: u32,
        end_block_number: u32,
        merkle_mountain_range: Vec<H256>,
    ) -> Self {
        Self { prev_hash, end_hash, start_block_number, end_block_number, merkle_mountain_range }
    }

    pub fn to_instance<F: Field>(&self) -> Vec<F> {
        // * prevHash: uint256 represented as 2 uint128s
        // * endHash:  uint256 represented as 2 uint128s
        // * startBlockNumber || endBlockNumber: 0..0 || uint32 || 0..0 || uint32 as u64 (exactly 64 bits)
        // * merkleRoots: Vec<uint256>, each represented as 2 uint128s
        let [prev_hash, end_hash] =
            [&self.prev_hash, &self.end_hash].map(|hash| encode_h256_to_field::<F>(hash));
        let block_numbers =
            F::from(((self.start_block_number as u64) << 32) + (self.end_block_number as u64));
        let merkle_mountain_range = self
            .merkle_mountain_range
            .iter()
            .flat_map(|hash| encode_h256_to_field::<F>(hash))
            .collect_vec();

        [&prev_hash[..], &end_hash[..], &[block_numbers], &merkle_mountain_range].concat()
    }
}

#[derive(Clone, Debug)]
pub struct EthBlockHeaderChainCircuit<F> {
    inputs: Vec<Vec<u8>>,
    num_blocks: u32, // num_blocks in [0, 2 ** max_depth)
    /// (prev_hash, end_hash, start_block_number, end_block_number, merkle_roots: [H256; max_depth + 1])
    pub instance: EthBlockHeaderChainInstance,
    max_depth: usize,
    network: Network,
    _marker: PhantomData<F>,
}

impl<F: Field + PrimeField> Circuit<F> for EthBlockHeaderChainCircuit<F> {
    type Config = EthBlockHeaderConfig<F>;
    type FloorPlanner = SimpleFloorPlanner;

    fn without_witnesses(&self) -> Self {
        Self {
            inputs: self.inputs.iter().map(|input| vec![0; input.len()]).collect_vec(),
            num_blocks: 0,
            instance: EthBlockHeaderChainInstance {
                prev_hash: H256::default(),
                end_hash: H256::default(),
                start_block_number: 0,
                end_block_number: 0,
                merkle_mountain_range: vec![H256::default(); self.max_depth + 1],
            },
            max_depth: self.max_depth,
            network: self.network,
            _marker: PhantomData,
        }
    }

    fn configure(meta: &mut ConstraintSystem<F>) -> Self::Config {
        let params = EthBlockHeaderConfigParams::get();
        EthBlockHeaderConfig::configure(meta, params, 0)
    }

    fn synthesize(
        &self,
        config: Self::Config,
        mut layouter: impl Layouter<F>,
    ) -> Result<(), Error> {
        #[cfg(feature = "display")]
        let witness_gen = start_timer!(|| "synthesize");

        config.rlp.range.load_lookup_table(&mut layouter).expect("load range lookup table");
        config.keccak.load(&mut layouter).expect("load keccak lookup tables");
        let instance_column = config.instance;
        let mut chip = EthBlockHeaderChip::new(config, Value::unknown());

        let mut first_pass = SKIP_FIRST_PASS;
        let mut instances = Vec::new();
        layouter
            .assign_region(
                || "Eth block header chain of variable length with merkle mountain range",
                |region| {
                    if first_pass {
                        first_pass = false;
                        return Ok(());
                    }
                    let mut aux = Context::new(
                        region,
                        ContextParams {
                            max_rows: chip.gate().max_rows,
                            num_context_ids: 2,
                            fixed_columns: chip.gate().constants.clone(),
                        },
                    );
                    let ctx = &mut aux;

                    // ======== FIRST PHASE ===========
                    // ==== Load private inputs =====
                    let num_blocks = chip
                        .gate()
                        .load_witness(ctx, Value::known(F::from(self.num_blocks as u64)));
                    let num_blocks_minus_one =
                        chip.gate().sub(ctx, Existing(&num_blocks), Constant(F::one()));
                    // `num_blocks_minus_one` should be < 2^max_depth. We do not check this because `num_blocks_minus_one` will equal the difference of the start, end block numbers, which are public inputs

                    // ==== Load RLP encoding and decode ====
                    // The block header RLPs are assigned as witnesses in this function
                    let block_chain_witness =
                        chip.decompose_block_header_chain_prepare(ctx, &self.inputs, self.network);
                    // All keccaks must be done in FirstPhase, so we compute the merkle mountain range from the RLP decoded witnesses now
                    let num_leaves_bits =
                        chip.gate().num_to_bits(ctx, &num_blocks, self.max_depth + 1);
                    let block_hashes = block_chain_witness
                        .iter()
                        .map(|witness| &witness.block_hash_bytes[..])
                        .collect_vec();
                    // mountain range in bytes
                    let mountain_range = chip.keccak.merkle_mountain_range(
                        ctx,
                        chip.rlp.gate(),
                        &block_hashes,
                        &num_leaves_bits,
                    );
                    let mountain_range = mountain_range
                        .into_iter()
                        .zip(num_leaves_bits.iter().rev())
                        .flat_map(|(hash_bytes, bit)| {
                            // convert bytes32 to two u128
                            let hash_u128s: [_; 2] =
                                bytes_be_to_u128(ctx, chip.gate(), &hash_bytes).try_into().unwrap();
                            // if the bit is 0, then we set the hash root to 0
                            hash_u128s.map(|hash_u128| {
                                chip.gate().mul(ctx, Existing(&hash_u128), Existing(bit))
                            })
                        })
                        .collect_vec();

                    // assign cells to range check to special advice columns with lookup enabled
                    let num_lookup_advice_cells = chip.range().finalize(ctx);
                    ctx.next_phase();

                    // ======== SECOND PHASE ========
                    // get challenge now that it has been squeezed
                    chip.get_challenge(ctx);

                    let block_chain_trace = chip.decompose_block_header_chain_finalize(
                        ctx,
                        block_chain_witness,
                        Some(&num_blocks_minus_one),
                    );

                    // It's better to do more computations in SecondPhase because that means less commitments for the Challenge to hash, although the difference is probably marginal.
                    let (prev_block_hash, end_block_hash, block_numbers) = get_boundary_block_data(
                        ctx,
                        chip.gate(),
                        &block_chain_trace,
                        &num_blocks_minus_one,
                    );

                    instances.extend(
                        iter::empty()
                            .chain([
                                &prev_block_hash[0],
                                &prev_block_hash[1],
                                &end_block_hash[0],
                                &end_block_hash[1],
                                &block_numbers,
                            ])
                            .chain(mountain_range.iter())
                            .map(|assigned| assigned.cell())
                            .cloned(),
                    );

                    #[cfg(feature = "display")]
                    {
                        ctx.print_stats(&["Range", "RLC"], num_lookup_advice_cells);
                        chip.keccak.print_stats(ctx);
                    }
                    Ok(())
                },
            )
            .unwrap();

        // expose public instances
        for (i, instance) in instances.into_iter().enumerate() {
            layouter.constrain_instance(instance, instance_column, i);
        }
        #[cfg(feature = "display")]
        end_timer!(witness_gen);
        Ok(())
    }
}

impl<F: Field> EthBlockHeaderChainCircuit<F> {
    pub fn get_num_instance(max_depth: usize) -> usize {
        5 + 2 * (max_depth + 1)
    }

    #[cfg(feature = "providers")]
    pub fn from_provider(
        provider: &Provider<Http>,
        network: Network,
        start_block_number: u32,
        num_blocks: u32,
        max_depth: usize,
    ) -> Self {
        let header_rlp_max_bytes = match network {
            Network::Mainnet => MAINNET_BLOCK_HEADER_RLP_MAX_BYTES,
            Network::Goerli => GOERLI_BLOCK_HEADER_RLP_MAX_BYTES,
        };
        let (mut block_rlps, instance) =
            crate::providers::get_blocks_input(provider, start_block_number, num_blocks, max_depth);
        for block_rlp in block_rlps.iter_mut() {
            block_rlp.resize(header_rlp_max_bytes, 0u8);
        }

        Self { inputs: block_rlps, num_blocks, instance, max_depth, network, _marker: PhantomData }
    }
}
