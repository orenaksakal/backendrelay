// Remote capacity-baseline runner for P7.9. Drives the P7.9 profiles against an
// already-deployed staging origin from a machine outside it, so the measured
// latencies include the public network path. It creates throwaway ephemeral
// apps, seeds the same fixture shape as the local benchmark, and writes the
// phase verdicts and latencies. Server and database CPU are not sampled here:
// the host is remote, so read them from the host's own monitoring for the
// phase windows printed below.
//
// Ephemeral app creation must be enabled on the target (staging only; see
// self-hosting/capacity-baseline/README.md). Admin and refresh tokens stay in
// memory: nothing here writes them to the output.
//
// Usage: node remote-scale-run.mjs --origin https://staging-api.example.com
//   [--profile single|many] [--apps 1] [--users-per-app 200] [--steady 300]
//   [--burst 60] [--recovery 90] [--ramp-per-second 50] [--out <dir>]
// Exit: 0 passed, 1 failed, 2 invalid (generator saturated or could not measure).
import assert from 'node:assert/strict';
import {Worker} from 'node:worker_threads';
import {mkdir,writeFile} from 'node:fs/promises';
import {join,resolve} from 'node:path';
import {homedir} from 'node:os';
import {randomUUID} from 'node:crypto';
import {generatorWorkerEnvironment} from './generator-worker-env.mjs';
import {localApps} from './local-scale-model.mjs';
import {fixtureIdentityCapacity,fixtureRoomMembers} from './fixture-rooms.mjs';
import {rowId,summary,profile} from './model.mjs';
import {latencyAcceptance} from './dau-model.mjs';
import {remotePhaseFailures,remoteProfile,remoteScenario} from './remote-scale-model.mjs';

