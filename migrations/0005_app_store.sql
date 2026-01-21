-- App store domain: apps, packages, firmware, and user assignments
create table if not exists apps (
    id uuid primary key,
    slug text not null unique,
    name text not null,
    description text,
    created_at timestamptz not null default now()
);

create table if not exists app_packages (
    id uuid primary key,
    app_id uuid not null references apps(id) on delete cascade,
    version text not null,
    os text not null default 'unknown',
    arch text not null default 'x86_64',
    pkg_file_id uuid not null references cached_files(id) on delete restrict,
    pkg_sha256 text not null,
    created_at timestamptz not null default now(),
    unique (app_id, version, os, arch)
);

create table if not exists firmware_releases (
    id uuid primary key,
    app_id uuid not null references apps(id) on delete cascade,
    channel text not null default 'stable',
    version text not null,
    file_id uuid not null references cached_files(id) on delete restrict,
    sha256 text not null,
    created_at timestamptz not null default now(),
    unique (app_id, channel, version)
);

create table if not exists app_assignments (
    app_id uuid not null references apps(id) on delete cascade,
    user_id uuid not null references users(id) on delete cascade,
    created_at timestamptz not null default now(),
    primary key (app_id, user_id)
);

create index if not exists idx_app_assignments_user on app_assignments(user_id);
create index if not exists idx_app_packages_app_created_at on app_packages(app_id, created_at desc);
create index if not exists idx_firmware_releases_app_created_at on firmware_releases(app_id, created_at desc);
create index if not exists idx_app_packages_sha256 on app_packages(app_id, pkg_sha256);
create index if not exists idx_firmware_releases_sha256 on firmware_releases(app_id, sha256);
