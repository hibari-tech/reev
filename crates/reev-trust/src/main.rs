use anyhow::{anyhow, bail, Context, Result};
use clap::{Parser, Subcommand};
use reev_trust::{
    attestation_memo, audit, benchmark_files, build_report, load_latest_results, report_hash,
    AuditIssue, TrustReport,
};
use serde_json::json;
use solana_client::{rpc_client::RpcClient, rpc_request::RpcRequest};
use solana_sdk::{
    commitment_config::CommitmentConfig,
    instruction::{AccountMeta, Instruction},
    pubkey::Pubkey,
    signature::{read_keypair_file, write_keypair_file, Keypair, Signature},
    signer::Signer,
    transaction::Transaction,
};
use std::{path::PathBuf, str::FromStr, time::SystemTime};

const MEMO_PROGRAM_ID: &str = "MemoSq4gqABAXKb96qnH8TysNcWxMyWCqXgDLGmfcHr";

#[derive(Parser)]
#[command(about = "Agent Trust Score for Solana LLM agents, built on reev benchmark sessions")]
struct Cli {
    #[command(subcommand)]
    command: Command,
}

#[derive(Subcommand)]
enum Command {
    /// Aggregate session logs into a trust report (JSON) and print a leaderboard.
    Score {
        #[arg(long, default_value = "logs/sessions")]
        sessions: PathBuf,
        #[arg(long, default_value = "benchmarks")]
        benchmarks: PathBuf,
        #[arg(long, default_value = "trust/report.json")]
        out: PathBuf,
    },
    /// Write one memo per rated agent with its scores and the report hash.
    Attest {
        #[arg(long, default_value = "trust/report.json")]
        report: PathBuf,
        #[arg(long, default_value = "http://127.0.0.1:8899")]
        rpc: String,
        /// Payer keypair; created (and airdropped on local/devnet) when missing.
        #[arg(long, default_value = "trust/attester.json")]
        keypair: PathBuf,
        /// Only attest this agent.
        #[arg(long)]
        agent: Option<String>,
    },
    /// Re-derive a report's inputs from local files: benchmark hashes and session evidence hashes.
    Audit {
        #[arg(long, default_value = "trust/report.json")]
        report: PathBuf,
        #[arg(long, default_value = "benchmarks")]
        benchmarks: PathBuf,
        #[arg(long, default_value = "logs/sessions")]
        sessions: PathBuf,
    },
    /// Print the attester's address, creating the keypair if needed (no airdrop, no transaction).
    Address {
        #[arg(long, default_value = "trust/attester.json")]
        keypair: PathBuf,
    },
    /// Check that an on-chain attestation matches a report file.
    Verify {
        #[arg(long, default_value = "trust/report.json")]
        report: PathBuf,
        #[arg(long, default_value = "http://127.0.0.1:8899")]
        rpc: String,
        #[arg(long)]
        signature: String,
    },
}

fn main() -> Result<()> {
    match Cli::parse().command {
        Command::Score {
            sessions,
            benchmarks,
            out,
        } => score(sessions, benchmarks, out),
        Command::Attest {
            report,
            rpc,
            keypair,
            agent,
        } => attest(report, rpc, keypair, agent),
        Command::Audit {
            report,
            benchmarks,
            sessions,
        } => run_audit(report, benchmarks, sessions),
        Command::Address { keypair } => {
            println!("{}", load_or_create_keypair(&keypair)?.pubkey());
            Ok(())
        }
        Command::Verify {
            report,
            rpc,
            signature,
        } => verify(report, rpc, signature),
    }
}

fn score(sessions: PathBuf, benchmarks: PathBuf, out: PathBuf) -> Result<()> {
    let results = load_latest_results(&sessions)?;
    let all = benchmark_files(&benchmarks)?;
    let now = SystemTime::now().duration_since(SystemTime::UNIX_EPOCH)?.as_secs();
    let report = build_report(results, all, now);
    let hash = report_hash(&report)?;

    if let Some(dir) = out.parent() {
        std::fs::create_dir_all(dir)?;
    }
    std::fs::write(&out, serde_json::to_vec_pretty(&report)?)?;

    let pct = |x: Option<f64>| x.map(|v| format!("{:.1}%", v * 100.0)).unwrap_or_else(|| "–".into());
    println!("{:<16} {:>10} {:>8} {:>7} {:>6}  coverage (cap / safety)", "agent", "capability", "safety", "trust", "grade");
    let mut agents: Vec<_> = report.agents.iter().collect();
    agents.sort_by(|a, b| b.trust.partial_cmp(&a.trust).unwrap_or(std::cmp::Ordering::Equal));
    for a in agents {
        println!(
            "{:<16} {:>10} {:>8} {:>7} {:>6}  {} / {}",
            a.agent,
            pct(a.capability),
            pct(a.safety),
            a.trust.map(|t| format!("{t:.1}")).unwrap_or_else(|| "–".into()),
            a.grade,
            a.coverage.capability,
            a.coverage.safety,
        );
    }
    println!(
        "\nreport: {}\nsha256: {hash}\nbenchmark set sha256: {}",
        out.display(),
        report.benchmark_set_sha256
    );
    Ok(())
}

fn load_report(path: &PathBuf) -> Result<(TrustReport, String)> {
    let report: TrustReport = serde_json::from_slice(
        &std::fs::read(path).with_context(|| format!("reading {}", path.display()))?,
    )?;
    let hash = report_hash(&report)?;
    Ok((report, hash))
}

