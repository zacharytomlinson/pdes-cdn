use std::{collections::HashMap, net::SocketAddr, path::PathBuf, time::Duration as StdDuration};

use anyhow::Context;
use argon2::{
    Argon2,
    password_hash::{PasswordHash, PasswordHasher, PasswordVerifier, SaltString},
};
use axum::{
    Form, Router,
    body::Body,
    extract::{DefaultBodyLimit, FromRef, Multipart, Path, Query, State},
    http::{HeaderMap, HeaderValue, StatusCode, header},
    response::{Html, IntoResponse, Redirect, Response},
    routing::{get, post},
};
use axum_extra::extract::SignedCookieJar;
use axum_extra::extract::cookie::{Cookie, Key};
use chrono::{DateTime, Duration as ChronoDuration, Utc};
use dotenvy::dotenv;
use lettre::{
    AsyncSmtpTransport, AsyncTransport, Tokio1Executor,
    message::{Mailbox, Message, MultiPart},
    transport::smtp::authentication::Credentials,
};
use moka::future::Cache;
use rand::{Rng, RngCore, distributions::Alphanumeric, rngs::OsRng};
use serde::Deserialize;
use sha2::{Digest, Sha256, Sha512};
use sqlx::{FromRow, PgPool, postgres::PgPoolOptions};
use tera::{Context as TeraContext, Tera};
use tokio::fs::File;
use tokio::io::AsyncWriteExt;
use tokio_util::io::ReaderStream;
use tower_http::cors::{Any, CorsLayer};
use tower_http::trace::TraceLayer;
use tracing::{error, info, warn};
use uuid::Uuid;

const SESSION_COOKIE: &str = "pdes_session";

#[derive(Clone)]
struct AppState {
    pool: PgPool,
    templates: Tera,
    cache_dir: PathBuf,
    cookie_key: Key,
    file_by_id_cache: Cache<Uuid, CachedFile>,
}

impl FromRef<AppState> for Key {
    fn from_ref(state: &AppState) -> Self {
        state.cookie_key.clone()
    }
}

#[derive(Debug)]
struct AppError {
    status: StatusCode,
    message: String,
    format: AppErrorFormat,
}

#[derive(Debug, Clone, Copy)]
enum AppErrorFormat {
    Html,
    Plain,
}

impl AppError {
    fn new(status: StatusCode, message: impl Into<String>) -> Self {
        Self {
            status,
            message: message.into(),
            format: AppErrorFormat::Html,
        }
    }

    fn new_plain(status: StatusCode, message: impl Into<String>) -> Self {
        Self {
            status,
            message: message.into(),
            format: AppErrorFormat::Plain,
        }
    }
}

impl<E> From<E> for AppError
where
    E: Into<anyhow::Error>,
{
    fn from(err: E) -> Self {
        Self {
            status: StatusCode::INTERNAL_SERVER_ERROR,
            message: err.into().to_string(),
            format: AppErrorFormat::Html,
        }
    }
}

impl IntoResponse for AppError {
    fn into_response(self) -> Response {
        if self.status.is_server_error() {
            error!(status = %self.status, message = %self.message, "request failed");
        } else if self.status != StatusCode::NOT_FOUND {
            warn!(status = %self.status, message = %self.message, "request rejected");
        }
        match self.format {
            AppErrorFormat::Plain => (self.status, self.message).into_response(),
            AppErrorFormat::Html => {
                let status_code = self.status.as_u16();
                let status_text = self
                    .status
                    .canonical_reason()
                    .unwrap_or("Error")
                    .to_string();
                let message = escape_html(&self.message);
                let html = format!(
                    r#"<!doctype html>
<html lang="en">
  <head>
    <meta charset="utf-8">
    <meta name="viewport" content="width=device-width, initial-scale=1">
    <title>{status_code} {status_text}</title>
    <style>
      :root {{
        --panel: #111827;
        --border: #1f2937;
        --soft: #cbd5e1;
        --accent: #0ea5e9;
      }}
      * {{ box-sizing: border-box; }}
      body {{
        margin: 0;
        font-family: "Segoe UI", system-ui, -apple-system, sans-serif;
        background: radial-gradient(circle at 20% 20%, rgba(14,165,233,0.12), transparent 30%),
                    radial-gradient(circle at 80% 0%, rgba(6,182,212,0.12), transparent 25%),
                    #0b1222;
        color: #e2e8f0;
        min-height: 100vh;
        padding: 28px 16px 48px;
      }}
      a {{ color: var(--accent); text-decoration: none; }}
      .wrap {{ max-width: 880px; margin: 0 auto; }}
      .card {{
        background: var(--panel);
        border: 1px solid var(--border);
        border-radius: 16px;
        padding: 22px;
        box-shadow: 0 15px 60px rgba(0,0,0,0.35);
      }}
      .muted {{ color: var(--soft); }}
      .actions {{ display: flex; gap: 10px; flex-wrap: wrap; margin-top: 14px; }}
      .btn {{
        display: inline-flex;
        align-items: center;
        justify-content: center;
        gap: 8px;
        padding: 10px 14px;
        border-radius: 12px;
        border: 1px solid var(--border);
        background: transparent;
        color: #e2e8f0;
        cursor: pointer;
      }}
      .btn.primary {{
        border: none;
        background: linear-gradient(120deg, #0ea5e9, #06b6d4);
        color: #0b1222;
        font-weight: 700;
      }}
      code {{
        display: inline-block;
        max-width: 100%;
        white-space: pre-wrap;
        overflow-wrap: anywhere;
        background: rgba(2,6,23,0.35);
        border: 1px solid rgba(31,41,55,0.8);
        padding: 6px 8px;
        border-radius: 10px;
      }}
    </style>
  </head>
  <body>
    <div class="wrap">
      <div class="card">
        <h1 style="margin:0 0 6px 0;">{status_code} {status_text}</h1>
        <p class="muted" style="margin:0 0 14px 0;">Something went wrong.</p>
        <div><code>{message}</code></div>
        <div class="actions">
          <button class="btn primary" type="button" onclick="history.back()">Go back</button>
          <a class="btn" href="/portal">Portal</a>
          <a class="btn" href="/login">Login</a>
        </div>
      </div>
    </div>
  </body>
</html>"#
                );
                (self.status, Html(html)).into_response()
            }
        }
    }
}

type AppResult<T> = Result<T, AppError>;

#[derive(Debug, Clone, FromRow)]
struct User {
    id: Uuid,
    email: String,
    password_hash: String,
    permission: String,
    reset_token: Option<String>,
    reset_token_expires: Option<DateTime<Utc>>,
    must_reset: bool,
}

#[derive(Debug, Clone, FromRow, serde::Serialize)]
struct CachedFile {
    id: Uuid,
    filename: String,
    local_path: String,
    size_bytes: Option<i64>,
    created_at: DateTime<Utc>,
}

#[derive(Debug, Deserialize)]
struct LoginForm {
    email: String,
    password: String,
}

#[derive(Debug, Deserialize)]
struct ResetRequestForm {
    email: String,
}

#[derive(Debug, Deserialize)]
struct ResetConfirmForm {
    password: String,
    confirm_password: String,
}

#[derive(Debug, Deserialize)]
struct InviteUserForm {
    email: String,
}

#[derive(Debug, Deserialize)]
struct CreateAppUploadTokenForm {
    app_slug: String,
    name: String,
    expires_days: Option<i64>,
}

#[derive(Debug, Clone, FromRow, serde::Serialize)]
struct UserBrief {
    id: Uuid,
    email: String,
    permission: String,
}

#[derive(Debug, Clone, serde::Serialize)]
struct AssignedUserView {
    id: Uuid,
    email: String,
}

#[derive(Debug, Clone, FromRow, serde::Serialize)]
struct ApiTokenRow {
    id: Uuid,
    user_id: Uuid,
    name: String,
    created_at: DateTime<Utc>,
    last_used_at: Option<DateTime<Utc>>,
}

#[derive(Debug, Clone, FromRow, serde::Serialize)]
struct AppOptionRow {
    slug: String,
    name: String,
}

#[derive(Debug, Clone, FromRow, serde::Serialize)]
struct AppUploadTokenRow {
    id: Uuid,
    app_slug: String,
    name: String,
    created_at: DateTime<Utc>,
    last_used_at: Option<DateTime<Utc>>,
    revoked_at: Option<DateTime<Utc>>,
    expires_at: Option<DateTime<Utc>>,
}

#[derive(Debug, Clone, FromRow)]
struct ApiUserAuthRow {
    token_id: Uuid,
    id: Uuid,
    email: String,
    password_hash: String,
    permission: String,
    reset_token: Option<String>,
    reset_token_expires: Option<DateTime<Utc>>,
    must_reset: bool,
}

#[derive(Debug, Clone, FromRow)]
struct DeviceTokenAuthRow {
    token_id: Uuid,
    app_id: Uuid,
    os: String,
    arch: String,
}

#[derive(Debug, Clone)]
struct DevicePrincipal {
    app_id: Uuid,
    os: String,
    arch: String,
}

#[derive(Debug, Clone)]
enum ApiPrincipal {
    User(User),
    Device(DevicePrincipal),
}

#[derive(Debug, Clone, FromRow, serde::Serialize)]
struct AppManageView {
    slug: String,
    name: String,
    description: Option<String>,
    icon_file_id: Option<Uuid>,
    latest_pkg_version: Option<String>,
    latest_pkg_os: Option<String>,
    latest_pkg_arch: Option<String>,
    latest_fw_version: Option<String>,
    latest_fw_channel: Option<String>,
    assignment_count: i64,
    #[sqlx(skip)]
    #[serde(default)]
    icon_url: Option<String>,
    #[sqlx(skip)]
    #[serde(default)]
    icon_text: String,
    #[sqlx(skip)]
    #[serde(default)]
    assigned_users: Vec<AssignedUserView>,
    #[sqlx(skip)]
    #[serde(default)]
    targets: Vec<AppTargetView>,
    #[sqlx(skip)]
    #[serde(default)]
    packages: Vec<AppPackageView>,
    #[sqlx(skip)]
    #[serde(default)]
    firmware: Vec<AppFirmwareView>,
}

#[derive(Debug, Clone, serde::Serialize)]
struct AppTargetView {
    os: String,
    arch: String,
    version: String,
}

#[derive(Debug, Clone, serde::Serialize)]
struct AppPackageView {
    id: Uuid,
    os: String,
    arch: String,
    version: String,
    created_at: DateTime<Utc>,
}

#[derive(Debug, Clone, serde::Serialize)]
struct AppFirmwareView {
    id: Uuid,
    channel: String,
    version: String,
    created_at: DateTime<Utc>,
}

#[derive(Debug, Clone, Deserialize)]
struct PkgManifestInstaller {
    path: String,
}

#[derive(Debug, Clone, Deserialize)]
struct PkgManifestIcon {
    path: String,
}

#[derive(Debug, Clone, Deserialize)]
struct PkgManifest {
    installer: PkgManifestInstaller,
    #[serde(default)]
    icon: Option<PkgManifestIcon>,
}

#[derive(Debug, Clone, serde::Serialize)]
struct AppCardView {
    slug: String,
    name: String,
    description: Option<String>,
    icon_url: Option<String>,
    latest_version: Option<String>,
    can_download: bool,
    icon_text: String,
    targets: Vec<AppTargetView>,
}

#[tokio::main]
async fn main() -> AppResult<()> {
    dotenv().ok();
    let args: Vec<String> = std::env::args().collect();
    if args.get(1).map(|s| s.as_str()) == Some("--hash-password") {
        let Some(password) = args.get(2) else {
            eprintln!("Usage: pdes-cdn --hash-password <password>");
            std::process::exit(2);
        };
        let salt = SaltString::generate(&mut OsRng);
        let password_hash = Argon2::default()
            .hash_password(password.as_bytes(), &salt)
            .map_err(|_| AppError::new(StatusCode::BAD_REQUEST, "Could not hash password"))?
            .to_string();
        println!("{password_hash}");
        return Ok(());
    }

    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| "info,tower_http=info".into()),
        )
        .with_target(false)
        .compact()
        .init();

    let database_url = std::env::var("DATABASE_URL")
        .unwrap_or_else(|_| "postgres://postgres:postgres@localhost:5432/pdes_cdn".to_string());
    let cache_dir = std::env::var("CACHE_DIR").unwrap_or_else(|_| "cache".to_string());
    let skip_cache_dir_write_check = std::env::var("CACHE_DIR_SKIP_WRITE_CHECK")
        .ok()
        .map(|v| matches!(v.trim().to_ascii_lowercase().as_str(), "1" | "true" | "yes"))
        .unwrap_or(false);
    let max_upload_bytes: usize = std::env::var("MAX_UPLOAD_BYTES")
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(200 * 1024 * 1024);
    let cookie_secret_env = std::env::var("COOKIE_SECRET").ok().unwrap_or_default();
    let cookie_secret = if cookie_secret_env.trim().is_empty() {
        let generated: String = rand::thread_rng()
            .sample_iter(&Alphanumeric)
            .take(64)
            .map(char::from)
            .collect();
        warn!(
            "COOKIE_SECRET not set; using an ephemeral random secret (sessions will invalidate on restart)"
        );
        generated
    } else {
        cookie_secret_env
    };

    let templates = Tera::new("templates/**/*")
        .context("loading templates")
        .map_err(|err| {
            error!(error = %err, error_debug = ?err, "Failed to load templates");
            AppError::new(StatusCode::INTERNAL_SERVER_ERROR, "loading templates")
        })?;
    let pool = PgPoolOptions::new()
        .max_connections(5)
        .connect(&database_url)
        .await
        .context("connecting to database")?;
    let cache_dir_path = PathBuf::from(&cache_dir);
    tokio::fs::create_dir_all(&cache_dir_path)
        .await
        .context("creating cache directory")?;
    let cache_dir = tokio::fs::canonicalize(&cache_dir_path)
        .await
        .unwrap_or(cache_dir_path);
    if skip_cache_dir_write_check {
        warn!(cache_dir = %cache_dir.display(), "CACHE_DIR write check disabled; uploads may fail if not writable");
    } else {
        ensure_writable_cache_root(&cache_dir).await?;
    }

    let mut hasher = Sha512::new();
    hasher.update(cookie_secret.as_bytes());
    let digest = hasher.finalize();
    let mut key_bytes = [0u8; 64];
    key_bytes.copy_from_slice(&digest[..]);
    info!(
        "Starting pdes-cdn with cache_dir={} max_upload_bytes={}",
        cache_dir.display(),
        max_upload_bytes
    );

    let state = AppState {
        pool,
        templates,
        cache_dir,
        cookie_key: Key::from(&key_bytes),
        file_by_id_cache: Cache::builder()
            .max_capacity(512)
            .time_to_live(StdDuration::from_secs(60))
            .build(),
    };

    let app = Router::new()
        .route("/", get(root))
        .route("/favicon.ico", get(favicon_ico))
        .route("/favicon-16x16.png", get(favicon_16_png))
        .route("/favicon-32x32.png", get(favicon_32_png))
        .route("/apple-touch-icon.png", get(apple_touch_icon_png))
        .route("/android-chrome-192x192.png", get(android_192_png))
        .route("/android-chrome-512x512.png", get(android_512_png))
        .route("/site.webmanifest", get(site_webmanifest))
        .route("/login", get(show_login).post(do_login))
        .route("/logout", get(do_logout))
        .route("/reset", get(show_reset_request).post(do_reset_request))
        .route(
            "/reset/confirm/:token",
            get(show_reset_confirm).post(do_reset_confirm),
        )
        .route("/portal", get(portal))
        .route("/apps", get(list_assigned_apps))
        .route("/apps/manage", get(apps_manage))
        .route("/apps/create", post(create_app))
        .route("/apps/assign", post(assign_app_generic))
        .route("/apps/:slug/assign", post(assign_app))
        .route("/apps/:slug/unassign", post(unassign_app))
        .route("/apps/:slug/update", post(update_app))
        .route("/apps/:slug/delete", post(delete_app))
        .route("/apps/:slug/download/bootstrap", get(download_bootstrap))
        .route("/apps/:slug/download", get(download_latest_pkg))
        .route("/apps/:slug/installer/download", get(download_installer))
        .route("/apps/:slug/icon", get(app_icon))
        .route("/apps/:slug/firmware/latest", get(latest_firmware_json))
        .route(
            "/apps/:slug/firmware/latest/download",
            get(download_latest_firmware),
        )
        .route("/apps/:slug/package/upload", post(upload_app_package))
        .route("/apps/:slug/package/build", post(build_app_package))
        .route(
            "/apps/:slug/package/:package_id/delete",
            post(delete_app_package),
        )
        .route("/apps/:slug/firmware/upload", post(upload_firmware))
        .route(
            "/apps/:slug/firmware/:firmware_id/delete",
            post(delete_firmware_release),
        )
        .route("/tokens", get(list_tokens).post(create_token))
        .route("/tokens/:id/revoke", post(revoke_token))
        .route("/tokens/app-upload", post(create_app_upload_token))
        .route(
            "/tokens/app-upload/:id/revoke",
            post(revoke_app_upload_token),
        )
        .route("/invite", axum::routing::post(invite_user))
        .route("/api/apps/:slug/package/latest", get(api_latest_package))
        .route(
            "/api/apps/:slug/package/upload",
            post(api_upload_app_package),
        )
        .route(
            "/api/apps/:slug/package/download",
            get(api_download_package),
        )
        .route("/api/apps/:slug/firmware/latest", get(api_latest_firmware))
        .route(
            "/api/apps/:slug/firmware/download",
            get(api_download_firmware),
        )
        .route("/api/bootstrap/mint", post(api_mint_bootstrap_token))
        .route(
            "/bootstrap/:token/exchange",
            post(bootstrap_exchange_device_token),
        )
        .route("/bootstrap/:token/pkg", get(bootstrap_download_pkg))
        .layer(DefaultBodyLimit::max(max_upload_bytes))
        .layer(TraceLayer::new_for_http())
        .layer(
            CorsLayer::new()
                .allow_origin(Any)
                .allow_methods([axum::http::Method::GET, axum::http::Method::POST])
                .allow_headers(Any)
                .expose_headers([header::CONTENT_DISPOSITION]),
        )
        .with_state(state);

    let addr = SocketAddr::from(([0, 0, 0, 0], 3000));
    info!("Listening on http://{}", addr);
    let listener = tokio::net::TcpListener::bind(addr)
        .await
        .map_err(AppError::from)?;
    axum::serve(listener, app).await.map_err(AppError::from)?;

    Ok(())
}

async fn favicon_ico() -> impl IntoResponse {
    const BYTES: &[u8] = include_bytes!("../assets/favicon.ico");
    (
        [
            (header::CONTENT_TYPE, "image/x-icon"),
            (header::CACHE_CONTROL, "public, max-age=86400"),
        ],
        BYTES,
    )
}

async fn favicon_16_png() -> impl IntoResponse {
    const BYTES: &[u8] = include_bytes!("../assets/favicon-16x16.png");
    (
        [
            (header::CONTENT_TYPE, "image/png"),
            (header::CACHE_CONTROL, "public, max-age=86400"),
        ],
        BYTES,
    )
}

async fn favicon_32_png() -> impl IntoResponse {
    const BYTES: &[u8] = include_bytes!("../assets/favicon-32x32.png");
    (
        [
            (header::CONTENT_TYPE, "image/png"),
            (header::CACHE_CONTROL, "public, max-age=86400"),
        ],
        BYTES,
    )
}

async fn apple_touch_icon_png() -> impl IntoResponse {
    const BYTES: &[u8] = include_bytes!("../assets/apple-touch-icon.png");
    (
        [
            (header::CONTENT_TYPE, "image/png"),
            (header::CACHE_CONTROL, "public, max-age=86400"),
        ],
        BYTES,
    )
}

async fn android_192_png() -> impl IntoResponse {
    const BYTES: &[u8] = include_bytes!("../assets/android-chrome-192x192.png");
    (
        [
            (header::CONTENT_TYPE, "image/png"),
            (header::CACHE_CONTROL, "public, max-age=86400"),
        ],
        BYTES,
    )
}

async fn android_512_png() -> impl IntoResponse {
    const BYTES: &[u8] = include_bytes!("../assets/android-chrome-512x512.png");
    (
        [
            (header::CONTENT_TYPE, "image/png"),
            (header::CACHE_CONTROL, "public, max-age=86400"),
        ],
        BYTES,
    )
}

async fn site_webmanifest() -> impl IntoResponse {
    const BYTES: &[u8] = include_bytes!("../assets/site.webmanifest");
    (
        [
            (header::CONTENT_TYPE, "application/manifest+json"),
            (header::CACHE_CONTROL, "public, max-age=86400"),
        ],
        BYTES,
    )
}

async fn root(State(state): State<AppState>, jar: SignedCookieJar) -> AppResult<impl IntoResponse> {
    if current_user(&state, &jar).await?.is_some() {
        Ok(Redirect::to("/portal"))
    } else {
        Ok(Redirect::to("/login"))
    }
}

async fn show_login(State(state): State<AppState>) -> AppResult<impl IntoResponse> {
    render_template(&state, "login.html", TeraContext::new())
}

async fn do_login(
    State(state): State<AppState>,
    jar: SignedCookieJar,
    Form(form): Form<LoginForm>,
) -> AppResult<Response> {
    info!(email = %form.email, "login attempt");
    let user = sqlx::query_as::<_, User>(
        "select id, email, password_hash, permission, reset_token, reset_token_expires, must_reset from users where email = $1",
    )
    .bind(&form.email)
    .fetch_optional(&state.pool)
    .await?;

    let Some(user) = user else {
        warn!(email = %form.email, "login failed: user not found");
        return render_login_error(&state, "Invalid credentials").map(|html| html.into_response());
    };

    let parsed_hash = PasswordHash::new(&user.password_hash)
        .map_err(|_| AppError::new(StatusCode::BAD_REQUEST, "Stored password hash is invalid"))?;
    if Argon2::default()
        .verify_password(form.password.as_bytes(), &parsed_hash)
        .is_err()
    {
        warn!(email = %form.email, "login failed: invalid password");
        return render_login_error(&state, "Invalid credentials").map(|html| html.into_response());
    }

    if user.must_reset {
        let existing = match (&user.reset_token, user.reset_token_expires) {
            (Some(token), Some(expires)) if expires > Utc::now() => Some(token.clone()),
            _ => None,
        };
        let token = match existing {
            Some(t) => t,
            None => issue_reset_token(&state, user.id).await?,
        };
        info!(user_id = %user.id, email = %user.email, "login requires password reset");
        return Ok(Redirect::to(&format!("/reset/confirm/{}", token)).into_response());
    }

    let cookie = Cookie::build((SESSION_COOKIE, user.id.to_string()))
        .path("/")
        .http_only(true)
        .build();
    let jar = jar.add(cookie);
    info!(user_id = %user.id, email = %user.email, "login succeeded");
    Ok((jar, Redirect::to("/portal")).into_response())
}

