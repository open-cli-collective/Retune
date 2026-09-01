# Install and set up Retune

Retune supports these native packages:

| Platform | Supported architecture | Minimum runtime |
| --- | --- | --- |
| macOS | Apple Silicon | macOS 11 |
| Windows 10/11 | x64, ARM64 | WebView2 105 (the installer updates older runtimes) |
| Ubuntu 22.04 | amd64, arm64 | Distribution WebKitGTK 4.1 |
| Compatible Debian/Ubuntu systems | amd64, arm64 | WebKitGTK 4.1 |

Homebrew, Winget, and APT are recommended because they manage updates and verify
published package metadata.

## Direct downloads

The [latest Retune release](https://github.com/open-cli-collective/Retune/releases/latest)
provides these fallback downloads. Use the version shown on that release page
in each artifact name:

| Platform | Architecture | Asset |
| --- | --- | --- |
| macOS | Apple Silicon | `Retune-<version>-aarch64.zip` |
| Windows | x64 | `Retune-<version>-windows-x64-setup.exe` |
| Windows | ARM64 | `Retune-<version>-windows-arm64-setup.exe` |
| Debian/Ubuntu | amd64 | `retune_<version>_amd64.deb` |
| Debian/Ubuntu | arm64 | `retune_<version>_arm64.deb` |

Download `checksums.txt` from that same latest release and verify the matching
asset before installing. macOS artifacts are Developer ID-signed and notarized;
Windows installers and their installed application payloads are
Authenticode-signed. Checksums remain an independent transport-integrity check.
These files support only the targets listed above.

After verifying the download, install the matching package:

```sh
# macOS (replace VERSION with the latest release version)
VERSION=latest-version
/usr/bin/ditto -x -k "Retune-${VERSION}-aarch64.zip" .
spctl --assess --type execute --verbose=4 Retune.app
sudo mv Retune.app /Applications/

# Debian/Ubuntu amd64 (use the arm64 filename on ARM64)
sudo apt install "./retune_${VERSION}_amd64.deb"
```

On Windows, run the downloaded `.exe` installer from File Explorer or
PowerShell, for example `./Retune-<version>-windows-x64-setup.exe`.

## macOS with Homebrew

Install:

```sh
brew install --cask open-cli-collective/tap/retune
```

Upgrade or uninstall:

```sh
brew update
brew upgrade --cask retune
brew uninstall --cask retune
```

The app is signed with the Open CLI Collective's Apple Developer ID identity,
uses hardened runtime and a secure timestamp, and has an Apple notarization
ticket stapled before publication. Gatekeeper therefore validates the normal
download without a quarantine bypass.

## Windows with Winget

Install:

```powershell
winget install --exact --id OpenCLICollective.Retune
```

Upgrade or uninstall:

```powershell
winget upgrade --exact --id OpenCLICollective.Retune
winget uninstall --exact --id OpenCLICollective.Retune
```

The installer and installed Retune executable are Authenticode-signed through
Microsoft Artifact Signing with SHA-256 RFC 3161 timestamps. Windows should
show the verified publisher rather than **Unknown Publisher**. Winget also
verifies the published installer hash. New Winget submissions can take time to
appear while Microsoft publishes the manifest; if Winget reports that no
package was found, use the signed direct download or retry later.

## Debian or Ubuntu with APT

Add the signed Open CLI Collective repository, then install Retune:

```sh
curl -fsSL https://open-cli-collective.github.io/linux-packages/keys/gpg.asc \
  | sudo gpg --dearmor --yes -o /usr/share/keyrings/open-cli-collective.gpg
echo "deb [arch=$(dpkg --print-architecture) signed-by=/usr/share/keyrings/open-cli-collective.gpg] https://open-cli-collective.github.io/linux-packages/apt stable main" \
  | sudo tee /etc/apt/sources.list.d/open-cli-collective.list
sudo apt update
sudo apt install retune
```

Upgrade or uninstall:

```sh
sudo apt update
sudo apt install --only-upgrade retune
sudo apt remove retune
```

Release builds encrypt Spotify tokens. The encryption key is stored in the
desktop session's Linux Secret Service; local files still work if Spotify is
signed out or that credential store is unavailable.

## Connect Spotify

Spotify access is optional. You can import and play local files while signed
out. Spotify library access and playback require Spotify Premium and a Spotify
developer application:

1. Open the [Spotify Developer Dashboard](https://developer.spotify.com/dashboard),
   create an app following Spotify's [app guidance](https://developer.spotify.com/documentation/web-api/concepts/apps),
   and select **Web API**.
2. In the app settings, follow Spotify's [redirect URI guidance](https://developer.spotify.com/documentation/web-api/concepts/redirect_uri)
   and register exactly
   `http://127.0.0.1:8898/callback`. Use the literal `127.0.0.1`, with the exact
   path, no trailing slash, and no `localhost` substitution. Do **not** register
   `http://127.0.0.1:8898/login`; `/login` is Retune's separate internal
   built-in-playback callback.
3. Copy the app's Client ID. Retune uses Authorization Code with PKCE, so it
   does not need or store the client secret.
4. In Retune's setup window, enter the Client ID, leave **Web API enabled**
   selected, choose **Connect…**, approve access in the browser, then choose
   **Sync**.
5. The first time built-in playback starts, approve the separate one-time
   **Authorize Spotify playback** prompt. This does not replace or disconnect
   the Web API grant.

Spotify Development Mode currently requires the application owner to have
Premium and limits each app to five authenticated users. Anyone other than the
owner must be added to the app allowlist in the Dashboard. Spotify may also
apply Development Mode quota limits; see its current [quota-mode
documentation](https://developer.spotify.com/documentation/web-api/concepts/quota-modes).

## Troubleshooting

- **Invalid redirect URI or the browser never returns:** compare the Dashboard
  entry character-for-character with `http://127.0.0.1:8898/callback`. Remove a
  trailing slash, `localhost`, or any `/login` entry, save, and reconnect.
- **User not registered, forbidden, or quota errors:** confirm the app owner has
  Premium, add the signing-in Spotify account to the app allowlist, and keep the
  total at five users or fewer. Quota exhaustion must clear at Spotify; repeated
  reconnects do not bypass it.
- **macOS trust failure:** verify the checksum, then run
  `spctl --assess --type execute --verbose=4 Retune.app`. A current published
  artifact must pass Developer ID and notarization assessment without removing
  quarantine metadata.
- **Windows trust failure:** run
  `Get-AuthenticodeSignature .\Retune-<version>-windows-<arch>-setup.exe` and
  require `Status` to be `Valid`; do not install an unsigned or mismatched
  artifact.
- **Credential-store unavailable:** unlock macOS Keychain, Windows Credential
  Manager, or Linux Secret Service and relaunch. Local-only use remains
  available without stored Spotify credentials.
- **Verify a direct download:** download `checksums.txt` beside the package and
  run `sha256sum -c checksums.txt --ignore-missing` on Linux, `shasum -a 256`
  on macOS, or `Get-FileHash -Algorithm SHA256` in PowerShell and compare the
  result.
