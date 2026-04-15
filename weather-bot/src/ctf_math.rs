//! CTF math — deterministic derivation of `questionId`, `conditionId`, and
//! ERC-1155 `positionId` for both the standard Gnosis Conditional Tokens path
//! and Polymarket's NegRiskAdapter.
//!
//! # Why this exists
//!
//! When a new weather market is prepared on chain (NegRiskAdapter
//! `MarketPrepared` + `QuestionPrepared` logs), we want to pre-sign CLOB dump
//! orders BEFORE waiting for Polymarket's CLOB indexer to ingest the market.
//! That requires knowing the `tokenId` for each bucket the instant the logs
//! land. This module computes those tokenIds off-chain, matching the exact
//! Solidity math.
//!
//! # Critical NegRiskAdapter quirk
//!
//! The adapter inverts the standard Gnosis CTF index-set convention:
//! * NegRisk:   YES = indexSet 1 (0b01), NO = indexSet 2 (0b10)
//! * Standard:  YES = indexSet 2 (0b10), NO = indexSet 1 (0b01)
//!
//! And the oracle used for `conditionId` derivation is the NegRiskAdapter
//! itself, not the UMA resolver. Collateral is the wrapped-collateral contract
//! (`wcol`), not raw USDC.
//!
//! Source of truth:
//! * `Polymarket/neg-risk-ctf-adapter/src/libraries/NegRiskIdLib.sol`
//! * `Polymarket/neg-risk-ctf-adapter/src/libraries/CTHelpers.sol`
//! * `Polymarket/neg-risk-ctf-adapter/src/NegRiskAdapter.sol::getPositionId`
//!
//! All tests in this module are validated against LIVE gamma-API data for the
//! Lucknow April 15 2026 temperature event — if you edit anything here and
//! the tests fail, the bot will compute wrong tokenIds and place orders on
//! the wrong markets. Fix the code, don't weaken the tests.

use alloy_primitives::{address, keccak256, Address, B256, U256};

// -------- Polygon contract addresses --------

/// USDC on Polygon PoS. Collateral for all standard binary CTF markets.
pub const USDC_POLYGON: Address = address!("2791Bca1f2de4661ED88A30C99A7a9449Aa84174");

/// Gnosis Conditional Tokens Framework on Polygon.
pub const CTF_ADDRESS: Address = address!("4D97DCd97eC945f40cF65F87097ACe5EA0476045");

/// Polymarket NegRiskAdapter. Acts as the CTF "oracle" for neg-risk sub-questions.
pub const NEG_RISK_ADAPTER: Address = address!("d91E80cF2E7be2e162c6513ceD06f1dD0dA35296");

/// Polymarket NegRiskWrappedCollateral (`wcol`) — the actual collateral token
/// used in `getPositionId` for neg-risk markets. USDC is wrapped into this
/// and unwrapped at redemption. Deployed from the NegRiskAdapter constructor.
pub const NEG_RISK_WCOL: Address = address!("3A3BD7bb9528E159577F7C2e685CC81A765002E2");

/// CTF Exchange (binary market trading).
pub const CTF_EXCHANGE: Address = address!("4bFb41d5B3570DeFd03C39a9A4D8dE6Bd8B8982E");

/// NegRisk CTF Exchange (multi-outcome trading).
pub const NEG_RISK_EXCHANGE: Address = address!("C5d563A36AE78145C45a50134d48A1215220f80a");

// -------- NegRiskIdLib port --------

/// Given a NegRisk `marketId` and a 0-based question index, return the
/// per-question `questionId`. Mirrors `NegRiskIdLib.getQuestionId` which is
/// literally `bytes32(uint256(marketId) + questionIndex)`.
///
/// Safety: the adapter guarantees `marketId`'s low byte is zero via the MASK,
/// so adding a u8 index cannot overflow into higher bytes. Debug-asserted.
pub fn neg_risk_question_id(market_id: B256, question_index: u8) -> B256 {
    debug_assert_eq!(
        market_id.0[31], 0,
        "neg-risk marketId must have its low byte zeroed (it's masked in Solidity)"
    );
    let mut bytes = market_id.0;
    bytes[31] = question_index;
    B256::from(bytes)
}