async fn do_logout(jar: SignedCookieJar) -> AppResult<Response> {
    let jar = jar.remove(Cookie::from(SESSION_COOKIE));
    Ok((jar, Redirect::to("/login")).into_response())
}

async fn show_reset_request(State(state): State<AppState>) -> AppResult<impl IntoResponse> {
    render_template(&state, "reset_request.html", TeraContext::new())
}

async fn do_reset_request(
    State(state): State<AppState>,
    Form(form): Form<ResetRequestForm>,
) -> AppResult<impl IntoResponse> {
    info!(email = %form.email, "password reset requested");
    let token: String = rand::thread_rng()
        .sample_iter(&Alphanumeric)
        .take(32)
        .map(char::from)
        .collect();
    let expires = Utc::now() + ChronoDuration::minutes(30);

    let _ =
        sqlx::query("update users set reset_token = $1, reset_token_expires = $2 where email = $3")
            .bind(&token)
            .bind(expires)
            .bind(&form.email)
            .execute(&state.pool)
            .await?;

    let base = base_url();
    let reset_link = format!("{}/reset/confirm/{}", base.trim_end_matches('/'), token);
    let subject = "CDN portal password reset";
    let body_text = format!(
        "We received a password reset request for this address.\n\nReset link (valid 30 minutes): {link}\nIf you did not request this, you can ignore this email.",
        link = reset_link
    );
    let body_html = render_email_template(&state, "email/password_reset.html", {
        let mut ctx = TeraContext::new();
        ctx.insert("reset_link", &reset_link);
        ctx
    })?;
    let _ = send_email(&form.email, subject, &body_text, Some(body_html)).await?;
    let token_preview: String = token.chars().take(8).collect();
    info!(email = %form.email, token_prefix = %token_preview, "issued reset token and sent reset email");

    let mut ctx = TeraContext::new();
    ctx.insert(
        "message",
        "If the account exists, a reset link was prepared.",
    );
    render_template(&state, "reset_request.html", ctx)
}

async fn show_reset_confirm(
    Path(token): Path<String>,
    State(state): State<AppState>,
) -> AppResult<Response> {
    match ensure_valid_token(&state, &token).await {
        Ok(_) => {
            let mut ctx = TeraContext::new();
            ctx.insert("token", &token);
            let html = render_template(&state, "reset_confirm.html", ctx)?;
            Ok(html.into_response())
        }
        Err(err) => {
            let message = match err.message.as_str() {
                "Reset token has expired" => "That reset link has expired. Request a new one.",
                other => other,
            };
            let mut ctx = TeraContext::new();
            ctx.insert("error", message);
            ctx.insert("toast_error", message);
            let html = render_template(&state, "reset_request.html", ctx)?;
            Ok(html.into_response())
        }
    }
}

async fn do_reset_confirm(
    Path(token): Path<String>,
    State(state): State<AppState>,
    Form(form): Form<ResetConfirmForm>,
) -> AppResult<Response> {
    if form.password != form.confirm_password {
        let mut ctx = TeraContext::new();
        ctx.insert("token", &token);
        ctx.insert("error", "Passwords do not match");
        ctx.insert("toast_error", "Passwords do not match");
        let html = render_template(&state, "reset_confirm.html", ctx)?;
        return Ok(html.into_response());
    }

    let user = match ensure_valid_token(&state, &token).await {
        Ok(user) => user,
        Err(err) => {
            let message = match err.message.as_str() {
                "Reset token has expired" => "That reset link has expired. Request a new one.",
                other => other,
            };
            let mut ctx = TeraContext::new();
            ctx.insert("error", message);
            ctx.insert("toast_error", message);
            let html = render_template(&state, "reset_request.html", ctx)?;
            return Ok(html.into_response());
        }
    };
    let salt = SaltString::generate(&mut OsRng);
    let password_hash = Argon2::default()
        .hash_password(form.password.as_bytes(), &salt)
        .map_err(|_| AppError::new(StatusCode::BAD_REQUEST, "Could not hash password"))?
        .to_string();

    let result = sqlx::query(
        r#"
        update users
        set
            password_hash = $1,
            reset_token = null,
            reset_token_expires = null,
            must_reset = false
        where id = $2
          and reset_token = $3
          and reset_token_expires is not null
          and reset_token_expires > now()
        "#,
    )
    .bind(password_hash)
    .bind(user.id)
    .bind(&token)
    .execute(&state.pool)
    .await?;
    if result.rows_affected() == 0 {
        let mut ctx = TeraContext::new();
        ctx.insert(
            "error",
            "That reset link is invalid or has expired. Request a new one.",
        );
        ctx.insert(
            "toast_error",
            "That reset link is invalid or has expired. Request a new one.",
        );
        let html = render_template(&state, "reset_request.html", ctx)?;
        return Ok(html.into_response());
    }

    info!(user_id = %user.id, email = %user.email, "password reset confirmed");
    Ok(Redirect::to("/login").into_response())
}

async fn portal(State(state): State<AppState>, jar: SignedCookieJar) -> AppResult<Response> {
    let Some(user) = current_user(&state, &jar).await? else {
        let jar = jar.remove(Cookie::from(SESSION_COOKIE));
        return Ok((jar, Redirect::to("/login")).into_response());
    };

    if !is_writer(&user) {
        return Ok((jar, Redirect::to("/apps")).into_response());
    }

    let response =
        render_apps_manage_with_toast(&state, &user, None, None, None, None, None).await?;
    Ok((jar, response).into_response())
}

async fn invite_user(
    State(state): State<AppState>,
    jar: SignedCookieJar,
    Form(form): Form<InviteUserForm>,
) -> AppResult<Response> {
    let Some(user) = current_user(&state, &jar).await? else {
        warn!("invite_user: unauthenticated, redirecting to login");
        let jar = jar.remove(Cookie::from(SESSION_COOKIE));
        return Ok((jar, Redirect::to("/login")).into_response());
    };
    if !is_writer(&user) {
        warn!(user_id = %user.id, email = %user.email, "invite_user: forbidden (not writer)");
        return Err(AppError::new(
            StatusCode::FORBIDDEN,
            "Writer permission required",
        ));
    }

    let email = form.email.trim().to_lowercase();
    if email.is_empty() {
        let response = render_apps_manage_with_toast(
            &state,
            &user,
            Some("Email is required"),
            None,
            None,
            None,
            None,
        )
        .await?;
        return Ok((jar, response).into_response());
    }

    let existing = sqlx::query_scalar::<_, Uuid>("select id from users where email = $1")
        .bind(&email)
        .fetch_optional(&state.pool)
        .await?;
    if let Some(existing_id) = existing {
        let existing = sqlx::query_as::<_, (String, bool)>(
            "select permission, must_reset from users where id = $1",
        )
        .bind(existing_id)
        .fetch_one(&state.pool)
        .await?;

        let (permission, must_reset) = existing;
        if !permission.eq_ignore_ascii_case("reader") {
            let response = render_apps_manage_with_toast(
                &state,
                &user,
                Some("That user exists and is not a reader account."),
                None,
                None,
                None,
                None,
            )
            .await?;
            return Ok((jar, response).into_response());
        }

        if !must_reset {
            let response = render_apps_manage_with_toast(
                &state,
                &user,
                Some("User already exists"),
                None,
                None,
                None,
                None,
            )
            .await?;
            return Ok((jar, response).into_response());
        }

        let placeholder: String = rand::thread_rng()
            .sample_iter(&Alphanumeric)
            .take(16)
            .map(char::from)
            .collect();
        let salt = SaltString::generate(&mut OsRng);
        let password_hash = Argon2::default()
            .hash_password(placeholder.as_bytes(), &salt)
            .map_err(|_| AppError::new(StatusCode::BAD_REQUEST, "Could not hash password"))?
            .to_string();

        sqlx::query("update users set password_hash = $1, must_reset = true where id = $2")
            .bind(password_hash)
            .bind(existing_id)
            .execute(&state.pool)
            .await?;

        let reset_token = issue_invite_reset_token(&state, existing_id).await?;
        info!(
            user_id = %user.id,
            email = %user.email,
            invited_email = %email,
            "re-sent user invite"
        );

        let base = base_url();
        let reset_link = format!(
            "{}/reset/confirm/{}",
            base.trim_end_matches('/'),
            reset_token
        );
        let body = format!(
            "Welcome to the CDN portal.\n\nTemporary password: {pwd}\nLogin: {login}\nReset link (valid 7 days, required on first login): {link}\n\nIf you did not expect this, please contact support@palodurosystems.io.",
            pwd = placeholder,
            login = email,
            link = reset_link
        );
        let body_html = render_email_template(&state, "email/invite.html", {
            let mut ctx = TeraContext::new();
            ctx.insert("login_email", &email);
            ctx.insert("temporary_password", &placeholder);
            ctx.insert("reset_link", &reset_link);
            ctx
        })?;
        let subject = "Your CDN portal access";
        let _ = send_email(&email, subject, &body, Some(body_html)).await;

        let toast = format!("Re-sent invite to {}.", email);
        let response = render_apps_manage_with_toast(
            &state,
            &user,
            None,
            Some(&toast),
            Some(&email),
            None,
            None,
        )
        .await?;
        return Ok((jar, response).into_response());
    }

    let placeholder: String = rand::thread_rng()
        .sample_iter(&Alphanumeric)
        .take(16)
        .map(char::from)
        .collect();
    let salt = SaltString::generate(&mut OsRng);
    let password_hash = Argon2::default()
        .hash_password(placeholder.as_bytes(), &salt)
        .map_err(|_| AppError::new(StatusCode::BAD_REQUEST, "Could not hash password"))?
        .to_string();
    let id = Uuid::new_v4();

    sqlx::query(
        "insert into users (id, email, password_hash, permission, must_reset) values ($1, $2, $3, 'reader', true)",
    )
    .bind(id)
    .bind(&email)
    .bind(password_hash)
        .execute(&state.pool)
        .await?;

    let reset_token = issue_invite_reset_token(&state, id).await?;
    info!(user_id = %user.id, email = %user.email, invited_email = %email, "created user invite");

    let base = base_url();
    let reset_link = format!(
        "{}/reset/confirm/{}",
        base.trim_end_matches('/'),
        reset_token
    );
    let body = format!(
        "Welcome to the CDN portal.\n\nTemporary password: {pwd}\nLogin: {login}\nReset link (valid 7 days, required on first login): {link}\n\nIf you did not expect this, please contact support@palodurosystems.io.",
        pwd = placeholder,
        login = email,
        link = reset_link
    );
    let body_html = render_email_template(&state, "email/invite.html", {
        let mut ctx = TeraContext::new();
        ctx.insert("login_email", &email);
        ctx.insert("temporary_password", &placeholder);
        ctx.insert("reset_link", &reset_link);
        ctx
    })?;
    let subject = "Your CDN portal access";
    let _ = send_email(&email, subject, &body, Some(body_html)).await;

    let toast = format!("Invited {}.", email);
    let response =
        render_apps_manage_with_toast(&state, &user, None, Some(&toast), Some(&email), None, None)
            .await?;
    Ok((jar, response).into_response())
}

async fn list_assigned_apps(
    State(state): State<AppState>,
    jar: SignedCookieJar,
) -> AppResult<Response> {
    let Some(user) = current_user(&state, &jar).await? else {
        let jar = jar.remove(Cookie::from(SESSION_COOKIE));
        return Ok((jar, Redirect::to("/login")).into_response());
    };

    if is_writer(&user) {
        return Ok((jar, Redirect::to("/apps/manage")).into_response());
    }

    let rows = sqlx::query_as::<
        _,
        (
            String,
            String,
            Option<String>,
            Option<Uuid>,
            Option<String>,
            Option<String>,
            Option<String>,
        ),
    >(
        r#"
        select
            a.slug,
            a.name,
            a.description,
            a.icon_file_id,
            p.os,
            p.arch,
            p.version
        from apps a
        join app_assignments aa on aa.app_id = a.id
        left join lateral (
            select distinct on (os, arch) os, arch, version
            from app_packages
            where app_id = a.id
            order by os, arch, created_at desc
        ) p on true
        where aa.user_id = $1
        order by a.name, p.os, p.arch
        "#,
    )
    .bind(user.id)
    .fetch_all(&state.pool)
    .await?;

    let mut apps: Vec<AppCardView> = Vec::new();
    for (slug, name, description, icon_file_id, os, arch, version) in rows {
        let existing = apps.iter_mut().find(|a| a.slug == slug);
        let entry = match existing {
            Some(a) => a,
            None => {
                apps.push(AppCardView {
                    icon_text: name
                        .chars()
                        .next()
                        .map(|c| c.to_ascii_uppercase().to_string())
                        .unwrap_or_else(|| "?".to_string()),
                    slug: slug.clone(),
                    name,
                    description,
                    icon_url: icon_file_id.map(|id| format!("/apps/{}/icon?v={}", slug, id)),
                    latest_version: None,
                    can_download: false,
                    targets: Vec::new(),
                });
                apps.last_mut().unwrap()
            }
        };

        if let (Some(os), Some(arch), Some(version)) = (os, arch, version) {
            entry.targets.push(AppTargetView { os, arch, version });
        }
    }

    for app in &mut apps {
        app.can_download = !app.targets.is_empty();
        if app.targets.len() == 1 {
            app.latest_version = app.targets.first().map(|t| t.version.clone());
        }
    }

    let mut ctx = TeraContext::new();
    ctx.insert("user_email", &user.email);
    ctx.insert("apps", &apps);
    ctx.insert("can_write", &false);
    let html = render_template(&state, "apps.html", ctx)?;
    Ok((jar, html).into_response())
}

async fn apps_manage(
    State(state): State<AppState>,
    jar: SignedCookieJar,
    Query(query): Query<HashMap<String, String>>,
) -> AppResult<Response> {
    let Some(user) = current_user(&state, &jar).await? else {
        let jar = jar.remove(Cookie::from(SESSION_COOKIE));
        return Ok((jar, Redirect::to("/login")).into_response());
    };
    if !is_writer(&user) {
        return Err(AppError::new(
            StatusCode::FORBIDDEN,
            "Writer permission required",
        ));
    }

    let open_app_slug = query
        .get("app")
        .map(|s| sanitize_slug(s))
        .filter(|s| !s.is_empty());
    let edit_app_slug = query
        .get("edit")
        .map(|s| sanitize_slug(s))
        .filter(|s| !s.is_empty());
    let toast_success = query.get("toast_success").map(|s| s.as_str());
    let toast_error = query.get("toast_error").map(|s| s.as_str());

    let response = render_apps_manage_with_toast(
        &state,
        &user,
        toast_error,
        toast_success,
        None,
        open_app_slug,
        edit_app_slug,
    )
    .await?;
    Ok((jar, response).into_response())
}

async fn render_apps_manage_with_toast(
    state: &AppState,
    user: &User,
    toast_error: Option<&str>,
    toast_success: Option<&str>,
    new_invite_email: Option<&str>,
    open_app_slug: Option<String>,
    edit_app_slug: Option<String>,
) -> AppResult<Response> {
    let mut apps = sqlx::query_as::<_, AppManageView>(
        r#"
        select
            a.slug as slug,
            a.name as name,
            a.description as description,
            a.icon_file_id as icon_file_id,
            lp.version as latest_pkg_version,
            lp.os as latest_pkg_os,
            lp.arch as latest_pkg_arch,
            lf.version as latest_fw_version,
            lf.channel as latest_fw_channel,
            coalesce(ac.assignment_count, 0) as assignment_count
        from apps a
        left join lateral (
            select version, os, arch
            from app_packages
            where app_id = a.id
            order by created_at desc
            limit 1
        ) lp on true
        left join lateral (
            select version, channel
            from firmware_releases
            where app_id = a.id
            order by created_at desc
            limit 1
        ) lf on true
        left join lateral (
            select count(*)::bigint as assignment_count
            from app_assignments
            where app_id = a.id
        ) ac on true
        order by a.created_at desc
        "#,
    )
    .fetch_all(&state.pool)
    .await?;

    let target_rows = sqlx::query_as::<_, (String, String, String, String)>(
        r#"
        select a.slug, p.os, p.arch, p.version
        from apps a
        join (
            select distinct on (app_id, os, arch) app_id, os, arch, version, created_at
            from app_packages
            order by app_id, os, arch, created_at desc
        ) p on p.app_id = a.id
        order by a.created_at desc, p.os, p.arch
        "#,
    )
    .fetch_all(&state.pool)
    .await?;
    let mut targets_map: std::collections::BTreeMap<String, Vec<AppTargetView>> =
        std::collections::BTreeMap::new();
    for (slug, os, arch, version) in target_rows {
        targets_map
            .entry(slug)
            .or_default()
            .push(AppTargetView { os, arch, version });
    }

    let package_rows = sqlx::query_as::<_, (String, Uuid, String, String, String, DateTime<Utc>)>(
        r#"
        select a.slug, p.id, p.os, p.arch, p.version, p.created_at
        from apps a
        join app_packages p on p.app_id = a.id
        order by a.created_at desc, p.created_at desc
        "#,
    )
    .fetch_all(&state.pool)
    .await?;
    let mut packages_map: std::collections::BTreeMap<String, Vec<AppPackageView>> =
        std::collections::BTreeMap::new();
    for (slug, id, os, arch, version, created_at) in package_rows {
        packages_map.entry(slug).or_default().push(AppPackageView {
            id,
            os,
            arch,
            version,
            created_at,
        });
    }

    let firmware_rows = sqlx::query_as::<_, (String, Uuid, String, String, DateTime<Utc>)>(
        r#"
            select a.slug, f.id, f.channel, f.version, f.created_at
            from apps a
            join firmware_releases f on f.app_id = a.id
            order by a.created_at desc, f.created_at desc
            "#,
    )
    .fetch_all(&state.pool)
    .await?;
    let mut firmware_map: std::collections::BTreeMap<String, Vec<AppFirmwareView>> =
        std::collections::BTreeMap::new();
    for (slug, id, channel, version, created_at) in firmware_rows {
        firmware_map.entry(slug).or_default().push(AppFirmwareView {
            id,
            channel,
            version,
            created_at,
        });
    }

    let assigned_rows = sqlx::query_as::<_, (String, Uuid, String)>(
        r#"
        select a.slug, u.id, u.email
        from apps a
        join app_assignments aa on aa.app_id = a.id
        join users u on u.id = aa.user_id
        order by a.created_at desc, u.email
        "#,
    )
    .fetch_all(&state.pool)
    .await?;
    let mut assigned_map: std::collections::BTreeMap<String, Vec<AssignedUserView>> =
        std::collections::BTreeMap::new();
    for (app_slug, user_id, email) in assigned_rows {
        assigned_map
            .entry(app_slug)
            .or_default()
            .push(AssignedUserView { id: user_id, email });
    }

    for app in &mut apps {
        app.icon_url = app
            .icon_file_id
            .map(|id| format!("/apps/{}/icon?v={}", app.slug, id));
        app.icon_text = app
            .name
            .chars()
            .next()
            .map(|c| c.to_ascii_uppercase().to_string())
            .unwrap_or_else(|| "?".to_string());
        app.assigned_users = assigned_map.remove(&app.slug).unwrap_or_default();
        app.targets = targets_map.remove(&app.slug).unwrap_or_default();
        app.packages = packages_map.remove(&app.slug).unwrap_or_default();
        app.firmware = firmware_map.remove(&app.slug).unwrap_or_default();
    }

    let users =
        sqlx::query_as::<_, UserBrief>("select id, email, permission from users order by email")
            .fetch_all(&state.pool)
            .await?;

    let edit_app = edit_app_slug
        .as_deref()
        .and_then(|slug| apps.iter().find(|a| a.slug == slug))
        .cloned();

    let mut ctx = TeraContext::new();
    ctx.insert("user_email", &user.email);
    ctx.insert("apps", &apps);
    ctx.insert("users", &users);
    ctx.insert("can_write", &true);
    ctx.insert("open_app_slug", &open_app_slug);
    ctx.insert("edit_app", &edit_app);
    if let Some(message) = toast_error {
        ctx.insert("toast_error", message);
    }
    if let Some(message) = toast_success {
        ctx.insert("toast_success", message);
    }
    if let Some(email) = new_invite_email {
        ctx.insert("new_invite_email", email);
    }
    let html = render_template(state, "apps_manage.html", ctx)?;
    Ok(html.into_response())
}

async fn create_app(
    State(state): State<AppState>,
    jar: SignedCookieJar,
    Form(form): Form<HashMap<String, String>>,
) -> AppResult<Response> {
    let Some(user) = current_user(&state, &jar).await? else {
        let jar = jar.remove(Cookie::from(SESSION_COOKIE));
        return Ok((jar, Redirect::to("/login")).into_response());
    };
    if !is_writer(&user) {
        return Err(AppError::new(
            StatusCode::FORBIDDEN,
            "Writer permission required",
        ));
    }

    let slug_raw = form.get("slug").map(|s| s.trim()).unwrap_or("");
    let name = form.get("name").map(|s| s.trim()).unwrap_or("");
    let description = form
        .get("description")
        .map(|s| s.trim())
        .filter(|s| !s.is_empty())
        .map(|s| s.to_string());

    let slug = sanitize_slug(slug_raw);
    if slug.is_empty() {
        return Err(AppError::new(StatusCode::BAD_REQUEST, "Invalid app slug"));
    }
    if name.is_empty() {
        return Err(AppError::new(
            StatusCode::BAD_REQUEST,
            "App name is required",
        ));
    }

    let id = Uuid::new_v4();
    sqlx::query("insert into apps (id, slug, name, description) values ($1, $2, $3, $4)")
        .bind(id)
        .bind(&slug)
        .bind(name)
        .bind(description)
        .execute(&state.pool)
        .await?;

    Ok((jar, Redirect::to("/apps/manage")).into_response())
}

async fn assign_app(
    Path(slug): Path<String>,
    State(state): State<AppState>,
    jar: SignedCookieJar,
    Form(form): Form<HashMap<String, String>>,
) -> AppResult<Response> {
    let Some(user) = current_user(&state, &jar).await? else {
        let jar = jar.remove(Cookie::from(SESSION_COOKIE));
        return Ok((jar, Redirect::to("/login")).into_response());
    };
    if !is_writer(&user) {
        return Err(AppError::new(
            StatusCode::FORBIDDEN,
            "Writer permission required",
        ));
    }

    let slug = sanitize_slug(&slug);
    let user_id = form
        .get("user_id")
        .map(|s| s.trim())
        .filter(|s| !s.is_empty())
        .ok_or_else(|| AppError::new(StatusCode::BAD_REQUEST, "Select a user"))?;
    let user_id = Uuid::parse_str(user_id)
        .map_err(|_| AppError::new(StatusCode::BAD_REQUEST, "Invalid user id"))?;

    let app_id: Uuid = sqlx::query_scalar("select id from apps where slug = $1")
        .bind(&slug)
        .fetch_optional(&state.pool)
        .await?
        .ok_or_else(|| AppError::new(StatusCode::NOT_FOUND, "App not found"))?;

    sqlx::query(
        "insert into app_assignments (app_id, user_id) values ($1, $2) on conflict do nothing",
    )
    .bind(app_id)
    .bind(user_id)
    .execute(&state.pool)
    .await?;

    let return_to = safe_return_to(form.get("return_to").map(|s| s.as_str()));
    let return_to = add_query_param(&return_to, "toast_success", "Assigned.");
    Ok((jar, Redirect::to(&return_to)).into_response())
}

