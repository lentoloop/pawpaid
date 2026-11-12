import fs from 'node:fs/promises';
import path from 'node:path';
import { PNG } from 'pngjs';
const shots = process.argv[2] || '/workspace/briefs/pp-d2-1610/shots';
const design = process.argv[3] || '/workspace/design/receipt-roots/q38-pet-case';
const results = [];
for (const name of ['01-case', '02-rescue-timeline', '04-identity']) {
  for (const locale of ['', '-en']) for (const width of [1440,390]) {
    const file = `${name}${locale}-${width}.png`;
    const referencePath = `${design}/redo/${name === '01-case' ? 'r4-frame/F2' : 'r3/D'}/shots/${file}`;
    const reference = PNG.sync.read(await fs.readFile(referencePath));
    const actual = PNG.sync.read(await fs.readFile(path.join(shots,file)));
    const comparison = new PNG({width: reference.width + actual.width, height: Math.max(reference.height, actual.height), fill: true});
    comparison.data.fill(255);
    PNG.bitblt(reference, comparison, 0, 0, reference.width, reference.height, 0, 0);
    PNG.bitblt(actual, comparison, 0, 0, actual.width, actual.height, reference.width, 0);
    await fs.writeFile(path.join(shots,`${file.slice(0,-4)}-compare.png`),PNG.sync.write(comparison));
    let differentPixels = 0;
    let maxChannelDelta = 0;
    if (reference.width === actual.width && reference.height === actual.height) {
      for (let offset = 0; offset < actual.data.length; offset += 4) {
        let different = false;
        for (let channel = 0; channel < 4; channel++) {
          const delta = Math.abs(reference.data[offset + channel] - actual.data[offset + channel]);
          if (delta) different = true;
          maxChannelDelta = Math.max(maxChannelDelta, delta);
        }
        if (different) differentPixels++;
      }
    }
    results.push({file, approved:[reference.width,reference.height],actual:[actual.width,actual.height],differentPixels,maxChannelDelta});
  }
}
await fs.writeFile(path.join(shots,'comparison-sizes.json'), JSON.stringify(results,null,2)+'\n');
console.log(`${results.length} comparisons saved; approved left, app right.`);
