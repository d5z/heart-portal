#!/usr/bin/env node
// Minimal stdio MCP server: one tool `hello` returning a fixed string.
const readline = require("readline");

const rl = readline.createInterface({ input: process.stdin, crlfDelay: Infinity });

function send(obj) {
  process.stdout.write(JSON.stringify(obj) + "\n");
}

send({
  jsonrpc: "2.0",
  method: "initialized",
  params: {},
});

rl.on("line", (line) => {
  let msg;
  try {
    msg = JSON.parse(line);
  } catch {
    return;
  }
  if (msg.method === "tools/list") {
    send({
      jsonrpc: "2.0",
      id: msg.id,
      result: {
        tools: [
          {
            name: "hello",
            description: "Returns a short greeting.",
            inputSchema: { type: "object", properties: {}, required: [] },
          },
        ],
      },
    });
    return;
  }
  if (msg.method === "tools/call" && msg.params?.name === "hello") {
    send({
      jsonrpc: "2.0",
      id: msg.id,
      result: { content: [{ type: "text", text: "Hello from hello-tool.js" }] },
    });
    return;
  }
  if (msg.id !== undefined) {
    send({ jsonrpc: "2.0", id: msg.id, error: { code: -32601, message: "Not found" } });
  }
});
