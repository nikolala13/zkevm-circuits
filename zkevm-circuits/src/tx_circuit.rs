//! The transaction circuit implementation.

// Naming notes:
// - *_be: Big-Endian bytes
// - *_le: Little-Endian bytes

#[cfg(any(feature = "test", test, feature = "test-circuits"))]
/// TxCircuitTester is the combined circuit of tx circuit and sig circuit.
mod dev;
#[cfg(any(feature = "test", test))]
mod test;
#[cfg(any(feature = "test", test, feature = "test-circuits"))]
pub use dev::TxCircuitTester as TestTxCircuit;

use crate::{
    evm_circuit::util::constraint_builder::{BaseConstraintBuilder, ConstrainBuilderCommon},
    sig_circuit::SigCircuit,
    table::{
        BlockContextFieldTag::{CumNumTxs, NumAllTxs, NumTxs},
        BlockTable, KeccakTable, LookupTable, RlpFsmRlpTable as RlpTable, SigTable, TxFieldTag,
        TxFieldTag::{
            BlockNumber, CallData, CallDataGasCost, CallDataLength, CallDataRLC, CalleeAddress,
            CallerAddress, ChainID, Gas, GasPrice, IsCreate, Nonce, SigR, SigS, SigV,
            TxDataGasCost, TxHashLength, TxHashRLC, TxSignHash, TxSignLength, TxSignRLC,
        },
        TxTable, U16Table, U8Table,
    },
    util::{
        is_zero::{IsZeroChip, IsZeroConfig},
        keccak, random_linear_combine_word as rlc, rlc_be_bytes, SubCircuit, SubCircuitConfig,
    },
    witness,
    witness::{
        rlp_fsm::{Tag, ValueTagLength},
        Format::{L1MsgHash, TxHashEip155, TxHashPreEip155, TxSignEip155, TxSignPreEip155},
        RlpTag,
        RlpTag::{GasCost, Len, Null, RLC},
        Tag::TxType as RLPTxType,
        Transaction,
    },
};
use bus_mapping::circuit_input_builder::keccak_inputs_sign_verify;
use eth_types::{
    geth_types::{
        TxType,
        TxType::{Eip155, L1Msg, PreEip155},
    },
    sign_types::SignData,
    Address, Field, ToAddress, ToBigEndian, ToLittleEndian, ToScalar,
};
use gadgets::{
    binary_number::{BinaryNumberChip, BinaryNumberConfig},
    comparator::{ComparatorChip, ComparatorConfig, ComparatorInstruction},
    is_equal::{IsEqualChip, IsEqualConfig, IsEqualInstruction},
    less_than::{LtChip, LtConfig, LtInstruction},
    util::{and, not, select, sum, Expr},
};
use halo2_proofs::{
    circuit::{Layouter, Region, Value},
    plonk::{Advice, Column, ConstraintSystem, Error, Expression, VirtualCells},
    poly::Rotation,
};
use log::error;
use num::Zero;
use std::{
    collections::{BTreeMap, BTreeSet, HashMap},
    iter,
    marker::PhantomData,
};

#[cfg(feature = "onephase")]
use halo2_proofs::plonk::FirstPhase as SecondPhase;
use halo2_proofs::plonk::Fixed;
#[cfg(not(feature = "onephase"))]
use halo2_proofs::plonk::SecondPhase;
use itertools::Itertools;
use crate::witness::rlp_fsm::get_rlp_len_tag_length;

/// Number of rows of one tx occupies in the fixed part of tx table
pub const TX_LEN: usize = 23;
/// Offset of TxHash tag in the tx table
pub const TX_HASH_OFFSET: usize = 21;
/// Offset of ChainID tag in the tx table
pub const CHAIN_ID_OFFSET: usize = 12;

#[derive(Copy, Clone, Debug, PartialEq, Eq, Hash)]
enum LookupCondition {
    // lookup into tx table
    TxCalldata,
    // lookup into rlp table
    L1MsgHash,
    RlpSignTag,
    RlpHashTag,
    // lookup into keccak table
    Keccak,
}

/// Config for TxCircuit
#[derive(Clone, Debug)]
pub struct TxCircuitConfig<F: Field> {
    minimum_rows: usize,

    // This is only true at the first row of calldata part of tx table
    q_calldata_first: Column<Fixed>,
    q_calldata_last: Column<Fixed>,
    // A selector which is enabled at 1st row
    q_first: Column<Fixed>,
    tx_table: TxTable,
    tx_tag_bits: BinaryNumberConfig<TxFieldTag, 5>,

    tx_type: Column<Advice>,
    tx_type_bits: BinaryNumberConfig<TxType, 3>,
    // The associated rlp tag to lookup in the RLP table
    rlp_tag: Column<Advice>,
    // Whether tag's RLP-encoded value is 0x80 = rlp([])
    is_none: Column<Advice>,
    tx_value_length: Column<Advice>,
    tx_value_rlc: Column<Advice>,

    u8_table: U8Table,
    u16_table: U16Table,

    /// Verify if the tx_id is zero or not.
    tx_id_is_zero: IsZeroConfig<F>,
    /// Primarily used to verify if the `CallDataLength` is zero or non-zero
    ///  and `CallData` byte is zero or non-zero.
    value_is_zero: IsZeroConfig<F>,
    /// We use an equality gadget to know whether the tx id changes between
    /// subsequent rows or not.
    tx_id_unchanged: IsEqualConfig<F>,

    /// Columns used to reduce degree
    is_tag_block_num: Column<Advice>,
    is_calldata: Column<Advice>,
    is_caller_address: Column<Advice>,
    is_l1_msg: Column<Advice>,
    is_chain_id: Column<Advice>,
    lookup_conditions: HashMap<LookupCondition, Column<Advice>>,

    /// Columns for computing num_all_txs
    tx_nonce: Column<Advice>,
    block_num: Column<Advice>,
    block_num_unchanged: IsEqualConfig<F>,
    num_all_txs_acc: Column<Advice>,
    total_l1_popped_before: Column<Advice>,

    /// Columns for accumulating call_data_length and call_data_gas_cost
    /// A boolean advice column, which is turned on only for the last byte in
    /// call data.
    is_final: Column<Advice>,
    /// An accumulator value used to correctly calculate the calldata gas cost
    /// for a tx.
    calldata_gas_cost_acc: Column<Advice>,
    /// An accumulator value used to correctly calculate the RLC(calldata) for a tx.
    calldata_rlc: Column<Advice>,
    /// 1st phase column which equals to tx_table.value when is_calldata is true
    /// We need this because tx_table.value is a 2nd phase column and is used to get calldata_rlc.
    /// It's not safe to do RLC on columns of same phase.
    calldata_byte: Column<Advice>,

    /// Columns for ensuring that BlockNum is correct
    is_padding_tx: Column<Advice>,
    /// Tx id must be no greater than cum_num_txs
    tx_id_cmp_cum_num_txs: ComparatorConfig<F, 2>,
    tx_id_gt_prev_cnt: LtConfig<F, 2>,
    /// Cumulative number of txs up to a block
    cum_num_txs: Column<Advice>,
    /// Number of txs in a block
    num_txs: Column<Advice>,

    /// Address recovered by SignVerifyChip
    sv_address: Column<Advice>,

    sig_table: SigTable,

    // External tables
    block_table: BlockTable,
    rlp_table: RlpTable,
    keccak_table: KeccakTable,

    _marker: PhantomData<F>,
}

/// Circuit configuration arguments
pub struct TxCircuitConfigArgs<F: Field> {
    /// TxTable
    pub tx_table: TxTable,
    /// Block Table
    pub block_table: BlockTable,
    /// KeccakTable
    pub keccak_table: KeccakTable,
    /// RlpTable
    pub rlp_table: RlpTable,
    /// SigTable
    pub sig_table: SigTable,
    /// Reusable u8 lookup table,
    pub u8_table: U8Table,
    /// Reusable u16 lookup table,
    pub u16_table: U16Table,
    /// Challenges
    pub challenges: crate::util::Challenges<Expression<F>>,
}

impl<F: Field> SubCircuitConfig<F> for TxCircuitConfig<F> {
    type ConfigArgs = TxCircuitConfigArgs<F>;

    /// Return a new TxCircuitConfig
    fn new(
        meta: &mut ConstraintSystem<F>,
        Self::ConfigArgs {
            tx_table,
            block_table,
            keccak_table,
            rlp_table,
            sig_table,
            u8_table,
            u16_table,
            challenges,
        }: Self::ConfigArgs,
    ) -> Self {
        let q_enable = tx_table.q_enable;

        let q_first = meta.fixed_column();
        let q_calldata_first = meta.fixed_column();
        let q_calldata_last = meta.fixed_column();
        // Since we allow skipping l1 txs that could cause potential circuit overflow,
        // the num_all_txs (num_l1_msgs + num_l2_txs) in the input to get chunk data hash
        // does not necessarily equal to num_txs (self.txs.len()) in block table.
        // Therefore we calculated two numbers (num_l1_msgs, num_l2_txs) in tx circuit
        // and then asserts that `num_l1_msgs + num_l2_txs = num_all_txs` in pi circuit.
        //
        // In more detail, all txs in same block are grouped together and we iterate over
        // its txs to get `num_all_txs`.
        //
        //  | is_l1_msg | queue_index | total_l1_popped_before |  num_all_txs  |
        //  |    true   |     q1      |           c            |    q1-c+1     |
        //  |    false  |             |         q1+1           |    q1-c+2     |
        //  |    true   |     q2      |         q1+1           |    q2-c+2     |
        //  |    true   |     q3      |         q2+1           |    q3-c+2     |

        let tx_nonce = meta.advice_column();
        let block_num = meta.advice_column();

        let total_l1_popped_before = meta.advice_column();
        // num_all_txs = num_l1_msgs + num_l2_txs
        let num_all_txs_acc = meta.advice_column();

        // tag, rlp_tag, tx_type, is_none
        let tx_type = meta.advice_column();
        let rlp_tag = meta.advice_column();
        let tx_value_rlc = meta.advice_column_in(SecondPhase);
        let tx_value_length = meta.advice_column();
        let is_none = meta.advice_column();
        let tag_bits = BinaryNumberChip::configure(meta, q_enable, Some(tx_table.tag.into()));
        let tx_type_bits = BinaryNumberChip::configure(meta, q_enable, Some(tx_type.into()));

        // columns for constraining BlockNum is valid
        let cum_num_txs = meta.advice_column();
        // num_of_txs that each block contains
        let num_txs = meta.advice_column();
        let is_padding_tx = meta.advice_column();

        // columns for accumulating length and gas_cost of call_data
        let is_final = meta.advice_column();
        let calldata_gas_cost_acc = meta.advice_column();
        let calldata_rlc = meta.advice_column_in(SecondPhase);
        let calldata_byte = meta.advice_column();

        // booleans to reduce degree
        let is_l1_msg = meta.advice_column();
        let is_calldata = meta.advice_column();
        let is_caller_address = meta.advice_column();
        let is_chain_id = meta.advice_column();
        let is_tag_block_num = meta.advice_column();
        let lookup_conditions = [
            LookupCondition::TxCalldata,
            LookupCondition::L1MsgHash,
            LookupCondition::RlpSignTag,
            LookupCondition::RlpHashTag,
            LookupCondition::Keccak,
        ]
        .into_iter()
        .map(|condition| (condition, meta.advice_column()))
        .collect::<HashMap<LookupCondition, Column<Advice>>>();

        // TODO: add lookup to SignVerify table for sv_address
        let sv_address = meta.advice_column();
        meta.enable_equality(tx_table.value);

        let log_deg = |s: &'static str, meta: &mut ConstraintSystem<F>| {
            debug_assert!(meta.degree() <= 9);
            log::info!("after {}, meta.degree: {}", s, meta.degree());
        };

        // tx_id == 0
        let tx_id_is_zero = IsZeroChip::configure(
            meta,
            |meta| meta.query_fixed(q_enable, Rotation::cur()),
            tx_table.tx_id,
            |meta| meta.advice_column(),
        );

        // macros
        macro_rules! is_tx_tag {
            ($var:ident, $tag_variant:ident) => {
                let $var = |meta: &mut VirtualCells<F>| {
                    tag_bits.value_equals(TxFieldTag::$tag_variant, Rotation::cur())(meta)
                };
            };
        }

