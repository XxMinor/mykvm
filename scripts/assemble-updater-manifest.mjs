import fs from 'node:fs'
import path from 'node:path'
import { pathToFileURL } from 'node:url'

export function stageUpdaterAssets(artifacts, directory) {
  fs.mkdirSync(directory, { recursive: true })
  for (const artifact of artifacts) {
    if (!/(\.app\.tar\.gz|\.dmg|setup\.exe|\.msi|\.AppImage|\.deb|\.rpm)(\.sig)?$/i.test(artifact)) continue
    if (!fs.statSync(artifact).isFile()) continue
    fs.copyFileSync(artifact, path.join(directory, path.basename(artifact)))
    if (fs.existsSync(`${artifact}.sig`)) {
      fs.copyFileSync(`${artifact}.sig`, path.join(directory, `${path.basename(artifact)}.sig`))
    }
  }
  if (!fs.readdirSync(directory).length) throw new Error('No release packages were staged.')
}

export function assembleUpdaterManifest({ directory, repo, tag, version, channel, notes, date = new Date().toISOString() }) {
  const beta = channel === 'beta'
  let files = fs.readdirSync(directory).filter((name) => name !== 'latest.json')
  // The rolling release must keep immutable, versioned package URLs. A client
  // may still be downloading the preceding build when a new one is published.
  if (beta) {
    const staleManifest = path.join(directory, 'latest.json')
    if (fs.existsSync(staleManifest)) fs.unlinkSync(staleManifest)
    for (const name of files) {
      if (!name.includes(version)) fs.renameSync(path.join(directory, name), path.join(directory, `${version}-${name}`))
    }
    files = fs.readdirSync(directory).filter((name) => name !== 'latest.json')
  }
  const baseUrl = `https://github.com/${repo}/releases/download/${beta ? 'beta' : tag}`
  const platforms = {}
  const signatureFor = (name) => files.includes(`${name}.sig`)
    ? fs.readFileSync(path.join(directory, `${name}.sig`), 'utf8').trim() : null
  const addPlatform = (key, name) => {
    const signature = name && signatureFor(name)
    if (!signature) return false
    platforms[key] = { signature, url: `${baseUrl}/${encodeURIComponent(name)}` }
    return true
  }

  const mac = files.find((name) => /\.app\.tar\.gz$/i.test(name))
  if (!mac || !addPlatform('darwin-aarch64', mac) || !addPlatform('darwin-x86_64', mac)) {
    throw new Error('Missing signed macOS updater .app.tar.gz artifact.')
  }
  const windows = files.find((name) => !name.endsWith('.sig') &&
    !/mykvm-windows-x64(?:-.*)?-setup\.exe$/i.test(name) &&
    (/setup\.exe$/i.test(name) || /\.msi$/i.test(name)))
  if (windows) {
    const alias = beta ? `mykvm-windows-x64-${version}-setup.exe` : 'mykvm-windows-x64-setup.exe'
    const signature = signatureFor(windows)
    if (!signature) throw new Error(`Missing signature for ${windows}.`)
    fs.copyFileSync(path.join(directory, windows), path.join(directory, alias))
    fs.copyFileSync(path.join(directory, `${windows}.sig`), path.join(directory, `${alias}.sig`))
    files = fs.readdirSync(directory)
    addPlatform('windows-x86_64', alias)
  }
  addPlatform('linux-x86_64', files.find((name) => /\.AppImage$/i.test(name) && /(amd64|x86_64)/i.test(name)))
  addPlatform('linux-x86_64-deb', files.find((name) => /_amd64\.deb$/i.test(name)))
  addPlatform('linux-x86_64-rpm', files.find((name) => /\.x86_64\.rpm$/i.test(name)))
  return { version, notes: notes || `Release v${version}`, pub_date: date, platforms }
}

export function obsoleteBetaAssets(assets, currentVersion, keep = 3) {
  const versionOf = (name) => name.match(/\d+\.\d+\.\d+-beta\.\d+(?:\.\d+)?/)?.[0]
  const compare = (a, b) => {
    const left = a.split(/\D+/).map(Number), right = b.split(/\D+/).map(Number)
    for (let i = 0; i < Math.max(left.length, right.length); i++) {
      const difference = (right[i] ?? 0) - (left[i] ?? 0)
      if (difference) return difference
    }
    return 0
  }
  const versions = [...new Set(assets.map((asset) => versionOf(asset.name)).filter(Boolean))]
    .filter((version) => version !== currentVersion).sort(compare)
  const retained = new Set([currentVersion, ...versions.slice(0, keep - 1)])
  return assets.filter((asset) => {
    const version = versionOf(asset.name)
    return version && !retained.has(version)
  })
}

if (process.argv[1] && import.meta.url === pathToFileURL(process.argv[1]).href) {
  const manifest = assembleUpdaterManifest({ directory: 'dl', repo: process.env.REPO,
    tag: process.env.TAG, version: process.env.VERSION, channel: process.env.CHANNEL,
    notes: process.env.RELEASE_NOTES })
  fs.writeFileSync('latest.json', `${JSON.stringify(manifest, null, 2)}\n`)
  console.log(JSON.stringify(manifest, null, 2))
}
