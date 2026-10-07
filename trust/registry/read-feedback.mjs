// Reads every feedback stored for the evaluated agents, as any third party would.
import fs from "node:fs";
import { PublicKey } from "@solana/web3.js";
import { SolanaSDK } from "8004-solana";

const state = JSON.parse(fs.readFileSync(new URL("./agents-devnet.json", import.meta.url), "utf8"));
const sdk = new SolanaSDK({ cluster: "devnet" }); // read-only, no signer
for (const [name, e] of Object.entries(state.agents)) {
  for (const f of await sdk.readAllFeedback(new PublicKey(e.asset))) {
    const value = Number(f.value) / 10 ** f.valueDecimals;
    console.log(`${name}: value=${value} score=${f.score} tags=${f.tag1}/${f.tag2} client=${f.client.toBase58().slice(0, 8)}… endpoint="${f.endpoint ?? ""}"`);
    console.log(`  feedbackUri=${f.feedbackUri ?? ""}`);
  }
}
