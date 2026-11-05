drop index instant_user_refresh_tokens_token_hash_key;

alter table instant_user_refresh_tokens drop column last_used_at;

alter table instant_user_refresh_tokens drop column token_hash;
