use super::{
    auth::{token, CHECK_AUTH, EXP},
    error::AppError,
    ext::RequestExt,
    ConfigExt,
};
use crate::{constant, InstallConfig, Running, ServeConfig};
use anyhow::Context;
use axum::{
    body::{Body, StreamBody},
    extract::State,
    http::{header, HeaderName, HeaderValue, StatusCode},
    response::{Html, IntoResponse, Redirect, Response},
    routing::{any, get, post},
    Form, Json, Router,
};
use axum_server::{tls_rustls::RustlsConfig, AddrIncomingConfig, Handle, HttpConfig};
use serde::Deserialize;
use std::{
    io::{BufRead, Read},
    process::Stdio,
    str::FromStr,
    sync::Arc,
    time::Duration,
};
use tokio::io::BufReader;
use tokio_util::io::ReaderStream;
use tower_http::trace;
use tracing::Level;

// Access cookie
const ACCESS_COOKIE: &'static str = "access_token";
// Login html
const LOGIN_HTML: &str = include_str!("../static/login.html");

#[derive(Deserialize)]
struct User {
    password: String,
}

pub(super) struct FrontendServer(ServeConfig, InstallConfig, tokio::sync::mpsc::Receiver<()>);

impl Running for FrontendServer {
    fn run(self) -> anyhow::Result<()> {
        self.start_server()
    }
}

impl FrontendServer {
    pub(super) fn new(
        serve_config: ServeConfig,
        install_config: InstallConfig,
        graceful_shutdown: tokio::sync::mpsc::Receiver<()>,
    ) -> Self {
        Self(serve_config, install_config, graceful_shutdown)
    }

    #[tokio::main]
    async fn start_server(self) -> anyhow::Result<()> {
        log::info!("Starting frontend server: {}", self.0.bind);

        // Set check auth
        CHECK_AUTH.set(self.0.auth_password.clone())?;

        // router
        let router = Router::new()
            .route("/webman/login.cgi", get(get_webman_login))
            .route("/", any(get_pan_thunder_com))
            .route("/*path", any(get_pan_thunder_com))
            // Need to auth middleware
            .route_layer(axum::middleware::from_fn(auth_middleware))
            .route("/login", get(get_login))
            .route("/login", post(post_login))
            .layer(
                tower_http::trace::TraceLayer::new_for_http()
                    .make_span_with(trace::DefaultMakeSpan::new().level(Level::INFO))
                    .on_response(trace::DefaultOnResponse::new().level(Level::INFO))
                    .on_request(trace::DefaultOnRequest::new().level(Level::INFO))
                    .on_failure(trace::DefaultOnFailure::new().level(Level::WARN)),
            )
            .with_state(Arc::new((self.0.clone(), self.1.clone())));

        // http server config
        let http_config = HttpConfig::new()
            .http1_title_case_headers(true)
            .http1_preserve_header_case(true)
            .http2_keep_alive_interval(Duration::from_secs(60))
            .build();

        // http server incoming config
        let incoming_config = AddrIncomingConfig::new()
            .tcp_sleep_on_accept_errors(true)
            .tcp_keepalive(Some(Duration::from_secs(60)))
            .build();

        // Signal the server to shutdown using Handle.
        let handle = Handle::new();

        // Wait for the server to shutdown gracefully
        tokio::spawn(graceful_shutdown_signal(handle.clone(), self.2));

        // If tls_cert and tls_key is not None, use https
        let result = match (self.0.tls_cert, self.0.tls_key) {
            (Some(cert), Some(key)) => {
                // Load tls config
                let tls_config = RustlsConfig::from_pem_file(cert, key).await?;

                axum_server::bind_rustls(self.0.bind, tls_config)
                    .handle(handle)
                    .addr_incoming_config(incoming_config)
                    .http_config(http_config)
                    .serve(router.into_make_service())
                    .await
            }
            _ => {
                axum_server::bind(self.0.bind)
                    .handle(handle)
                    .addr_incoming_config(incoming_config)
                    .http_config(http_config)
                    .serve(router.into_make_service())
                    .await
            }
        };

        if let Some(err) = result.err() {
            log::warn!("Http Server error: {}", err);
        }

        Ok(())
    }
}

/// Authentication
fn authentication(auth_password: &str) -> bool {
    match CHECK_AUTH.get() {
        Some(Some(p)) => auth_password.eq(p),
        _ => true,
    }
}

/// GET /login handler
async fn get_login() -> Html<&'static str> {
    Html(LOGIN_HTML)
}

/// POST Login handler
async fn post_login(user: Form<User>) -> Result<impl IntoResponse, Redirect> {
    if authentication(user.password.as_str()) {
        if let Ok(token) = token::generate_token() {
            let resp = Response::builder()
                .header(header::LOCATION, constant::SYNOPKG_WEB_UI_HOME)
                .header(
                    header::SET_COOKIE,
                    format!("{ACCESS_COOKIE}={token}; Max-Age={EXP}; Path=/; HttpOnly"),
                )
                .status(StatusCode::SEE_OTHER)
                .body(Body::empty())
                .expect("Failed to build response");
            return Ok(resp.into_response());
        }
    }

    Err(Redirect::to("/login"))
}

/// GET "/webman/login.cgi" handler
async fn get_webman_login() -> Json<&'static str> {
    Json(r#"{"SynoToken", ""}"#)
}

