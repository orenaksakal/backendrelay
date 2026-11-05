import test from 'node:test';
import assert from 'node:assert/strict';
import {fixtureIdentityCapacity,fixtureRoomMembers} from './fixture-rooms.mjs';
test('partial active groups retain all canonical room members without adding active clients',()=>{
 for(const active of [50,150,250,750]){
  const capacity=fixtureIdentityCapacity(active),identities=Array.from({length:capacity},(_,n)=>({userId:`user-${n}`}));
  assert.equal(capacity%20,0);assert.equal(capacity-active,10);
  const members=fixtureRoomMembers(identities,active-1);
  assert.equal(members.length,20);assert(members.includes(`user-${active+9}`));assert(!members.includes(`user-${active+10}`));
  assert.equal(identities.slice(0,active).length,active);
 }
});
test('shrinking a tier cannot turn legitimate historical room authors into cross-room rows',()=>{
 const large=Array.from({length:fixtureIdentityCapacity(112)},(_,n)=>({userId:`user-${n}`}));
 const small=large.slice(0,fixtureIdentityCapacity(50));
 assert.deepEqual(fixtureRoomMembers(small,40),fixtureRoomMembers(large,40));
 assert(!fixtureRoomMembers(small,40).includes('user-60'));
 assert.throws(()=>fixtureRoomMembers(small.slice(0,50),40));
 for(const value of [0,-1,1.5,NaN,Infinity])assert.throws(()=>fixtureIdentityCapacity(value));
});
