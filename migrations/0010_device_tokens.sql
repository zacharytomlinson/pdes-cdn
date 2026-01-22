-- Long-lived, app-scoped bearer tokens for desktop clients.
-- Store only SHA256(token). These tokens are intended for update checks on startup.
create table if not exists device_tokens (
    id uuid primary key,
    app_id uuid not null references apps(id) on delete cascade,
    os text not null default 'unknown',
    arch text not null default 'x86_64',
    name text not null default '',
    token_sha256 text not null unique,
    created_at timestamptz not null default now(),
    last_used_at timestamptz,
    revoked_at timestamptz,
    expires_at timestamptz
);

create index if not exists idx_device_tokens_app_id on device_tokens(app_id);
create index if not exists idx_device_tokens_expires_at on device_tokens(expires_at);
