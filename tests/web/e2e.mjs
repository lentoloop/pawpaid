import { chromium } from '../../web/node_modules/playwright-core/index.mjs';
import ts from '../../web/node_modules/typescript/lib/typescript.js';
import assert from 'node:assert/strict';
import { test, before, after } from 'node:test';
import fs from 'node:fs/promises';
import { existsSync } from 'node:fs';
import { tmpdir } from 'node:os';
import path from 'node:path';
import { spawn } from 'node:child_process';
import { parseStrings } from '../../web/scripts/strings-from-md.mjs';

const root = new URL('../../', import.meta.url);
const port = process.env.UI_E2E_PORT || '4178';
const base = `http://127.0.0.1:${port}`;
const templates = JSON.parse(await fs.readFile(new URL('web/src/templates.json', root), 'utf8'));
const source = await fs.readFile(new URL('web/src/i18n/strings.ts', root), 'utf8');
const moduleURL = code => 'data:text/javascript;base64,' + Buffer.from(code).toString('base64');
const stringsURL = moduleURL(ts.transpileModule(source, { compilerOptions: { module: ts.ModuleKind.ESNext } }).outputText);
const { strings } = await import(stringsURL);
const layer = await fs.readFile(new URL('web/src/i18n/index.ts', root), 'utf8');
const { dictionaries } = await import(moduleURL(ts.transpileModule(layer, { compilerOptions: { module: ts.ModuleKind.ESNext, target: ts.ScriptTarget.ES2022 } }).outputText.replace('"./strings"', JSON.stringify(stringsURL))));
const shots = process.env.PAWPAID_SHOTS_DIR || path.join(tmpdir(), 'pawpaid-i18n-shots');
let preview, browser;
before(async () => {
  preview = spawn(process.execPath, ['node_modules/vite/bin/vite.js', 'preview', '--host', '127.0.0.1', '--port', port, '--strictPort'], { cwd: new URL('web/', root), stdio: 'ignore' });
  for (let n = 0; n < 100; n++) {
    try { if ((await fetch(base)).ok) break; } catch {}
    await new Promise(resolve => setTimeout(resolve, 50));
  }
  browser = await chromium.launch({ executablePath: process.env.CHROME_PATH || ['/opt/google/chrome/chrome', '/usr/bin/google-chrome'].find(existsSync), args: ['--no-sandbox'] });
});
after(async () => { await browser?.close(); preview?.kill(); });

test('dictionaries have identical nonempty key sets and unique source keys', () => {
  assert.equal(new Set(strings.map(row => row.key)).size, strings.length);
  assert.deepEqual(Object.keys(dictionaries.en).sort(), Object.keys(dictionaries['zh-Hant']).sort());
  for (const row of strings) for (const locale of ['en', 'zh-Hant']) assert.ok(row[locale].trim(), row.key);
});
for (const direction of ['C', 'D']) test(`Q38 r3 ${direction} import is deduplicated and stable`, { skip: !existsSync(`/workspace/design/receipt-roots/q38-pet-case/redo/r3/${direction}/STRINGS.md`) }, async () => {
  const md = await fs.readFile(`/workspace/design/receipt-roots/q38-pet-case/redo/r3/${direction}/STRINGS.md`, 'utf8');
  const rows = parseStrings(md);
  if (direction === 'D') {
    const imported = new Map(strings.map(row => [row.key, row]));
    for (const row of rows) assert.deepEqual(imported.get(row.key), row);
  }
  assert.equal(new Set(rows.map(row => row.key)).size, rows.length);
  assert.ok(rows.every(row => row.en && row['zh-Hant'] && row.screen));
  assert.deepEqual(parseStrings(md + '\n' + md), rows);
  const reversed = md.split('\n');
  const header = reversed.findIndex(line => /^\| screen /.test(line));
  assert.deepEqual(parseStrings([...reversed.slice(0, header + 2), ...reversed.slice(header + 2).reverse()].join('\n')).sort((a,b)=>a.key.localeCompare(b.key)), [...rows].sort((a,b)=>a.key.localeCompare(b.key)));
  console.log(`Q38 r3 ${direction}: ${rows.length} entries`);
});

