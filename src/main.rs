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
        // fast path
        if let Some(t) = self.token.read().await.as_ref() {
            if Instant::now() + self.leeway < t.valid_until {
                return Ok(t.header_value.clone());
            }
        }
        // single-flight refresh
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
        let resp = self.http.post(&self.token_url).form(&form).send()
            .await.map_err(|e| format!("token http err: {e}"))?;
        if !resp.status().is_success() {
            return Err(format!("token status {}", resp.status()));
        }
        let tr: TokenResp = resp.json().await.map_err(|e| format!("token json {e}"))?;
        let ttl = tr.expires_in.max(60);
        let valid_until = Instant::now() + Duration::from_secs(ttl.saturating_sub(10));
        let header_value = format!("Bearer {}", tr.access_token);
        *self.token.write().await = Some(CachedToken { header_value: header_value.clone(), valid_until });
        Ok(header_value)
    }
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

    // get/refresh token
    let bearer = st.bearer().await.map_err(|e| (StatusCode::BAD_GATEWAY, e))?;

    // build outgoing headers (strip hop-by-hop & client auth)
    let mut out = HeaderMap::new();
    for (k, v) in headers.iter() {
        let n = k.as_str().to_ascii_lowercase();
        if matches!(n.as_str(),
            "connection"|"keep-alive"|"proxy-authenticate"|"proxy-authorization"|
            "te"|"trailer"|"transfer-encoding"|"upgrade"|"authorization"
        ) { continue; }
        out.append(k, v.clone());
    }
    out.insert("authorization", HeaderValue::from_str(&bearer).unwrap());

    // forward request

    let bytes = body
        .collect()
        .await
        .map_err(|e| (StatusCode::BAD_REQUEST, e.to_string()))?
        .to_bytes();

    let resp = st.http.request(method.clone(), &upstream)
        .headers(out)
        .body(bytes)
        .send().await.map_err(|e| (StatusCode::BAD_GATEWAY, e.to_string()))?;

    // return response
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
    let body_bytes = resp.bytes().await.map_err(|e| (StatusCode::BAD_GATEWAY, e.to_string()))?;
    Ok((status, resp_headers, body_bytes))
}

#[tokio::main]
async fn main() {
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
