import { chromium } from '../../web/node_modules/playwright-core/index.mjs';
import assert from 'node:assert/strict';
import { test, beforeEach, afterEach } from 'node:test';
import { spawn } from 'node:child_process';
import { createServer } from 'node:http';
import { readFile, mkdir } from 'node:fs/promises';
import path from 'node:path';
import { randomUUID } from 'node:crypto';

const root = new URL('../../', import.meta.url);
const shots = process.env.PAWPAID_SHOTS_DIR || '/tmp/pp-uifix-shots';
let host, browser, server, base;
const diagnostics = [];
function check(actual, expected, message) {
  try { assert.deepEqual(actual, expected, message); }
  catch (error) { diagnostics.push(error.message); }
}
beforeEach(async () => {
  assert.match(process.env.TEST_DATABASE_URL || '', /^postgres(?:ql)?:\/\/[^/]*127\.0\.0\.1:/, 'throwaway loopback Postgres required');
  host = spawn(process.env.UI_E2E_HOST || new URL('target/debug/examples/ui_e2e', root).pathname, [], { env: process.env, stdio: ['ignore', 'pipe', 'inherit'] });
  const address = await new Promise((resolve, reject) => {
    const timer = setTimeout(() => reject(new Error('Rust host startup timed out')), 20000);
    host.once('exit', code => { clearTimeout(timer); reject(new Error(`Rust host exited ${code}`)); });
    host.stdout.on('data', data => { const match = String(data).match(/UI_E2E_ADDR=(127\.0\.0\.1:\d+)/); if (match) { clearTimeout(timer); resolve(match[1]); } });
  });
  // Serve only the production build. All API traffic goes unchanged to the real Rust host.
  server = createServer(async (req, res) => {
    try {
      const nginx = await readFile(new URL('docker/nginx.conf', root), 'utf8');
      res.setHeader('Content-Security-Policy', nginx.match(/add_header Content-Security-Policy "([^"]+)"/)[1]);
      if (req.url.startsWith('/api/')) {
        const chunks = []; for await (const chunk of req) chunks.push(chunk);
        const response = await fetch(`http://${address}${req.url}`, { method: req.method, headers: req.headers, body: ['GET','HEAD'].includes(req.method) ? undefined : Buffer.concat(chunks) });
        res.writeHead(response.status, Object.fromEntries(response.headers));
        res.end(Buffer.from(await response.arrayBuffer())); return;
      }
      const pathname = new URL(req.url, 'http://localhost').pathname;
      const file = /^\/(assets|fonts)\/[a-zA-Z0-9._,/-]+$/.test(pathname) && !pathname.includes('..') ? pathname.slice(1) : 'index.html';
      const content = await readFile(new URL(`${process.env.UI_E2E_DIST || 'web/dist'}/${file}`, root));
      res.setHeader('Content-Type', file.endsWith('.svg') ? 'image/svg+xml' : file.endsWith('.js') ? 'text/javascript' : file.endsWith('.css') ? 'text/css' : file.endsWith('.woff2') ? 'font/woff2' : file.endsWith('.ttf') ? 'font/ttf' : 'text/html');
      res.end(content);
    } catch (error) { res.writeHead(500); res.end(String(error)); }
  });
  await new Promise(resolve => server.listen(0, '127.0.0.1', resolve));
  base = `http://127.0.0.1:${server.address().port}`;
  browser = await chromium.launch({ executablePath: process.env.CHROME_PATH || '/opt/google/chrome/chrome', args: ['--no-sandbox'] });
  await mkdir(shots, { recursive: true });
});
afterEach(async () => { await browser?.close(); server?.closeAllConnections(); await new Promise(resolve => server ? server.close(resolve) : resolve()); host?.kill(); });

