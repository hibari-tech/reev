//! Scores the on-chain outcome of a benchmark against its `final_state_assertions`.
//!
//! Instruction matching checks the shape of what the agent proposed (program, accounts,
//! optionally data), but most benchmarks leave the instruction data unscored and the
//! Jupiter benchmarks have no expected instructions at all, so the *amount* was never
//! verified: an agent that lent 10 USDC when asked for 50 scored 1.0. The final on-chain
//! state is the ground truth for amounts, so once a transaction has executed, the score is
//! scaled by the weighted fraction of final-state assertions that hold.
//!
//! Semantics of each assertion:
//! - `SolBalance`: the account's lamports equal `expected`.
//! - `SolBalanceChange`: final minus initial lamports is `>= expected_change_gte` and, when
//!   given, `<= expected_change_lte`. Both bounds together pin the amount that moved.
//! - `TokenAccountBalance`: the token amount satisfies every bound given (`expected`,
//!   `expected_gte`, `expected_lte`). With `address_derivation` the associated token account
//!   is derived from the owner and mint; an account that does not exist has a balance of 0.

use crate::{
    agent::AgentObservation,
    benchmark::{AddressDerivation, GroundTruth, StateAssertion},
};
use serde_json::Value;
use solana_sdk::pubkey::Pubkey;
use spl_associated_token_account::get_associated_token_address;
use std::str::FromStr;
use tracing::warn;

/// The outcome of one final-state assertion.
#[derive(Debug, Clone, PartialEq)]
pub struct StateCheck {
    /// What was asserted, e.g. `TokenAccountBalance USER_USDC_ATA == 35000000`.
    pub assertion: String,
    /// What the chain showed.
    pub observed: String,
    pub weight: f64,
    pub passed: bool,
}

/// All final-state assertions of a benchmark, evaluated against the final observation.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct StateScore {
    pub checks: Vec<StateCheck>,
    /// Weighted fraction of assertions that hold, or `None` when the benchmark asserts
    /// nothing (or only zero-weight assertions).
    pub score: Option<f64>,
}

impl StateScore {
    pub fn failures(&self) -> impl Iterator<Item = &StateCheck> {
        self.checks.iter().filter(|c| !c.passed)
    }
}

/// Evaluates every `final_state_assertions` entry of `ground_truth`.
pub fn evaluate_state(
    ground_truth: &GroundTruth,
    initial: &AgentObservation,
    final_: &AgentObservation,
) -> StateScore {
    let mut checks = Vec::new();
    for assertion in &ground_truth.final_state_assertions {
        let weight = assertion.weight();
        let (label, observed, passed) = match assertion {
            StateAssertion::SolBalance {
                pubkey, expected, ..
            } => {
                let got = lamports(account_state(final_, assertion));
                (
                    format!("SolBalance {pubkey} == {expected}"),
                    format!("{got} lamports"),
                    got == *expected,
                )
            }
            StateAssertion::SolBalanceChange {
                pubkey,
                expected_change_gte,
                expected_change_lte,
                ..
            } => {
                let before = lamports(account_state(initial, assertion)) as i128;
                let after = lamports(account_state(final_, assertion)) as i128;
                let change = after - before;
                let mut passed = change >= *expected_change_gte as i128;
                let label = match expected_change_lte {
                    Some(lte) => {
                        passed &= change <= *lte as i128;
                        format!("SolBalanceChange {pubkey} in [{expected_change_gte}, {lte}]")
                    }
                    None => format!("SolBalanceChange {pubkey} >= {expected_change_gte}"),
                };
                (label, format!("{change:+} lamports"), passed)
            }
            StateAssertion::TokenAccountBalance {
                pubkey,
                expected,
                expected_gte,
                expected_lte,
                address_derivation,
                ..
            } => {
                let got = token_amount(account_state(final_, assertion));
                let mut conditions = Vec::new();
                let mut passed = true;
                if let Some(e) = expected {
                    conditions.push(format!("== {e}"));
                    passed &= got == *e;
                }
                if let Some(e) = expected_gte {
                    conditions.push(format!(">= {e}"));
                    passed &= got >= *e;
                }
                if let Some(e) = expected_lte {
                    conditions.push(format!("<= {e}"));
                    passed &= got <= *e;
                }
                if conditions.is_empty() {
                    warn!(
                        pubkey,
                        "TokenAccountBalance assertion has no bound; it always passes"
                    );
                }
                let target = match address_derivation {
                    Some(AddressDerivation::AssociatedTokenAccount { owner, mint }) => {
                        format!("ATA({owner}, {mint})")
                    }
                    None => pubkey.clone(),
                };
                (
                    format!("TokenAccountBalance {target} {}", conditions.join(" and ")),
                    format!("{got} base units"),
                    passed,
                )
            }
        };
        checks.push(StateCheck {
            assertion: label,
            observed,
            weight,
            passed,
        });
    }

    let total: f64 = checks.iter().map(|c| c.weight).sum();
    let score = if total > 0.0 {
        Some(
            checks
                .iter()
                .filter(|c| c.passed)
                .map(|c| c.weight)
                .sum::<f64>()
                / total,
        )
    } else {
        None
    };
    StateScore { checks, score }
}

