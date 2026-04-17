# ZECK — Claude Context

## Project

ZECK is a Zcash wallet recovery tool for ZecWallet Lite seeds. It has three components:
- `crates/zeck-core` — shared Rust library (derivation, scanning, sweeping)
- `crates/zeck-cli` — command-line interface
- `gui/` — Tauri v2 desktop app (static HTML/JS frontend + Rust backend)

## Key Technical Facts

### lightwalletd connection drops (GoAway)
Long syncs against public lightwalletd endpoints will regularly receive HTTP/2 GoAway frames (`NO_ERROR`). This is normal server-side connection recycling, not a bug. **ZECK handles this with `run_wallet_sync_with_retry` in `crates/zeck-core/src/scan.rs`** — it catches transport errors (GoAway, TLS close_notify, TimedOut, UnexpectedEof, h2 protocol error) and automatically reconnects up to 10 times with a 5-second delay between attempts, re-probing all configured lightwalletd endpoints on each retry.

### Tauri frontend
- Uses `withGlobalTauri: true` — access Tauri APIs via `window.__TAURI__.core.invoke` and `window.__TAURI__.event.listen`, NOT bare ES module imports
- No bundler — `<script src="./main.js">` not `type="module"`
- Default data directory is `/tmp/zeck_data` — do NOT write workspace files inside `src-tauri/` or the Tauri dev watcher will trigger a rebuild mid-scan

### Lightwalletd endpoints
- Mainnet: `https://zec.rocks:443`, `https://na.zec.rocks:443`
- Testnet: `https://lightwalletd.testnet.electriccoin.co:9067`
- Always include `https://` prefix — bare `host:port` fails TLS

### Test seed (BIP-39 test vector, no real funds)
`abandon abandon abandon abandon abandon abandon abandon abandon abandon abandon abandon abandon abandon abandon abandon abandon abandon abandon abandon abandon abandon abandon abandon art`

## GitHub
https://github.com/Bedrock-Strata/zec-wallet-rescue
