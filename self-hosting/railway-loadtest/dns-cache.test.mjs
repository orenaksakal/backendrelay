import test from 'node:test';
import assert from 'node:assert/strict';
import {createOriginLookup} from './dns-cache.mjs';

test('DNS stage counters retain durations only and do not extend TTL or hide waits',async()=>{
 let now=0,release;
 const lookup=createOriginLookup({clock:()=>now,lookup:(host,options,callback)=>{release=()=>{now+=1501;callback(null,[{address:'192.0.2.55',family:4}]);};},ttlMs:async()=>{now+=1201;return 10000;}});
 const pending=new Promise((resolve,reject)=>lookup('api.example.com',{all:true},(error,rows)=>error?reject(error):resolve(rows)));
 release();assert.deepEqual(await pending,[{address:'192.0.2.55',family:4}]);
 const stats=lookup.snapshot();assert.equal(stats.nativeLookupMaximumMs,1501);assert.equal(stats.ttlLookupMaximumMs,1201);assert.equal(stats.slowNativeLookups,1);assert.equal(stats.slowTtlLookups,1);
 assert(!JSON.stringify(stats).includes('192.0.2.55'));assert(!JSON.stringify(stats).includes('api.example.com'));
 now=10000;const expired=new Promise(resolve=>lookup('api.example.com',{all:true},()=>resolve()));assert.equal(lookup.snapshot().misses,2);release();await expired;
});
const call=(lookup,options={all:true})=>new Promise((resolve,reject)=>lookup('api.example.com',options,(error,...value)=>error?reject(error):resolve(value)));

test('origin DNS coalesces concurrent native lookups and expires at the DNS TTL',async()=>{
 let calls=0,now=0,release;
 const lookup=createOriginLookup({lookup:(host,options,callback)=>{calls++;release=()=>callback(null,[{address:'192.0.2.1',family:4}]);},ttlMs:async()=>2000,clock:()=>now});
 const a=call(lookup),b=call(lookup);assert.equal(calls,1);release();assert.deepEqual(await a,await b);
 const first=await call(lookup);first[0][0].address='mutated';assert.equal((await call(lookup))[0][0].address,'192.0.2.1');assert.equal(calls,1);
 now=2000;const expired=call(lookup);assert.equal(calls,2);release();await expired;
 assert.equal(lookup.snapshot().coalesced,1);assert.equal(lookup.snapshot().pending,0);
});
test('DNS preserves native family/hints/order options and callback signatures',async()=>{
 const options=[];
 const lookup=createOriginLookup({lookup:(host,value,callback)=>{options.push(value);callback(null,[{address:value.family===6?'2001:db8::1':'192.0.2.1',family:value.family||4}]);},ttlMs:async()=>60000});
 assert.deepEqual(await call(lookup,{family:6,all:false,hints:32,verbatim:true}),['2001:db8::1',6]);
 assert.deepEqual(await call(lookup,{family:4,all:false,order:'ipv4first'}),['192.0.2.1',4]);
 assert.equal(options[0].family,6);assert.equal(options[0].hints,32);assert.equal(options[0].verbatim,true);assert.equal(options[0].all,true);assert.equal(options[1].order,'ipv4first');
});
test('expired DNS never falls back to stale addresses and other hosts are untouched',async()=>{
 let calls=0,now=0,fail=false;
 const native=(host,options,callback)=>{calls++;if(fail)return callback(Object.assign(new Error('lookup failed'),{code:'ENOTFOUND'}));callback(null,[{address:'192.0.2.1',family:4}]);};
 const lookup=createOriginLookup({lookup:native,ttlMs:async()=>100,clock:()=>now});
 await call(lookup);now=101;fail=true;await assert.rejects(call(lookup),{code:'ENOTFOUND'});assert.equal(calls,2);
 fail=false;await new Promise(resolve=>lookup('other.example',{all:true},()=>resolve()));assert.equal(calls,3);
 const noTtl=createOriginLookup({lookup:native,ttlMs:async()=>0});await call(noTtl);await call(noTtl);assert.equal(calls,5);
});
