use axum::{
    body::Body,
    extract::{OriginalUri, State},
    http::{HeaderMap, HeaderValue, Method, StatusCode},
    response::IntoResponse,
    routing::any,
    Router,
};
use reqwest::Client;
use serde::Deserialize;
use std::{collections::HashMap, env, sync::Arc, time::{Duration, Instant}};
use tokio::net::TcpListener;
use tokio::sync::{Mutex, RwLock};
use http_body_util::BodyExt as _;   // <- important
use bytes::Bytes;
use hex;
use tracing::{debug, info, warn};
use dotenvy::dotenv;
#[derive(Clone)]
struct AppState {
    http: Client,
    upstream_base: String,     // e.g. https://enterprise.blockstream.info/api
    token_url: String,         // OIDC token endpoint
    client_id: String,
    client_secret: String,
    // (optional) shared secret your app sends via set_chain_source_esplora_with_headers
    token: Arc<RwLock<Option<CachedToken>>>,
    refresh_lock: Arc<Mutex<()>>,
    leeway: Duration,          // refresh a bit before expiry
}

#[derive(Clone)]
struct CachedToken {
    header_value: String,      // "Bearer <access_token>"
    valid_until: Instant,
}

#[derive(Deserialize)]
struct TokenResp {
    access_token: String,
    expires_in: u64,
}

impl AppState {
    async fn bearer(&self) -> Result<String, String> {
        if let Some(t) = self.token.read().await.as_ref() {
            if Instant::now() + self.leeway < t.valid_until {
                return Ok(t.header_value.clone());
            }
        }

        let _g = self.refresh_lock.lock().await;

        if let Some(t) = self.token.read().await.as_ref() {
            if Instant::now() + self.leeway < t.valid_until {
                return Ok(t.header_value.clone());
            }
        }

        let form = [
            ("grant_type", "client_credentials"),
            ("client_id", self.client_id.as_str()),
            ("client_secret", self.client_secret.as_str()),
            ("scope", "openid"),
        ];

        let t0 = Instant::now();
        let resp = self.http.post(&self.token_url).form(&form).send().await;
        match resp {
            Ok(r) if r.status().is_success() => {
                let status = r.status();
                let tr: TokenResp = r.json().await.map_err(|e| {
                    warn!(err = %e, "token json parse failed");
                    format!("token json {e}")
                })?;
                let ttl = tr.expires_in.max(60);
                let valid_until = Instant::now() + Duration::from_secs(ttl.saturating_sub(10));
                let header_value = format!("Bearer {}", tr.access_token);

                *self.token.write().await = Some(CachedToken {
                    header_value: header_value.clone(),
                    valid_until,
                });

                info!(
                    took_ms = %t0.elapsed().as_millis(),
                    status = %status,
                    expires_in_s = tr.expires_in,
                    "esplora token refreshed"
                );
                Ok(header_value)
            }
            Ok(r) => {
                warn!(
                    took_ms = %t0.elapsed().as_millis(),
                    status = %r.status(),
                    "token refresh http failure"
                );
                Err(format!("token status {}", r.status()))
            }
            Err(e) => {
                warn!(took_ms = %t0.elapsed().as_millis(), err = %e, "token refresh request failed");
                Err(format!("token http err: {e}"))
            }
        }
    }
}


fn redact_headers(h: &HeaderMap) -> Vec<(String, String)> {
    h.iter()
        .filter_map(|(k, v)| {
            let name = k.as_str().to_string();
            let lower = name.to_ascii_lowercase();
            let val_str = v.to_str().unwrap_or("<bin>").to_string();
            if lower == "authorization" {
                Some((name, "<redacted>".into()))
            } else {
                Some((name, val_str))
            }
        })
        .collect()
}