async function visibleCopy(page) {
  await page.waitForLoadState("networkidle");
  return page.locator('#root').evaluate(root => {
    const values = [], walker = document.createTreeWalker(root, NodeFilter.SHOW_TEXT);
    while (walker.nextNode()) {
      const node = walker.currentNode, element = node.parentElement;
      if (element?.checkVisibility() && !element.closest('textarea, [data-locale-switch]')) values.push(node.textContent.trim());
    }
    for (const element of root.querySelectorAll('[aria-label], [placeholder], [title], [data-label]')) {
      if (element.closest('[data-locale-switch]')) continue;
      for (const attr of ['aria-label','placeholder','title','data-label']) if(element.hasAttribute(attr)) values.push(element.getAttribute(attr));
    }
    return values;
  });
}
function noEnglish(values) {
  const english = new Set(strings.filter(row => row.en !== row['zh-Hant']).map(row => row.en));
  assert.deepEqual(values.filter(value => english.has(value)), [], 'untranslated dictionary UI');
}
for (const screen of Object.keys(templates)) test(`${screen}: every fixture switches to zh-Hant and back`, async () => {
  const context = await browser.newContext({ viewport: { width: 1280, height: 800 }, locale: 'en-US' });
  try {
    const page = await context.newPage(); page.setDefaultTimeout(5000); page.setDefaultNavigationTimeout(15000); const errors = [];
    page.on('pageerror', error => errors.push(error.message));
    await page.goto(`${base}/${screen}.html?fixtures`);
    await page.getByRole('button', { name: 'EN', exact: true }).click();
    const english = await visibleCopy(page);
    if(screen==='07-payout-status') assert.doesNotMatch((await page.locator('dl').allTextContents()).join(' '),/\b(?:null|undefined)\b/);
    await page.getByRole('button', { name: '繁中', exact: true }).click();
    assert.equal(await page.locator('html').getAttribute('lang'), 'zh-Hant');
    noEnglish(await visibleCopy(page));
    if(screen==='07-payout-status') assert.doesNotMatch((await page.locator('dl').allTextContents()).join(' '),/\b(?:null|undefined)\b/);
    assert.match(await page.title(), /[\u3400-\u9fff]/);
    assert.equal(await page.evaluate(() => localStorage.getItem('pawpaid.locale')), 'zh-Hant');
    await page.reload();
    noEnglish(await visibleCopy(page));
    await page.getByRole('button', { name: 'EN', exact: true }).click();
    assert.deepEqual(await visibleCopy(page), english);
    assert.equal(await page.locator('html').getAttribute('lang'), 'en');
    assert.deepEqual(errors, []);
    // Capture one representative state per screen, rather than the tall fixture gallery.
    await page.goto(`${base}/${screen}.html?fixtures&state=${{'01-budget':3,'03-review':3,'04-claim-status':5,'05-finance-queue':3,'06-finance-review':2,'07-payout-status':4}[screen] || 0}`);
    await page.evaluate(() => document.fonts.ready);
    await fs.mkdir(shots, { recursive: true });
    for (const locale of ['en', 'zh-Hant']) {
      await page.getByRole('button', { name: locale === 'en' ? 'EN' : '繁中', exact: true }).click();
      assert.equal(await page.evaluate(() => document.documentElement.scrollWidth - innerWidth), 0);
      await page.screenshot({ path: `${shots}/${screen}-${locale}.png`, fullPage: false });
    }
  } finally { await context.close(); }
});

test('browser default, saved preference, sign-in error, and password survive switching', async () => {
  const context = await browser.newContext({ locale: 'zh-TW', viewport: { width:1280, height:800 } });
  try {
    await context.route('**/api/session', route => route.fulfill({status:401,json:{code:'FORBIDDEN',message:'raw server message'}}));
    const page=await context.newPage(); await page.goto(base);
    assert.equal(await page.locator('html').getAttribute('lang'),'zh-Hant');
    await page.locator('input[type=password]').fill('local-test-input');
    await page.getByRole('button',{name:'EN',exact:true}).click();
    assert.equal(await page.locator('input[type=password]').inputValue(),'local-test-input');
    await page.reload();
    assert.equal(await page.locator('html').getAttribute('lang'),'en');
    await page.locator('input[type=password]').fill('local-test-input');
    await page.getByRole('button',{name:'Sign in',exact:true}).click();
    await page.getByRole('alert').filter({hasText:'Wrong account or password'}).waitFor();
    await page.getByRole('button',{name:'繁中',exact:true}).click();
    noEnglish(await visibleCopy(page));
    assert.doesNotMatch(await page.locator('body').innerText(),/raw server message/);
    for(const locale of ['en','zh-Hant']) {
      await page.getByRole('button',{name:locale==='en'?'EN':'繁中',exact:true}).click();
      await page.screenshot({path:`${shots}/signin-${locale}.png`,fullPage:false});
    }
  } finally { await context.close(); }
});

