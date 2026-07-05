// Componentize filter.js against the plecto:filter WIT, with every WASI-backed engine
// feature disabled: the result is a "pure component" importing only the plecto host-API.
import { componentize } from '@bytecodealliance/componentize-js';
import { mkdir, writeFile } from 'node:fs/promises';
import { fileURLToPath } from 'node:url';

const dir = (p) => fileURLToPath(new URL(p, import.meta.url));

const { component } = await componentize({
  sourcePath: dir('./filter.js'),
  witPath: dir('../../../wit'),
  worldName: 'filter-body',
  disableFeatures: ['random', 'stdio', 'clocks', 'http', 'fetch-event'],
});

await mkdir(dir('./dist'), { recursive: true });
await writeFile(dir('./dist/filter_hello_js.wasm'), component);
console.log(`dist/filter_hello_js.wasm: ${component.length} bytes`);