        // tx tags
        is_tx_tag!(is_null, Null);
        is_tx_tag!(is_nonce, Nonce);
        is_tx_tag!(is_gas_price, GasPrice);
        is_tx_tag!(is_gas, Gas);
        is_tx_tag!(is_caller_addr, CallerAddress);
        is_tx_tag!(is_to, CalleeAddress);
        is_tx_tag!(is_create, IsCreate);
        is_tx_tag!(is_value, Value);
        is_tx_tag!(is_data, CallData);
        is_tx_tag!(is_data_length, CallDataLength);
        is_tx_tag!(is_data_gas_cost, CallDataGasCost);
        is_tx_tag!(is_tx_gas_cost, TxDataGasCost);
        is_tx_tag!(is_data_rlc, CallDataRLC);
        is_tx_tag!(is_chain_id_expr, ChainID);
        is_tx_tag!(is_sig_v, SigV);
        is_tx_tag!(is_sig_r, SigR);
        is_tx_tag!(is_sig_s, SigS);
        is_tx_tag!(is_sign_length, TxSignLength);
        is_tx_tag!(is_sign_rlc, TxSignRLC);
        is_tx_tag!(is_hash_length, TxHashLength);
        is_tx_tag!(is_hash_rlc, TxHashRLC);
        is_tx_tag!(is_sign_hash, TxSignHash);
        is_tx_tag!(is_hash, TxHash);
        is_tx_tag!(is_block_num, BlockNumber);
        is_tx_tag!(is_tx_type, TxType);

        let tx_id_unchanged = IsEqualChip::configure(
            meta,
            |meta| meta.query_fixed(q_enable, Rotation::cur()),
            |meta| meta.query_advice(tx_table.tx_id, Rotation::cur()),
            |meta| meta.query_advice(tx_table.tx_id, Rotation::next()),
        );

        // testing if value is zero for tags
        let value_is_zero = IsZeroChip::configure(
            meta,
            |meta| {
                and::expr(vec![
                    meta.query_fixed(q_enable, Rotation::cur()),
                    sum::expr(vec![
                        // if caller_address is zero, then skip the sig verify.
                        is_caller_addr(meta),
                        // if call_data_length is zero, then skip lookup to tx table for call data
                        is_data_length(meta),
                        // if call data byte is zero, then gas_cost = 4 (16 otherwise)
                        is_data(meta),
                    ]),
                ])
            },
            tx_table.value,
            |meta| meta.advice_column_in(SecondPhase), // value is at 2nd phase
        );

        // tx_id transition in the fixed part of tx table
        meta.create_gate("tx_id starts with 1", |meta| {
            let mut cb = BaseConstraintBuilder::default();

            // the first row in tx table are all-zero rows
            cb.require_equal(
                "tx_id == 1",
                meta.query_advice(tx_table.tx_id, Rotation::next()),
                1.expr(),
            );

            cb.gate(meta.query_fixed(q_first, Rotation::cur()))
        });

        meta.create_gate("tx_id transition in the fixed part of tx table", |meta| {
            let mut cb = BaseConstraintBuilder::default();

            // if tag_next == Nonce, then tx_id' = tx_id + 1
            cb.condition(tag_bits.value_equals(Nonce, Rotation::next())(meta), |cb| {
                cb.require_equal(
                    "tx_id increments",
                    meta.query_advice(tx_table.tx_id, Rotation::next()),
                    meta.query_advice(tx_table.tx_id, Rotation::cur()) + 1.expr(),
                );
            });
            // if tag_next != Nonce, then tx_id' = tx_id, tx_type' = tx_type
            cb.condition(
                not::expr(tag_bits.value_equals(Nonce, Rotation::next())(meta)),
                |cb| {
                    cb.require_equal(
                        "tx_id does not change",
                        meta.query_advice(tx_table.tx_id, Rotation::next()),
                        meta.query_advice(tx_table.tx_id, Rotation::cur()),
                    );
                    // tx meta infos that extracted at some row and need to be copied to all rows of
                    // same tx
                    let tx_meta_info_fields = vec![
                        ("tx_type", tx_type),             // extracted at SigV row
                        ("is_padding_tx", is_padding_tx), // extracted at CallerAddress row
                        ("sv_address", sv_address),       // extracted at ChainID row
                        ("block_num", block_num),         // extracted at BlockNum row
                        ("total_l1_popped_before", total_l1_popped_before),
                        ("num_txs", num_txs),
                        ("cum_num_txs", cum_num_txs),
                        ("num_all_txs_acc", num_all_txs_acc),
                        // is_l1_msg does not need to spread out as it's extracted from tx_type

                        // these do not need to spread out as they are related to tx_table.tag
                        // (which is fixed col) is_chain_id,
                        // is_caller_address, is_tag_block_num, is_calldata
                    ];
                    for (col_name, meta_info) in tx_meta_info_fields {
                        cb.require_equal(
                            col_name,
                            meta.query_advice(meta_info, Rotation::next()),
                            meta.query_advice(meta_info, Rotation::cur()),
                        );
                    }
                },
            );

            cb.gate(and::expr([
                meta.query_fixed(q_enable, Rotation::cur()),
                not::expr(meta.query_advice(is_calldata, Rotation::next())),
            ]))
        });

        // Basic constraints
        meta.create_gate("basic constraints", |meta| {
            let mut cb = BaseConstraintBuilder::default();

            let rlp_tag_map: Vec<(Expression<F>, RlpTag)> = vec![
                (is_nonce(meta), Tag::Nonce.into()),
                (is_gas_price(meta), Tag::GasPrice.into()),
                (is_gas(meta), Tag::Gas.into()),
                (is_to(meta), Tag::To.into()),
                (is_value(meta), Tag::Value.into()),
                (is_data_rlc(meta), Tag::Data.into()),
                (is_sig_v(meta), Tag::SigV.into()),
                (is_sig_r(meta), Tag::SigR.into()),
                (is_sig_s(meta), Tag::SigS.into()),
                (is_sign_length(meta), Len),
                (is_sign_rlc(meta), RLC),
                (is_hash_length(meta), Len),
                (is_hash_rlc(meta), RLC),
                (is_caller_addr(meta), Tag::Sender.into()),
                (is_tx_gas_cost(meta), GasCost),
                // tx tags which correspond to Null
                (is_null(meta), Null),
                (is_create(meta), Null),
                (is_data_length(meta), Null),
                (is_data_gas_cost(meta), Null),
                (is_sign_hash(meta), Null),
                (is_hash(meta), Null),
                (is_data(meta), Null),
                (is_block_num(meta), Null),
                (is_chain_id_expr(meta), Tag::ChainId.into()),
                (is_tx_type(meta), Null),
            ];

            cb.require_boolean(
                "is_none is boolean",
                meta.query_advice(is_none, Rotation::cur()),
            );

            cb.require_in_set(
                "tx_type supported",
                meta.query_advice(tx_type, Rotation::cur()),
                vec![
                    usize::from(PreEip155).expr(),
                    usize::from(Eip155).expr(),
                    usize::from(L1Msg).expr(),
                ],
            );

            cb.condition(is_tx_type(meta), |cb| {
                cb.require_equal(
                    "associated tx type to tag",
                    meta.query_advice(tx_type, Rotation::cur()),
                    meta.query_advice(tx_table.value, Rotation::cur()),
                );
            });

            cb.require_equal(
                "associated rlp_tag",
                meta.query_advice(rlp_tag, Rotation::cur()),
                rlp_tag_map.into_iter().fold(0.expr(), |acc, (expr, tag)| {
                    acc + usize::from(tag).expr() * expr
                }),
            );

            cb.condition(is_to(meta), |cb| {
                cb.require_equal(
                    "is_create == is_none",
                    // we rely on the assumption that IsCreate is next to CalleeAddress
                    meta.query_advice(tx_table.value, Rotation::next()),
                    meta.query_advice(is_none, Rotation::cur()),
                );
            });

            let is_none_expr = meta.query_advice(is_none, Rotation::cur());
            // is_none == true
            cb.condition(is_none_expr.expr(), |cb| {
                // value == 0
                cb.require_equal(
                    "is_none is true => value == 0",
                    meta.query_advice(tx_table.value, Rotation::cur()),
                    0.expr(),
                );
            });

            // CallData is none =>
            // 1. CallDataLength == 0
            // 2. CallDataGasCost == 0
            cb.condition(and::expr([is_data_rlc(meta), is_none_expr.expr()]), |cb| {
                // we rely on the assumption that CallDataLength and CallDataGasCost are after
                // CallDataRLC
                cb.require_equal(
                    "CallDataLength.value == 0",
                    meta.query_advice(tx_table.value, Rotation::next()),
                    0.expr(),
                );
                cb.require_equal(
                    "CallDataGasCost.value == 0",
                    meta.query_advice(tx_table.value, Rotation(2)),
                    0.expr(),
                );
            });

            // CallData is not none => CallDataLength != 0
            cb.condition(
                and::expr([is_data_rlc(meta), not::expr(is_none_expr)]),
                |cb| {
                    cb.require_zero(
                        "CallDataLength != 0",
                        value_is_zero.expr(Rotation::next())(meta),
                    );
                },
            );

            cb.gate(meta.query_fixed(q_enable, Rotation::cur()))
        });

        //////////////////////////////////////////////////////////
        ///// Constraints for booleans that reducing degree  /////
        //////////////////////////////////////////////////////////
        meta.create_gate("is_calldata", |meta| {
            let mut cb = BaseConstraintBuilder::default();

            cb.require_equal(
                "is_calldata",
                is_data(meta),
                meta.query_advice(is_calldata, Rotation::cur()),
            );

            cb.gate(meta.query_fixed(q_enable, Rotation::cur()))
        });

        meta.create_gate("is_caller_address", |meta| {
            let mut cb = BaseConstraintBuilder::default();

            cb.require_equal(
                "is_caller_address",
                is_caller_addr(meta),
                meta.query_advice(is_caller_address, Rotation::cur()),
            );

            cb.gate(meta.query_fixed(q_enable, Rotation::cur()))
        });

        meta.create_gate("is_chain_id", |meta| {
            let mut cb = BaseConstraintBuilder::default();

            cb.require_equal(
                "is_chain_id",
                is_chain_id_expr(meta),
                meta.query_advice(is_chain_id, Rotation::cur()),
            );

            cb.gate(meta.query_fixed(q_enable, Rotation::cur()))
        });

        meta.create_gate("is_tag_block_num", |meta| {
            let mut cb = BaseConstraintBuilder::default();

            cb.require_equal(
                "is_tag_block_num = (tag == BlockNum)",
                is_block_num(meta),
                meta.query_advice(is_tag_block_num, Rotation::cur()),
            );

            cb.gate(meta.query_fixed(q_enable, Rotation::cur()))
        });

        meta.create_gate("is_l1_msg", |meta| {
            let mut cb = BaseConstraintBuilder::default();

            cb.require_equal(
                "is_l1_msg = (tx_type == L1Msg)",
                meta.query_advice(is_l1_msg, Rotation::cur()),
                tx_type_bits.value_equals(L1Msg, Rotation::cur())(meta),
            );

            cb.gate(meta.query_fixed(q_enable, Rotation::cur()))
        });

        meta.create_gate("calldata lookup into tx table condition", |meta| {
            let mut cb = BaseConstraintBuilder::default();

            cb.require_equal(
                "condition",
                and::expr([
                    is_data_length(meta),
                    not::expr(value_is_zero.expr(Rotation::cur())(meta)),
                ]),
                meta.query_advice(
                    lookup_conditions[&LookupCondition::TxCalldata],
                    Rotation::cur(),
                ),
            );

            cb.gate(meta.query_fixed(q_enable, Rotation::cur()))
        });

        meta.create_gate("sign tag lookup into RLP table condition", |meta| {
            let mut cb = BaseConstraintBuilder::default();

            let is_tag_in_tx_sign = sum::expr([
                is_nonce(meta),
                is_gas_price(meta),
                is_gas(meta),
                is_to(meta),
                is_value(meta),
                is_data_rlc(meta),
                and::expr([
                    meta.query_advice(is_chain_id, Rotation::cur()),
                    tx_type_bits.value_equals(Eip155, Rotation::cur())(meta),
                ]),
                is_sign_length(meta),
                is_sign_rlc(meta),
            ]);

            cb.require_equal(
                "condition",
                is_tag_in_tx_sign,
                meta.query_advice(
                    lookup_conditions[&LookupCondition::RlpSignTag],
                    Rotation::cur(),
                ),
            );

            cb.gate(and::expr([
                meta.query_fixed(q_enable, Rotation::cur()),
                not::expr(meta.query_advice(is_l1_msg, Rotation::cur())),
            ]))
        });

        meta.create_gate("hash tag lookup into RLP table condition", |meta| {
            let mut cb = BaseConstraintBuilder::default();

            let is_tag_in_tx_hash = sum::expr([
                is_nonce(meta),
                is_gas_price(meta),
                is_gas(meta),
                is_to(meta),
                is_value(meta),
                is_tx_gas_cost(meta),
                is_data_rlc(meta),
                is_sig_v(meta),
                is_sig_r(meta),
                is_sig_s(meta),
                is_hash_length(meta),
                is_hash_rlc(meta),
            ]);

            cb.require_equal(
                "condition",
                is_tag_in_tx_hash,
                meta.query_advice(
                    lookup_conditions[&LookupCondition::RlpHashTag],
                    Rotation::cur(),
                ),
            );

            cb.gate(and::expr([
                meta.query_fixed(q_enable, Rotation::cur()),
                not::expr(meta.query_advice(is_l1_msg, Rotation::cur())),
            ]))
        });

