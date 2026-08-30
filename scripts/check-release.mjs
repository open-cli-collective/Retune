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
const autoWorkflow = read('.github/workflows/auto-release.yml')
const workflow = read('.github/workflows/release.yml')
const ci = read('.github/workflows/ci.yml')
const gitignore = read('.gitignore')
const cask = read('packaging/homebrew/retune.rb.template')
const buildInstall = read('scripts/build-install.sh')
const lastfmBackend = read('apps/desktop/src-tauri/src/lastfm.rs')
const frontendState = [
  read('apps/desktop/src/App.tsx'),
  read('apps/desktop/src/dialogViews.tsx'),
  read('apps/desktop/src/types.ts'),
].join('\n')

const nativeBundleStep = workflow.match(/- name: Build native bundle\n[\s\S]*?(?=\n      - name:)/)?.[0] ?? ''
const credentialStep = workflow.match(/- name: Require Last\.fm credentials\n[\s\S]*?(?=\n      - name:)/)?.[0] ?? ''
const macPackageStep = workflow.match(/- name: Package macOS app\n[\s\S]*?(?=\n      - name:)/)?.[0] ?? ''
const windowsRenameStep = workflow.match(/- name: Rename Windows NSIS installer\n[\s\S]*?(?=\n      - name:)/)?.[0] ?? ''
const debRenameStep = workflow.match(/- name: Rename Debian package\n[\s\S]*?(?=\n      - name:)/)?.[0] ?? ''
const autoDryRunStep = autoWorkflow.match(/- name: Report dry-run tag\n[\s\S]*?(?=\n      - name:)/)?.[0] ?? ''
const autoTagStep = autoWorkflow.match(/- name: Tag\n[\s\S]*$/)?.[0] ?? ''

const cargoVersion = desktopCargo.match(/name = "retune-desktop"\s+version = "([^"]+)"/s)?.[1]
const lockVersion = lock.match(/\[\[package\]\]\s+name = "retune-desktop"\s+version = "([^"]+)"/s)?.[1]
assert.match(tauri.version, /^\d+\.\d+\.\d+$/)
assert.match(tauri.version, /^\d+\.\d+\.0$/)
assert.equal(cargoVersion, tauri.version)
assert.equal(lockVersion, tauri.version)
assert.equal(tauri.identifier, 'com.rianjs.retune')
assert.equal(tauri.bundle.linux.deb.section, 'sound')

