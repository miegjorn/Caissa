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
    // Extended-thinking token budget for the aporia discipline. Absent
    // (undefined) when the agent's Fondament definition doesn't declare the
    // aporia modifier — omitted from the query() options in that case so
    // extended thinking stays off exactly like before this was wired.
    maxThinkingTokens: obj.maxThinkingTokens,
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
        // Without this, the SDK defaults permissionMode to 'default', which
        // prompts for tool approval on every dangerous operation. There is no
        // TTY attached to this headless sidecar to answer such a prompt, so
        // the very first tool call (e.g. Guilhem's own persona-mandated
        // list_context_nodes call on turn one) hangs the query() call forever.
        // Root-caused via a direct curl to /matrix/reply (deterministic
        // 25s+ hang, isolated to this process, no other component involved).
        permissionMode: 'bypassPermissions',
        allowDangerouslySkipPermissions: true,
      };
      if (init.maxThinkingTokens !== undefined && init.maxThinkingTokens !== null) {
        options.maxThinkingTokens = init.maxThinkingTokens;
      }
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