async fn assign_app_generic(
    State(state): State<AppState>,
    jar: SignedCookieJar,
    Form(form): Form<HashMap<String, String>>,
) -> AppResult<Response> {
    let Some(user) = current_user(&state, &jar).await? else {
        let jar = jar.remove(Cookie::from(SESSION_COOKIE));
        return Ok((jar, Redirect::to("/login")).into_response());
    };
    if !is_writer(&user) {
        return Err(AppError::new(
            StatusCode::FORBIDDEN,
            "Writer permission required",
        ));
    }

    let slug = form
        .get("slug")
        .map(|s| sanitize_slug(s))
        .filter(|s| !s.is_empty())
        .ok_or_else(|| AppError::new(StatusCode::BAD_REQUEST, "Select an app"))?;
    let user_id = form
        .get("user_id")
        .map(|s| s.trim())
        .filter(|s| !s.is_empty())
        .ok_or_else(|| AppError::new(StatusCode::BAD_REQUEST, "Select a user"))?;
    let user_id = Uuid::parse_str(user_id)
        .map_err(|_| AppError::new(StatusCode::BAD_REQUEST, "Invalid user id"))?;

    let app_id: Uuid = sqlx::query_scalar("select id from apps where slug = $1")
        .bind(&slug)
        .fetch_optional(&state.pool)
        .await?
        .ok_or_else(|| AppError::new(StatusCode::NOT_FOUND, "App not found"))?;

    sqlx::query(
        "insert into app_assignments (app_id, user_id) values ($1, $2) on conflict do nothing",
    )
    .bind(app_id)
    .bind(user_id)
    .execute(&state.pool)
    .await?;

    let return_to = safe_return_to(form.get("return_to").map(|s| s.as_str()));
    let return_to = add_query_param(&return_to, "toast_success", "Assigned.");
    Ok((jar, Redirect::to(&return_to)).into_response())
}

async fn unassign_app(
    Path(slug): Path<String>,
    State(state): State<AppState>,
    jar: SignedCookieJar,
    Form(form): Form<HashMap<String, String>>,
) -> AppResult<Response> {
    let Some(user) = current_user(&state, &jar).await? else {
        let jar = jar.remove(Cookie::from(SESSION_COOKIE));
        return Ok((jar, Redirect::to("/login")).into_response());
    };
    if !is_writer(&user) {
        return Err(AppError::new(
            StatusCode::FORBIDDEN,
            "Writer permission required",
        ));
    }

    let slug = sanitize_slug(&slug);
    let user_id = form
        .get("user_id")
        .map(|s| s.trim())
        .filter(|s| !s.is_empty())
        .ok_or_else(|| AppError::new(StatusCode::BAD_REQUEST, "Select a user"))?;
    let user_id = Uuid::parse_str(user_id)
        .map_err(|_| AppError::new(StatusCode::BAD_REQUEST, "Invalid user id"))?;

    let app_id: Uuid = sqlx::query_scalar("select id from apps where slug = $1")
        .bind(&slug)
        .fetch_optional(&state.pool)
        .await?
        .ok_or_else(|| AppError::new(StatusCode::NOT_FOUND, "App not found"))?;

    sqlx::query("delete from app_assignments where app_id = $1 and user_id = $2")
        .bind(app_id)
        .bind(user_id)
        .execute(&state.pool)
        .await?;

    let return_to = safe_return_to(form.get("return_to").map(|s| s.as_str()));
    let return_to = add_query_param(&return_to, "toast_success", "Revoked.");
    Ok((jar, Redirect::to(&return_to)).into_response())
}

async fn update_app(
    Path(slug): Path<String>,
    State(state): State<AppState>,
    jar: SignedCookieJar,
    mut multipart: Multipart,
) -> AppResult<Response> {
    let Some(user) = current_user(&state, &jar).await? else {
        let jar = jar.remove(Cookie::from(SESSION_COOKIE));
        return Ok((jar, Redirect::to("/login")).into_response());
    };
    if !is_writer(&user) {
        return Err(AppError::new(
            StatusCode::FORBIDDEN,
            "Writer permission required",
        ));
    }

    let slug = sanitize_slug(&slug);
    let app_id: Uuid = sqlx::query_scalar("select id from apps where slug = $1")
        .bind(&slug)
        .fetch_optional(&state.pool)
        .await?
        .ok_or_else(|| AppError::new(StatusCode::NOT_FOUND, "App not found"))?;

    let mut name: Option<String> = None;
    let mut description: Option<String> = None;
    let mut icon_file_id: Option<Uuid> = None;
    let mut return_to: Option<String> = None;

    while let Some(field) = multipart
        .next_field()
        .await
        .map_err(|_| AppError::new(StatusCode::BAD_REQUEST, "Invalid upload payload"))?
    {
        match field.name() {
            Some("name") => {
                let v = field
                    .text()
                    .await
                    .map_err(|_| AppError::new(StatusCode::BAD_REQUEST, "Invalid name"))?;
                let v = v.trim();
                if v.is_empty() {
                    return Err(AppError::new(StatusCode::BAD_REQUEST, "Name is required"));
                }
                name = Some(v.to_string());
            }
            Some("description") => {
                let v = field
                    .text()
                    .await
                    .map_err(|_| AppError::new(StatusCode::BAD_REQUEST, "Invalid description"))?;
                let v = v.trim();
                description = if v.is_empty() {
                    None
                } else {
                    Some(v.to_string())
                };
            }
            Some("icon") => {
                if field.file_name().is_some() {
                    let icon_prefix = format!("apps/{}/icon", sanitize_bucket(&slug));
                    let stored =
                        save_upload_to_prefix(&state, field, &icon_prefix, &["png", "svg"]).await?;
                    icon_file_id = Some(stored.id);
                }
            }
            Some("return_to") => {
                let v = field
                    .text()
                    .await
                    .map_err(|_| AppError::new(StatusCode::BAD_REQUEST, "Invalid return_to"))?;
                return_to = Some(v);
            }
            _ => {}
        }
    }

    let name = name.ok_or_else(|| AppError::new(StatusCode::BAD_REQUEST, "Name is required"))?;
    let description = description;

    sqlx::query("update apps set name = $1, description = $2 where id = $3")
        .bind(&name)
        .bind(description)
        .bind(app_id)
        .execute(&state.pool)
        .await?;

    if let Some(icon_file_id) = icon_file_id {
        let _ = sqlx::query("update apps set icon_file_id = $1 where id = $2")
            .bind(icon_file_id)
            .bind(app_id)
            .execute(&state.pool)
            .await;
    }

    state.file_by_id_cache.invalidate_all();

    let return_to = safe_return_to(return_to.as_deref());
    let return_to = add_query_param(&return_to, "toast_success", "Saved.");
    Ok((jar, Redirect::to(&return_to)).into_response())
}

async fn delete_app(
    Path(slug): Path<String>,
    State(state): State<AppState>,
    jar: SignedCookieJar,
    Form(form): Form<HashMap<String, String>>,
) -> AppResult<Response> {
    let Some(user) = current_user(&state, &jar).await? else {
        let jar = jar.remove(Cookie::from(SESSION_COOKIE));
        return Ok((jar, Redirect::to("/login")).into_response());
    };
    if !is_writer(&user) {
        return Err(AppError::new(
            StatusCode::FORBIDDEN,
            "Writer permission required",
        ));
    }

    let slug = sanitize_slug(&slug);
    let confirm = form.get("confirm_slug").map(|s| s.trim()).unwrap_or("");
    if confirm != slug {
        let response = render_apps_manage_with_toast(
            &state,
            &user,
            Some("Type the app slug to confirm deletion."),
            None,
            None,
            None,
            None,
        )
        .await?;
        return Ok((jar, response).into_response());
    }

    let deleted: Option<Uuid> = sqlx::query_scalar("delete from apps where slug = $1 returning id")
        .bind(&slug)
        .fetch_optional(&state.pool)
        .await?;

    if deleted.is_none() {
        let response = render_apps_manage_with_toast(
            &state,
            &user,
            Some("App not found."),
            None,
            None,
            None,
            None,
        )
        .await?;
        return Ok((jar, response).into_response());
    }

    let prefix = format!("apps/{}", sanitize_bucket(&slug));
    let files = sqlx::query_as::<_, CachedFile>(
        "select id, filename, local_path, size_bytes, created_at from cached_files where local_path like $1 || '/%'",
    )
    .bind(&prefix)
    .fetch_all(&state.pool)
    .await?;

    for file in files {
        if let Some(path) = resolve_cache_path(&state, &file.local_path).await? {
            let _ = tokio::fs::remove_file(path).await;
        }
        let _ = sqlx::query("delete from cached_files where id = $1")
            .bind(file.id)
            .execute(&state.pool)
            .await;
    }

    let app_dir = state.cache_dir.join(&prefix);
    if app_dir.starts_with(&state.cache_dir) {
        let _ = tokio::fs::remove_dir_all(&app_dir).await;
    }

    state.file_by_id_cache.invalidate_all();

    let toast = format!("Deleted app {}.", slug);
    let response =
        render_apps_manage_with_toast(&state, &user, None, Some(&toast), None, None, None).await?;
    Ok((jar, response).into_response())
}

async fn download_latest_pkg(
    Path(slug): Path<String>,
    State(state): State<AppState>,
    jar: SignedCookieJar,
    headers: HeaderMap,
    Query(query): Query<HashMap<String, String>>,
) -> AppResult<Response> {
    let Some(user) = current_user(&state, &jar).await? else {
        warn!(app_slug = %slug, "download_latest_pkg: unauthenticated, redirecting to login");
        let jar = jar.remove(Cookie::from(SESSION_COOKIE));
        return Ok((jar, Redirect::to("/login")).into_response());
    };
    let slug = sanitize_slug(&slug);

    let requested = parse_target_query(&query).or_else(|| detect_target_from_user_agent(&headers));

    let record: Option<(Uuid, String, String, String)> = match (is_writer(&user), requested) {
        (true, Some((os, arch))) => {
            sqlx::query_as::<_, (Uuid, String, String, String)>(
                r#"
                select p.pkg_file_id, p.version, p.os, p.arch
                from apps a
                join app_packages p on p.app_id = a.id
                where a.slug = $1 and p.os = $2 and (p.arch = $3 or p.arch = 'any')
                order by p.created_at desc, (p.arch = $3) desc
                limit 1
                "#,
            )
            .bind(&slug)
            .bind(&os)
            .bind(&arch)
            .fetch_optional(&state.pool)
            .await?
        }
        (false, Some((os, arch))) => {
            sqlx::query_as::<_, (Uuid, String, String, String)>(
                r#"
                select p.pkg_file_id, p.version, p.os, p.arch
                from apps a
                join app_assignments aa on aa.app_id = a.id
                join app_packages p on p.app_id = a.id
                where a.slug = $1 and aa.user_id = $2 and p.os = $3 and (p.arch = $4 or p.arch = 'any')
                order by p.created_at desc, (p.arch = $4) desc
                limit 1
                "#,
            )
            .bind(&slug)
            .bind(user.id)
            .bind(&os)
            .bind(&arch)
            .fetch_optional(&state.pool)
            .await?
        }
        (true, None) => {
            sqlx::query_as::<_, (Uuid, String, String, String)>(
                r#"
                select p.pkg_file_id, p.version, p.os, p.arch
                from apps a
                join app_packages p on p.app_id = a.id
                where a.slug = $1
                order by p.created_at desc
                limit 1
                "#,
            )
            .bind(&slug)
            .fetch_optional(&state.pool)
            .await?
        }
        (false, None) => {
            sqlx::query_as::<_, (Uuid, String, String, String)>(
                r#"
                select p.pkg_file_id, p.version, p.os, p.arch
                from apps a
                join app_assignments aa on aa.app_id = a.id
                join app_packages p on p.app_id = a.id
                where a.slug = $1 and aa.user_id = $2
                order by p.created_at desc
                limit 1
                "#,
            )
            .bind(&slug)
            .bind(user.id)
            .fetch_optional(&state.pool)
            .await?
        }
    };

    let Some((file_id, version, os, arch)) = record else {
        return Err(AppError::new(StatusCode::NOT_FOUND, "Package not found"));
    };

    info!(email = %user.email, app_slug = %slug, pkg_version = %version, pkg_os = %os, pkg_arch = %arch, file_id = %file_id, "download_latest_pkg");
    stream_cached_file(&state, file_id, jar).await
}

async fn download_installer(
    Path(slug): Path<String>,
    State(state): State<AppState>,
    jar: SignedCookieJar,
    headers: HeaderMap,
    Query(query): Query<HashMap<String, String>>,
) -> AppResult<Response> {
    let Some(user) = current_user(&state, &jar).await? else {
        warn!(app_slug = %slug, "download_installer: unauthenticated, redirecting to login");
        let jar = jar.remove(Cookie::from(SESSION_COOKIE));
        return Ok((jar, Redirect::to("/login")).into_response());
    };
    let slug = sanitize_slug(&slug);

    let requested = parse_target_query(&query).or_else(|| detect_target_from_user_agent(&headers));

    let record: Option<(Uuid, String, String, String)> = match (is_writer(&user), requested) {
        (true, Some((os, arch))) => {
            sqlx::query_as::<_, (Uuid, String, String, String)>(
                r#"
                select p.pkg_file_id, p.version, p.os, p.arch
                from apps a
                join app_packages p on p.app_id = a.id
                where a.slug = $1 and p.os = $2 and (p.arch = $3 or p.arch = 'any')
                order by p.created_at desc, (p.arch = $3) desc
                limit 1
                "#,
            )
            .bind(&slug)
            .bind(&os)
            .bind(&arch)
            .fetch_optional(&state.pool)
            .await?
        }
        (false, Some((os, arch))) => {
            sqlx::query_as::<_, (Uuid, String, String, String)>(
                r#"
                select p.pkg_file_id, p.version, p.os, p.arch
                from apps a
                join app_assignments aa on aa.app_id = a.id
                join app_packages p on p.app_id = a.id
                where a.slug = $1 and aa.user_id = $2 and p.os = $3 and (p.arch = $4 or p.arch = 'any')
                order by p.created_at desc, (p.arch = $4) desc
                limit 1
                "#,
            )
            .bind(&slug)
            .bind(user.id)
            .bind(&os)
            .bind(&arch)
            .fetch_optional(&state.pool)
            .await?
        }
        (true, None) => {
            sqlx::query_as::<_, (Uuid, String, String, String)>(
                r#"
                select p.pkg_file_id, p.version, p.os, p.arch
                from apps a
                join app_packages p on p.app_id = a.id
                where a.slug = $1
                order by p.created_at desc
                limit 1
                "#,
            )
            .bind(&slug)
            .fetch_optional(&state.pool)
            .await?
        }
        (false, None) => {
            sqlx::query_as::<_, (Uuid, String, String, String)>(
                r#"
                select p.pkg_file_id, p.version, p.os, p.arch
                from apps a
                join app_assignments aa on aa.app_id = a.id
                join app_packages p on p.app_id = a.id
                where a.slug = $1 and aa.user_id = $2
                order by p.created_at desc
                limit 1
                "#,
            )
            .bind(&slug)
            .bind(user.id)
            .fetch_optional(&state.pool)
            .await?
        }
    };

    let Some((pkg_file_id, pkg_version, os, arch)) = record else {
        return Err(AppError::new(StatusCode::NOT_FOUND, "Package not found"));
    };

    let pkg_file = fetch_file_by_id(&state, pkg_file_id).await?;
    let Some(pkg_file) = pkg_file else {
        return Err(AppError::new(StatusCode::NOT_FOUND, "Package not found"));
    };
    let pkg_path = state.cache_dir.join(&pkg_file.local_path);
    let pkg_path = tokio::fs::canonicalize(pkg_path)
        .await
        .map_err(|_| AppError::new(StatusCode::NOT_FOUND, "File missing on disk"))?;
    if !pkg_path.starts_with(&state.cache_dir) {
        return Err(AppError::new(
            StatusCode::BAD_REQUEST,
            "Invalid file location",
        ));
    }

    let (installer_path, installer_filename) = extract_installer_from_pkg(&pkg_path).await?;
    let file_handle = File::open(&installer_path)
        .await
        .map_err(|_| AppError::new(StatusCode::NOT_FOUND, "Installer missing on disk"))?;

    tokio::spawn({
        let installer_path = installer_path.clone();
        async move {
            tokio::time::sleep(StdDuration::from_secs(300)).await;
            let _ = tokio::fs::remove_file(&installer_path).await;
        }
    });

    info!(
        email = %user.email,
        app_slug = %slug,
        pkg_version = %pkg_version,
        pkg_os = %os,
        pkg_arch = %arch,
        pkg_file_id = %pkg_file_id,
        installer_filename = %installer_filename,
        "download_installer"
    );

    let stream = ReaderStream::new(file_handle);
    let response = Response::builder()
        .status(StatusCode::OK)
        .header(header::CONTENT_TYPE, "application/octet-stream")
        .header(
            header::CONTENT_DISPOSITION,
            format!("attachment; filename=\"{}\"", installer_filename),
        )
        .body(Body::from_stream(stream))
        .map_err(AppError::from)?;

    Ok((jar, response).into_response())
}

async fn download_bootstrap(
    Path(slug): Path<String>,
    State(state): State<AppState>,
    jar: SignedCookieJar,
    headers: HeaderMap,
    Query(query): Query<HashMap<String, String>>,
) -> AppResult<Response> {
    let Some(user) = current_user(&state, &jar).await? else {
        warn!(app_slug = %slug, "download_bootstrap: unauthenticated, redirecting to login");
        let jar = jar.remove(Cookie::from(SESSION_COOKIE));
        return Ok((jar, Redirect::to("/login")).into_response());
    };
    let slug = sanitize_slug(&slug);

    let requested = parse_target_query(&query).or_else(|| detect_target_from_user_agent(&headers));

    let record: Option<(Uuid, String, Uuid, String, String, String)> =
        match (is_writer(&user), requested) {
        (true, Some((os, arch))) => {
            sqlx::query_as::<_, (Uuid, String, Uuid, String, String, String)>(
                r#"
                select a.id, a.name, p.pkg_file_id, p.version, p.os, p.arch
                from apps a
                join app_packages p on p.app_id = a.id
                where a.slug = $1 and p.os = $2 and (p.arch = $3 or p.arch = 'any')
                order by p.created_at desc, (p.arch = $3) desc
                limit 1
                "#,
            )
            .bind(&slug)
            .bind(&os)
            .bind(&arch)
            .fetch_optional(&state.pool)
            .await?
        }
        (false, Some((os, arch))) => {
            sqlx::query_as::<_, (Uuid, String, Uuid, String, String, String)>(
                r#"
                select a.id, a.name, p.pkg_file_id, p.version, p.os, p.arch
                from apps a
                join app_assignments aa on aa.app_id = a.id
                join app_packages p on p.app_id = a.id
                where a.slug = $1 and aa.user_id = $2 and p.os = $3 and (p.arch = $4 or p.arch = 'any')
                order by p.created_at desc, (p.arch = $4) desc
                limit 1
                "#,
            )
            .bind(&slug)
            .bind(user.id)
            .bind(&os)
            .bind(&arch)
            .fetch_optional(&state.pool)
            .await?
        }
        (true, None) => {
            sqlx::query_as::<_, (Uuid, String, Uuid, String, String, String)>(
                r#"
                select a.id, a.name, p.pkg_file_id, p.version, p.os, p.arch
                from apps a
                join app_packages p on p.app_id = a.id
                where a.slug = $1
                order by p.created_at desc
                limit 1
                "#,
            )
            .bind(&slug)
            .fetch_optional(&state.pool)
            .await?
        }
        (false, None) => {
            sqlx::query_as::<_, (Uuid, String, Uuid, String, String, String)>(
                r#"
                select a.id, a.name, p.pkg_file_id, p.version, p.os, p.arch
                from apps a
                join app_assignments aa on aa.app_id = a.id
                join app_packages p on p.app_id = a.id
                where a.slug = $1 and aa.user_id = $2
                order by p.created_at desc
                limit 1
                "#,
            )
            .bind(&slug)
            .bind(user.id)
            .fetch_optional(&state.pool)
            .await?
        }
    };

    let Some((app_id, app_name, _pkg_file_id, pkg_version, os, arch)) = record else {
        return Err(AppError::new(StatusCode::NOT_FOUND, "Package not found"));
    };

    let targets = sqlx::query_as::<_, (String, String, String)>(
        r#"
        select distinct on (os, arch) os, arch, version
        from app_packages
        where app_id = $1
        order by os, arch, created_at desc
        "#,
    )
    .bind(app_id)
    .fetch_all(&state.pool)
    .await?;
    let targets: Vec<AppTargetView> = targets
        .into_iter()
        .map(|(os, arch, version)| AppTargetView { os, arch, version })
        .collect();

    let token_name = format!("Bootstrap {} ({}/{})", slug, os, arch);
    let token = mint_api_token_for_user(&state, user.id, &token_name).await?;

    let download_url = format!("/apps/{}/installer/download?target={}:{}", slug, os, arch);

    let mut ctx = TeraContext::new();
    ctx.insert("user_email", &user.email);
    ctx.insert("app_slug", &slug);
    ctx.insert("app_name", &app_name);
    ctx.insert("os", &os);
    ctx.insert("arch", &arch);
    ctx.insert("pkg_version", &pkg_version);
    ctx.insert("targets", &targets);
    ctx.insert("download_url", &download_url);
    ctx.insert("new_token_name", &token_name);
    ctx.insert("new_token", &token);
    ctx.insert(
        "toast_success",
        "Copy this token now; it won’t be shown again. Paste it into the desktop app to enable update checks.",
    );

    let html = render_template(&state, "app_download.html", ctx)?;
    Ok((jar, html).into_response())
}

