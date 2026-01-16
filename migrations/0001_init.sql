-- Core schema for CDN node portal auth and cached files
create table if not exists users (
    id uuid primary key,
    email text not null unique,
    password_hash text not null,
    reset_token text,
    reset_token_expires timestamptz,
    created_at timestamptz not null default now()
);

create table if not exists cached_files (
    id uuid primary key,
    filename text not null,
    local_path text not null,
    size_bytes bigint,
    created_at timestamptz not null default now()
);

create index if not exists idx_cached_files_created_at on cached_files(created_at desc);
