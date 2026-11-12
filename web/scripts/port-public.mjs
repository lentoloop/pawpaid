// Reproduce the approved Q38 D/F2 fixture markup and isolate its styles.
import fs from 'node:fs/promises';
import postcss from 'postcss';
import { parseStrings } from './strings-from-md.mjs';
const design = process.argv[2] || '/workspace/design/receipt-roots/q38-pet-case';
const mockups = `${design}/redo/r4-frame/F2/mockups`;
const pages = {};
for (const name of ['01-case', '02-rescue-timeline', '04-identity']) {
  pages[name] = {};
  for (const locale of ['zh-Hant', 'en']) {
    const html = await fs.readFile(`${mockups}/${name}${locale === 'en' ? '-en' : ''}.html`, 'utf8');
    pages[name][locale] = { title: html.match(/<title>(.*?)<\/title>/s)[1], html: html.match(/<body>(.*?)<\/body>/s)[1].replaceAll('../../../photos/', '/pawpaid/photos/') };
  }
}
await fs.writeFile(new URL('../src/public/fixtures.json', import.meta.url), JSON.stringify(pages, null, 2) + '\n');
const css = postcss.parse(await fs.readFile(`${mockups}/style.css`, 'utf8'));
css.walkRules(rule => {
  rule.selectors = rule.selectors.map(selector => {
    if (selector === ':root' || selector === 'html' || selector === 'body') return '.public-page';
    if (selector === 'html[lang="zh-Hant"] body') return '.public-page[lang="zh-Hant"]';
    if (selector.startsWith('html[lang="zh-Hant"] ')) return selector.replace('html[lang="zh-Hant"]', '.public-page[lang="zh-Hant"]');
    return `.public-page ${selector}`;
  });
});
await fs.writeFile(new URL('../src/public/public.css', import.meta.url), css.toString() + '\nbody:has(.public-page) { margin: 0; }\n.public-page { min-height: 100vh; }\n.public-page[data-screen="01-case"] { padding-bottom: 40px; }\n.public-page [hidden] { display: none !important; }\n');
const stringsPath = new URL('../src/i18n/strings.ts', import.meta.url);
const original = await fs.readFile(stringsPath, 'utf8');
const existing = JSON.parse(original.slice(original.indexOf('['), original.indexOf('] as const') + 1));
const imported = parseStrings(await fs.readFile(`${design}/redo/r3/D/STRINGS.md`, 'utf8'));
const rows = new Map(existing.map(row => [row.key, row]));
for (const row of imported) rows.set(row.key, row);
await fs.writeFile(stringsPath, `export type Locale = "en" | "zh-Hant";\nexport const strings = ${JSON.stringify([...rows.values()], null, 2)} as const satisfies readonly { key: string; screen: string; en: string; "zh-Hant": string }[];\n`);
console.log(`Ported 6 approved templates and ${imported.length} D strings; scoped CSS.`);
