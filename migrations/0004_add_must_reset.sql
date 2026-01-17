alter table users
    add column if not exists must_reset boolean not null default false;
