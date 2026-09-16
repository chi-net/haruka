import { copyFile, mkdir } from 'node:fs/promises';
import { dirname } from 'node:path';

const assets = [
  ['node_modules/htmx.org/dist/htmx.min.js', 'static/vendor/htmx.min.js'],
  ['node_modules/htmx.org/LICENSE', 'static/vendor/htmx.LICENSE'],
  ['node_modules/chart.js/dist/chart.umd.min.js', 'static/vendor/chart.umd.min.js'],
  ['node_modules/chart.js/LICENSE.md', 'static/vendor/chart.LICENSE.md'],
  ['node_modules/tesseract.js/dist/tesseract.min.js', 'static/ocr/tesseract.min.js'],
  ['node_modules/tesseract.js/dist/tesseract.min.js.LICENSE.txt', 'static/ocr/tesseract.LICENSE.txt'],
  ['node_modules/tesseract.js/dist/worker.min.js', 'static/ocr/worker.min.js'],
  ['node_modules/tesseract.js/dist/worker.min.js.LICENSE.txt', 'static/ocr/worker.LICENSE.txt'],
  ['node_modules/tesseract.js-core/tesseract-core-lstm.wasm.js', 'static/ocr/tesseract-core-lstm.wasm.js'],
  ['node_modules/tesseract.js-core/tesseract-core-simd-lstm.wasm.js', 'static/ocr/tesseract-core-simd-lstm.wasm.js'],
  ['node_modules/tesseract.js-core/tesseract-core-relaxedsimd-lstm.wasm.js', 'static/ocr/tesseract-core-relaxedsimd-lstm.wasm.js'],
  ['node_modules/tesseract.js-core/LICENSE', 'static/ocr/tesseract-core.LICENSE'],
  ['node_modules/@tesseract.js-data/chi_sim/4.0.0_best_int/chi_sim.traineddata.gz', 'static/ocr/chi_sim.traineddata.gz'],
  ['node_modules/@tesseract.js-data/eng/4.0.0_best_int/eng.traineddata.gz', 'static/ocr/eng.traineddata.gz'],
  ['assets/receipt-scanner.js', 'static/receipt-scanner.js'],
];

for (const [source, target] of assets) {
  await mkdir(dirname(target), { recursive: true });
  await copyFile(source, target);
}

console.log(`Copied ${assets.length} browser assets.`);