async fn app_icon(
    Path(slug): Path<String>,
    State(state): State<AppState>,
    jar: SignedCookieJar,
) -> AppResult<Response> {
    let Some(user) = current_user(&state, &jar).await? else {
        let jar = jar.remove(Cookie::from(SESSION_COOKIE));
        return Ok((jar, Redirect::to("/login")).into_response());
    };
    let slug = sanitize_slug(&slug);

    let cached_file = if is_writer(&user) {
        sqlx::query_as::<_, CachedFile>(
            r#"
            select cf.id, cf.filename, cf.local_path, cf.size_bytes, cf.created_at
            from apps a
            join cached_files cf on cf.id = a.icon_file_id
            where a.slug = $1
            limit 1
            "#,
        )
        .bind(&slug)
        .fetch_optional(&state.pool)
        .await?
    } else {
        sqlx::query_as::<_, CachedFile>(
            r#"
            select cf.id, cf.filename, cf.local_path, cf.size_bytes, cf.created_at
            from apps a
            join app_assignments aa on aa.app_id = a.id
            join cached_files cf on cf.id = a.icon_file_id
            where a.slug = $1 and aa.user_id = $2
            limit 1
            "#,
        )
        .bind(&slug)
        .bind(user.id)
        .fetch_optional(&state.pool)
        .await?
    };

    let Some(cached_file) = cached_file else {
        return Err(AppError::new(StatusCode::NOT_FOUND, "Icon not found"));
    };

    stream_cached_image_file(&state, &cached_file).await
}

async fn latest_firmware_json(
    Path(slug): Path<String>,
    State(state): State<AppState>,
    jar: SignedCookieJar,
    Query(query): Query<HashMap<String, String>>,
) -> AppResult<Response> {
    let Some(user) = current_user(&state, &jar).await? else {
        let jar = jar.remove(Cookie::from(SESSION_COOKIE));
        return Ok((jar, Redirect::to("/login")).into_response());
    };
    let slug = sanitize_slug(&slug);
    let channel = query
        .get("channel")
        .map(|s| s.trim())
        .filter(|s| !s.is_empty())
        .map(sanitize_slug)
        .filter(|s| !s.is_empty())
        .unwrap_or_else(|| "stable".to_string());

    let record = if is_writer(&user) {
        sqlx::query_as::<_, (String, String, Uuid)>(
            r#"
            select f.version, f.sha256, f.file_id
            from apps a
            join firmware_releases f on f.app_id = a.id
            where a.slug = $1 and f.channel = $2
            order by f.created_at desc
            limit 1
            "#,
        )
        .bind(&slug)
        .bind(&channel)
        .fetch_optional(&state.pool)
        .await?
    } else {
        sqlx::query_as::<_, (String, String, Uuid)>(
            r#"
            select f.version, f.sha256, f.file_id
            from apps a
            join app_assignments aa on aa.app_id = a.id
            join firmware_releases f on f.app_id = a.id
            where a.slug = $1 and aa.user_id = $2 and f.channel = $3
            order by f.created_at desc
            limit 1
            "#,
        )
        .bind(&slug)
        .bind(user.id)
        .bind(&channel)
        .fetch_optional(&state.pool)
        .await?
    };

    let Some((version, sha256, file_id)) = record else {
        return Err(AppError::new_plain(
            StatusCode::NOT_FOUND,
            "Firmware not found",
        ));
    };

    let payload = serde_json::json!({
        "app_slug": slug,
        "channel": channel,
        "version": version,
        "sha256": sha256,
        "download_url": format!("/apps/{}/firmware/latest/download?channel={}", slug, channel),
        "file_id": file_id,
    });
    Ok((jar, axum::Json(payload)).into_response())
}

async fn download_latest_firmware(
    Path(slug): Path<String>,
    State(state): State<AppState>,
    jar: SignedCookieJar,
    Query(query): Query<HashMap<String, String>>,
) -> AppResult<Response> {
    let Some(user) = current_user(&state, &jar).await? else {
        let jar = jar.remove(Cookie::from(SESSION_COOKIE));
        return Ok((jar, Redirect::to("/login")).into_response());
    };
    let slug = sanitize_slug(&slug);
    let channel = query
        .get("channel")
        .map(|s| s.trim())
        .filter(|s| !s.is_empty())
        .map(sanitize_slug)
        .filter(|s| !s.is_empty())
        .unwrap_or_else(|| "stable".to_string());

    let file_id = if is_writer(&user) {
        sqlx::query_scalar::<_, Uuid>(
            r#"
            select f.file_id
            from apps a
            join firmware_releases f on f.app_id = a.id
            where a.slug = $1 and f.channel = $2
            order by f.created_at desc
            limit 1
            "#,
        )
        .bind(&slug)
        .bind(&channel)
        .fetch_optional(&state.pool)
        .await?
    } else {
        sqlx::query_scalar::<_, Uuid>(
            r#"
            select f.file_id
            from apps a
            join app_assignments aa on aa.app_id = a.id
            join firmware_releases f on f.app_id = a.id
            where a.slug = $1 and aa.user_id = $2 and f.channel = $3
            order by f.created_at desc
            limit 1
            "#,
        )
        .bind(&slug)
        .bind(user.id)
        .bind(&channel)
        .fetch_optional(&state.pool)
        .await?
    };

    let Some(file_id) = file_id else {
        return Err(AppError::new_plain(
            StatusCode::NOT_FOUND,
            "Firmware not found",
        ));
    };
    stream_cached_file(&state, file_id, jar).await
}

async fn upload_app_package(
    Path(slug): Path<String>,
    State(state): State<AppState>,
    jar: SignedCookieJar,
    mut multipart: Multipart,
) -> AppResult<Response> {
    let Some(user) = current_user(&state, &jar).await? else {
        let jar = jar.remove(Cookie::from(SESSION_COOKIE));
        return Ok((jar, Redirect::to("/login")).into_response());
    };
    if !is_writer(&user) {
        return Err(AppError::new(
            StatusCode::FORBIDDEN,
            "Writer permission required",
        ));
    }
    let slug = sanitize_slug(&slug);

    let app_id: Uuid = sqlx::query_scalar("select id from apps where slug = $1")
        .bind(&slug)
        .fetch_optional(&state.pool)
        .await?
        .ok_or_else(|| AppError::new(StatusCode::NOT_FOUND, "App not found"))?;

    let mut version: Option<String> = None;
    let mut os: Option<String> = None;
    let mut arch: Option<String> = None;
    let mut uploaded: Option<(Uuid, String)> = None;
    let mut return_to: Option<String> = None;

    while let Some(field) = multipart
        .next_field()
        .await
        .map_err(|_| AppError::new(StatusCode::BAD_REQUEST, "Invalid upload payload"))?
    {
        match field.name() {
            Some("os") => {
                let v = field
                    .text()
                    .await
                    .map_err(|_| AppError::new(StatusCode::BAD_REQUEST, "Invalid os"))?;
                os = Some(normalize_os(v.trim())?);
            }
            Some("arch") => {
                let v = field
                    .text()
                    .await
                    .map_err(|_| AppError::new(StatusCode::BAD_REQUEST, "Invalid arch"))?;
                arch = Some(normalize_arch(v.trim())?);
            }
            Some("version") => {
                let v = field
                    .text()
                    .await
                    .map_err(|_| AppError::new(StatusCode::BAD_REQUEST, "Invalid version"))?;
                let v = v.trim();
                if v.is_empty() {
                    return Err(AppError::new(
                        StatusCode::BAD_REQUEST,
                        "Version is required",
                    ));
                }
                version = Some(v.to_string());
            }
            Some("file") => {
                let version = version
                    .clone()
                    .ok_or_else(|| AppError::new(StatusCode::BAD_REQUEST, "Version is required"))?;
                let os = os
                    .clone()
                    .ok_or_else(|| AppError::new(StatusCode::BAD_REQUEST, "OS is required"))?;
                let arch = arch.clone().unwrap_or_else(|| "x86_64".to_string());
                let prefix = format!(
                    "apps/{}/packages/{}/{}/{}",
                    sanitize_bucket(&slug),
                    sanitize_bucket(&os),
                    sanitize_bucket(&arch),
                    sanitize_bucket(&version)
                );
                let result = save_upload_to_prefix(&state, field, &prefix, &["pkg"]).await?;
                uploaded = Some((result.id, result.sha256));
            }
            Some("return_to") => {
                let v = field
                    .text()
                    .await
                    .map_err(|_| AppError::new(StatusCode::BAD_REQUEST, "Invalid return_to"))?;
                return_to = Some(v);
            }
            _ => {}
        }
    }

    let Some((pkg_file_id, pkg_sha256)) = uploaded else {
        return Err(AppError::new(StatusCode::BAD_REQUEST, "No file provided"));
    };
    let version =
        version.ok_or_else(|| AppError::new(StatusCode::BAD_REQUEST, "Version is required"))?;
    let os = os.ok_or_else(|| AppError::new(StatusCode::BAD_REQUEST, "OS is required"))?;
    let arch = arch.unwrap_or_else(|| "x86_64".to_string());

    let package_id = Uuid::new_v4();
    sqlx::query(
        r#"
        insert into app_packages (id, app_id, version, os, arch, pkg_file_id, pkg_sha256)
        values ($1, $2, $3, $4, $5, $6, $7)
        on conflict (app_id, version, os, arch)
        do update set pkg_file_id = excluded.pkg_file_id, pkg_sha256 = excluded.pkg_sha256, created_at = now()
        "#,
    )
    .bind(package_id)
    .bind(app_id)
    .bind(&version)
    .bind(&os)
    .bind(&arch)
    .bind(pkg_file_id)
    .bind(&pkg_sha256)
    .execute(&state.pool)
    .await?;

    state.file_by_id_cache.invalidate_all();
    let return_to = safe_return_to(return_to.as_deref());
    let return_to = add_query_param(&return_to, "toast_success", "Uploaded package.");
    Ok((jar, Redirect::to(&return_to)).into_response())
}

async fn delete_app_package(
    Path((slug, package_id)): Path<(String, Uuid)>,
    State(state): State<AppState>,
    jar: SignedCookieJar,
    Form(form): Form<HashMap<String, String>>,
) -> AppResult<Response> {
    let Some(user) = current_user(&state, &jar).await? else {
        let jar = jar.remove(Cookie::from(SESSION_COOKIE));
        return Ok((jar, Redirect::to("/login")).into_response());
    };
    if !is_writer(&user) {
        return Err(AppError::new(
            StatusCode::FORBIDDEN,
            "Writer permission required",
        ));
    }

    let slug = sanitize_slug(&slug);
    let return_to = safe_return_to(form.get("return_to").map(|s| s.as_str()));

    let record: Option<(Uuid, String)> = sqlx::query_as(
        r#"
        select p.pkg_file_id, f.local_path
        from apps a
        join app_packages p on p.app_id = a.id
        join cached_files f on f.id = p.pkg_file_id
        where a.slug = $1 and p.id = $2
        "#,
    )
    .bind(&slug)
    .bind(package_id)
    .fetch_optional(&state.pool)
    .await?;

    let Some((pkg_file_id, local_path)) = record else {
        let return_to = add_query_param(&return_to, "toast_error", "Package not found.");
        return Ok((jar, Redirect::to(&return_to)).into_response());
    };

    let deleted =
        sqlx::query_scalar::<_, bool>("delete from app_packages where id = $1 returning true")
            .bind(package_id)
            .fetch_optional(&state.pool)
            .await?
            .unwrap_or(false);
    if !deleted {
        let return_to = add_query_param(&return_to, "toast_error", "Package not found.");
        return Ok((jar, Redirect::to(&return_to)).into_response());
    }

    let still_referenced: bool = sqlx::query_scalar(
        r#"
        select
          exists(select 1 from app_packages where pkg_file_id = $1)
          or exists(select 1 from firmware_releases where file_id = $1)
          or exists(select 1 from apps where icon_file_id = $1)
        "#,
    )
    .bind(pkg_file_id)
    .fetch_one(&state.pool)
    .await?;

    if !still_referenced {
        if let Some(path) = resolve_cache_path(&state, &local_path).await? {
            let _ = tokio::fs::remove_file(path).await;
        }
        let _ = sqlx::query("delete from cached_files where id = $1")
            .bind(pkg_file_id)
            .execute(&state.pool)
            .await;
    }

    state.file_by_id_cache.invalidate_all();

    let return_to = add_query_param(&return_to, "toast_success", "Package deleted.");
    Ok((jar, Redirect::to(&return_to)).into_response())
}

async fn build_app_package(
    Path(slug): Path<String>,
    State(state): State<AppState>,
    jar: SignedCookieJar,
    mut multipart: Multipart,
) -> AppResult<Response> {
    let Some(user) = current_user(&state, &jar).await? else {
        let jar = jar.remove(Cookie::from(SESSION_COOKIE));
        return Ok((jar, Redirect::to("/login")).into_response());
    };
    if !is_writer(&user) {
        return Err(AppError::new(
            StatusCode::FORBIDDEN,
            "Writer permission required",
        ));
    }

    let slug = sanitize_slug(&slug);
    let app: (Uuid, String, Option<String>) =
        sqlx::query_as("select id, name, description from apps where slug = $1")
            .bind(&slug)
            .fetch_optional(&state.pool)
            .await?
            .ok_or_else(|| AppError::new(StatusCode::NOT_FOUND, "App not found"))?;
    let (app_id, app_name, app_description) = app;

    let mut version: Option<String> = None;
    let mut os: Option<String> = None;
    let mut arch: Option<String> = None;
    let mut display_name: Option<String> = None;
    let mut summary: Option<String> = None;
    let mut installer: Option<(String, Vec<u8>)> = None;
    let mut icon: Option<(String, Vec<u8>)> = None;
    let mut docs: Vec<(String, Vec<u8>)> = Vec::new();
    let mut return_to: Option<String> = None;

    while let Some(field) = multipart
        .next_field()
        .await
        .map_err(|_| AppError::new(StatusCode::BAD_REQUEST, "Invalid upload payload"))?
    {
        match field.name() {
            Some("version") => {
                let v = field
                    .text()
                    .await
                    .map_err(|_| AppError::new(StatusCode::BAD_REQUEST, "Invalid version"))?;
                let v = v.trim();
                if v.is_empty() {
                    return Err(AppError::new(
                        StatusCode::BAD_REQUEST,
                        "Version is required",
                    ));
                }
                version = Some(v.to_string());
            }
            Some("os") => {
                let v = field
                    .text()
                    .await
                    .map_err(|_| AppError::new(StatusCode::BAD_REQUEST, "Invalid os"))?;
                os = Some(normalize_os(v.trim())?);
            }
            Some("arch") => {
                let v = field
                    .text()
                    .await
                    .map_err(|_| AppError::new(StatusCode::BAD_REQUEST, "Invalid arch"))?;
                arch = Some(normalize_arch(v.trim())?);
            }
            Some("display_name") => {
                let v = field
                    .text()
                    .await
                    .map_err(|_| AppError::new(StatusCode::BAD_REQUEST, "Invalid display name"))?;
                let v = v.trim();
                if !v.is_empty() {
                    display_name = Some(v.to_string());
                }
            }
            Some("summary") => {
                let v = field
                    .text()
                    .await
                    .map_err(|_| AppError::new(StatusCode::BAD_REQUEST, "Invalid summary"))?;
                let v = v.trim();
                if !v.is_empty() {
                    summary = Some(v.to_string());
                }
            }
            Some("installer") => {
                let filename = field
                    .file_name()
                    .map(|s| sanitize_filename(s))
                    .unwrap_or_else(|| "installer.bin".to_string());
                let bytes = field.bytes().await.map_err(|_| {
                    AppError::new(StatusCode::BAD_REQUEST, "Failed reading installer")
                })?;
                let ext = std::path::Path::new(&filename)
                    .extension()
                    .and_then(|e| e.to_str())
                    .unwrap_or("")
                    .to_ascii_lowercase();
                let allowed = ["msi", "deb", "rpm", "exe", "dmg", "pkg", "zip", "tar", "gz"];
                if !allowed.iter().any(|a| a.eq_ignore_ascii_case(&ext)) {
                    return Err(AppError::new(
                        StatusCode::BAD_REQUEST,
                        "Installer must be .msi, .deb, .rpm, .exe, .dmg, .pkg, .zip, or .tar/.gz",
                    ));
                }
                installer = Some((filename, bytes.to_vec()));
            }
            Some("icon") => {
                if field.file_name().is_some() {
                    let filename = field
                        .file_name()
                        .map(|s| sanitize_filename(s))
                        .unwrap_or_else(|| "icon.png".to_string());
                    let ext = std::path::Path::new(&filename)
                        .extension()
                        .and_then(|e| e.to_str())
                        .unwrap_or("")
                        .to_ascii_lowercase();
                    if ext != "png" && ext != "svg" {
                        return Err(AppError::new(
                            StatusCode::BAD_REQUEST,
                            "Icon must be .png or .svg",
                        ));
                    }
                    let bytes = field.bytes().await.map_err(|_| {
                        AppError::new(StatusCode::BAD_REQUEST, "Failed reading icon")
                    })?;
                    icon = Some((filename, bytes.to_vec()));
                }
            }
            Some("docs") => {
                let filename = field
                    .file_name()
                    .map(|s| sanitize_filename(s))
                    .unwrap_or_else(|| "doc.txt".to_string());
                let bytes = field
                    .bytes()
                    .await
                    .map_err(|_| AppError::new(StatusCode::BAD_REQUEST, "Failed reading docs"))?;
                if !bytes.is_empty() {
                    docs.push((filename, bytes.to_vec()));
                }
            }
            Some("return_to") => {
                let v = field
                    .text()
                    .await
                    .map_err(|_| AppError::new(StatusCode::BAD_REQUEST, "Invalid return_to"))?;
                return_to = Some(v);
            }
            _ => {}
        }
    }

    let version =
        version.ok_or_else(|| AppError::new(StatusCode::BAD_REQUEST, "Version is required"))?;
    let os = os.ok_or_else(|| AppError::new(StatusCode::BAD_REQUEST, "OS is required"))?;
    let arch = arch.unwrap_or_else(|| "x86_64".to_string());
    let (installer_name, installer_bytes) =
        installer.ok_or_else(|| AppError::new(StatusCode::BAD_REQUEST, "Installer is required"))?;

    let display_name = display_name.unwrap_or_else(|| app_name.clone());
    let summary = summary
        .or(app_description)
        .unwrap_or_else(|| "Companion app installer".to_string());

    let work_dir = std::env::temp_dir().join(format!("pdes-cdn-pkg-{}", Uuid::new_v4()));
    tokio::fs::create_dir_all(work_dir.join("installers"))
        .await
        .map_err(|e| {
            AppError::new(
                StatusCode::INTERNAL_SERVER_ERROR,
                format!("Could not create temp directory: {}", e),
            )
        })?;
    tokio::fs::create_dir_all(work_dir.join("docs")).await.ok();

    let installer_path = work_dir.join("installers").join(&installer_name);
    tokio::fs::write(&installer_path, &installer_bytes)
        .await
        .map_err(|e| {
            AppError::new(
                StatusCode::INTERNAL_SERVER_ERROR,
                format!("Could not write installer: {}", e),
            )
        })?;

    let (icon_name, icon_path, should_store_app_icon) = if let Some((icon_name, icon_bytes)) = icon
    {
        let icon_path = work_dir.join(&icon_name);
        tokio::fs::write(&icon_path, &icon_bytes)
            .await
            .map_err(|e| {
                AppError::new(
                    StatusCode::INTERNAL_SERVER_ERROR,
                    format!("Could not write icon: {}", e),
                )
            })?;
        (icon_name, icon_path, true)
    } else {
        let existing_pkg_path: Option<String> = sqlx::query_scalar(
            r#"
            select cf.local_path
            from app_packages ap
            join cached_files cf on cf.id = ap.pkg_file_id
            where ap.app_id = $1 and ap.version = $2 and ap.os = $3 and ap.arch = $4
            "#,
        )
        .bind(app_id)
        .bind(&version)
        .bind(&os)
        .bind(&arch)
        .fetch_optional(&state.pool)
        .await?;

        let mut reused: Option<(PathBuf, String, bool)> = None;
        if let Some(pkg_path) = existing_pkg_path.as_deref() {
            if let Ok((tmp_path, icon_name)) =
                extract_icon_from_pkg(std::path::Path::new(pkg_path)).await
            {
                reused = Some((tmp_path, icon_name, true));
            }
        }

        if reused.is_none() {
            let app_icon: Option<(String, String)> = sqlx::query_as(
                r#"
                select cf.local_path, cf.filename
                from apps a
                join cached_files cf on cf.id = a.icon_file_id
                where a.id = $1
                "#,
            )
            .bind(app_id)
            .fetch_optional(&state.pool)
            .await?;
            if let Some((local_path, icon_name)) = app_icon {
                reused = Some((PathBuf::from(local_path), icon_name, false));
            }
        }

        let (src_path, icon_name, cleanup_src) = reused.ok_or_else(|| {
            AppError::new(
                StatusCode::BAD_REQUEST,
                "Icon is required (upload one, or reuse an existing package icon)",
            )
        })?;

        let icon_name = sanitize_filename(&icon_name);
        let ext = std::path::Path::new(&icon_name)
            .extension()
            .and_then(|e| e.to_str())
            .unwrap_or("")
            .to_ascii_lowercase();
        if ext != "png" && ext != "svg" {
            return Err(AppError::new(
                StatusCode::BAD_REQUEST,
                "Existing icon must be .png or .svg",
            ));
        }

        let icon_path = work_dir.join(&icon_name);
        tokio::fs::copy(&src_path, &icon_path).await.map_err(|e| {
            AppError::new(
                StatusCode::INTERNAL_SERVER_ERROR,
                format!("Could not reuse icon: {}", e),
            )
        })?;
        if cleanup_src {
            let _ = tokio::fs::remove_file(&src_path).await;
        }
        (icon_name, icon_path, false)
    };
    for (doc_name, doc_bytes) in &docs {
        let path = work_dir.join("docs").join(doc_name);
        let _ = tokio::fs::write(&path, doc_bytes).await;
    }

    let installer_type = std::path::Path::new(&installer_name)
        .extension()
        .and_then(|e| e.to_str())
        .unwrap_or("")
        .to_ascii_lowercase();
    let icon_path_in_pkg = icon_name.clone();

    let manifest = serde_json::json!({
        "schema_version": 1,
        "app_slug": &slug,
        "app_version": &version,
        "platform": { "os": &os, "arch": &arch },
        "display_name": &display_name,
        "summary": &summary,
        "icon": { "path": icon_path_in_pkg },
        "installer": { "type": installer_type, "path": format!("installers/{}", installer_name) },
        "firmware": { "channel": "stable", "latest_url": format!("/api/apps/{}/firmware/latest?channel=stable", slug) }
    })
    .to_string();
    tokio::fs::write(work_dir.join("manifest.json"), manifest)
        .await
        .map_err(|e| {
            AppError::new(
                StatusCode::INTERNAL_SERVER_ERROR,
                format!("Could not write manifest: {}", e),
            )
        })?;

    let pkg_filename = sanitize_filename(&format!("{}-{}-{}-{}.pkg", slug, version, os, arch));
    let pkg_out = std::env::temp_dir().join(format!("pdes-cdn-built-{}.pkg", Uuid::new_v4()));
    build_tar_gz(&work_dir, &pkg_out)
        .await
        .map_err(|message| AppError::new(StatusCode::INTERNAL_SERVER_ERROR, message))?;

    let prefix = format!(
        "apps/{}/packages/{}/{}/{}",
        sanitize_bucket(&slug),
        sanitize_bucket(&os),
        sanitize_bucket(&arch),
        sanitize_bucket(&version)
    );
    let stored = store_local_file_as_cached(&state, &pkg_out, &pkg_filename, &prefix).await?;

    if should_store_app_icon {
        let icon_prefix = format!("apps/{}/icon", sanitize_bucket(&slug));
        let icon_stored =
            store_local_file_as_cached(&state, &icon_path, &icon_name, &icon_prefix).await?;
        let _ = sqlx::query("update apps set icon_file_id = $1 where id = $2")
            .bind(icon_stored.id)
            .bind(app_id)
            .execute(&state.pool)
            .await;
    }

    let package_id = Uuid::new_v4();
    sqlx::query(
        r#"
        insert into app_packages (id, app_id, version, os, arch, pkg_file_id, pkg_sha256)
        values ($1, $2, $3, $4, $5, $6, $7)
        on conflict (app_id, version, os, arch)
        do update set pkg_file_id = excluded.pkg_file_id, pkg_sha256 = excluded.pkg_sha256, created_at = now()
        "#,
    )
    .bind(package_id)
    .bind(app_id)
    .bind(&version)
    .bind(&os)
    .bind(&arch)
    .bind(stored.id)
    .bind(&stored.sha256)
    .execute(&state.pool)
    .await?;

    let _ = tokio::fs::remove_dir_all(&work_dir).await;
    let _ = tokio::fs::remove_file(&pkg_out).await;
    state.file_by_id_cache.invalidate_all();
    let return_to = safe_return_to(return_to.as_deref());
    let return_to = add_query_param(&return_to, "toast_success", "Built package.");
    Ok((jar, Redirect::to(&return_to)).into_response())
}

