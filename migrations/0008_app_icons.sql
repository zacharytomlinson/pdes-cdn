alter table apps
    add column if not exists icon_file_id uuid references cached_files(id);

create index if not exists idx_apps_icon_file_id on apps(icon_file_id);
