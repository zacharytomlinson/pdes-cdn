-- API tokens for desktop apps (bearer auth). Store only SHA256(token).
create table if not exists api_tokens (
    id uuid primary key,
    user_id uuid not null references users(id) on delete cascade,
    name text not null default '',
    token_sha256 text not null unique,
    created_at timestamptz not null default now(),
    last_used_at timestamptz
);

create index if not exists idx_api_tokens_user_id on api_tokens(user_id);
