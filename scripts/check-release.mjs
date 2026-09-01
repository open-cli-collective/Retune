import assert from 'node:assert/strict'
import { readFileSync } from 'node:fs'
import { fileURLToPath } from 'node:url'
import { dirname, resolve } from 'node:path'

const root = resolve(dirname(fileURLToPath(import.meta.url)), '..')
const read = (path) => readFileSync(resolve(root, path), 'utf8')
const required = (text, value, message = value) => assert.ok(text.includes(value), `missing ${message}`)
const stepMarker = (name) => `      - name: ${name}\n`

const tauri = JSON.parse(read('apps/desktop/src-tauri/tauri.conf.json'))
const macTauri = JSON.parse(read('apps/desktop/src-tauri/tauri.macos.conf.json'))
const windowsTauri = JSON.parse(read('apps/desktop/src-tauri/tauri.windows.conf.json'))
const linuxTauri = JSON.parse(read('apps/desktop/src-tauri/tauri.linux.conf.json'))
const vite = read('apps/desktop/vite.config.ts')
const desktopCargo = read('apps/desktop/src-tauri/Cargo.toml')
const lock = read('Cargo.lock')
const autoWorkflow = read('.github/workflows/auto-release.yml')
const workflow = read('.github/workflows/release.yml')
const ci = read('.github/workflows/ci.yml')
const gitignore = read('.gitignore')
const cask = read('packaging/homebrew/retune.rb.template')
const buildInstall = read('scripts/build-install.sh')
const development = read('docs/DEVELOPMENT.md')
const installation = read('docs/INSTALL.md')
const tauriArchitecture = read('docs/tauri.md')
const normalized = (text) => text.replace(/\s+/g, ' ')
const nativeComposition = read('apps/desktop/src-tauri/src/lib.rs')
const frontendState = [
  read('apps/desktop/src/App.tsx'),
  read('apps/desktop/src/dialogViews.tsx'),
  read('apps/desktop/src/types.ts'),
].join('\n')

