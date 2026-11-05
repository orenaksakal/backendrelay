import dns from 'node:dns';
import {createDedicatedOriginLookup} from './dedicated-dns.mjs';

// Node workers share libuv's small getaddrinfo pool. An SDK auth transition
// creates/replaces sockets; resolving the same public origin for every socket
// can queue both handshakes and health checks before they reach the service.
// Preserve native family/hints/order selection and public hostname/TLS checks.
export function createOriginLookup({lookup,ttlMs,clock=()=>Date.now(),hostname='api.example.com',maximumAgeMs=30000}){
 const entries=new Map(),pending=new Map(),counts={hits:0,misses:0,coalesced:0,errors:0,nativeLookupMaximumMs:0,ttlLookupMaximumMs:0,slowNativeLookups:0,slowTtlLookups:0};
 function startLookup(host,nativeOptions,key){
  if(pending.has(key))return pending.get(key);
  counts.misses++;
  const started=clock();
  const work=new Promise((resolve,reject)=>lookup(host,{...nativeOptions,all:true},(error,rows)=>error?reject(error):resolve(rows)))
   .then(async addresses=>{
    const nativeElapsed=Math.max(0,clock()-started);counts.nativeLookupMaximumMs=Math.max(counts.nativeLookupMaximumMs,nativeElapsed);if(nativeElapsed>1000)counts.slowNativeLookups++;
    if(!addresses.length)throw new Error('No public origin address');
    const ttlStarted=clock();
    const ttl=await ttlMs(host,addresses).catch(()=>0);
    const ttlElapsed=Math.max(0,clock()-ttlStarted);counts.ttlLookupMaximumMs=Math.max(counts.ttlLookupMaximumMs,ttlElapsed);if(ttlElapsed>1000)counts.slowTtlLookups++;
    const age=Math.min(maximumAgeMs,Math.max(0,ttl)),expires=started+age;
    if(expires>clock()){
     if(entries.size>=16)entries.delete(entries.keys().next().value);
     entries.set(key,{addresses:addresses.map(row=>({...row})),expires});
    }
    return addresses;
   }).finally(()=>pending.delete(key));
  pending.set(key,work);
  return work;
 }
 function cachedLookup(host,options,callback){
  if(typeof options==='function'){callback=options;options={};}
  else if(typeof options==='number')options={family:options};
  else options=options||{};
  if(host!==hostname||typeof callback!=='function')return lookup(host,options,callback);
  const {all=false,...nativeOptions}=options;
  const key=JSON.stringify({family:nativeOptions.family||0,hints:nativeOptions.hints||0,order:nativeOptions.order||null,verbatim:nativeOptions.verbatim??null});
  const hit=entries.get(key);
  const deliver=addresses=>{const copy=addresses.map(row=>({...row}));queueMicrotask(()=>all?callback(null,copy):callback(null,copy[0].address,copy[0].family));};
  if(hit&&clock()<hit.expires){counts.hits++;deliver(hit.addresses);return;}
  let work=pending.get(key);
  if(work)counts.coalesced++;
  else work=startLookup(host,nativeOptions,key);
  work.then(deliver,error=>{counts.errors++;queueMicrotask(()=>callback(error));});
 }
 cachedLookup.snapshot=()=>({...counts,entries:entries.size,pending:pending.size,maximumAgeMs,expiry:'minimum authoritative address TTL, capped at 30s; no stale-on-error'});
 return cachedLookup;
}

const ttlResolver=new dns.promises.Resolver({timeout:1000,tries:1});
async function addressTtlMs(host,addresses){
 const requests=[];
 if(addresses.some(row=>row.family===4))requests.push(ttlResolver.resolve4(host,{ttl:true}));
 if(addresses.some(row=>row.family===6))requests.push(ttlResolver.resolve6(host,{ttl:true}));
 const records=await Promise.allSettled(requests);
 const ttl=new Map(records.flatMap(result=>result.status==='fulfilled'?result.value:[]).map(row=>[row.address,row.ttl*1000]));
 // Unknown/zero TTL means single-flight only, never a synthetic positive lease.
 return Math.min(...addresses.map(row=>ttl.get(row.address)||0));
}
const nativeLookup=dns.lookup.bind(dns);
const servers=process.env.LOADTEST_DNS_RESOLVERS?.split(',').map(value=>value.trim()).filter(Boolean);
let source={lookup:nativeLookup,ttlMs:addressTtlMs,snapshot:()=>({backend:'native-getaddrinfo'})};
if(servers?.length){
 const resolver=new dns.promises.Resolver({timeout:1000,tries:1});resolver.setServers(servers);
 source=createDedicatedOriginLookup({resolver,nativeLookup});
}
export const originLookup=createOriginLookup(source);
const cachedSnapshot=originLookup.snapshot;
originLookup.snapshot=()=>{const stats=cachedSnapshot();return {...stats,lookupMaximumMs:stats.nativeLookupMaximumMs,...source.snapshot(),resolverServers:servers||null};};
export function installOriginLookup(){dns.lookup=originLookup;}
