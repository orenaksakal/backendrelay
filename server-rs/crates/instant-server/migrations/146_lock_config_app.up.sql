-- The deployment-wide configuration app is a public constant. Claim or
-- reclaim it before it can be created by a dashboard user, then strip every
-- credential and membership that could reach it.
insert into apps (id, creator_id, org_id, title)
values (
  '24a4d71b-7bb2-4630-9aee-01146af26239'::uuid,
  'e1111111-1111-1111-1111-111111111ca7'::uuid,
  null,
  'Deployment config'
)
on conflict (id) do update
  set creator_id = excluded.creator_id,
      org_id = excluded.org_id,
      title = excluded.title,
      status = 'active',
      deletion_marked_at = null,
      connection_string = null;

delete from app_admin_tokens
 where app_id = '24a4d71b-7bb2-4630-9aee-01146af26239'::uuid;

delete from app_members
 where app_id = '24a4d71b-7bb2-4630-9aee-01146af26239'::uuid;

delete from app_member_invites
 where app_id = '24a4d71b-7bb2-4630-9aee-01146af26239'::uuid;

insert into rules (app_id, code)
values (
  '24a4d71b-7bb2-4630-9aee-01146af26239'::uuid,
  '{"$default": {"allow": {"$default": "false"}}}'::jsonb
)
on conflict (app_id) do update
  set code = excluded.code;
