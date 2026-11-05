import dns from 'node:dns';
import {isIP} from 'node:net';
import {networkInterfaces} from 'node:os';

// Load-generator only: an explicitly configured resolver object, never system
// DNS configuration, origin IP pinning, a TLS bypass or a stale-answer fallback.
export function createDedicatedOriginLookup({resolver,nativeLookup,interfaces=networkInterfaces,hostname='api.example.com',defaultOrder=()=>dns.getDefaultResultOrder()}){
 const lifetimes=new WeakMap(),counts={resolverQueries:0,resolverErrors:0,nativeFallbacks:0};
 const fallback=(host,options,callback)=>{counts.nativeFallbacks++;return nativeLookup(host,options,callback);};
 function lookup(host,options,callback){
  if(typeof options==='function'){callback=options;options={};}
  else if(typeof options==='number')options={family:options};
  else options=options||{};
  if(host!==hostname||typeof callback!=='function')return fallback(host,options,callback);
  const family=options.family||0,hints=options.hints||0;
  const order=options.order||(options.verbatim===false?'ipv4first':options.verbatim===true?'verbatim':defaultOrder());
  // Unused native flags retain their native implementation, rather than silently
  // changing V4MAPPED/ALL or invalid-option behavior. Report every such fallback.
  if(![0,4,6].includes(family)||(hints&~dns.ADDRCONFIG)||!['verbatim','ipv4first','ipv6first'].includes(order))return fallback(host,options,callback);
  const configured=new Set(Object.values(interfaces()).flat().filter(row=>row&&!row.internal).map(row=>row.family==='IPv4'||row.family===4?4:row.family==='IPv6'||row.family===6?6:0));
  const families=(family?[family]:[4,6]).filter(value=>!(hints&dns.ADDRCONFIG)||configured.has(value));
  void Promise.all(families.map(async value=>{
   counts.resolverQueries++;
   let records;
   try{records=await resolver[value===4?'resolve4':'resolve6'](host,{ttl:true});}
   catch(error){if(['ENODATA','ENOTFOUND'].includes(error.code))return [];throw error;}
   return records.map(record=>{
    if(isIP(record.address)!==value)throw Object.assign(new Error('Invalid origin DNS address'),{code:'EBADRESP'});
    return {address:record.address,family:value,ttlMs:Number.isFinite(record.ttl)&&record.ttl>0?record.ttl*1000:0};
   });
  })).then(results=>{
   let records=results.flat();
   if(order==='ipv6first')records=records.filter(row=>row.family===6).concat(records.filter(row=>row.family===4));
   if(order==='ipv4first')records=records.filter(row=>row.family===4).concat(records.filter(row=>row.family===6));
   if(!records.length)throw Object.assign(new Error('No origin DNS address'),{code:'ENOTFOUND',syscall:'getaddrinfo'});
   const addresses=records.map(({address,family})=>({address,family}));
   lifetimes.set(addresses,Math.min(...records.map(row=>row.ttlMs)));
   return addresses;
  }).then(addresses=>{
   if(options.all)callback(null,addresses);else callback(null,addresses[0].address,addresses[0].family);
  },error=>{counts.resolverErrors++;callback(error);});
 }
 return {lookup,ttlMs:async(host,addresses)=>lifetimes.get(addresses)||0,snapshot:()=>({...counts,backend:'dedicated-cares',systemDnsUnchanged:true})};
}
