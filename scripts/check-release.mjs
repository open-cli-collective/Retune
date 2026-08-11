import assert from 'node:assert/strict'
import { readFileSync } from 'node:fs'
import { fileURLToPath } from 'node:url'
import { dirname, resolve } from 'node:path'

const root = resolve(dirname(fileURLToPath(import.meta.url)), '..')
const read = (path) => readFileSync(resolve(root, path), 'utf8')
const required = (text, value, message = value) => assert.ok(text.includes(value), `missing ${message}`)

const tauri = JSON.parse(read('apps/desktop/src-tauri/tauri.conf.json'))
const desktopCargo = read('apps/desktop/src-tauri/Cargo.toml')
const lock = read('Cargo.lock')
const workflow = read('.github/workflows/release.yml')
const cask = read('packaging/homebrew/retune.rb.template')

const cargoVersion = desktopCargo.match(/name = "retune-desktop"\s+version = "([^"]+)"/s)?.[1]
const lockVersion = lock.match(/\[\[package\]\]\s+name = "retune-desktop"\s+version = "([^"]+)"/s)?.[1]
assert.match(tauri.version, /^\d+\.\d+\.\d+$/)
assert.equal(cargoVersion, tauri.version)
assert.equal(lockVersion, tauri.version)
assert.equal(tauri.identifier, 'com.rianjs.retune')

required(workflow, 'workflow_dispatch:')
required(workflow, 'tags:\n      - "v*"')
for (const [runner, arch, bundle] of [
  ['macos-15', 'arm64', 'app'],
  ['windows-2025', 'x64', 'nsis'],
  ['windows-11-arm', 'arm64', 'nsis'],
  ['ubuntu-22.04', 'amd64', 'deb'],
  ['ubuntu-22.04-arm', 'arm64', 'deb'],
]) {
  required(workflow, `os: ${runner}`)
  required(workflow, `arch: ${arch}`)
  required(workflow, `bundle: ${bundle}`)
}

for (const asset of [
  'Retune-${VERSION}-aarch64.tar.gz',
  'Retune-${VERSION}-windows-x64-setup.exe',
  'Retune-${VERSION}-windows-arm64-setup.exe',
  'retune_${VERSION}_amd64.deb',
  'retune_${VERSION}_arm64.deb',
  'checksums.txt',
]) required(workflow, asset, `asset contract ${asset}`)

const sharedCommit = '74d24fcd862d7b9cbe8f6fdda31db6a833e3d706'
for (const action of ['macos-codesign-setup', 'homebrew-alias', 'winget-submit']) {
  required(workflow, `open-cli-collective/.github/actions/${action}@${sharedCommit}`)
}
for (const secret of [
  'MACOS_CERT_P12',
  'MACOS_CERT_PASSWORD',
  'MACOS_CERT_CN',
  'MACOS_CERT_LEAF_SHA',
  'TAP_GITHUB_TOKEN',
  'WINGET_GITHUB_TOKEN',
  'LINUX_PACKAGES_DISPATCH_TOKEN',
]) required(workflow, secret)
required(workflow, 'stable macOS signing credentials are required')
required(workflow, '42e1afd02aae8666c09c15f171e1639550f301c2')
required(workflow, 'identifier "com.rianjs.retune"')
required(workflow, 'certificate leaf')
required(workflow, 'codesign --verify --deep --strict')
assert.doesNotMatch(workflow, /--timestamp(?:=|\s)/)
assert.doesNotMatch(workflow, /--options\s+runtime/)
assert.doesNotMatch(workflow, /notarytool/i)
assert.doesNotMatch(workflow, /uses:.*notariz/i)
required(workflow, 'alias-tokens: ""')
required(workflow, 'bootstrap: true')
required(workflow, 'x64-marker: Retune-${{ needs.prepare.outputs.version }}-windows-x64-setup.exe')
required(workflow, 'arm64-marker: Retune-${{ needs.prepare.outputs.version }}-windows-arm64-setup.exe')
required(workflow, 'client_payload:{package:$package,version:$version,repo:$repo}')
required(workflow, 'curl --fail-with-body')
assert.doesNotMatch(workflow, /gh api --method POST .*dispatches/)
required(workflow, '::warning::Linux package dispatch failed')
for (const job of ['publish-homebrew', 'dispatch-linux', 'publish-winget'])
  required(workflow, `${job}:\n    if: github.event_name == 'push' && startsWith(github.ref, 'refs/tags/v')`, `${job} tag-only guard`)
const releaseStep = workflow.slice(workflow.indexOf('- name: Create GitHub release'))
required(releaseStep, "if: github.event_name == 'push' && startsWith(github.ref, 'refs/tags/v')", 'release tag-only guard')
required(releaseStep, 'gh release view')
required(releaseStep, 'gh release upload "$GITHUB_REF_NAME" "${assets[@]}" --clobber', 'rerun-safe release upload')

required(cask, 'cask "retune"')
required(cask, 'version "__VERSION__"')
required(cask, 'sha256 "__SHA256__"')
required(cask, 'https://github.com/open-cli-collective/Retune/releases/download')
required(cask, 'homepage "https://github.com/open-cli-collective/Retune"')
required(cask, 'args: ["-cr"')
required(cask, 'stable-signed')
required(cask, 'Always Allow')

const winget = [
  read('packaging/winget/OpenCLICollective.Retune.yaml'),
  read('packaging/winget/OpenCLICollective.Retune.locale.en-US.yaml'),
  read('packaging/winget/OpenCLICollective.Retune.installer.yaml'),
].join('\n')
required(winget, 'PackageIdentifier: OpenCLICollective.Retune')
required(winget, 'ManifestType: version')
required(winget, 'ManifestType: defaultLocale')
required(winget, 'ManifestType: installer')
assert.equal((winget.match(/InstallerType: nullsoft/g) ?? []).length, 1)
required(winget, 'Architecture: x64')
required(winget, 'Architecture: arm64')
required(winget, 'Retune-0.0.0-windows-x64-setup.exe')
required(winget, 'Retune-0.0.0-windows-arm64-setup.exe')

console.log(`release contract OK (${tauri.version})`)
