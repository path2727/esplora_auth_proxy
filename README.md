# Esplora Auth Proxy — Quick Setup

This README explains how to:

1. Sign up and get API credentials for Blockstream Enterprise Explorer.
2. Configure environment variables (`.env`).
3. Build and run the Rust proxy that auto-refreshes OAuth tokens and forwards Esplora requests.
4. Point LDK Node (or any client) at the proxy.
5. Verify and troubleshoot.

---

## 1. Create your Blockstream Enterprise account and credentials

- Go to: [https://enterprise.blockstream.info](https://enterprise.blockstream.info)
- Create an account and complete any onboarding steps.
- In the dashboard, go to **Manage Keys** and create a key:
  - `CLIENT_ID`
  - `CLIENT_SECRET`
- Confirm the token endpoint (OIDC):  
  `https://login.blockstream.com/realms/blockstream-public/protocol/openid-connect/token`
- Confirm the Esplora base endpoint for mainnet:  
  `https://enterprise.blockstream.info/api`  
  (Other networks are listed in their docs: `/testnet/api`, `/liquid/api`, `/liquidtestnet/api`)

⚠️ Access tokens expire after ~300 seconds; this proxy handles refresh automatically.

---

## 2. `.env` configuration

Create a `.env` file in the same directory where you will run the proxy binary.  
**Do not commit this file to source control.**

You can reference `example.env` included in the repository.

### Example `.env` (mainnet)

```dotenv
# Upstream Esplora base (no trailing slash)
ESPLORA_UPSTREAM=https://enterprise.blockstream.info/api

# OAuth2 / OIDC token endpoint (client_credentials grant)
OIDC_TOKEN_URL=https://login.blockstream.com/realms/blockstream-public/protocol/openid-connect/token

# Your enterprise credentials (keep secret; rotate if leaked)
ESPLORA_CLIENT_ID=your_client_id_here
ESPLORA_CLIENT_SECRET=your_client_secret_here

# Proxy bind address (loopback keeps it local-only)
BIND=127.0.0.1:3002

# Logging (tracing-subscriber uses this)
RUST_LOG=info

# Optional: dump first N bytes of each response body for debugging
# DUMP_BODY_BYTES=256
```

**Tip:** For testnet, change:

```dotenv
ESPLORA_UPSTREAM=https://enterprise.blockstream.info/testnet/api
```

---

## 3. Build & run the proxy

**Prereqs:** Rust toolchain (stable), cargo.

From the proxy project directory:

```bash
cargo build
cargo run
```

You should see:

```
esplora_auth_proxy listening on http://127.0.0.1:3002
… "esplora token refreshed" logs shortly after start
```

- **Linux/systemd**: put your env vars in `/etc/default/esplora-auth-proxy` and reference it in a systemd unit with `EnvironmentFile=`.
- **WSL/Windows**: running via WSL is fine; keep `BIND=127.0.0.1:3002`.

---

## 4. Test with curl

Basic sanity checks:

```bash
curl http://127.0.0.1:3002/blocks/tip/height
curl http://127.0.0.1:3002/blocks/tip/hash
curl http://127.0.0.1:3002/tx/<txid>
```

Expected: HTTP 200 with height/hash/JSON response.  
If you see:

```json
{"error_msg":"404 Route Not Found"}
```

→ see Troubleshooting below.

---

## 5. Usage with LDK Node

Point LDK Node at the local proxy so it treats it like a normal Esplora server. The proxy handles OAuth and injects `Authorization: Bearer …` for you.


```rust
builder.set_chain_source_esplora(
    "http://127.0.0.1:3002".into(),
    Some(EsploraSyncConfig::default()),
);
```

The proxy adds the `Authorization` header upstream; your node doesn’t need to know about tokens.

### Network switching

Change `.env` (proxy) and LDK network accordingly:

- **Mainnet**  
  Proxy: `ESPLORA_UPSTREAM=https://enterprise.blockstream.info/api`  
  LDK: `Network::Bitcoin`

- **Testnet**  
  Proxy: `ESPLORA_UPSTREAM=https://enterprise.blockstream.info/testnet/api`  
  LDK: `Network::Testnet`

- **Liquid**  
  Proxy: `.../liquid/api` (or `.../liquidtestnet/api`)  
  LDK: (still `Network::Bitcoin` for Lightning; Liquid is not a Lightning chain)

Restart the proxy after changing `.env`.

### Health check

```bash
curl http://127.0.0.1:3002/blocks/tip/height
```

If it returns a height (200), the node can sync via Esplora.

---

## 6. Troubleshooting

- **404 "Route Not Found" from Enterprise**
  - Most common cause: forwarding the wrong `Host` header upstream. Ensure your proxy strips it.
  - Another cause: double-prefixing `/api`.  
    Example: `https://enterprise.blockstream.info/api/api/blocks/tip/height`
  - Correct examples:  
    - `ESPLORA_UPSTREAM=https://enterprise.blockstream.info/api` → `http://127.0.0.1:3002/blocks/tip/height`  
    - `ESPLORA_UPSTREAM=https://enterprise.blockstream.info` → `http://127.0.0.1:3002/api/blocks/tip/height`
  - Test against the public API to isolate issues:  
    `ESPLORA_UPSTREAM=https://blockstream.info/api`

- **401/403 errors**
  - Check `ESPLORA_CLIENT_ID` / `ESPLORA_CLIENT_SECRET`
  - Ensure `OIDC_TOKEN_URL` is correct
  - Tokens expire quickly (~5 minutes); proxy refreshes every ~4 minutes. Look for `"esplora token refreshed"` logs.

- **429 (rate limiting)**
  - Slow down clients or add caching.

- **Logging**
  - Set `RUST_LOG=debug` for detailed logs.
  - Use `DUMP_BODY_BYTES=512` to preview partial response bodies.

- **Firewall**
  - Keep the proxy bound to `127.0.0.1` or firewall the port.

---

## 7. Example: switching networks

- **Mainnet**  
  `ESPLORA_UPSTREAM=https://enterprise.blockstream.info/api`

- **Testnet**  
  `ESPLORA_UPSTREAM=https://enterprise.blockstream.info/testnet/api`

- **Liquid mainnet**  
  `ESPLORA_UPSTREAM=https://enterprise.blockstream.info/liquid/api`

- **Liquid testnet**  
  `ESPLORA_UPSTREAM=https://enterprise.blockstream.info/liquidtestnet/api`

Restart the proxy after changing .env or export variables.

---

## 8. Security reminders

- Never commit `.env` or `CLIENT_SECRET` to git.
- Rotate credentials if leaked.
- Avoid logging tokens (the proxy redacts Authorization).
- Keep the proxy on loopback or behind a firewall.

---

✅ Once `/blocks/tip/height` returns a height, you can point clients (e.g., LDK Node) at `http://127.0.0.1:3002` as if it were a regular Esplora.
