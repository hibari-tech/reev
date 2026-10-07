// Publishes Agent Trust Scores to the Solana Agent Registry (8004) on devnet.
//
// Roles are separated on purpose: an "owner" keypair registers the evaluated agents, and the
// evaluator (the same attester that wrote the memo attestations) gives each agent feedback.
// Each feedback carries the trust score and links to the memo transaction that holds the
// report hash, so the registry entry can be traced back to verifiable evidence.
//
// Usage: node publish.mjs            (idempotent: reuses agents recorded in agents-devnet.json)
import fs from "node:fs";
import path from "node:path";
import { Connection, Keypair, LAMPORTS_PER_SOL, PublicKey, SystemProgram, Transaction, sendAndConfirmTransaction } from "@solana/web3.js";
import { SolanaSDK } from "8004-solana";

const here = path.dirname(new URL(import.meta.url).pathname);
const trust = path.join(here, "..");
const STATE = path.join(here, "agents-devnet.json");
const RPC = "https://api.devnet.solana.com";

const loadKey = (file) => Keypair.fromSecretKey(Uint8Array.from(JSON.parse(fs.readFileSync(file, "utf8"))));
function loadOrCreateKey(file) {
  if (fs.existsSync(file)) return loadKey(file);
  const kp = Keypair.generate();
  fs.writeFileSync(file, JSON.stringify(Array.from(kp.secretKey)), { mode: 0o600 });
  return kp;
}

const evaluator = loadKey(path.join(trust, "attester-devnet.json"));
const owner = loadOrCreateKey(path.join(trust, "attester-owner-devnet.json"));
const report = JSON.parse(fs.readFileSync(path.join(trust, "report.json"), "utf8"));
const attestation = JSON.parse(fs.readFileSync(path.join(trust, "site", "attestation.json"), "utf8"));
const state = fs.existsSync(STATE) ? JSON.parse(fs.readFileSync(STATE, "utf8")) : { cluster: "devnet", agents: {} };
const save = () => fs.writeFileSync(STATE, JSON.stringify(state, null, 2) + "\n");

const connection = new Connection(RPC, "confirmed");
console.log(`evaluator ${evaluator.publicKey.toBase58()}  owner ${owner.publicKey.toBase58()}`);

// Fund the owner from the evaluator for registration rent (~0.0065 SOL per agent).
if ((await connection.getBalance(owner.publicKey)) < 0.03 * LAMPORTS_PER_SOL) {
  const tx = new Transaction().add(SystemProgram.transfer({ fromPubkey: evaluator.publicKey, toPubkey: owner.publicKey, lamports: 0.05 * LAMPORTS_PER_SOL }));
  const sig = await sendAndConfirmTransaction(connection, tx, [evaluator]);
  console.log(`funded owner with 0.05 SOL: ${sig}`);
}

const ownerSdk = new SolanaSDK({ cluster: "devnet", signer: owner });
const evaluatorSdk = new SolanaSDK({ cluster: "devnet", signer: evaluator });

// Agent metadata kept inline (agent_uri max 250 bytes) since the evaluated agents have no hosted card.
const agentUri = (name) =>
  "data:application/json," + encodeURIComponent(JSON.stringify({ name, description: `reev-evaluated agent: ${name}` }));

const memoSig = Object.fromEntries(attestation.signatures.map((s) => [s.agent, s.signature]));
const explorer = (sig) => `https://explorer.solana.com/tx/${sig}?cluster=devnet`;
const hashShort = report && attestation.sha256.slice(0, 16);

for (const agent of report.agents) {
  if (agent.trust == null) continue;
  const entry = (state.agents[agent.agent] ??= {});

  if (!entry.asset) {
    const uri = agentUri(agent.agent);
    if (Buffer.byteLength(uri) > 250) throw new Error(`agent_uri too long for ${agent.agent}`);
    const res = await ownerSdk.registerAgent(uri);
    if (!res.asset) throw new Error(`register failed for ${agent.agent}: ${JSON.stringify(res)}`);
    entry.asset = res.asset.toBase58();
    entry.registerSignatures = res.signatures ?? [res.signature].filter(Boolean);
    save();
    console.log(`registered ${agent.agent}: ${entry.asset}`);
  }

  if (entry.feedback?.reportSha256 === attestation.sha256) {
    console.log(`${agent.agent}: feedback for this report already given (index ${entry.feedback.index})`);
    continue;
  }
  const sig = memoSig[agent.agent];
  if (!sig) throw new Error(`no memo attestation for ${agent.agent}`);
  const res = await evaluatorSdk.giveFeedback(new PublicKey(entry.asset), {
    value: agent.trust.toFixed(2),          // trust score, 2 decimals
    score: Math.round(agent.trust),          // 0-100 quality score for reputation engines
    tag1: "reevTrustScore",
    tag2: agent.grade,
    endpoint: `reev-trust/v1 sha256:${hashShort}`,
    feedbackUri: explorer(sig),              // memo tx holding the full report hash
  });
  if (res.success === false) throw new Error(`feedback failed for ${agent.agent}: ${JSON.stringify(res)}`);
  entry.feedback = {
    index: res.feedbackIndex?.toString(),
    signature: res.signature,
    trust: agent.trust,
    grade: agent.grade,
    reportSha256: attestation.sha256,
    memoSignature: sig,
  };
  save();
  console.log(`${agent.agent}: feedback trust=${agent.trust.toFixed(2)} grade=${agent.grade} tx=${res.signature}`);
}

// Read back what the registry now says about each agent.
for (const [name, entry] of Object.entries(state.agents)) {
  try {
    const summary = await evaluatorSdk.getSummary(new PublicKey(entry.asset));
    console.log(`summary ${name}: averageScore=${summary.averageScore} feedbacks=${summary.totalFeedbacks}`);
  } catch (e) {
    console.log(`summary ${name}: not available yet (${e.message})`);
  }
}
