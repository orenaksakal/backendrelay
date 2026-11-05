delete from rust_realtime_room_events where kind = 'byop-wal';

drop index rust_realtime_room_events_wal_idx;

create unique index rust_realtime_room_events_wal_idx
  on rust_realtime_room_events (kind, app_id, room_id)
  where kind = 'wal';

alter table rust_realtime_room_events
  drop constraint rust_realtime_room_events_kind_check;

alter table rust_realtime_room_events
  add constraint rust_realtime_room_events_kind_check check (
    kind in (
      'presence', 'broadcast', 'stream-append', 'stream-disconnect', 'sse-push',
      'wal'
    )
  );
