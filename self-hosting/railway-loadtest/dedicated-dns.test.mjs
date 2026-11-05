import test from 'node:test';
import assert from 'node:assert/strict';
import dns from 'node:dns';
import {createDedicatedOriginLookup} from './dedicated-dns.mjs';
import {createOriginLookup} from './dns-cache.mjs';

const call=(lookup,options={all:true},host='api.example.com')=>new Promise((resolve,reject)=>lookup(host,options,(error,...result)=>error?reject(error):resolve(result)));
const interfaces=()=>({test:[{family:'IPv4',internal:false},{family:'IPv6',internal:false}]});
const records={resolve4:async()=>[{address:'192.0.2.1',ttl:2}],resolve6:async()=>[{address:'2001:db8::1',ttl:3}]};
const unexpected=()=>{throw new Error('Native lookup unexpectedly called');};

test('dedicated DNS respects family, callback shape, order and returned TTLs',async()=>{
 const source=createDedicatedOriginLookup({resolver:records,nativeLookup:unexpected,interfaces,defaultOrder:()=> 'ipv4first'});
 assert.deepEqual(await call(source.lookup,{all:false,family:6}),['2001:db8::1',6]);
 const [rows]=await call(source.lookup,{all:true,order:'ipv6first'});
 assert.deepEqual(rows,[{address:'2001:db8::1',family:6},{address:'192.0.2.1',family:4}]);
 assert.equal(await source.ttlMs('api.example.com',rows),2000);
 assert(!JSON.stringify(source.snapshot()).includes('192.0.2.1'));
});

test('ADDRCONFIG avoids unavailable families; unsupported flags and other names use native lookup',async()=>{
 let v6=0,native=0;
 const source=createDedicatedOriginLookup({resolver:{...records,resolve6:async()=>{v6++;return records.resolve6();}},nativeLookup:(host,options,callback)=>{native++;callback(null,[{address:'192.0.2.9',family:4}]);},interfaces:()=>({test:[{family:'IPv4',internal:false}]})});
 assert.deepEqual(await call(source.lookup,{all:true,hints:dns.ADDRCONFIG}),[[{address:'192.0.2.1',family:4}]]);assert.equal(v6,0);
 await call(source.lookup,{all:true,hints:dns.V4MAPPED});await call(source.lookup,{all:true},'other.example');assert.equal(native,2);assert.equal(source.snapshot().nativeFallbacks,2);
});

test('dedicated resolver errors are surfaced without falling back to a stale lease',async()=>{
 let now=0,fail=false,calls=0;
 const source=createDedicatedOriginLookup({resolver:{resolve4:async()=>{calls++;if(fail)throw Object.assign(new Error('DNS timeout'),{code:'ETIMEOUT'});return records.resolve4();},resolve6:async()=>[]},nativeLookup:unexpected,interfaces});
 const lookup=createOriginLookup({...source,clock:()=>now});
 await call(lookup);await call(lookup);assert.equal(calls,1);
 now=2000;fail=true;await assert.rejects(call(lookup),{code:'ETIMEOUT'});assert.equal(calls,2);
 fail=false;await call(lookup);assert.equal(calls,3);
});

test('empty, malformed and missing-TTL answers cannot create synthetic cache leases',async()=>{
 const bad=createDedicatedOriginLookup({resolver:{resolve4:async()=>[{address:'not-an-ip',ttl:99}],resolve6:async()=>[]},nativeLookup:unexpected,interfaces});
 await assert.rejects(call(bad.lookup),{code:'EBADRESP'});
 const empty=createDedicatedOriginLookup({resolver:{resolve4:async()=>[],resolve6:async()=>[]},nativeLookup:unexpected,interfaces});await assert.rejects(call(empty.lookup),{code:'ENOTFOUND'});
 const noTtl=createDedicatedOriginLookup({resolver:{resolve4:async()=>[{address:'192.0.2.1'}],resolve6:async()=>[]},nativeLookup:unexpected,interfaces});const [rows]=await call(noTtl.lookup);assert.equal(await noTtl.ttlMs('api.example.com',rows),0);
});
