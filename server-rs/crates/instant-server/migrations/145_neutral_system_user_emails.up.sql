-- The server resolves the system creator users by email, but migrations 62 and
-- 105 seeded them with upstream addresses. Rename them to the neutral
-- self-hosted addresses, only where the row still carries the seeded value so
-- an operator's manual change is never overwritten.
update instant_users
   set email = 'ephemeral-apps-dev@backendrelay.invalid'
 where id = 'dda67f26-6962-479e-b476-60e4b6963b74'::uuid
   and email = 'hello+ephemeralappsdev@instantdb.com';

update instant_users
   set email = 'ephemeral-apps@backendrelay.invalid'
 where id = 'ee25bd4e-b968-4c1c-bde4-df046ef0dade'::uuid
   and email = 'hello+ephemeralapps@instantdb.com';

update instant_users
   set email = 'get-a-db@backendrelay.invalid'
 where id = '9bacaf3d-0c7d-46e6-bb1b-4b9fdb9c5b61'::uuid
   and email = 'hello+getadbapps@instantdb.com';
