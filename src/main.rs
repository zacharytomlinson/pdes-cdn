use std::{collections::HashMap, net::SocketAddr, path::PathBuf, time::Duration as StdDuration};

use anyhow::Context;
use argon2::{
    password_hash::{PasswordHash, PasswordHasher, PasswordVerifier, SaltString},
    Argon2,
};
use axum::{
    body::Body,
    extract::{DefaultBodyLimit, FromRef, Multipart, Path, State},
    http::{header, HeaderMap, StatusCode},
    response::{Html, IntoResponse, Redirect, Response},
    routing::{get, post},
    Form, Router,
};
use axum_extra::extract::cookie::{Cookie, Key};
use axum_extra::extract::SignedCookieJar;
use chrono::{DateTime, Duration as ChronoDuration, Utc};
use dotenvy::dotenv;
use lettre::{
    message::{Mailbox, Message},
    transport::smtp::authentication::Credentials,
    AsyncSmtpTransport, AsyncTransport, Tokio1Executor,
};
use moka::future::Cache;
use rand::{distributions::Alphanumeric, rngs::OsRng, Rng};
use serde::Deserialize;
use sqlx::{postgres::PgPoolOptions, FromRow, PgPool};
use sha2::{Digest, Sha512};
use tera::{Context as TeraContext, Tera};
use tokio::fs::File;
use tokio::io::AsyncWriteExt;
use tokio_util::io::ReaderStream;
use uuid::Uuid;

const SESSION_COOKIE: &str = "pdes_session";

#[derive(Clone)]
struct AppState {
    pool: PgPool,
    templates: Tera,
    cache_dir: PathBuf,
    cookie_key: Key,
    file_list_cache: Cache<String, Vec<CachedFile>>,
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
}

impl AppError {
    fn new(status: StatusCode, message: impl Into<String>) -> Self {
        Self {
            status,
            message: message.into(),
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
        }
    }
}