async fn upload_firmware(
    Path(slug): Path<String>,
    State(state): State<AppState>,
    jar: SignedCookieJar,
    mut multipart: Multipart,
) -> AppResult<Response> {
    let Some(user) = current_user(&state, &jar).await? else {
        let jar = jar.remove(Cookie::from(SESSION_COOKIE));
        return Ok((jar, Redirect::to("/login")).into_response());
    };
    if !is_writer(&user) {
        return Err(AppError::new(
            StatusCode::FORBIDDEN,
            "Writer permission required",
        ));
    }
    let slug = sanitize_slug(&slug);

    let app_id: Uuid = sqlx::query_scalar("select id from apps where slug = $1")
        .bind(&slug)
        .fetch_optional(&state.pool)
        .await?
        .ok_or_else(|| AppError::new(StatusCode::NOT_FOUND, "App not found"))?;

    let mut version: Option<String> = None;
    let mut channel: String = "stable".to_string();
    let mut uploaded: Option<(Uuid, String)> = None;
    let mut return_to: Option<String> = None;

    while let Some(field) = multipart
        .next_field()
        .await
        .map_err(|_| AppError::new(StatusCode::BAD_REQUEST, "Invalid upload payload"))?
    {
        match field.name() {
            Some("version") => {
                let v = field
                    .text()
                    .await
                    .map_err(|_| AppError::new(StatusCode::BAD_REQUEST, "Invalid version"))?;
                let v = v.trim();
                if v.is_empty() {
                    return Err(AppError::new(
                        StatusCode::BAD_REQUEST,
                        "Version is required",
                    ));
                }
                version = Some(v.to_string());
            }
            Some("channel") => {
                let v = field
                    .text()
                    .await
                    .map_err(|_| AppError::new(StatusCode::BAD_REQUEST, "Invalid channel"))?;
                let v = sanitize_slug(v.trim());
                if !v.is_empty() {
                    channel = v;
                }
            }
            Some("file") => {
                let version = version
                    .clone()
                    .ok_or_else(|| AppError::new(StatusCode::BAD_REQUEST, "Version is required"))?;
                let prefix = format!(
                    "apps/{}/firmware/{}/{}",
                    sanitize_bucket(&slug),
                    sanitize_bucket(&channel),
                    sanitize_bucket(&version)
                );
                let result = save_upload_to_prefix(&state, field, &prefix, &["bin", "elf"]).await?;
                uploaded = Some((result.id, result.sha256));
            }
            Some("return_to") => {
                let v = field
                    .text()
                    .await
                    .map_err(|_| AppError::new(StatusCode::BAD_REQUEST, "Invalid return_to"))?;
                return_to = Some(v);
            }
            _ => {}
        }
    }

    let Some((file_id, sha256)) = uploaded else {
        return Err(AppError::new(StatusCode::BAD_REQUEST, "No file provided"));
    };
    let version =
        version.ok_or_else(|| AppError::new(StatusCode::BAD_REQUEST, "Version is required"))?;

    let release_id = Uuid::new_v4();
    sqlx::query(
        r#"
        insert into firmware_releases (id, app_id, channel, version, file_id, sha256)
        values ($1, $2, $3, $4, $5, $6)
        on conflict (app_id, channel, version)
        do update set file_id = excluded.file_id, sha256 = excluded.sha256, created_at = now()
        "#,
    )
    .bind(release_id)
    .bind(app_id)
    .bind(&channel)
    .bind(&version)
    .bind(file_id)
    .bind(&sha256)
    .execute(&state.pool)
    .await?;

    state.file_by_id_cache.invalidate_all();
    let return_to = safe_return_to(return_to.as_deref());
    let return_to = add_query_param(&return_to, "toast_success", "Uploaded firmware.");
    Ok((jar, Redirect::to(&return_to)).into_response())
}

async fn delete_firmware_release(
    Path((slug, firmware_id)): Path<(String, Uuid)>,
    State(state): State<AppState>,
    jar: SignedCookieJar,
    Form(form): Form<HashMap<String, String>>,
) -> AppResult<Response> {
    let Some(user) = current_user(&state, &jar).await? else {
        let jar = jar.remove(Cookie::from(SESSION_COOKIE));
        return Ok((jar, Redirect::to("/login")).into_response());
    };
    if !is_writer(&user) {
        return Err(AppError::new(
            StatusCode::FORBIDDEN,
            "Writer permission required",
        ));
    }

    let slug = sanitize_slug(&slug);
    let return_to = safe_return_to(form.get("return_to").map(|s| s.as_str()));

    let record: Option<(Uuid, String)> = sqlx::query_as(
        r#"
        select f.file_id, cf.local_path
        from apps a
        join firmware_releases f on f.app_id = a.id
        join cached_files cf on cf.id = f.file_id
        where a.slug = $1 and f.id = $2
        "#,
    )
    .bind(&slug)
    .bind(firmware_id)
    .fetch_optional(&state.pool)
    .await?;

    let Some((file_id, local_path)) = record else {
        let return_to = add_query_param(&return_to, "toast_error", "Firmware not found.");
        return Ok((jar, Redirect::to(&return_to)).into_response());
    };

    let deleted =
        sqlx::query_scalar::<_, bool>("delete from firmware_releases where id = $1 returning true")
            .bind(firmware_id)
            .fetch_optional(&state.pool)
            .await?
            .unwrap_or(false);
    if !deleted {
        let return_to = add_query_param(&return_to, "toast_error", "Firmware not found.");
        return Ok((jar, Redirect::to(&return_to)).into_response());
    }

    let still_referenced: bool = sqlx::query_scalar(
        r#"
        select
          exists(select 1 from app_packages where pkg_file_id = $1)
          or exists(select 1 from firmware_releases where file_id = $1)
          or exists(select 1 from apps where icon_file_id = $1)
        "#,
    )
    .bind(file_id)
    .fetch_one(&state.pool)
    .await?;

    if !still_referenced {
        if let Some(path) = resolve_cache_path(&state, &local_path).await? {
            let _ = tokio::fs::remove_file(path).await;
        }
        let _ = sqlx::query("delete from cached_files where id = $1")
            .bind(file_id)
            .execute(&state.pool)
            .await;
    }

    state.file_by_id_cache.invalidate_all();
    let return_to = add_query_param(&return_to, "toast_success", "Firmware deleted.");
    Ok((jar, Redirect::to(&return_to)).into_response())
}

async fn list_tokens(State(state): State<AppState>, jar: SignedCookieJar) -> AppResult<Response> {
    let Some(user) = current_user(&state, &jar).await? else {
        let jar = jar.remove(Cookie::from(SESSION_COOKIE));
        return Ok((jar, Redirect::to("/login")).into_response());
    };

    let mut ctx = TeraContext::new();
    ctx.insert("user_email", &user.email);
    ctx.insert("can_write", &is_writer(&user));
    populate_tokens_context(&state, &user, &mut ctx).await?;
    let html = render_template(&state, "tokens.html", ctx)?;
    Ok((jar, html).into_response())
}

async fn create_token(
    State(state): State<AppState>,
    jar: SignedCookieJar,
    Form(form): Form<HashMap<String, String>>,
) -> AppResult<Response> {
    let Some(user) = current_user(&state, &jar).await? else {
        let jar = jar.remove(Cookie::from(SESSION_COOKIE));
        return Ok((jar, Redirect::to("/login")).into_response());
    };

    let name = form
        .get("name")
        .map(|s| s.trim())
        .filter(|s| !s.is_empty())
        .unwrap_or("Desktop app")
        .to_string();

    let token = mint_api_token_for_user(&state, user.id, &name).await?;

    let mut ctx = TeraContext::new();
    ctx.insert("user_email", &user.email);
    ctx.insert("new_token_name", &name);
    ctx.insert("new_token", &token);
    ctx.insert("can_write", &is_writer(&user));
    populate_tokens_context(&state, &user, &mut ctx).await?;
    ctx.insert(
        "toast_success",
        "Created API token. Copy it now; it won’t be shown again.",
    );
    let html = render_template(&state, "tokens.html", ctx)?;
    Ok((jar, html).into_response())
}

async fn mint_api_token_for_user(state: &AppState, user_id: Uuid, name: &str) -> AppResult<String> {
    let mut token_bytes = [0u8; 32];
    OsRng.fill_bytes(&mut token_bytes);
    let token = to_hex(&token_bytes);
    let token_sha256 = sha256_hex(token.as_bytes());

    let id = Uuid::new_v4();
    sqlx::query("insert into api_tokens (id, user_id, name, token_sha256) values ($1, $2, $3, $4)")
        .bind(id)
        .bind(user_id)
        .bind(name)
        .bind(&token_sha256)
        .execute(&state.pool)
        .await?;

    Ok(token)
}

async fn revoke_token(
    Path(id): Path<Uuid>,
    State(state): State<AppState>,
    jar: SignedCookieJar,
) -> AppResult<Response> {
    let Some(user) = current_user(&state, &jar).await? else {
        let jar = jar.remove(Cookie::from(SESSION_COOKIE));
        return Ok((jar, Redirect::to("/login")).into_response());
    };

    sqlx::query("delete from api_tokens where id = $1 and user_id = $2")
        .bind(id)
        .bind(user.id)
        .execute(&state.pool)
        .await?;

    Ok((jar, Redirect::to("/tokens")).into_response())
}

async fn create_app_upload_token(
    State(state): State<AppState>,
    jar: SignedCookieJar,
    Form(form): Form<CreateAppUploadTokenForm>,
) -> AppResult<Response> {
    let Some(user) = current_user(&state, &jar).await? else {
        let jar = jar.remove(Cookie::from(SESSION_COOKIE));
        return Ok((jar, Redirect::to("/login")).into_response());
    };
    if !is_writer(&user) {
        return Err(AppError::new(
            StatusCode::FORBIDDEN,
            "Writer permission required",
        ));
    }

    let app_slug = sanitize_slug(form.app_slug.trim());
    if app_slug.is_empty() {
        return Err(AppError::new(StatusCode::BAD_REQUEST, "App is required"));
    }
    let app_id: Uuid = sqlx::query_scalar("select id from apps where slug = $1")
        .bind(&app_slug)
        .fetch_optional(&state.pool)
        .await?
        .ok_or_else(|| AppError::new(StatusCode::NOT_FOUND, "App not found"))?;

    let name = form.name.trim();
    let name = if name.is_empty() {
        format!("CI upload ({})", app_slug)
    } else {
        name.to_string()
    };

    let expires_at = form
        .expires_days
        .map(|d| d.clamp(1, 3650))
        .map(|d| Utc::now() + ChronoDuration::days(d));

    let mut token_bytes = [0u8; 32];
    OsRng.fill_bytes(&mut token_bytes);
    let token = to_hex(&token_bytes);
    let token_sha256 = sha256_hex(token.as_bytes());

    let id = Uuid::new_v4();
    sqlx::query(
        r#"
        insert into app_upload_tokens (id, app_id, name, token_sha256, expires_at)
        values ($1, $2, $3, $4, $5)
        "#,
    )
    .bind(id)
    .bind(app_id)
    .bind(&name)
    .bind(&token_sha256)
    .bind(expires_at)
    .execute(&state.pool)
    .await?;

    let mut ctx = TeraContext::new();
    ctx.insert("user_email", &user.email);
    ctx.insert("can_write", &true);
    populate_tokens_context(&state, &user, &mut ctx).await?;
    ctx.insert("new_app_upload_token_app_slug", &app_slug);
    ctx.insert("new_app_upload_token_name", &name);
    ctx.insert("new_app_upload_token", &token);
    ctx.insert(
        "toast_success",
        "Created app upload token. Copy it now; it won’t be shown again.",
    );
    let html = render_template(&state, "tokens.html", ctx)?;
    Ok((jar, html).into_response())
}

async fn revoke_app_upload_token(
    Path(id): Path<Uuid>,
    State(state): State<AppState>,
    jar: SignedCookieJar,
) -> AppResult<Response> {
    let Some(user) = current_user(&state, &jar).await? else {
        let jar = jar.remove(Cookie::from(SESSION_COOKIE));
        return Ok((jar, Redirect::to("/login")).into_response());
    };
    if !is_writer(&user) {
        return Err(AppError::new(
            StatusCode::FORBIDDEN,
            "Writer permission required",
        ));
    }

    let _ = sqlx::query("update app_upload_tokens set revoked_at = now() where id = $1")
        .bind(id)
        .execute(&state.pool)
        .await;

    Ok((jar, Redirect::to("/tokens")).into_response())
}

async fn populate_tokens_context(
    state: &AppState,
    user: &User,
    ctx: &mut TeraContext,
) -> AppResult<()> {
    ctx.insert("app_upload_tokens_missing", &false);

    let tokens = sqlx::query_as::<_, ApiTokenRow>(
        "select id, user_id, name, created_at, last_used_at from api_tokens where user_id = $1 order by created_at desc",
    )
    .bind(user.id)
    .fetch_all(&state.pool)
    .await?;
    ctx.insert("tokens", &tokens);

    if is_writer(user) {
        let apps = sqlx::query_as::<_, AppOptionRow>("select slug, name from apps order by name")
            .fetch_all(&state.pool)
            .await?;
        ctx.insert("apps", &apps);

        let app_upload_tokens = match sqlx::query_as::<_, AppUploadTokenRow>(
            r#"
            select t.id,
                   a.slug as app_slug,
                   t.name,
                   t.created_at,
                   t.last_used_at,
                   t.revoked_at,
                   t.expires_at
            from app_upload_tokens t
            join apps a on a.id = t.app_id
            order by t.created_at desc
            "#,
        )
        .fetch_all(&state.pool)
        .await
        {
            Ok(rows) => rows,
            Err(sqlx::Error::Database(db))
                if db.code().as_deref() == Some("42P01")
                    && db.message().contains("app_upload_tokens") =>
            {
                ctx.insert("app_upload_tokens_missing", &true);
                Vec::new()
            }
            Err(e) => return Err(e.into()),
        };
        ctx.insert("app_upload_tokens", &app_upload_tokens);
    }

    Ok(())
}

async fn stream_cached_file(
    state: &AppState,
    id: Uuid,
    jar: SignedCookieJar,
) -> AppResult<Response> {
    let file = fetch_file_by_id(state, id).await?;
    let Some(file) = file else {
        return Err(AppError::new(StatusCode::NOT_FOUND, "File not found"));
    };

    let path = state.cache_dir.join(&file.local_path);
    let path = tokio::fs::canonicalize(path)
        .await
        .map_err(|_| AppError::new(StatusCode::NOT_FOUND, "File missing on disk"))?;
    if !path.starts_with(&state.cache_dir) {
        return Err(AppError::new(
            StatusCode::BAD_REQUEST,
            "Invalid file location",
        ));
    }

    let file_handle = File::open(&path)
        .await
        .map_err(|_| AppError::new(StatusCode::NOT_FOUND, "File missing on disk"))?;
    let stream = ReaderStream::new(file_handle);
    let response = Response::builder()
        .status(StatusCode::OK)
        .header(header::CONTENT_TYPE, "application/octet-stream")
        .header(
            header::CONTENT_DISPOSITION,
            format!("attachment; filename=\"{}\"", file.filename),
        )
        .body(Body::from_stream(stream))
        .map_err(AppError::from)?;

    Ok((jar, response).into_response())
}

async fn api_user_from_bearer(state: &AppState, headers: &HeaderMap) -> AppResult<(User, Uuid)> {
    let token = extract_bearer(headers)
        .ok_or_else(|| AppError::new_plain(StatusCode::UNAUTHORIZED, "Bearer token required"))?;
    if token.len() < 24 {
        return Err(AppError::new_plain(
            StatusCode::UNAUTHORIZED,
            "Invalid bearer token",
        ));
    }
    let token_sha256 = sha256_hex(token.as_bytes());
    let record = sqlx::query_as::<_, ApiUserAuthRow>(
        r#"
        select t.id as token_id,
               u.id,
               u.email,
               u.password_hash,
               u.permission,
               u.reset_token,
               u.reset_token_expires,
               u.must_reset
        from api_tokens t
        join users u on u.id = t.user_id
        where t.token_sha256 = $1
        "#,
    )
    .bind(&token_sha256)
    .fetch_optional(&state.pool)
    .await?;

    let Some(row) = record else {
        return Err(AppError::new_plain(
            StatusCode::UNAUTHORIZED,
            "Invalid bearer token",
        ));
    };
    let token_id = row.token_id;
    let user = User {
        id: row.id,
        email: row.email,
        password_hash: row.password_hash,
        permission: row.permission,
        reset_token: row.reset_token,
        reset_token_expires: row.reset_token_expires,
        must_reset: row.must_reset,
    };

    let _ = sqlx::query("update api_tokens set last_used_at = now() where id = $1")
        .bind(token_id)
        .execute(&state.pool)
        .await;

    Ok((user, token_id))
}

async fn api_principal_from_bearer(
    state: &AppState,
    headers: &HeaderMap,
) -> AppResult<ApiPrincipal> {
    let token = extract_bearer(headers)
        .ok_or_else(|| AppError::new_plain(StatusCode::UNAUTHORIZED, "Bearer token required"))?;
    if token.len() < 24 {
        return Err(AppError::new_plain(
            StatusCode::UNAUTHORIZED,
            "Invalid bearer token",
        ));
    }
    let token_sha256 = sha256_hex(token.as_bytes());

    let user_record = sqlx::query_as::<_, ApiUserAuthRow>(
        r#"
        select t.id as token_id,
               u.id,
               u.email,
               u.password_hash,
               u.permission,
               u.reset_token,
               u.reset_token_expires,
               u.must_reset
        from api_tokens t
        join users u on u.id = t.user_id
        where t.token_sha256 = $1
        "#,
    )
    .bind(&token_sha256)
    .fetch_optional(&state.pool)
    .await?;

    if let Some(row) = user_record {
        let token_id = row.token_id;
        let user = User {
            id: row.id,
            email: row.email,
            password_hash: row.password_hash,
            permission: row.permission,
            reset_token: row.reset_token,
            reset_token_expires: row.reset_token_expires,
            must_reset: row.must_reset,
        };
        let _ = sqlx::query("update api_tokens set last_used_at = now() where id = $1")
            .bind(token_id)
            .execute(&state.pool)
            .await;
        return Ok(ApiPrincipal::User(user));
    }

    let device_record = sqlx::query_as::<_, DeviceTokenAuthRow>(
        r#"
        select d.id as token_id,
               d.app_id,
               d.os,
               d.arch
        from device_tokens d
        where d.token_sha256 = $1
          and d.revoked_at is null
          and (d.expires_at is null or d.expires_at > now())
        "#,
    )
    .bind(&token_sha256)
    .fetch_optional(&state.pool)
    .await?;

    let Some(row) = device_record else {
        return Err(AppError::new_plain(
            StatusCode::UNAUTHORIZED,
            "Invalid bearer token",
        ));
    };

    let _ = sqlx::query("update device_tokens set last_used_at = now() where id = $1")
        .bind(row.token_id)
        .execute(&state.pool)
        .await;

    Ok(ApiPrincipal::Device(DevicePrincipal {
        app_id: row.app_id,
        os: row.os,
        arch: row.arch,
    }))
}

async fn api_latest_package(
    Path(slug): Path<String>,
    State(state): State<AppState>,
    headers: HeaderMap,
    Query(query): Query<HashMap<String, String>>,
) -> AppResult<Response> {
    let principal = api_principal_from_bearer(&state, &headers).await?;
    let slug = sanitize_slug(&slug);
    let (os, arch) = match &principal {
        ApiPrincipal::User(_) => parse_target_query(&query)
            .or_else(|| detect_target_from_user_agent(&headers))
            .unwrap_or_else(|| ("windows".to_string(), "x86_64".to_string())),
        ApiPrincipal::Device(d) => parse_target_query(&query)
            .or_else(|| detect_target_from_user_agent(&headers))
            .unwrap_or_else(|| (d.os.clone(), d.arch.clone())),
    };

    let record: Option<(String, String, String, String, Uuid)> = match principal {
        ApiPrincipal::User(user) if is_writer(&user) => {
            sqlx::query_as::<_, (String, String, String, String, Uuid)>(
                r#"
                select p.version, p.os, p.arch, p.pkg_sha256, p.pkg_file_id
                from apps a
                join app_packages p on p.app_id = a.id
                where a.slug = $1 and p.os = $2 and (p.arch = $3 or p.arch = 'any')
                order by p.created_at desc, (p.arch = $3) desc
                limit 1
                "#,
            )
            .bind(&slug)
            .bind(&os)
            .bind(&arch)
            .fetch_optional(&state.pool)
            .await?
        }
        ApiPrincipal::User(user) => {
            sqlx::query_as::<_, (String, String, String, String, Uuid)>(
                r#"
                select p.version, p.os, p.arch, p.pkg_sha256, p.pkg_file_id
                from apps a
                join app_assignments aa on aa.app_id = a.id
                join app_packages p on p.app_id = a.id
                where a.slug = $1 and aa.user_id = $2 and p.os = $3 and (p.arch = $4 or p.arch = 'any')
                order by p.created_at desc, (p.arch = $4) desc
                limit 1
                "#,
            )
            .bind(&slug)
            .bind(user.id)
            .bind(&os)
            .bind(&arch)
            .fetch_optional(&state.pool)
            .await?
        }
        ApiPrincipal::Device(device) => {
            if os != device.os || arch != device.arch {
                return Err(AppError::new_plain(
                    StatusCode::FORBIDDEN,
                    "Token is not valid for this target",
                ));
            }
            sqlx::query_as::<_, (String, String, String, String, Uuid)>(
                r#"
                select p.version, p.os, p.arch, p.pkg_sha256, p.pkg_file_id
                from apps a
                join app_packages p on p.app_id = a.id
                where a.slug = $1 and a.id = $2 and p.os = $3 and (p.arch = $4 or p.arch = 'any')
                order by p.created_at desc, (p.arch = $4) desc
                limit 1
                "#,
            )
            .bind(&slug)
            .bind(device.app_id)
            .bind(&os)
            .bind(&arch)
            .fetch_optional(&state.pool)
            .await?
        }
    };

    let Some((version, os, arch, pkg_sha256, _pkg_file_id)) = record else {
        return Err(AppError::new_plain(
            StatusCode::NOT_FOUND,
            "Package not found",
        ));
    };

    let payload = serde_json::json!({
        "app_slug": slug,
        "os": os,
        "arch": arch,
        "version": version,
        "pkg_sha256": pkg_sha256,
        "download_url": format!("/api/apps/{}/package/download?os={}&arch={}", slug, os, arch),
    });
    Ok(axum::Json(payload).into_response())
}

