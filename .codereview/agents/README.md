# Retune code-review agents

These reviewers are repo-local enforcement for Retune's architecture and
implementation boundaries. Keep this catalog on main: review agents are
trusted from the base branch, so a pull request cannot weaken the reviewers
used to inspect itself. The bootstrap pull request cannot use trusted
base-branch agents; validate this catalog locally before merging it.

| Agent | Scope |
|---|---|
| architecture/seams | Retune ownership, dependency direction, provider isolation, persistence, playback authority, and YAGNI |
| rust/implementation-tests | Rust correctness, concurrency, resources, persistence, playback generations, and behavioral/hermetic tests |
| tauri/config-ipc | Tauri configuration, capabilities, commands/events, WebView inputs, DTOs, and release configuration |
| security/credential-boundary | Spotify and Last.fm authentication, token stores, redaction, separation, and cleanup |
| frontend/view-state | React state/effects, stale async results, navigation/playback independence, accessibility, and UI regression tests |

The catalog is adapted from the YakShed source at commit
edcbf1392152bd2c1e66dfa8dbe05ff5942217f6. Prompts are Retune-specific and
should return no findings when their own invariants are satisfied.

