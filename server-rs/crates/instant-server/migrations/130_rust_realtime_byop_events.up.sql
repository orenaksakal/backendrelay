alter table rust_realtime_room_events
  drop constraint rust_realtime_room_events_kind_check;

alter table rust_realtime_room_events
  add constraint rust_realtime_room_events_kind_check check (
    kind in (
      'presence', 'broadcast', 'stream-append', 'stream-disconnect', 'sse-push',
      'wal', 'byop-wal'
    )
  );

drop index rust_realtime_room_events_wal_idx;

create unique index rust_realtime_room_events_wal_idx
  on rust_realtime_room_events (kind, app_id, room_id)
  where kind in ('wal', 'byop-wal');
