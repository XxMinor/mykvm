import test from 'node:test'
import assert from 'node:assert/strict'
import fs from 'node:fs'
import os from 'node:os'
import path from 'node:path'
import { assembleUpdaterManifest, obsoleteBetaAssets, stageUpdaterAssets } from './assemble-updater-manifest.mjs'

function fixture(t) {
  const directory = fs.mkdtempSync(path.join(os.tmpdir(), 'mykvm-updater-'))
  t.after(() => fs.rmSync(directory, { recursive: true, force: true }))
  for (const name of ['mykvm.app.tar.gz', 'mykvm_0.9.14_x64-setup.exe',
    'mykvm_0.9.14_amd64.AppImage', 'mykvm_0.9.14_amd64.deb', 'mykvm-0.9.14.x86_64.rpm']) {
    fs.writeFileSync(path.join(directory, name), `package:${name}`)
    fs.writeFileSync(path.join(directory, `${name}.sig`), `signature:${name}`)
  }
  return directory
}

test('stable manifests preserve version-tag URLs and Linux package targets', (t) => {
  const directory = fixture(t)
  const result = assembleUpdaterManifest({ directory, repo: 'XxMinor/mykvm',
    tag: 'v0.9.14', version: '0.9.14', channel: 'stable' })
  assert.equal(Object.keys(result.platforms).length, 6)
  for (const value of Object.values(result.platforms)) assert.ok(value.url.includes('/download/v0.9.14/'))
  assert.ok(result.platforms['windows-x86_64'].url.endsWith('/mykvm-windows-x64-setup.exe'))
  assert.ok(result.platforms['linux-x86_64-deb'].url.endsWith('.deb'))
})

test('workflow artifacts contain flat packages and matching signatures only', (t) => {
  const source = fixture(t), directory = path.join(source, 'staged')
  fs.writeFileSync(path.join(source, 'signing-key.txt'), 'do not upload')
  stageUpdaterAssets(fs.readdirSync(source).filter((name) => !name.endsWith('.sig'))
    .map((name) => path.join(source, name)), directory)
  assert.equal(fs.readdirSync(directory).length, 10)
  assert.equal(fs.existsSync(path.join(directory, 'signing-key.txt')), false)
  assert.equal(fs.readFileSync(path.join(directory, 'mykvm.app.tar.gz.sig'), 'utf8'), 'signature:mykvm.app.tar.gz')
})

test('rolling beta manifests point only to versioned assets in the fixed slot', (t) => {
  const directory = fixture(t), version = '0.9.14-beta.105'
  fs.writeFileSync(path.join(directory, 'latest.json'), '{"version":"old"}')
  const result = assembleUpdaterManifest({ directory, repo: 'XxMinor/mykvm',
    tag: `v${version}`, version, channel: 'beta' })
  for (const value of Object.values(result.platforms)) {
    assert.ok(value.url.includes('/download/beta/'))
    assert.ok(value.url.includes(version))
    const name = decodeURIComponent(value.url.split('/').at(-1))
    assert.equal(fs.readFileSync(path.join(directory, `${name}.sig`), 'utf8'), value.signature)
  }
  assert.equal(result.platforms['darwin-aarch64'].signature, 'signature:mykvm.app.tar.gz')
  assert.ok(result.platforms['windows-x86_64'].url.endsWith(`mykvm-windows-x64-${version}-setup.exe`))
  assert.equal(fs.existsSync(path.join(directory, 'latest.json')), false)
  const again = assembleUpdaterManifest({ directory, repo: 'XxMinor/mykvm',
    tag: `v${version}`, version, channel: 'beta' })
  assert.deepEqual(again.platforms, result.platforms)
})

test('an unsigned macOS package cannot produce a published manifest', (t) => {
  const directory = fixture(t)
  fs.unlinkSync(path.join(directory, 'mykvm.app.tar.gz.sig'))
  assert.throws(() => assembleUpdaterManifest({ directory, repo: 'XxMinor/mykvm',
    tag: 'v0.9.14', version: '0.9.14', channel: 'stable' }), /signed macOS/)
})

test('beta retention preserves current and two previous builds plus unversioned metadata', () => {
  const assets = ['0.9.14-beta.9-file', '0.9.14-beta.10-file', '0.9.14-beta.11-file',
    '0.9.14-beta.12-file', '0.9.14-beta.12-file.sig', 'latest.json', 'mykvm_0.9.13_x64.exe']
    .map((name, id) => ({ name, id }))
  assert.deepEqual(obsoleteBetaAssets(assets, '0.9.14-beta.12').map((asset) => asset.name), ['0.9.14-beta.9-file'])
})
