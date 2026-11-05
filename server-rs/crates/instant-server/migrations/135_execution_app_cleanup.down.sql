drop trigger if exists apps_capture_execution_cleanup on apps;
drop function if exists capture_execution_app_cleanup_tombstone();
drop table if exists execution_app_cleanup_tombstones;