const id='00000000-0000-0000-0000-000000000010';
const eventId='00000000-0000-0000-0000-000000000001';
async function mockAPI(context, fixture) {
  let record={claim:{id,event_id:eventId,claimant_id:eventId,budget_line_id:eventId,receipt_sha256:'a'.repeat(64),fields:{merchant:'Synthetic animal clinic',date:'2026-10-01',total_cents:750,currency:'USD',uncertain:[]},amount_cents:750,revision:1,state:'Draft'},approval:null,payout:null};
  let queried=false;
  await context.addInitScript(()=>sessionStorage.setItem('claimant','local-synthetic-session'));
  await context.route('**/api/**',async route=>{
    const url=new URL(route.request().url()), path=url.pathname.replace('/api','');
    const body=route.request().postDataJSON();
    let data;
    if(path==='/config') data={paypal_mode:fixture==='sandbox'?'sandbox':'mock',llm_mode:'mock',event_id:eventId,claimant_id:eventId};
    else if(path.endsWith('/budget-lines')) data={budget_lines:[{id:eventId,event_id:eventId,category:'Synthetic supplies',approved_cents:10000,reserved_cents:0,paid_cents:0}]};
    else if(path==='/payees') data={payees:[{id:eventId,claimant_id:eventId,display_name:'Synthetic volunteer',sandbox_email:'clinic@example.invalid'}]};
    else if(path===`/events/${eventId}/claims`) data={claims:[record]};
    else if(path==='/claims') { record.claim.state='Draft'; data=record; }
    else if(path.endsWith('/extract')) {
      record.claim.fields.uncertain=fixture==='uncertain'?['date']:[];
      record.claim.state=fixture==='uncertain'?'NeedsInfo':'Draft'; data=record;
    } else if(path.endsWith('/submit')) { record.claim.fields=body.fields; record.claim.state='Submitted'; data=record; }
    else if(path.endsWith('/approve')) {
      record.claim.state='Approved';record.approval={id:eventId,claim_id:id,approver_id:eventId,payee_id:eventId,amount_cents:750,receipt_sha256:'a'.repeat(64),claim_revision:1,expires_at:Math.floor(Date.now()/1000)+600};data=record;
    } else if(path.endsWith('/payout')) {
      const state=fixture==='failed'?'PayoutFailed':fixture==='timeout'&&!queried?'PayoutUnknown':fixture==='pending'||!queried?'PayoutPending':'Paid';
      queried=true;record.claim.state=state;record.payout={id:eventId,claim_id:id,approval_id:eventId,sender_batch_id:'demo-batch',paypal_batch_id:fixture==='sandbox'?'TEST-BATCH-0042':null,item_id:null,state,reconciliation:[]};data=record;
    } else if(path===`/claims/${id}`) data=record;
    else return route.fulfill({status:404,json:{code:'NOT_FOUND',message:'never render this server message'}});
    return route.fulfill({json:data});
  });
}
for (const [fixture,width] of [['sandbox',390],['sandbox',1440],['success',390],['success',1440],['pending',390],['timeout',390],['failed',390],['uncertain',390]]) test(`local mocked ${fixture} flow at ${width}px`,async()=>{
  const context=await browser.newContext({viewport:{width,height:844},locale:'en-US'});
  try {
    await mockAPI(context,fixture);
    const p=await context.newPage();p.setDefaultTimeout(5000);p.setDefaultNavigationTimeout(15000);const errors=[];p.on('pageerror',e=>errors.push(e.message));
    const click=async name=>p.getByRole('button',{name,exact:true}).click();
    await p.goto(`${base}/01-budget.html`);
    await p.locator('button[data-budget]').first().waitFor();
    await p.locator('button[data-budget]').first().click();await p.waitForURL('**/02-new-claim.html');
    await p.locator('textarea').fill('synthetic test invoice');
    await click('Create clinic invoice and extract');await p.waitForURL('**/03-review.html?claim=*');
    if(['success','sandbox'].includes(fixture)) for(const locale of ['zh-Hant','en']) {
      await click(locale==='en'?'EN':'繁中');
      await p.locator('input').first().fill(locale==='zh-Hant'?'示範 Pawhaven 動物醫院':'Synthetic animal clinic');
      await p.locator('#total').fill('7.50');await p.locator('#qty').fill('1');await p.locator('#unit').fill('7.50');
      assert.match(await p.locator('[data-invoice-summary]').innerText(),/USD 7.50/);
      assert.match(await p.locator('[data-invoice-summary]').innerText(),locale==='en'?/Calculated by the server after submit/:/送出後由伺服器核算/);
      assert.match(await p.locator('[data-item-total]').innerText(),/USD 7.50/);
      await p.evaluate(()=>window.scrollTo(0,0));
      await p.screenshot({path:`${shots}/invoice-filled-${locale}-${width}.png`,fullPage:true});
      await p.locator('#total').fill('7.50');await p.locator('#qty').fill('1');await p.locator('#unit').fill('7.50');
    }
    await p.getByRole('checkbox').check();
    if(fixture==='uncertain') {
      assert.equal(await p.getByRole('button',{name:'Submit clinic invoice',exact:true}).isDisabled(),true);
      await p.getByLabel('Clinic invoice date',{exact:true}).fill('2026-10-02');
      await click('Mark resolved');
    }
    await click('繁中');
    await p.getByRole('checkbox').check();
    await click('送出診所發票');await p.waitForURL('**/04-claim-status.html?claim=*');
    noEnglish(await visibleCopy(p));
    if(width===390 && ['success','sandbox'].includes(fixture)) for(const locale of ['zh-Hant','en']) {
      await click(locale==='en'?'EN':'繁中');
      const identity=await p.locator('.ticket-body > .id').first().evaluate(el=>({text:el.textContent,fits:el.scrollWidth<=el.clientWidth,overflow:getComputedStyle(el).textOverflow,idBottom:el.getBoundingClientRect().bottom,dateTop:el.parentElement.querySelector('.date').getBoundingClientRect().top}));
      assert.equal(identity.text,id);assert.ok(identity.fits);assert.notEqual(identity.overflow,'ellipsis');assert.ok(identity.dateTop>=identity.idBottom-1);
      await p.screenshot({path:`${shots}/04-full-id-${fixture}-${locale}-390.png`,fullPage:true});
    }
    await click('EN');
    await p.evaluate(() => sessionStorage.setItem('finance', 'local-synthetic-finance-session'));
    await p.goto(`${base}/06-finance-review.html?claim=${id}`);
    await p.getByRole('checkbox').check();await click('Approve USD 7.50');
    await click('Send sandbox payout');await p.waitForURL('**/07-payout-status.html?claim=*');
    const expected=fixture==='timeout'?'Payment status unknown':fixture==='failed'?'Payment failed':'Payment pending';
    await p.locator('.outcome').filter({hasText:expected}).waitFor();
    if(fixture!=='failed') { await click('Check sandbox payout result'); if(fixture!=='pending') await p.locator('.outcome h2').filter({hasText:'Payment is confirmed paid.'}).waitFor(); }
    if(['success','sandbox'].includes(fixture)) for(const locale of ['zh-Hant','en']) {
      await click(locale==='en'?'EN':'繁中');
      const banner=await p.locator('.sandbox-banner').innerText();
      assert.ok(banner.includes(locale==='en'?`PayPal: ${fixture==='sandbox'?'sandbox':'simulated'}`:`PayPal：${fixture==='sandbox'?'沙盒':'模擬'}`));
      const facts=await p.locator('.outcome .kv').innerText();
      assert.doesNotMatch(facts,/\b(?:null|undefined)\b/);
      const item= p.locator('dl > div').filter({has:p.locator('dt', {hasText:locale==='en'?'Provider item ID':'付款服務商項目 ID'})}).locator('dd');
      assert.equal(await item.innerText(),locale==='en'?'No data':'無資料');
      const source=await p.locator('.source').filter({hasText:locale==='en'?'Last updated':'最後更新'}).first().innerText();
      assert.match(source,/\d{4}-\d{2}-\d{2} (?:[01]\d|2[0-3]):[0-5]\d$/);
      if(width===390) {
        const identity=await p.locator('.ticket-body > .id').first().evaluate(el=>({text:el.textContent,fits:el.scrollWidth<=el.clientWidth,overflow:getComputedStyle(el).textOverflow}));
        assert.equal(identity.text,id);assert.ok(identity.fits);assert.notEqual(identity.overflow,'ellipsis');
      }
      const copy=await p.locator('[data-payout-mode]').innerText();
      assert.equal(copy,fixture==='sandbox'?(locale==='en'?'A PayPal sandbox payout was sent. Batch ID: TEST-BATCH-0042':'已送出 PayPal 沙盒付款。批次編號：TEST-BATCH-0042'):(locale==='en'?'Mock payout. No sandbox payment was sent.':'模擬付款。未送出沙盒付款。'));
      if(fixture==='sandbox') assert.doesNotMatch(copy,/Mock payout|模擬付款/);
      await p.evaluate(()=>window.scrollTo(0,0));
      await p.screenshot({path:`${shots}/paid-${fixture}-${locale}-${width}.png`,fullPage:true});
    }
    await click('繁中');noEnglish(await visibleCopy(p));await click('EN');
    await p.goto(`${base}/05-finance-queue.html`);await p.locator('main .ticket a.btn').first().waitFor();
    assert.equal(await p.locator('main .ticket').count(),1);
    await p.goto(`${base}/04-claim-status.html?claim=00000000-0000-0000-0000-000000000099`);
    await p.locator('[data-api-error]').waitFor();await click('繁中');
    assert.doesNotMatch(await p.locator('body').innerText(),/never render/);
    assert.equal(await p.locator('[data-error-code]').innerText(),'NOT_FOUND');
    assert.deepEqual(errors,[]);
  } finally { await context.close(); }
});

