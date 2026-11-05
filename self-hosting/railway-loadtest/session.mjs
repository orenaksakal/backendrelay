import {init,i} from '@instantdb/core';
import WebSocket from 'ws';
import {randomUUID} from 'node:crypto';
import {rowId,profile} from './model.mjs';
import {WireTimings} from './wire-timing.mjs';
import {WireConnections} from './wire-connection.mjs';
import {WireHandshakes} from './wire-handshake.mjs';
import {installOriginLookup,originLookup} from './dns-cache.mjs';
installOriginLookup();
const timings=new WireTimings();
const connections=new WireConnections();
const handshakes=new WireHandshakes();
let wireErrors=0,sdkLogs=0,wireAddQueries=0,wireTransactions=0;const wireSummaries=[];
// The SDK can log credential-bearing frames. Retain counts, never raw messages.
console.error=()=>{sdkLogs++;};console.warn=()=>{sdkLogs++;};
function parseApiOrigin(value){
 const raw=value===undefined||value===''?'http://localhost:8888':value;
 let url;
 try{url=new URL(raw);}catch{throw new Error('LOADTEST_API_ORIGIN must be an http(s) origin with no path/query');}
 if(!['http:','https:'].includes(url.protocol))throw new Error('LOADTEST_API_ORIGIN must use http or https');
 if(url.pathname!=='/'||url.search||url.hash)throw new Error('LOADTEST_API_ORIGIN must be an origin with no path/query');
 if(url.username||url.password)throw new Error('LOADTEST_API_ORIGIN must not include credentials');
 return url;
}
const apiOrigin=parseApiOrigin(process.env.LOADTEST_API_ORIGIN);
const apiURI=apiOrigin.origin;
const websocketURI=(apiOrigin.protocol==='https:'?'wss://':'ws://')+apiOrigin.host+'/runtime/session';
export class ObservedSocket extends WebSocket{
  constructor(...args){super(...args);this.activity=connections.attach(this);handshakes.attach(this);this.requests=new Map();this.on('message',data=>{try{const m=JSON.parse(String(data)),request=this.requests.get(m['client-event-id']);if((m.op==='add-query-ok'||m.op==='transact-ok')&&request&&!request.completed){request.ackMs=performance.now()-request.at;request.completed=true;timings.record(request.operation,request.sentAtMs,request.ackMs);}if(m.op==='error'||m.op==='join-room-error'){wireErrors++;if(wireSummaries.length<10){const category=value=>/^[a-z-]{1,64}$/.test(value||'')?value:'unknown';wireSummaries.push({type:category(m.type||m['error-type']),operation:category(m['original-event']?.op),message:m.message==='Operation timed out: handle-receive'?m.message:'[redacted]',condition:category(m.hint?.condition),timeoutMs:Number.isFinite(m.hint?.['timeout-ms'])?m.hint['timeout-ms']:null,elapsedMs:request?performance.now()-request.at:null,ackMs:request?.ackMs??null});}}}catch{wireErrors++;}});}
  send(data,...args){this.activity.sending();try{const m=JSON.parse(String(data));if(m.op==='add-query')wireAddQueries++;if(m.op==='transact')wireTransactions++;if(m.op==='add-query'||m.op==='transact'){this.requests.set(m['client-event-id'],{at:performance.now(),sentAtMs:Date.now(),operation:m.op==='add-query'?'read':'write'});if(this.requests.size>128)this.requests.delete(this.requests.keys().next().value);}}catch{}return super.send(data,...args);}
  // Pinned ws also calls close() internally to reply to a received close frame.
  // That reply is not a locally initiated SDK shutdown. Loopback integration
  // covers the pinned flag, remote close/reset and explicit pre-open cancellation.
  close(...args){if(!this._closeFrameReceived)this.activity.closing();return super.close(...args);}
}
globalThis.WebSocket=ObservedSocket;globalThis.window={};globalThis.BroadcastChannel=undefined;
class MemoryStore{items=new Map();async getItem(k){return this.items.get(k);}async removeItem(k){this.items.delete(k);}async multiSet(p){for(const[k,v]of p)this.items.set(k,v);}async getAllKeys(){return [...this.items.keys()];}}
class Network{static async getIsOnline(){return true;}static listen(){return ()=>{};}}
export function makeClient(appId,key){return init({appId,apiURI,websocketURI,devtool:false,__extraDedupeKey:key},MemoryStore,Network);}
const schema=i.schema({entities:{todos:i.entity({ownerId:i.string().indexed(),title:i.string(),completed:i.boolean(),revision:i.string(),updatedAt:i.number()})}});
export const counts=()=>({wireErrors,sdkLogs,wireAddQueries,wireTransactions});
export const wireDiagnostics=()=>[...wireSummaries];
export const wireTimings=()=>timings.snapshot();
export const wireConnections=()=>({...connections.snapshot(),handshakes:handshakes.snapshot(),dnsLookup:originLookup.snapshot()});
export const sleep=ms=>new Promise(resolve=>setTimeout(resolve,ms));
export async function deadline(work,ms=profile.operationTimeoutMs){let timer;try{return await Promise.race([work,new Promise((_,reject)=>{timer=setTimeout(()=>reject(new Error('operation-timeout')),ms);})]);}finally{clearTimeout(timer);}}
export async function connect(appId,identity,key,onDisconnect=()=>{}){
 const db=init({appId,apiURI,websocketURI,schema,devtool:false,__extraDedupeKey:key},MemoryStore,Network);
 const state={db,userId:identity.userId,rows:[],ready:false,closed:false,subscriptionErrors:0,pending:new Map(),disconnects:0};
 try{
  await deadline(db.auth.signInWithToken(identity.token));
  const q={todos:{$:{where:{ownerId:identity.userId},limit:20}}};
  await deadline(new Promise((resolve,reject)=>{state.unsub=db.subscribeQuery(q,r=>{
   if(r.error){state.subscriptionErrors++;reject(new Error('subscription-error'));return;}
   if(r.data){state.rows=r.data.todos;if(!state.ready){state.ready=true;resolve();}for(const row of state.rows){const pending=state.pending.get(row.revision);if(pending){pending.resolve(performance.now()-pending.at);state.pending.delete(row.revision);}}}
  });}));
  if(state.rows.length!==profile.rowsPerUser||state.rows.some(row=>row.ownerId!==identity.userId))throw new Error('seed-or-isolation-mismatch');
  state.connection=db.subscribeConnectionStatus(status=>{if(state.ready&&!state.closed&&status!=='authenticated'){state.disconnects++;onDisconnect();}});
  state.read=async()=>{const nonce=randomUUID();const result=await deadline(db.queryOnce({todos:{$:{where:{ownerId:identity.userId,revision:{$ne:nonce}},limit:20}}}));if(result.data.todos.length!==profile.rowsPerUser||result.data.todos.some(r=>r.ownerId!==identity.userId))throw new Error('read-isolation-mismatch');};
  state.write=async()=>{
   const revision=randomUUID();let resolve;const observed=new Promise(r=>{resolve=r;});const begun=performance.now();const remote=state.observer;
   // Never count the writer SDK's optimistic cache as WAL propagation.
   remote?.pending.set(revision,{at:begun,resolve});
   try{const ack=await deadline(db.transact(db.tx.todos[rowId(identity.userId,0)].update({revision,completed:Math.random()<.5,updatedAt:Date.now()})));if(ack.status!=='synced')throw new Error('write-not-acknowledged');const ackMs=performance.now()-begun;const realtimeMs=remote?await deadline(observed):null;return {ackMs,realtimeMs};}finally{remote?.pending.delete(revision);}
  };
  state.close=()=>{state.closed=true;state.connection?.();state.unsub?.();state.observer?.close();db.shutdown();};
  return state;
 }catch{state.closed=true;state.unsub?.();db.shutdown();throw new Error('session-setup-failed');}
}
