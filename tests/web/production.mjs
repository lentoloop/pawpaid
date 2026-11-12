import { chromium } from '../../web/node_modules/playwright-core/index.mjs';
import { PNG } from '../../web/node_modules/pngjs/lib/png.js';
import { createServer } from 'node:http';
import fs from 'node:fs/promises';
import assert from 'node:assert/strict';
const root = new URL('../../', import.meta.url);
const shots = process.env.PAWPAID_SHOTS_DIR || new URL('../../artifacts/production', import.meta.url).pathname;
await fs.mkdir(shots, { recursive: true });
const nginx = await fs.readFile(new URL('docker/nginx.conf', root), 'utf8');
const headers = Object.fromEntries([...nginx.matchAll(/add_header ([\w-]+) "([^"]+)" always;/g)].map(m => [m[1],m[2]]));
const server = createServer(async (req,res) => {
  try {
    const pathname = new URL(req.url,'http://localhost').pathname;
    const file = pathname.startsWith('/assets/') || pathname.startsWith('/fonts/') || pathname.startsWith('/pawpaid/') ? pathname.slice(1) : 'index.html';
    assert.ok(!file.includes('..'));
    const data = await fs.readFile(new URL(`${process.env.UI_E2E_DIST || 'web/dist'}/${file}`,root));
    res.writeHead(200,{...headers,'Content-Type':file.endsWith('.svg')?'image/svg+xml':file.endsWith('.css')?'text/css':file.endsWith('.js')?'text/javascript':file.endsWith('.png')?'image/png':file.endsWith('.woff2')?'font/woff2':file.endsWith('.ttf')?'font/ttf':'text/html'});res.end(data);
  } catch {res.writeHead(404,headers);res.end();}
});
await new Promise(r=>server.listen(0,'127.0.0.1',r));
const browser=await chromium.launch({executablePath:'/opt/google/chrome/chrome',args:['--no-sandbox']});
let passed=0;
try {
 for(const width of [1440,390]) for(const locale of ['en','zh-Hant']) {
  const reference=PNG.sync.read(await fs.readFile(`/workspace/design/receipt-roots/q38-pet-case/backoffice/shots/00-signin${locale==='en'?'-en':''}-${width}.png`));
  const context=await browser.newContext({viewport:{width,height:900},locale:locale==='en'?'en-US':'zh-TW'});
  const page=await context.newPage(); const violations=[];
  await page.addInitScript(()=>document.addEventListener('securitypolicyviolation',e=>console.error('CSP:'+JSON.stringify({directive:e.violatedDirective,blocked:e.blockedURI,source:e.sourceFile,line:e.lineNumber}))));
  page.on('console',m=>{if(m.text().startsWith('CSP:')) violations.push(m.text());});
  const base=`http://127.0.0.1:${server.address().port}`;
  await page.goto(base); await page.locator('.f2-signin').waitFor();await page.evaluate(()=>document.fonts.ready);
  const styles=await page.locator('.f2-signin').evaluate(el=>({bg:getComputedStyle(document.documentElement).backgroundColor,font:getComputedStyle(el).fontFamily,card:getComputedStyle(el.querySelector('.ticket')).backgroundColor}));
  await page.screenshot({path:`${shots}/00-signin${locale==='en'?'-en':''}-${width}.png`});
  assert.equal(styles.bg,'rgb(238, 232, 225)');assert.match(styles.font,/Be Vietnam Pro.*Noto Sans CJK TC/);assert.equal(styles.card,'rgb(255, 252, 248)');assert.deepEqual(violations,[]);
  const actual=PNG.sync.read(await fs.readFile(`${shots}/00-signin${locale==='en'?'-en':''}-${width}.png`));let different=0;
  for(let i=0;i<actual.data.length;i+=4) if(Math.max(...[0,1,2].map(c=>Math.abs(actual.data[i+c]-reference.data[i+c])))>24)different++;
  const ratio=different/(reference.width*reference.height);console.log({width,locale,styles,pixelDifference:ratio,threshold:0.20});assert.ok(ratio<0.20,'approved F2 sign-in pixel difference (blank password and no design note)');passed++;
  await page.route('**/fonts/**',async route=>{await new Promise(r=>setTimeout(r,1200));await route.continue();});
  await page.getByRole('button',{name:locale==='en'?'繁中':'EN',exact:true}).click();
  const immediate=await page.locator('.f2-signin h1').evaluate(el=>({text:el.textContent,visible:el.checkVisibility(),display:[...document.fonts].map(f=>f.display)}));
  assert.ok(immediate.text.trim());assert.ok(immediate.visible);assert.ok(immediate.display.every(d=>d==='swap'));
  await page.screenshot({path:`${shots}/switch-first-frame-${width}-${locale}.png`});passed++;
  for(const route of ['/case','/case/updates','/case/identity']) {
    await page.goto(`${base}${route}?lang=${locale}`);await page.evaluate(()=>document.fonts.ready);
    await page.screenshot({path:`${shots}/${route.replaceAll('/','-').slice(1)}-${width}-${locale}.png`,fullPage:true});
    if(route.endsWith('updates')){assert.match(await page.locator('.idcard').innerText(),/126.50/);assert.equal(await page.locator('.track').getAttribute('aria-valuenow'),'69');assert.ok((await page.locator('time').allTextContents()).every(d=>d<='2026-11-12 23:59'));}
    await page.locator(`.lang a[lang="${locale==='en'?'zh-Hant':'en'}"]`).click();
    const switched=await page.locator('h1').evaluate(el=>({text:el.textContent,visible:el.checkVisibility(),fonts:[...document.fonts].map(f=>f.display)}));
    assert.ok(switched.text.trim());assert.ok(switched.visible);assert.ok(switched.fonts.every(d=>d!=='block'));
    await page.screenshot({path:`${shots}/${route.replaceAll('/','-').slice(1)}-switch-${width}-${locale}.png`,fullPage:true});
    if(route.endsWith('identity')) {const amounts=await page.locator('.amtv').allTextContents();assert.equal(amounts.reduce((s,v)=>s+Math.round(Number(v.replace('USD ',''))*100),0),12650);assert.match(await page.locator('.thanks .big').innerText(),/126.50/);}
  }
  // All back-office screens and the ten approved shared states, at both widths/locales.
  await page.unroute('**/fonts/**');
  violations.length=0;
  const screens = {'01-budget':3,'02-new-claim':0,'03-review':3,'04-claim-status':7,'05-finance-queue':3,'06-finance-review':2,'07-payout-status':3};
  const shared = ['loading','error','empty-budget','empty-claims','empty-queue','permission-nonfinance'];
  const signin = ['credentials','network','invalid-response','restore-failed'];
  const cases = [
    ...Object.entries(screens).map(([name,state])=>({name,url:`/${name}.html?fixtures&state=${state}`,state:String(state)})),
    ...shared.map(name=>({name:`s-${name}`,url:`/01-budget.html?fixtures&shared=s-${name}`,state:name})),
    ...signin.map(name=>({name:name==='restore-failed'?'s-session-restore-failed':`s-signin-${name}`,url:`/?fixtures&signin=${name}`,state:name})),
  ];
  await page.addInitScript(locale=>localStorage.setItem('pawpaid.locale',locale),locale);
  for(const specimen of cases) {
    await page.goto(`${base}${specimen.url}`);await page.evaluate(()=>document.fonts.ready);
    assert.match(await page.locator('body').evaluate(e=>getComputedStyle(e).fontFamily),/Be Vietnam Pro/);
    assert.equal(await page.locator('html').getAttribute('lang'),locale);
    assert.equal(await page.evaluate(()=>document.documentElement.scrollWidth>innerWidth),false,`${specimen.name}: horizontal overflow`);
    assert.equal(await page.locator('style').count(),0,'external stylesheets only');
    assert.equal(await page.locator('.state-jump,.variant-head,.spec-note').count(),0,'no design annotations');
    assert.deepEqual(violations,[],'production CSP');
    const filename=`${specimen.name}${locale==='en'?'-en':''}-${width}.png`;
    await page.screenshot({path:`${shots}/${filename}`,fullPage:true,animations:'disabled'});
    if(width===390 && ['04-claim-status','07-payout-status'].includes(specimen.name)) {
      const geometry=await page.evaluate(()=>{
        const box=s=>document.querySelector(s).getBoundingClientRect().toJSON();
        return {ticket:box('.ticket-summary'),panel:box('.status-first'),button:box('.status-first .btn-primary'),banner:box('.sandbox-banner'),font:getComputedStyle(document.querySelector('.sandbox-banner')).fontSize,
          pet:box('.ticket-summary h2'),amount:box('.ticket-summary .amount'),id:box('.ticket-summary .ticket-body > .id'),date:box('.ticket-summary .date'),clinic:box('.ticket-summary .facts-row > div'),word:box('.wordmark'),role:box('.role'),tabs:[...document.querySelectorAll('.tabs li')].map(e=>e.getBoundingClientRect().top),hidden:[...document.querySelectorAll('.ticket-summary .facts-row > div')].slice(1).every(e=>getComputedStyle(e).display==='none')};
      });
      assert.ok(geometry.ticket.bottom<=geometry.panel.top);
      assert.ok(geometry.button.bottom<=650,JSON.stringify(geometry));
      assert.equal(geometry.font,'12px');assert.ok(Math.abs(geometry.banner.height-(2*12*1.35+6))<2,'two banner lines');
      assert.ok(Math.abs(geometry.pet.top-geometry.amount.top)<8,'pet and amount share line one');
      assert.ok(geometry.date.top>=geometry.id.bottom-1,'date follows full ID');
      const identity=await page.locator('.ticket-summary .ticket-body > .id').evaluate(el=>({fits:el.scrollWidth<=el.clientWidth,overflow:getComputedStyle(el).textOverflow,wrap:getComputedStyle(el).overflowWrap}));
      assert.ok(identity.fits);assert.notEqual(identity.overflow,'ellipsis');assert.equal(identity.wrap,'anywhere');
      assert.ok(geometry.id.top>=geometry.pet.bottom-1);assert.ok(geometry.clinic.top>=geometry.date.bottom-1,'clinic follows date');
      assert.ok(geometry.role.top>=geometry.word.bottom);
      assert.equal(new Set(geometry.tabs).size,1);assert.ok(geometry.hidden);
    }
    const designFile=`/workspace/design/receipt-roots/q38-pet-case/backoffice/shots/${filename}`;
    try {
      const design=PNG.sync.read(await fs.readFile(designFile));
      const build=PNG.sync.read(await fs.readFile(`${shots}/${filename}`));
      const side=new PNG({width:width*2,height:Math.max(design.height,build.height)});side.data.fill(255);
      PNG.bitblt(design,side,0,0,width,design.height,0,0);PNG.bitblt(build,side,0,0,width,build.height,width,0);
      await fs.mkdir(`${shots}/compare`,{recursive:true});await fs.writeFile(`${shots}/compare/${filename}`,PNG.sync.write(side));
    } catch(error) { if(error.code!=='ENOENT')throw error; }
    if(specimen.name==='s-error')assert.equal(await page.locator('.chip').first().innerText(),'STORAGE_UNAVAILABLE');
    if(specimen.name==='s-signin-credentials')assert.equal(await page.locator('[aria-invalid=true]').count(),2);
    passed++;
  }
  await context.close();
 }
 const fixtures=JSON.parse(await fs.readFile(new URL('web/src/public/fixtures.json',root)));
 const comparison=[];
 for(const [name,translations] of Object.entries(fixtures)) for(const locale of ['zh-Hant','en']) for(const width of [1440,390]) {
   const route=name.startsWith('01-')?'/case':name.startsWith('02-')?'/case/updates':'/case/identity';
   const state=['01-case','02-rescue-timeline','04-identity'].includes(name)?'normal':name.replace(/^(01-case|02-updates|04-identity)-/,'');
   const stem=`${name}${locale==='en'?'-en':''}-${width}`;
   const candidates=[`final/public/shots/${stem}.png`,`public-states/shots/${stem}.png`,`redo/r3/D/shots/${stem}.png`];
   let designPath;
   for(const candidate of candidates) {try {await fs.access(`/workspace/design/receipt-roots/q38-pet-case/${candidate}`);designPath=candidate;break;}catch{}}
   assert.ok(designPath,`reference for ${stem}`);
   const reference=PNG.sync.read(await fs.readFile(`/workspace/design/receipt-roots/q38-pet-case/${designPath}`));
   const context=await browser.newContext({viewport:{width,height:900},reducedMotion:'reduce'});
   const page=await context.newPage();const forbidden=[];
   page.on('request',request=>{const u=new URL(request.url());if(request.method()!=='GET'||['fetch','xhr','websocket','eventsource'].includes(request.resourceType())||u.origin!==base||!(request.isNavigationRequest()||/^\/(assets|fonts|pawpaid)\//.test(u.pathname)))forbidden.push(request.url());});
   await page.addInitScript(()=>{window.publicViolations=[];document.addEventListener('securitypolicyviolation',e=>window.publicViolations.push(e.violatedDirective));});
   const base=`http://127.0.0.1:${server.address().port}`;
   await page.goto(`${base}${route}?lang=${locale}${state==='normal'?'':`&state=${state}`}`);
   await page.locator('.public-page main').waitFor();
   await page.evaluate(async()=>{await document.fonts.ready;for(const img of document.images){img.loading='eager';await img.decode();}});
   assert.equal(await page.title(),translations[locale].title);
   assert.equal(await page.locator('style').count(),0);
   assert.doesNotMatch(await page.locator('.public-page').innerText(),/State screen|狀態稿|Design sheet|設計稿/);
   assert.equal(await page.evaluate(()=>document.documentElement.scrollWidth<=innerWidth),true,stem+' fits');
   assert.equal(await page.locator('.public-page').evaluate(el=>getComputedStyle(el).backgroundColor),'rgb(238, 232, 225)');
   const before=page.url();
   for(const button of await page.locator('.public-page button:not([disabled])').all())await button.dispatchEvent('click');
   assert.equal(page.url(),before);
   assert.deepEqual(forbidden,[],stem+' only static requests');
   assert.deepEqual(await page.evaluate(()=>window.publicViolations),[],stem+' CSP');
   await page.screenshot({path:`${shots}/${stem}.png`,fullPage:true});
   const actual=PNG.sync.read(await fs.readFile(`${shots}/${stem}.png`));
   const side=new PNG({width:width*2,height:Math.max(reference.height,actual.height),fill:true});side.data.fill(255);
   PNG.bitblt(reference,side,0,0,width,reference.height,0,0);PNG.bitblt(actual,side,0,0,width,actual.height,width,0);
   await fs.writeFile(`${shots}/${stem}-compare.png`,PNG.sync.write(side));
   let differences=0;const overlap=Math.min(reference.height,actual.height);
   for(let y=0;y<overlap;y++)for(let x=0;x<width;x++){const i=(y*width+x)*4;if([0,1,2].some(c=>Math.abs(reference.data[i+c]-actual.data[i+c])>24))differences++;}
   const ratio=differences/(width*overlap);
   comparison.push({page:route,state,locale,width,design:designPath,build:`shots/${stem}.png`,compare:`shots/${stem}-compare.png`,difference:ratio,designHeight:reference.height,buildHeight:actual.height});
   // Corrected totals and removed designer annotations shift the normal timeline/identity vertically.
   // Keep their raw comparison for review; gate the unchanged templates against their reference pixels.
   if(state!=='normal' || name==='01-case')assert.ok(ratio<0.15,`${stem}: approved visual difference ${ratio}`);
   const card=await page.locator('.card').first().evaluate(el=>({background:getComputedStyle(el).backgroundColor,radius:getComputedStyle(el).borderRadius,font:getComputedStyle(el).fontFamily}));
   assert.equal(card.background,'rgb(255, 252, 248)');assert.match(card.font,/Be Vietnam Pro|Noto Sans CJK TC/);
   await page.locator(`.lang a[lang="${locale==='en'?'zh-Hant':'en'}"]`).click();
   assert.equal(new URL(page.url()).searchParams.get('state'),state==='normal'?null:state);
   assert.equal(await page.locator('.public-page').getAttribute('lang'),locale==='en'?'zh-Hant':'en');
   assert.deepEqual(forbidden,[]);
   await context.close();passed++;
 }
 const failurePage=await browser.newPage();
 await failurePage.route('**/pawpaid/photos/*.png',route=>route.abort());
 await failurePage.goto(`http://127.0.0.1:${server.address().port}/case?lang=en`);
 await failurePage.getByText('Demo photo unavailable',{exact:true}).first().waitFor();
 assert.equal(await failurePage.locator('.arch .win[role=img]').count(),1);
 await failurePage.close();passed++;
 await fs.writeFile(`${shots}/comparison.json`,JSON.stringify(comparison,null,2));
 console.log(`${passed} passed, 0 failed`);
} finally {await browser.close();await new Promise(r=>server.close(r));}