for (const [route, name] of [['/case','01-case'], ['/case/updates','02-rescue-timeline'], ['/case/identity','04-identity']]) {
  for (const locale of ['zh-Hant', 'en']) test(`public ${route}: ${locale} renders, mobile fits, demo only`, async () => {
    const page = await browser.newPage({ viewport: { width: 390, height: 900 } });
    const writes = [];
    page.on('request', request => { if (request.method() !== 'GET') writes.push(request.url()); });
    await page.goto(`${base}${route}?lang=${locale}`);
    await page.evaluate(() => document.fonts.ready);
    assert.equal(await page.locator('html').getAttribute('lang'), locale);
    assert.equal(await page.locator('.public-page main').count(), 1);
    assert.ok((await page.locator('.public-page').innerText()).includes(locale === 'en' ? 'Demo' : '示範'));
    assert.equal(await page.evaluate(() => document.documentElement.scrollWidth <= innerWidth), true);
    assert.equal(await page.locator('input[type=password], form').count(), 0);
    if (route === '/case') {
      const donate = page.locator('.donate button');
      assert.equal(await donate.getAttribute('type'), 'button');
      assert.equal(await donate.getAttribute('aria-disabled'), 'true');
      await donate.dispatchEvent('click');
      assert.equal(new URL(page.url()).pathname, route);
      assert.equal(await page.locator('.track').first().getAttribute('aria-valuenow'), '69');
      await page.locator('a[href*="/case/identity"]').click();
      assert.equal(new URL(page.url()).pathname, '/case/identity');
      await page.goto(`${base}${route}?lang=${locale}`);
    }
    if (route === '/case/identity') {
      assert.equal(await page.locator('.st:has(#sb) button[disabled]').count(), 1);
      assert.equal(await page.locator('.donors .amtv').count(), 5);
      assert.equal(await page.locator('.field input').getAttribute('readonly'), '');
    }
    await fs.mkdir(shots, { recursive: true });
    for (const width of [390, 1440]) {
      await page.setViewportSize({ width, height: 900 });
      await page.evaluate(async () => {
        await document.fonts.ready;
        for (const img of document.images) { img.loading = 'eager'; await img.decode(); }
      });
      await page.screenshot({ path: path.join(shots, `${name}${locale === 'en' ? '-en' : ''}-${width}.png`), fullPage: true });
    }
    const other = locale === 'en' ? 'zh-Hant' : 'en';
    await page.locator(`.lang a[lang="${other}"]`).click();
    assert.equal(await page.locator('html').getAttribute('lang'), other);
    assert.equal(await page.evaluate(() => localStorage.getItem('pawpaid.locale')), other);
    assert.deepEqual(writes, []);
    await page.close();
  });
}

