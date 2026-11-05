import test from 'node:test';
import assert from 'node:assert/strict';
import {EventEmitter} from 'node:events';
import {WireConnections} from './wire-connection.mjs';
import {apps,latencyAcceptance,phaseFailures,scenario} from './dau-model.mjs';
test('unexpected socket closure retains only code and bounded activity clocks',()=>{
 const d=new WireConnections(),socket=new EventEmitter();let now=0;const state=d.attach(socket,()=>now,()=>1792433330000+now);
 socket.emit('open');now=100;state.sending();now=200;socket.emit('message','credential payload');now=250;socket.emit('ping','private ping');now=1200;socket.emit('close',1006,'private close reason');
 assert.deepEqual(d.snapshot(),{counts:{opened:1,closed:1,localCloses:0,unexpectedCloses:1,errors:0,localHandshakeCancellations:0},events:[{kind:'close',at:'2026-10-19T18:08:51.200Z',ageMs:1200,sinceReceiveMs:950,sinceSendMs:1100,sincePingMs:950,pings:1,code:1006}]});
 assert(!JSON.stringify(d.snapshot()).includes('private'));const copy=d.snapshot();copy.events[0].code='sensitive';assert.equal(d.snapshot().events[0].code,1006);
});
test('intentional SDK closes stay distinct but errors are never concealed',()=>{
 const d=new WireConnections(),socket=new EventEmitter(),state=d.attach(socket);socket.emit('open');state.closing();socket.emit('error',{code:'ECONNRESET',message:'private'});socket.emit('close',1000,'private');
 const result=d.snapshot();assert.deepEqual(result.counts,{opened:1,closed:1,localCloses:1,unexpectedCloses:0,errors:1,localHandshakeCancellations:0});assert.equal(result.events[0].kind,'error');assert.equal(result.events[0].code,'ECONNRESET');assert(!JSON.stringify(result).includes('private'));
});
test('transport diagnostics cap memory and cannot accept arbitrary error categories',()=>{
 const d=new WireConnections();for(let i=0;i<100;i++){const socket=new EventEmitter();d.attach(socket);socket.emit('error',{code:'private-token'});socket.emit('close',1006);}
 const result=d.snapshot();assert.equal(result.events.length,32);assert.equal(result.counts.unexpectedCloses,100);assert.equal(result.counts.errors,100);assert(!JSON.stringify(result).includes('private-token'));
});
test('transport failure still fails the phase even if the SDK emits no status callback',()=>{
 const phase=scenario(1000).phases[3];const statuses=apps.map((app,i)=>({app:app.key,concurrency:{samples:600,peak:phase.targets[i],mean:phase.targets[i]},canceledLatencySamplesMs:{read:[],write:[]},metrics:Object.fromEntries(['read','write','realtime'].map(kind=>[kind,{count:10000,offered:10000,canceled:0,failed:0,maxMs:100}]))}));
 assert.deepEqual(phaseFailures(phase,statuses,latencyAcceptance),[]);statuses[0].diagnosticWireConnections={counts:{localCloses:10,unexpectedCloses:0,errors:0}};assert.deepEqual(phaseFailures(phase,statuses,latencyAcceptance),[]);
 statuses[0].diagnosticWireConnections.counts.unexpectedCloses=1;assert(phaseFailures(phase,statuses,latencyAcceptance).includes('tasks: transport failure'));
});
test('an exact locally requested pre-open SDK cancellation is counted apart',()=>{
 const d=new WireConnections(),socket=new EventEmitter(),state=d.attach(socket);state.closing();socket.emit('error',new Error('WebSocket was closed before the connection was established'));socket.emit('close',1006);
 assert.equal(d.snapshot().counts.localHandshakeCancellations,1);assert.equal(d.snapshot().counts.errors,0);assert.equal(d.snapshot().counts.unexpectedCloses,0);assert.equal(d.snapshot().counts.localCloses,1);
});
test('handshake cancellation exemption cannot hide an established loss, nonlocal close or different error',()=>{
 for(const variant of ['opened','not-local','sent','received','other-error']){
  const d=new WireConnections(),socket=new EventEmitter(),state=d.attach(socket);if(variant!=='not-local')state.closing();
  if(variant==='opened')socket.emit('open');if(variant==='sent')state.sending();if(variant==='received')socket.emit('message','private');
  socket.emit('error',new Error(variant==='other-error'?'remote failure':'WebSocket was closed before the connection was established'));
  assert.equal(d.snapshot().counts.errors,1,variant);assert.equal(d.snapshot().counts.localHandshakeCancellations,0,variant);
 }
});
test('HTTP upgrade diagnostics retain only an exact numeric rejection status',()=>{
 const d=new WireConnections();
 for(const message of ['Unexpected server response: 429','Unexpected server response: 503','Unexpected server response: 429 private-token','private-token']){const socket=new EventEmitter();socket._req={res:{headers:{server:'cloudflare','cf-mitigated':'challenge',private:'private-token'}}};d.attach(socket);socket.emit('error',new Error(message));}
 assert.deepEqual(d.snapshot().events.map(event=>event.httpStatus),[429,503,undefined,undefined]);
 assert(!JSON.stringify(d.snapshot()).includes('private-token'));
 assert.equal(d.snapshot().counts.errors,4);
 assert.equal(d.snapshot().events[0].cloudflareChallenge,true);
});
