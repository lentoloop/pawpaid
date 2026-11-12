import fs from 'node:fs/promises';
import { createHash } from 'node:crypto';
import { pathToFileURL } from 'node:url';
export function parseStrings(markdown) {
  const entries = []; const seen = new Set(); const keys = new Map();
  let columns;
  for (const line of markdown.split(/\r?\n/)) {
    if (!line.trim().startsWith('|')) continue;
    const cells = line.trim().replace(/^\||\|$/g, '').split(/(?<!\\)\|/).map(cell => cell.trim().replace(/\\\|/g, '|'));
    if (cells.includes('screen') && cells.includes('zh-Hant') && cells.some(cell => cell.toLowerCase() === 'en')) {
      columns = { screen: cells.indexOf('screen'), zh: cells.indexOf('zh-Hant'), en: cells.findIndex(cell => cell.toLowerCase() === 'en') }; continue;
    }
    if (!columns || cells.every(cell => /^[-:]+$/.test(cell))) continue;
    const screen = cells[columns.screen], en = cells[columns.en], zh = cells[columns.zh];
    if (!screen || !en || !zh) throw Error('Incomplete string row: ' + line);
    const identity = JSON.stringify([screen, zh, en]);
    if (seen.has(identity)) continue;
    seen.add(identity);
    const key = `${screen}.${en.toLowerCase().normalize('NFKD').replace(/[^a-z0-9]+/g, '-').replace(/^-|-$/g, '') || 'text'}`;
    keys.set(key, (keys.get(key) || 0) + 1);
    entries.push({ key, screen, 'zh-Hant': zh, en });
  }
  if (!columns) throw Error('Missing screen / zh-Hant / en table');
  // A repeated English label may have different Chinese copy (for example donor initials).
  // Suffix every member of that collision group so keys do not depend on row order.
  return entries.map(entry => keys.get(entry.key) > 1
    ? { ...entry, key: `${entry.key}-${createHash('sha256').update(entry['zh-Hant']).digest('hex').slice(0, 8)}` }
    : entry);
}
if (process.argv[1] && import.meta.url === pathToFileURL(process.argv[1]).href) {
  const [input, output] = process.argv.slice(2);
  if (!input) throw Error('Usage: node scripts/strings-from-md.mjs STRINGS.md [output.json|output.ts]');
  const entries = parseStrings(await fs.readFile(input, 'utf8'));
  if (output) await fs.writeFile(output, output.endsWith('.ts') ? `export type Locale = "en" | "zh-Hant";\nexport const strings = ${JSON.stringify(entries, null, 2)} as const satisfies readonly { key: string; screen: string; en: string; "zh-Hant": string }[];\n` : JSON.stringify(entries, null, 2) + '\n');
  console.log(`${input}: ${entries.length} entries`);
}
