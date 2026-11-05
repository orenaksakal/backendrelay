// Fixed diagnostic buckets only: never retain tokens, identities or raw errors.
export function setupFailureKind(error){
 const message=typeof error?.message==='string'?error.message:'';
 const match=/^DAU-session-(auth|subscribe)-(operation-timeout|field-projection-failed|isolation-error|private-isolation-error|shared-room-isolation-error|shared-author-isolation-error|failed|fixture-mismatch-private-\d{1,6}-shared-\d{1,6}|subscription-error-[a-z-]{1,64})$/.exec(message);
 if(!match)return 'unclassified';
 const reason=match[2].startsWith('fixture-mismatch-')?'fixture-mismatch':match[2].startsWith('subscription-error-')?'subscription-error':match[2];
 return `${match[1]}:${reason}`;
}