/// Inverse of `neg_risk_question_id` — extract the question index from the
/// low byte of a questionId. Handy when decoding `QuestionPrepared` logs.
pub fn neg_risk_question_index(question_id: B256) -> u8 {
    question_id.0[31]
}

// -------- CTHelpers port (simplified for top-level conditions) --------

/// Gnosis CTF `getConditionId` — `keccak256(abi.encodePacked(oracle, questionId, outcomeSlotCount))`.
///
/// `abi.encodePacked` means tight packing: oracle (20B) + questionId (32B) +
/// outcomeSlotCount as uint256 big-endian (32B) = 84 bytes total.
pub fn condition_id(oracle: Address, question_id: B256, outcome_slot_count: u32) -> B256 {
    let mut buf = [0u8; 84];
    buf[0..20].copy_from_slice(oracle.as_slice());
    buf[20..52].copy_from_slice(question_id.as_slice());
    // outcomeSlotCount is `uint256` in the Solidity signature — 32 bytes big-endian.
    // Only the low 4 bytes differ from zero for realistic counts.
    let cnt_be = outcome_slot_count.to_be_bytes();
    buf[80..84].copy_from_slice(&cnt_be);
    keccak256(&buf)
}

// -------- alt_bn128 curve constants for getCollectionId --------
//
// Gnosis `getCollectionId` maps (conditionId, indexSet) to a point on the
// alt_bn128 curve `y² = x³ + 3 (mod P)`. We only need the top-level case
// (parentCollectionId == 0).

use alloy_primitives::uint;

/// alt_bn128 base field prime.
const BN254_P: U256 =
    uint!(21888242871839275222246405745257275088696311157297823662689037894645226208583_U256);

/// Curve parameter B.
const BN254_B: U256 = uint!(3_U256);

/// `(P + 1) / 4` — exponent for square-root mod P when `P ≡ 3 (mod 4)`.
/// Computed once at runtime to avoid typos in the hardcoded constant
/// (trust me, it's been a source of bugs).
fn bn254_sqrt_exp() -> U256 {
    use std::sync::OnceLock;
    static EXP: OnceLock<U256> = OnceLock::new();
    *EXP.get_or_init(|| (BN254_P + U256::from(1u8)) >> 2)
}

/// Modular square root on `F_P` via `x^((P+1)/4) mod P`. Returns `None` if
/// `x` is not a quadratic residue (caller must increment and retry).
fn bn254_sqrt(x: U256) -> Option<U256> {
    let r = x.pow_mod(bn254_sqrt_exp(), BN254_P);
    if r.mul_mod(r, BN254_P) == x {
        Some(r)
    } else {
        None
    }
}

#[inline]
fn is_odd(u: U256) -> bool {
    (u.as_limbs()[0] & 1) == 1
}

/// Gnosis CTF `getCollectionId` — top-level only (`parentCollectionId == 0`).
///
/// Ported literally from
/// `Polymarket/neg-risk-ctf-adapter/src/libraries/CTHelpers.sol`:
///
/// ```text
/// x1 = uint256(keccak256(abi.encodePacked(conditionId, indexSet)))
/// odd = (x1 >> 255) != 0
/// do {
///     x1 = addmod(x1, 1, P)
///     yy = (x1³ + B) mod P
///     y1 = sqrt(yy)
/// } while (y1*y1 mod P != yy)
/// if ((odd && y1 even) || (!odd && y1 odd)) y1 = P - y1
/// if (y1 odd) x1 ^= 1 << 254
/// return bytes32(x1)
/// ```
///
/// NB: this is NOT a plain keccak — a previous version of this file assumed
/// it was, and produced wrong positionIds. The EC dance encodes point
/// compression and the sign of y into the top two bits of x.
pub fn collection_id_top_level(condition_id: B256, index_set: u8) -> B256 {
    let mut buf = [0u8; 64];
    buf[0..32].copy_from_slice(condition_id.as_slice());
    // indexSet is uint256 in the Solidity signature — tight-packed as 32 bytes
    // big-endian. Only the last byte is ever nonzero for binary/ternary.
    buf[63] = index_set;
    let initial = keccak256(&buf);
    let mut x1 = U256::from_be_slice(initial.as_slice());

    // Capture the original top bit BEFORE the increment loop runs.
    let top_bit_mask = U256::from(1u8) << 255;
    let odd_initial = (x1 & top_bit_mask) != U256::ZERO;

    // do { x1 = (x1 + 1) mod P; yy = x1^3 + B; y1 = sqrt(yy); } while sqrt nonexistent
    loop {
        x1 = x1.add_mod(U256::from(1u8), BN254_P);
        let xx = x1.mul_mod(x1, BN254_P);
        let xxx = xx.mul_mod(x1, BN254_P);
        let yy = xxx.add_mod(BN254_B, BN254_P);
        if let Some(mut y1) = bn254_sqrt(yy) {
            // Match the sign of y1 to the captured `odd_initial`:
            //   if (odd_initial && y1_even)  || (!odd_initial && y1_odd) => y1 = P - y1
            let y1_odd = is_odd(y1);
            if (odd_initial && !y1_odd) || (!odd_initial && y1_odd) {
                y1 = BN254_P - y1;
            }
            // Encode final y1 parity into bit 254 of x1 (point compression).
            if is_odd(y1) {
                x1 ^= U256::from(1u8) << 254;
            }
            return B256::from_slice(&x1.to_be_bytes::<32>());
        }
        // No sqrt: try x1 + 1.
    }
}

