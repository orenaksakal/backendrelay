import {parentPort,workerData} from 'node:worker_threads';
import {monitorEventLoopDelay} from 'node:perf_hooks';
import {connect} from './dau-session.mjs';
import {counts,sleep,wireDiagnostics,wireTimings,wireConnections} from './session.mjs';
import {random} from './dau-model.mjs';
import {summary} from './model.mjs';
import {timingDiagnostics,recordTiming,copyTiming} from './dau-timing.mjs';
import {setupFailureKind} from './setup-failures.mjs';
const {app,identities}=workerData,rng=random(20261018+app.index),active=new Map(),opening=new Set(),histories=new Map(),phaseResults=new Map(),concurrency=new Map();
const setupFailureKinds=new Map();
let stopped=false,paused=false,phase=null,target=0,setupFailures=0,disconnects=0,churn=0,skipped=0,canceled=0,operationFailures=0,subscriptionErrors=0,transition=Promise.resolve();
const delay=monitorEventLoopDelay({resolution:20});delay.enable();
 const metric=()=>({read:[],write:[],realtime:[],failed:{read:0,write:0,realtime:0},offered:{read:0,write:0},canceled:{read:0,write:0},canceledLatencySamplesMs:{read:[],write:[]},timing:timingDiagnostics(),churn:0});
