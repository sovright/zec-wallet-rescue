# Release Signing

How Argos release artifacts are code-signed. macOS signing/notarization is
configured via Apple secrets in the `build-macos` job; this document covers
**Windows** signing via **Azure Trusted Signing**.

## Architecture

Windows MSI and NSIS installers — and the inner `Argos.exe` they wrap — are
signed by **Azure Trusted Signing** (a cloud-held key; no certificate file or
signing secret ever lives in CI).

- The `build-windows` job authenticates to Entra ID with **OIDC** via
  `azure/login` (federated credential, no stored secret).
- Tauri's `signCommand` invokes the Windows SDK `signtool.exe` with Microsoft's
  `Azure.CodeSigning.Dlib.dll`. The dlib authenticates with
  `DefaultAzureCredential`, which reuses the `azure/login` session.
- Because signing happens **during** `tauri build`, the existing
  `Compute artifact hashes` step — and therefore the **SLSA provenance** — covers
  the *signed* bytes. No step reordering is required.
- `tauri.conf.json` carries **no** signing config; it is injected at release
  time via `tauri build --config`. Local/dev builds are therefore unsigned.

## Azure resources

| Resource | Value |
|---|---|
| Subscription | `cd8568f1-be90-45d3-8bdf-65b2c3f09ad2` (Pay-As-You-Go) |
| Resource group | `argos-signing` (East US) |
| Trusted Signing account | `argossigning` |
| Endpoint | `https://eus.codesigning.azure.net/` |
| Test cert profile | `argos-test` (`PublicTrustTest` — chains to a **test** root, not trusted by Windows) |
| Production cert profile | `argos` (`PublicTrust` — created after org identity validation completes) |
| CI app registration | `argos-windows-signing`, appId `aecd3457-d4f7-44fa-9f36-f6d205946288` |
| RBAC | `Artifact Signing Certificate Profile Signer` on the account, granted to the app's service principal |
| Federated credential subject | `repo:sovright/zec-wallet-rescue:environment:release-sign` |

## GitHub configuration

Non-secret signing parameters are stored as **`release-sign` environment
variables** (no secrets — OIDC needs none):

`AZURE_SIGNING_CLIENT_ID`, `AZURE_SIGNING_TENANT_ID`,
`AZURE_SIGNING_SUBSCRIPTION_ID`, `AZURE_SIGNING_ENDPOINT`,
`AZURE_SIGNING_ACCOUNT`, `AZURE_SIGNING_PROFILE`.

## Identity validation (one-time, Azure portal)

The `PublicTrust` certificate profile cannot be created until an **organization
identity validation** for the publishing legal entity (Iqlusion) is
**Completed** by Microsoft. This is portal-only — there is no CLI/ARM surface.

1. Portal → **Trusted Signing Accounts** → `argossigning` → **Identity
   validations** → **+ New identity validation** → **Organization**.
2. Fill in the Iqlusion legal entity details; submit. Microsoft reviews
   (typically 1–7 business days). A verification email may be sent to the
   listed contact.
3. The validation gets an **ID (GUID) immediately**, with status *In Progress*.
   This ID is reusable for both the test and production profiles.

## Standing up / testing the pipeline before validation completes

A `PublicTrustTest` profile can be created against the *in-progress* validation
ID, letting the whole CI pipeline be exercised before the org validation is
approved. Test-profile signatures are **not** trusted by Windows (test root) —
they validate the plumbing only.

```bash
az trustedsigning certificate-profile create \
  -g argos-signing --account-name argossigning \
  -n argos-test --profile-type PublicTrustTest \
  --identity-validation-id <VALIDATION_GUID>
```

## Going to production (after validation is Completed)

1. Create the public-trust profile:

   ```bash
   az trustedsigning certificate-profile create \
     -g argos-signing --account-name argossigning \
     -n argos --profile-type PublicTrust \
     --identity-validation-id <VALIDATION_GUID>
   ```

2. Flip the one variable:

   ```bash
   gh variable set AZURE_SIGNING_PROFILE \
     --repo sovright/zec-wallet-rescue --env release-sign --body argos
   ```

3. Tag a release (`v*`). The `build-windows` job now produces trusted,
   timestamped signatures.

## Verifying a signed artifact

On Windows:

```powershell
signtool verify /pa /v Argos_<version>_x64-setup.exe
```

Or inspect the Authenticode publisher in the file's *Digital Signatures* tab.
The publisher should read the validated Iqlusion organization name (production
profile) — a test-profile signature will show as untrusted.
