//! This module contains the core logic for building Solana transactions
//! from Jupiter API responses. It is agnostic to the execution environment (real or simulated).

use crate::models::{InstructionData, UnsignedTransaction};
use anyhow::{Context, Result};
use base64::{Engine, engine::general_purpose::STANDARD};
use solana_client::rpc_client::RpcClient;
use solana_sdk::{
    address_lookup_table::{AddressLookupTableAccount, state::AddressLookupTable},
    instruction::{AccountMeta, Instruction},
    message::{VersionedMessage, v0},
    pubkey::Pubkey,
    signature::Signature,
    transaction::VersionedTransaction,
};
use std::str::FromStr;

/// Converts a vector of Jupiter's `InstructionData` into `solana_sdk::Instruction`.
/// This is a shared utility used by all transaction-building flows.
pub fn convert_instructions(instructions_data: Vec<InstructionData>) -> Result<Vec<Instruction>> {
    instructions_data
        .into_iter()
        .map(|ix_data| -> Result<Instruction> {
            Ok(Instruction {
                program_id: Pubkey::from_str(&ix_data.program_id)
                    .context("Failed to parse program_id")?,
                accounts: ix_data
                    .accounts
                    .into_iter()
                    .map(|k| -> Result<AccountMeta> {
                        Ok(AccountMeta {
                            pubkey: Pubkey::from_str(&k.pubkey)
                                .context("Failed to parse account pubkey")?,
                            is_signer: k.is_signer,
                            is_writable: k.is_writable,
                        })
                    })
                    .collect::<Result<Vec<_>>>()?,
                data: STANDARD
                    .decode(&ix_data.data)
                    .context("Failed to decode instruction data from base64")?,
            })
        })
        .collect()
}

/// Fetches Address Lookup Table accounts from the RPC client.
pub fn fetch_address_lookup_tables(
    rpc_client: &RpcClient,
    lookup_table_addresses: Vec<String>,
) -> Result<Vec<AddressLookupTableAccount>> {
    let lookup_table_keys: Vec<Pubkey> = lookup_table_addresses
        .into_iter()
        .map(|s| Pubkey::from_str(&s))
        .collect::<Result<Vec<_>, _>>()
        .context("Failed to parse lookup table addresses")?;

    if lookup_table_keys.is_empty() {
        return Ok(vec![]);
    }

    let alt_accounts = rpc_client
        .get_multiple_accounts(&lookup_table_keys)?
        .into_iter()
        .zip(lookup_table_keys)
        .filter_map(|(acc_opt, key)| acc_opt.map(|acc| (key, acc)))
        .map(|(key, acc)| {
            let table =
                AddressLookupTable::deserialize(&acc.data).context("Failed to deserialize ALT")?;
            Ok(AddressLookupTableAccount {
                key,
                addresses: table.addresses.to_vec(),
            })
        })
        .collect::<Result<Vec<_>>>()?;

    Ok(alt_accounts)
}

/// Compiles instructions and ALTs into an unsigned `VersionedTransaction`.
pub fn compile_transaction(
    rpc_client: &RpcClient,
    user_public_key: &Pubkey,
    instructions: Vec<Instruction>,
    alt_accounts: Vec<AddressLookupTableAccount>,
) -> Result<UnsignedTransaction> {
    let (latest_blockhash, last_valid_block_height) = rpc_client
        .get_latest_blockhash_with_commitment(rpc_client.commitment())
        .context("Failed to get latest blockhash")?;

    let message = v0::Message::try_compile(
        user_public_key,
        &instructions,
        &alt_accounts,
        latest_blockhash,
    )?;

    let versioned_message = VersionedMessage::V0(message);
    let num_signers = versioned_message.header().num_required_signatures as usize;
    let transaction = VersionedTransaction {
        signatures: vec![Signature::default(); num_signers],
        message: versioned_message,
    };

    Ok(UnsignedTransaction {
        transaction,
        last_valid_block_height,
    })
}

/// Prepends idempotent ATA-creation instructions for any of the user's associated
/// token accounts that the given instructions write to.
///
/// The Jupiter Lend API returns only the Lend program instruction and assumes the
/// user's jlToken / underlying-asset ATAs already exist. On a fresh surfpool wallet
/// they don't, which fails with `AccountNotInitialized` (0xbc4).
pub fn with_user_atas(user_pubkey: &Pubkey, instructions: Vec<Instruction>) -> Vec<Instruction> {
    let token_programs = [
        "TokenkegQfeZyiNwAJbNbGKPFXCWuBvf9Ss623VQ5DA",
        "TokenzQdBNbLqP5VEhdkAS6EPFLC1PHnBqCXEpPxuEb",
    ]
    .map(|id| Pubkey::from_str(id).expect("valid token program id"));
    let mut setup: Vec<Instruction> = Vec::new();
    let mut seen: Vec<Pubkey> = Vec::new();
    for ix in &instructions {
        for target in ix.accounts.iter().filter(|a| a.is_writable && !a.is_signer) {
            if seen.contains(&target.pubkey) {
                continue;
            }
            'search: for mint in ix.accounts.iter().map(|a| a.pubkey) {
                for program in &token_programs {
                    let ata = spl_associated_token_account::get_associated_token_address_with_program_id(
                        user_pubkey,
                        &mint,
                        program,
                    );
                    if ata == target.pubkey {
                        setup.push(
                            spl_associated_token_account::instruction::create_associated_token_account_idempotent(
                                user_pubkey,
                                user_pubkey,
                                &mint,
                                program,
                            ),
                        );
                        seen.push(target.pubkey);
                        break 'search;
                    }
                }
            }
        }
    }
    setup.extend(instructions);
    setup
}
