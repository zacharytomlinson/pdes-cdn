-- Add a permission column to distinguish reader vs writer roles
alter table users add column if not exists permission text not null default 'reader';

-- Optional backfill to writer for existing admin users; adjust as needed.
update users set permission = 'writer' where email ilike '%admin%';
