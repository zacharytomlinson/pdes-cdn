-- Optional per-user firmware channel access per app.
-- Notes:
-- - "stable" is treated as implicitly allowed for assigned users in application code.
-- - Rows in this table grant access to additional channels (e.g. beta, dev).

create table if not exists firmware_channel_access (
    app_id uuid not null references apps(id) on delete cascade,
    user_id uuid not null references users(id) on delete cascade,
    channel text not null,
    created_at timestamptz not null default now(),
    primary key (app_id, user_id, channel)
);

create index if not exists idx_firmware_channel_access_user on firmware_channel_access(user_id);
create index if not exists idx_firmware_channel_access_app_user on firmware_channel_access(app_id, user_id);
