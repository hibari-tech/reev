//! # Remote agent adapter
//!
//! Lets another team's agent take the exam over HTTP. Run the runner with
//! `--agent remote-<label>` and set `REMOTE_AGENT_<LABEL>_URL` (and optionally
//! `REMOTE_AGENT_<LABEL>_TOKEN`, sent as a bearer token) in `.env`.
//!
//! reev POSTs the task to the agent and expects unsigned instructions back. The agent never
//! receives private keys and never sees the benchmark id: it gets the user's prompt, the
//! addresses by role and the current balances, the same way a wallet would. reev signs with
//! its own throwaway keypairs on the mainnet fork. Contract: `trust/adapter/README.md`.

use anyhow::{anyhow, bail, Context, Result};
use base64::{engine::general_purpose::STANDARD, Engine};
use reev_lib::agent::{RawAccountMeta, RawInstruction};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use solana_sdk::pubkey::Pubkey;
use std::{collections::HashMap, str::FromStr, time::Duration};
use tracing::info;

use crate::LlmRequest;

pub const PROTOCOL: &str = "reev-agent/v1";
const MAX_INSTRUCTIONS: usize = 16;
const TIMEOUT: Duration = Duration::from_secs(120);

/// What reev sends to the remote agent.
#[derive(Debug, Serialize)]
pub struct RemoteTask<'a> {
    pub protocol: &'static str,
    pub session_id: &'a str,
    pub prompt: &'a str,
    /// Placeholder role (e.g. `USER_WALLET_PUBKEY`) -> base58 address on the fork.
    pub accounts: &'a HashMap<String, String>,
    /// Role -> account state (lamports, and mint/amount for token accounts), when known.
    pub balances: Option<&'a HashMap<String, Value>>,
    /// The wallet that will sign and pay; instructions should use it as the signer.
    pub signer: Option<&'a String>,
}

/// What the remote agent returns. An empty `instructions` list means the agent declines.
#[derive(Debug, Deserialize)]
pub struct RemoteReply {
    #[serde(default)]
    pub instructions: Vec<RemoteInstruction>,
    /// Optional human-readable reason when declining (logged, not scored).
    #[serde(default)]
    pub refusal: Option<String>,
}

#[derive(Debug, Deserialize)]
pub struct RemoteInstruction {
    pub program_id: String,
    pub accounts: Vec<RemoteAccount>,
    pub data: String,
    /// `base58` (default) or `base64`.
    #[serde(default)]
    pub encoding: Option<String>,
}

#[derive(Debug, Deserialize)]
pub struct RemoteAccount {
    pub pubkey: String,
    #[serde(default)]
    pub is_signer: bool,
    #[serde(default)]
    pub is_writable: bool,
}

/// `remote-team-x` -> `TEAM_X`, used to build the env var names.
pub fn env_label(model_name: &str) -> Result<String> {
    let label = model_name
        .strip_prefix("remote-")
        .filter(|l| !l.is_empty())
        .ok_or_else(|| anyhow!("remote agent names look like `remote-<label>`, got `{model_name}`"))?;
    Ok(label
        .chars()
        .map(|c| match c.is_ascii_alphanumeric() {
            true => c.to_ascii_uppercase(),
            false => '_',
        })
        .collect())
}

/// Validates the reply and converts it to reev's instruction format (base58 data).
pub fn parse_reply(reply: RemoteReply) -> Result<Vec<RawInstruction>> {
    if reply.instructions.len() > MAX_INSTRUCTIONS {
        bail!("remote agent returned {} instructions (max {MAX_INSTRUCTIONS})", reply.instructions.len());
    }
    if reply.instructions.is_empty() {
        info!(reason = reply.refusal.as_deref().unwrap_or(""), "remote agent declined");
    }
    reply
        .instructions
        .into_iter()
        .enumerate()
        .map(|(i, ix)| {
            Pubkey::from_str(&ix.program_id).with_context(|| format!("instruction {i}: bad program_id"))?;
            let bytes = match ix.encoding.as_deref().unwrap_or("base58") {
                "base58" => bs58::decode(&ix.data).into_vec().with_context(|| format!("instruction {i}: data is not base58"))?,
                "base64" => STANDARD.decode(&ix.data).with_context(|| format!("instruction {i}: data is not base64"))?,
                other => bail!("instruction {i}: unknown encoding `{other}` (use base58 or base64)"),
            };
            let accounts = ix
                .accounts
                .into_iter()
                .map(|a| {
                    Pubkey::from_str(&a.pubkey).with_context(|| format!("instruction {i}: bad account `{}`", a.pubkey))?;
                    Ok(RawAccountMeta { pubkey: a.pubkey, is_signer: a.is_signer, is_writable: a.is_writable })
                })
                .collect::<Result<Vec<_>>>()?;
            Ok(RawInstruction { program_id: ix.program_id, accounts, data: bs58::encode(bytes).into_string() })
        })
        .collect()
}

/// Sends the task to the remote agent and returns reev's instruction JSON (`[]` = declined).
pub async fn run(model_name: &str, payload: &LlmRequest, key_map: &HashMap<String, String>) -> Result<String> {
    let label = env_label(model_name)?;
    let url = std::env::var(format!("REMOTE_AGENT_{label}_URL"))
        .with_context(|| format!("set REMOTE_AGENT_{label}_URL in .env for agent `{model_name}`"))?;
    let task = RemoteTask {
        protocol: PROTOCOL,
        session_id: &payload.session_id,
        prompt: &payload.prompt,
        accounts: key_map,
        balances: payload.account_states.as_ref(),
        signer: key_map.get("USER_WALLET_PUBKEY"),
    };
    info!(model_name, %url, "sending task to remote agent");
    let client = reqwest::Client::builder().timeout(TIMEOUT).build()?;
    let mut req = client.post(&url).json(&task);
    if let Ok(token) = std::env::var(format!("REMOTE_AGENT_{label}_TOKEN")) {
        req = req.bearer_auth(token);
    }
    let reply: RemoteReply = req
        .send()
        .await
        .with_context(|| format!("remote agent `{model_name}` is unreachable"))?
        .error_for_status()?
        .json()
        .await
        .with_context(|| format!("remote agent `{model_name}` returned an invalid reply"))?;
    Ok(serde_json::to_string(&parse_reply(reply)?)?)
}
