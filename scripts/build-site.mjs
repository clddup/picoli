import { execFileSync } from 'node:child_process';
import { copyFile, lstat, mkdir, readFile, readdir, rm, writeFile } from 'node:fs/promises';
import { dirname, join, relative, resolve } from 'node:path';
import { fileURLToPath } from 'node:url';

const root = fileURLToPath(new URL('../', import.meta.url));
const dist = join(root, 'dist');
const git = (...args) => execFileSync('git', args, { cwd: root, encoding: 'utf8' }).trim();

// Keep this list explicit: never publish samples, user images, tests or build caches.
const webFiles = [
  'index.html',
  'styles.css',
  'app.js',
  'engine-client.js',
  'compress-worker.js',
  'files.js',
  'download.js',
  'favicon.svg',
  'pkg/picoli_wasm.js',
  'pkg/picoli_wasm_bg.wasm',
  'pkg/picoli_wasm.d.ts',
  'pkg/picoli_wasm_bg.wasm.d.ts',
];
const licenseFiles = [
  'LICENSE',
  'THIRD-PARTY-NOTICES.md',
  'Cargo.lock',
  'vendor/jpeg-encoder/PATCHES.md',
  'vendor/zune-jpeg/PATCHES.md',
  'vendor/libdeflater/PATCHES-PICOLI.md',
  'vendor/jpeg-encoder/LICENSE-MIT',
  'vendor/jpeg-encoder/LICENSE-APACHE',
  'vendor/jpeg-encoder/LICENSE-IJG',
  'vendor/zune-jpeg/LICENSE-MIT',
  'vendor/zune-jpeg/LICENSE-APACHE',
  'vendor/zune-jpeg/LICENSE-ZLIB',
  'vendor/libdeflater/LICENSE',
  'vendor/licenses/libdeflate-COPYING',
];
const files = [
  ...webFiles.map((file) => [`web/${file}`, file]),
  // Retain these paths so the notice's relative license links still resolve.
  ...licenseFiles.map((file) => [file, file]),
];

// Fail before clearing a previous build if a required input is missing or a symlink.
for (const [source] of files) {
  if (!(await lstat(join(root, source))).isFile()) {
    throw new Error(`Site input must be a regular file: ${source}`);
  }
}

// Inspect only cached, locked dependencies. CI's WASM build fetches them first.
let metadata;
try {
  metadata = JSON.parse(execFileSync('cargo', [
    'metadata', '--locked', '--offline', '--format-version', '1',
    '--filter-platform', 'wasm32-unknown-unknown',
  ], { cwd: root, encoding: 'utf8', maxBuffer: 16 * 1024 * 1024, stdio: ['ignore', 'pipe', 'inherit'] }));
} catch (cause) {
  throw new Error('Cannot collect WASM dependency licenses offline. Run cargo fetch --locked to cache all workspace dependency manifests first.', { cause });
}
const packages = new Map(metadata.packages.map((pkg) => [pkg.id, pkg]));
const nodes = new Map(metadata.resolve.nodes.map((node) => [node.id, node]));
const wasmPackage = metadata.packages.find((pkg) => pkg.name === 'picoli-wasm' && metadata.workspace_members.includes(pkg.id));
if (!wasmPackage) throw new Error('Cannot find workspace package picoli-wasm in Cargo metadata.');
const reachable = new Set();
const pending = [wasmPackage.id];
while (pending.length) {
  const id = pending.pop();
  if (reachable.has(id)) continue;
  reachable.add(id);
  const node = nodes.get(id);
  if (!node) throw new Error(`Missing resolved WASM dependency: ${packages.get(id)?.name || 'unknown package'}`);
  // Build dependencies are included conservatively; test-only dependencies are not shipped.
  pending.push(...node.deps.filter((dep) => dep.dep_kinds.some(({ kind }) => kind !== 'dev')).map((dep) => dep.pkg));
}

// Some crates place notices below licenses/ or inside bundled C-library trees.
// Read regular files only; never follow directory or file symlinks.
async function findLicenseFiles(directory, insideLicenseDirectory = false) {
  const found = [];
  for (const entry of await readdir(directory, { withFileTypes: true })) {
    const path = join(directory, entry.name);
    if (entry.isDirectory() && !entry.name.startsWith('.') && entry.name !== 'target') {
      found.push(...await findLicenseFiles(path, insideLicenseDirectory || /^licen[sc]es?$/i.test(entry.name)));
    } else if (entry.isFile() && (insideLicenseDirectory || /^(LICENSE|LICENCE|COPYING|NOTICE|COPYRIGHT)/i.test(entry.name))) {
      found.push(path);
    }
  }
  return found;
}