required(workflow, 'workflow_dispatch:')
required(workflow, 'tags:\n      - "v*"')
required(autoWorkflow, 'push:\n    branches: ["main"]', 'automatic release main trigger')
required(autoWorkflow, 'workflow_dispatch:', 'automatic release manual trigger')
required(autoWorkflow, 'fetch-depth: 0')
required(autoWorkflow, 'fetch-tags: true')
required(autoWorkflow, 'persist-credentials: false')
required(autoWorkflow, 'open-cli-collective/.github/actions/auto-release@74d24fcd862d7b9cbe8f6fdda31db6a833e3d706')
required(autoWorkflow, 'release-paths: apps/**,crates/**,Cargo.toml,Cargo.lock,packaging/**,scripts/**,.github/workflows/release.yml,.github/workflows/auto-release.yml')
required(autoWorkflow, 'version-file: apps/desktop/src-tauri/tauri.conf.json')
const contractIndex = autoWorkflow.indexOf('- name: Check release contract')
const gateIndex = autoWorkflow.indexOf('- id: gate')
assert.ok(contractIndex >= 0 && contractIndex < gateIndex, 'release contract must run before automatic release gate')
required(autoWorkflow, 'run: node scripts/check-release.mjs', 'automatic release contract check')
required(autoDryRunStep, "if: github.event_name == 'workflow_dispatch' && steps.gate.outputs.should-release == 'true'", 'automatic release dry-run condition')
assert.doesNotMatch(autoDryRunStep, /TAP_GITHUB_TOKEN|TAG_TOKEN|push origin/, 'automatic release dry-run token/push isolation')
required(autoTagStep, "if: github.event_name == 'push' && steps.gate.outputs.should-release == 'true'", 'automatic release tag condition')
required(autoTagStep, 'TAP_GITHUB_TOKEN')
required(autoTagStep, 'Same-tag/same-SHA')
required(autoTagStep, 'collision')
required(autoTagStep, 'push origin "refs/tags/$TAG"', 'automatic release tag push')
assert.doesNotMatch(autoTagStep, /DRY_RUN|workflow_dispatch/, 'automatic release tag dry-run branch')
assert.doesNotMatch(autoWorkflow, /dry_run/)
assert.doesNotMatch(autoWorkflow, /version\.txt|identity\.yml|goreleaser/i)
required(credentialStep, 'RETUNE_LASTFM_API_KEY: ${{ vars.LASTFM_API_KEY }}', 'trusted Last.fm API key variable mapping')
required(credentialStep, 'RETUNE_LASTFM_SHARED_SECRET: ${{ secrets.LASTFM_API_SECRET }}', 'trusted Last.fm shared-secret mapping')
required(credentialStep, "node --input-type=module -e \"for (const name of ['RETUNE_LASTFM_API_KEY', 'RETUNE_LASTFM_SHARED_SECRET'])", 'release Last.fm credential presence check')
assert.doesNotMatch(credentialStep, /tauri build|writeFileSync/)
required(nativeBundleStep, 'RETUNE_LASTFM_API_KEY: ${{ vars.LASTFM_API_KEY }}', 'native Last.fm API key variable mapping')
required(nativeBundleStep, 'RETUNE_LASTFM_SHARED_SECRET: ${{ secrets.LASTFM_API_SECRET }}', 'trusted Last.fm shared-secret mapping')
assert.equal((workflow.match(/vars\.LASTFM_API_KEY/g) ?? []).length, 2)
assert.equal((workflow.match(/secrets\.LASTFM_API_SECRET/g) ?? []).length, 2)
assert.doesNotMatch(ci, /LASTFM_API_KEY|LASTFM_API_SECRET|RETUNE_LASTFM/)
required(buildInstall, '.env.lastfm.local')
required(buildInstall, 'chmod 600')
required(buildInstall, 'unset RETUNE_LASTFM_API_KEY RETUNE_LASTFM_SHARED_SECRET')
required(gitignore, '.env.lastfm.local')
required(lastfmBackend, 'option_env!("RETUNE_LASTFM_API_KEY")', 'backend-only Last.fm API key compile option')
required(lastfmBackend, 'option_env!("RETUNE_LASTFM_SHARED_SECRET")', 'backend-only Last.fm shared-secret compile option')
assert.doesNotMatch(frontendState, /RETUNE_LASTFM_API_KEY|RETUNE_LASTFM_SHARED_SECRET|LASTFM_API_SECRET|LASTFM_API_KEY/)
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
required(ci, `open-cli-collective/.github/actions/pr-title@${sharedCommit}`)
required(ci, 'title: ${{ github.event.pull_request.title }}')
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
const verifyStepName = '- name: Verify macOS app signature'
const verifyStep = workflow.match(/- name: Verify macOS app signature\n[\s\S]*?(?=\n      - name:)/)?.[0] ?? ''
required(verifyStep, verifyStepName)
required(verifyStep, 'codesign --verify --deep --strict "$app"')
required(verifyStep, 'for target in "$executable" "$app"; do')
required(verifyStep, 'codesign --verify --strict "$target"')
required(verifyStep, 'requirement="$(codesign -d -r- "$target" 2>&1 | sed -n \'s/^designated => //p\')"')
required(verifyStep, '*\'identifier "com.rianjs.retune"\'*\'certificate leaf = H"42e1afd02aae8666c09c15f171e1639550f301c2"\'*) ;;')
required(verifyStep, '*cdhash*) echo "::error::macOS designated requirement contains cdhash for $target"; exit 1 ;;')
assert.doesNotMatch(workflow, /security\s+find-identity/)
assert.doesNotMatch(workflow, /--timestamp(?:=|\s)/)
assert.doesNotMatch(workflow, /--options\s+runtime/)
assert.doesNotMatch(workflow, /notarytool/i)
assert.doesNotMatch(workflow, /uses:.*notariz/i)
required(workflow, 'alias-tokens: ""')
required(workflow, 'fetch-depth: 0')
required(workflow, 'git merge-base --is-ancestor "$GITHUB_SHA" origin/main', 'release main-branch guard')
required(workflow, 'release_line="${baseline%.*}"', 'configured release line')
required(workflow, '[[ ! "$TAG" =~ ^v[0-9]+\\.[0-9]+\\.[0-9]+$ ]]', 'strict release tag')
required(workflow, 'tag_line="${version%.*}"', 'tag-derived release line')
required(nativeBundleStep, 'VERSION: ${{ needs.prepare.outputs.version }}', 'tag version environment')
required(nativeBundleStep, "require('node:fs').writeFileSync('src-tauri/tauri.release.conf.json'", 'Tauri version override config')
required(nativeBundleStep, 'npx tauri build --config src-tauri/tauri.release.conf.json', 'Tauri version override')
assert.doesNotMatch(nativeBundleStep, /shell: bash/)
required(macPackageStep, 'CFBundleShortVersionString', 'macOS package version assertion')
required(macPackageStep, '/usr/libexec/PlistBuddy', 'macOS package metadata assertion')
required(macPackageStep, '[ "$actual" = "$VERSION" ]', 'macOS package version match')
required(windowsRenameStep, 'BaseName -notmatch', 'Windows package version assertion')
required(windowsRenameStep, 'VersionInfo.ProductVersion', 'Windows installer ProductVersion metadata assertion')
required(windowsRenameStep, '$productVersion -ne $env:VERSION', 'Windows installer ProductVersion match')
required(debRenameStep, 'dpkg-deb -f', 'Debian package version assertion')
required(debRenameStep, '[ "$package_version" = "$VERSION" ]', 'Debian package version match')
required(ci, 'push:\n    branches: ["main"]', 'CI push main-only guard')
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