async fn proxy(
    State(st): State<AppState>,
    method: Method,
    headers: HeaderMap,
    OriginalUri(orig): OriginalUri,
    body: Body,
) -> Result<impl IntoResponse, (StatusCode, String)> {
    // build upstream URL
    let mut pathq = orig.path().to_string();
    if let Some(q) = orig.query() { pathq.push('?'); pathq.push_str(q); }
    let upstream = format!("{}{}", st.upstream_base.trim_end_matches('/'), pathq);

    // log inbound request (redacted)
    debug!(
        method = %method,
        path = %orig.path(),
        query = orig.query().unwrap_or(""),
        req_headers = ?redact_headers(&headers),
        upstream = %upstream,
        "proxy request"
    );

    // read request body (rarely used by Esplora; still handle it)
    let req_bytes: Bytes = body
        .collect().await
        .map_err(|e| (StatusCode::BAD_REQUEST, e.to_string()))?
        .to_bytes();

    // get/refresh token
    let bearer = st.bearer().await.map_err(|e| (StatusCode::BAD_GATEWAY, e))?;

    // outgoing headers (strip hop-by-hop & incoming auth)
    let mut out = HeaderMap::new();
    for (k, v) in headers.iter() {
        let n = k.as_str().to_ascii_lowercase();
        if matches!(n.as_str(),
            "connection"|"keep-alive"|"proxy-authenticate"|"proxy-authorization"|
            "te"|"trailer"|"transfer-encoding"|"upgrade"|"authorization"|"host"
        ) { continue; }
        out.append(k, v.clone());
    }
    out.insert("authorization", HeaderValue::from_str(&bearer).unwrap());

    let t0 = Instant::now();
    let resp = st.http
        .request(method.clone(), &upstream)
        .headers(out)
        .body(req_bytes)
        .send()
        .await
        .map_err(|e| (StatusCode::BAD_GATEWAY, e.to_string()))?;

    let status = StatusCode::from_u16(resp.status().as_u16()).unwrap();
    let mut resp_headers = HeaderMap::new();
    for (k, v) in resp.headers().iter() {
        let n = k.as_str().to_ascii_lowercase();
        if matches!(n.as_str(),
            "connection"|"keep-alive"|"proxy-authenticate"|"proxy-authorization"|
            "te"|"trailer"|"transfer-encoding"|"upgrade"
        ) { continue; }
        resp_headers.append(k, v.clone());
    }

    // optionally dump some of the body for debugging
    let dump_n: usize = std::env::var("DUMP_BODY_BYTES")
        .ok()
        .and_then(|s| s.parse().ok())
        .unwrap_or(0);

    let body_bytes = resp.bytes().await.map_err(|e| (StatusCode::BAD_GATEWAY, e.to_string()))?;
    let len = body_bytes.len();
    if dump_n > 0 {
        let take = dump_n.min(len);
        let preview = &body_bytes[..take];
        // try to log as UTF-8; if binary, show hex of first few bytes
        if let Ok(s) = std::str::from_utf8(preview) {
            debug!(status=%status, len=len, took_ms=%t0.elapsed().as_millis(), preview=?s, "proxy response");
        } else {
            debug!(status=%status, len=len, took_ms=%t0.elapsed().as_millis(), preview_hex=%hex::encode(preview), "proxy response");
        }
    } else {
        debug!(
            status = %status,
            len = len,
            took_ms = %t0.elapsed().as_millis(),
            resp_headers = ?redact_headers(&resp_headers),
            "proxy response"
        );
    }

    Ok((status, resp_headers, body_bytes))
}

#[tokio::main]
async fn main() {
    dotenv().ok();

    tracing_subscriber::fmt()
        .with_env_filter(tracing_subscriber::EnvFilter::from_default_env())
        .compact()
        .init();
    // ENV
    let upstream = env::var("ESPLORA_UPSTREAM")
        .unwrap_or_else(|_| "https://enterprise.blockstream.info/api".to_string());
    let token_url = env::var("OIDC_TOKEN_URL")
        .unwrap_or_else(|_| "https://login.blockstream.com/realms/blockstream-public/protocol/openid-connect/token".to_string());
    let client_id     = env::var("ESPLORA_CLIENT_ID").expect("ESPLORA_CLIENT_ID missing");
    let client_secret = env::var("ESPLORA_CLIENT_SECRET").expect("ESPLORA_CLIENT_SECRET missing");
    let bind = env::var("BIND").unwrap_or_else(|_| "127.0.0.1:3002".to_string());



    let st = AppState {
        http: Client::builder()
            .pool_max_idle_per_host(32)
            .pool_idle_timeout(Duration::from_secs(45))
            .gzip(true).brotli(true).deflate(true)
            .build().unwrap(),
        upstream_base: upstream,
        token_url,
        client_id,
        client_secret,
        token: Arc::new(RwLock::new(None)),
        refresh_lock: Arc::new(Mutex::new(())),
        leeway: Duration::from_secs(20),
    };

    // warm token in background & refresh every ~4 min
    {
        let st2 = st.clone();
        tokio::spawn(async move {
            loop {
                let _ = st2.bearer().await;
                tokio::time::sleep(Duration::from_secs(240)).await;
            }
        });
    }

    let app = Router::new()
        .route("/*path", any(proxy))
        .with_state(st);

    println!("esplora_auth_proxy listening on http://{bind}");
    let listener = TcpListener::bind(&bind).await.unwrap();
    println!("esplora_auth_proxy listening on http://{bind}");
    axum::serve(listener, app).await.unwrap();
}
