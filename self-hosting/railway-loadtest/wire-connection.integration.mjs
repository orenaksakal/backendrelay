// Run from a copied tools context with the same pinned ws/core dependencies.
// Loopback only: no public/production API, credentials, payloads or load traffic.
import assert from 'node:assert/strict';
import http from 'node:http';
import {once} from 'node:events';
import {WebSocketServer} from 'ws';
import {ObservedSocket,wireConnections} from './session.mjs';

const server=http.createServer(),websockets=new WebSocketServer({noServer:true});
server.on('upgrade',(request,socket,head)=>{
 if(request.url==='/reject'){socket.end('HTTP/1.1 403 Forbidden\r\nContent-Length: 0\r\n\r\n');return;}
 websockets.handleUpgrade(request,socket,head,ws=>{
  if(request.url==='/remote-close')ws.close(1011,'not retained');
  else if(request.url==='/reset')ws.terminate();
 });
});
await new Promise(resolve=>server.listen(0,'127.0.0.1',resolve));
const endpoint='ws://127.0.0.1:'+server.address().port;
const done=socket=>new Promise(resolve=>socket.once('close',resolve));
try{
 const canceled=new ObservedSocket(endpoint+'/local');const ended=done(canceled);canceled.close();await ended;
 let d=wireConnections();assert.equal(d.counts.localHandshakeCancellations,1);assert.equal(d.counts.errors,0);assert.equal(d.counts.unexpectedCloses,0);
 const rejected=new ObservedSocket(endpoint+'/reject');await done(rejected);
 d=wireConnections();assert.equal(d.counts.errors,1);assert.equal(d.counts.unexpectedCloses,1);assert.equal(d.counts.localHandshakeCancellations,1);
 const remote=new ObservedSocket(endpoint+'/remote-close');await done(remote);
 d=wireConnections();assert.equal(d.counts.unexpectedCloses,2);assert(d.events.some(e=>e.kind==='close'&&e.code===1011));
 const reset=new ObservedSocket(endpoint+'/reset');await done(reset);
 d=wireConnections();assert.equal(d.counts.unexpectedCloses,3);assert(d.events.some(e=>e.kind==='close'&&e.code===1006));
 const local=new ObservedSocket(endpoint+'/local');await once(local,'open');const closed=done(local);local.close();await closed;
 d=wireConnections();assert.equal(d.counts.unexpectedCloses,3);assert.equal(d.counts.errors,1);assert.equal(d.counts.localCloses,2);assert(!JSON.stringify(d).includes('not retained'));
 console.log(JSON.stringify({loopbackCasesPassed:5,exactObservedSocketWrapperTested:true,noProductionTraffic:true,localSdkHandshakeCancellationCountedApart:true,remoteHandshakeRejectionCloseAndResetStillFailures:true}));
}finally{
 for(const socket of websockets.clients)socket.terminate();
 await new Promise(resolve=>websockets.close(resolve));
 await new Promise(resolve=>server.close(resolve));
}