/// Gnosis CTF `getPositionId` — `uint256(keccak256(abi.encodePacked(collateralToken, collectionId)))`.
pub fn position_id(collateral: Address, collection: B256) -> U256 {
    let mut buf = [0u8; 52];
    buf[0..20].copy_from_slice(collateral.as_slice());
    buf[20..52].copy_from_slice(collection.as_slice());
    let h = keccak256(&buf);
    U256::from_be_slice(h.as_slice())
}

// -------- Combined helpers (the public API the watchers use) --------

/// Given a NegRisk `marketId` and a question index, return
/// `(yes_token_id, no_token_id)` as ERC-1155 position IDs.
///
/// Remember: NegRiskAdapter inverts the Gnosis convention — YES uses
/// indexSet 1, NO uses indexSet 2.
pub fn neg_risk_bucket_token_ids(market_id: B256, question_index: u8) -> (U256, U256) {
    let q_id = neg_risk_question_id(market_id, question_index);
    let c_id = condition_id(NEG_RISK_ADAPTER, q_id, 2);
    let coll_yes = collection_id_top_level(c_id, 1); // NegRisk: YES = 1
    let coll_no = collection_id_top_level(c_id, 2); //           NO  = 2
    (
        position_id(NEG_RISK_WCOL, coll_yes),
        position_id(NEG_RISK_WCOL, coll_no),
    )
}

/// Given a NegRisk `marketId` and a question index, return the CTF
/// `conditionId` for that bucket — needed when building split/convert txs.
pub fn neg_risk_bucket_condition_id(market_id: B256, question_index: u8) -> B256 {
    let q_id = neg_risk_question_id(market_id, question_index);
    condition_id(NEG_RISK_ADAPTER, q_id, 2)
}

/// Standard binary CTF tokenId derivation — used for non-negrisk markets
/// (rare for weather, but supported for completeness).
///
/// Unlike NegRisk, here YES = indexSet 2, NO = indexSet 1. Collateral is USDC
/// directly (no wcol wrapper).
pub fn binary_ctf_bucket_token_ids(
    oracle: Address,
    question_id: B256,
    collateral: Address,
) -> (U256, U256) {
    let c_id = condition_id(oracle, question_id, 2);
    let coll_no = collection_id_top_level(c_id, 1); // standard: NO = 1
    let coll_yes = collection_id_top_level(c_id, 2); //           YES = 2
    (
        position_id(collateral, coll_yes),
        position_id(collateral, coll_no),
    )
}

#[cfg(test)]
mod tests {
    //! Ground-truth verification against LIVE Lucknow April 15 2026 data.
    //! If any of these break, the NegRiskIdLib port is wrong — DO NOT weaken
    //! the tests, fix the code.
    //!
    //! Data source:
    //!   curl -s 'https://gamma-api.polymarket.com/events/slug/highest-temperature-in-lucknow-on-april-15-2026'
    //! Captured on 2026-04-14.