        meta.create_gate("l1 msg lookup into RLP table condition", |meta| {
            let mut cb = BaseConstraintBuilder::default();
            let is_tag_in_l1_msg_hash = sum::expr([
                is_nonce(meta),
                is_gas(meta),
                is_to(meta),
                is_value(meta),
                is_data_rlc(meta),
                is_caller_addr(meta),
                is_hash_length(meta),
                is_hash_rlc(meta),
            ]);

            cb.require_equal(
                "lookup into RLP table iff tag in l1 msg hash",
                is_tag_in_l1_msg_hash,
                meta.query_advice(
                    lookup_conditions[&LookupCondition::L1MsgHash],
                    Rotation::cur(),
                ),
            );

            cb.gate(and::expr([
                meta.query_fixed(q_enable, Rotation::cur()),
                meta.query_advice(is_l1_msg, Rotation::cur()),
            ]))
        });

        meta.create_gate("lookup into Keccak table condition", |meta| {
            let mut cb = BaseConstraintBuilder::default();

            let is_tag_sign_or_hash = sum::expr([
                and::expr([
                    is_sign_length(meta),
                    not::expr(meta.query_advice(is_l1_msg, Rotation::cur())),
                ]),
                is_hash_length(meta),
            ]);
            cb.require_equal(
                "condition",
                is_tag_sign_or_hash,
                meta.query_advice(lookup_conditions[&LookupCondition::Keccak], Rotation::cur()),
            );

            cb.gate(meta.query_fixed(q_enable, Rotation::cur()))
        });

        // lookups to RLP table, Tx table, Keccak table
        Self::configure_lookups(
            meta,
            q_enable,
            rlp_tag,
            tx_value_rlc,
            tx_value_length,
            tx_type_bits,
            tx_id_is_zero.clone(),
            is_none,
            &lookup_conditions,
            is_final,
            is_calldata,
            is_chain_id,
            is_l1_msg,
            sv_address,
            calldata_gas_cost_acc,
            calldata_rlc,
            tx_table.clone(),
            keccak_table.clone(),
            rlp_table,
            sig_table,
        );

        meta.create_gate("tx_gas_cost == 0 for L1 msg", |meta| {
            let mut cb = BaseConstraintBuilder::default();

            cb.condition(is_tx_gas_cost(meta), |cb| {
                cb.require_zero(
                    "tx_gas_cost == 0",
                    meta.query_advice(tx_table.value, Rotation::cur()),
                );
            });

            cb.gate(and::expr([
                meta.query_fixed(q_enable, Rotation::cur()),
                meta.query_advice(is_l1_msg, Rotation::cur()),
            ]))
        });

        ///////////////////////////////////////////////////////////////////////
        ///////////////  constraints on num_all_txs  // ///////////////////////
        ///////////////////////////////////////////////////////////////////////
        meta.create_gate("copy tx_nonce", |meta| {
            let mut cb = BaseConstraintBuilder::default();

            cb.condition(is_nonce(meta), |cb| {
                cb.require_equal(
                    "tx_nonce = tx_table.value if tag == Nonce",
                    meta.query_advice(tx_table.value, Rotation::cur()),
                    meta.query_advice(tx_nonce, Rotation::cur()),
                );
            });

            cb.gate(meta.query_fixed(q_enable, Rotation::cur()))
        });

        meta.create_gate("copy block_num", |meta| {
            let mut cb = BaseConstraintBuilder::default();

            cb.condition(meta.query_advice(is_tag_block_num, Rotation::cur()), |cb| {
                cb.require_equal(
                    "block_num = tx_table.value if tag == BlockNum",
                    meta.query_advice(tx_table.value, Rotation::cur()),
                    meta.query_advice(block_num, Rotation::cur()),
                );
            });

            cb.gate(meta.query_fixed(q_enable, Rotation::cur()))
        });

        // block num is the last row of each tx's fixed rows and since block num is
        // copied to TX_LEN rows. The row at which tag = BlockNum and tx_id = i,
        // its next row has tx_id = i+1. That is, we can use Rotation::next() to get next
        // tx's all meta-infos (including block_num, tx_nonce, num_all_txs_acc, ...)
        let block_num_unchanged = IsEqualChip::configure(
            meta,
            |meta| {
                and::expr([
                    meta.query_fixed(q_enable, Rotation::cur()),
                    meta.query_advice(is_tag_block_num, Rotation::cur()),
                ])
            },
            |meta| meta.query_advice(block_num, Rotation::next()),
            |meta| meta.query_advice(block_num, Rotation::cur()),
        );

        meta.lookup("block_num is non-decreasing till padding txs", |meta| {
            // Block nums like this [1, 3, 5, 4, 0] is rejected by this. But [1, 2, 3, 5, 0] is
            // acceptable.
            let lookup_condition = and::expr([
                // next row should not belong to a padding tx
                not::expr(meta.query_advice(is_padding_tx, Rotation::next())),
                // next row should not be in the calldata region
                not::expr(meta.query_advice(is_calldata, Rotation::next())),
                meta.query_advice(is_tag_block_num, Rotation::cur()),
            ]);

            let block_num_diff = meta.query_advice(block_num, Rotation::next())
                - meta.query_advice(block_num, Rotation::cur());

            vec![(lookup_condition * block_num_diff, u16_table.into())]
        });

        meta.create_gate("num_all_txs in a block", |meta| {
            let mut cb = BaseConstraintBuilder::default();
            let queue_index = tx_nonce;
            // first tx in tx table
            cb.condition(meta.query_fixed(q_first, Rotation::next()), |cb| {
                cb.require_equal(
                    "num_all_txs_acc = is_l1_msg ? queue_index - total_l1_popped_before + 1 : 1",
                    meta.query_advice(num_all_txs_acc, Rotation::cur()),
                    select::expr(
                        meta.query_advice(is_l1_msg, Rotation::cur()),
                        // first tx is l1 msg
                        meta.query_advice(queue_index, Rotation::cur())
                            - meta.query_advice(total_l1_popped_before, Rotation::cur())
                            + 1.expr(),
                        1.expr(),
                    ),
                );
            });

            // non-last tx in cur block
            cb.condition(
                and::expr([
                    // see the comment below
                    not::expr(meta.query_advice(is_calldata, Rotation::next())),
                    block_num_unchanged.expr(),
                ]),
                |cb| {
                    cb.require_equal(
                        "total_l1_popped' = tx.is_l1_msg ? queue_index + 1 : total_l1_popped",
                        meta.query_advice(total_l1_popped_before, Rotation::next()),
                        select::expr(
                            meta.query_advice(is_l1_msg, Rotation::cur()),
                            meta.query_advice(queue_index, Rotation::cur()) + 1.expr(),
                            meta.query_advice(total_l1_popped_before, Rotation::cur()),
                        ),
                    );

                    // num_all_txs_acc' - num_all_txs_acc = is_l1_msg' ? queue_index' -
                    // total_l1_popped' + 1 : 1
                    cb.require_equal(
                        "num_all_txs_acc' - num_all_txs_acc",
                        meta.query_advice(num_all_txs_acc, Rotation::next())
                            - meta.query_advice(num_all_txs_acc, Rotation::cur()),
                        select::expr(
                            meta.query_advice(is_l1_msg, Rotation::next()),
                            meta.query_advice(tx_nonce, Rotation::next())
                                - meta.query_advice(total_l1_popped_before, Rotation::next())
                                + 1.expr(),
                            1.expr(),
                        ),
                    );
                },
            );

            // last tx in cur block (next tx is the first tx in next block)
            // and cur block is not the last block (s.t. we can init next block's num_all_txs)
            cb.condition(
                and::expr([
                    // We need this condition because if this is the last tx of fixed part of tx
                    // table, not(block_num_unchanged.expr()) is very likely to
                    // be true. Since it does not make sense to assign values
                    // to `num_all_txs` col in the calldata part of tx table.
                    // Therefore we can skip assign any values to fixed part related cols
                    // (e.g. block_num, tx_type, is_padding_tx, ....). The witness assignment of
                    // calldata part need only make sure that (is_final,
                    // calldata_gas_cost_acc) are correctly assigned.
                    not::expr(meta.query_advice(is_calldata, Rotation::next())),
                    not::expr(block_num_unchanged.expr()),
                ]),
                |cb| {
                    cb.require_equal(
                        "total_l1_popped' = tx.is_l1_msg ? queue_index + 1 : total_l1_popped",
                        meta.query_advice(total_l1_popped_before, Rotation::next()),
                        select::expr(
                            meta.query_advice(is_l1_msg, Rotation::cur()),
                            meta.query_advice(queue_index, Rotation::cur()) + 1.expr(),
                            meta.query_advice(total_l1_popped_before, Rotation::cur()),
                        ),
                    );

                    // init new block's num_all_txs
                    // num_all_txs_acc' = is_l1_msg' ? queue_index' - total_l1_popped_before' + 1 :
                    // 1
                    cb.require_equal(
                        "init new block's num_all_txs",
                        meta.query_advice(num_all_txs_acc, Rotation::next()),
                        select::expr(
                            meta.query_advice(is_l1_msg, Rotation::next()),
                            meta.query_advice(tx_nonce, Rotation::next())
                                - meta.query_advice(total_l1_popped_before, Rotation::next())
                                + 1.expr(),
                            1.expr(),
                        ),
                    );
                },
            );

            // no constraints on last tx in the fixed part of tx table

            cb.gate(and::expr([
                meta.query_fixed(tx_table.q_enable, Rotation::cur()),
                // we are in the fixed part of tx table
                not::expr(meta.query_advice(is_calldata, Rotation::cur())),
                // calculate num_all_txs at tag = BlockNum row
                meta.query_advice(is_tag_block_num, Rotation::cur()),
            ]))
        });

        meta.lookup_any("num_all_txs in block table", |meta| {
            let is_tag_block_num = meta.query_advice(is_tag_block_num, Rotation::cur());
            let block_num = meta.query_advice(tx_table.value, Rotation::cur());
            let num_all_txs_acc = meta.query_advice(num_all_txs_acc, Rotation::cur());

            let input_expr = vec![NumAllTxs.expr(), block_num, num_all_txs_acc];
            let table_expr = block_table.table_exprs(meta);
            let condition = and::expr([
                is_tag_block_num,
                not::expr(block_num_unchanged.expr()), // the last tx in each block
                not::expr(meta.query_advice(is_padding_tx, Rotation::cur())),
            ]);

            input_expr
                .into_iter()
                .zip(table_expr.into_iter())
                .map(|(input, table)| (input * condition.clone(), table))
                .collect::<Vec<_>>()
        });

        ///////////////////////////////////////////////////////////////////////
        ///////  constraints on block_table's num_txs & num_cum_txs  //////////
        ///////////////////////////////////////////////////////////////////////
        meta.create_gate("is_padding_tx", |meta| {
            let is_tag_caller_addr = is_caller_addr(meta);
            let mut cb = BaseConstraintBuilder::default();

            // if tag == CallerAddress
            cb.condition(is_tag_caller_addr.expr(), |cb| {
                cb.require_equal(
                    "is_padding_tx = true if caller_address = 0",
                    meta.query_advice(is_padding_tx, Rotation::cur()),
                    value_is_zero.expr(Rotation::cur())(meta),
                );
            });
            cb.gate(meta.query_fixed(q_enable, Rotation::cur()))
        });

        // prev block's cum_num_txs < tx_id
        let tx_id_gt_prev_cnt = LtChip::configure(
            meta,
            |meta| meta.query_fixed(q_enable, Rotation::cur()),
            |meta| {
                let num_txs = meta.query_advice(num_txs, Rotation::cur());
                let cum_num_txs = meta.query_advice(cum_num_txs, Rotation::cur());

                cum_num_txs - num_txs
            },
            |meta| meta.query_advice(tx_table.tx_id, Rotation::cur()),
            u8_table.into(),
        );

        // last non-padding tx must have tx_id == cum_num_txs
        meta.create_gate(
            "last non-padding tx must have tx_id == cum_num_txs",
            |meta| {
                let mut cb = BaseConstraintBuilder::default();
                let is_tag_block_num = meta.query_advice(is_tag_block_num, Rotation::cur());
                let is_cur_tx_non_padding =
                    not::expr(meta.query_advice(is_padding_tx, Rotation::cur()));
                let is_next_tx_padding = meta.query_advice(is_padding_tx, Rotation::next());
                let cum_num_txs = meta.query_advice(cum_num_txs, Rotation::cur());
                let tx_id = meta.query_advice(tx_table.tx_id, Rotation::cur());

                // tag == BlockNum && cur tx is the last non-padding tx
                cb.condition(
                    and::expr([is_tag_block_num, is_cur_tx_non_padding, is_next_tx_padding]),
                    |cb| {
                        cb.require_equal("tx_id == cum_num_txs", tx_id, cum_num_txs);
                    },
                );

                cb.gate(meta.query_fixed(tx_table.q_enable, Rotation::cur()))
            },
        );

