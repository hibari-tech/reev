// Reads back each agent's reputation summary from the 8004 registry (via its devnet indexer),
// retrying while the indexer catches up.
import fs from "node:fs";
import { PublicKey } from "@solana/web3.js";
import { SolanaSDK } from "8004-solana";

const state = JSON.parse(fs.readFileSync(new URL("./agents-devnet.json", import.meta.url), "utf8"));
const sdk = new SolanaSDK({ cluster: "devnet" }); // read-only
const sleep = (ms) => new Promise((r) => setTimeout(r, ms));
for (let attempt = 1; attempt <= 12; attempt++) {
  const rows = [];
  for (const [name, e] of Object.entries(state.agents)) {
    const s = await sdk.getSummary(new PublicKey(e.asset));
    rows.push({ name, asset: e.asset, averageScore: s.averageScore, feedbacks: s.totalFeedbacks });
  }
  if (rows.every((r) => r.feedbacks > 0) || attempt === 12) {
    console.log(`attempt ${attempt}`);
    console.table(rows);
    break;
  }
  await sleep(15000);
}