    use super::*;
    use alloy_primitives::b256;

    /// The NegRisk marketId for the Lucknow April 15 event. Ends in 0x00.
    const LUCKNOW_MARKET_ID: B256 =
        b256!("f7296c562facec52c9a18672d0849f1816ac4a305395782ffcb130da8613b900");

    #[test]
    fn question_id_derivation_matches_ground_truth() {
        // Bucket 0: "37°C or below" → questionID ends in 0x00
        let q0 = neg_risk_question_id(LUCKNOW_MARKET_ID, 0);
        assert_eq!(
            q0,
            b256!("f7296c562facec52c9a18672d0849f1816ac4a305395782ffcb130da8613b900"),
        );

        // Bucket 3: "40°C" → questionID ends in 0x03
        let q3 = neg_risk_question_id(LUCKNOW_MARKET_ID, 3);
        assert_eq!(
            q3,
            b256!("f7296c562facec52c9a18672d0849f1816ac4a305395782ffcb130da8613b903"),
        );

        // Round trip
        assert_eq!(neg_risk_question_index(q3), 3);
    }

    #[test]
    fn condition_id_matches_gamma_for_40c() {
        // Expected conditionId for Lucknow 40°C bucket 3
        let c = neg_risk_bucket_condition_id(LUCKNOW_MARKET_ID, 3);
        assert_eq!(
            c,
            b256!("755f5ab4549827311b82bc3b2eebe8d2b561d7a4944727b6b8838d0c16331882"),
        );
    }

    #[test]
    fn condition_id_matches_gamma_for_37corbelow() {
        let c = neg_risk_bucket_condition_id(LUCKNOW_MARKET_ID, 0);
        assert_eq!(
            c,
            b256!("313f6e0ae9b8a71eac6158056f6aeec41b79064355d027ffb7bdf6456603dfb5"),
        );
    }

    #[test]
    fn position_ids_match_gamma_for_40c() {
        // From gamma clobTokenIds for "Will the highest temperature in Lucknow be 40°C on April 15?"
        // outcomes: ["Yes", "No"]
        let expected_yes = U256::from_str_radix(
            "102303837401854256787167961846178706641364635135540425700555802960141850980079",
            10,
        )
        .unwrap();
        let expected_no = U256::from_str_radix(
            "62083515218446170130608630717788112610699995417825187654300364579001218572587",
            10,
        )
        .unwrap();

        let (yes, no) = neg_risk_bucket_token_ids(LUCKNOW_MARKET_ID, 3);
        assert_eq!(
            yes, expected_yes,
            "Lucknow 40°C YES positionId mismatch — NegRiskIdLib port is wrong"
        );
        assert_eq!(
            no, expected_no,
            "Lucknow 40°C NO positionId mismatch — NegRiskIdLib port is wrong"
        );
    }

    #[test]
    fn position_ids_match_gamma_for_38c() {
        let expected_yes = U256::from_str_radix(
            "55419471594771195607446585671446224114812450137060000029720873702017441340588",
            10,
        )
        .unwrap();
        let expected_no = U256::from_str_radix(
            "69852217622963961602844386021927404400550876292951493210073670281622813776052",
            10,
        )
        .unwrap();
        let (yes, no) = neg_risk_bucket_token_ids(LUCKNOW_MARKET_ID, 1);
        assert_eq!(yes, expected_yes);
        assert_eq!(no, expected_no);
    }

    #[test]
    fn position_ids_match_gamma_for_46c() {
        let expected_yes = U256::from_str_radix(
            "109650837893728857843182720184824679817609420943075752968211635049432188675646",
            10,
        )
        .unwrap();
        let expected_no = U256::from_str_radix(
            "58760577143340196600596979747638304119250109131481433039976452795466281199338",
            10,
        )
        .unwrap();
        let (yes, no) = neg_risk_bucket_token_ids(LUCKNOW_MARKET_ID, 9);
        assert_eq!(yes, expected_yes);
        assert_eq!(no, expected_no);
    }

    #[test]
    fn question_index_from_low_byte() {
        let q = b256!("f7296c562facec52c9a18672d0849f1816ac4a305395782ffcb130da8613b907");
        assert_eq!(neg_risk_question_index(q), 7);
    }
}