        // tx_id <= cum_num_txs
        let tx_id_cmp_cum_num_txs = ComparatorChip::configure(
            meta,
            |meta| meta.query_fixed(q_enable, Rotation::cur()),
            |meta| meta.query_advice(tx_table.tx_id, Rotation::cur()),
            |meta| meta.query_advice(cum_num_txs, Rotation::cur()),
            u8_table.into(),
        );

        meta.create_gate("tx_id <= cum_num_txs", |meta| {
            let mut cb = BaseConstraintBuilder::default();

            let (lt_expr, eq_expr) = tx_id_cmp_cum_num_txs.expr(meta, None);
            cb.condition(is_block_num(meta), |cb| {
                cb.require_equal("lt or eq", sum::expr([lt_expr, eq_expr]), true.expr());
            });

            cb.gate(and::expr([
                meta.query_fixed(q_enable, Rotation::cur()),
                not::expr(meta.query_advice(is_padding_tx, Rotation::cur())),
            ]))
        });

        meta.lookup_any("num_txs in block table", |meta| {
            let is_tag_block_num = meta.query_advice(is_tag_block_num, Rotation::cur());
            let block_num = meta.query_advice(tx_table.value, Rotation::cur());
            let num_txs = meta.query_advice(num_txs, Rotation::cur());

            let input_expr = vec![NumTxs.expr(), block_num, num_txs];
            let table_expr = block_table.table_exprs(meta);
            let condition = and::expr([
                is_tag_block_num,
                not::expr(meta.query_advice(is_padding_tx, Rotation::cur())),
                meta.query_fixed(q_enable, Rotation::cur()),
            ]);

            input_expr
                .into_iter()
                .zip(table_expr.into_iter())
                .map(|(input, table)| (input * condition.clone(), table))
                .collect::<Vec<_>>()
        });

        meta.lookup_any("cum_num_txs in block table", |meta| {
            let is_tag_block_num = meta.query_advice(is_tag_block_num, Rotation::cur());
            let block_num = meta.query_advice(tx_table.value, Rotation::cur());
            let cum_num_txs = meta.query_advice(cum_num_txs, Rotation::cur());

            let input_expr = vec![CumNumTxs.expr(), block_num, cum_num_txs];
            let table_expr = block_table.table_exprs(meta);
            let condition = and::expr([
                is_tag_block_num,
                not::expr(meta.query_advice(is_padding_tx, Rotation::cur())),
                meta.query_fixed(q_enable, Rotation::cur()),
            ]);

            input_expr
                .into_iter()
                .zip(table_expr.into_iter())
                .map(|(input, table)| (input * condition.clone(), table))
                .collect::<Vec<_>>()
        });

        ////////////////////////////////////////////////////////////////////////
        ///////////  CallData length and gas_cost calculation  /////////////////
        ////////////////////////////////////////////////////////////////////////
        meta.lookup("tx_id_diff must in u16", |meta| {
            let q_enable = meta.query_fixed(q_enable, Rotation::next());
            let is_calldata = meta.query_advice(is_calldata, Rotation::cur());
            let tx_id = meta.query_advice(tx_table.tx_id, Rotation::cur());
            let tx_id_next = meta.query_advice(tx_table.tx_id, Rotation::next());
            let tx_id_next_is_zero = tx_id_is_zero.expr(Rotation::next())(meta);

            let lookup_condition =
                and::expr([q_enable, is_calldata, not::expr(tx_id_next_is_zero)]);

            vec![(lookup_condition * (tx_id_next - tx_id), u16_table.into())]
        });

        meta.create_gate("last row of call data", |meta| {
            let q_calldata_last = meta.query_fixed(q_calldata_last, Rotation::cur());
            let is_final = meta.query_advice(is_final, Rotation::cur());

            vec![(q_calldata_last * (is_final - true.expr()))]
        });
        meta.create_gate("calldata_byte == tx_table.value", |meta| {
            let mut cb = BaseConstraintBuilder::default();
            let is_calldata = meta.query_advice(is_calldata, Rotation::cur());

            cb.condition(is_calldata, |cb| {
                cb.require_equal(
                    "calldata_byte == tx_table.value",
                    meta.query_advice(calldata_byte, Rotation::cur()),
                    meta.query_advice(tx_table.value, Rotation::cur()),
                );
            });

            cb.gate(meta.query_fixed(tx_table.q_enable, Rotation::cur()))
        });

        meta.create_gate("tx call data init", |meta| {
            let mut cb = BaseConstraintBuilder::default();

            let value_is_zero = value_is_zero.expr(Rotation::cur())(meta);
            let gas_cost = select::expr(value_is_zero, 4.expr(), 16.expr());

            cb.require_equal(
                "index == 0",
                meta.query_advice(tx_table.index, Rotation::cur()),
                0.expr(),
            );
            cb.require_equal(
                "calldata_gas_cost_acc == gas_cost",
                meta.query_advice(calldata_gas_cost_acc, Rotation::cur()),
                gas_cost,
            );
            cb.require_equal(
                "calldata_rlc == byte",
                meta.query_advice(calldata_rlc, Rotation::cur()),
                meta.query_advice(tx_table.value, Rotation::cur()),
            );

            cb.gate(and::expr([
                meta.query_fixed(q_calldata_first, Rotation::cur()),
                not::expr(tx_id_is_zero.expr(Rotation::cur())(meta)),
            ]))
        });

        meta.create_gate("tx call data bytes", |meta| {
            let mut cb = BaseConstraintBuilder::default();

            let is_final_cur = meta.query_advice(is_final, Rotation::cur());
            cb.require_boolean("is_final is boolean", is_final_cur.clone());

            // checks for any row, except the final call data byte.
            cb.condition(not::expr(is_final_cur.clone()), |cb| {
                cb.require_equal(
                    "index::next == index::cur + 1",
                    meta.query_advice(tx_table.index, Rotation::next()),
                    meta.query_advice(tx_table.index, Rotation::cur()) + 1.expr(),
                );
                cb.require_equal(
                    "tx_id::next == tx_id::cur",
                    tx_id_unchanged.is_equal_expression.clone(),
                    1.expr(),
                );

                let value_next_is_zero = value_is_zero.expr(Rotation::next())(meta);
                let gas_cost_next = select::expr(value_next_is_zero, 4.expr(), 16.expr());
                // call data gas cost accumulator check.
                cb.require_equal(
                    "calldata_gas_cost_acc::next == calldata_gas_cost::cur + gas_cost_next",
                    meta.query_advice(calldata_gas_cost_acc, Rotation::next()),
                    meta.query_advice(calldata_gas_cost_acc, Rotation::cur()) + gas_cost_next,
                );
                cb.require_equal(
                    "calldata_rlc' = calldata_rlc * r + byte'",
                    meta.query_advice(calldata_rlc, Rotation::next()),
                    meta.query_advice(calldata_rlc, Rotation::cur()) * challenges.keccak_input()
                        + meta.query_advice(tx_table.value, Rotation::next()),
                );
            });

            // on the final call data byte, tx_id must change.
            cb.condition(is_final_cur.expr(), |cb| {
                cb.require_zero(
                    "tx_id changes at is_final == 1",
                    tx_id_unchanged.is_equal_expression.clone(),
                );
            });

            cb.condition(
                and::expr([
                    is_final_cur,
                    not::expr(tx_id_is_zero.expr(Rotation::next())(meta)),
                ]),
                |cb| {
                    let value_next_is_zero = value_is_zero.expr(Rotation::next())(meta);
                    let gas_cost_next = select::expr(value_next_is_zero, 4.expr(), 16.expr());

                    cb.require_equal(
                        "index' == 0",
                        meta.query_advice(tx_table.index, Rotation::next()),
                        0.expr(),
                    );
                    cb.require_equal(
                        "calldata_gas_cost_acc' == gas_cost_next",
                        meta.query_advice(calldata_gas_cost_acc, Rotation::next()),
                        gas_cost_next,
                    );
                    cb.require_equal(
                        "calldata_rlc' == byte'",
                        meta.query_advice(calldata_rlc, Rotation::next()),
                        meta.query_advice(tx_table.value, Rotation::next()),
                    );
                },
            );

            cb.gate(and::expr(vec![
                meta.query_fixed(q_enable, Rotation::cur()),
                meta.query_advice(is_calldata, Rotation::cur()),
                not::expr(tx_id_is_zero.expr(Rotation::cur())(meta)),
            ]))
        });

        ////////////////////////////////////////////////////////////////////////
        ///////////   SignVerify recover CallerAddress    //////////////////////
        ////////////////////////////////////////////////////////////////////////
        meta.create_gate("tx signature v", |meta| {
            let mut cb = BaseConstraintBuilder::default();
            let is_chain_id = meta.query_advice(is_chain_id, Rotation::cur());

            //  1. eip155 tx: v Є {chain_id*2 + 35, chain_id*2 + 36}
            cb.condition(
                and::expr([
                    is_chain_id.expr(),
                    tx_type_bits.value_equals(Eip155, Rotation::cur())(meta),
                ]),
                |cb| {
                    // we rely on the assumption that SigV is on the next of ChainID
                    let v = meta.query_advice(tx_table.value, Rotation::next());
                    let chain_id = meta.query_advice(tx_table.value, Rotation::cur());

                    cb.require_boolean(
                        "V - (chain_id * 2 + 35) Є {0, 1}",
                        v - chain_id * 2.expr() - 35.expr(),
                    );
                },
            );

            //  2. pre-eip155 tx: v Є {27, 28}
            cb.condition(
                and::expr([
                    is_chain_id.expr(),
                    tx_type_bits.value_equals(PreEip155, Rotation::cur())(meta),
                ]),
                |cb| {
                    let v = meta.query_advice(tx_table.value, Rotation::next());
                    cb.require_boolean("V - 27 Є {0, 1}", v - 27.expr());
                },
            );

            //  3. l1 msg: v == 0
            cb.condition(
                and::expr([
                    is_chain_id.expr(),
                    tx_type_bits.value_equals(L1Msg, Rotation::cur())(meta),
                ]),
                |cb| {
                    let v = meta.query_advice(tx_table.value, Rotation::next());
                    cb.require_zero("V == 0", v);
                },
            );

            // TODO:
            //  4. eip1559 tx: v Є {0, 1}
            //  5. eip2930 tx: v Є {0, 1}

            cb.gate(meta.query_fixed(q_enable, Rotation::cur()))
        });

        meta.create_gate(
            "caller address == sv_address if it's not zero and tx_type != L1Msg",
            |meta| {
                let mut cb = BaseConstraintBuilder::default();

                cb.condition(not::expr(value_is_zero.expr(Rotation::cur())(meta)), |cb| {
                    cb.require_equal(
                        "caller address == sv_address",
                        meta.query_advice(tx_table.value, Rotation::cur()),
                        meta.query_advice(sv_address, Rotation::cur()),
                    );
                });

                cb.gate(and::expr([
                    meta.query_fixed(q_enable, Rotation::cur()),
                    meta.query_advice(is_caller_address, Rotation::cur()),
                    not::expr(meta.query_advice(is_l1_msg, Rotation::cur())),
                ]))
            },
        );

        log_deg("tx_circuit", meta);

        Self {
            minimum_rows: meta.minimum_rows(),
            q_first,
            q_calldata_first,
            q_calldata_last,
            tx_tag_bits: tag_bits,
            tx_type,
            tx_type_bits,
            rlp_tag,
            is_none,
            tx_value_rlc,
            tx_value_length,
            u8_table,
            u16_table,
            tx_id_is_zero,
            value_is_zero,
            tx_id_unchanged,
            is_calldata,
            is_caller_address,
            tx_id_cmp_cum_num_txs,
            tx_id_gt_prev_cnt,
            cum_num_txs,
            is_padding_tx,
            lookup_conditions,
            tx_nonce,
            block_num,
            block_num_unchanged,
            num_all_txs_acc,
            total_l1_popped_before,
            is_l1_msg,
            is_chain_id,
            is_final,
            calldata_gas_cost_acc,
            calldata_rlc,
            calldata_byte,
            sv_address,
            sig_table,
            block_table,
            tx_table,
            keccak_table,
            rlp_table,
            is_tag_block_num,
            _marker: PhantomData,
            num_txs,
        }
    }
}