const flags=new Map();
for(let index=2;index<process.argv.length;index+=2){
 const name=process.argv[index],value=process.argv[index+1];
 assert(name?.startsWith('--')&&value!==undefined&&!value.startsWith('--'),`unexpected argument ${name??''}; expected --name value pairs`);
 flags.set(name.slice(2),value);
}
const known=new Set(['profile','origin','apps','users-per-app','steady','burst','recovery','ramp-per-second','out']);
for(const name of flags.keys())assert(known.has(name),'unknown argument --'+name);
const numeric=(name,fallback)=>{if(!flags.has(name))return fallback;const value=Number(flags.get(name));assert(Number.isFinite(value),'--'+name+' must be a finite number');return value;};
const selected=flags.has('profile')?remoteProfile(flags.get('profile')):null;
const originValue=flags.get('origin')??process.env.LOADTEST_API_ORIGIN;
assert(originValue,'--origin (or LOADTEST_API_ORIGIN) must name the staging origin');
let origin;
try{origin=new URL(originValue);}catch{throw new Error('--origin must be an http(s) origin with no path/query');}
assert(['http:','https:'].includes(origin.protocol),'--origin must use http or https');
assert(origin.pathname==='/'&&!origin.search&&!origin.hash&&!origin.username&&!origin.password,'--origin must be an origin with no path, query or credentials');
assert(!['127.0.0.1','localhost','::1','[::1]','0.0.0.0'].includes(origin.hostname),'refusing a loopback origin; use self-hosting/scale-local for local runs');
// session.mjs reads this at module load in every worker; nothing in this
// process talks to any other origin.
process.env.LOADTEST_API_ORIGIN=origin.origin;
const startedAt=new Date().toISOString();
const stateDir=process.env.CAPACITY_STATE_DIR||join(process.env.XDG_STATE_HOME||join(homedir(),'.local/state'),'backendrelay-capacity');
const stamp=startedAt.replaceAll(':','-').replace('.','-');
const scenario=remoteScenario({apps:numeric('apps',selected?.apps??remoteProfile('single').apps),usersPerApp:numeric('users-per-app',selected?.usersPerApp??remoteProfile('single').usersPerApp),steadySeconds:numeric('steady',300),burstSeconds:numeric('burst',60),recoverySeconds:numeric('recovery',90),rampPerSecond:numeric('ramp-per-second',50)});
const appList=localApps(scenario.apps);
const shapeLabel=`${scenario.apps}-apps-${scenario.usersPerApp}-users`;
const directory=resolve(flags.get('out')||join(stateDir,'runs',`${stamp}-${shapeLabel}`));
await mkdir(join(directory,'phases'),{recursive:true});
const save=async(name,value)=>writeFile(join(directory,name),JSON.stringify(value,null,2)+'\n',{mode:0o600});
const emit=value=>console.log(JSON.stringify({at:new Date().toISOString(),...value}));
const sleep=ms=>new Promise(resolve=>setTimeout(resolve,ms));
async function request(path,{method='GET',body,headers={},timeout=30000,attempts=5}={}){
 const url=path.startsWith('http')?path:origin.origin+path;
 for(let attempt=1;;attempt+=1){
  let response;
  try{response=await fetch(url,{method,headers:{'Content-Type':'application/json',...headers},body:body===undefined?undefined:JSON.stringify(body),signal:AbortSignal.timeout(timeout)});}
  catch(error){if(attempt>=attempts)throw error;await sleep(Math.min(8000,500*2**(attempt-1))+Math.floor(Math.random()*250));continue;}
  if(response.ok)return response.json();
  const status=response.status;await response.body?.cancel();
  // Bounded retry/backoff for staging admission control or a restarting pool.
  if(![429,500,502,503,504].includes(status)||attempt>=attempts)throw new Error(`HTTP-${status}`);
  await sleep(Math.min(8000,500*2**(attempt-1))+Math.floor(Math.random()*250));
 }
}
async function healthy(){try{const response=await fetch(origin.origin+'/health/system',{signal:AbortSignal.timeout(5000)});await response.body?.cancel();return response.status===200;}catch{return false;}}
// The fixture schema mirrors local-scale-run.mjs and dau-run.mjs: same entities
// and fields, id unique, id/ownerId/roomId/updatedAt indexed, updatedAt checked
// as number. The create path derives the id attribute itself (unique,
// unindexed), so readiness waits on id uniqueness and only requires an explicit
// index on the other three.
const schema={entities:{
 todos:{attrs:{id:{config:{unique:true,indexed:true}},ownerId:{config:{indexed:true}},title:{},completed:{},revision:{},updatedAt:{valueType:'number',config:{indexed:true}}}},
 messages:{attrs:{id:{config:{unique:true,indexed:true}},roomId:{config:{indexed:true}},authorId:{},memberIds:{},text:{},revision:{},updatedAt:{valueType:'number',config:{indexed:true}}}},
}};
// Copied verbatim from local-scale-run.mjs so remote results stay comparable.
const rules={todos:{allow:{view:'auth.id != null && auth.id == data.ownerId',create:'auth.id != null && auth.id == data.ownerId',update:'auth.id != null && auth.id == data.ownerId && auth.id == newData.ownerId',delete:'auth.id != null && auth.id == data.ownerId'}},messages:{allow:{view:'auth.id != null && auth.id in data.memberIds',create:'auth.id != null && auth.id == data.authorId && auth.id in data.memberIds',update:'auth.id != null && auth.id == data.authorId && auth.id in data.memberIds && newData.memberIds == data.memberIds && newData.roomId == data.roomId && newData.authorId == data.authorId',delete:'auth.id != null && auth.id == data.authorId'}},$default:{allow:{$default:'false'}}};
const requiredAttributes=[['todos','id','unique?'],['todos','ownerId','index?'],['messages','roomId','index?'],['todos','updatedAt','index?'],['messages','updatedAt','index?'],['todos','updatedAt','checked-data-type'],['messages','updatedAt','checked-data-type']];
const schemaReady=metadata=>requiredAttributes.every(([entity,label,field])=>field==='checked-data-type'?metadata.blobs?.[entity]?.[label]?.[field]==='number':metadata.blobs?.[entity]?.[label]?.[field]===true);
assert(await healthy(),'staging server is not healthy at '+origin.origin);
const seedStart=Date.now();
const workloads=[];
emit({phase:'seeding',origin:origin.origin,apps:scenario.apps,usersPerApp:scenario.usersPerApp,totalUsers:scenario.totalUsers,out:directory});
for(const app of appList){
 let created;
 try{created=await request('/dash/apps/ephemeral',{method:'POST',body:{title:`Capacity baseline - ${app.key}`,schema,rules:{code:rules}},timeout:60000});}
 catch(error){if(String(error?.message)==='HTTP-403')throw new Error('ephemeral app creation is disabled on this origin; enable ephemeral-apps-enabled on staging only (see self-hosting/capacity-baseline/README.md)');throw error;}
 const appId=created.app?.id,adminToken=created.app?.['admin-token'];
 assert(typeof appId==='string'&&typeof adminToken==='string'&&appId&&adminToken,'ephemeral app create did not return app id and admin token');
 const adminHeaders={Authorization:'Bearer '+adminToken,'app-id':appId};
 const admin=(path,body,options={})=>request('/admin/'+path+'?app_id='+appId,{method:'POST',headers:adminHeaders,body,...options});
 const until=Date.now()+60000;let metadata=null;
 while(Date.now()<until){metadata=(await request('/admin/schema?app_id='+appId,{headers:adminHeaders})).schema;if(schemaReady(metadata))break;await sleep(250);}
 assert(schemaReady(metadata),`${app.key}: schema indexes/checked types did not settle within 60s`);
 const pool=fixtureIdentityCapacity(scenario.poolPerApp,scenario.groupSize),identities=[];
 for(let start=0;start<pool;start+=16){
  identities.push(...await Promise.all(Array.from({length:Math.min(16,pool-start)},async(_,offset)=>{
   const index=start+offset,response=await admin('refresh_tokens',{email:`capacity-${app.index}-${index}@example.invalid`},{timeout:20000});
   assert(response.user?.id&&response.user?.refresh_token,'refresh token response is incomplete');
   return {userId:response.user.id,token:response.user.refresh_token,roomId:'room-'+Math.floor(index/scenario.groupSize)};
  })));
 }
 for(let start=0;start<pool;start+=16){
  const batch=identities.slice(start,start+16),steps=[];
  for(let offset=0;offset<batch.length;offset+=1){
   const user=batch[offset],index=start+offset;
   user.members=fixtureRoomMembers(identities,index,scenario.groupSize);
   for(let row=0;row<profile.rowsPerUser;row+=1)steps.push(['update','todos',rowId(user.userId,row),{ownerId:user.userId,title:'Synthetic item '+row,completed:false,revision:'seed',updatedAt:1700000000000+row}]);
   steps.push(['update','messages',rowId(user.userId,100),{roomId:user.roomId,authorId:user.userId,memberIds:user.members,text:'Synthetic shared item',revision:'seed',updatedAt:1700000000000+index}]);
  }
  await admin('transact',{steps},{timeout:60000});
 }
 const probe=await admin('query',{query:{todos:{$:{where:{ownerId:identities[0].userId},limit:profile.rowsPerUser}}}},{timeout:20000});
 assert(probe.todos?.length===profile.rowsPerUser,`${app.key}: fixture rows were not persisted`);
 workloads.push({app:{...app,appId},identities});
 emit({phase:'app-ready',app:app.key,users:identities.length,rooms:identities.length/scenario.groupSize});
}
const seedingMs=Date.now()-seedStart;
await save('scenario.json',{...scenario,apps:appList.map(app=>app.key),appCount:scenario.apps,appIds:workloads.map(workload=>workload.app.appId),seededApps:scenario.apps,seedingMs,out:directory,startedAt,origin:origin.origin,network:'public path from the generator host to the staging origin'});
emit({phase:'seeded',apps:workloads.length,seedingMs});

