"""Compare runtime captures with re-rendered sources and approved JPEG exports."""
from pathlib import Path
from PIL import Image, ImageChops, ImageStat
import json
ROOT = Path(__file__).resolve().parents[2]
OUT = ROOT / 'docs/evidence/s2-web'
APPROVED = Path('/workspace/design/paypal/r3-ledger/shots')
results = []
for actual_path in sorted(OUT.glob('ledger--*.png')):
    actual = Image.open(actual_path).convert('RGB')
    reference = Image.open(OUT / actual_path.name.replace('ledger--', 'reference--')).convert('RGB')
    approved = Image.open(APPROVED / actual_path.name.replace('.png', '.jpg')).convert('RGB')
    row = {'file': actual_path.name, 'actual_size': actual.size, 'source_size': reference.size,
           'approved_size': approved.size}
    for label, other in [('source', reference), ('approved', approved)]:
        width, height = min(actual.width, other.width), min(actual.height, other.height)
        diff = ImageChops.difference(actual.crop((0,0,width,height)), other.crop((0,0,width,height)))
        row[label + '_mae'] = round(sum(ImageStat.Stat(diff).mean)/3, 5)
        row[label + '_bbox'] = diff.getbbox()
    results.append(row)
(OUT / 'visual-diff.json').write_text(json.dumps(results, indent=2) + '\n')
print(json.dumps(results, indent=2))
