-- Short-lived bootstrap tokens for installer-time package acquisition.
-- Store only SHA256(token). Tokens are scoped to a specific cached_files pkg_file_id.
create table if not exists bootstrap_tokens (
    id uuid primary key,
    minted_by_user_id uuid references users(id) on delete set null,
    pkg_file_id uuid not null references cached_files(id) on delete restrict,
    token_sha256 text not null unique,
    created_at timestamptz not null default now(),
    expires_at timestamptz not null,
    max_uses integer not null default 10,
    use_count integer not null default 0,
    last_used_at timestamptz
);

create index if not exists idx_bootstrap_tokens_expires_at on bootstrap_tokens(expires_at);
create index if not exists idx_bootstrap_tokens_pkg_file_id on bootstrap_tokens(pkg_file_id);
create index if not exists idx_bootstrap_tokens_minted_by_user_id on bootstrap_tokens(minted_by_user_id);