let halting=false,haltReason=null,invalid=null,current=null,finished=false;
function stop(reason){if(halting)return;halting=true;haltReason=reason;}
// Generator-side problems make the measurement invalid, not the server failed.
function invalidate(reason){invalid??=reason;stop(reason);}
process.on('SIGTERM',()=>stop('operator-stop'));process.on('SIGINT',()=>stop('operator-stop'));
const workers=[],progress=new Map(),requests=new Map();
for(const workload of workloads){
 const worker=new Worker(new URL('./dau-worker.mjs',import.meta.url),{workerData:{app:workload.app,identities:workload.identities},env:{...generatorWorkerEnvironment(),LOADTEST_API_ORIGIN:origin.origin}});
 workers.push(worker);
 worker.on('message',message=>{
  if(message.type==='progress'||message.type==='final')progress.set(workload.app.key,message);
  if(message.type==='snapshot'){const pending=requests.get(message.requestId);if(pending){requests.delete(message.requestId);pending.resolve(message);}}
 });
 worker.on('error',()=>invalidate('generator-worker-failure'));
 worker.on('exit',()=>{if(!finished)invalidate('generator-worker-unexpected-exit');});
}
const snapshot=worker=>new Promise((resolve,reject)=>{
 const requestId=randomUUID(),timer=setTimeout(()=>{requests.delete(requestId);reject(new Error('missing-worker-snapshot'));},20000);
 requests.set(requestId,{resolve:value=>{clearTimeout(timer);resolve(value);}});
 worker.postMessage({type:'snapshot',requestId});
});
const metric=(statuses,name)=>{const value=summary(statuses.flatMap(status=>status.latencySamplesMs?.[name]??[]));return {count:value.count,p50:value.p50Ms,p95:value.p95Ms,p99:value.p99Ms,max:value.maxMs};};
const expectedSeconds=scenario.phases.reduce((total,phase)=>total+phase.rampSeconds+phase.holdSeconds+profile.operationTimeoutMs/1000+20,0);
const watchdog=setTimeout(()=>stop('bounded-run-deadline'),Math.ceil(expectedSeconds+180)*1000);
watchdog.unref();
const phaseReports=[];
for(const definition of scenario.phases){
 if(halting)break;
 const epoch=Date.now()+1000;
 current={...definition,epoch,holdAt:epoch+definition.rampSeconds*1000,endAt:epoch+(definition.rampSeconds+definition.holdSeconds)*1000};
 workers.forEach((worker,index)=>worker.postMessage({type:'phase',phase:current,target:current.targets[index]}));
 emit({phase:'phase-start',name:current.name,targets:current.targets,rampSeconds:current.rampSeconds,holdSeconds:current.holdSeconds,rateMultiplier:current.rateMultiplier??1,requestedAt:new Date(current.holdAt).toISOString()});
 while(!halting&&Date.now()<current.endAt){
  if(!invalid&&[...progress.values()].some(status=>status.eventLoopP99Ms>200))invalidate('generator-event-loop-saturation');
  await sleep(250);
 }
 await sleep(profile.operationTimeoutMs+500);
 const statuses=await Promise.all(workers.map(worker=>snapshot(worker))).catch(()=>{invalidate('incomplete-worker-accounting');return [];});
 if(!statuses.length||statuses.length!==workers.length)invalidate('incomplete-worker-accounting');
 const failures=statuses.length===workers.length?remotePhaseFailures(current,statuses,latencyAcceptance,appList):['incomplete worker accounting'];
 if(statuses.some(status=>status.eventLoopP99Ms>200))invalidate('generator-event-loop-saturation');
 const completedOps=statuses.reduce((total,status)=>total+status.metrics.read.count+status.metrics.write.count,0),holds=current.holdSeconds;
 const phase={name:current.name,holdSeconds:holds,holdStartedAt:new Date(current.holdAt).toISOString(),holdEndedAt:new Date(current.endAt).toISOString(),targets:current.targets,rampSeconds:current.rampSeconds,rateMultiplier:current.rateMultiplier??1,passed:!halting&&failures.length===0,failures,completedOps,opsPerSecond:completedOps/holds,read:metric(statuses,'read'),write:metric(statuses,'write'),realtime:metric(statuses,'realtime'),generatorEventLoopP99Ms:Math.max(0,...statuses.map(status=>status.eventLoopP99Ms))};
 phaseReports.push(phase);
 await save(`phases/${current.name}.json`,{name:current.name,kind:current.kind,targets:current.targets,rampSeconds:current.rampSeconds,holdSeconds:holds,rateMultiplier:current.rateMultiplier??1,holdStartedAt:phase.holdStartedAt,holdEndedAt:phase.holdEndedAt,passed:phase.passed,failures,completedOps,opsPerSecond:phase.opsPerSecond,apps:statuses.map(({type,requestId,latencySamplesMs,canceledLatencySamplesMs,diagnosticTimings,diagnosticWireTimings,...rest})=>rest)});
 emit({phase:'phase-complete',name:current.name,passed:phase.passed,completedOps,opsPerSecond:Math.round(phase.opsPerSecond),read:phase.read,write:phase.write,realtime:phase.realtime,failures,holdStartedAt:phase.holdStartedAt,holdEndedAt:phase.holdEndedAt});
 if(failures.length)stop('phase-latency-or-correctness-gate');
}
finished=true;
for(const worker of workers)worker.postMessage({type:'stop'});
await Promise.all(workers.map(worker=>new Promise(resolve=>{worker.once('exit',resolve);setTimeout(()=>{worker.terminate();resolve();},15000).unref();})));
clearTimeout(watchdog);
const passed=!invalid&&phaseReports.length===scenario.phases.length&&phaseReports.every(report=>report.passed);
const result={passed,invalid:Boolean(invalid),invalidReason:invalid,haltReason,profile:flags.get('profile')??null,apps:scenario.apps,usersPerApp:scenario.usersPerApp,totalUsers:scenario.totalUsers,observers:scenario.observers,seedingMs,origin:origin.origin,startedAt,finishedAt:new Date().toISOString(),out:directory,appIds:workloads.map(workload=>workload.app.appId),resourceSampling:'not sampled remotely; read server and PostgreSQL CPU for the phase windows from the host',phases:phaseReports.map(({name,passed:phasePassed,failures,opsPerSecond,read,write,realtime,completedOps,generatorEventLoopP99Ms,holdStartedAt,holdEndedAt})=>({name,passed:phasePassed,failures,opsPerSecond,read,write,realtime,completedOps,generatorEventLoopP99Ms,holdStartedAt,holdEndedAt}))};
await save('result.json',result);
const cell=(value,width=9)=>String(value??'-').padStart(width),latency=value=>[value.p50,value.p95,value.p99,value.max].map(entry=>entry===null?'-':entry.toFixed(1)).join('/');
console.log(['phase'.padEnd(9),'result'.padEnd(7),cell('ops/s',9),('read p50/p95/p99/max').padEnd(30),('write p50/p95/p99/max').padEnd(31),('realtime p50/p95/p99/max').padEnd(33)].join(' '));
for(const phase of result.phases)console.log([phase.name.padEnd(9),(phase.passed?'PASS':'FAIL').padEnd(7),cell(phase.opsPerSecond.toFixed(1),9),latency(phase.read).padEnd(30),latency(phase.write).padEnd(31),latency(phase.realtime).padEnd(33)].join(' '));
for(const phase of result.phases)console.log(`phase ${phase.name} window: ${phase.holdStartedAt} .. ${phase.holdEndedAt}`);
if(result.phases.some(phase=>phase.failures.some(failure=>failure.includes('setupFailures'))))console.log('hint: setupFailures can mean the per-IP live-session limit is below 210; raise INSTANT_MAX_LIVE_SESSIONS_PER_IP on staging for the run');
console.log(`result: ${result.invalid?'INVALID':result.passed?'PASSED':'FAILED'} ${phaseReports.filter(report=>report.passed).length}/${scenario.phases.length} phases passed${phaseReports.length<scenario.phases.length?` (${phaseReports.length}/${scenario.phases.length} ran)`:''}${result.invalidReason||result.haltReason?' ('+(result.invalidReason||result.haltReason)+')':''}: ${join(directory,'result.json')}`);
process.exit(result.invalid?2:result.passed?0:1);
