const test = require('node:test');
const assert = require('node:assert');
const { parseInitLine, parseMessageLine, formatReply, formatError } = require('./agent-sidecar.js');

test('parseInitLine extracts systemPrompt, model, allowedTools, skills, mcpServers', () => {
  const line = JSON.stringify({
    systemPrompt: 'You are Guilhem.',
    model: 'claude-sonnet-4-6',
    allowedTools: ['Bash', 'mcp__farga__search_signals'],
    skills: ['superpowers:systematic-debugging'],
    mcpServers: { farga: { type: 'http', url: 'http://farga:7500/mcp' } },
  });
  const init = parseInitLine(line);
  assert.strictEqual(init.systemPrompt, 'You are Guilhem.');
  assert.strictEqual(init.model, 'claude-sonnet-4-6');
  assert.deepStrictEqual(init.allowedTools, ['Bash', 'mcp__farga__search_signals']);
  assert.deepStrictEqual(init.skills, ['superpowers:systematic-debugging']);
  assert.deepStrictEqual(init.mcpServers, { farga: { type: 'http', url: 'http://farga:7500/mcp' } });
});

test('parseMessageLine extracts sender and content', () => {
  const line = JSON.stringify({ sender: '@pierre-luc:occitane.guilhem', content: 'hello' });
  const msg = parseMessageLine(line);
  assert.strictEqual(msg.sender, '@pierre-luc:occitane.guilhem');
  assert.strictEqual(msg.content, 'hello');
});

test('formatReply produces a single-line JSON object with a reply field', () => {
  const line = formatReply('the response text');
  assert.strictEqual(JSON.parse(line).reply, 'the response text');
  assert.ok(!line.includes('\n'), 'reply line must not contain embedded newlines');
});

test('formatError produces a single-line JSON object with an error field', () => {
  const line = formatError('boom');
  assert.strictEqual(JSON.parse(line).error, 'boom');
});
