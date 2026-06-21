#!/usr/bin/env node
const readline = require('node:readline');

function parseInitLine(line) {
  const obj = JSON.parse(line);
  return {
    systemPrompt: obj.systemPrompt,
    model: obj.model,
    allowedTools: obj.allowedTools || [],
    skills: obj.skills || [],
    mcpServers: obj.mcpServers || {},
  };
}

function parseMessageLine(line) {
  const obj = JSON.parse(line);
  return { sender: obj.sender, content: obj.content };
}

function formatReply(text) {
  return JSON.stringify({ reply: text });
}

function formatError(message) {
  return JSON.stringify({ error: message });
}

async function main() {
  // Lazy require so the test file (which only needs the pure functions above)
  // doesn't need the SDK installed to run.
  const { query } = require('@anthropic-ai/claude-agent-sdk');

  const rl = readline.createInterface({ input: process.stdin, terminal: false });
  const lines = [];
  rl.on('line', (line) => lines.push(line));

  await new Promise((resolve) => rl.once('close', resolve));
  // NOTE: this buffers all input before processing, which only works for a
  // finite test harness. The real run loop (below) processes lines as they
  // arrive instead — this main() is replaced by runLoop() in production use,
  // kept separate so the pure parse/format functions stay testable without
  // a live stdin stream.
}

async function runLoop() {
  const { query } = require('@anthropic-ai/claude-agent-sdk');
  const rl = readline.createInterface({ input: process.stdin, terminal: false });

  let init = null;
  let sessionId = null;

  for await (const line of rl) {
    if (!init) {
      init = parseInitLine(line);
      continue;
    }

    const msg = parseMessageLine(line);
    const prompt = `${msg.sender}: ${msg.content}`;

    try {
      let replyText = '';
      const options = {
        model: init.model,
        systemPrompt: init.systemPrompt,
        allowedTools: init.allowedTools,
        skills: init.skills,
        mcpServers: init.mcpServers,
      };
      if (sessionId) {
        options.resume = sessionId;
      }

      for await (const message of query({ prompt, options })) {
        if (message.type === 'system' && message.session_id) {
          sessionId = message.session_id;
        }
        if (message.type === 'assistant') {
          for (const block of message.message.content) {
            if ('text' in block) {
              replyText += block.text;
            }
          }
        }
      }

      process.stdout.write(formatReply(replyText) + '\n');
    } catch (err) {
      process.stdout.write(formatError(err.message || String(err)) + '\n');
    }
  }
}

module.exports = { parseInitLine, parseMessageLine, formatReply, formatError };

if (require.main === module) {
  runLoop().catch((err) => {
    process.stderr.write(`fatal: ${err.stack || err}\n`);
    process.exit(1);
  });
}
