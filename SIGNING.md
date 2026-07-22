# macOS Code Signing

## Current State

CI signs macOS release binaries with an **Apple Developer ID Application** certificate (hardened runtime). This is active in `.github/workflows/release.yml`.

```
codesign --force \
  --sign 'Developer ID Application: D5 Inc. (7N8XHQWCNN)' \
  --identifier 'com.aspect.heart-portal' \
  --options runtime \
  --keychain $KEYCHAIN_PATH \
  <binary>
```

| Field | Value |
|-------|--------|
| Identity | `Developer ID Application: D5 Inc. (7N8XHQWCNN)` |
| Identifier | `com.aspect.heart-portal` |
| Team ID | `7N8XHQWCNN` |
| Certificate expires | 2031 |

`--timestamp` is intentionally omitted (Apple's timestamp server can be slow/unreliable in CI). `--options runtime` is kept so binaries remain ready for notarization later.

## Why

macOS Sequoia's TCC tracks permission grants. With ad-hoc signing, TCC primarily matches on CDHash, so every rebuild can re-prompt. With Developer ID signing, TCC matches on `TeamIdentifier + BundleIdentifier`, so upgrades keep existing grants (accessibility, screen recording, etc.).

## GitHub Secrets

| Secret | Contents |
|--------|----------|
| `APPLE_CERTIFICATE_BASE64` | Base64-encoded `.p12` (Developer ID Application cert + private key) |
| `APPLE_CERTIFICATE_PASSWORD` | Password for the `.p12` file |

Set under repository **Settings → Secrets and variables → Actions**.

## CI signing flow (macOS only)

1. Decode `APPLE_CERTIFICATE_BASE64` to a temporary `.p12`
2. Create an ephemeral keychain (`actions-keychain` password)
3. Import the `.p12` into that keychain
4. Download and import Apple **Developer ID G2** intermediate:  
   https://www.apple.com/certificateauthority/DeveloperIDG2CA.cer
5. Add the keychain to the user search list and set the partition list so `codesign` can use the key without UI prompts
6. Sign and verify (`codesign --verify --deep --strict`)
7. Delete the temporary keychain (always runs, even if signing fails)

Linux and Windows builds are unchanged (no Apple signing).

## Rotating the certificate (expires 2031)

1. In [Apple Developer](https://developer.apple.com/account/resources/certificates/list), create a new **Developer ID Application** certificate (or renew before expiry).
2. Install it in Keychain Access, then export as `.p12` (include private key). Choose a strong export password.
3. Base64-encode the `.p12`:
   ```bash
   base64 -i DeveloperID.p12 | pbcopy   # macOS — copies to clipboard
   ```
4. Update GitHub secrets:
   - `APPLE_CERTIFICATE_BASE64` → new base64 string
   - `APPLE_CERTIFICATE_PASSWORD` → new `.p12` password
5. Trigger a test release (`workflow_dispatch` or a tag) and confirm:
   ```bash
   codesign -dv --verbose=2 heart-portal-macos-arm64
   # Authority=Developer ID Application: D5 Inc. (7N8XHQWCNN)
   # Identifier=com.aspect.heart-portal
   ```
6. Revoke or delete the old certificate / `.p12` after the new one is verified in CI.

Keep the signing **identity** string and **identifier** (`com.aspect.heart-portal`) stable unless you intentionally change TCC identity for users.

## Future: Notarization

Notarization is not enabled yet. When ready, submit signed binaries with `notarytool` (requires App Store Connect API key / Apple ID credentials as additional secrets). Hardened runtime (`--options runtime`) is already applied.
