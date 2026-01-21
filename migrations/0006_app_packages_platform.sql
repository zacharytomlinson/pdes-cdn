-- Support multiple packages per app version by OS/arch
alter table app_packages
    add column if not exists os text not null default 'unknown';

alter table app_packages
    add column if not exists arch text not null default 'x86_64';

alter table app_packages
    drop constraint if exists app_packages_app_id_version_key;

alter table app_packages
    drop constraint if exists app_packages_app_id_version_os_arch_key;

alter table app_packages
    add constraint app_packages_app_id_version_os_arch_key unique (app_id, version, os, arch);
