-- App-scoped upload tokens for CI (least-privilege).
-- Store only SHA256(token). These tokens are only valid for uploading/registering packages for a single app.
create table if not exists app_upload_tokens (
    id uuid primary key,
    app_id uuid not null references apps(id) on delete cascade,
    name text not null default '',
    token_sha256 text not null unique,
    created_at timestamptz not null default now(),
    last_used_at timestamptz,
    revoked_at timestamptz,
    expires_at timestamptz
);

create index if not exists idx_app_upload_tokens_app_id on app_upload_tokens(app_id);
create index if not exists idx_app_upload_tokens_expires_at on app_upload_tokens(expires_at);
