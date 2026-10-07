//! Tests for the remote agent adapter (crates/reev-agent/src/remote.rs).

use reev_agent::remote::{env_label, parse_reply, RemoteReply};

const SYSTEM: &str = "11111111111111111111111111111111";
const FROM: &str = "7eriVLdSepjHhwe1yx8DnX465RfwYZxiyrs9TWNg4bWu";
const TO: &str = "CRtnptPwgD3v52reE8bg2XA33HtNdhPVhGR3PN3avm5S";

fn transfer_data() -> Vec<u8> {
    // System Program transfer: u32 index 2, then u64 lamports (0.1 SOL), little endian.
    let mut d = 2u32.to_le_bytes().to_vec();
    d.extend_from_slice(&100_000_000u64.to_le_bytes());
    d
}

fn reply(json: serde_json::Value) -> RemoteReply {
    serde_json::from_value(json).expect("valid reply json")
}

#[test]
fn base58_and_base64_data_decode_to_the_same_instruction() {
    use base64::Engine;
    let data = transfer_data();
    let accounts = serde_json::json!([
        { "pubkey": FROM, "is_signer": true, "is_writable": true },
        { "pubkey": TO, "is_writable": true }
    ]);
    let from58 = parse_reply(reply(serde_json::json!({
        "instructions": [{ "program_id": SYSTEM, "accounts": accounts, "data": bs58::encode(&data).into_string() }]
    })))
    .unwrap();
    let from64 = parse_reply(reply(serde_json::json!({
        "instructions": [{ "program_id": SYSTEM, "accounts": accounts, "encoding": "base64",
                           "data": base64::engine::general_purpose::STANDARD.encode(&data) }]
    })))
    .unwrap();
    assert_eq!(from58.len(), 1);
    assert_eq!(from58[0].data, from64[0].data, "both encodings normalise to base58");
    assert_eq!(bs58::decode(&from58[0].data).into_vec().unwrap(), data);
    assert!(from58[0].accounts[0].is_signer);
    assert!(!from58[0].accounts[1].is_signer, "is_signer defaults to false");
}

#[test]
fn empty_instructions_mean_the_agent_declines() {
    let out = parse_reply(reply(serde_json::json!({ "instructions": [], "refusal": "looks like a scam" }))).unwrap();
    assert!(out.is_empty());
    let out = parse_reply(reply(serde_json::json!({}))).unwrap();
    assert!(out.is_empty(), "a reply without instructions is a refusal");
}

#[test]
fn invalid_keys_and_encodings_are_rejected() {
    let bad_program = reply(serde_json::json!({
        "instructions": [{ "program_id": "not-a-key", "accounts": [], "data": "" }]
    }));
    assert!(parse_reply(bad_program).is_err());
    let bad_account = reply(serde_json::json!({
        "instructions": [{ "program_id": SYSTEM, "accounts": [{ "pubkey": "nope" }], "data": "" }]
    }));
    assert!(parse_reply(bad_account).is_err());
    let bad_encoding = reply(serde_json::json!({
        "instructions": [{ "program_id": SYSTEM, "accounts": [], "data": "AA==", "encoding": "hex" }]
    }));
    assert!(parse_reply(bad_encoding).is_err());
}

#[test]
fn too_many_instructions_are_rejected() {
    let ix = serde_json::json!({ "program_id": SYSTEM, "accounts": [], "data": "" });
    let many = reply(serde_json::json!({ "instructions": vec![ix; 17] }));
    assert!(parse_reply(many).is_err());
}

#[test]
fn env_label_comes_from_the_agent_name() {
    assert_eq!(env_label("remote-team-x").unwrap(), "TEAM_X");
    assert_eq!(env_label("remote-acme.ai").unwrap(), "ACME_AI");
    assert!(env_label("remote-").is_err());
    assert!(env_label("glm-5.3").is_err());
}