fn load_or_create_keypair(path: &PathBuf) -> Result<Keypair> {
    if path.exists() {
        return read_keypair_file(path).map_err(|e| anyhow!("reading keypair {}: {e}", path.display()));
    }
    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir)?;
    }
    let kp = Keypair::new();
    write_keypair_file(&kp, path).map_err(|e| anyhow!("writing keypair: {e}"))?;
    Ok(kp)
}

fn load_or_create_payer(path: &PathBuf, client: &RpcClient) -> Result<Keypair> {
    let payer = if path.exists() {
        read_keypair_file(path).map_err(|e| anyhow!("reading keypair {}: {e}", path.display()))?
    } else {
        if let Some(dir) = path.parent() {
            std::fs::create_dir_all(dir)?;
        }
        let kp = Keypair::new();
        write_keypair_file(&kp, path).map_err(|e| anyhow!("writing keypair: {e}"))?;
        kp
    };
    if client.get_balance(&payer.pubkey())? < 10_000_000 {
        println!("airdropping 1 SOL to attester {}", payer.pubkey());
        let sig = client
            .request_airdrop(&payer.pubkey(), 1_000_000_000)
            .context("airdrop failed (use a funded --keypair on this cluster)")?;
        for _ in 0..30 {
            if client.confirm_transaction(&sig)? {
                break;
            }
            std::thread::sleep(std::time::Duration::from_secs(1));
        }
    }
    Ok(payer)
}

fn attest(report: PathBuf, rpc: String, keypair: PathBuf, only: Option<String>) -> Result<()> {
    let (report, hash) = load_report(&report)?;
    let client = RpcClient::new_with_commitment(rpc.clone(), CommitmentConfig::confirmed());
    let payer = load_or_create_payer(&keypair, &client)?;
    let memo_program = Pubkey::from_str(MEMO_PROGRAM_ID)?;

    let mut attested = 0;
    for agent in report
        .agents
        .iter()
        .filter(|a| a.trust.is_some() && only.as_ref().is_none_or(|o| o == &a.agent))
    {
        let memo = attestation_memo(agent, &hash);
        let ix = Instruction {
            program_id: memo_program,
            accounts: vec![AccountMeta::new_readonly(payer.pubkey(), true)],
            data: memo.clone().into_bytes(),
        };
        let blockhash = client.get_latest_blockhash()?;
        let tx = Transaction::new_signed_with_payer(&[ix], Some(&payer.pubkey()), &[&payer], blockhash);
        let sig = client.send_and_confirm_transaction(&tx)?;
        println!("{}\n  memo: {memo}\n  signature: {sig}", agent.agent);
        attested += 1;
    }
    if attested == 0 {
        bail!("no rated agents to attest (an agent needs both capability and safety results)");
    }
    println!("\nattester: {}  rpc: {rpc}", payer.pubkey());
    Ok(())
}

fn run_audit(report: PathBuf, benchmarks: PathBuf, sessions: PathBuf) -> Result<()> {
    let (report, hash) = load_report(&report)?;
    let issues = audit(&report, &benchmarks, &sessions)?;
    let results: usize = report.agents.iter().map(|a| a.results.len()).sum();
    println!(
        "report {hash}\nchecked {} benchmark files and {results} session evidence files",
        report.benchmarks.len()
    );
    if issues.is_empty() {
        println!("✅ every input and evidence hash matches the report");
        return Ok(());
    }
    for issue in &issues {
        match issue {
            AuditIssue::BenchmarkChanged { id } => println!("❌ benchmark changed since scoring: {id}"),
            AuditIssue::BenchmarkMissing { id } => println!("❌ benchmark file missing: {id}"),
            AuditIssue::BenchmarkSetHashMismatch => println!("❌ benchmark set hash does not match the listed files"),
            AuditIssue::EvidenceChanged { agent, benchmark_id } => {
                println!("❌ session evidence changed: {agent} / {benchmark_id}")
            }
            AuditIssue::EvidenceMissing { agent, benchmark_id } => {
                println!("❌ session evidence missing: {agent} / {benchmark_id}")
            }
        }
    }
    bail!("{} audit issue(s)", issues.len())
}

fn verify(report: PathBuf, rpc: String, signature: String) -> Result<()> {
    let (_, hash) = load_report(&report)?;
    let client = RpcClient::new_with_commitment(rpc, CommitmentConfig::confirmed());
    Signature::from_str(&signature).context("invalid signature")?;
    let tx: serde_json::Value = client.send(
        RpcRequest::GetTransaction,
        json!([signature, {"encoding": "json", "maxSupportedTransactionVersion": 0, "commitment": "confirmed"}]),
    )?;
    let logs = tx["meta"]["logMessages"]
        .as_array()
        .ok_or_else(|| anyhow!("transaction not found or has no logs"))?;
    let memo = logs
        .iter()
        .filter_map(|l| l.as_str())
        .find(|l| l.contains("reev-trust/"))
        .ok_or_else(|| anyhow!("no reev-trust memo in this transaction"))?;
    println!("on-chain memo: {memo}");
    if memo.contains(&format!("sha256={hash}")) {
        println!("✅ report hash matches: {hash}");
        Ok(())
    } else {
        bail!("❌ report hash {hash} does not match the attested memo")
    }
}
