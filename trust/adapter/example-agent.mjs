// Example remote agent for the reev-agent/v1 protocol. No dependencies: `node example-agent.mjs`.
//
// It is deliberately naive: if the prompt says "send|transfer <n> SOL to <ROLE>", it builds that
// System Program transfer; otherwise it declines. It shows the request and reply shapes and how a
// careless agent fares on the safety exam (it follows injected instructions). Replace `decide()`
// with your agent.
import http from "node:http";

const PORT = Number(process.env.PORT ?? 8788);
const TOKEN = process.env.AGENT_TOKEN; // optional: require `Authorization: Bearer <token>`
const SYSTEM_PROGRAM = "11111111111111111111111111111111";

function transferData(lamports) {
  // System Program Transfer: u32 instruction index 2, u64 lamports, little endian.
  const buf = Buffer.alloc(12);
  buf.writeUInt32LE(2, 0);
  buf.writeBigUInt64LE(BigInt(lamports), 4);
  return buf.toString("base64");
}

// task: { protocol, session_id, prompt, accounts: { ROLE: pubkey }, balances, signer }
function decide(task) {
  const m = task.prompt.match(/(?:send|transfer)\s+([0-9]*\.?[0-9]+)\s*SOL\s+to\s+(?:the\s+recipient\s*\()?([A-Z_]+)/i);
  if (!m || !task.signer) {
    return { instructions: [], refusal: "no clear SOL transfer in the request" };
  }
  const to = task.accounts[m[2]];
  if (!to) return { instructions: [], refusal: `unknown recipient ${m[2]}` };
  const lamports = Math.round(Number(m[1]) * 1e9);
  return {
    instructions: [{
      program_id: SYSTEM_PROGRAM,
      accounts: [
        { pubkey: task.signer, is_signer: true, is_writable: true },
        { pubkey: to, is_signer: false, is_writable: true },
      ],
      data: transferData(lamports),
      encoding: "base64",
    }],
  };
}

http.createServer((req, res) => {
  const reply = (code, body) => { res.writeHead(code, { "content-type": "application/json" }); res.end(JSON.stringify(body)); };
  if (req.method !== "POST") return reply(405, { error: "POST a reev-agent/v1 task" });
  if (TOKEN && req.headers.authorization !== `Bearer ${TOKEN}`) return reply(401, { error: "bad token" });
  let raw = "";
  req.on("data", (c) => (raw += c));
  req.on("end", () => {
    try {
      const task = JSON.parse(raw);
      if (task.protocol !== "reev-agent/v1") return reply(400, { error: `unsupported protocol ${task.protocol}` });
      const out = decide(task);
      console.log(`[${task.session_id}] ${out.instructions.length ? "transfer" : "decline"}: ${task.prompt.slice(0, 70).replace(/\s+/g, " ")}`);
      reply(200, out);
    } catch (e) {
      reply(400, { error: e.message });
    }
  });
}).listen(PORT, "127.0.0.1", () => console.log(`example agent on http://127.0.0.1:${PORT}`));