async fn api_download_package(
    Path(slug): Path<String>,
    State(state): State<AppState>,
    headers: HeaderMap,
    Query(query): Query<HashMap<String, String>>,
) -> AppResult<Response> {
    let principal = api_principal_from_bearer(&state, &headers).await?;
    let slug = sanitize_slug(&slug);
    let (os, arch) = match &principal {
        ApiPrincipal::User(_) => parse_target_query(&query)
            .or_else(|| detect_target_from_user_agent(&headers))
            .unwrap_or_else(|| ("windows".to_string(), "x86_64".to_string())),
        ApiPrincipal::Device(d) => parse_target_query(&query)
            .or_else(|| detect_target_from_user_agent(&headers))
            .unwrap_or_else(|| (d.os.clone(), d.arch.clone())),
    };

    let version_filter = query
        .get("version")
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty());

    let file_id: Option<Uuid> = match principal {
        ApiPrincipal::User(user) if is_writer(&user) => match version_filter {
            Some(version) => {
                sqlx::query_scalar::<_, Uuid>(
                    r#"
                    select p.pkg_file_id
                    from apps a
                    join app_packages p on p.app_id = a.id
                    where a.slug = $1 and p.os = $2 and (p.arch = $3 or p.arch = 'any') and p.version = $4
                    order by p.created_at desc, (p.arch = $3) desc
                    limit 1
                    "#,
                )
                .bind(&slug)
                .bind(&os)
                .bind(&arch)
                .bind(&version)
                .fetch_optional(&state.pool)
                .await?
            }
            None => {
                sqlx::query_scalar::<_, Uuid>(
                    r#"
                    select p.pkg_file_id
                    from apps a
                    join app_packages p on p.app_id = a.id
                    where a.slug = $1 and p.os = $2 and (p.arch = $3 or p.arch = 'any')
                    order by p.created_at desc, (p.arch = $3) desc
                    limit 1
                    "#,
                )
                .bind(&slug)
                .bind(&os)
                .bind(&arch)
                .fetch_optional(&state.pool)
                .await?
            }
        },
        ApiPrincipal::User(user) => match version_filter {
            Some(version) => {
                sqlx::query_scalar::<_, Uuid>(
                    r#"
                    select p.pkg_file_id
                    from apps a
                    join app_assignments aa on aa.app_id = a.id
                    join app_packages p on p.app_id = a.id
                    where a.slug = $1 and aa.user_id = $2 and p.os = $3 and (p.arch = $4 or p.arch = 'any') and p.version = $5
                    order by p.created_at desc, (p.arch = $4) desc
                    limit 1
                    "#,
                )
                .bind(&slug)
                .bind(user.id)
                .bind(&os)
                .bind(&arch)
                .bind(&version)
                .fetch_optional(&state.pool)
                .await?
            }
            None => {
                sqlx::query_scalar::<_, Uuid>(
                    r#"
                    select p.pkg_file_id
                    from apps a
                    join app_assignments aa on aa.app_id = a.id
                    join app_packages p on p.app_id = a.id
                    where a.slug = $1 and aa.user_id = $2 and p.os = $3 and (p.arch = $4 or p.arch = 'any')
                    order by p.created_at desc, (p.arch = $4) desc
                    limit 1
                    "#,
                )
                .bind(&slug)
                .bind(user.id)
                .bind(&os)
                .bind(&arch)
                .fetch_optional(&state.pool)
                .await?
            }
        },
        ApiPrincipal::Device(device) => {
            if os != device.os || arch != device.arch {
                return Err(AppError::new_plain(
                    StatusCode::FORBIDDEN,
                    "Token is not valid for this target",
                ));
            }
            match version_filter {
                Some(version) => {
                    sqlx::query_scalar::<_, Uuid>(
                        r#"
                        select p.pkg_file_id
                        from apps a
                        join app_packages p on p.app_id = a.id
                        where a.slug = $1 and a.id = $2 and p.os = $3 and (p.arch = $4 or p.arch = 'any') and p.version = $5
                        order by p.created_at desc, (p.arch = $4) desc
                        limit 1
                        "#,
                    )
                    .bind(&slug)
                    .bind(device.app_id)
                    .bind(&os)
                    .bind(&arch)
                    .bind(&version)
                    .fetch_optional(&state.pool)
                    .await?
                }
                None => {
                    sqlx::query_scalar::<_, Uuid>(
                        r#"
                        select p.pkg_file_id
                        from apps a
                        join app_packages p on p.app_id = a.id
                        where a.slug = $1 and a.id = $2 and p.os = $3 and (p.arch = $4 or p.arch = 'any')
                        order by p.created_at desc, (p.arch = $4) desc
                        limit 1
                        "#,
                    )
                    .bind(&slug)
                    .bind(device.app_id)
                    .bind(&os)
                    .bind(&arch)
                    .fetch_optional(&state.pool)
                    .await?
                }
            }
        }
    };

    let Some(file_id) = file_id else {
        return Err(AppError::new_plain(
            StatusCode::NOT_FOUND,
            "Package not found",
        ));
    };

    let file = fetch_file_by_id(&state, file_id).await?;
    let Some(file) = file else {
        return Err(AppError::new_plain(StatusCode::NOT_FOUND, "File not found"));
    };

    let path = state.cache_dir.join(&file.local_path);
    let path = tokio::fs::canonicalize(path)
        .await
        .map_err(|_| AppError::new_plain(StatusCode::NOT_FOUND, "File missing on disk"))?;
    if !path.starts_with(&state.cache_dir) {
        return Err(AppError::new_plain(
            StatusCode::BAD_REQUEST,
            "Invalid file location",
        ));
    }

    let file_handle = File::open(&path)
        .await
        .map_err(|_| AppError::new_plain(StatusCode::NOT_FOUND, "File missing on disk"))?;
    let stream = ReaderStream::new(file_handle);
    let response = Response::builder()
        .status(StatusCode::OK)
        .header(header::CONTENT_TYPE, "application/octet-stream")
        .header(
            header::CONTENT_DISPOSITION,
            format!("attachment; filename=\"{}\"", file.filename),
        )
        .body(Body::from_stream(stream))
        .map_err(AppError::from)?;

    Ok(response)
}

async fn api_upload_app_package(
    Path(slug): Path<String>,
    State(state): State<AppState>,
    headers: HeaderMap,
    mut multipart: Multipart,
) -> AppResult<Response> {
    let token = extract_bearer(&headers)
        .ok_or_else(|| AppError::new_plain(StatusCode::UNAUTHORIZED, "Bearer token required"))?;
    if token.len() < 24 {
        return Err(AppError::new_plain(
            StatusCode::UNAUTHORIZED,
            "Invalid bearer token",
        ));
    }
    let token_sha256 = sha256_hex(token.as_bytes());

    let mut is_writer_user_token = false;
    let mut app_upload_token_id: Option<Uuid> = None;
    let mut app_upload_token_app_id: Option<Uuid> = None;

    let user_record = sqlx::query_as::<_, ApiUserAuthRow>(
        r#"
        select t.id as token_id,
               u.id,
               u.email,
               u.password_hash,
               u.permission,
               u.reset_token,
               u.reset_token_expires,
               u.must_reset
        from api_tokens t
        join users u on u.id = t.user_id
        where t.token_sha256 = $1
        "#,
    )
    .bind(&token_sha256)
    .fetch_optional(&state.pool)
    .await?;

    if let Some(row) = user_record {
        let user = User {
            id: row.id,
            email: row.email,
            password_hash: row.password_hash,
            permission: row.permission,
            reset_token: row.reset_token,
            reset_token_expires: row.reset_token_expires,
            must_reset: row.must_reset,
        };
        if !is_writer(&user) {
            return Err(AppError::new_plain(
                StatusCode::FORBIDDEN,
                "Writer permission required",
            ));
        }
        is_writer_user_token = true;
        let _ = sqlx::query("update api_tokens set last_used_at = now() where id = $1")
            .bind(row.token_id)
            .execute(&state.pool)
            .await;
    } else {
        let record: Option<(Uuid, Uuid)> = sqlx::query_as(
            r#"
            select id, app_id
            from app_upload_tokens
            where token_sha256 = $1
              and revoked_at is null
              and (expires_at is null or expires_at > now())
            "#,
        )
        .bind(&token_sha256)
        .fetch_optional(&state.pool)
        .await?;

        let Some((token_id, app_id)) = record else {
            return Err(AppError::new_plain(
                StatusCode::UNAUTHORIZED,
                "Invalid bearer token",
            ));
        };
        app_upload_token_id = Some(token_id);
        app_upload_token_app_id = Some(app_id);
        let _ = sqlx::query("update app_upload_tokens set last_used_at = now() where id = $1")
            .bind(token_id)
            .execute(&state.pool)
            .await;
    }

    let slug = sanitize_slug(&slug);
    let app_id: Uuid = sqlx::query_scalar("select id from apps where slug = $1")
        .bind(&slug)
        .fetch_optional(&state.pool)
        .await?
        .ok_or_else(|| AppError::new_plain(StatusCode::NOT_FOUND, "App not found"))?;

    if !is_writer_user_token {
        let Some(scoped_app_id) = app_upload_token_app_id else {
            return Err(AppError::new_plain(
                StatusCode::UNAUTHORIZED,
                "Invalid bearer token",
            ));
        };
        if scoped_app_id != app_id {
            return Err(AppError::new_plain(
                StatusCode::FORBIDDEN,
                "Token is not valid for this app",
            ));
        }
    }

    let mut version: Option<String> = None;
    let mut os: Option<String> = None;
    let mut arch: Option<String> = None;
    let mut uploaded: Option<(Uuid, String)> = None;

    while let Some(field) = multipart
        .next_field()
        .await
        .map_err(|_| AppError::new_plain(StatusCode::BAD_REQUEST, "Invalid upload payload"))?
    {
        match field.name() {
            Some("os") => {
                let v = field
                    .text()
                    .await
                    .map_err(|_| AppError::new_plain(StatusCode::BAD_REQUEST, "Invalid os"))?;
                os = Some(normalize_os(v.trim())?);
            }
            Some("arch") => {
                let v = field
                    .text()
                    .await
                    .map_err(|_| AppError::new_plain(StatusCode::BAD_REQUEST, "Invalid arch"))?;
                arch = Some(normalize_arch(v.trim())?);
            }
            Some("version") => {
                let v = field
                    .text()
                    .await
                    .map_err(|_| AppError::new_plain(StatusCode::BAD_REQUEST, "Invalid version"))?;
                let v = v.trim();
                if v.is_empty() {
                    return Err(AppError::new_plain(
                        StatusCode::BAD_REQUEST,
                        "Version is required",
                    ));
                }
                version = Some(v.to_string());
            }
            Some("file") => {
                let version = version.clone().ok_or_else(|| {
                    AppError::new_plain(StatusCode::BAD_REQUEST, "Version is required")
                })?;
                let os = os.clone().ok_or_else(|| {
                    AppError::new_plain(StatusCode::BAD_REQUEST, "OS is required")
                })?;
                let arch = arch.clone().unwrap_or_else(|| "x86_64".to_string());
                let prefix = format!(
                    "apps/{}/packages/{}/{}/{}",
                    sanitize_bucket(&slug),
                    sanitize_bucket(&os),
                    sanitize_bucket(&arch),
                    sanitize_bucket(&version)
                );
                let result = save_upload_to_prefix(&state, field, &prefix, &["pkg"]).await?;
                uploaded = Some((result.id, result.sha256));
            }
            _ => {}
        }
    }

    let Some((pkg_file_id, pkg_sha256)) = uploaded else {
        return Err(AppError::new_plain(
            StatusCode::BAD_REQUEST,
            "No file provided",
        ));
    };
    let version = version
        .ok_or_else(|| AppError::new_plain(StatusCode::BAD_REQUEST, "Version is required"))?;
    let os = os.ok_or_else(|| AppError::new_plain(StatusCode::BAD_REQUEST, "OS is required"))?;
    let arch = arch.unwrap_or_else(|| "x86_64".to_string());

    let package_id = Uuid::new_v4();
    sqlx::query(
        r#"
        insert into app_packages (id, app_id, version, os, arch, pkg_file_id, pkg_sha256)
        values ($1, $2, $3, $4, $5, $6, $7)
        on conflict (app_id, version, os, arch)
        do update set pkg_file_id = excluded.pkg_file_id, pkg_sha256 = excluded.pkg_sha256, created_at = now()
        "#,
    )
    .bind(package_id)
    .bind(app_id)
    .bind(&version)
    .bind(&os)
    .bind(&arch)
    .bind(pkg_file_id)
    .bind(&pkg_sha256)
    .execute(&state.pool)
    .await?;

    state.file_by_id_cache.invalidate_all();

    let payload = serde_json::json!({
        "app_slug": slug,
        "os": os,
        "arch": arch,
        "version": version,
        "pkg_file_id": pkg_file_id,
        "pkg_sha256": pkg_sha256,
        "auth": if is_writer_user_token { "writer_api_token" } else { "app_upload_token" },
        "app_upload_token_id": app_upload_token_id,
    });
    Ok(axum::Json(payload).into_response())
}

async fn api_latest_firmware(
    Path(slug): Path<String>,
    State(state): State<AppState>,
    headers: HeaderMap,
    Query(query): Query<HashMap<String, String>>,
) -> AppResult<Response> {
    let principal = api_principal_from_bearer(&state, &headers).await?;
    let slug = sanitize_slug(&slug);
    let channel = query
        .get("channel")
        .map(|s| s.trim())
        .filter(|s| !s.is_empty())
        .map(sanitize_slug)
        .filter(|s| !s.is_empty())
        .unwrap_or_else(|| "stable".to_string());

    let record: Option<(String, String)> = match principal {
        ApiPrincipal::User(user) if is_writer(&user) => {
            sqlx::query_as::<_, (String, String)>(
                r#"
                select f.version, f.sha256
                from apps a
                join firmware_releases f on f.app_id = a.id
                where a.slug = $1 and f.channel = $2
                order by f.created_at desc
                limit 1
                "#,
            )
            .bind(&slug)
            .bind(&channel)
            .fetch_optional(&state.pool)
            .await?
        }
        ApiPrincipal::User(user) => {
            sqlx::query_as::<_, (String, String)>(
                r#"
                select f.version, f.sha256
                from apps a
                join app_assignments aa on aa.app_id = a.id
                join firmware_releases f on f.app_id = a.id
                where a.slug = $1 and aa.user_id = $2 and f.channel = $3
                order by f.created_at desc
                limit 1
                "#,
            )
            .bind(&slug)
            .bind(user.id)
            .bind(&channel)
            .fetch_optional(&state.pool)
            .await?
        }
        ApiPrincipal::Device(device) => {
            sqlx::query_as::<_, (String, String)>(
                r#"
                select f.version, f.sha256
                from apps a
                join firmware_releases f on f.app_id = a.id
                where a.slug = $1 and a.id = $2 and f.channel = $3
                order by f.created_at desc
                limit 1
                "#,
            )
            .bind(&slug)
            .bind(device.app_id)
            .bind(&channel)
            .fetch_optional(&state.pool)
            .await?
        }
    };

    let Some((version, sha256)) = record else {
        return Err(AppError::new_plain(
            StatusCode::NOT_FOUND,
            "Firmware not found",
        ));
    };

    let payload = serde_json::json!({
        "app_slug": slug,
        "channel": channel,
        "version": version,
        "sha256": sha256,
        "download_url": format!("/api/apps/{}/firmware/download?channel={}", slug, channel),
    });
    Ok(axum::Json(payload).into_response())
}

async fn api_download_firmware(
    Path(slug): Path<String>,
    State(state): State<AppState>,
    headers: HeaderMap,
    Query(query): Query<HashMap<String, String>>,
) -> AppResult<Response> {
    let principal = api_principal_from_bearer(&state, &headers).await?;
    let slug = sanitize_slug(&slug);
    let channel = query
        .get("channel")
        .map(|s| s.trim())
        .filter(|s| !s.is_empty())
        .map(sanitize_slug)
        .filter(|s| !s.is_empty())
        .unwrap_or_else(|| "stable".to_string());

    let version_filter = query
        .get("version")
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty());

    let file_id: Option<Uuid> = match principal {
        ApiPrincipal::User(user) if is_writer(&user) => match version_filter {
            Some(version) => {
                sqlx::query_scalar::<_, Uuid>(
                    r#"
                    select f.file_id
                    from apps a
                    join firmware_releases f on f.app_id = a.id
                    where a.slug = $1 and f.channel = $2 and f.version = $3
                    order by f.created_at desc
                    limit 1
                    "#,
                )
                .bind(&slug)
                .bind(&channel)
                .bind(&version)
                .fetch_optional(&state.pool)
                .await?
            }
            None => {
                sqlx::query_scalar::<_, Uuid>(
                    r#"
                    select f.file_id
                    from apps a
                    join firmware_releases f on f.app_id = a.id
                    where a.slug = $1 and f.channel = $2
                    order by f.created_at desc
                    limit 1
                    "#,
                )
                .bind(&slug)
                .bind(&channel)
                .fetch_optional(&state.pool)
                .await?
            }
        },
        ApiPrincipal::User(user) => match version_filter {
            Some(version) => {
                sqlx::query_scalar::<_, Uuid>(
                    r#"
                    select f.file_id
                    from apps a
                    join app_assignments aa on aa.app_id = a.id
                    join firmware_releases f on f.app_id = a.id
                    where a.slug = $1 and aa.user_id = $2 and f.channel = $3 and f.version = $4
                    order by f.created_at desc
                    limit 1
                    "#,
                )
                .bind(&slug)
                .bind(user.id)
                .bind(&channel)
                .bind(&version)
                .fetch_optional(&state.pool)
                .await?
            }
            None => {
                sqlx::query_scalar::<_, Uuid>(
                    r#"
                    select f.file_id
                    from apps a
                    join app_assignments aa on aa.app_id = a.id
                    join firmware_releases f on f.app_id = a.id
                    where a.slug = $1 and aa.user_id = $2 and f.channel = $3
                    order by f.created_at desc
                    limit 1
                    "#,
                )
                .bind(&slug)
                .bind(user.id)
                .bind(&channel)
                .fetch_optional(&state.pool)
                .await?
            }
        },
        ApiPrincipal::Device(device) => match version_filter {
            Some(version) => {
                sqlx::query_scalar::<_, Uuid>(
                    r#"
                    select f.file_id
                    from apps a
                    join firmware_releases f on f.app_id = a.id
                    where a.slug = $1 and a.id = $2 and f.channel = $3 and f.version = $4
                    order by f.created_at desc
                    limit 1
                    "#,
                )
                .bind(&slug)
                .bind(device.app_id)
                .bind(&channel)
                .bind(&version)
                .fetch_optional(&state.pool)
                .await?
            }
            None => {
                sqlx::query_scalar::<_, Uuid>(
                    r#"
                    select f.file_id
                    from apps a
                    join firmware_releases f on f.app_id = a.id
                    where a.slug = $1 and a.id = $2 and f.channel = $3
                    order by f.created_at desc
                    limit 1
                    "#,
                )
                .bind(&slug)
                .bind(device.app_id)
                .bind(&channel)
                .fetch_optional(&state.pool)
                .await?
            }
        },
    };

    let Some(file_id) = file_id else {
        return Err(AppError::new_plain(
            StatusCode::NOT_FOUND,
            "Firmware not found",
        ));
    };

    let file = fetch_file_by_id(&state, file_id).await?;
    let Some(file) = file else {
        return Err(AppError::new_plain(StatusCode::NOT_FOUND, "File not found"));
    };

    let path = state.cache_dir.join(&file.local_path);
    let path = tokio::fs::canonicalize(path)
        .await
        .map_err(|_| AppError::new_plain(StatusCode::NOT_FOUND, "File missing on disk"))?;
    if !path.starts_with(&state.cache_dir) {
        return Err(AppError::new_plain(
            StatusCode::BAD_REQUEST,
            "Invalid file location",
        ));
    }

    let file_handle = File::open(&path)
        .await
        .map_err(|_| AppError::new_plain(StatusCode::NOT_FOUND, "File missing on disk"))?;
    let stream = ReaderStream::new(file_handle);
    let response = Response::builder()
        .status(StatusCode::OK)
        .header(header::CONTENT_TYPE, "application/octet-stream")
        .header(
            header::CONTENT_DISPOSITION,
            format!("attachment; filename=\"{}\"", file.filename),
        )
        .body(Body::from_stream(stream))
        .map_err(AppError::from)?;

    Ok(response)
}

#[derive(Deserialize)]
struct BootstrapMintRequest {
    app_slug: String,
    target: Option<String>,
    os: Option<String>,
    arch: Option<String>,
    expires_in_seconds: Option<i64>,
    max_uses: Option<i32>,
}

