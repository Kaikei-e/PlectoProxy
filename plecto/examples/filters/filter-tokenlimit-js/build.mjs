// Componentize filter.js against this guest's own subset world (wit/world.wit), with every
// WASI-backed engine feature disabled: the result is a "pure component" importing only the
// plecto host-API.
import { componentize } from '@bytecodealliance/componentize-js';
import { copyFile, mkdir, writeFile } from 'node:fs/promises';
import { fileURLToPath } from 'node:url';

const dir = (p) => fileURLToPath(new URL(p, import.meta.url));

// wit/world.wit declares its own package and `use`s `plecto:filter@0.4.0`, so the resolver
// needs that package under wit/deps/. Copy it from the canonical file instead of committing a
// second copy: a vendored snapshot can silently drift from the contract, and a symlink does not
// survive being copied out of the tree — this guest is meant to be lifted out as a starter.
const witDeps = dir('./wit/deps/plecto-filter');
await mkdir(witDeps, { recursive: true });
await copyFile(dir('../../../wit/world.wit'), `${witDeps}/world.wit`);

const { component } = await componentize({
  sourcePath: dir('./filter.js'),
  witPath: dir('./wit'),
  worldName: 'filter-request-body',
  disableFeatures: ['random', 'stdio', 'clocks', 'http', 'fetch-event'],
});

await mkdir(dir('./dist'), { recursive: true });
await writeFile(dir('./dist/filter_tokenlimit_js.wasm'), component);
console.log(`dist/filter_tokenlimit_js.wasm: ${component.length} bytes`);
