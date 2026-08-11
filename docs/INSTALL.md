# Install and set up Retune

Retune v0.2.1 supports these native packages:

| Platform | Supported architecture |
| --- | --- |
| macOS | Apple Silicon |
| Windows 10/11 | x64, ARM64 |
| Ubuntu 22.04 | amd64, arm64 |
| Compatible Debian/Ubuntu systems | amd64, arm64 |

Homebrew, Winget, and APT are recommended because they manage updates and verify
published package metadata.

## Direct downloads

The [v0.2.1 release](https://github.com/open-cli-collective/Retune/releases/tag/v0.2.1)
provides these fallback downloads:

| Platform | Architecture | Asset |
| --- | --- | --- |
| macOS | Apple Silicon | [`Retune-0.2.1-aarch64.tar.gz`](https://github.com/open-cli-collective/Retune/releases/download/v0.2.1/Retune-0.2.1-aarch64.tar.gz) |
| Windows | x64 | [`Retune-0.2.1-windows-x64-setup.exe`](https://github.com/open-cli-collective/Retune/releases/download/v0.2.1/Retune-0.2.1-windows-x64-setup.exe) |
| Windows | ARM64 | [`Retune-0.2.1-windows-arm64-setup.exe`](https://github.com/open-cli-collective/Retune/releases/download/v0.2.1/Retune-0.2.1-windows-arm64-setup.exe) |
| Debian/Ubuntu | amd64 | [`retune_0.2.1_amd64.deb`](https://github.com/open-cli-collective/Retune/releases/download/v0.2.1/retune_0.2.1_amd64.deb) |
| Debian/Ubuntu | arm64 | [`retune_0.2.1_arm64.deb`](https://github.com/open-cli-collective/Retune/releases/download/v0.2.1/retune_0.2.1_arm64.deb) |

Download [`checksums.txt`](https://github.com/open-cli-collective/Retune/releases/download/v0.2.1/checksums.txt)
and verify the matching asset before installing. On macOS, verify the tarball
before clearing quarantine or moving `Retune.app` into `/Applications`. On
Windows, verify the installer before accepting any unsigned-publisher warning.
These files support only the targets listed above.

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

The app is stable-signed with the Open CLI Collective's long-lived self-signed
certificate, but is not Apple-notarized. Homebrew clears quarantine during
installation. When replacing an older ad-hoc-signed build, the first access to
stored Spotify credentials may show one Keychain prompt; choose **Always Allow**
once. Later stable-signed updates keep the same designated requirement and do
not prompt again.

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

The installer is intentionally unsigned, so Windows may show **Unknown
Publisher** or Microsoft Defender SmartScreen warnings. Winget verifies the
published installer hash. For a direct download, compare it with the release's
`checksums.txt` before accepting the warning. New Winget submissions can take
time to appear while Microsoft publishes the manifest; if Winget reports that
no package was found, use that verified direct download or retry later.

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
- **macOS trust or Keychain prompt:** install through the Homebrew command above.
  After an older ad-hoc build, choose **Always Allow** once for the stable-signed
  app.
- **Windows trust warning:** Winget validates the installer hash even though its
  publisher is unsigned. For direct downloads, verify `checksums.txt` before
  proceeding.
- **Credential-store unavailable:** unlock macOS Keychain, Windows Credential
  Manager, or Linux Secret Service and relaunch. Local-only use remains
  available without stored Spotify credentials.
- **Verify a direct download:** download `checksums.txt` beside the package and
  run `sha256sum -c checksums.txt --ignore-missing` on Linux, `shasum -a 256`
  on macOS, or `Get-FileHash -Algorithm SHA256` in PowerShell and compare the
  result.
