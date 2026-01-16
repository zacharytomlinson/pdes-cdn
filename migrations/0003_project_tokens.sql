create table if not exists project_tokens (
    id uuid primary key,
    project text not null,
    bucket text not null,
    token text not null unique,
    created_at timestamptz not null default now(),
    unique (project, bucket)
);
