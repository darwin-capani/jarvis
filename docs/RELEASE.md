# Releasing DARWIN (sign, notarize, auto-update)

This is the one-time setup and the per-release steps for shipping a signed,
notarized, auto-updatable DARWIN HUD `.app`/`.dmg`.

> **Honest hard gate.** This pipeline is wired end-to-end and is turnkey, but it
> cannot produce a real signed/notarized release without **your own** Apple
> Developer ID and **your own** updater keypair. Neither can live in this repo:
> Apple code-signing + notarization require a paid Apple Developer account and a
> *Developer ID Application* certificate, and the auto-updater requires a private
> updater key you generate and keep secret. **No private key or secret is in this
> repo** — the workflow reads everything from CI secrets (`${{ secrets.* }}`), and
> `tauri.conf.json` ships the owner's **public** updater key (safe to publish;
> only the matching **private** half is a secret). Until you add your CI secrets,
> the release workflow fails at the signing step (honestly). The moment you add
> your secrets + publish a release, signing and auto-update light up with no code
> changes.
>
> **Rotating the updater key:** if you don't have the private half of the
> committed public key, generate a fresh pair (`npm run tauri signer generate`)
> and paste the new **public** key into `tauri.conf.json` → `plugins.updater.pubkey`
> (this is safe — no signed release depends on the old key until one is published),
> then set the new private key as the `TAURI_SIGNING_PRIVATE_KEY` secret.

---

## What ships in the repo (and what does not)

| In the repo (public, safe) | NOT in the repo (you provide via secrets) |
| --- | --- |
| `.github/workflows/release.yml` (the pipeline) | Developer ID `.p12` certificate + its password |
| `hud/src-tauri/entitlements.plist` (hardened-runtime entitlements) | Apple ID + app-specific password + Team ID |
| `hud/src-tauri/Info.plist` (TCC usage strings) | The **private** Tauri updater key + its password |
| `tauri.conf.json` signing config (`signingIdentity: "-"`, `hardenedRuntime`, updater endpoint) | The updater **public** key (you paste it into `tauri.conf.json`) |
| The updater **public** key (committed in `tauri.conf.json`; rotate if you lack the private half) | — |

`signingIdentity: "-"` is the **ad-hoc / unsigned** default so a local
`tauri build` (no Apple account) still produces a runnable app. CI overrides it
with your real identity via the `APPLE_SIGNING_IDENTITY` secret.

---

## One-time setup

### 1. Get a Developer ID (Apple)

1. Enroll in the [Apple Developer Program](https://developer.apple.com/programs/)
   (paid). Note your 10-character **Team ID**.
2. In **Certificates, Identifiers & Profiles**, create a **Developer ID
   Application** certificate. Download it and add it to your Keychain.
3. Export it as a `.p12` (Keychain Access → right-click the cert → Export),
   choosing a password. Then base64-encode it for the secret:

   ```bash
   base64 -i developer_id.p12 | pbcopy   # now on your clipboard
   ```

4. Find your exact signing-identity string:

   ```bash
   security find-identity -v -p codesigning
   # -> "Developer ID Application: Your Name (TEAMID)"
   ```

5. Create an **app-specific password** for notarization at
   <https://appleid.apple.com> → Sign-In and Security → App-Specific Passwords.

### 2. Generate the updater keypair (Tauri)

Run this **once**, on your own machine, and keep the private key secret:

```bash
cd hud
npm run tauri signer generate -- -w ~/.darwin-updater.key
```

This prints (and writes) two things:

- a **private key** (the file `~/.darwin-updater.key`, plus its password if you
  set one) — **NEVER commit this; it goes into a CI secret only**;
- a **public key** (a base64 blob) — this is safe to publish.

Paste the **public key** into `hud/src-tauri/tauri.conf.json`, replacing the
currently-committed `pubkey` blob:

```jsonc
"plugins": {
  "updater": {
    "endpoints": [
      "https://github.com/darwin-capani/darwin/releases/latest/download/latest.json"
    ],
    "pubkey": "PASTE_YOUR_PUBLIC_KEY_HERE"
  }
}
```

> The `updates.rs` tripwire (`PUBKEY_PLACEHOLDER` / empty check) short-circuits to
> `not_configured` only if the committed key regresses to the placeholder sentinel
> or empty. Any real key (the one shipped, or your rotated one) leaves the in-app
> check armed to hit the endpoint.

### 3. Add the CI secrets

In the GitHub repo: **Settings → Secrets and variables → Actions → New
repository secret**. Add each of these (names must match exactly):

| Secret | Value |
| --- | --- |
| `APPLE_CERTIFICATE` | base64 of your Developer ID `.p12` (step 1.3) |
| `APPLE_CERTIFICATE_PASSWORD` | the `.p12` export password |
| `APPLE_SIGNING_IDENTITY` | `Developer ID Application: Your Name (TEAMID)` |
| `APPLE_ID` | your Apple ID email |
| `APPLE_PASSWORD` | the app-specific password (step 1.5) |
| `APPLE_TEAM_ID` | your 10-char Team ID |
| `TAURI_SIGNING_PRIVATE_KEY` | the contents of `~/.darwin-updater.key` (step 2) |
| `TAURI_SIGNING_PRIVATE_KEY_PASSWORD` | its password (set an empty secret if you used none) |

`GITHUB_TOKEN` is provided automatically by Actions — you do not add it.

---

## Cutting a release

1. Bump the version in **both** `hud/package.json` and
   `hud/src-tauri/tauri.conf.json` (`version`) to the new `X.Y.Z`, and commit.
2. Tag and push:

   ```bash
   git tag v0.2.0
   git push origin v0.2.0
   ```

3. The `release` workflow runs on `macos-latest` and:
   - runs the gates (`tsc`, `vitest`, `cargo test`) — a red gate aborts the release;
   - imports your Developer ID cert into a throwaway keychain;
   - builds the universal `.app`/`.dmg`, **code-signs** it with hardened runtime +
     `entitlements.plist`, **notarizes** + **staples** it with Apple, and **signs**
     the updater bundle with your private updater key;
   - creates a **draft** GitHub Release and uploads the `.app`/`.dmg` + `latest.json`.
4. Open the draft release, verify the assets, and **Publish** it. Publishing makes
   it the `latest` release, so the `latest.json` endpoint
   (`…/releases/latest/download/latest.json`) resolves and installed apps can
   auto-update.

### Verifying a built app locally (optional)

```bash
spctl -a -vv "DARWIN.app"     # should say: accepted, source=Notarized Developer ID
codesign -dv --verbose=4 "DARWIN.app"   # check the identity + hardened runtime flag
```

---

## How auto-update works for users

Once a signed release with `latest.json` is published, the **System Settings →
Updates** panel's **Check for updates** button (and the `check_for_updates`
command behind it) fetches the endpoint, compares versions, and — only if a newer
build exists — offers **Install**, which downloads the bundle, **verifies its
minisign signature against your public key**, and installs it. An unsigned or
wrong-key bundle is rejected, so a hostile endpoint cannot push code. Until a real
key + release exist, the panel honestly says updates are not armed yet.
