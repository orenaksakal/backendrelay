import test from 'node:test';
import assert from 'node:assert/strict';
import {EventEmitter} from 'node:events';
import {WireHandshakes} from './wire-handshake.mjs';
test('handshakes distinguish DNS, TCP, TLS and upgrade queues without raw connection data',()=>{
 const diagnostics=new WireHandshakes(),ws=new EventEmitter(),request=new EventEmitter(),socket=new EventEmitter();ws._req=request;let now=0;
 diagnostics.attach(ws,()=>now,()=>1792433330000+now);
 assert.equal(diagnostics.snapshot().pendingByStage.socket,1);request.emit('socket',socket);assert.equal(diagnostics.snapshot().pendingByStage.dns,1);
 now=3000;socket.emit('lookup',null,'private-address',4,'private-host');assert.equal(diagnostics.snapshot().pendingByStage.connect,1);
 now=3100;socket.emit('connect');assert.equal(diagnostics.snapshot().pendingByStage.tls,1);
 now=3200;socket.emit('secureConnect');assert.equal(diagnostics.snapshot().pendingByStage.upgrade,1);
 now=3300;ws.emit('open');ws.emit('close');
 const result=diagnostics.snapshot();assert.deepEqual(result.completed,{opened:1,closedBeforeOpen:0});assert.equal(result.maximumMs.lookup,3000);assert.equal(result.slow[0].stage,'open');assert(!JSON.stringify(result).includes('private'));
});
test('slow pre-open cancellation records the blocked stage under fixed memory bounds',()=>{
 const diagnostics=new WireHandshakes();let now=0;
 for(let n=0;n<100;n++){const ws=new EventEmitter();ws._req=new EventEmitter();diagnostics.attach(ws,()=>now,()=>1792433330000+now);ws._req.emit('socket',new EventEmitter());now+=10000;ws.emit('close');ws.emit('close');}
 const result=diagnostics.snapshot();assert.equal(result.completed.closedBeforeOpen,100);assert.equal(result.slow.length,32);assert(result.slow.every(s=>s.stage==='dns'));assert.equal(result.pendingByStage.dns,0);
});
