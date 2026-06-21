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

    try {
      const msg = parseMessageLine(line);
      const prompt = `${msg.sender}: ${msg.content}`;
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
