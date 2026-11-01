//! Identity for cmbd. Two kinds of bearer credential are accepted side by side:
//! - static tokens (`cmbt_…`), minted with `cmbd token`, stored only as hashes;
//! - OIDC ID/access tokens (JWTs) from your identity provider, verified
//!   against its published signing keys (JWKS). Nothing here is tied to one provider.
//!
//! OIDC is off unless CAMBIUM_OIDC_ISSUER is set. Settings:
//!   CAMBIUM_OIDC_ISSUER        expected `iss` (exact match)
//!   CAMBIUM_OIDC_AUDIENCE      expected `aud` (the app registration's client id / API audience)
//!   CAMBIUM_OIDC_JWKS_FILE     path to a JWKS JSON file, or
//!   CAMBIUM_OIDC_JWKS_URL      URL to fetch it from (refetched on unknown key id, at most every 5 min)
//!   CAMBIUM_OIDC_USER_CLAIM    claim used as the user name (default `preferred_username`, then `email`, then `sub`)
//!   CAMBIUM_OIDC_GROUPS_CLAIM  claim holding group names or ids (default `groups`)
use cambium_core::{Engine, Error, Principal};
use jsonwebtoken::{decode, decode_header, jwk::JwkSet, Algorithm, DecodingKey, Validation};
use serde_json::Value;
use std::sync::RwLock;
use std::time::{Duration, Instant};

pub struct OidcConfig {
    pub issuer: String,
    pub audience: String,
    pub jwks_file: Option<String>,
    pub jwks_url: Option<String>,
    pub user_claim: Option<String>,
    pub groups_claim: String,
}

impl OidcConfig {
    pub fn from_env() -> Option<Self> {
        let issuer = std::env::var("CAMBIUM_OIDC_ISSUER").ok().filter(|s| !s.is_empty())?;
        let audience = std::env::var("CAMBIUM_OIDC_AUDIENCE")
            .ok()
            .filter(|s| !s.is_empty())
            .unwrap_or_else(|| panic!("CAMBIUM_OIDC_AUDIENCE is required with CAMBIUM_OIDC_ISSUER"));
        let jwks_file = std::env::var("CAMBIUM_OIDC_JWKS_FILE").ok().filter(|s| !s.is_empty());
        let jwks_url = std::env::var("CAMBIUM_OIDC_JWKS_URL").ok().filter(|s| !s.is_empty());
        if jwks_file.is_none() && jwks_url.is_none() {
            panic!("set CAMBIUM_OIDC_JWKS_FILE or CAMBIUM_OIDC_JWKS_URL");
        }
        Some(Self {
            issuer,
            audience,
            jwks_file,
            jwks_url,
            user_claim: std::env::var("CAMBIUM_OIDC_USER_CLAIM").ok().filter(|s| !s.is_empty()),
            groups_claim: std::env::var("CAMBIUM_OIDC_GROUPS_CLAIM").unwrap_or_else(|_| "groups".into()),
        })
    }
}

pub struct Oidc {
    cfg: OidcConfig,
    keys: RwLock<(JwkSet, Instant)>,
}

impl Oidc {
    pub fn new(cfg: OidcConfig) -> Result<Self, String> {
        let keys = Self::load(&cfg)?;
        Ok(Self { cfg, keys: RwLock::new((keys, Instant::now())) })
    }
    fn load(cfg: &OidcConfig) -> Result<JwkSet, String> {
        let text = if let Some(f) = &cfg.jwks_file {
            std::fs::read_to_string(f).map_err(|e| format!("read {f}: {e}"))?
        } else {
            let u = cfg.jwks_url.as_ref().unwrap();
            ureq::get(u)
                .timeout(Duration::from_secs(10))
                .call()
                .map_err(|e| format!("fetch {u}: {e}"))?
                .into_string()
                .map_err(|e| e.to_string())?
        };
        serde_json::from_str(&text).map_err(|e| format!("bad JWKS: {e}"))
    }
    fn key_for(&self, kid: Option<&str>) -> Option<(DecodingKey, Algorithm)> {
        let find = |set: &JwkSet| -> Option<(DecodingKey, Algorithm)> {
            let jwk = match kid {
                Some(k) => set.find(k)?,
                None if set.keys.len() == 1 => &set.keys[0],
                None => return None,
            };
            let alg = match &jwk.algorithm {
                jsonwebtoken::jwk::AlgorithmParameters::RSA(_) => Algorithm::RS256,
                jsonwebtoken::jwk::AlgorithmParameters::EllipticCurve(_) => Algorithm::ES256,
                _ => return None, // symmetric keys are never accepted from a JWKS
            };
            Some((DecodingKey::from_jwk(jwk).ok()?, alg))
        };
        if let Some(k) = find(&self.keys.read().unwrap().0) {
            return Some(k);
        }
        // unknown key id: the provider may have rotated keys; refetch (rate-limited)
        let mut w = self.keys.write().unwrap();
        if w.1.elapsed() > Duration::from_secs(300) {
            if let Ok(set) = Self::load(&self.cfg) {
                *w = (set, Instant::now());
            }
        }
        find(&w.0)
    }
    pub fn verify(&self, jwt: &str) -> Result<Principal, Error> {
        let h = decode_header(jwt).map_err(|_| Error::Unauthorized)?;
        let (key, alg) = self.key_for(h.kid.as_deref()).ok_or(Error::Unauthorized)?;
        if h.alg != alg {
            return Err(Error::Unauthorized);
        }
        let mut v = Validation::new(alg);
        v.set_issuer(&[&self.cfg.issuer]);
        v.set_audience(&[&self.cfg.audience]);
        v.leeway = 60;
        let claims: Value = decode::<Value>(jwt, &key, &v).map_err(|_| Error::Unauthorized)?.claims;
        let user = match &self.cfg.user_claim {
            Some(c) => claims[c].as_str(),
            None => claims["preferred_username"].as_str().or(claims["email"].as_str()).or(claims["sub"].as_str()),
        }
        .filter(|u| !u.is_empty())
        .ok_or(Error::Unauthorized)?
        .to_string();
        let groups = match &claims[&self.cfg.groups_claim] {
            Value::Array(a) => a.iter().filter_map(|g| g.as_str().map(String::from)).collect(),
            Value::String(s) => s.split([',', ' ']).filter(|g| !g.is_empty()).map(String::from).collect(),
            _ => vec![],
        };
        Ok(Principal { user, groups })
    }
}

/// Static tokens first (cheap, local), then OIDC if configured.
pub fn authenticate(eng: &Engine, oidc: Option<&Oidc>, bearer: &str) -> Result<Principal, Error> {
    if bearer.starts_with("cmbt_") {
        return eng.verify_principal(bearer);
    }
    match oidc {
        Some(o) if bearer.split('.').count() == 3 => o.verify(bearer),
        _ => Err(Error::Unauthorized),
    }
}