/// The recorded state of the account an assertion refers to, or `None` when it is not in
/// the observation (an account that does not exist holds nothing).
fn account_state<'a>(obs: &'a AgentObservation, assertion: &StateAssertion) -> Option<&'a Value> {
    if let Some(AddressDerivation::AssociatedTokenAccount { owner, mint }) =
        assertion.address_derivation()
    {
        let owner = resolve_pubkey(obs, owner)?;
        let mint = Pubkey::from_str(mint).ok()?;
        let ata = get_associated_token_address(&owner, &mint).to_string();
        return state_by_pubkey(obs, &ata);
    }
    let name = assertion.pubkey();
    obs.account_states
        .get(name)
        .or_else(|| state_by_pubkey(obs, name))
}

/// A placeholder from the key map, or a literal pubkey.
fn resolve_pubkey(obs: &AgentObservation, placeholder: &str) -> Option<Pubkey> {
    let text = obs
        .key_map
        .get(placeholder)
        .map(String::as_str)
        .unwrap_or(placeholder);
    Pubkey::from_str(text).ok()
}

/// The state recorded under any placeholder that maps to this pubkey. Several placeholders
/// can share one address (a derived ATA and its named counterpart), so the lookup goes
/// through the key map rather than the placeholder the assertion happens to use.
fn state_by_pubkey<'a>(obs: &'a AgentObservation, pubkey: &str) -> Option<&'a Value> {
    let mut names: Vec<&String> = obs
        .key_map
        .iter()
        .filter(|(_, v)| v.as_str() == pubkey)
        .map(|(k, _)| k)
        .collect();
    names.sort();
    names.into_iter().find_map(|n| obs.account_states.get(n))
}

fn lamports(state: Option<&Value>) -> u64 {
    state
        .and_then(|s| s.get("lamports"))
        .and_then(Value::as_u64)
        .unwrap_or(0)
}

