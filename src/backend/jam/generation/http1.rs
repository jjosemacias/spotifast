//! One ephemeral H1 transport for the explicit experiment, never a fallback.
use super::*;
use bytes::Bytes;
use http_body_util::Full;
use hyper_proxy2::{Intercept, Proxy, ProxyConnector};
use hyper_rustls::{ConfigBuilderExt, DefaultServerNameResolver, HttpsConnector};
use hyper_util::{
    client::legacy::{Client, connect::HttpConnector},
    rt::TokioExecutor,
};

type Http1 = Client<ProxyConnector<HttpsConnector<HttpConnector>>, Full<Bytes>>;

// Same native-root helper, safe protocol versions and provider as the pinned
// proxy connector (process default if present, otherwise local ring). No global
// provider installation, environment proxy discovery or trust override.
fn native_tls() -> Result<rustls::ClientConfig, Terminal> {
    let provider = rustls::crypto::CryptoProvider::get_default()
        .cloned()
        .unwrap_or_else(|| Arc::new(rustls::crypto::ring::default_provider()));
    let config = rustls::ClientConfig::builder_with_provider(provider)
        .with_safe_default_protocol_versions()
        .map_err(|_| Terminal::Transport(TransportStage::PrepareHttp))?
        .with_native_roots()
        .map_err(|_| Terminal::Transport(TransportStage::PrepareHttp))?
        .with_no_client_auth();
    Ok(config)
}

fn client(mut tls: rustls::ClientConfig, proxy: Option<&reqwest::Url>) -> Result<Http1, Terminal> {
    // enable_http1().build() leaves ALPN empty. Use the public constructor to
    // advertise exactly H1, including the TLS connection after proxy CONNECT.
    tls.alpn_protocols = vec![b"http/1.1".to_vec()];
    let tls = Arc::new(tls);
    let mut tcp = HttpConnector::new();
    tcp.enforce_http(false); // Same as HttpsConnectorBuilder's inner connector.
    let https = HttpsConnector::new(
        tcp,
        Arc::clone(&tls),
        false,
        Arc::new(DefaultServerNameResolver::default()),
    );
    let proxy = match proxy {
        Some(url) => Proxy::new(
            Intercept::All,
            url.as_str()
                .parse()
                .map_err(|_| Terminal::Transport(TransportStage::PrepareHttp))?,
        ),
        None => Proxy::new(Intercept::None, http::Uri::from_static("0.0.0.0")),
    };
    // Avoid the proxy constructor's second native-root load. This public path
    // applies the same configuration to direct TLS and TLS after CONNECT.
    let mut connector = ProxyConnector::from_proxy_unsecured(https, proxy);
    connector.set_tls(Some(tokio_rustls::TlsConnector::from(tls)));
    Ok(Client::builder(TokioExecutor::new())
        .http2_adaptive_window(true)
        .retry_canceled_requests(false)
        .pool_max_idle_per_host(0)
        .build(connector))
}

fn user_agent() -> http::HeaderValue {
    use librespot_core::{config::OS, version};
    // Spotifast's supported desktop platforms, exactly HttpClient::new's
    // platform mapping and public version constants. No other-client identity.
    let platform = match OS {
        "windows" => "Win32",
        "macos" => "OSX",
        _ => "Linux",
    };
    http::HeaderValue::from_str(&format!(
        "Spotify/{} {platform}/0 ({})",
        version::spotify_version(),
        version::VERSION_STRING
    ))
    .unwrap_or_else(|_| http::HeaderValue::from_static(version::FALLBACK_USER_AGENT))
}

// Called only after lease.dispatch. Auth and URI are still obtained through the
// existing Session path. The explicitly approved exception omits only Session's
// private local domain limiter, not the Bridge allowance or server limits.
pub(super) async fn send(
    request: http::Request<Vec<u8>>,
    proxy: Option<&reqwest::Url>,
    check: Permit<'_>,
) -> Result<Reply, Terminal> {
    send_with(request, check, || client(native_tls()?, proxy)).await
}

async fn send_with(
    mut request: http::Request<Vec<u8>>,
    check: Permit<'_>,
    setup: impl FnOnce() -> Result<Http1, Terminal>,
) -> Result<Reply, Terminal> {
    check()?;
    if request.version() != http::Version::HTTP_11 {
        return Err(Terminal::PreconditionRejected);
    }
    let client = setup();
    // Native roots/setup can take time. Recheck before constructing or polling
    // the sole HTTP future; dropping here must not open a socket.
    check()?;
    let client = client?;
    request
        .headers_mut()
        .insert(http::header::USER_AGENT, user_agent());
    http_reply(
        || Ok::<_, Terminal>(client.request(request.map(|body| Full::new(Bytes::from(body))))),
        check,
        |error| failure::project(&error),
    )
    .await
    // No cached client, tokens, redirects, retry loop or next transport.
}

#[cfg(test)]
mod tests;