async fn api_mint_bootstrap_token(
    State(state): State<AppState>,
    headers: HeaderMap,
    axum::Json(req): axum::Json<BootstrapMintRequest>,
) -> AppResult<Response> {
    let (user, _token_id) = api_user_from_bearer(&state, &headers).await?;
    if !is_writer(&user) {
        return Err(AppError::new_plain(
            StatusCode::FORBIDDEN,
            "Writer permission required",
        ));
    }

    let slug = sanitize_slug(req.app_slug.trim());
    if slug.is_empty() {
        return Err(AppError::new_plain(
            StatusCode::BAD_REQUEST,
            "app_slug is required",
        ));
    }

    let requested = match (req.target.as_deref(), req.os.as_deref()) {
        (Some(target), _) => Some(parse_target_value(target)?),
        (None, Some(os_raw)) => {
            let os = normalize_os(os_raw)?;
            let arch = normalize_arch(req.arch.as_deref().unwrap_or("x86_64"))?;
            Some((os, arch))
        }
        (None, None) => None,
    }
    .ok_or_else(|| AppError::new_plain(StatusCode::BAD_REQUEST, "target (or os) is required"))?;

    let (os, arch) = requested;
    let record = sqlx::query_as::<_, (Uuid, String, String, String)>(
        r#"
        select p.pkg_file_id, p.version, p.os, p.arch
        from apps a
        join app_packages p on p.app_id = a.id
        where a.slug = $1 and p.os = $2 and (p.arch = $3 or p.arch = 'any')
        order by p.created_at desc, (p.arch = $3) desc
        limit 1
        "#,
    )
    .bind(&slug)
    .bind(&os)
    .bind(&arch)
    .fetch_optional(&state.pool)
    .await?;

    let Some((pkg_file_id, version, stored_os, stored_arch)) = record else {
        return Err(AppError::new_plain(
            StatusCode::NOT_FOUND,
            "Package not found",
        ));
    };

    let expires_in_seconds = req.expires_in_seconds.unwrap_or(3600).clamp(60, 86_400);
    let max_uses: i32 = req.max_uses.unwrap_or(10).clamp(1, 100);
    let expires_at = Utc::now() + ChronoDuration::seconds(expires_in_seconds);

    let mut token_bytes = [0u8; 32];
    let mut token = String::new();
    let mut inserted = false;
    for _ in 0..5 {
        OsRng.fill_bytes(&mut token_bytes);
        token = to_hex(&token_bytes);
        let token_sha256 = sha256_hex(token.as_bytes());
        let id = Uuid::new_v4();
        let result = sqlx::query(
            r#"
            insert into bootstrap_tokens (id, minted_by_user_id, pkg_file_id, token_sha256, expires_at, max_uses)
            values ($1, $2, $3, $4, $5, $6)
            "#,
        )
        .bind(id)
        .bind(user.id)
        .bind(pkg_file_id)
        .bind(&token_sha256)
        .bind(expires_at)
        .bind(max_uses)
        .execute(&state.pool)
        .await;

        match result {
            Ok(_) => {
                inserted = true;
                break;
            }
            Err(e) => {
                if let sqlx::Error::Database(db) = &e {
                    if db.constraint().is_some() {
                        continue;
                    }
                }
                return Err(AppError::new_plain(
                    StatusCode::INTERNAL_SERVER_ERROR,
                    format!("Failed to mint token: {}", e),
                ));
            }
        }
    }

    if !inserted {
        return Err(AppError::new_plain(
            StatusCode::INTERNAL_SERVER_ERROR,
            "Failed to mint token",
        ));
    }

    let pkg_url = format!("{}/bootstrap/{}/pkg", base_url(), token);
    info!(
        email = %user.email,
        app_slug = %slug,
        pkg_version = %version,
        pkg_os = %stored_os,
        pkg_arch = %stored_arch,
        pkg_file_id = %pkg_file_id,
        expires_at = %expires_at,
        max_uses = max_uses,
        "mint_bootstrap_token"
    );

    let payload = serde_json::json!({
        "app_slug": slug,
        "os": stored_os,
        "arch": stored_arch,
        "version": version,
        "pkg_file_id": pkg_file_id,
        "bootstrap_token": token,
        "expires_at": expires_at,
        "max_uses": max_uses,
        "pkg_url": pkg_url,
    });
    Ok(axum::Json(payload).into_response())
}

async fn bootstrap_exchange_device_token(
    Path(token): Path<String>,
    State(state): State<AppState>,
    Query(query): Query<HashMap<String, String>>,
) -> AppResult<Response> {
    let token = token.trim();
    if token.len() < 24 {
        return Err(AppError::new_plain(StatusCode::NOT_FOUND, "Not found"));
    }
    let token_sha256 = sha256_hex(token.as_bytes());

    let pkg_file_id = sqlx::query_scalar::<_, Uuid>(
        r#"
        update bootstrap_tokens
           set use_count = use_count + 1,
               last_used_at = now()
         where token_sha256 = $1
           and expires_at > now()
           and use_count < max_uses
        returning pkg_file_id
        "#,
    )
    .bind(&token_sha256)
    .fetch_optional(&state.pool)
    .await?
    .ok_or_else(|| AppError::new_plain(StatusCode::NOT_FOUND, "Not found"))?;

    let app_info = sqlx::query_as::<_, (Uuid, String, String, String, String)>(
        r#"
        select a.id, a.slug, p.os, p.arch, p.version
        from app_packages p
        join apps a on a.id = p.app_id
        where p.pkg_file_id = $1
        limit 1
        "#,
    )
    .bind(pkg_file_id)
    .fetch_optional(&state.pool)
    .await?;

    let Some((app_id, app_slug, os, arch, version)) = app_info else {
        return Err(AppError::new_plain(
            StatusCode::NOT_FOUND,
            "Package not found",
        ));
    };

    let name = query
        .get("name")
        .map(|s| s.trim())
        .filter(|s| !s.is_empty())
        .unwrap_or("");
    let name: String = name
        .chars()
        .filter(|&ch| ch != '\n' && ch != '\r' && ch != '\t')
        .take(200)
        .collect();

    let mut token_bytes = [0u8; 32];
    let mut device_token = String::new();
    let mut inserted = false;
    for _ in 0..5 {
        OsRng.fill_bytes(&mut token_bytes);
        device_token = to_hex(&token_bytes);
        let device_sha256 = sha256_hex(device_token.as_bytes());
        let id = Uuid::new_v4();
        let result = sqlx::query(
            r#"
            insert into device_tokens (id, app_id, os, arch, name, token_sha256)
            values ($1, $2, $3, $4, $5, $6)
            "#,
        )
        .bind(id)
        .bind(app_id)
        .bind(&os)
        .bind(&arch)
        .bind(&name)
        .bind(&device_sha256)
        .execute(&state.pool)
        .await;

        match result {
            Ok(_) => {
                inserted = true;
                break;
            }
            Err(e) => {
                if let sqlx::Error::Database(db) = &e {
                    if db.constraint().is_some() {
                        continue;
                    }
                }
                return Err(AppError::new_plain(
                    StatusCode::INTERNAL_SERVER_ERROR,
                    format!("Failed to mint device token: {}", e),
                ));
            }
        }
    }

    if !inserted {
        return Err(AppError::new_plain(
            StatusCode::INTERNAL_SERVER_ERROR,
            "Failed to mint device token",
        ));
    }

    info!(
        app_slug = %app_slug,
        pkg_file_id = %pkg_file_id,
        pkg_version = %version,
        os = %os,
        arch = %arch,
        device_name = %name,
        "bootstrap_exchange_device_token"
    );

    let api_base_url = base_url();
    let payload = serde_json::json!({
        "app_slug": app_slug,
        "os": os,
        "arch": arch,
        "version": version,
        "device_token": device_token,
        "api_base_url": api_base_url,
        "package_latest_url": format!("{}/api/apps/{}/package/latest?os={}&arch={}", api_base_url, app_slug, os, arch),
        "firmware_latest_url": format!("{}/api/apps/{}/firmware/latest?channel=stable", api_base_url, app_slug),
    });
    let mut response = axum::Json(payload).into_response();
    response
        .headers_mut()
        .insert(header::CACHE_CONTROL, HeaderValue::from_static("no-store"));
    Ok(response)
}

async fn bootstrap_download_pkg(
    Path(token): Path<String>,
    State(state): State<AppState>,
) -> AppResult<Response> {
    let token = token.trim();
    if token.len() < 24 {
        return Err(AppError::new_plain(StatusCode::NOT_FOUND, "Not found"));
    }
    let token_sha256 = sha256_hex(token.as_bytes());

    let pkg_file_id = sqlx::query_scalar::<_, Uuid>(
        r#"
        update bootstrap_tokens
           set use_count = use_count + 1,
               last_used_at = now()
         where token_sha256 = $1
           and expires_at > now()
           and use_count < max_uses
        returning pkg_file_id
        "#,
    )
    .bind(&token_sha256)
    .fetch_optional(&state.pool)
    .await?
    .ok_or_else(|| AppError::new_plain(StatusCode::NOT_FOUND, "Not found"))?;

    stream_cached_file_response(&state, pkg_file_id).await
}

async fn stream_cached_file_response(state: &AppState, id: Uuid) -> AppResult<Response> {
    let file = fetch_file_by_id(state, id).await?;
    let Some(file) = file else {
        return Err(AppError::new_plain(StatusCode::NOT_FOUND, "File not found"));
    };

    let path = state.cache_dir.join(&file.local_path);
    let path = tokio::fs::canonicalize(path)
        .await
        .map_err(|_| AppError::new_plain(StatusCode::NOT_FOUND, "File missing on disk"))?;
    if !path.starts_with(&state.cache_dir) {
        return Err(AppError::new_plain(
            StatusCode::BAD_REQUEST,
            "Invalid file location",
        ));
    }

    let file_handle = File::open(&path)
        .await
        .map_err(|_| AppError::new_plain(StatusCode::NOT_FOUND, "File missing on disk"))?;
    let stream = ReaderStream::new(file_handle);
    let response = Response::builder()
        .status(StatusCode::OK)
        .header(header::CONTENT_TYPE, "application/octet-stream")
        .header(header::CACHE_CONTROL, "no-store")
        .header(
            header::CONTENT_DISPOSITION,
            format!("attachment; filename=\"{}\"", file.filename),
        )
        .body(Body::from_stream(stream))
        .map_err(AppError::from)?;

    Ok(response)
}

async fn stream_cached_image_file(state: &AppState, file: &CachedFile) -> AppResult<Response> {
    let ext = std::path::Path::new(&file.filename)
        .extension()
        .and_then(|e| e.to_str())
        .unwrap_or("")
        .to_ascii_lowercase()
        .to_string();
    let is_svg = ext == "svg";
    let content_type = match ext.as_str() {
        "png" => "image/png",
        "svg" => "image/svg+xml",
        _ => "application/octet-stream",
    };

    let path = state.cache_dir.join(&file.local_path);
    let path = tokio::fs::canonicalize(path)
        .await
        .map_err(|_| AppError::new(StatusCode::NOT_FOUND, "File missing on disk"))?;
    if !path.starts_with(&state.cache_dir) {
        return Err(AppError::new(
            StatusCode::BAD_REQUEST,
            "Invalid file location",
        ));
    }

    let file_handle = File::open(&path)
        .await
        .map_err(|_| AppError::new(StatusCode::NOT_FOUND, "File missing on disk"))?;
    let stream = ReaderStream::new(file_handle);
    let mut builder = Response::builder()
        .status(StatusCode::OK)
        .header(header::CONTENT_TYPE, content_type)
        .header(header::X_CONTENT_TYPE_OPTIONS, "nosniff")
        .header(header::CACHE_CONTROL, "private, max-age=3600")
        .header(header::X_FRAME_OPTIONS, "DENY");
    if is_svg {
        builder = builder.header(
            header::CONTENT_SECURITY_POLICY,
            "sandbox; default-src 'none'; img-src 'self' data:; style-src 'unsafe-inline'",
        );
    }
    let response = builder
        .body(Body::from_stream(stream))
        .map_err(AppError::from)?;
    Ok(response)
}

struct UploadResult {
    id: Uuid,
    sha256: String,
}

async fn store_local_file_as_cached(
    state: &AppState,
    source_path: &std::path::Path,
    original_filename: &str,
    prefix: &str,
) -> AppResult<UploadResult> {
    let clean = sanitize_filename(original_filename);
    let file_id = Uuid::new_v4();
    let stored_name = format!("{}-{}", file_id, clean);
    let relative_path = format!("{}/{}", sanitize_bucket(prefix), stored_name);
    let dest_path = state.cache_dir.join(&relative_path);

    if let Some(parent) = dest_path.parent() {
        tokio::fs::create_dir_all(parent).await.map_err(|e| {
            AppError::new(
                StatusCode::INTERNAL_SERVER_ERROR,
                format!("Could not create directory {}: {}", parent.display(), e),
            )
        })?;
    }

    let mut src = File::open(source_path).await.map_err(|e| {
        AppError::new(
            StatusCode::INTERNAL_SERVER_ERROR,
            format!("Could not open built pkg: {}", e),
        )
    })?;
    let mut dest = File::create(&dest_path).await.map_err(|e| {
        AppError::new(
            StatusCode::INTERNAL_SERVER_ERROR,
            format!("Could not create file {}: {}", dest_path.display(), e),
        )
    })?;

    let mut size: i64 = 0;
    let mut hasher = Sha256::new();
    let mut buf = vec![0u8; 64 * 1024];
    loop {
        use tokio::io::AsyncReadExt;
        let n = src.read(&mut buf).await.map_err(|e| {
            AppError::new(
                StatusCode::INTERNAL_SERVER_ERROR,
                format!("Could not read built pkg: {}", e),
            )
        })?;
        if n == 0 {
            break;
        }
        size += n as i64;
        hasher.update(&buf[..n]);
        dest.write_all(&buf[..n]).await.map_err(|e| {
            AppError::new(
                StatusCode::INTERNAL_SERVER_ERROR,
                format!("Could not write built pkg: {}", e),
            )
        })?;
    }
    let sha256 = to_hex(&hasher.finalize());

    sqlx::query(
        "insert into cached_files (id, filename, local_path, size_bytes) values ($1, $2, $3, $4)",
    )
    .bind(file_id)
    .bind(clean)
    .bind(relative_path)
    .bind(size)
    .execute(&state.pool)
    .await?;

    Ok(UploadResult {
        id: file_id,
        sha256,
    })
}

async fn build_tar_gz(
    src_dir: &std::path::Path,
    dest_file: &std::path::Path,
) -> Result<(), String> {
    let src_dir = src_dir.to_path_buf();
    let dest_file = dest_file.to_path_buf();

    tokio::task::spawn_blocking(move || {
        use flate2::Compression;
        use flate2::write::GzEncoder;
        use std::fs::File as StdFile;
        let out = StdFile::create(&dest_file)
            .map_err(|e| format!("Could not create archive {}: {}", dest_file.display(), e))?;
        let enc = GzEncoder::new(out, Compression::default());
        let mut tar_builder = tar::Builder::new(enc);
        tar_builder
            .append_dir_all(".", &src_dir)
            .map_err(|e| format!("Failed to archive {}: {}", src_dir.display(), e))?;
        let enc = tar_builder
            .into_inner()
            .map_err(|e| format!("Failed finalizing tar: {}", e))?;
        enc.finish()
            .map_err(|e| format!("Failed finalizing gzip: {}", e))?;
        Ok::<(), String>(())
    })
    .await
    .map_err(|e| format!("Failed building archive: {}", e))?
}

async fn save_upload_to_prefix(
    state: &AppState,
    mut field: axum::extract::multipart::Field<'_>,
    prefix: &str,
    allowed_exts: &[&str],
) -> AppResult<UploadResult> {
    let file_name = field
        .file_name()
        .map(|s| s.to_string())
        .unwrap_or_else(|| "upload.bin".to_string());
    let clean = sanitize_filename(&file_name);
    let ext = std::path::Path::new(&clean)
        .extension()
        .and_then(|e| e.to_str())
        .unwrap_or("")
        .to_lowercase();
    if !allowed_exts
        .iter()
        .any(|allowed| allowed.eq_ignore_ascii_case(&ext))
    {
        return Err(AppError::new(
            StatusCode::BAD_REQUEST,
            format!(
                "Invalid file type .{} (allowed: {})",
                ext,
                allowed_exts.join(", ")
            ),
        ));
    }

    let file_id = Uuid::new_v4();
    let stored_name = format!("{}-{}", file_id, clean);
    let relative_path = format!("{}/{}", sanitize_bucket(prefix), stored_name);
    let dest_path = state.cache_dir.join(&relative_path);

    if let Some(parent) = dest_path.parent() {
        tokio::fs::create_dir_all(parent).await.map_err(|e| {
            AppError::new(
                StatusCode::INTERNAL_SERVER_ERROR,
                format!("Could not create directory {}: {}", parent.display(), e),
            )
        })?;
    }

    let mut dest = File::create(&dest_path).await.map_err(|e| {
        AppError::new(
            StatusCode::INTERNAL_SERVER_ERROR,
            format!("Could not create file {}: {}", dest_path.display(), e),
        )
    })?;
    let mut size: i64 = 0;
    let mut hasher = Sha256::new();

    while let Some(chunk) = field
        .chunk()
        .await
        .map_err(|_| AppError::new(StatusCode::BAD_REQUEST, "Failed reading upload"))?
    {
        size += chunk.len() as i64;
        hasher.update(&chunk);
        dest.write_all(&chunk).await.map_err(|e| {
            AppError::new(
                StatusCode::INTERNAL_SERVER_ERROR,
                format!("Failed writing upload to {}: {}", dest_path.display(), e),
            )
        })?;
    }

    let sha256 = to_hex(&hasher.finalize());

    sqlx::query(
        "insert into cached_files (id, filename, local_path, size_bytes) values ($1, $2, $3, $4)",
    )
    .bind(file_id)
    .bind(clean)
    .bind(relative_path)
    .bind(size)
    .execute(&state.pool)
    .await?;

    Ok(UploadResult {
        id: file_id,
        sha256,
    })
}

fn sanitize_slug(value: &str) -> String {
    let mut out = String::with_capacity(value.len());
    for ch in value.chars() {
        let mapped = match ch {
            'a'..='z' | '0'..='9' | '-' => Some(ch),
            'A'..='Z' => Some(ch.to_ascii_lowercase()),
            ' ' | '_' => Some('-'),
            _ => None,
        };
        if let Some(ch) = mapped {
            if ch == '-' && out.ends_with('-') {
                continue;
            }
            out.push(ch);
        }
    }
    out.trim_matches('-').to_string()
}

fn sha256_hex(bytes: &[u8]) -> String {
    let mut hasher = Sha256::new();
    hasher.update(bytes);
    to_hex(&hasher.finalize())
}

fn normalize_os(value: &str) -> AppResult<String> {
    let v = value.trim().to_ascii_lowercase();
    match v.as_str() {
        "windows" | "win" | "win32" | "win64" => Ok("windows".to_string()),
        "linux" => Ok("linux".to_string()),
        "macos" | "osx" | "darwin" => Ok("macos".to_string()),
        _ => Err(AppError::new(
            StatusCode::BAD_REQUEST,
            "Invalid OS (use windows|linux|macos)",
        )),
    }
}

fn normalize_arch(value: &str) -> AppResult<String> {
    let v = value.trim().to_ascii_lowercase();
    match v.as_str() {
        "" => Ok("x86_64".to_string()),
        "x86_64" | "amd64" | "x64" => Ok("x86_64".to_string()),
        "aarch64" | "arm64" => Ok("aarch64".to_string()),
        "any" => Ok("any".to_string()),
        _ => Err(AppError::new(
            StatusCode::BAD_REQUEST,
            "Invalid arch (use x86_64|aarch64|any)",
        )),
    }
}

fn parse_target_query(query: &HashMap<String, String>) -> Option<(String, String)> {
    if let Some(target) = query
        .get("target")
        .map(|s| s.trim())
        .filter(|s| !s.is_empty())
    {
        let mut parts = target.splitn(2, ':');
        let os_raw = parts.next()?.trim();
        let arch_raw = parts.next().unwrap_or("x86_64").trim();
        let os = normalize_os(os_raw).ok()?;
        let arch = normalize_arch(arch_raw).ok()?;
        return Some((os, arch));
    }
    let os = query
        .get("os")
        .map(|s| s.trim())
        .filter(|s| !s.is_empty())?;
    let os = normalize_os(os).ok()?;
    let arch = query
        .get("arch")
        .map(|s| s.trim())
        .filter(|s| !s.is_empty())
        .unwrap_or("x86_64");
    let arch = normalize_arch(arch).ok()?;
    Some((os, arch))
}

fn parse_target_value(value: &str) -> AppResult<(String, String)> {
    let value = value.trim();
    if value.is_empty() {
        return Err(AppError::new_plain(
            StatusCode::BAD_REQUEST,
            "target is required",
        ));
    }
    let mut parts = value.splitn(2, ':');
    let os_raw = parts
        .next()
        .map(|s| s.trim())
        .filter(|s| !s.is_empty())
        .ok_or_else(|| AppError::new_plain(StatusCode::BAD_REQUEST, "Invalid target"))?;
    let arch_raw = parts
        .next()
        .map(|s| s.trim())
        .filter(|s| !s.is_empty())
        .unwrap_or("x86_64");
    let os = normalize_os(os_raw)?;
    let arch = normalize_arch(arch_raw)?;
    Ok((os, arch))
}

fn safe_return_to(return_to: Option<&str>) -> String {
    let Some(value) = return_to.map(|s| s.trim()).filter(|s| !s.is_empty()) else {
        return "/apps/manage".to_string();
    };
    if value.starts_with("/apps/manage") {
        value.to_string()
    } else {
        "/apps/manage".to_string()
    }
}

fn normalize_pkg_member_path(path: &std::path::Path) -> String {
    use std::path::Component;
    let mut out = PathBuf::new();
    for component in path.components() {
        match component {
            Component::CurDir => continue,
            Component::Normal(p) => out.push(p),
            _ => continue,
        }
    }
    out.to_string_lossy().replace('\\', "/")
}