for (const locale of ['en', 'zh-Hant']) test(`real Postgres UI: ${locale} manual invoice through Paid`, async () => {
  const context = await browser.newContext({ locale: locale === 'en' ? 'en-US' : 'zh-TW', viewport: {width:1280,height:960} });
  const page = await context.newPage(); page.setDefaultTimeout(7000);
  const errors = []; page.on('pageerror', error => errors.push(error.message));
  const start = diagnostics.length;
  const clinic = `Synthetic UI Clinic ${locale} ${randomUUID()}`;
  const capture = async name => { await page.evaluate(()=>document.fonts.ready); await page.evaluate(()=>window.scrollTo(0,0)); await page.screenshot({path:path.join(shots, `${locale}-${name}.png`),fullPage:true}); };
  const switchPresent = async screen => {
    await page.waitForLoadState('networkidle');
    check(await page.getByRole('button',{name:'EN',exact:true}).isVisible(),true,`${screen}: EN on first load`);
    check(await page.getByRole('button',{name:'繁中',exact:true}).isVisible(),true,`${screen}: 繁中 on first load`);
    check(await page.locator('html').getAttribute('lang'),locale,`${screen}: locale persisted`);
    if(locale === 'zh-Hant' && await page.locator('.banner-inner').count()) {
      const banner = await page.locator('.banner-inner').innerText();
      check(banner.startsWith('PayPal：'),true,`${screen}: full-width banner colon`);
      check(/[\u3400-\u9fff][.:,;!?]/.test(banner),false,`${screen}: full-width prose punctuation`);
    }
  };
  const paid = locale === 'en' ? 'Payment is confirmed paid.' : '已確認付款完成。';
  async function login(role) {
    await page.locator('select').selectOption(`judge-${role}`);
    await page.locator('input[type=password]').click(); await page.locator('input[type=password]').pressSequentially(`ui-e2e-${role}`);
    await page.locator('input[type=password]').press('Tab');
    await page.locator('button[type=submit]').click();
    await page.locator('button[data-budget]').first().waitFor();
    await switchPresent(`${role} start`);
  }
  try {
    await page.goto(base); await switchPresent('sign-in'); await capture('signin');
    await login('claimant');
    check(await page.locator('main [role=alert], [data-api-error]').filter({visible:true}).count(),0,'no error banner after start settles');
    check(/Missing clinic estimate|Clinic payee unavailable|缺少診所估價|不允許使用此動物醫院/.test(await page.locator('main').innerText()),false,'no fixture configuration errors on start');
    await capture('start');
    await page.locator('button[data-budget]').first().click(); await page.waitForURL('**/02-new-claim.html*'); await switchPresent('new invoice');
    check(await page.locator('main [role=alert], [data-api-error]').filter({visible:true}).count(),0,'no error banner after new invoice loads');
    check(await page.locator('main .inset').count(),0,'no illustrative alternate errors in runtime templates');
    await page.locator('textarea').click(); await page.locator('textarea').pressSequentially(`Synthetic UI invoice ${locale} ${Date.now()}`); await page.locator('textarea').press('Tab');
    // Stable original action labels are independent of translated copy.
    await page.locator('button[data-original-label="Create claim for manual entry"]').click();
    await page.waitForURL('**/03-review.html?claim=*'); await page.locator('input[type=date]').waitFor(); await switchPresent('manual review');
    const fields = page.locator('main input:not([type=checkbox])');
    check(await fields.nth(3).inputValue(),'USD','currency must display USD for a manual draft');
    check(await fields.nth(3).getAttribute('readonly'),'','currency remains read-only');
    for (const [i,value] of [[0,clinic],[1,'2026-11-12'],[2,'7.50'],[5,'7.50']]) {
      const field = fields.nth(i); await field.click();
      if (i === 1) { await field.press('ArrowLeft'); await field.press('1'); await field.fill(value); }
      else { await field.press('ControlOrMeta+A'); await field.pressSequentially(value); }
      await field.press('Tab');
    }
    check(await fields.nth(4).inputValue(),'1','one confirmed total item');
    const entered = await fields.evaluateAll(es=>es.map(e=>e.value));
    if (await page.locator('[data-locale-switch]').count()) {
      await page.getByRole('button',{name:locale === 'en' ? '繁中' : 'EN',exact:true}).click();
      await page.getByRole('button',{name:locale === 'en' ? 'EN' : '繁中',exact:true}).click();
    }
    check(await fields.evaluateAll(es=>es.map(e=>e.value)),entered,'manual values survive both locale switches');
    await page.getByRole('checkbox').check(); await capture('filled');
    const sent = [];
    page.on('request', req => {if(req.url().endsWith('/submit')) sent.push(req.postDataJSON());});
    await page.locator('button[data-original-label="Submit claim"]').click();
    try { await page.waitForURL('**/04-claim-status.html?claim=*',{timeout:4000}); }
    catch {
      await capture('submit-failed');
      const draft = await page.evaluate(async () => {const id = new URL(location.href).searchParams.get('claim'); const r = await fetch(`/api/claims/${id}`,{headers:{Authorization:`Bearer ${sessionStorage.getItem('claimant')}`}}); return r.json();});
      console.log(JSON.stringify({locale,submittedRequests:sent,storedDraft:draft,visibleError:await page.locator('main').innerText(),inputs:await fields.evaluateAll(es=>es.map(e=>({value:e.value,type:e.type}))) }));
      throw new Error(`manual UI submit blocked; ${diagnostics.slice(start).join('; ')}`);
    }
    check(sent.length,1,'one real submit request');
    check(sent[0],{fields:{merchant:clinic,date:'2026-11-12',total_cents:750,currency:'USD',uncertain:[]},items:[{quantity:1,unit_cents:750}]},'UI maps exact API contract');
    await switchPresent('claim status'); await capture('submitted');
    const claimId = new URL(page.url()).searchParams.get('claim');
    check(await page.locator('.ticket-head .date').innerText(),'2026-11-12','status shows the saved date');
    // Clear claimant authentication and sign in through the actual finance form.
    // A distinct test client IP keeps both roles inside the unchanged 30/minute policy.
    await context.setExtraHTTPHeaders({'X-Real-IP':'127.0.0.2'});
    await context.clearCookies(); await page.evaluate(()=>sessionStorage.clear());
    await page.goto(base); await switchPresent('finance sign-in'); await login('finance');
    await page.goto(`${base}/05-finance-queue.html`); await page.locator(`a[href*="${claimId}"]`).first().waitFor(); await switchPresent('finance queue');
    await page.locator(`a[href*="${claimId}"]`).first().click(); await page.waitForURL('**/06-finance-review.html?claim=*'); await switchPresent('finance review');
    check(await page.locator('dt[data-original-label="Clinic invoice date"] + dd').innerText(),'2026-11-12','finance sees the saved date');
    await page.getByRole('checkbox').check(); await capture('finance-review');
    await page.locator('button[data-original-label="Approve claim"]').click();
    await page.locator('button[data-original-label="Send mock payout"]').waitFor(); await capture('approved');
    await page.locator('button[data-original-label="Send mock payout"]').click(); await page.waitForURL('**/07-payout-status.html?claim=*'); await switchPresent('payout status');
    await page.locator('button[data-original-label="Check payment status"]').click();
    await page.locator('.outcome h2').filter({hasText:paid}).waitFor(); await capture('paid');
    const saved = await page.evaluate(async id => { const r = await fetch(`/api/claims/${id}`,{headers:{Authorization:`Bearer ${sessionStorage.getItem('finance')}`}}); if(!r.ok) throw new Error(`saved claim HTTP ${r.status}`); return r.json(); },claimId);
    check(saved.claim.state,'Paid','real stored state Paid'); check(saved.claim.fields.merchant,clinic,'real stored clinic'); check(saved.claim.amount_cents,750,'real stored amount');
    check(errors,[],'no browser errors');
    assert.deepEqual(diagnostics.slice(start),[]);
  } finally { await context.close(); }
});