/// Any "/webman/3rdparty/pan-thunder-com/index.cgi/" handler
async fn get_pan_thunder_com(
    State(conf): State<Arc<(ServeConfig, InstallConfig)>>,
    req: RequestExt,
) -> Result<impl IntoResponse, AppError> {
    if !req.uri.to_string().contains(constant::SYNOPKG_WEB_UI_HOME) {
        return Ok(Redirect::temporary(constant::SYNOPKG_WEB_UI_HOME).into_response());
    }

    // environment variables
    let envs = (&conf.0, &conf.1).envs()?;

    // My Server real host
    let remove_host = extract_real_host(&req);

    let mut cmd = tokio::process::Command::new(constant::SYNOPKG_CLI_WEB);
    cmd.current_dir(constant::SYNOPKG_PKGDEST)
        .envs(envs)
        .env("SERVER_SOFTWARE", "rust")
        .env("SERVER_PROTOCOL", "HTTP/1.1")
        .env("HTTP_HOST", remove_host)
        .env("GATEWAY_INTERFACE", "CGI/1.1")
        .env("REQUEST_METHOD", req.method.as_str())
        .env("QUERY_STRING", req.uri.query().unwrap_or_default())
        .env(
            "REQUEST_URI",
            req.uri
                .path_and_query()
                .context("Failed to get path_and_query")?
                .as_str(),
        )
        .env("PATH_INFO", req.uri.path())
        .env("SCRIPT_NAME", ".")
        .env("SCRIPT_FILENAME", req.uri.path())
        .env("SERVER_PORT", conf.0.bind.port().to_string())
        .env("REMOTE_ADDR", remove_host)
        .env("SERVER_NAME", remove_host)
        .uid(conf.1.uid)
        .gid(conf.1.gid)
        .stdout(Stdio::piped())
        .stdin(Stdio::piped());

    // If debug is false, hide stderr
    if !conf.0.debug {
        cmd.stderr(Stdio::null());
    }

    for ele in req.headers.iter() {
        let k = ele.0.as_str().to_ascii_lowercase();
        let v = ele.1;
        if k == "PROXY" {
            continue;
        }
        if !v.is_empty() {
            cmd.env(format!("HTTP_{k}"), v.to_str().unwrap_or_default());
        }
    }

    req.headers.get(header::CONTENT_TYPE).map(|h| {
        cmd.env("CONTENT_TYPE", h.to_str().unwrap_or_default());
    });

    req.headers.get(header::CONTENT_LENGTH).map(|h| {
        cmd.env("CONTENT_LENGTH", h.to_str().unwrap_or_default());
    });

    let mut child = cmd.spawn()?;

    if let Some(body) = req.body {
        if let Some(w) = child.stdin.as_mut() {
            let mut r = BufReader::new(&body[..]);
            tokio::io::copy(&mut r, w).await?;
        }
    }

    // Wait for the child to exit
    let output = child.wait_with_output().await?;

    // Get status code
    let mut status_code = 200;

    // Response builder
    let mut builder = Response::builder();

    // Extract headers
    let mut cursor = std::io::Cursor::new(output.stdout);
    for header_res in cursor.by_ref().lines() {
        let header = header_res?;
        if header.is_empty() {
            break;
        }
        if header.starts_with("getEnvs ") {
            continue;
        }

        let (header, val) = header
            .split_once(':')
            .context("Failed to split_once header")?;
        let val = &val[1..];

        if header == "Status" {
            status_code = val[0..3]
                .parse()
                .context("Status returned by CGI program is invalid")?;
        } else {
            builder = builder.header(HeaderName::from_str(header)?, HeaderValue::from_str(val)?);
        }
    }

    Ok(builder
        .status(status_code)
        .body(StreamBody::from(ReaderStream::new(cursor)))?
        .into_response())
}

/// Extract real request host (bind, port)
fn extract_real_host(req: &RequestExt) -> &str {
    req.headers
        .get(header::HOST)
        .map(|h| h.to_str().unwrap_or_default())
        .unwrap_or_default()
}

use axum::{http::Request, middleware::Next};

/// Auth middleware
pub(crate) async fn auth_middleware<B>(
    request: Request<B>,
    next: Next<B>,
) -> Result<Response, Redirect> {
    // If CHECK_AUTH is None, return true
    if let Some(None) = CHECK_AUTH.get() {
        return Ok(next.run(request).await);
    }

    // extract access_token from cookie
    if let Some(h) = request.headers().get(header::COOKIE) {
        let cookie = h.to_str().unwrap_or_default();
        let cookie = cookie
            .split(';')
            .filter(|c| !c.is_empty())
            .collect::<Vec<&str>>();
        for c in cookie {
            let c = c.trim();
            if c.starts_with(ACCESS_COOKIE) {
                let token = c.split('=').collect::<Vec<&str>>();
                if token.len() == 2 {
                    // Verify token
                    if token::verifier(token[1]).is_ok() {
                        return Ok(next.run(request).await);
                    }
                }
            }
        }
    }

    Err(Redirect::to("/login"))
}

/// Graceful shutdown signal
async fn graceful_shutdown_signal(
    handle: Handle,
    mut graceful_shutdown: tokio::sync::mpsc::Receiver<()>,
) {
    tokio::select! {
        _ = graceful_shutdown.recv() => {
            println!("Received signal to shutdown");
            handle.shutdown();
            return ;
        }
    }
}