test('public identity fixture states A–D match approved cards', async () => {
  const page = await browser.newPage();
  for (const [state, id] of [['A','sa'], ['B','sb'], ['C','sc'], ['D','sd']]) {
    await page.goto(`${base}/case/identity?lang=en&state=${state}`);
    assert.equal(await page.locator(`#${id}`).isVisible(), true);
    assert.equal(await page.locator('.st:visible, .thanks:visible').count(), 1);
    await page.locator('.lang a[lang="zh-Hant"]').click();
    assert.equal(await page.locator(`#${id}`).isVisible(), true);
  }
  await page.close();
});

test('back office sign-in and styles remain isolated from public routes', async () => {
  const page = await browser.newPage();
  await page.goto(`${base}/`);
  await page.locator('input[type=password]').waitFor();
  assert.equal(await page.locator('input[type=password]').count(), 1);
  assert.equal(await page.locator('.public-page').count(), 0);
  assert.equal(await page.evaluate(() => [...document.styleSheets].some(sheet => [...sheet.cssRules].some(rule => rule.cssText.includes('.public-page')))), false);
  await page.goto(`${base}/case?lang=en`);
  assert.equal(await page.locator('input[type=password]').count(), 0);
  assert.equal(await page.evaluate(() => [...document.styleSheets].some(sheet => sheet.href?.includes('backoffice'))), false);
  await page.close();
});