impl<F: Field> TxCircuitConfig<F> {
    #[allow(clippy::too_many_arguments)]
    fn configure_lookups(
        meta: &mut ConstraintSystem<F>,
        q_enable: Column<Fixed>,
        rlp_tag: Column<Advice>,
        tx_value_rlc: Column<Advice>,
        tx_value_length: Column<Advice>,
        tx_type_bits: BinaryNumberConfig<TxType, 3>,
        tx_id_is_zero: IsZeroConfig<F>,
        is_none: Column<Advice>,
        lookup_conditions: &HashMap<LookupCondition, Column<Advice>>,
        is_final: Column<Advice>,
        is_calldata: Column<Advice>,
        is_chain_id: Column<Advice>,
        is_l1_msg_col: Column<Advice>,
        sv_address: Column<Advice>,
        calldata_gas_cost_acc: Column<Advice>,
        calldata_rlc: Column<Advice>,
        tx_table: TxTable,
        keccak_table: KeccakTable,
        rlp_table: RlpTable,
        sig_table: SigTable,
    ) {
        macro_rules! is_tx_type {
            ($var:ident, $type_variant:ident) => {
                let $var = |meta: &mut VirtualCells<F>| {
                    tx_type_bits.value_equals(TxType::$type_variant, Rotation::cur())(meta)
                };
            };
        }
        /////////////////////////////////////////////////////////////////
        /////////////////    block table lookups     ////////////////////
        ///////////////// ////////////////////////////////////////////////

        /////////////////////////////////////////////////////////////////
        /////////////////    tx table lookups     ///////////////////////
        /////////////////////////////////////////////////////////////////
        // lookup to check CallDataGasCost of the tx's call data.
        meta.lookup_any("tx call data gas cost in TxTable", |meta| {
            // if call data length != 0, then we can lookup the calldata gas cost on the
            // last row of the tx's call data bytes.
            let enable = and::expr(vec![
                meta.query_fixed(q_enable, Rotation::cur()),
                meta.query_advice(
                    lookup_conditions[&LookupCondition::TxCalldata],
                    Rotation::cur(),
                ),
            ]);

            vec![
                meta.query_advice(tx_table.tx_id, Rotation::cur()),
                CallData.expr(),
                meta.query_advice(tx_table.value, Rotation::next()), // calldata_gas_cost
                1.expr(),                                            // is_final = 1
            ]
            .into_iter()
            .zip(
                vec![
                    meta.query_advice(tx_table.tx_id, Rotation::cur()),
                    meta.query_fixed(tx_table.tag, Rotation::cur()),
                    meta.query_advice(calldata_gas_cost_acc, Rotation::cur()),
                    meta.query_advice(is_final, Rotation::cur()),
                ]
                .into_iter(),
            )
            .map(|(arg, table)| (enable.clone() * arg, table))
            .collect()
        });
        // We need to handle the case in which some of the call data bytes is skipped in
        // the tx table. If the call data length is larger than 0, then we will
        // do lookup in the tx table to make sure the last call data byte in tx
        // has index = call_data_length-1.
        meta.lookup_any("is_final call data byte should be present", |meta| {
            let enable = and::expr(vec![
                meta.query_fixed(q_enable, Rotation::cur()),
                meta.query_advice(
                    lookup_conditions[&LookupCondition::TxCalldata],
                    Rotation::cur(),
                ),
            ]);
            vec![
                meta.query_advice(tx_table.tx_id, Rotation::cur()),
                CallData.expr(),
                meta.query_advice(tx_table.value, Rotation::cur()) - 1.expr(), /* index starts
                                                                                * from 0 */
                1.expr(), // is_final = true
            ]
            .into_iter()
            .zip(
                vec![
                    meta.query_advice(tx_table.tx_id, Rotation::cur()),
                    meta.query_fixed(tx_table.tag, Rotation::cur()),
                    meta.query_advice(tx_table.index, Rotation::cur()),
                    meta.query_advice(is_final, Rotation::cur()),
                ]
                .into_iter(),
            )
            .map(|(arg, table)| (enable.clone() * arg, table))
            .collect()
        });
        meta.lookup_any("lookup CallDataRLC in the calldata part", |meta| {
            let is_call_data = meta.query_advice(is_calldata, Rotation::cur());
            let calldata_rlc = meta.query_advice(calldata_rlc, Rotation::cur());
            let enable = and::expr([
                meta.query_fixed(tx_table.q_enable, Rotation::cur()),
                is_call_data,
                not::expr(tx_id_is_zero.expr(Rotation::cur())(meta)),
                meta.query_advice(is_final, Rotation::cur()),
            ]);

            let input_exprs = vec![
                meta.query_advice(tx_table.tx_id, Rotation::cur()),
                CallDataRLC.expr(),
                calldata_rlc.expr(),
            ];
            let table_exprs = vec![
                meta.query_advice(tx_table.tx_id, Rotation::cur()),
                meta.query_fixed(tx_table.tag, Rotation::cur()),
                meta.query_advice(tx_table.value, Rotation::cur()),
            ];

            input_exprs
                .into_iter()
                .zip(table_exprs.into_iter())
                .map(|(input, table)| (input * enable.expr(), table))
                .collect()
        });

        /////////////////////////////////////////////////////////////////
        /////////////////    RLP table lookups     //////////////////////
        ///////////////// ////////////////////////////////////////////////
        is_tx_type!(is_pre_eip155, PreEip155);
        is_tx_type!(is_eip155, Eip155);
        is_tx_type!(is_l1_msg, L1Msg);

        // lookup tx type in RLP table for L1Msg only
        meta.lookup_any("lookup tx type in RLP table", |meta| {
            let enable = and::expr([meta.query_fixed(q_enable, Rotation::cur()), is_l1_msg(meta)]);
            let hash_format = L1MsgHash.expr();
            let tag_value = 0x7E.expr();
            let tag_bytes_rlc = 0x7E.expr();
            let tag_length = 1.expr();

            let input_exprs = vec![
                1.expr(), // q_enable = true
                meta.query_advice(tx_table.tx_id, Rotation::cur()),
                hash_format,
                RLPTxType.expr(),
                tag_value,
                tag_bytes_rlc,
                tag_length,
                1.expr(), // is_output = true
                0.expr(), // is_none = false
            ];
            assert_eq!(input_exprs.len(), rlp_table.table_exprs(meta).len());

            input_exprs
                .into_iter()
                .zip(rlp_table.table_exprs(meta).into_iter())
                .map(|(input, table)| (enable.expr() * input, table))
                .collect()
        });

        // lookup tx tag in RLP table for signing.
        meta.lookup_any("lookup tx tag in RLP Table for signing", |meta| {
            let enable = and::expr([
                meta.query_fixed(q_enable, Rotation::cur()),
                meta.query_advice(
                    lookup_conditions[&LookupCondition::RlpSignTag],
                    Rotation::cur(),
                ),
            ]);
            let rlp_tag = meta.query_advice(rlp_tag, Rotation::cur());
            let is_none = meta.query_advice(is_none, Rotation::cur());
            let sign_format = is_pre_eip155(meta) * TxSignPreEip155.expr()
                + is_eip155(meta) * TxSignEip155.expr();

            // q_enable, tx_id, format, rlp_tag, tag_value, is_output, is_none
            vec![
                1.expr(), // q_enable = true
                meta.query_advice(tx_table.tx_id, Rotation::cur()),
                sign_format,
                rlp_tag,
                meta.query_advice(tx_table.value, Rotation::cur()),
                meta.query_advice(tx_value_rlc, Rotation::cur()),
                meta.query_advice(tx_value_length, Rotation::cur()),
                1.expr(), // is_output = true
                is_none,
            ]
            .into_iter()
            .zip_eq(rlp_table.table_exprs(meta).into_iter())
            .map(|(arg, table)| (enable.clone() * arg, table))
            .collect()
        });

        // lookup tx tag in RLP table for hashing
        meta.lookup_any("lookup tx tag in RLP Table for hashing", |meta| {
            let rlp_tag = meta.query_advice(rlp_tag, Rotation::cur());
            let enable = and::expr(vec![
                meta.query_fixed(q_enable, Rotation::cur()),
                sum::expr([
                    meta.query_advice(
                        lookup_conditions[&LookupCondition::RlpHashTag],
                        Rotation::cur(),
                    ),
                    meta.query_advice(
                        lookup_conditions[&LookupCondition::L1MsgHash],
                        Rotation::cur(),
                    ),
                ]),
            ]);
            let is_none = meta.query_advice(is_none, Rotation::cur());
            let hash_format = is_pre_eip155(meta) * TxHashPreEip155.expr()
                + is_eip155(meta) * TxHashEip155.expr()
                + is_l1_msg(meta) * L1MsgHash.expr();

            vec![
                1.expr(), // q_enable = true
                meta.query_advice(tx_table.tx_id, Rotation::cur()),
                hash_format,
                rlp_tag,
                meta.query_advice(tx_table.value, Rotation::cur()),
                meta.query_advice(tx_value_rlc, Rotation::cur()),
                meta.query_advice(tx_value_length, Rotation::cur()),
                1.expr(), // is_output = true
                is_none,
            ]
            .into_iter()
            .zip_eq(rlp_table.table_exprs(meta).into_iter())
            .map(|(arg, table)| (enable.clone() * arg, table))
            .collect()
        });

        ////////////////////////////////////////////////////////////////////
        /////////////////    Sig table lookups     //////////////////////
        ///////////////// //////////////////////////////////////////////////
        meta.lookup_any("Sig table lookup", |meta| {
            let enabled = and::expr([
                // use is_l1_msg_col instead of is_l1_msg(meta) because it has lower degree
                not::expr(meta.query_advice(is_l1_msg_col, Rotation::cur())),
                // lookup to sig table on the ChainID row because we have an indicator of degree 1
                // for ChainID and ChainID is not far from (msg_hash_rlc, sig_v,
                // ...)
                meta.query_advice(is_chain_id, Rotation::cur()),
            ]);

            let msg_hash_rlc = meta.query_advice(tx_table.value, Rotation(6));
            let chain_id = meta.query_advice(tx_table.value, Rotation::cur());
            let sig_v = meta.query_advice(tx_table.value, Rotation(1));
            let sig_r = meta.query_advice(tx_table.value, Rotation(2));
            let sig_s = meta.query_advice(tx_table.value, Rotation(3));
            let sv_address = meta.query_advice(sv_address, Rotation::cur());

            let v = is_eip155(meta) * (sig_v.expr() - 2.expr() * chain_id - 35.expr())
                + is_pre_eip155(meta) * (sig_v.expr() - 27.expr());

            let input_exprs = vec![
                1.expr(),     // q_enable = true
                msg_hash_rlc, // msg_hash_rlc
                v,            // sig_v
                sig_r,        // sig_r
                sig_s,        // sig_s
                sv_address,
                1.expr(), // is_valid
            ];

            // LookupTable::table_exprs is not used here since `is_valid` not used by evm circuit.
            let table_exprs = vec![
                meta.query_fixed(sig_table.q_enable, Rotation::cur()),
                // msg_hash_rlc not needed to be looked up for tx circuit?
                meta.query_advice(sig_table.msg_hash_rlc, Rotation::cur()),
                meta.query_advice(sig_table.sig_v, Rotation::cur()),
                meta.query_advice(sig_table.sig_r_rlc, Rotation::cur()),
                meta.query_advice(sig_table.sig_s_rlc, Rotation::cur()),
                meta.query_advice(sig_table.recovered_addr, Rotation::cur()),
                meta.query_advice(sig_table.is_valid, Rotation::cur()),
            ];

            input_exprs
                .into_iter()
                .zip(table_exprs.into_iter())
                .map(|(input, table)| (input * enabled.expr(), table))
                .collect()
        });

        ////////////////////////////////////////////////////////////////////
        /////////////////    Keccak table lookups     //////////////////////
        ///////////////// //////////////////////////////////////////////////
        // lookup Keccak table for tx sign data hash, i.e. the sighash that has to be
        // signed.
        // lookup Keccak table for tx hash too.
        meta.lookup_any("Keccak table lookup for TxSign and TxHash", |meta| {
            let enable = and::expr(vec![
                meta.query_fixed(q_enable, Rotation::cur()),
                meta.query_advice(lookup_conditions[&LookupCondition::Keccak], Rotation::cur()),
            ]);

            vec![
                1.expr(),                                            // q_enable
                1.expr(),                                            // is_final
                meta.query_advice(tx_table.value, Rotation::next()), // input_rlc
                meta.query_advice(tx_table.value, Rotation::cur()),  // input_len
                meta.query_advice(tx_table.value, Rotation(2)),      // output_rlc
            ]
            .into_iter()
            .zip(keccak_table.table_exprs(meta).into_iter())
            .map(|(arg, table)| (enable.clone() * arg, table))
            .collect()
        });
    }

