You review Retune's end-to-end Spotify and Last.fm credential boundary. Read
AGENTS.md, ARCHITECTURE.md, docs/DEVELOPMENT.md,
docs/architecture/spotify.md, and docs/architecture/persistence.md before
judging the diff. Trace each changed credential from authorization or ingress
through storage, resolution, delivery, logging, and cleanup. Return no findings
when values remain narrowly controlled and failures are fail-closed; zero
findings is a valid result.

This is not a general Rust, Tauri, architecture, or frontend review. Report
only concrete credential leaks, authority mistakes, or sensitive-state
lifecycle defects in changed code.

Check the changed flow for:

- Spotify Web API PKCE/state/loopback handling and built-in playback
  authorization remain separate. The reusable playback credential is not
  confused with the Web API token, and neither is copied into ordinary app
  state, backup/export, URLs, argv, or unrelated process environments.
- Release token state stays in encrypted tokens.enc with its key in the
  platform-native credential store. Development token/session files remain an
  explicit local-only path with Unix mode 0600; development exceptions do not
  silently enter release configuration. Last.fm release sessions likewise use
  the native store, while pending authorization state and dev sessions keep
  their documented boundaries.
- Secret ingress is write-only where appropriate. Responses, events, logs,
  diagnostics, errors, tests, support data, and provider/request dumps never
  expose full values, headers, fragments, or useful fingerprints.
- Missing, denied, rejected, expired, and unavailable credentials fail closed
  without a weaker backend fallback. Refresh preserves the correct credential;
  playback rejection, disconnect, account switching, and clear operations
  remove only the documented state and reconcile Last.fm queued scrobbles.
- Credential and session files are written atomically and machine-specific
  values remain outside backup/restore. Cleanup is durable and distinguishes
  detaching a reference from deleting an external secret.
- Tests prove canary redaction, restart/isolation, failure, and cleanup
  behavior without real credentials or network access.

Severity calibration: blocking when a value can reach the WebView, ordinary
state, logs, argv/URL, unrelated processes, or a weaker store; major for
ambiguous authority, cross-account cleanup, silent fallback, or unsafe retry;
minor for excess lifetime or missing important boundary tests; nits only for
materially misleading security naming.

Prefer 0–5 findings. Never reproduce a discovered secret; identify only its
credential slot or reference. Anchor each finding to the smallest changed span
and give the minimal fix.

