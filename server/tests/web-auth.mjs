import assert from 'node:assert/strict';
import fs from 'node:fs/promises';
import ts from '../../web/node_modules/typescript/lib/typescript.js';
const source = await fs.readFile(new URL('../../web/src/api.ts', import.meta.url), 'utf8');
const { outputText } = ts.transpileModule(source, { compilerOptions: { module: ts.ModuleKind.ESNext } });
const file = new URL('../../web/node_modules/.s4-api-test.mjs', import.meta.url);
await fs.writeFile(file, outputText);
try {
  const { request, configSchema, responseSchema } = await import(file.href);
  const values = new Map();
  globalThis.sessionStorage = { getItem: k => values.get(k) ?? null, setItem: (k,v) => values.set(k,v) };
  let prompted = 0;
  globalThis.prompt = () => { prompted++; return 'scoped-test-token'; };
  let headers;
  globalThis.fetch = async (_, init) => { headers = init.headers; return { ok: true, json: async () => ({ paypal_mode: 'mock', llm_mode: 'mock', event_id: '00000000-0000-0000-0000-000000000001', claimant_id: '00000000-0000-0000-0000-000000000003' }) }; };
  await request('/config', configSchema);
  assert.equal(prompted, 0);
  await request('/payees', configSchema);
  assert.equal(headers.Authorization, 'Bearer scoped-test-token');
  assert.equal(prompted, 1);
  await request('/payees', configSchema);
  assert.equal(prompted, 1);
  values.set('finance', 'finance-test-token');
  await request('/claims/test/payout', configSchema, {});
  assert.equal(headers.Authorization, 'Bearer finance-test-token');
  await request('/claims/test/approve', configSchema, {}, 'explicit-token');
  assert.equal(headers.Authorization, 'Bearer explicit-token');
  const id = '00000000-0000-0000-0000-000000000001';
  const record = responseSchema.parse({
    claim: { id, event_id: id, claimant_id: id, budget_line_id: id,
      receipt_sha256: 'a'.repeat(64), fields: { merchant: null, date: null,
        total_cents: null, currency: null, uncertain: [] }, amount_cents: 1250,
      revision: 2, state: 'Paid' },
    approval: null,
    payout: { id, claim_id: id, approval_id: id, sender_batch_id: 'sender',
      paypal_batch_id: 'batch', item_id: 'item', state: 'Paid',
      reconciliation: [{ source: 'mock-webhook:reversal', status: 'Reversed',
        paypal_batch_id: 'batch', item_id: 'item' }] },
  });
  assert.equal(record.payout.reconciliation.length, 1);
  console.log('PASS: 5 web authorization cases and reconciliation visibility');
} finally {
  await fs.unlink(file);
}
