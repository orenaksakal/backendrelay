-- Restore the addresses seeded by migrations 62 and 105, only where the row
-- still carries the neutral value written by this migration.
update instant_users
   set email = 'hello+ephemeralappsdev@instantdb.com'
 where id = 'dda67f26-6962-479e-b476-60e4b6963b74'::uuid
   and email = 'ephemeral-apps-dev@backendrelay.invalid';

update instant_users
   set email = 'hello+ephemeralapps@instantdb.com'
 where id = 'ee25bd4e-b968-4c1c-bde4-df046ef0dade'::uuid
   and email = 'ephemeral-apps@backendrelay.invalid';

update instant_users
   set email = 'hello+getadbapps@instantdb.com'
 where id = '9bacaf3d-0c7d-46e6-bb1b-4b9fdb9c5b61'::uuid
   and email = 'get-a-db@backendrelay.invalid';