const thirdPartyPackages = [...reachable]
  .filter((id) => !metadata.workspace_members.includes(id))
  .map((id) => packages.get(id))
  .sort((a, b) => `${a.name}@${a.version}`.localeCompare(`${b.name}@${b.version}`, 'en'));
const sections = [
  'Third-party licenses for the locked picoli-wasm dependency graph (wasm32-unknown-unknown).',
  'Build dependencies are included; Picoli itself is covered by the accompanying LICENSE.',
  'This software is based in part on the work of the Independent JPEG Group.',
];
for (const pkg of thirdPartyPackages) {
  const directory = dirname(pkg.manifest_path);
  const paths = new Set(await findLicenseFiles(directory));
  if (pkg.license_file) {
    const path = resolve(directory, pkg.license_file);
    if (!(await lstat(path)).isFile()) throw new Error(`Declared license_file is not a regular file: ${pkg.name} ${pkg.version}`);
    paths.add(path);
  }
  if (!paths.size) throw new Error(`Unhandled license notices: ${pkg.name} ${pkg.version} has no license_file or regular license/notice files.`);
  sections.push(`\n${'='.repeat(72)}\n${pkg.name} ${pkg.version}\nLicense: ${pkg.license || 'See full notices below'}\n`);
  for (const path of [...paths].sort()) {
    const text = await readFile(path, 'utf8');
    if (!text.trim()) throw new Error(`Empty license notice: ${pkg.name} ${pkg.version} (${relative(directory, path)})`);
    sections.push(`--- ${relative(directory, path).replaceAll('\\', '/')} ---\n${text}`);
  }
}
sections.push(`\n${'='.repeat(72)}\nlibdeflate C library — vendor/licenses/libdeflate-COPYING\n${await readFile(join(root, 'vendor/licenses/libdeflate-COPYING'), 'utf8')}`);
const thirdPartyLicenses = `${sections.join('\n')}\n`;

const gitSha = process.env.GITHUB_SHA || git('rev-parse', 'HEAD');
const repositoryUrl = process.env.GITHUB_REPOSITORY
  ? `${process.env.GITHUB_SERVER_URL || 'https://github.com'}/${process.env.GITHUB_REPOSITORY}`
  : git('remote', 'get-url', 'origin').replace(/^git@github\.com:/, 'https://github.com/').replace(/\.git$/, '');
if (!/^[a-f0-9]{40}$/i.test(gitSha)) throw new Error('Expected a full Git commit SHA for versioned assets.');
const version = { gitSha, buildTimestamp: new Date().toISOString(), repositoryUrl };
// A new page must never pair cached JavaScript with another commit's WASM.
const assetDirectory = `assets/${gitSha}`;

await rm(dist, { recursive: true, force: true });
await mkdir(dist, { recursive: true });
for (const [source, destination] of files) {
  const target = source.startsWith('web/') && destination !== 'index.html'
    ? join(assetDirectory, destination) : destination;
  const output = join(dist, target);
  await mkdir(dirname(output), { recursive: true });
  if (destination === 'index.html') {
    let html = await readFile(join(root, source), 'utf8');
    for (const asset of ['styles.css', 'app.js', 'favicon.svg']) {
      const relativeUrl = `./${asset}`;
      if (!html.includes(`\"${relativeUrl}\"`)) throw new Error(`Missing HTML asset reference: ${asset}`);
      html = html.replaceAll(`\"${relativeUrl}\"`, `\"./${assetDirectory}/${asset}\"`);
    }
    await writeFile(output, html);
  } else {
    await copyFile(join(root, source), output);
  }
}
for (const destination of ['version.json', `${assetDirectory}/version.json`]) {
  await writeFile(join(dist, destination), `${JSON.stringify(version, null, 2)}\n`);
}
await writeFile(join(dist, 'third-party-licenses.txt'), thirdPartyLicenses);
await writeFile(join(dist, '.nojekyll'), '');
console.log(`Built ${files.length} site files and notices for ${thirdPartyPackages.length} third-party packages in ${dist} (${gitSha}).`);