    /// Assigns a tx circuit row and returns the assigned cell of the value in
    /// the row.
    #[allow(clippy::too_many_arguments)]
    fn assign_row(
        &self,
        region: &mut Region<'_, F>,
        offset: &mut usize,
        tx: Option<&Transaction>,
        tx_id: usize,
        tx_id_next: usize,
        tag: TxFieldTag,
        value: Value<F>,
        be_bytes_rlc: Value<F>,
        be_bytes_length: Option<u32>,
        rlp_tag: Option<RlpTag>,
        is_none: Option<bool>,
        is_padding_tx: Option<bool>,
        cum_num_txs: Option<usize>,
        num_txs: Option<usize>,
        is_final: Option<bool>,
        calldata_gas_cost_acc: Option<u64>,
        calldata_rlc: Option<Value<F>>,
        cur_block_num: Option<u64>,
        next_block_number: Option<u64>,
        num_all_txs_acc: Option<u64>,
        total_l1_popped_before: Option<u64>,
    ) -> Result<(), Error> {
        // assign to tag, rlp_tag, is_none
        let tag_chip = BinaryNumberChip::construct(self.tx_tag_bits);
        tag_chip.assign(region, *offset, &tag)?;
        let tx_type = tx.map_or(Default::default(), |tx| tx.tx_type);
        let tx_type_chip = BinaryNumberChip::construct(self.tx_type_bits);
        tx_type_chip.assign(region, *offset, &tx_type)?;

        region.assign_advice(
            || "tx_type",
            self.tx_type,
            *offset,
            || Value::known(F::from(usize::from(tx_type) as u64)),
        )?;
        region.assign_advice(
            || "rlp tag",
            self.rlp_tag,
            *offset,
            || Value::known(F::from(usize::from(rlp_tag.unwrap_or(Null)) as u64)),
        )?;
        region.assign_advice(
            || "is_none",
            self.is_none,
            *offset,
            || Value::known(F::from(is_none.unwrap_or(false) as u64)),
        )?;
        region.assign_advice(
            || "value_be_bytes_rlc",
            self.tx_value_rlc,
            *offset,
            || be_bytes_rlc,
        )?;
        region.assign_advice(
            || "value_be_bytes_length",
            self.tx_value_length,
            *offset,
            || Value::known(F::from(be_bytes_length.unwrap_or(0) as u64)),
        )?;

        let block_num_unchanged_chip = IsEqualChip::construct(self.block_num_unchanged.clone());
        block_num_unchanged_chip.assign(
            region,
            *offset,
            Value::known(F::from(next_block_number.unwrap_or(0))),
            Value::known(F::from(cur_block_num.unwrap_or(0))),
        )?;
        region.assign_advice(
            || "tx_nonce",
            self.tx_nonce,
            *offset,
            || Value::known(F::from(tx.map_or(0, |tx| tx.nonce))),
        )?;
        region.assign_advice(
            || "block_num",
            self.block_num,
            *offset,
            || Value::known(F::from(cur_block_num.unwrap_or(0))),
        )?;
        region.assign_advice(
            || "num_all_txs_acc",
            self.num_all_txs_acc,
            *offset,
            || Value::known(F::from(num_all_txs_acc.unwrap_or(0))),
        )?;

        region.assign_advice(
            || "total_l1_popped_before",
            self.total_l1_popped_before,
            *offset,
            || Value::known(F::from(total_l1_popped_before.unwrap_or(0))),
        )?;

        // assign to lookup condition columns
        let is_l1_msg = tx.map(|tx| tx.tx_type.is_l1_msg()).unwrap_or(false);
        let mut conditions = HashMap::<LookupCondition, Value<F>>::new();
        if tag == CallData {
            conditions = vec![
                (LookupCondition::TxCalldata, Value::known(F::zero())),
                (LookupCondition::L1MsgHash, Value::known(F::zero())),
                (LookupCondition::RlpSignTag, Value::known(F::zero())),
                (LookupCondition::RlpHashTag, Value::known(F::zero())),
                (LookupCondition::Keccak, Value::known(F::zero())),
            ]
            .into_iter()
            .collect();
        } else {
            // lookup to Tx table for CallDataLength and CallDataGasCost
            conditions.insert(LookupCondition::TxCalldata, {
                let is_data_length = tag == CallDataLength;
                if is_data_length {
                    value.map(|value| F::from(!value.is_zero_vartime() as u64))
                } else {
                    Value::known(F::zero())
                }
            });
            // lookup to RLP table for signing (non L1 msg)
            conditions.insert(LookupCondition::RlpSignTag, {
                let sign_set = [
                    Nonce,
                    GasPrice,
                    Gas,
                    CalleeAddress,
                    TxFieldTag::Value,
                    CallDataRLC,
                    TxSignLength,
                    TxSignRLC,
                ];
                let is_tag_in_set = sign_set.into_iter().filter(|_tag| tag == *_tag).count() == 1;
                let case1 = is_tag_in_set && !is_l1_msg;
                let case2 = (tag == ChainID) && tx.map_or(false, |tx| tx.tx_type.is_eip155_tx());
                Value::known(F::from((case1 || case2) as u64))
            });
            // lookup to RLP table for hashing (non L1 msg)
            conditions.insert(LookupCondition::RlpHashTag, {
                let hash_set = [
                    Nonce,
                    GasPrice,
                    Gas,
                    CalleeAddress,
                    TxFieldTag::Value,
                    CallDataRLC,
                    TxDataGasCost,
                    SigV,
                    SigR,
                    SigS,
                    TxHashLength,
                    TxHashRLC,
                ];
                let is_tag_in_set = hash_set.into_iter().filter(|_tag| tag == *_tag).count() == 1;
                Value::known(F::from((!is_l1_msg && is_tag_in_set) as u64))
            });
            // lookup to RLP table for hashing (L1 msg)
            conditions.insert(LookupCondition::L1MsgHash, {
                let hash_set = [
                    Nonce,
                    Gas,
                    CalleeAddress,
                    TxFieldTag::Value,
                    CallDataRLC,
                    CallerAddress,
                    TxHashLength,
                    TxHashRLC,
                ];

                let is_tag_in_set = hash_set.into_iter().filter(|_tag| tag == *_tag).count() == 1;
                Value::known(F::from((is_l1_msg && is_tag_in_set) as u64))
            });
            // lookup to Keccak table for tx_sign_hash and tx_hash
            conditions.insert(LookupCondition::Keccak, {
                let case1 = (tag == TxSignLength) && !is_l1_msg;
                let case2 = tag == TxHashLength;
                Value::known(F::from((case1 || case2) as u64))
            });
        }
        for (condition, value) in conditions {
            region.assign_advice(
                || format!("lookup condition {condition:?}"),
                self.lookup_conditions[&condition],
                *offset,
                || value,
            )?;
        }

        // assign to columns which are used to reduce degree
        region.assign_advice(
            || "is_l1_msg",
            self.is_l1_msg,
            *offset,
            || Value::known(F::from(is_l1_msg as u64)),
        )?;
        region.assign_advice(
            || "is_tag_block_num",
            self.is_tag_block_num,
            *offset,
            || Value::known(F::from((tag == BlockNumber) as u64)),
        )?;
        region.assign_advice(
            || "is_chain_id",
            self.is_chain_id,
            *offset,
            || Value::known(F::from((tag == ChainID) as u64)),
        )?;
        region.assign_advice(
            || "is_caller_address",
            self.is_caller_address,
            *offset,
            || Value::known(F::from((tag == CallerAddress) as u64)),
        )?;
        region.assign_advice(
            || "is_calldata",
            self.is_calldata,
            *offset,
            || Value::known(F::from((tag == CallData) as u64)),
        )?;

        // assign to is_zero/is_equal chips
        let tx_id_is_zero_chip = IsZeroChip::construct(self.tx_id_is_zero.clone());
        tx_id_is_zero_chip.assign(region, *offset, Value::known(F::from(tx_id as u64)))?;

        let value_is_zero_chip = IsZeroChip::construct(self.value_is_zero.clone());
        value_is_zero_chip.assign(region, *offset, value)?;

        let tx_id_unchanged_chip = IsEqualChip::construct(self.tx_id_unchanged.clone());
        tx_id_unchanged_chip.assign(
            region,
            *offset,
            Value::known(F::from(tx_id as u64)),
            Value::known(F::from(tx_id_next as u64)),
        )?;

        // assign to call_data related columns
        region.assign_advice(
            || "is_final",
            self.is_final,
            *offset,
            || Value::known(F::from(is_final.unwrap_or(false) as u64)),
        )?;
        region.assign_advice(
            || "calldata_gas_cost_acc",
            self.calldata_gas_cost_acc,
            *offset,
            || Value::known(F::from(calldata_gas_cost_acc.unwrap_or_default())),
        )?;
        region.assign_advice(
            || "calldata_rlc",
            self.calldata_rlc,
            *offset,
            || calldata_rlc.unwrap_or(Value::known(F::zero())),
        )?;
        if tag == CallData {
            region.assign_advice(|| "calldata_byte", self.calldata_byte, *offset, || value)?;
        }

        // assign to
        region.assign_advice(
            || "is_padding_tx",
            self.is_padding_tx,
            *offset,
            || Value::known(F::from(is_padding_tx.unwrap_or(false) as u64)),
        )?;
        let tx_id_cmp_cum_num_txs = ComparatorChip::construct(self.tx_id_cmp_cum_num_txs.clone());
        tx_id_cmp_cum_num_txs.assign(
            region,
            *offset,
            F::from(tx_id as u64),
            F::from(cum_num_txs.unwrap_or_default() as u64),
        )?;
        let tx_id_gt_prev_cnt = LtChip::construct(self.tx_id_gt_prev_cnt);
        tx_id_gt_prev_cnt.assign(
            region,
            *offset,
            F::from((cum_num_txs.unwrap_or_default() - num_txs.unwrap_or_default()) as u64),
            F::from(tx_id as u64),
        )?;
        region.assign_advice(
            || "cum_num_txs",
            self.cum_num_txs,
            *offset,
            || Value::known(F::from(cum_num_txs.unwrap_or_default() as u64)),
        )?;
        region.assign_advice(
            || "num_txs",
            self.num_txs,
            *offset,
            || Value::known(F::from(num_txs.unwrap_or_default() as u64)),
        )?;

        *offset += 1;

        Ok(())
    }

    fn assign_calldata_zeros(
        &self,
        region: &mut Region<'_, F>,
        start: usize,
        end: usize,
    ) -> Result<(), Error> {
        // let rlp_data = F::from( as u64);
        let tag = F::from(CallData as u64);
        let tx_id_is_zero_chip = IsZeroChip::construct(self.tx_id_is_zero.clone());
        let value_is_zero_chip = IsZeroChip::construct(self.value_is_zero.clone());
        let tx_id_unchanged = IsEqualChip::construct(self.tx_id_unchanged.clone());
        let tag_chip = BinaryNumberChip::construct(self.tx_tag_bits);

        for offset in start..end {
            region.assign_fixed(
                || "q_enable",
                self.tx_table.q_enable,
                offset,
                || Value::known(F::one()),
            )?;
            region.assign_advice(
                || "rlp_tag",
                self.rlp_tag,
                offset,
                || Value::known(F::from(usize::from(Null) as u64)),
            )?;
            region.assign_fixed(|| "tag", self.tx_table.tag, offset, || Value::known(tag))?;
            tag_chip.assign(region, offset, &CallData)?;
            // no need to assign tx_id_is_zero_chip for real prover as tx_id = 0
            tx_id_is_zero_chip.assign(region, offset, Value::known(F::zero()))?;
            // no need to assign value_is_zero_chip for real prover as value = 0
            value_is_zero_chip.assign(region, offset, Value::known(F::zero()))?;
            tx_id_unchanged.assign(
                region,
                offset,
                Value::known(F::zero()),
                Value::known(F::zero()),
            )?;

            for (col, value) in [
                (self.tx_table.tx_id, F::zero()),
                (self.tx_table.index, F::zero()),
                (self.tx_table.value, F::zero()),
                (self.is_final, F::one()),
                (self.is_calldata, F::one()),
                (self.calldata_gas_cost_acc, F::zero()),
            ] {
                region.assign_advice(|| "", col, offset, || Value::known(value))?;
            }
            for col in self.lookup_conditions.values() {
                region.assign_advice(
                    || "lookup condition",
                    *col,
                    offset,
                    || Value::known(F::zero()),
                )?;
            }
        }

        Ok(())
    }

    fn assign_paddings(
        &self,
        region: &mut Region<'_, F>,
        start: usize,
        end: usize,
    ) -> Result<(), Error> {
        for offset in start..end {
            region.assign_fixed(
                || "tag",
                self.tx_table.tag,
                offset,
                || Value::known(F::from(TxFieldTag::Null as u64)),
            )?;
        }

        Ok(())
    }
}

/// Tx Circuit for verifying transaction signatures and tx table.
/// PI circuit ensures that each tx's hash in the tx table is
/// equal to the one in public input. Then we can use RLP circuit to decode each
/// tx field's value from RLP-encoded tx bytes.
#[derive(Clone, Default, Debug)]
pub struct TxCircuit<F: Field> {
    /// Max number of supported transactions
    pub max_txs: usize,
    /// Max number of supported calldata bytes
    pub max_calldata: usize,
    /// List of Transactions
    pub txs: Vec<Transaction>,
    /// Chain ID
    pub chain_id: u64,
    /// Start L1 Queue Index
    pub start_l1_queue_index: u64,
    /// Size
    pub size: usize,
    _marker: PhantomData<F>,
}

impl<F: Field> TxCircuit<F> {
    /// Return a new TxCircuit
    pub fn new(
        max_txs: usize,
        max_calldata: usize,
        chain_id: u64,
        start_l1_queue_index: u64,
        txs: Vec<Transaction>,
    ) -> Self {
        log::info!(
            "TxCircuit::new(max_txs = {}, max_calldata = {}, chain_id = {})",
            max_txs,
            max_calldata,
            chain_id
        );
        debug_assert!(txs.len() <= max_txs);

        TxCircuit::<F> {
            max_txs,
            max_calldata,
            txs,
            size: Self::min_num_rows(max_txs, max_calldata),
            chain_id,
            start_l1_queue_index,
            _marker: PhantomData::default(),
        }
    }

