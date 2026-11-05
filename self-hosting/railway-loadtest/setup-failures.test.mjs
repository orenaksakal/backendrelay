import test from 'node:test';
import assert from 'node:assert/strict';
import {setupFailureKind} from './setup-failures.mjs';

test('setup failures distinguish authentication and subscription without raw data',()=>{
 for(const phase of ['auth','subscribe']){
  for(const reason of ['operation-timeout','field-projection-failed','isolation-error','private-isolation-error','shared-room-isolation-error','shared-author-isolation-error','failed'])assert.equal(setupFailureKind(new Error(`DAU-session-${phase}-${reason}`)),`${phase}:${reason}`);
  assert.equal(setupFailureKind(new Error(`DAU-session-${phase}-fixture-mismatch-private-10-shared-0`)),`${phase}:fixture-mismatch`);
  assert.equal(setupFailureKind(new Error(`DAU-session-${phase}-subscription-error-permission-denied`)),`${phase}:subscription-error`);
 }
});

test('arbitrary or malformed errors become one bounded bucket',()=>{
 for(const error of [null,undefined,{},'secret-token',new Error('secret-token'),new Error('DAU-session-auth-secret-token'),new Error('DAU-session-subscribe-subscription-error-'+ 'a'.repeat(65)),new Error('DAU-session-auth-operation-timeout\nsecret-token'),new Error('DAU-session-subscribe-fixture-mismatch-private-1234567-shared-0')])assert.equal(setupFailureKind(error),'unclassified');
});