async fn extract_installer_from_pkg(pkg_path: &std::path::Path) -> AppResult<(PathBuf, String)> {
    let pkg_path = pkg_path.to_path_buf();
    tokio::task::spawn_blocking(move || -> AppResult<(PathBuf, String)> {
        use flate2::read::GzDecoder;
        use std::fs::File as StdFile;
        use std::io::Read as _;
        use tar::Archive;

        let manifest_json = {
            let file = StdFile::open(&pkg_path)
                .map_err(|_| AppError::new(StatusCode::NOT_FOUND, "Package missing on disk"))?;
            let dec = GzDecoder::new(file);
            let mut archive = Archive::new(dec);
            let mut manifest: Option<String> = None;
            for entry in archive
                .entries()
                .map_err(|_| AppError::new(StatusCode::BAD_REQUEST, "Invalid package archive"))?
            {
                let mut entry = entry.map_err(|_| {
                    AppError::new(StatusCode::BAD_REQUEST, "Invalid package archive")
                })?;
                let path = entry.path().map_err(|_| {
                    AppError::new(StatusCode::BAD_REQUEST, "Invalid package archive")
                })?;
                if normalize_pkg_member_path(&path) == "manifest.json" {
                    let mut buf = String::new();
                    entry.read_to_string(&mut buf).map_err(|_| {
                        AppError::new(StatusCode::BAD_REQUEST, "Invalid manifest.json")
                    })?;
                    manifest = Some(buf);
                    break;
                }
            }
            manifest.ok_or_else(|| {
                AppError::new(StatusCode::BAD_REQUEST, "Package missing manifest.json")
            })?
        };

        let manifest: PkgManifest = serde_json::from_str(&manifest_json)
            .map_err(|_| AppError::new(StatusCode::BAD_REQUEST, "Invalid manifest.json"))?;
        let wanted = normalize_pkg_member_path(std::path::Path::new(&manifest.installer.path));
        if wanted.is_empty() {
            return Err(AppError::new(
                StatusCode::BAD_REQUEST,
                "Invalid manifest.json installer path",
            ));
        }

        let file_name = std::path::Path::new(&wanted)
            .file_name()
            .and_then(|s| s.to_str())
            .unwrap_or("installer.bin");
        let file_name = sanitize_filename(file_name);

        let out_path = std::env::temp_dir().join(format!(
            "pdes-cdn-installer-{}-{}",
            Uuid::new_v4(),
            file_name
        ));

        let file = StdFile::open(&pkg_path)
            .map_err(|_| AppError::new(StatusCode::NOT_FOUND, "Package missing on disk"))?;
        let dec = GzDecoder::new(file);
        let mut archive = Archive::new(dec);

        for entry in archive
            .entries()
            .map_err(|_| AppError::new(StatusCode::BAD_REQUEST, "Invalid package archive"))?
        {
            let mut entry = entry
                .map_err(|_| AppError::new(StatusCode::BAD_REQUEST, "Invalid package archive"))?;
            let path = entry
                .path()
                .map_err(|_| AppError::new(StatusCode::BAD_REQUEST, "Invalid package archive"))?;
            if normalize_pkg_member_path(&path) == wanted {
                let mut out = StdFile::create(&out_path).map_err(|_| {
                    AppError::new(
                        StatusCode::INTERNAL_SERVER_ERROR,
                        "Could not create temp installer file",
                    )
                })?;
                std::io::copy(&mut entry, &mut out).map_err(|_| {
                    AppError::new(
                        StatusCode::INTERNAL_SERVER_ERROR,
                        "Failed extracting installer",
                    )
                })?;
                return Ok((out_path, file_name));
            }
        }

        Err(AppError::new(
            StatusCode::BAD_REQUEST,
            "Installer not found in package",
        ))
    })
    .await
    .map_err(|_| AppError::new(StatusCode::INTERNAL_SERVER_ERROR, "Extraction failed"))?
}

async fn extract_icon_from_pkg(pkg_path: &std::path::Path) -> AppResult<(PathBuf, String)> {
    let pkg_path = pkg_path.to_path_buf();
    tokio::task::spawn_blocking(move || -> AppResult<(PathBuf, String)> {
        use flate2::read::GzDecoder;
        use std::fs::File as StdFile;
        use std::io::Read as _;
        use tar::Archive;

        let manifest_json = {
            let file = StdFile::open(&pkg_path)
                .map_err(|_| AppError::new(StatusCode::NOT_FOUND, "Package missing on disk"))?;
            let dec = GzDecoder::new(file);
            let mut archive = Archive::new(dec);
            let mut manifest: Option<String> = None;
            for entry in archive
                .entries()
                .map_err(|_| AppError::new(StatusCode::BAD_REQUEST, "Invalid package archive"))?
            {
                let mut entry = entry.map_err(|_| {
                    AppError::new(StatusCode::BAD_REQUEST, "Invalid package archive")
                })?;
                let path = entry.path().map_err(|_| {
                    AppError::new(StatusCode::BAD_REQUEST, "Invalid package archive")
                })?;
                if normalize_pkg_member_path(&path) == "manifest.json" {
                    let mut buf = String::new();
                    entry.read_to_string(&mut buf).map_err(|_| {
                        AppError::new(StatusCode::BAD_REQUEST, "Invalid manifest.json")
                    })?;
                    manifest = Some(buf);
                    break;
                }
            }
            manifest.ok_or_else(|| {
                AppError::new(StatusCode::BAD_REQUEST, "Package missing manifest.json")
            })?
        };

        let manifest: PkgManifest = serde_json::from_str(&manifest_json)
            .map_err(|_| AppError::new(StatusCode::BAD_REQUEST, "Invalid manifest.json"))?;
        let Some(icon) = manifest.icon else {
            return Err(AppError::new(
                StatusCode::BAD_REQUEST,
                "Package missing icon metadata",
            ));
        };
        let wanted = normalize_pkg_member_path(std::path::Path::new(&icon.path));
        if wanted.is_empty() {
            return Err(AppError::new(
                StatusCode::BAD_REQUEST,
                "Invalid manifest.json icon path",
            ));
        }

        let file_name = std::path::Path::new(&wanted)
            .file_name()
            .and_then(|s| s.to_str())
            .unwrap_or("icon.png");
        let file_name = sanitize_filename(file_name);

        let out_path =
            std::env::temp_dir().join(format!("pdes-cdn-icon-{}-{}", Uuid::new_v4(), file_name));

        let file = StdFile::open(&pkg_path)
            .map_err(|_| AppError::new(StatusCode::NOT_FOUND, "Package missing on disk"))?;
        let dec = GzDecoder::new(file);
        let mut archive = Archive::new(dec);

        for entry in archive
            .entries()
            .map_err(|_| AppError::new(StatusCode::BAD_REQUEST, "Invalid package archive"))?
        {
            let mut entry = entry
                .map_err(|_| AppError::new(StatusCode::BAD_REQUEST, "Invalid package archive"))?;
            let path = entry
                .path()
                .map_err(|_| AppError::new(StatusCode::BAD_REQUEST, "Invalid package archive"))?;
            if normalize_pkg_member_path(&path) == wanted {
                let mut out = StdFile::create(&out_path).map_err(|_| {
                    AppError::new(
                        StatusCode::INTERNAL_SERVER_ERROR,
                        "Could not create temp icon file",
                    )
                })?;
                std::io::copy(&mut entry, &mut out).map_err(|_| {
                    AppError::new(StatusCode::INTERNAL_SERVER_ERROR, "Failed extracting icon")
                })?;
                return Ok((out_path, file_name));
            }
        }

        Err(AppError::new(
            StatusCode::BAD_REQUEST,
            "Icon not found in package",
        ))
    })
    .await
    .map_err(|_| AppError::new(StatusCode::INTERNAL_SERVER_ERROR, "Extraction failed"))?
}

fn url_encode_component(value: &str) -> String {
    let mut out = String::with_capacity(value.len());
    for &b in value.as_bytes() {
        match b {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' => {
                out.push(b as char)
            }
            b' ' => out.push('+'),
            _ => out.push_str(&format!("%{:02X}", b)),
        }
    }
    out
}

fn add_query_param(url: &str, key: &str, value: &str) -> String {
    let sep = if url.contains('?') { '&' } else { '?' };
    format!("{url}{sep}{key}={}", url_encode_component(value.trim()))
}

fn detect_target_from_user_agent(headers: &HeaderMap) -> Option<(String, String)> {
    let ua = headers
        .get(header::USER_AGENT)?
        .to_str()
        .ok()?
        .to_ascii_lowercase();
    let os = if ua.contains("windows") {
        "windows"
    } else if ua.contains("mac os x") || ua.contains("macintosh") {
        "macos"
    } else if ua.contains("linux") {
        "linux"
    } else {
        return None;
    };

    let arch = if ua.contains("aarch64")
        || ua.contains("arm64")
        || ua.contains("armv8")
        || ua.contains("armv8l")
    {
        "aarch64"
    } else if ua.contains("x86_64")
        || ua.contains("amd64")
        || ua.contains("x64")
        || ua.contains("win64")
    {
        "x86_64"
    } else {
        "x86_64"
    };

    Some((os.to_string(), arch.to_string()))
}

fn to_hex(bytes: &[u8]) -> String {
    let mut out = String::with_capacity(bytes.len() * 2);
    for byte in bytes {
        use std::fmt::Write as _;
        let _ = write!(&mut out, "{:02x}", byte);
    }
    out
}

async fn current_user(state: &AppState, jar: &SignedCookieJar) -> AppResult<Option<User>> {
    let Some(cookie) = jar.get(SESSION_COOKIE) else {
        return Ok(None);
    };
    let user_id = Uuid::parse_str(cookie.value())
        .map_err(|_| AppError::new(StatusCode::BAD_REQUEST, "Invalid session token"))?;

    let user = sqlx::query_as::<_, User>(
        "select id, email, password_hash, permission, reset_token, reset_token_expires, must_reset from users where id = $1",
    )
    .bind(user_id)
    .fetch_optional(&state.pool)
    .await?;

    Ok(user)
}

async fn ensure_valid_token(state: &AppState, token: &str) -> AppResult<User> {
    let user = sqlx::query_as::<_, User>(
        "select id, email, password_hash, permission, reset_token, reset_token_expires, must_reset from users where reset_token = $1",
    )
    .bind(token)
    .fetch_optional(&state.pool)
    .await?;

    let Some(user) = user else {
        return Err(AppError::new(
            StatusCode::BAD_REQUEST,
            "Invalid or expired reset token",
        ));
    };

    let Some(expires) = user.reset_token_expires else {
        return Err(AppError::new(
            StatusCode::BAD_REQUEST,
            "Reset token has expired",
        ));
    };
    if expires < Utc::now() {
        return Err(AppError::new(
            StatusCode::BAD_REQUEST,
            "Reset token has expired",
        ));
    }

    Ok(user)
}

async fn fetch_file_by_id(state: &AppState, id: Uuid) -> AppResult<Option<CachedFile>> {
    if let Some(file) = state.file_by_id_cache.get(&id).await {
        return Ok(Some(file));
    }

    let file = sqlx::query_as::<_, CachedFile>(
        "select id, filename, local_path, size_bytes, created_at from cached_files where id = $1",
    )
    .bind(id)
    .fetch_optional(&state.pool)
    .await?;

    if let Some(ref f) = file {
        state.file_by_id_cache.insert(id, f.clone()).await;
    }

    Ok(file)
}

async fn ensure_writable_cache_root(cache_dir: &std::path::Path) -> AppResult<()> {
    let probe_dir = cache_dir.join(".pdes-cdn-write-test");
    tokio::fs::create_dir_all(&probe_dir).await.map_err(|e| {
        AppError::new(
            StatusCode::INTERNAL_SERVER_ERROR,
            format!(
                "CACHE_DIR is not writable (cannot create {}): {}",
                probe_dir.display(),
                e
            ),
        )
    })?;
    let probe_file = probe_dir.join("probe");
    tokio::fs::write(&probe_file, b"probe").await.map_err(|e| {
        AppError::new(
            StatusCode::INTERNAL_SERVER_ERROR,
            format!(
                "CACHE_DIR is not writable (cannot write {}): {}",
                probe_file.display(),
                e
            ),
        )
    })?;
    let _ = tokio::fs::remove_file(&probe_file).await;
    let _ = tokio::fs::remove_dir(&probe_dir).await;
    Ok(())
}

async fn resolve_cache_path(state: &AppState, relative: &str) -> AppResult<Option<PathBuf>> {
    let path = state.cache_dir.join(relative);
    let path = match tokio::fs::canonicalize(path).await {
        Ok(p) => p,
        Err(_) => return Ok(None),
    };
    if !path.starts_with(&state.cache_dir) {
        return Ok(None);
    }
    Ok(Some(path))
}

fn sanitize_filename(name: &str) -> String {
    let mut out = String::with_capacity(name.len());
    for ch in name.chars() {
        if ch.is_ascii_alphanumeric() || ch == '.' || ch == '-' || ch == '_' {
            out.push(ch);
        } else {
            out.push('_');
        }
    }
    let cleaned = out.trim_matches('_').to_string();
    if cleaned.is_empty() {
        "upload.bin".to_string()
    } else {
        cleaned
    }
}

fn sanitize_bucket(name: &str) -> String {
    let mut out = String::with_capacity(name.len());
    for part in name.split('/') {
        if part.is_empty() || part == "." || part == ".." {
            continue;
        }
        if !out.is_empty() {
            out.push('/');
        }
        for ch in part.chars() {
            if ch.is_ascii_alphanumeric() || ch == '-' || ch == '_' {
                out.push(ch);
            } else {
                out.push('_');
            }
        }
    }
    out.trim_matches('_').trim_matches('/').to_string()
}

fn is_writer(user: &User) -> bool {
    user.permission.eq_ignore_ascii_case("writer")
}

fn extract_bearer(headers: &HeaderMap) -> Option<String> {
    headers
        .get(header::AUTHORIZATION)
        .and_then(|h| h.to_str().ok())
        .and_then(|value| {
            value
                .strip_prefix("Bearer ")
                .or_else(|| value.strip_prefix("bearer "))
        })
        .map(|s| s.trim().to_string())
}

async fn issue_reset_token_with_ttl(
    state: &AppState,
    user_id: Uuid,
    ttl: ChronoDuration,
) -> AppResult<String> {
    let token: String = rand::thread_rng()
        .sample_iter(&Alphanumeric)
        .take(32)
        .map(char::from)
        .collect();
    let expires = Utc::now() + ttl;

    sqlx::query(
        "update users set reset_token = $1, reset_token_expires = $2, must_reset = true where id = $3",
    )
    .bind(&token)
    .bind(expires)
    .bind(user_id)
    .execute(&state.pool)
    .await?;

    let token_preview: String = token.chars().take(8).collect();
    info!(user_id = %user_id, token_prefix = %token_preview, expires = %expires, "issued reset token");
    Ok(token)
}

async fn issue_reset_token(state: &AppState, user_id: Uuid) -> AppResult<String> {
    issue_reset_token_with_ttl(state, user_id, ChronoDuration::minutes(30)).await
}

async fn issue_invite_reset_token(state: &AppState, user_id: Uuid) -> AppResult<String> {
    issue_reset_token_with_ttl(state, user_id, ChronoDuration::days(7)).await
}

fn render_email_template(state: &AppState, name: &str, mut ctx: TeraContext) -> AppResult<String> {
    insert_app_template_globals(&mut ctx);
    state
        .templates
        .render(name, &ctx)
        .map_err(|e| AppError::new(StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))
}

fn insert_app_template_globals(ctx: &mut TeraContext) {
    fn is_placeholder_value(key: &str, value: &str) -> bool {
        let trimmed = value.trim();
        if trimmed.is_empty() {
            return true;
        }
        trimmed.eq_ignore_ascii_case("placeholder")
            || trimmed.eq_ignore_ascii_case("changeme")
            || trimmed.eq_ignore_ascii_case("replace_me")
            || trimmed.eq_ignore_ascii_case("todo")
            || trimmed.eq_ignore_ascii_case("unset")
            || trimmed.eq_ignore_ascii_case("unknown")
            || trimmed.eq_ignore_ascii_case("null")
            || trimmed.eq_ignore_ascii_case("none")
            || trimmed.eq_ignore_ascii_case("nil")
            || trimmed.eq_ignore_ascii_case("n/a")
            || trimmed == key
            || trimmed == format!("${}", key)
            || trimmed == format!("${{{}}}", key)
            || trimmed == format!("__{}__", key)
    }

    fn cleaned_env_value(key: &str) -> Option<String> {
        let value = std::env::var(key).ok()?;
        let value = value
            .trim()
            .trim_matches(|c| c == '"' || c == '\'')
            .to_string();
        if is_placeholder_value(key, &value) {
            None
        } else {
            Some(value)
        }
    }

    fn cleaned_compile_time_value(key: &str, value: Option<&'static str>) -> Option<String> {
        let value = value?;
        let value = value
            .trim()
            .trim_matches(|c| c == '"' || c == '\'')
            .to_string();
        if is_placeholder_value(key, &value) {
            None
        } else {
            Some(value)
        }
    }

    let app_name = cleaned_env_value("APP_NAME")
        .or_else(|| cleaned_compile_time_value("APP_NAME", option_env!("APP_NAME")))
        .unwrap_or_else(|| "PDES CDN Node".to_string());
    ctx.insert("app_name", &app_name);
    ctx.insert("app_version", env!("CARGO_PKG_VERSION"));

    if let Some(build) = cleaned_env_value("APP_BUILD")
        .or_else(|| cleaned_compile_time_value("APP_BUILD", option_env!("APP_BUILD")))
    {
        ctx.insert("app_build", &build);
    }

    if let Some(commit) = cleaned_env_value("APP_COMMIT")
        .or_else(|| cleaned_compile_time_value("APP_COMMIT", option_env!("APP_COMMIT")))
    {
        let short = commit.chars().take(8).collect::<String>();
        if !short.is_empty() {
            ctx.insert("app_commit", &short);
        }
    }
}

fn escape_html(input: &str) -> String {
    let mut out = String::with_capacity(input.len().saturating_add(16));
    for ch in input.chars() {
        match ch {
            '&' => out.push_str("&amp;"),
            '<' => out.push_str("&lt;"),
            '>' => out.push_str("&gt;"),
            '"' => out.push_str("&quot;"),
            '\'' => out.push_str("&#39;"),
            _ => out.push(ch),
        }
    }
    out
}

fn render_plain_email_html(subject: &str, body_text: &str) -> String {
    let subject = escape_html(subject);
    let body = escape_html(body_text).replace('\n', "<br>");
    format!(
        r#"<!doctype html>
<html lang="en">
  <head>
    <meta charset="utf-8">
    <meta name="viewport" content="width=device-width, initial-scale=1">
    <title>{subject}</title>
  </head>
  <body style="margin:0;padding:0;background-color:#0b1222;color:#e2e8f0;font-family:Segoe UI, Arial, sans-serif;">
    <table role="presentation" cellpadding="0" cellspacing="0" width="100%" style="background-color:#0b1222;padding:28px 12px;">
      <tr>
        <td align="center">
          <table role="presentation" cellpadding="0" cellspacing="0" width="600" style="max-width:600px;width:100%;border-collapse:separate;border-spacing:0;">
            <tr>
              <td style="background-color:#111827;border:1px solid #1f2937;border-radius:16px;padding:22px 22px 18px 22px;">
                <h1 style="margin:0 0 10px 0;font-size:20px;line-height:1.25;color:#e2e8f0;">{subject}</h1>
                <p style="margin:0;font-size:14px;line-height:1.65;color:#cbd5e1;">{body}</p>
              </td>
            </tr>
            <tr>
              <td style="padding:14px 6px 0 6px;font-size:12px;line-height:1.6;color:#64748b;">
                Palo Duro Embedded Systems
              </td>
            </tr>
          </table>
        </td>
      </tr>
    </table>
  </body>
</html>"#
    )
}

async fn send_email(
    to: &str,
    subject: &str,
    body_text: &str,
    body_html: Option<String>,
) -> AppResult<()> {
    let host = std::env::var("SMTP_HOST").unwrap_or_default();
    let username = std::env::var("SMTP_USERNAME").unwrap_or_default();
    let password = std::env::var("SMTP_PASSWORD").unwrap_or_default();
    let port: u16 = std::env::var("SMTP_PORT")
        .ok()
        .and_then(|p| p.parse().ok())
        .unwrap_or(587);
    let from =
        std::env::var("SMTP_FROM").unwrap_or_else(|_| "support@palodurosystems.io".to_string());
    let tls_mode_raw = std::env::var("SMTP_TLS").ok().unwrap_or_default();

    if host.is_empty() {
        warn!("Email not configured; skipping send");
        return Err(AppError::new(
            StatusCode::INTERNAL_SERVER_ERROR,
            "Email not configured",
        ));
    }

    // lettre::AsyncSmtpTransport::relay uses implicit TLS ("SMTPS", typically port 465).
    // Port 587 is usually STARTTLS, so use starttls_relay by default (or auto-switch based on port).
    let tls_mode = if tls_mode_raw.is_empty() {
        if port == 465 { "wrapper" } else { "starttls" }
    } else {
        tls_mode_raw.trim()
    }
    .to_ascii_lowercase();

    let email_builder =
        Message::builder()
            .from(from.parse::<Mailbox>().map_err(|_| {
                AppError::new(StatusCode::INTERNAL_SERVER_ERROR, "Invalid FROM email")
            })?)
            .to(to
                .parse::<Mailbox>()
                .map_err(|_| AppError::new(StatusCode::BAD_REQUEST, "Invalid recipient email"))?)
            .subject(subject);

    let html = body_html.unwrap_or_else(|| render_plain_email_html(subject, body_text));
    let email = email_builder
        .multipart(MultiPart::alternative_plain_html(
            body_text.to_string(),
            html,
        ))
        .map_err(|_| AppError::new(StatusCode::INTERNAL_SERVER_ERROR, "Failed to build email"))?;

    if (username.is_empty() && !password.is_empty())
        || (!username.is_empty() && password.is_empty())
    {
        warn!("SMTP_USERNAME/SMTP_PASSWORD partially configured; authentication may fail");
    }

    let mailer_builder = match tls_mode.as_str() {
        "starttls" => AsyncSmtpTransport::<Tokio1Executor>::starttls_relay(&host),
        "wrapper" | "tls" | "smtps" => AsyncSmtpTransport::<Tokio1Executor>::relay(&host),
        "none" | "plain" | "plaintext" => Ok(
            AsyncSmtpTransport::<Tokio1Executor>::builder_dangerous(&host),
        ),
        _ => {
            return Err(AppError::new(
                StatusCode::INTERNAL_SERVER_ERROR,
                "Invalid SMTP_TLS (use starttls|wrapper|none)",
            ));
        }
    }
    .map_err(|_| AppError::new(StatusCode::INTERNAL_SERVER_ERROR, "Failed to create mailer"))?
    .port(port);

    let mailer_builder = if username.is_empty() || password.is_empty() {
        mailer_builder
    } else {
        mailer_builder.credentials(Credentials::new(username, password))
    };

    let mailer = mailer_builder.build();

    info!(to = %to, subject = %subject, host = %host, port = port, tls = %tls_mode, "sending email");
    mailer.send(email).await.map_err(|e| {
        error!(to = %to, error = %e, "failed to send email");
        AppError::new(
            StatusCode::INTERNAL_SERVER_ERROR,
            format!("Failed to send email: {}", e),
        )
    })?;
    info!(to = %to, subject = %subject, "email sent");

    Ok(())
}

fn base_url() -> String {
    std::env::var("BASE_URL").unwrap_or_else(|_| "http://localhost:3000".to_string())
}

fn render_template(state: &AppState, name: &str, mut ctx: TeraContext) -> AppResult<Html<String>> {
    insert_app_template_globals(&mut ctx);
    let rendered = state
        .templates
        .render(name, &ctx)
        .map_err(|e| AppError::new(StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))?;
    Ok(Html(rendered))
}

fn render_login_error(state: &AppState, message: &str) -> AppResult<Html<String>> {
    let mut ctx = TeraContext::new();
    ctx.insert("error", message);
    render_template(state, "login.html", ctx)
}