    /// Returned data contains both the tx hash and sig hash
    fn keccak_inputs(&self) -> Result<Vec<Vec<u8>>, Error> {
        let mut inputs = Vec::new();

        let padding_tx = {
            let mut tx = Transaction::dummy(self.chain_id);
            tx.id = self.txs.len() + 1;
            tx
        };
        let hash_datas = self
            .txs
            .iter()
            .chain(iter::once(&padding_tx))
            .map(|tx| tx.rlp_signed.clone())
            .collect::<Vec<Vec<u8>>>();
        inputs.extend_from_slice(&hash_datas);

        let sign_datas: Vec<SignData> = self
            .txs
            .iter()
            .chain(iter::once(&padding_tx))
            .enumerate()
            .map(|(_, tx)| {
                if tx.tx_type.is_l1_msg() {
                    Ok(SignData::default())
                } else {
                    tx.sign_data().map_err(|e| {
                        error!("keccak_inputs_tx_circuit error: {:?}", e);
                        Error::Synthesis
                    })
                }
            })
            .collect::<Result<Vec<SignData>, Error>>()?;
        // Keccak inputs from SignVerify Chip
        let sign_verify_inputs = keccak_inputs_sign_verify(&sign_datas);
        inputs.extend_from_slice(&sign_verify_inputs);

        Ok(inputs)
    }

    /// Return the minimum number of rows required to prove an input of a
    /// particular size.
    pub fn min_num_rows(txs_len: usize, call_data_len: usize) -> usize {
        txs_len * TX_LEN + call_data_len
    }

    // assign num_txs, cum_num_txs, num_all_txs only as we only lookup into
    // block table for these three fields and this is mainly used for unit-test
    fn assign_dev_block_table(
        &self,
        config: TxCircuitConfig<F>,
        layouter: &mut impl Layouter<F>,
    ) -> Result<(), Error> {
        let mut total_l1_popped_before = 0;
        let block_nums = self
            .txs
            .iter()
            .map(|tx| tx.block_number)
            .collect::<BTreeSet<u64>>();
        let mut num_txs_in_blocks = BTreeMap::new();
        let mut num_all_txs_in_blocks: BTreeMap<u64, u64> = BTreeMap::new();
        for tx in self.txs.iter() {
            if let Some(num_txs) = num_txs_in_blocks.get_mut(&tx.block_number) {
                *num_txs += 1;
            } else {
                num_txs_in_blocks.insert(tx.block_number, 1_usize);
            }

            if let Some(num_all_txs) = num_all_txs_in_blocks.get_mut(&tx.block_number) {
                if tx.tx_type.is_l1_msg() {
                    *num_all_txs += tx.nonce - total_l1_popped_before + 1;
                    total_l1_popped_before = tx.nonce + 1;
                } else {
                    *num_all_txs += 1;
                }
            } else {
                let num_all_txs = if tx.tx_type.is_l1_msg() {
                    tx.nonce - total_l1_popped_before + 1
                } else {
                    1
                };
                num_all_txs_in_blocks.insert(tx.block_number, num_all_txs);
            }
        }
        log::debug!("block_nums: {:?}", block_nums);
        log::debug!("num_all_txs: {:?}", num_all_txs_in_blocks);

        layouter.assign_region(
            || "dev block table",
            |mut region| {
                for (offset, (block_num, num_txs, cum_num_txs, num_all_txs)) in
                    iter::once((0, 0, 0, 0))
                        .chain(block_nums.iter().scan(0, |cum_num_txs, block_num| {
                            let num_txs = num_txs_in_blocks[block_num];
                            let num_all_txs = num_all_txs_in_blocks[block_num];
                            *cum_num_txs += num_txs;

                            Some((*block_num, num_txs, *cum_num_txs, num_all_txs))
                        }))
                        .enumerate()
                {
                    for (j, (tag, value)) in [
                        (NumTxs, num_txs as u64),
                        (CumNumTxs, cum_num_txs as u64),
                        (NumAllTxs, num_all_txs),
                    ]
                    .into_iter()
                    .enumerate()
                    {
                        let row = offset * 3 + j;
                        region.assign_fixed(
                            || "block_table.tag",
                            config.block_table.tag,
                            row,
                            || Value::known(F::from(tag as u64)),
                        )?;
                        region.assign_advice(
                            || "block_table.index",
                            config.block_table.index,
                            row,
                            || Value::known(F::from(block_num)),
                        )?;
                        region.assign_advice(
                            || "block_table.value",
                            config.block_table.value,
                            row,
                            || Value::known(F::from(value)),
                        )?;
                    }
                }
                Ok(())
            },
        )
    }