function currentMetric(start){if(!phase||start<phase.holdAt||start>=phase.endAt)return null;let m=phaseResults.get(phase.name);if(!m){m=metric();phaseResults.set(phase.name,m);}return m;}
async function add(index){
 if(stopped||paused||active.has(index)||opening.has(index)||index>=target)return;
 opening.add(index);let client;
 try{
  const identity=identities[index],history=histories.get(index)||[];histories.set(index,history);
  client=await connect(app,identity,`${app.key}-${index}-${Date.now()}`,()=>{disconnects++;},history);
  if(stopped||paused||index>=target){client.close();return;}
  if(index%20===0)client.observer=await connect(app,identity,`${app.key}-observer-${index}-${Date.now()}`,()=>{disconnects++;});
  if(stopped||paused||index>=target){client.close();return;}
  const session={client,index,expires:Date.now()+(300+rng()*600)*1000};active.set(index,session);void work(session);
 }catch(error){client?.close();setupFailures++;const key=setupFailureKind(error);setupFailureKinds.set(key,(setupFailureKinds.get(key)||0)+1);}finally{opening.delete(index);}
}
async function work(session){
 const c=session.client;let rate=phase?.rateMultiplier??1,nextRead=Date.now()+rng()*app.readMs/rate,nextWrite=Date.now()+rng()*app.writeMs/rate;
  while(!stopped&&!c.closed){
   if(paused){await sleep(100);continue;}
   // Rescale pending waits when the phase rate changes so a burst takes effect immediately, not one 1x interval later.
   const current=phase?.rateMultiplier??1;if(current!==rate){const now=Date.now(),scale=rate/current;nextRead=now+Math.max(0,nextRead-now)*scale;nextWrite=now+Math.max(0,nextWrite-now)*scale;rate=current;}
   const next=Math.min(nextRead,nextWrite);await sleep(Math.max(0,Math.min(next-Date.now(),1000)));if(stopped||c.closed)break;if(paused||Date.now()<next)continue;
  const kind=nextRead<=nextWrite?'read':'write';if(kind==='read')nextRead+=app.readMs/rate*(.5+rng());else nextWrite+=app.writeMs/rate*(.5+rng());
   const start=Date.now(),m=currentMetric(start),begin=performance.now();if(m)m.offered[kind]++;if(start-next>5000){skipped++;continue;}
     try{if(kind==='read'){await c.read(rng()<.4,rng()<.2);const elapsedMs=performance.now()-begin;m?.read.push(elapsedMs);if(m)recordTiming(m.timing,{kind,startedAtMs:start,elapsedMs});}else{const shared=rng()<app.sharedWrite,r=await c.write(shared);m?.write.push(r.ackMs);if(r.realtimeMs!==null)m?.realtime.push(r.realtimeMs);if(m)recordTiming(m.timing,{kind,startedAtMs:start,elapsedMs:performance.now()-begin,ackMs:r.ackMs,realtimeMs:r.realtimeMs,shared});}}catch{if(c.closed||stopped){canceled++;if(m){m.canceled[kind]++;m.canceledLatencySamplesMs[kind].push((c.closedAtMs??performance.now())-begin);}}else{operationFailures++;if(m){m.failed[kind]++;if(kind==='write'&&c.observer)m.failed.realtime++;}}}
 }
}
function close(session){subscriptionErrors+=session.client.subscriptionErrors+(session.client.observer?.subscriptionErrors||0);session.client.close();active.delete(session.index);}
function status(){const m=phaseResults.get(phase?.name)||metric(),c=concurrency.get(phase?.name);return {app:app.key,phase:phase?.name,connected:active.size,opening:opening.size,concurrency:c?{samples:c.samples,peak:c.peak,min:c.min,mean:c.sum/c.samples,observerPeak:c.observerPeak,observerMin:c.observerMin,observerMean:c.observerSum/c.samples}:null,observers:[...active.values()].filter(x=>x.client.observer).length,target,setupFailures,setupFailureKinds:Object.fromEntries(setupFailureKinds),disconnects,churn,phaseChurn:m.churn,skipped,canceled,operationFailures,...counts(),wireDiagnostics:wireDiagnostics(),subscriptionErrors:subscriptionErrors+[...active.values()].reduce((n,x)=>n+x.client.subscriptionErrors+(x.client.observer?.subscriptionErrors||0),0),metrics:{read:{...summary(m.read),failed:m.failed.read,offered:m.offered.read,canceled:m.canceled.read},write:{...summary(m.write),failed:m.failed.write,offered:m.offered.write,canceled:m.canceled.write},realtime:{...summary(m.realtime),failed:m.failed.realtime}},eventLoopP99Ms:delay.percentile(99)/1e6,rssBytes:process.memoryUsage().rss};}
parentPort.on('message',message=>{
  if(message.type==='pause'){paused=true;return;}
  if(message.type==='stop'){stopped=true;for(const x of active.values())close(x);return;}
 if(message.type==='phase'){
  const rateMultiplier=message.phase.rateMultiplier??1;
  if(!Number.isFinite(rateMultiplier)||rateMultiplier<=0)throw new Error('rateMultiplier must be finite and greater than 0');
  phase=message.phase;target=message.target;const remove=[...active.keys()].filter(index=>index>=target),additions=Array.from({length:target},(_,n)=>n).filter(index=>!active.has(index));
  transition=Promise.all([...remove.map((index,n)=>(async()=>{await sleep(phase.rampSeconds*1000*n/Math.max(1,remove.length));const s=active.get(index);if(s)close(s);})()),...additions.map((index,n)=>(async()=>{await sleep(Math.max(0,phase.epoch+phase.rampSeconds*1000*n/Math.max(1,additions.length)-Date.now()));await add(index);})())]);
 }
    if(message.type==='snapshot'){const m=phaseResults.get(phase?.name)||metric();parentPort.postMessage({type:'snapshot',requestId:message.requestId,...status(),latencySamplesMs:{read:[...m.read],write:[...m.write],realtime:[...m.realtime]},canceledLatencySamplesMs:{read:[...m.canceledLatencySamplesMs.read],write:[...m.canceledLatencySamplesMs.write]},diagnosticTimings:copyTiming(m.timing),diagnosticWireTimings:wireTimings(),diagnosticWireConnections:wireConnections()});}
});
const progress=setInterval(()=>parentPort.postMessage({type:'progress',...status()}),5000);
const exposureTimer=setInterval(()=>{if(!stopped&&phase&&Date.now()>=phase.holdAt&&Date.now()<phase.endAt){const c=concurrency.get(phase.name)||{samples:0,sum:0,peak:0,min:Infinity,observerSum:0,observerPeak:0,observerMin:Infinity};const observers=[...active.values()].filter(x=>x.client.observer).length;c.samples++;c.sum+=active.size;c.peak=Math.max(c.peak,active.size);c.min=Math.min(c.min,active.size);c.observerSum+=observers;c.observerPeak=Math.max(c.observerPeak,observers);c.observerMin=Math.min(c.observerMin,observers);concurrency.set(phase.name,c);}},1000);
const churnTimer=setInterval(()=>{if(stopped||paused||!phase||Date.now()<phase.holdAt)return;let n=0;for(const[index,s]of active){if(s.expires<Date.now()&&n++<2){const m=phaseResults.get(phase.name)||metric();m.churn++;phaseResults.set(phase.name,m);close(s);churn++;void add(index);}}},1000);
while(!stopped)await sleep(500);
await transition;for(const x of active.values())close(x);clearInterval(progress);clearInterval(churnTimer);clearInterval(exposureTimer);delay.disable();
parentPort.postMessage({type:'final',...status()});parentPort.close();
