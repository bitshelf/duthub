//! Proxy policy for download clients.

use std::time::Duration;

use crate::error::ToolInstallError;

/// Proxy policy for one download run.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub enum ProxyConfig {
    /// Ignore proxy environment variables; always connect directly
    /// (reqwest `no_proxy()`, so `HTTP(S)_PROXY`/`ALL_PROXY` are not consulted).
    Off,
    /// Follow proxy *environment variables* (`HTTP_PROXY`, `HTTPS_PROXY`, `ALL_PROXY`,
    /// `NO_PROXY`) — read and registered as explicit reqwest proxies, so OS-level
    /// proxy settings are never consulted: neither via reqwest's `system-proxy`
    /// feature (omitted here) nor via the hyper-util `client-proxy-system` feature
    /// that other crates in the build graph may unify in.
    #[default]
    Env,
    /// Route every request through the given proxy URL (http://, https:// or socks5://).
    /// An unparsable URL fails the client build with `InvalidProxyUrl`.
    Custom(String),
}

impl ProxyConfig {
    /// Build the reqwest client for this policy.
    ///
    /// TLS: reqwest 0.13's default rustls stack (the `rustls` feature, same expansion as
    /// its `default-tls`). A process-wide rustls crypto provider installed by the
    /// embedding app (e.g. ring via `install_default()`) is honored first; otherwise
    /// reqwest falls back to its built-in provider. No total request timeout — large
    /// tool archives must not be cut off mid-stream — but a 30s connect timeout and a
    /// 60s read timeout: the read timeout applies per read operation and resets on
    /// every successful read, so a stalled connection (proxy/weak network delivering
    /// headers then no body) turns into an error instead of parking
    /// `next_chunk().await` forever. That bound is what keeps the install loop's
    /// cooperative cancellation meaningful: the cancel flag is only observed between
    /// chunks (`install.rs`), so a never-returning chunk would make the cancel
    /// unobservable and pin the download center's active slot until process exit.
    pub fn build_client(&self) -> Result<reqwest::Client, ToolInstallError> {
        let mut builder = reqwest::Client::builder()
            .connect_timeout(Duration::from_secs(30))
            .read_timeout(Duration::from_secs(60));
        builder = match self {
            ProxyConfig::Off => builder.no_proxy(),
            ProxyConfig::Env => {
                // Register env proxies explicitly instead of relying on
                // reqwest's implicit "system" matcher: that matcher also
                // consults OS proxy settings whenever the hyper-util
                // `client-proxy-system` feature is unified into the build by
                // another crate, and on Windows it then folds the registry
                // `ProxyOverride` bypass list into the policy, silently
                // overriding the env-only contract documented above. Adding
                // any explicit proxy (or calling `no_proxy`) disables the
                // auto matcher.
                //
                // Lookup order mirrors hyper-util: uppercase var first, then
                // lowercase; empty means unset; `ALL_PROXY` is the fallback
                // for both schemes; `REQUEST_METHOD` marks a CGI process,
                // whose proxy env must not be trusted.
                let cgi = std::env::var_os("REQUEST_METHOD").is_some();
                let no_proxy = reqwest::NoProxy::from_env();
                let all = env_proxy_var(&["ALL_PROXY", "all_proxy"]);
                let http = env_proxy_var(&["HTTP_PROXY", "http_proxy"]).or_else(|| all.clone());
                let https = env_proxy_var(&["HTTPS_PROXY", "https_proxy"]).or(all);
                if cgi || (http.is_none() && https.is_none()) {
                    // No (trusted) proxy env: stay direct, and keep the auto
                    // system matcher disabled all the same.
                    builder.no_proxy()
                } else {
                    if let Some(url) = http {
                        let proxy = reqwest::Proxy::http(url.clone())
                            .map_err(|source| ToolInstallError::InvalidProxyUrl { url, source })?
                            .no_proxy(no_proxy.clone());
                        builder = builder.proxy(proxy);
                    }
                    if let Some(url) = https {
                        let proxy = reqwest::Proxy::https(url.clone())
                            .map_err(|source| ToolInstallError::InvalidProxyUrl { url, source })?
                            .no_proxy(no_proxy);
                        builder = builder.proxy(proxy);
                    }
                    builder
                }
            }
            ProxyConfig::Custom(url) => {
                builder.proxy(reqwest::Proxy::all(url).map_err(|source| {
                    ToolInstallError::InvalidProxyUrl {
                        url: url.clone(),
                        source,
                    }
                })?)
            }
        };
        builder
            .build()
            .map_err(|source| ToolInstallError::ClientBuild { source })
    }
}

/// First non-empty proxy URL among the given env vars, mirroring hyper-util's
/// lookup order (uppercase form first, then lowercase; empty means unset).
fn env_proxy_var(names: &[&str]) -> Option<String> {
    names
        .iter()
        .find_map(|name| std::env::var(name).ok().filter(|value| !value.is_empty()))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn build_ok(config: &ProxyConfig) {
        config.build_client().expect("client should build");
    }

    fn build_err(config: &ProxyConfig) -> ToolInstallError {
        config.build_client().expect_err("client should not build")
    }

    #[tokio::test]
    async fn off_and_env_build_clients() {
        build_ok(&ProxyConfig::Off);
        build_ok(&ProxyConfig::Env);
    }

    #[tokio::test]
    async fn custom_http_and_socks5_urls_build() {
        build_ok(&ProxyConfig::Custom("http://127.0.0.1:8080".to_string()));
        build_ok(&ProxyConfig::Custom(
            "https://proxy.example:8443".to_string(),
        ));
        // socks5 requires the reqwest `socks` feature; the build must succeed with it on.
        build_ok(&ProxyConfig::Custom("socks5://127.0.0.1:1080".to_string()));
    }

    #[tokio::test]
    async fn invalid_custom_urls_are_rejected_clearly() {
        // Note: reqwest's Proxy::all validates URL syntax but tolerates a scheme-only
        // "socks5://" (the failure would surface at connect time), so that form is not
        // asserted here; these are URLs the url parser itself rejects.
        for url in ["not a url", "http://[bad", "http://:notaport"] {
            let err = build_err(&ProxyConfig::Custom(url.to_string()));
            let ToolInstallError::InvalidProxyUrl { url: reported, .. } = err else {
                panic!("expected InvalidProxyUrl for {url:?}, got {err:?}");
            };
            assert_eq!(reported, url);
        }
    }
}