for (const [kind, status, json, heading] of [
  ['credentials', 401, {message:'Do not render this response'}, 'Wrong account or password'],
  ['network', 503, {message:'Do not render this response'}, 'Sign-in is unavailable right now'],
  ['invalid-response', 200, {unexpected:true}, 'Invalid service response'],
]) test(`F2 sign-in: ${kind} keeps the form and localizes its cause`, async () => {
  const context = await browser.newContext({locale:'en-US'});
  try {
    await context.route('**/api/session', route => route.fulfill({status,json}));
    const page=await context.newPage();
    await page.goto(base);
    await page.locator('input[type=password]').fill('synthetic-test-input');
    await page.getByRole('button',{name:'Sign in',exact:true}).click();
    await page.getByRole('alert').filter({hasText:heading}).waitFor();
    assert.equal(await page.locator('input[type=password]').inputValue(),'synthetic-test-input');
    assert.equal(await page.locator('[aria-invalid=true]').count(),kind==='credentials'?2:0);
    assert.equal(await page.evaluate(()=>sessionStorage.getItem('claimant')||sessionStorage.getItem('finance')),null);
    assert.doesNotMatch(await page.locator('body').innerText(),/Do not render this response/);
    await page.getByRole('button',{name:'繁中',exact:true}).click();
    noEnglish(await visibleCopy(page));
    if(kind==='invalid-response')assert.equal(await page.locator('.alert .chip').innerText(),'INVALID_RESPONSE');
  } finally {await context.close();}
});

test('F2 saved-session failure clears the saved session and offers sign-in', async () => {
  const context=await browser.newContext({locale:'en-US'});
  try {
    await context.addInitScript(()=>sessionStorage.setItem('claimant','synthetic-stale-session'));
    await context.route('**/api/config',route=>route.fulfill({status:503,json:{code:'UNAVAILABLE'}}));
    const page=await context.newPage();await page.goto(base);
    await page.getByRole('heading',{name:'Your saved sign-in could not be restored'}).waitFor();
    assert.equal(await page.evaluate(()=>sessionStorage.getItem('claimant')),null);
    assert.doesNotMatch(await page.locator('main').innerText(),/expired|wrong password/i);
    await page.getByRole('button',{name:'Sign in',exact:true}).click();
    await page.locator('input[type=password]').waitFor();
  } finally {await context.close();}
});

test('F2 queue role denial differs from approval API refusal', async () => {
  const context=await browser.newContext({locale:'en-US'});
  try {
    await mockAPI(context,'success');
    await context.route(`**/api/events/${eventId}/claims`,route=>route.fulfill({status:403,json:{code:'FORBIDDEN',message:'Do not render this response'}}));
    const page=await context.newPage();await page.goto(`${base}/05-finance-queue.html`);
    await page.getByRole('heading',{name:'A finance account is needed'}).waitFor();
    assert.equal(await page.locator('[data-api-error]').count(),0);
    await page.getByRole('button',{name:'繁中',exact:true}).click();
    noEnglish(await visibleCopy(page));
    assert.equal(await page.locator('[data-api-error]').count(),0);
  } finally {await context.close();}
});