    fn assign(
        &self,
        config: &TxCircuitConfig<F>,
        challenges: &crate::util::Challenges<Value<F>>,
        layouter: &mut impl Layouter<F>,
        start_l1_queue_index: u64,
        sign_datas: Vec<SignData>,
        padding_txs: &[Transaction],
    ) -> Result<(), Error> {
        let last_off = layouter.assign_region(
            || "tx table aux",
            |mut region| {
                let mut offset = 0;

                let sigs = &sign_datas;

                debug_assert_eq!(padding_txs.len() + self.txs.len(), sigs.len());

                let mut cum_num_txs;
                let mut num_txs;
                let mut is_padding_tx;
                let mut num_all_txs_acc = 0;
                let mut total_l1_popped_before = start_l1_queue_index;
                let mut total_l1_popped_after = start_l1_queue_index;
                // Empty entry
                config.assign_row(
                    &mut region,
                    &mut offset,
                    None,
                    0,                         // tx_id
                    !sigs.is_empty() as usize, // tx_id_next
                    TxFieldTag::Null,
                    Value::known(F::zero()),
                    challenges.keccak_input().map(|_| F::zero()),
                    None,
                    None,
                    None,
                    None,
                    None,
                    None,
                    None,
                    None,
                    None,
                    None,
                    None,
                    None,
                    None,
                )?;

                region.assign_fixed(|| "q_first", config.q_first, 0, || Value::known(F::one()))?;
                let zero_rlc = challenges.keccak_input().map(|_| F::zero());

                // Assign all tx fields except for call data
                for (i, sign_data) in sigs.iter().enumerate() {
                    let tx = if i < self.txs.len() {
                        &self.txs[i]
                    } else {
                        &padding_txs[i - self.txs.len()]
                    };
                    let block_num = tx.block_number;
                    let rlp_unsigned_tx_be_bytes = tx.rlp_unsigned.clone();
                    let rlp_signed_tx_be_bytes = tx.rlp_signed.clone();
                    if i < self.txs.len() {
                        cum_num_txs = self
                            .txs
                            .iter()
                            .filter(|tx| tx.block_number <= block_num)
                            .count();
                        num_txs = self
                            .txs
                            .iter()
                            .filter(|tx| tx.block_number == block_num)
                            .count();
                        log::info!("num_txs: {}", num_txs);
                        is_padding_tx = false;
                        let mut init_new_block = |tx: &Transaction| {
                            if tx.tx_type.is_l1_msg() {
                                let queue_index = tx.nonce;
                                num_all_txs_acc = queue_index - total_l1_popped_before + 1;
                                total_l1_popped_after = queue_index + 1;
                            } else {
                                // next tx's total_l1_popped_before do not change
                                total_l1_popped_after = total_l1_popped_before;
                                num_all_txs_acc = 1;
                            }
                        };
                        // first tx of all or first tx of next block
                        if i == 0 || tx.block_number != self.txs[i - 1].block_number {
                            init_new_block(tx);
                        } else {
                            // same block
                            if tx.tx_type.is_l1_msg() {
                                let queue_index = tx.nonce;
                                num_all_txs_acc += queue_index - total_l1_popped_before + 1;
                                total_l1_popped_after = queue_index + 1;
                            } else {
                                // next tx's total_l1_popped_before do not change
                                total_l1_popped_after = total_l1_popped_before;
                                num_all_txs_acc += 1;
                            }
                        }
                    } else {
                        cum_num_txs = 0;
                        num_txs = 0;
                        is_padding_tx = true;
                        // padding_tx is an l2 tx
                        num_all_txs_acc = (i - self.txs.len() + 1) as u64;
                    }

                    let tx_sign_hash = {
                        challenges.evm_word().map(|rand| {
                            sign_data
                                .msg
                                .to_vec()
                                .into_iter()
                                .fold(F::zero(), |acc, byte| acc * rand + F::from(byte as u64))
                        })
                    };
                    log::debug!("calldata len: {}", tx.call_data.len());
                    for (tag, rlp_tag, is_none, be_bytes_rlc, be_bytes_length, value) in [
                        // need to be in same order as that tx table load function uses
                        (
                            Nonce,
                            Some(Tag::Nonce.into()), // lookup into RLP table
                            Some(tx.nonce == 0),
                            rlc_be_bytes(&tx.nonce.to_be_bytes(), challenges.keccak_input()),
                            Some(tx.nonce.tag_length()),
                            Value::known(F::from(tx.nonce)),
                        ),
                        (
                            GasPrice,
                            Some(Tag::GasPrice.into()),
                            Some(tx.gas_price.is_zero()),
                            rlc_be_bytes(&tx.gas_price.to_be_bytes(), challenges.keccak_input()),
                            Some(tx.gas_price.tag_length()),
                            challenges
                                .evm_word()
                                .map(|challenge| rlc(tx.gas_price.to_le_bytes(), challenge)),
                        ),
                        (
                            Gas,
                            Some(Tag::Gas.into()),
                            Some(tx.gas == 0),
                            rlc_be_bytes(&tx.gas.to_be_bytes(), challenges.keccak_input()),
                            Some(tx.gas.tag_length()),
                            Value::known(F::from(tx.gas)),
                        ),
                        (
                            CallerAddress,
                            Some(Tag::Sender.into()),
                            None,
                            rlc_be_bytes(
                                &tx.caller_address.to_fixed_bytes(),
                                challenges.keccak_input(),
                            ),
                            Some(tx.caller_address.tag_length()),
                            Value::known(tx.caller_address.to_scalar().expect("tx.from too big")),
                        ),
                        (
                            CalleeAddress,
                            Some(Tag::To.into()),
                            Some(tx.callee_address.is_none()),
                            rlc_be_bytes(
                                &tx.callee_address
                                    .map_or(vec![], |callee| callee.to_fixed_bytes().to_vec()),
                                challenges.keccak_input(),
                            ),
                            Some(tx.callee_address.tag_length()),
                            Value::known(
                                tx.callee_address
                                    .unwrap_or(Address::zero())
                                    .to_scalar()
                                    .expect("tx.to too big"),
                            ),
                        ),
                        (
                            IsCreate,
                            None, // do not lookup into RLP table
                            None,
                            zero_rlc,
                            None,
                            Value::known(F::from(tx.is_create as u64)),
                        ),
                        (
                            TxFieldTag::Value,
                            Some(Tag::Value.into()),
                            Some(tx.value.is_zero()),
                            rlc_be_bytes(&tx.value.to_be_bytes(), challenges.keccak_input()),
                            Some(tx.value.tag_length()),
                            challenges
                                .evm_word()
                                .map(|challenge| rlc(tx.value.to_le_bytes(), challenge)),
                        ),
                        (
                            CallDataRLC,
                            Some(Tag::Data.into()),
                            Some(tx.call_data.is_empty()),
                            rlc_be_bytes(&tx.call_data, challenges.keccak_input()),
                            Some(tx.call_data.tag_length()),
                            rlc_be_bytes(&tx.call_data, challenges.keccak_input()),
                        ),
                        (
                            CallDataLength,
                            None,
                            None,
                            zero_rlc,
                            None,
                            Value::known(F::from(tx.call_data.len() as u64)),
                        ),
                        (
                            CallDataGasCost,
                            None,
                            None,
                            zero_rlc,
                            None,
                            Value::known(F::from(tx.call_data_gas_cost)),
                        ),
                        (
                            TxDataGasCost,
                            Some(GasCost),
                            None,
                            zero_rlc,
                            None,
                            Value::known(F::from(tx.tx_data_gas_cost)),
                        ),
                        (
                            ChainID,
                            Some(Tag::ChainId.into()),
                            Some(tx.chain_id.is_zero()),
                            rlc_be_bytes(&tx.chain_id.to_be_bytes(), challenges.keccak_input()),
                            Some(tx.chain_id.tag_length()),
                            Value::known(F::from(tx.chain_id)),
                        ),
                        (
                            SigV,
                            Some(Tag::SigV.into()),
                            Some(tx.v.is_zero()),
                            rlc_be_bytes(&tx.v.to_be_bytes(), challenges.keccak_input()),
                            Some(tx.v.tag_length()),
                            Value::known(F::from(tx.v)),
                        ),
                        (
                            SigR,
                            Some(Tag::SigR.into()),
                            Some(tx.r.is_zero()),
                            rlc_be_bytes(&tx.r.to_be_bytes(), challenges.keccak_input()),
                            Some(tx.r.tag_length()),
                            challenges
                                .evm_word()
                                .map(|challenge| rlc(tx.r.to_le_bytes(), challenge)),
                        ),
                        (
                            SigS,
                            Some(Tag::SigS.into()),
                            Some(tx.s.is_zero()),
                            rlc_be_bytes(&tx.s.to_be_bytes(), challenges.keccak_input()),
                            Some(tx.s.tag_length()),
                            challenges
                                .evm_word()
                                .map(|challenge| rlc(tx.s.to_le_bytes(), challenge)),
                        ),
                        (
                            TxSignLength,
                            Some(Len),
                            Some(false),
                            zero_rlc,
                            Some(get_rlp_len_tag_length(&rlp_unsigned_tx_be_bytes)),
                            Value::known(F::from(rlp_unsigned_tx_be_bytes.len() as u64)),
                        ),
                        (
                            TxSignRLC,
                            Some(RLC),
                            Some(false),
                            zero_rlc,
                            None,
                            challenges.keccak_input().map(|rand| {
                                rlp_unsigned_tx_be_bytes
                                    .iter()
                                    .fold(F::zero(), |acc, byte| acc * rand + F::from(*byte as u64))
                            }),
                        ),
                        (TxSignHash, None, None, zero_rlc, None, tx_sign_hash),
                        (
                            TxHashLength,
                            Some(Len),
                            Some(false),
                            zero_rlc,
                            Some(get_rlp_len_tag_length(&rlp_signed_tx_be_bytes)),
                            Value::known(F::from(rlp_signed_tx_be_bytes.len() as u64)),
                        ),
                        (
                            TxHashRLC,
                            Some(RLC),
                            Some(false),
                            zero_rlc,
                            None,
                            challenges.keccak_input().map(|rand| {
                                rlp_signed_tx_be_bytes
                                    .iter()
                                    .fold(F::zero(), |acc, byte| acc * rand + F::from(*byte as u64))
                            }),
                        ),
                        (
                            TxFieldTag::TxHash,
                            None,
                            None,
                            zero_rlc,
                            None,
                            challenges.evm_word().map(|challenge| {
                                tx.hash
                                    .to_fixed_bytes()
                                    .into_iter()
                                    .fold(F::zero(), |acc, byte| {
                                        acc * challenge + F::from(byte as u64)
                                    })
                            }),
                        ),
                        (
                            TxFieldTag::TxType,
                            None,
                            None,
                            zero_rlc,
                            None,
                            Value::known(F::from(tx.tx_type as u64)),
                        ),
                        (
                            BlockNumber,
                            None,
                            None,
                            zero_rlc,
                            None,
                            Value::known(F::from(tx.block_number)),
                        ),
                    ] {
                        let (tx_id_next, cur_block_num, next_block_num) = match tag {
                            BlockNumber => {
                                log::debug!(
                                    "tx_id: {}, block_num: {}, num_all_txs_acc: {}",
                                    i,
                                    tx.block_number,
                                    num_all_txs_acc
                                );
                                if i == sigs.len() - 1 {
                                    (
                                        self.txs
                                            .iter()
                                            .enumerate()
                                            .find(|(_i, tx)| !tx.call_data.is_empty())
                                            .map(|(i, _tx)| i + 1)
                                            .unwrap_or_else(|| 0),
                                        tx.block_number,
                                        0,
                                    )
                                } else {
                                    // tx_id in tx table starts with 1
                                    if i + 1 >= self.txs.len() {
                                        (i + 2, tx.block_number, padding_txs[0].block_number)
                                    } else {
                                        (i + 2, tx.block_number, self.txs[i + 1].block_number)
                                    }
                                }
                            }
                            _ => (i + 1, tx.block_number, tx.block_number), /* tx_id in tx table
                                                                             * starts with 1 */
                        };
                        config.assign_row(
                            &mut region,
                            &mut offset,
                            Some(tx),
                            i + 1,      // tx_id
                            tx_id_next, // tx_id_next
                            tag,
                            value,
                            be_bytes_rlc,
                            be_bytes_length,
                            rlp_tag,
                            is_none,
                            Some(is_padding_tx),
                            Some(cum_num_txs),
                            Some(num_txs),
                            None,
                            None,
                            None,
                            Some(cur_block_num),
                            Some(next_block_num),
                            Some(num_all_txs_acc),
                            Some(total_l1_popped_before),
                        )?;
                        let sv_address: F = sign_data.get_addr().to_scalar().unwrap();
                        region.assign_advice(
                            || "sv_address",
                            config.sv_address,
                            offset - 1,
                            || Value::known(sv_address),
                        )?;
                    }
                    // set next tx's total_l1_popped_before
                    total_l1_popped_before = total_l1_popped_after;
                }

                log::debug!("assigning calldata, offset {}", offset);
                assert_eq!(offset, self.max_txs * TX_LEN + 1);

                // Assign call data
                let mut calldata_count = 0;
                for (i, tx) in self.txs.iter().enumerate() {
                    let mut calldata_gas_cost = 0;
                    let mut calldata_rlc = Value::known(F::zero());
                    let calldata_length = tx.call_data.len();
                    calldata_count += calldata_length;
                    for (index, byte) in tx.call_data.iter().enumerate() {
                        assert!(calldata_count < self.max_calldata);
                        let (tx_id_next, is_final) = if index == calldata_length - 1 {
                            if i == self.txs.len() - 1 {
                                (0, true)
                            } else {
                                (
                                    self.txs
                                        .iter()
                                        .enumerate()
                                        .skip(i + 1)
                                        .find(|(_, tx)| !tx.call_data.is_empty())
                                        .map(|(j, _)| j + 1)
                                        .unwrap_or_else(|| 0),
                                    true,
                                )
                            }
                        } else {
                            (i + 1, false)
                        };
                        calldata_gas_cost += if byte.is_zero() { 4 } else { 16 };
                        if i == 0 && index == 0 {
                            region.assign_fixed(
                                || "q_calldata_first",
                                config.q_calldata_first,
                                offset,
                                || Value::known(F::one()),
                            )?;
                        }
                        calldata_rlc = calldata_rlc
                            .zip(challenges.keccak_input())
                            .map(|(rlc, r)| rlc * r + F::from(*byte as u64));
                        config.assign_row(
                            &mut region,
                            &mut offset,
                            Some(tx),
                            i + 1,      // tx_id
                            tx_id_next, // tx_id_next
                            CallData,
                            Value::known(F::from(*byte as u64)),
                            zero_rlc,
                            None,
                            None,
                            None,
                            None,
                            None,
                            None,
                            Some(is_final),
                            Some(calldata_gas_cost),
                            Some(calldata_rlc),
                            None,
                            None,
                            None,
                            None,
                        )?;
                    }
                }

                assert!(calldata_count <= self.max_calldata);
                let q_calldata_last_offset = self.max_txs * TX_LEN + self.max_calldata;
                if offset == q_calldata_last_offset + 1 {
                    region.assign_fixed(
                        || "q_calldata_last",
                        config.q_calldata_last,
                        q_calldata_last_offset,
                        || Value::known(F::one()),
                    )?;
                }
                debug_assert_eq!(offset, self.max_txs * TX_LEN + 1 + calldata_count);

                Ok(offset)
            },
        )?;
        if last_off + config.minimum_rows > self.size {
            log::error!(
                "circuit size not enough, last offset {}, minimum_rows {}, self.size {}",
                last_off,
                config.minimum_rows,
                self.size
            );
        }
        layouter.assign_region(
            || "tx table (calldata zeros and paddings)",
            |mut region| {
                if last_off == self.max_txs * TX_LEN + 1 {
                    // The txs do not have any call data bytes
                    // Therefore q_calldata_first is not assigned in prev region.
                    region.assign_fixed(
                        || "q_calldata_first",
                        config.q_calldata_first,
                        0,
                        || Value::known(F::one()),
                    )?;
                }
                if last_off < self.max_txs * TX_LEN + 1 + self.max_calldata {
                    let calldata_count = last_off - self.max_txs * TX_LEN - 1;
                    region.assign_fixed(
                        || "q_calldata_last",
                        config.q_calldata_last,
                        self.max_calldata - calldata_count - 1,
                        || Value::known(F::one()),
                    )?;
                }
                config.assign_calldata_zeros(
                    &mut region,
                    0,
                    self.max_calldata + self.max_txs * TX_LEN + 1 - last_off,
                )?;
                config.assign_paddings(
                    &mut region,
                    self.max_calldata + self.max_txs * TX_LEN + 1 - last_off,
                    self.size - config.minimum_rows - last_off,
                )?;

                Ok(())
            },
        )
    }
}

impl<F: Field> SubCircuit<F> for TxCircuit<F> {
    type Config = TxCircuitConfig<F>;

    fn unusable_rows() -> usize {
        8
    }

    fn new_from_block(block: &witness::Block<F>) -> Self {
        for tx in &block.txs {
            if tx.chain_id != block.chain_id {
                panic!(
                    "inconsistent chain id, block chain id {}, tx {:?}",
                    block.chain_id, tx.chain_id
                );
            }
        }
        Self::new(
            block.circuits_params.max_txs,
            block.circuits_params.max_calldata,
            block.chain_id,
            block.start_l1_queue_index,
            block.txs.clone(),
        )
    }

    /// Return the minimum number of rows required to prove the block
    fn min_num_rows_block(block: &witness::Block<F>) -> (usize, usize) {
        (
            Self::min_num_rows(
                block.txs.len(),
                block.txs.iter().map(|tx| tx.call_data.len()).sum(),
            ),
            Self::min_num_rows(
                block.circuits_params.max_txs,
                block.circuits_params.max_calldata,
            ),
        )
    }

    /// Make the assignments to the TxCircuit
    fn synthesize_sub(
        &self,
        config: &Self::Config,
        challenges: &crate::util::Challenges<Value<F>>,
        layouter: &mut impl Layouter<F>,
    ) -> Result<(), Error> {
        assert!(self.txs.len() <= self.max_txs);

        let padding_txs = (self.txs.len()..self.max_txs)
            .into_iter()
            .map(|i| {
                let mut tx = Transaction::dummy(self.chain_id);
                tx.id = i + 1;
                tx
            })
            .collect::<Vec<Transaction>>();
        let sign_datas: Vec<SignData> = self
            .txs
            .iter()
            .chain(padding_txs.iter())
            .map(|tx| {
                if tx.tx_type.is_l1_msg() {
                    Ok(SignData::default())
                } else {
                    tx.sign_data().map_err(|e| {
                        error!("tx_to_sign_data error for tx {:?}", e);
                        Error::Synthesis
                    })
                }
            })
            .collect::<Result<Vec<SignData>, Error>>()?;

        // check if tx.caller_address == recovered_pk
        let recovered_pks = keccak_inputs_sign_verify(&sign_datas)
            .into_iter()
            .enumerate()
            .filter(|(idx, _)| {
                // each sign_data produce two inputs for hashing
                // pk -> pk_hash, msg -> msg_hash
                idx % 2 == 0
            })
            .map(|(_, input)| input)
            .collect::<Vec<_>>();

        for (pk, tx) in recovered_pks.into_iter().zip(self.txs.iter()) {
            let pk_hash = keccak(&pk);
            let address = pk_hash.to_address();
            // L1 Msg does not have signature
            if !tx.tx_type.is_l1_msg() && address != tx.caller_address {
                log::error!(
                    "pk address from sign data {:?} does not match the one from tx address {:?}",
                    address,
                    tx.caller_address
                )
            }
        }

        self.assign(
            config,
            challenges,
            layouter,
            self.start_l1_queue_index,
            sign_datas,
            &padding_txs,
        )?;

        Ok(())
    }
}

pub(crate) fn get_sign_data(
    txs: &[Transaction],
    max_txs: usize,
    chain_id: usize,
) -> Result<Vec<SignData>, halo2_proofs::plonk::Error> {
    let padding_txs = (txs.len()..max_txs)
        .into_iter()
        .map(|i| {
            let mut tx = Transaction::dummy(chain_id as u64);
            tx.id = i + 1;
            tx
        })
        .collect::<Vec<Transaction>>();
    let signatures: Vec<SignData> = txs
        .iter()
        .chain(padding_txs.iter())
        .map(|tx| {
            if tx.tx_type.is_l1_msg() {
                // dummy signature
                Ok(SignData::default())
            } else {
                // TODO: map err or still use bus_mapping::err?
                tx.sign_data().map_err(|e| {
                    log::error!("tx_to_sign_data error for tx {:?}", e);
                    halo2_proofs::plonk::Error::Synthesis
                })
            }
        })
        .collect::<Result<Vec<SignData>, halo2_proofs::plonk::Error>>()?;
    Ok(signatures)
}