const namedStep = (name) => {
  const marker = stepMarker(name)
  assert.equal(workflow.split(marker).length - 1, 1, `expected exactly one workflow step named ${name}`)
  const start = workflow.indexOf(marker)
  const tail = workflow.slice(start + marker.length)
  const boundary = tail.search(/\n(?:      - name:|  [A-Za-z0-9_-]+:\n)/)
  return workflow.slice(start, boundary < 0 ? workflow.length : start + marker.length + boundary)
}
const requireStepOrder = (label, names) => {
  let previous = -1
  for (const name of names) {
    const current = workflow.indexOf(stepMarker(name))
    assert.ok(current >= 0, `${label}: missing step ${name}`)
    assert.ok(current > previous, `${label}: step ${name} is out of order`)
    previous = current
  }
}
const requireImmutableExternalActions = (label, contents) => {
  for (const match of contents.matchAll(/^\s*(?:-\s*)?uses:\s*([^\s#]+)/gm)) {
    const action = match[1]
    if (action.startsWith('./') || action.startsWith('../')) continue
    assert.match(action, /^[^@]+@[0-9a-f]{40}$/, `${label}: external action is not pinned to a full lowercase commit SHA: ${action}`)
  }
}

const credentialStep = namedStep('Require Last.fm credentials')
const releaseCandidateStep = namedStep('Require release candidate on main')
const releaseConfigStep = namedStep('Write release configuration')
const windowsReleaseConfigStep = namedStep('Write Windows release configuration')
const macBuildStep = namedStep('Build macOS bundle')
const linuxBuildStep = namedStep('Build Linux bundle')
const windowsTargetStep = namedStep('Install Windows Rust target')
const artifactSigningCliStep = namedStep('Install pinned Artifact Signing CLI')
const windowsBuildStep = namedStep('Build and sign Windows bundle')
const windowsPayloadVerifyStep = namedStep('Verify Windows signed payload')
const windowsInstallVerifyStep = namedStep('Verify installed Windows payload')
const macPackageStep = namedStep('Package macOS app')
const windowsRenameStep = namedStep('Verify and rename Windows NSIS installer')
const debRenameStep = namedStep('Rename Debian package')
const appleCredentialStep = namedStep('Require Apple distribution credentials')
const appleImportStep = namedStep('Import Developer ID certificate')
const appleIdentityStep = namedStep('Verify imported Developer ID identity')
const appleKeyStep = namedStep('Prepare Apple notarization key')
const appleVerifyStep = namedStep('Verify macOS app signature')
const appleCleanupStep = namedStep('Remove Apple notarization key')
const windowsCredentialStep = namedStep('Require Windows Artifact Signing credentials')
const autoDryRunStep = autoWorkflow.match(/- name: Report dry-run tag\n[\s\S]*?(?=\n      - name:)/)?.[0] ?? ''
const autoTagStep = autoWorkflow.match(/- name: Tag\n[\s\S]*$/)?.[0] ?? ''

const cargoVersion = desktopCargo.match(/name = "retune-desktop"\s+version = "([^"]+)"/s)?.[1]
const lockVersion = lock.match(/\[\[package\]\]\s+name = "retune-desktop"\s+version = "([^"]+)"/s)?.[1]
assert.match(tauri.version, /^\d+\.\d+\.\d+$/)
assert.match(tauri.version, /^\d+\.\d+\.0$/)
assert.equal(cargoVersion, tauri.version)
assert.equal(lockVersion, tauri.version)
assert.equal(tauri.identifier, 'com.rianjs.retune')
assert.equal(tauri.build.devUrl, 'http://127.0.0.1:5173')
assert.equal(tauri.bundle.linux.deb.section, 'sound')
assert.equal(tauri.bundle.targets, undefined)
assert.equal(tauri.bundle.macOS, undefined, 'base config must not force ad-hoc macOS signing')
assert.deepEqual(macTauri.bundle.targets, ['app'])
assert.equal(macTauri.bundle.macOS.minimumSystemVersion, '11.0')
assert.deepEqual(windowsTauri.bundle.targets, ['nsis'])
assert.equal(windowsTauri.bundle.windows.allowDowngrades, false)
assert.equal(windowsTauri.bundle.windows.minimumWebview2Version, '105.0.0.0')
assert.deepEqual(linuxTauri.bundle.targets, ['deb'])
required(vite, "host: '127.0.0.1'")
required(vite, 'port: 5173')
required(vite, 'strictPort: true')
required(vite, "'windows' ? 'chrome105' : 'safari13'")

required(workflow, 'workflow_dispatch:')
required(workflow, 'tags:\n      - "v*"')
requireImmutableExternalActions('release workflow', workflow)
requireImmutableExternalActions('automatic release workflow', autoWorkflow)
for (const [action, sha] of [
  ['actions/checkout', 'fbc6f3992d24b796d5a048ff273f7fcc4a7b6c09'],
  ['actions/setup-node', 'a0853c24544627f65ddf259abe73b1d18a591444'],
  ['dtolnay/rust-toolchain', '4360b52568e2003a75bf9bc1d59f33a8e3fc893c'],
  ['Swatinem/rust-cache', '6323deb102c322ba6fcbdcafc7e3dddab59af2b6'],
  ['actions/upload-artifact', 'ea165f8d65b6e75b540449e92b4886f43607fa02'],
  ['actions/download-artifact', 'd3f86a106a0bac45b974a628896c90dbdf5c8093'],
]) required(workflow, `${action}@${sha}`, `verified release action pin ${action}`)
required(autoWorkflow, 'actions/checkout@fbc6f3992d24b796d5a048ff273f7fcc4a7b6c09', 'verified automatic-release checkout pin')
required(autoWorkflow, 'actions/setup-node@a0853c24544627f65ddf259abe73b1d18a591444', 'verified automatic-release setup-node pin')
required(workflow, 'dtolnay/rust-toolchain@4360b52568e2003a75bf9bc1d59f33a8e3fc893c\n        with:\n          toolchain: stable', 'stable toolchain input with immutable action pin')
requireStepOrder('release authorization', [
  'Check release contract',
  'Require release candidate on main',
  'Resolve version',
])
requireStepOrder('macOS signing', [
  'Require Apple distribution credentials',
  'Import Developer ID certificate',
  'Verify imported Developer ID identity',
  'Prepare Apple notarization key',
  'Write release configuration',
  'Build macOS bundle',
  'Verify macOS app signature',
  'Package macOS app',
  'Remove Apple notarization key',
  'Upload macOS artifact',
])
requireStepOrder('Windows signing', [
  'Require Windows Artifact Signing credentials',
  'Install pinned Artifact Signing CLI',
  'Write Windows release configuration',
  'Build and sign Windows bundle',
  'Verify Windows signed payload',
  'Verify and rename Windows NSIS installer',
  'Upload Windows artifact',
])
required(releaseCandidateStep, 'git fetch --no-tags origin main', 'main ancestry fetch')
required(releaseCandidateStep, 'git merge-base --is-ancestor "$GITHUB_SHA" origin/main', 'manual and tag main-ancestry guard')
required(releaseCandidateStep, 'release candidates must point to a commit on main')
assert.doesNotMatch(releaseCandidateStep, /\n\s+if:/, 'main-ancestry guard must apply to every release event')
assert.ok(workflow.indexOf(stepMarker('Require release candidate on main')) < workflow.indexOf('\n  build:'), 'main-ancestry guard must precede the signing-secret build job')
required(workflow, 'build:\n    needs: prepare\n    environment: release', 'build job must depend on ancestry-checked prepare and protected release environment')
required(workflow, 'windows-install-smoke:\n    needs: [prepare, build]\n    environment: release', 'native Windows install smoke must use protected release environment')
required(workflow, 'aggregate:\n    needs: [prepare, build, windows-install-smoke]\n    environment: release', 'release writer must wait for install smoke and use protected release environment')
for (const job of ['publish-homebrew', 'dispatch-linux', 'publish-winget']) {
  const header = `${job}:\n    if: github.event_name == 'push' && startsWith(github.ref, 'refs/tags/v')`
  const start = workflow.indexOf(header)
  assert.ok(start >= 0, `missing ${job} tag-only job`)
  const tail = workflow.slice(start + header.length)
  const next = tail.search(/\n  [A-Za-z0-9_-]+:\n/)
  const block = workflow.slice(start, next < 0 ? workflow.length : start + header.length + next)
  required(block, 'environment: release', `${job} protected release environment`)
}
required(autoWorkflow, "tag:\n    if: github.event_name == 'push' && needs.auto-release.outputs.should-release == 'true'\n    needs: auto-release\n    environment: release", 'automatic tag job protected release environment')
assert.equal((workflow.match(/environment: release/g) ?? []).length, 6, 'release workflow privileged and secret-consuming jobs must use the release environment')
assert.equal((autoWorkflow.match(/environment: release/g) ?? []).length, 1, 'automatic tag job must use the release environment')
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
required(autoWorkflow, 'should-release: ${{ steps.gate.outputs.should-release }}', 'automatic release decision output')
required(autoWorkflow, 'tag: ${{ steps.gate.outputs.tag }}', 'automatic release tag output')
required(autoTagStep, 'TAG: ${{ needs.auto-release.outputs.tag }}', 'protected tag job output input')
assert.doesNotMatch(autoTagStep, /if:\s*github\.event_name/, 'tag condition belongs on the protected job')
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
for (const step of [macBuildStep, windowsBuildStep, linuxBuildStep]) {
  required(step, 'RETUNE_LASTFM_API_KEY: ${{ vars.LASTFM_API_KEY }}', 'native Last.fm API key variable mapping')
  required(step, 'RETUNE_LASTFM_SHARED_SECRET: ${{ secrets.LASTFM_API_SECRET }}', 'trusted Last.fm shared-secret mapping')
}
assert.equal((workflow.match(/vars\.LASTFM_API_KEY/g) ?? []).length, 4)
assert.equal((workflow.match(/secrets\.LASTFM_API_SECRET/g) ?? []).length, 4)
assert.doesNotMatch(ci, /LASTFM_API_KEY|LASTFM_API_SECRET|RETUNE_LASTFM/)
required(buildInstall, '.env.lastfm.local')
required(buildInstall, 'chmod 600')
required(buildInstall, 'unset RETUNE_LASTFM_API_KEY RETUNE_LASTFM_SHARED_SECRET')
required(buildInstall, 'codesign --force --deep --sign - "$bundle"', 'explicit local macOS bundle signing')
assert.ok(buildInstall.indexOf('codesign --force --deep --sign - "$bundle"') < buildInstall.indexOf('codesign --verify --deep --strict "$bundle"'), 'local macOS bundle must be signed before verification')
required(gitignore, '.env.lastfm.local')
required(nativeComposition, 'option_env!("RETUNE_LASTFM_API_KEY")', 'backend-only Last.fm API key compile option')
required(nativeComposition, 'option_env!("RETUNE_LASTFM_SHARED_SECRET")', 'backend-only Last.fm shared-secret compile option')
assert.doesNotMatch(frontendState, /RETUNE_LASTFM_API_KEY|RETUNE_LASTFM_SHARED_SECRET|LASTFM_API_SECRET|LASTFM_API_KEY/)
for (const [runner, arch, bundle] of [
  ['macos-15', 'arm64', 'app'],
  ['windows-2022', 'x64', 'nsis'],
  ['windows-2022', 'arm64', 'nsis'],
  ['ubuntu-22.04', 'amd64', 'deb'],
  ['ubuntu-22.04-arm', 'arm64', 'deb'],
]) {
  required(workflow, `os: ${runner}`)
  required(workflow, `arch: ${arch}`)
  required(workflow, `bundle: ${bundle}`)
}
for (const [name, arch, target] of [
  ['Windows x64', 'x64', 'x86_64-pc-windows-msvc'],
  ['Windows ARM64', 'arm64', 'aarch64-pc-windows-msvc'],
]) {
  required(workflow, `name: ${name}\n            os: windows-2022\n            arch: ${arch}\n            runner-arch: x64\n            target: ${target}`, `${name} cross-build matrix contract`)
}

for (const asset of [
  'Retune-${VERSION}-aarch64.zip',
  'Retune-${VERSION}-windows-x64-setup.exe',
  'Retune-${VERSION}-windows-arm64-setup.exe',
  'retune_${VERSION}_amd64.deb',
  'retune_${VERSION}_arm64.deb',
  'checksums.txt',
]) required(workflow, asset, `asset contract ${asset}`)
assert.doesNotMatch(workflow, /Retune-\$\{VERSION\}-aarch64\.tar\.gz|tar -czf/, 'macOS release must use Apple-compatible ZIP packaging')
for (const value of [
  '/usr/bin/ditto -c -k --keepParent "$app" "$archive"',
  '/usr/bin/ditto -x -k "$archive" "$extracted"',
  'codesign --verify --deep --strict "$packaged_app"',
  'xcrun stapler validate "$packaged_app"',
  'spctl --assess --type execute --verbose=4 "$packaged_app"',
]) required(macPackageStep, value, `macOS distribution archive proof ${value}`)
required(workflow, 'path: apps/desktop/Retune-${{ needs.prepare.outputs.version }}-aarch64.zip', 'macOS ZIP artifact upload path')
required(workflow, '"dist/assets/Retune-${VERSION}-aarch64.zip"', 'macOS ZIP GitHub release asset')
required(workflow, 'awk -v name="Retune-${VERSION}-aarch64.zip"', 'macOS ZIP Homebrew checksum lookup')
required(cask, 'Retune-#{version}-aarch64.zip', 'Homebrew macOS ZIP URL')
required(development, 'Retune-<version>-aarch64.zip', 'development release asset contract')
required(installation, 'Retune-<version>-aarch64.zip', 'installation release asset contract')
required(installation, '/usr/bin/ditto -x -k "Retune-${VERSION}-aarch64.zip" .', 'metadata-preserving macOS installation extraction')
required(tauriArchitecture, '`ditto`-created ZIP', 'Tauri macOS archive contract')
required(tauriArchitecture, '`ditto -c -k --keepParent`', 'Tauri macOS packaging command')
for (const [label, contents] of [
  ['release workflow', workflow],
  ['Homebrew template', cask],
  ['development docs', development],
  ['installation docs', installation],
  ['Tauri architecture', tauriArchitecture],
]) assert.doesNotMatch(contents, /aarch64\.tar\.gz|release tarball/, `${label}: stale macOS tar archive contract`)

const sharedCommit = '74d24fcd862d7b9cbe8f6fdda31db6a833e3d706'
required(ci, `open-cli-collective/.github/actions/pr-title@${sharedCommit}`)
required(ci, 'title: ${{ github.event.pull_request.title }}')
for (const action of ['homebrew-alias', 'winget-submit']) {
  required(workflow, `open-cli-collective/.github/actions/${action}@${sharedCommit}`)
}
required(appleImportStep, 'apple-actions/import-codesign-certs@fe74d46e82474f87e1ba79832ad28a4013d0e33a', 'pinned Apple certificate import')
assert.doesNotMatch(workflow, /macos-codesign-setup/, 'retired self-signed macOS setup remains')
for (const secret of [
  'MACOS_CERT_P12',
  'MACOS_CERT_PASSWORD',
  'MACOS_CERT_CN',
  'MACOS_CERT_LEAF_SHA',
  'MACOS_TEAM_ID',
  'APPLE_API_ISSUER',
  'APPLE_API_KEY',
  'APPLE_API_KEY_P8_BASE64',
  'AZURE_TENANT_ID',
  'AZURE_CLIENT_ID',
  'AZURE_CLIENT_SECRET',
  'AZURE_ARTIFACT_SIGNING_ENDPOINT',
  'AZURE_ARTIFACT_SIGNING_ACCOUNT',
  'AZURE_ARTIFACT_SIGNING_CERTIFICATE_PROFILE',
  'WINDOWS_SIGNING_SUBJECT',
  'TAP_GITHUB_TOKEN',
  'WINGET_GITHUB_TOKEN',
  'LINUX_PACKAGES_DISPATCH_TOKEN',
]) required(workflow, secret)
required(appleCredentialStep, 'MACOS_CERT_CN must name a Developer ID Application identity')
required(appleIdentityStep, 'security find-identity -v -p codesigning')
required(appleIdentityStep, 'imported Developer ID identity does not match MACOS_CERT_CN and MACOS_CERT_LEAF_SHA')
required(appleKeyStep, 'APPLE_API_KEY_PATH=$key_path')
assert.ok(
  appleKeyStep.indexOf('echo "APPLE_API_KEY_PATH=$key_path" >> "$GITHUB_ENV"') < appleKeyStep.indexOf('writeFileSync'),
  'Apple key path must be exported before the credential file is written so failure cleanup can locate it',
)
required(appleCleanupStep, "if: always() && matrix.os == 'macos-15'")
required(appleCleanupStep, 'rm -f "$APPLE_API_KEY_PATH"')
required(appleVerifyStep, 'codesign --verify --deep --strict "$app"')
required(appleVerifyStep, 'for target in "$executable" "$app"; do')
required(appleVerifyStep, 'codesign --verify --strict "$target"')
required(appleVerifyStep, 'Authority=Developer ID Application:')
required(appleVerifyStep, 'MACOS_TEAM_ID: ${{ secrets.MACOS_TEAM_ID }}')
required(appleVerifyStep, 'TeamIdentifier=$MACOS_TEAM_ID')
required(appleVerifyStep, "grep -E '^Timestamp=.+$' | grep -v '^Timestamp=none$'")
required(appleVerifyStep, "grep -E 'flags=.*runtime'")
required(appleVerifyStep, 'xcrun stapler validate "$app"')
required(appleVerifyStep, 'spctl --assess --type execute --verbose=4 "$app"')
required(macBuildStep, 'APPLE_SIGNING_IDENTITY: ${{ secrets.MACOS_CERT_CN }}')
required(macBuildStep, 'APPLE_API_ISSUER: ${{ secrets.APPLE_API_ISSUER }}')
required(macBuildStep, 'APPLE_API_KEY: ${{ secrets.APPLE_API_KEY }}')
required(windowsCredentialStep, 'WINDOWS_SIGNING_SUBJECT')
required(windowsCredentialStep, "Scheme -ne 'https'", 'Artifact Signing endpoint HTTPS validation')
required(windowsCredentialStep, ".Host.EndsWith('.codesigning.azure.net')", 'Artifact Signing endpoint host validation')
assert.doesNotMatch(workflow, /azure\/artifact-signing-action@/, 'out-of-lifecycle Artifact Signing action remains')
required(artifactSigningCliStep, 'cargo install artifact-signing-cli --version 0.11.0 --locked', 'pinned Artifact Signing CLI install')
required(windowsTargetStep, 'rustup target add ${{ matrix.target }}', 'explicit Windows Rust target installation')
required(windowsReleaseConfigStep, "cmd: 'artifact-signing-cli'", 'Tauri object-form Artifact Signing command')
required(windowsReleaseConfigStep, "'-d', 'Retune', '--fd', 'SHA256', '--tr', 'http://timestamp.acs.microsoft.com', '--td', 'SHA256', '%1'", 'explicit Tauri signing digest, timestamp, and path contract')
for (const option of ["'-e'", "'-a'", "'-c'"]) required(windowsReleaseConfigStep, option)
required(windowsBuildStep, 'npx tauri build --target $env:WINDOWS_TARGET', 'target-specific Windows cross-build')
required(windowsBuildStep, '--bundles nsis', 'Tauri must own Windows signing and bundling lifecycle')
for (const credential of ['AZURE_TENANT_ID', 'AZURE_CLIENT_ID', 'AZURE_CLIENT_SECRET']) required(windowsBuildStep, credential)
assert.doesNotMatch(windowsBuildStep, /AZURE_ARTIFACT_SIGNING_ENDPOINT|AZURE_ARTIFACT_SIGNING_ACCOUNT|AZURE_ARTIFACT_SIGNING_CERTIFICATE_PROFILE|WINDOWS_SIGNING_SUBJECT/, 'Windows build step must receive only Azure authentication credentials')
required(windowsPayloadVerifyStep, 'Get-AuthenticodeSignature')
required(windowsPayloadVerifyStep, "Status -ne 'Valid'")
required(windowsPayloadVerifyStep, 'TimeStamperCertificate')
required(windowsPayloadVerifyStep, 'signtool verify /pa /all /tw /v')
required(windowsPayloadVerifyStep, 'target/$env:WINDOWS_TARGET/release/retune-desktop.exe', 'target-specific signed payload path')
required(workflow, 'target/${{ matrix.target }}/release/bundle/nsis/Retune-${{ needs.prepare.outputs.version }}-windows-${{ matrix.arch }}-setup.exe', 'target-specific Windows upload path')
for (const value of [
  'EXPECTED_PE_MACHINE: ${{ matrix.pe-machine }}',
  'Get-AuthenticodeSignature',
  'TimeStamperCertificate',
  'signtool verify /pa /all /tw /v',
  "Start-Process -FilePath $installer -ArgumentList @('/S', \"/D=$installRoot\") -Wait -PassThru",
  '[BitConverter]::ToUInt16($bytes, $peOffset + 4)',
  'if ($uninstallers.Count -ne 1) { throw "expected one uninstaller, found $($uninstallers.Count)" }',
  "Start-Process -FilePath $uninstallerCopy -ArgumentList @('/S', \"_?=$installRoot\") -Wait -PassThru",
  'if ($uninstall.ExitCode -ne 0)',
  "Test-Path -LiteralPath $installedPayload",
  "Test-Path -LiteralPath $installRoot",
]) required(windowsInstallVerifyStep, value, `native Windows install verification ${value}`)
required(workflow, 'os: windows-11-arm', 'native ARM64 Windows install runner')
required(workflow, 'pe-machine: 34404', 'x64 PE machine contract')
required(workflow, 'pe-machine: 43620', 'ARM64 PE machine contract')
required(workflow, 'alias-tokens: ""')
required(workflow, 'fetch-depth: 0')
required(workflow, 'release_line="${baseline%.*}"', 'configured release line')
required(workflow, '[[ ! "$TAG" =~ ^v[0-9]+\\.[0-9]+\\.[0-9]+$ ]]', 'strict release tag')
required(workflow, 'tag_line="${version%.*}"', 'tag-derived release line')
required(releaseConfigStep, 'VERSION: ${{ needs.prepare.outputs.version }}', 'tag version environment')
required(releaseConfigStep, "require('node:fs').writeFileSync('src-tauri/tauri.release.conf.json'", 'Tauri version override config')
required(windowsReleaseConfigStep, 'VERSION: ${{ needs.prepare.outputs.version }}', 'Windows tag version environment')
required(macBuildStep, 'npx tauri build --config src-tauri/tauri.release.conf.json', 'macOS Tauri version override')
required(linuxBuildStep, 'npx tauri build --config src-tauri/tauri.release.conf.json', 'Linux Tauri version override')
required(windowsBuildStep, '--config src-tauri/tauri.release.conf.json', 'Windows Tauri version override')
required(macPackageStep, 'CFBundleShortVersionString', 'macOS package version assertion')
required(macPackageStep, '/usr/libexec/PlistBuddy', 'macOS package metadata assertion')
required(macPackageStep, '[ "$actual" = "$VERSION" ]', 'macOS package version match')
required(windowsRenameStep, 'BaseName -notmatch', 'Windows package version assertion')
required(windowsRenameStep, 'VersionInfo.ProductVersion', 'Windows installer ProductVersion metadata assertion')
required(windowsRenameStep, '$productVersion -ne $env:VERSION', 'Windows installer ProductVersion match')
required(windowsRenameStep, 'Get-AuthenticodeSignature', 'Windows installer Authenticode verification')
required(windowsRenameStep, "Status -ne 'Valid'", 'Windows installer trusted status')
required(windowsRenameStep, 'TimeStamperCertificate', 'Windows installer timestamp verification')
required(windowsRenameStep, 'signtool verify /pa /all /tw /v', 'Windows installer SignTool verification')
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
assert.doesNotMatch(cask, /xattr|postflight/, 'notarized cask must not bypass quarantine')
required(cask, 'Developer ID-signed')
required(cask, 'Apple-notarized')

for (const value of [
  'Developer ID Application',
  'hardened runtime',
  'secure timestamp',
  'Apple notarization',
  'Microsoft Artifact Signing',
  'RFC 3161',
  'WINDOWS_SIGNING_SUBJECT',
  'protected GitHub Actions environment named `release`',
  'only from `main` and `v*` tags',
  'require maintainer review',
]) required(normalized(development), value, `development release trust contract ${value}`)
for (const value of [
  'Developer ID-signed and notarized',
  'hardened runtime',
  'Apple notarization',
  'Authenticode-signed',
  'RFC 3161',
  'spctl --assess',
  'Get-AuthenticodeSignature',
]) required(normalized(installation), value, `installation trust contract ${value}`)
for (const value of [
  'Developer ID Application',
  'hardened runtime',
  'Microsoft Artifact Signing',
  'RFC 3161',
  'protected GitHub Actions environment named `release`',
]) required(normalized(tauriArchitecture), value, `Tauri distribution contract ${value}`)
assert.doesNotMatch(development, /feature branch for packaging validation/i, 'retired feature-branch signing guidance remains')
for (const text of [workflow, development, installation, tauriArchitecture, cask]) {
  assert.doesNotMatch(text, /ad-hoc signing|self-signed|not Apple-notarized|intentionally unsigned/i, 'retired distribution-trust deviation remains')
}

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