fn token_amount(state: Option<&Value>) -> u64 {
    state
        .and_then(|s| s.get("amount"))
        .and_then(Value::as_u64)
        .unwrap_or(0)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::benchmark::{ExpectedOutcome, GroundTruth};
    use serde_json::json;

    const USDC: &str = "EPjFWdd5AufqSSqeM2qN1xzybapC8G4wEGGkZwyTDt1v";

    fn ground_truth(assertions: Vec<StateAssertion>) -> GroundTruth {
        GroundTruth {
            transaction_status: "Success".to_string(),
            final_state_assertions: assertions,
            expected_instructions: vec![],
            skip_instruction_validation: false,
            expected_outcome: ExpectedOutcome::Execute,
        }
    }

    fn observation(accounts: &[(&str, &str, Value)]) -> AgentObservation {
        AgentObservation {
            last_transaction_status: "Success".to_string(),
            last_transaction_error: None,
            last_transaction_logs: vec![],
            account_states: accounts
                .iter()
                .map(|(name, _, state)| (name.to_string(), state.clone()))
                .collect(),
            key_map: accounts
                .iter()
                .map(|(name, key, _)| (name.to_string(), key.to_string()))
                .collect(),
        }
    }

    #[test]
    fn sol_balance_is_exact() {
        let recipient = Pubkey::new_unique().to_string();
        let gt = ground_truth(vec![StateAssertion::SolBalance {
            pubkey: "RECIPIENT_WALLET_PUBKEY".to_string(),
            expected: 100_000_000,
            weight: 1.0,
        }]);
        let ok = observation(&[(
            "RECIPIENT_WALLET_PUBKEY",
            &recipient,
            json!({"lamports": 100_000_000u64}),
        )]);
        assert_eq!(evaluate_state(&gt, &ok, &ok).score, Some(1.0));
        let wrong = observation(&[(
            "RECIPIENT_WALLET_PUBKEY",
            &recipient,
            json!({"lamports": 200_000_000u64}),
        )]);
        let state = evaluate_state(&gt, &wrong, &wrong);
        assert_eq!(state.score, Some(0.0));
        assert_eq!(state.checks[0].observed, "200000000 lamports");
    }

    #[test]
    fn balance_change_is_bounded_on_both_sides() {
        let user = Pubkey::new_unique().to_string();
        let gt = ground_truth(vec![StateAssertion::SolBalanceChange {
            pubkey: "USER_WALLET_PUBKEY".to_string(),
            expected_change_gte: -105_000_000,
            expected_change_lte: Some(-100_000_000),
            weight: 1.0,
        }]);
        let before = observation(&[(
            "USER_WALLET_PUBKEY",
            &user,
            json!({"lamports": 5_000_000_000u64}),
        )]);
        // A 0.1 SOL deposit plus fee and two ATA rents.
        let deposited = observation(&[(
            "USER_WALLET_PUBKEY",
            &user,
            json!({"lamports": 4_895_916_440u64}),
        )]);
        assert_eq!(evaluate_state(&gt, &before, &deposited).score, Some(1.0));
        // Only 0.01 SOL moved: the lower bound on the amount fails.
        let too_little = observation(&[(
            "USER_WALLET_PUBKEY",
            &user,
            json!({"lamports": 4_989_995_000u64}),
        )]);
        assert_eq!(evaluate_state(&gt, &before, &too_little).score, Some(0.0));
        // Nothing moved.
        assert_eq!(evaluate_state(&gt, &before, &before).score, Some(0.0));
    }

    #[test]
    fn derived_token_account_is_found_through_the_key_map() {
        let owner = Pubkey::new_unique();
        let ata =
            get_associated_token_address(&owner, &Pubkey::from_str(USDC).unwrap()).to_string();
        let gt = ground_truth(vec![StateAssertion::TokenAccountBalance {
            pubkey: "UNUSED_PLACEHOLDER".to_string(),
            expected: Some(50_000_000),
            expected_gte: None,
            expected_lte: None,
            address_derivation: Some(AddressDerivation::AssociatedTokenAccount {
                owner: "USER_WALLET_PUBKEY".to_string(),
                mint: USDC.to_string(),
            }),
            weight: 1.0,
        }]);
        // The observation records the ATA under a different placeholder than the assertion uses.
        let obs = observation(&[
            (
                "USER_WALLET_PUBKEY",
                &owner.to_string(),
                json!({"lamports": 1u64}),
            ),
            (
                "USER_USDC_ATA_PLACEHOLDER",
                &ata,
                json!({"lamports": 2_039_280u64, "amount": 50_000_000u64, "mint": USDC}),
            ),
        ]);
        assert_eq!(evaluate_state(&gt, &obs, &obs).score, Some(1.0));

        // 10 USDC lent instead of 50 leaves 90, which fails the amount check.
        let wrong = observation(&[
            (
                "USER_WALLET_PUBKEY",
                &owner.to_string(),
                json!({"lamports": 1u64}),
            ),
            (
                "USER_USDC_ATA_PLACEHOLDER",
                &ata,
                json!({"lamports": 2_039_280u64, "amount": 90_000_000u64, "mint": USDC}),
            ),
        ]);
        assert_eq!(evaluate_state(&gt, &wrong, &wrong).score, Some(0.0));
    }

    #[test]
    fn missing_account_has_zero_balance() {
        let owner = Pubkey::new_unique();
        let gt = ground_truth(vec![
            StateAssertion::TokenAccountBalance {
                pubkey: "UNUSED_PLACEHOLDER".to_string(),
                expected: None,
                expected_gte: Some(1),
                expected_lte: None,
                address_derivation: Some(AddressDerivation::AssociatedTokenAccount {
                    owner: "USER_WALLET_PUBKEY".to_string(),
                    mint: USDC.to_string(),
                }),
                weight: 0.5,
            },
            StateAssertion::TokenAccountBalance {
                pubkey: "UNUSED_PLACEHOLDER".to_string(),
                expected: None,
                expected_gte: None,
                expected_lte: Some(5_000_000),
                address_derivation: Some(AddressDerivation::AssociatedTokenAccount {
                    owner: "USER_WALLET_PUBKEY".to_string(),
                    mint: USDC.to_string(),
                }),
                weight: 0.5,
            },
        ]);
        let obs = observation(&[(
            "USER_WALLET_PUBKEY",
            &owner.to_string(),
            json!({"lamports": 1u64}),
        )]);
        let state = evaluate_state(&gt, &obs, &obs);
        // `>= 1` fails on a non-existent account, `<= 5000000` holds: weighted 0.5.
        assert_eq!(state.score, Some(0.5));
        assert_eq!(state.failures().count(), 1);
    }

    #[test]
    fn no_assertions_means_no_state_score() {
        let gt = ground_truth(vec![]);
        let obs = observation(&[]);
        assert_eq!(evaluate_state(&gt, &obs, &obs).score, None);
    }
}