impl IntoResponse for AppError {
    fn into_response(self) -> Response {
        (self.status, self.message).into_response()
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

#[derive(Debug, Clone, serde::Serialize)]
struct DirSummary {
    path: String,
    file_count: usize,
    total_bytes: i64,
}

#[derive(Debug, Clone, FromRow, serde::Serialize)]
struct ProjectToken {
    id: Uuid,
    project: String,
    bucket: String,
    token: String,
    created_at: DateTime<Utc>,
}

#[derive(Debug, Clone, serde::Serialize)]
struct BucketView {
    id: Uuid,
    project: String,
    bucket: String,
    token: String,
    created_at: DateTime<Utc>,
    files: Vec<CachedFile>,
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
struct CreateBucketForm {
    project: String,
    bucket: String,
}

#[derive(Debug, Deserialize)]
struct InviteUserForm {
    email: String,
}

#[tokio::main]
async fn main() -> AppResult<()> {
    dotenv().ok();

    let database_url = std::env::var("DATABASE_URL").unwrap_or_else(|_| "postgres://postgres:postgres@localhost:5432/pdes_cdn".to_string());
    let cache_dir = std::env::var("CACHE_DIR").unwrap_or_else(|_| "cache".to_string());
    let max_upload_bytes: usize = std::env::var("MAX_UPLOAD_BYTES")
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(200 * 1024 * 1024);
    let cookie_secret =
        std::env::var("COOKIE_SECRET").unwrap_or_else(|_| "dev-secret-change-me".to_string());

    let templates = Tera::new("templates/**/*").context("loading templates")?;
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

    let mut hasher = Sha512::new();
    hasher.update(cookie_secret.as_bytes());
    let digest = hasher.finalize();
    let mut key_bytes = [0u8; 64];
    key_bytes.copy_from_slice(&digest[..]);

    let state = AppState {
        pool,
        templates,
        cache_dir,
        cookie_key: Key::from(&key_bytes),
        file_list_cache: Cache::builder()
            .max_capacity(64)
            .time_to_live(StdDuration::from_secs(30))
            .build(),
        file_by_id_cache: Cache::builder()
            .max_capacity(512)
            .time_to_live(StdDuration::from_secs(60))
            .build(),
    };

    let app = Router::new()
        .route("/", get(root))
        .route("/login", get(show_login).post(do_login))
        .route("/logout", get(do_logout))
        .route("/reset", get(show_reset_request).post(do_reset_request))
        .route(
            "/reset/confirm/:token",
            get(show_reset_confirm).post(do_reset_confirm),
        )
        .route("/portal", get(portal))
        .route("/files", get(list_files))
        .route("/files/:id/delete", post(delete_file))
        .route("/cache-summary", get(cache_summary))
        .route("/buckets", axum::routing::post(create_bucket))
        .route("/buckets/:id/delete", post(delete_bucket))
        .route("/invite", axum::routing::post(invite_user))
        .route("/upload", axum::routing::post(upload_file))
        .route("/api/upload", axum::routing::post(api_upload))
        .route("/download/:id", get(download_file))
        .layer(DefaultBodyLimit::max(max_upload_bytes))
        .with_state(state);

    let addr = SocketAddr::from(([0, 0, 0, 0], 3000));
    println!("Listening on http://{}", addr);
    let listener = tokio::net::TcpListener::bind(addr)
        .await
        .map_err(AppError::from)?;
    axum::serve(listener, app).await.map_err(AppError::from)?;

    Ok(())
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
    let user = sqlx::query_as::<_, User>(
        "select id, email, password_hash, permission, reset_token, reset_token_expires, must_reset from users where email = $1",
    )
    .bind(&form.email)
    .fetch_optional(&state.pool)
    .await?;

    let Some(user) = user else {
        return render_login_error(&state, "Invalid credentials")
            .map(|html| html.into_response());
    };

    let parsed_hash =
        PasswordHash::new(&user.password_hash).map_err(|_| AppError::new(StatusCode::BAD_REQUEST, "Stored password hash is invalid"))?;
    if Argon2::default()
        .verify_password(form.password.as_bytes(), &parsed_hash)
        .is_err()
    {
        return render_login_error(&state, "Invalid credentials")
            .map(|html| html.into_response());
    }

    if user.must_reset {
        let token = issue_reset_token(&state, user.id).await?;
        return Ok(Redirect::to(&format!("/reset/confirm/{}", token)).into_response());
    }

    let cookie = Cookie::build((SESSION_COOKIE, user.id.to_string()))
        .path("/")
        .http_only(true)
        .build();
    let jar = jar.add(cookie);
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
    let token: String = rand::thread_rng()
        .sample_iter(&Alphanumeric)
        .take(32)
        .map(char::from)
        .collect();
    let expires = Utc::now() + ChronoDuration::minutes(30);

    let _ = sqlx::query(
        "update users set reset_token = $1, reset_token_expires = $2 where email = $3",
    )
    .bind(&token)
    .bind(expires)
    .bind(&form.email)
    .execute(&state.pool)
    .await?;

    let mut ctx = TeraContext::new();
    ctx.insert("message", "If the account exists, a reset link was prepared.");
    ctx.insert("token_preview", &token);
    render_template(&state, "reset_request.html", ctx)
}

async fn show_reset_confirm(
    Path(token): Path<String>,
    State(state): State<AppState>,
) -> AppResult<impl IntoResponse> {
    ensure_valid_token(&state, &token).await?;
    let mut ctx = TeraContext::new();
    ctx.insert("token", &token);
    render_template(&state, "reset_confirm.html", ctx)
}

async fn do_reset_confirm(
    Path(token): Path<String>,
    State(state): State<AppState>,
    Form(form): Form<ResetConfirmForm>,
) -> AppResult<impl IntoResponse> {
    if form.password != form.confirm_password {
        return Err(AppError::new(
            StatusCode::BAD_REQUEST,
            "Passwords do not match",
        ));
    }

    let user = ensure_valid_token(&state, &token).await?;
    let salt = SaltString::generate(&mut OsRng);
    let password_hash = Argon2::default()
        .hash_password(form.password.as_bytes(), &salt)
        .map_err(|_| AppError::new(StatusCode::BAD_REQUEST, "Could not hash password"))?
        .to_string();

    sqlx::query(
        "update users set password_hash = $1, reset_token = null, reset_token_expires = null, must_reset = false where id = $2",
    )
    .bind(password_hash)
    .bind(user.id)
    .execute(&state.pool)
    .await?;

    Ok(Redirect::to("/login"))
}

async fn portal(State(state): State<AppState>, jar: SignedCookieJar) -> AppResult<Response> {
    let Some(user) = current_user(&state, &jar).await? else {
        let jar = jar.remove(Cookie::from(SESSION_COOKIE));
        return Ok((jar, Redirect::to("/login")).into_response());
    };

    let buckets = if is_writer(&user) {
        fetch_buckets(&state).await?
    } else {
        Vec::new()
    };

    let mut ctx = TeraContext::new();
    ctx.insert("user_email", &user.email);
    ctx.insert("files_endpoint", "/files");
    ctx.insert(
        "cache_summary_endpoint",
        "/cache-summary",
    );
    ctx.insert("can_write", &is_writer(&user));
    ctx.insert("buckets", &buckets);
    let html = render_template(&state, "portal.html", ctx)?;
    Ok(html.into_response())
}

async fn list_files(
    State(state): State<AppState>,
    jar: SignedCookieJar,
    headers: HeaderMap,
) -> AppResult<Response> {
    let Some(user) = current_user(&state, &jar).await? else {
        let jar = jar.remove(Cookie::from(SESSION_COOKIE));
        return Ok((jar, Redirect::to("/login")).into_response());
    };

    let files = fetch_files(&state).await?;
    let mut ctx = TeraContext::new();
    ctx.insert("files", &files);
    ctx.insert("can_write", &is_writer(&user));
    if is_writer(&user) {
        let buckets = fetch_buckets(&state).await?;
        let bucket_tree = build_bucket_views(&files, &buckets)?;
        ctx.insert("bucket_tree", &bucket_tree);
    }

    let html = if is_hx_request(&headers) {
        render_template(&state, "partials/file_table.html", ctx)?
    } else {
        let buckets = if is_writer(&user) {
            fetch_buckets(&state).await?
        } else {
            Vec::new()
        };
        ctx.insert("user_email", &user.email);
        ctx.insert("files_endpoint", "/files");
        ctx.insert("cache_summary_endpoint", "/cache-summary");
        ctx.insert("can_write", &is_writer(&user));
        ctx.insert("buckets", &buckets);
        render_template(&state, "portal.html", ctx)?
    };

    Ok((jar, html).into_response())
}

async fn cache_summary(
    State(state): State<AppState>,
    jar: SignedCookieJar,
    headers: HeaderMap,
) -> AppResult<Response> {
    let Some(user) = current_user(&state, &jar).await? else {
        let jar = jar.remove(Cookie::from(SESSION_COOKIE));
        return Ok((jar, Redirect::to("/login")).into_response());
    };

    let files = fetch_files(&state).await?;
    let directories = summarize_directories(&files);
    let mut ctx = TeraContext::new();
    ctx.insert("directories", &directories);
    if is_writer(&user) {
        let buckets = fetch_buckets(&state).await?;
        let bucket_tree = build_bucket_views(&files, &buckets)?;
        ctx.insert("bucket_tree", &bucket_tree);
    }

    let html = if is_hx_request(&headers) {
        render_template(&state, "partials/cache_summary.html", ctx)?
    } else {
        let buckets = if is_writer(&user) {
            fetch_buckets(&state).await?
        } else {
            Vec::new()
        };
        ctx.insert("user_email", &user.email);
        ctx.insert("files_endpoint", "/files");
        ctx.insert("cache_summary_endpoint", "/cache-summary");
        ctx.insert("can_write", &is_writer(&user));
        ctx.insert("buckets", &buckets);
        render_template(&state, "portal.html", ctx)?
    };

    Ok((jar, html).into_response())
}

async fn upload_file(
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

    let mut bucket_path: Option<String> = None;
    let mut handled = false;
    while let Some(field) = multipart
        .next_field()
        .await
        .map_err(|_| AppError::new(StatusCode::BAD_REQUEST, "Invalid upload payload"))?
    {
        match field.name() {
            Some("file") => {
                save_upload(&state, field, bucket_path.clone(), None).await?;
                handled = true;
                break;
            }
            Some("bucket_path") => {
                if let Some(val) = field
                    .text()
                    .await
                    .map_err(|_| AppError::new(StatusCode::BAD_REQUEST, "Invalid bucket value"))?
                    .split_whitespace()
                    .next()
                    .map(|s| sanitize_bucket(s))
                {
                    bucket_path = if val.is_empty() || val == "root" {
                        None
                    } else {
                        Some(val)
                    };
                }
            }
            _ => {}
        }
    }

    if !handled {
        return Err(AppError::new(
            StatusCode::BAD_REQUEST,
            "No file provided",
        ));
    }

    state.file_list_cache.invalidate_all();
    state.file_by_id_cache.invalidate_all();
    Ok((jar, Redirect::to("/portal")).into_response())
}

async fn api_upload(
    State(state): State<AppState>,
    headers: HeaderMap,
    mut multipart: Multipart,
) -> AppResult<Response> {
    let token = extract_bearer(&headers).ok_or_else(|| {
        AppError::new(StatusCode::UNAUTHORIZED, "Bearer token required for API upload")
    })?;

    let record = sqlx::query_as::<_, ProjectToken>(
        "select id, project, bucket, token, created_at from project_tokens where token = $1",
    )
    .bind(&token)
    .fetch_optional(&state.pool)
    .await?;

    let Some(record) = record else {
        return Err(AppError::new(
            StatusCode::UNAUTHORIZED,
            "Invalid upload token",
        ));
    };

    let mut handled = false;
    while let Some(field) = multipart
        .next_field()
        .await
        .map_err(|_| AppError::new(StatusCode::BAD_REQUEST, "Invalid upload payload"))?
    {
        if field.name() == Some("file") {
            let bucket_path = format!("{}/{}", sanitize_bucket(&record.project), sanitize_bucket(&record.bucket));
            save_upload(&state, field, Some(bucket_path), Some(record.token.clone())).await?;
            handled = true;
            break;
        }
    }

    if !handled {
        return Err(AppError::new(
            StatusCode::BAD_REQUEST,
            "No file provided",
        ));
    }

    state.file_list_cache.invalidate_all();
    state.file_by_id_cache.invalidate_all();
    Ok(StatusCode::OK.into_response())
}

async fn delete_file(
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

    let file = fetch_file_by_id(&state, id).await?;
    let Some(file) = file else {
        return Err(AppError::new(StatusCode::NOT_FOUND, "File not found"));
    };

    if let Some(path) = resolve_cache_path(&state, &file.local_path).await? {
        let _ = tokio::fs::remove_file(path).await;
    }

    sqlx::query("delete from cached_files where id = $1")
        .bind(file.id)
        .execute(&state.pool)
        .await?;

    state.file_list_cache.invalidate_all();
    state.file_by_id_cache.invalidate_all();

    Ok((jar, Redirect::to("/portal")).into_response())
}

async fn delete_bucket(
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

    let record = sqlx::query_as::<_, ProjectToken>(
        "select id, project, bucket, token, created_at from project_tokens where id = $1",
    )
    .bind(id)
    .fetch_optional(&state.pool)
    .await?;

    let Some(record) = record else {
        return Err(AppError::new(StatusCode::NOT_FOUND, "Bucket not found"));
    };

    let prefix = format!("{}/{}", sanitize_bucket(&record.project), sanitize_bucket(&record.bucket));
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

    let bucket_dir = state.cache_dir.join(&prefix);
    if bucket_dir.starts_with(&state.cache_dir) {
        let _ = tokio::fs::remove_dir_all(&bucket_dir).await;
    }

    sqlx::query("delete from project_tokens where id = $1")
        .bind(record.id)
        .execute(&state.pool)
        .await?;

    state.file_list_cache.invalidate_all();
    state.file_by_id_cache.invalidate_all();

    Ok((jar, Redirect::to("/portal")).into_response())
}

async fn invite_user(
    State(state): State<AppState>,
    jar: SignedCookieJar,
    Form(form): Form<InviteUserForm>,
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

    let email = form.email.trim().to_lowercase();
    if email.is_empty() {
        return Err(AppError::new(
            StatusCode::BAD_REQUEST,
            "Email is required",
        ));
    }

    let existing = sqlx::query_scalar::<_, Uuid>("select id from users where email = $1")
        .bind(&email)
        .fetch_optional(&state.pool)
        .await?;
    if existing.is_some() {
        return Err(AppError::new(
            StatusCode::BAD_REQUEST,
            "User already exists",
        ));
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

    let reset_token = issue_reset_token(&state, id).await?;

    let reset_link = format!("/reset/confirm/{}", reset_token);
    let body = format!(
        "Welcome to the CDN portal.\n\nTemporary password: {pwd}\nLogin: {login}\nReset link (required on first login): {link}\n\nIf you did not expect this, please contact support@palodurosystems.io.",
        pwd = placeholder,
        login = email,
        link = reset_link
    );
    let subject = "Your CDN portal access";
    let _ = send_email(&email, subject, &body).await;

    let buckets = fetch_buckets(&state).await?;
    let mut ctx = TeraContext::new();
    ctx.insert("user_email", &user.email);
    ctx.insert("files_endpoint", "/files");
    ctx.insert("cache_summary_endpoint", "/cache-summary");
    ctx.insert("can_write", &true);
    ctx.insert("buckets", &buckets);
    ctx.insert("new_invite_email", &email);
    let html = render_template(&state, "portal.html", ctx)?;
    Ok((jar, html).into_response())
}

async fn download_file(
    Path(id): Path<Uuid>,
    State(state): State<AppState>,
    jar: SignedCookieJar,
) -> AppResult<Response> {
    let Some(_user) = current_user(&state, &jar).await? else {
        let jar = jar.remove(Cookie::from(SESSION_COOKIE));
        return Ok((jar, Redirect::to("/login")).into_response());
    };

    let file = fetch_file_by_id(&state, id).await?;

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

    if let Some(expires) = user.reset_token_expires {
        if expires < Utc::now() {
            return Err(AppError::new(
                StatusCode::BAD_REQUEST,
                "Reset token has expired",
            ));
        }
    }

    Ok(user)
}

fn is_hx_request(headers: &HeaderMap) -> bool {
    headers
        .get("HX-Request")
        .and_then(|h| h.to_str().ok())
        .is_some()
}

async fn fetch_files(state: &AppState) -> AppResult<Vec<CachedFile>> {
    if let Some(files) = state.file_list_cache.get("all").await {
        return Ok(files);
    }

    let files = sqlx::query_as::<_, CachedFile>(
        "select id, filename, local_path, size_bytes, created_at from cached_files order by created_at desc",
    )
    .fetch_all(&state.pool)
    .await?;
    state
        .file_list_cache
        .insert("all".to_string(), files.clone())
        .await;
    Ok(files)
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

async fn fetch_buckets(state: &AppState) -> AppResult<Vec<ProjectToken>> {
    let tokens = sqlx::query_as::<_, ProjectToken>(
        "select id, project, bucket, token, created_at from project_tokens order by project, bucket",
    )
    .fetch_all(&state.pool)
    .await?;
    Ok(tokens)
}

fn build_bucket_views(
    files: &[CachedFile],
    buckets: &[ProjectToken],
) -> AppResult<Vec<BucketView>> {
    let mut views: Vec<BucketView> = buckets
        .iter()
        .map(|b| BucketView {
            id: b.id,
            project: b.project.clone(),
            bucket: b.bucket.clone(),
            token: b.token.clone(),
            created_at: b.created_at,
            files: Vec::new(),
        })
        .collect();

    let mut root_files: Vec<CachedFile> = Vec::new();
    for file in files {
        if let Some(path) = bucket_path_from_local(&file.local_path) {
            if let Some(view) = views
                .iter_mut()
                .find(|v| format!("{}/{}", v.project, v.bucket) == path)
            {
                view.files.push(file.clone());
                continue;
            }
        }
        root_files.push(file.clone());
    }

    if !root_files.is_empty() {
        views.push(BucketView {
            id: Uuid::nil(),
            project: "root".to_string(),
            bucket: "root".to_string(),
            token: "-".to_string(),
            created_at: Utc::now(),
            files: root_files,
        });
    }

    views.sort_by(|a, b| a.project.cmp(&b.project).then(a.bucket.cmp(&b.bucket)));
    Ok(views)
}

fn summarize_directories(files: &[CachedFile]) -> Vec<DirSummary> {
    let mut map: HashMap<String, (usize, i64)> = HashMap::new();
    for file in files {
        let dir = PathBuf::from(&file.local_path)
            .parent()
            .map(|p| p.display().to_string())
            .filter(|s| !s.is_empty())
            .unwrap_or_else(|| ".".to_string());
        let entry = map.entry(dir).or_insert((0, 0));
        entry.0 += 1;
        if let Some(size) = file.size_bytes {
            entry.1 += size;
        }
    }

    let mut out: Vec<DirSummary> = map
        .into_iter()
        .map(|(path, (file_count, total_bytes))| DirSummary {
            path,
            file_count,
            total_bytes,
        })
        .collect();
    out.sort_by(|a, b| b.file_count.cmp(&a.file_count));
    out
}

async fn save_upload(
    state: &AppState,
    mut field: axum::extract::multipart::Field<'_>,
    bucket_path: Option<String>,
    token_used: Option<String>,
) -> AppResult<()> {
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
    if ext != "bin" && ext != "elf" && ext != "msi" && ext != "deb" {
        return Err(AppError::new(
            StatusCode::BAD_REQUEST,
            "Only .bin, .elf, .msi, and .deb uploads are allowed",
        ));
    }

    let file_id = Uuid::new_v4();
    let prefix = bucket_path.unwrap_or_else(|| "root".to_string());
    let stored_name = format!("{}-{}", file_id, clean);
    let relative_path = format!("{}/{}", prefix, stored_name);
    let dest_path = state.cache_dir.join(&relative_path);

    if let Some(parent) = dest_path.parent() {
        tokio::fs::create_dir_all(parent)
            .await
            .map_err(|_| AppError::new(StatusCode::INTERNAL_SERVER_ERROR, "Could not create directory"))?;
    }

    let mut dest = File::create(&dest_path)
        .await
        .map_err(|_| AppError::new(StatusCode::INTERNAL_SERVER_ERROR, "Could not create file"))?;
    let mut size: i64 = 0;

    while let Some(chunk) = field
        .chunk()
        .await
        .map_err(|_| AppError::new(StatusCode::BAD_REQUEST, "Failed reading upload"))?
    {
        size += chunk.len() as i64;
        dest.write_all(&chunk)
            .await
            .map_err(|_| AppError::new(StatusCode::INTERNAL_SERVER_ERROR, "Failed writing upload"))?;
    }

    sqlx::query(
        "insert into cached_files (id, filename, local_path, size_bytes) values ($1, $2, $3, $4)",
    )
    .bind(file_id)
    .bind(clean)
    .bind(relative_path)
    .bind(size)
    .execute(&state.pool)
    .await?;

    if let Some(token) = token_used {
        let _ = sqlx::query("update project_tokens set created_at = created_at where token = $1")
            .bind(token)
            .execute(&state.pool)
            .await;
    }

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

fn bucket_path_from_local(local_path: &str) -> Option<String> {
    let mut parts = local_path.split('/');
    let project = parts.next()?;
    let bucket = parts.next()?;
    if project.is_empty() || bucket.is_empty() {
        None
    } else {
        Some(format!("{}/{}", project, bucket))
    }
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

async fn issue_reset_token(state: &AppState, user_id: Uuid) -> AppResult<String> {
    let token: String = rand::thread_rng()
        .sample_iter(&Alphanumeric)
        .take(32)
        .map(char::from)
        .collect();
    let expires = Utc::now() + ChronoDuration::minutes(30);

    sqlx::query(
        "update users set reset_token = $1, reset_token_expires = $2, must_reset = true where id = $3",
    )
    .bind(&token)
    .bind(expires)
    .bind(user_id)
    .execute(&state.pool)
    .await?;

    Ok(token)
}

async fn send_email(to: &str, subject: &str, body: &str) -> AppResult<()> {
    let host = std::env::var("SMTP_HOST").unwrap_or_default();
    let username = std::env::var("SMTP_USERNAME").unwrap_or_default();
    let password = std::env::var("SMTP_PASSWORD").unwrap_or_default();
    let port: u16 = std::env::var("SMTP_PORT")
        .ok()
        .and_then(|p| p.parse().ok())
        .unwrap_or(587);
    let from = std::env::var("SMTP_FROM").unwrap_or_else(|_| "support@palodurosystems.io".to_string());

    if host.is_empty() {
        return Err(AppError::new(
            StatusCode::INTERNAL_SERVER_ERROR,
            "Email not configured",
        ));
    }

    let email = Message::builder()
        .from(from.parse::<Mailbox>().map_err(|_| AppError::new(StatusCode::INTERNAL_SERVER_ERROR, "Invalid FROM email"))?)
        .to(to.parse::<Mailbox>().map_err(|_| AppError::new(StatusCode::BAD_REQUEST, "Invalid recipient email"))?)
        .subject(subject)
        .body(body.to_string())
        .map_err(|_| AppError::new(StatusCode::INTERNAL_SERVER_ERROR, "Failed to build email"))?;

    let creds = Credentials::new(username, password);
    let mailer = AsyncSmtpTransport::<Tokio1Executor>::relay(&host)
        .map_err(|_| AppError::new(StatusCode::INTERNAL_SERVER_ERROR, "Failed to create mailer"))?
        .port(port)
        .credentials(creds)
        .build();

    mailer
        .send(email)
        .await
        .map_err(|e| AppError::new(StatusCode::INTERNAL_SERVER_ERROR, format!("Failed to send email: {}", e)))?;

    Ok(())
}

fn render_template(
    state: &AppState,
    name: &str,
    mut ctx: TeraContext,
) -> AppResult<Html<String>> {
    ctx.insert("app_name", "Palo Duro Embedded Systems");
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
async fn create_bucket(
    State(state): State<AppState>,
    jar: SignedCookieJar,
    Form(form): Form<CreateBucketForm>,
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

    let project = sanitize_bucket(&form.project);
    let bucket = sanitize_bucket(&form.bucket);
    if project.is_empty() || bucket.is_empty() {
        return Err(AppError::new(
            StatusCode::BAD_REQUEST,
            "Project and bucket are required",
        ));
    }

    let token: String = rand::thread_rng()
        .sample_iter(&Alphanumeric)
        .take(48)
        .map(char::from)
        .collect();
    let id = Uuid::new_v4();

    sqlx::query(
        "insert into project_tokens (id, project, bucket, token) values ($1, $2, $3, $4)",
    )
    .bind(id)
    .bind(&project)
    .bind(&bucket)
    .bind(&token)
    .execute(&state.pool)
    .await
    .map_err(|e| AppError::new(StatusCode::BAD_REQUEST, format!("Could not create bucket: {}", e)))?;

    state.file_list_cache.invalidate_all();
    state.file_by_id_cache.invalidate_all();

    let buckets = fetch_buckets(&state).await?;
    let mut ctx = TeraContext::new();
    ctx.insert("user_email", &user.email);
    ctx.insert("files_endpoint", "/files");
    ctx.insert("cache_summary_endpoint", "/cache-summary");
    ctx.insert("can_write", &true);
    ctx.insert("buckets", &buckets);
    ctx.insert("new_token", &token);
    ctx.insert("new_bucket_path", &format!("{}/{}", project, bucket));

    let html = render_template(&state, "portal.html", ctx)?;
    Ok((jar, html).into_response())
}
